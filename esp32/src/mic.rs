//! The PDM mic on IO2 (clock) and IO12 (data), and the `mic` thread.
//!
//! The thread reads 16 ms of PCM at 16 kHz from I2S0, runs the beat tracker, and compares the
//! mic grid with the Pro DJ Link master beats. It writes the result into `State::mic`.
//!
//! IO12 is a strapping pin. A high level at reset sets the flash to 1.8 V, and the box does not
//! start. A PDM mic without a clock keeps its data line at high impedance, and the internal
//! pull-down keeps IO12 low. Do not use a mic module with a pull-up on its data line.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use autoblink_core_beat::calib::{Calibrator, Stats};
use autoblink_core_beat::grid::{Grid, SampleClock};
use autoblink_core_beat::tracker::{MicState, Section, Tracker};
use autoblink_core_beat::{DEFAULT_OFFSET_US, LONG_FLYWHEEL_MS, SAMPLE_RATE};
use autoblink_core_ui::BoxConfig;
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Gpio12, Gpio2};
use esp_idf_svc::hal::i2s::config::{
    Config, DataBitWidth, PdmRxClkConfig, PdmRxConfig, PdmRxGpioConfig, PdmRxSlotConfig, SlotMode,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sRx, I2S0};
use esp_idf_svc::hal::task::queue::Queue;

use crate::midi::Beat;
use crate::State;

/// The frames of one read: 16 ms. The DMA holds 6 reads (96 ms), so that a WiFi burst on
/// core 0 drops no sample.
const FRAMES: usize = 256;
/// The time between two writes of the offset to NVS.
const SAVE_EVERY: Duration = Duration::from_secs(300);
/// The offset change that the box writes to NVS, in µs.
const SAVE_CHANGE_US: i64 = 2_000;

/// The mic part of the state. The `mic` thread writes it once for each read.
#[derive(Clone, Copy, Debug, Default)]
pub struct MicView {
    /// The setting "Mic" is on.
    pub enabled: bool,
    pub state: MicState,
    /// True while the mic is the master.
    pub master: bool,
    /// The device number that the mic master uses: the setting "Device Number".
    pub device: u8,
    /// The calibrated grid in `Locked` and `Flywheel`.
    pub grid: Option<Grid>,
    pub section: Option<Section>,
    pub level: u8,
    pub confidence: u8,
    pub offset_ms: i16,
    pub stats: Stats,
}

/// Starts the `mic` thread. `pdl` gives the Pro DJ Link master beats for the calibration.
///
/// Call it after `midi::start`: the MIDI UART takes IO2 first, and then routes its receive input
/// away from IO2.
pub fn start(
    state: Arc<State>,
    i2s: I2S0<'static>,
    clk: Gpio2<'static>,
    din: Gpio12<'static>,
    box_cfg: Arc<Mutex<BoxConfig>>,
    device: Arc<AtomicU8>,
    pdl: Arc<Queue<Beat>>,
    offset_us: Option<i64>,
) -> anyhow::Result<()> {
    // Core 0 and priority 4: below the WiFi task, above the `ui` thread (3).
    // ponytail: 6 KB is an estimate. Measure with /mem on a box with the mic.
    crate::spawn_rt(c"mic", 6144, 4, Some(Core::Core0), move || {
        let cfg = PdmRxConfig::new(
            Config::default()
                .dma_buffer_count(6)
                .frames_per_buffer(FRAMES as u32),
            PdmRxClkConfig::from_sample_rate_hz(SAMPLE_RATE),
            PdmRxSlotConfig::from_bits_per_sample_and_slot_mode(
                DataBitWidth::Bits16,
                SlotMode::Mono,
            ),
            PdmRxGpioConfig::new(false),
        );
        let driver = I2sDriver::new_pdm_rx(i2s, &cfg, clk, din)
            .inspect_err(|e| log::warn!("The mic does not start: {e}"))
            .ok();
        run(
            &state,
            driver,
            &box_cfg,
            &device,
            &pdl,
            offset_us.unwrap_or(DEFAULT_OFFSET_US),
        );
    })?;
    Ok(())
}

fn publish(state: &State, view: MicView) {
    *state.mic.lock().unwrap() = view;
}

fn run(
    state: &State,
    mut driver: Option<I2sDriver<'static, I2sRx>>,
    box_cfg: &Mutex<BoxConfig>,
    device: &AtomicU8,
    pdl: &Queue<Beat>,
    offset_us: i64,
) {
    let mut enabled = false;
    let mut tracker = Tracker::new();
    let mut clock = SampleClock::new();
    let mut calib = Calibrator::new(offset_us);
    let mut buf = [0u8; FRAMES * 2];
    let mut pcm = [0i16; FRAMES];
    let mut last_state = MicState::Listening;
    let mut saved = offset_us;
    let mut next_save = Instant::now();
    let mut long_logged = false;
    let timeout = TickType::from(Duration::from_millis(100)).ticks();
    loop {
        let want = box_cfg.lock().unwrap().mic;
        let Some(d) = driver.as_mut() else {
            publish(
                state,
                MicView {
                    enabled: want,
                    state: MicState::NoMic,
                    ..MicView::default()
                },
            );
            thread::sleep(Duration::from_secs(1));
            continue;
        };
        if want != enabled {
            let r = if want { d.rx_enable() } else { d.rx_disable() };
            if let Err(e) = r {
                log::warn!("The mic channel does not change: {e}");
            }
            enabled = want;
            log::info!("Mic: {}", if want { "on" } else { "off" });
            tracker = Tracker::new();
            clock = SampleClock::new();
            last_state = MicState::Listening;
        }
        if !enabled {
            publish(state, MicView::default());
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        let n = match d.read(&mut buf, timeout) {
            Ok(n) => n / 2,
            Err(e) => {
                log::warn!("The mic read fails: {e}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        for (p, b) in pcm.iter_mut().zip(buf.as_chunks::<2>().0).take(n) {
            *p = i16::from_le_bytes(*b);
        }
        tracker.push(&pcm[..n]);
        let now_us = state.now_us();
        clock.observe(tracker.samples(), now_us);
        let raw = tracker.grid().map(|g| clock.grid(&g));
        while let Some((b, _)) = pdl.recv_front(0) {
            calib.pdl_beat(b.at_us, b.beat_in_bar, b.bpm, raw.as_ref());
        }
        if let Some(g) = &raw {
            calib.track(g);
        }

        let s = tracker.state();
        if s != last_state {
            log::info!("Mic: {}", s.label());
            if s == MicState::Locked {
                if let Some(ms) = tracker.cold_lock_ms() {
                    calib.note_lock(ms);
                }
            }
            last_state = s;
            long_logged = false;
        }
        if !long_logged && tracker.flywheel_ms() > LONG_FLYWHEEL_MS {
            log::warn!(
                "Mic: the flywheel runs for {} min without a kick",
                tracker.flywheel_ms() / 60_000
            );
            long_logged = true;
        }
        if let Some(st) = calib.take_minute(now_us) {
            log::info!(
                "Mic accuracy: {} beats, tempo error {}.{:02} BPM, phase {} ms (95 %: {} ms), \
                 bar 1 {} %, offset {} ms, rotation {}, ratio {:?}",
                st.beats,
                st.tempo_err_cbpm / 100,
                st.tempo_err_cbpm % 100,
                st.phase_med_ms,
                st.phase_p95_ms,
                st.bar1_pct,
                calib.offset_us() / 1000,
                calib.rotation(),
                calib.ratio()
            );
        }
        // The main loop writes the offset to NVS. This thread does not wait for the flash.
        let off = calib.offset_us();
        if (off - saved).abs() > SAVE_CHANGE_US && Instant::now() >= next_save {
            *state.mic_offset.lock().unwrap() = Some(off as i32);
            saved = off;
            next_save = Instant::now() + SAVE_EVERY;
        }
        publish(
            state,
            MicView {
                enabled: true,
                state: s,
                master: false,
                device: device.load(Ordering::Relaxed),
                grid: raw.map(|g| calib.map(&g)),
                section: tracker.section(),
                level: tracker.level(),
                confidence: tracker.confidence(),
                offset_ms: (off / 1000) as i16,
                stats: calib.stats(),
            },
        );
    }
}
