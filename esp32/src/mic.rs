//! The PDM mic on IO2 (clock) and IO12 (data), and the `mic` thread.
//!
//! The thread reads 16 ms of PCM at 16 kHz from I2S0, runs the beat tracker, and compares the
//! mic grid with the Pro DJ Link master beats. It writes the result into `State::mic`.
//!
//! IO12 is a strapping pin. A high level at reset sets the flash to 1.8 V, and the box does not
//! start. A PDM mic without a clock keeps its data line at high impedance, and the internal
//! pull-down keeps IO12 low. Do not use a mic module with a pull-up on its data line.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use autoblink_core_beat::calib::{Calibrator, Stats};
use autoblink_core_beat::grid::{Emitter, Grid, SampleClock};
use autoblink_core_beat::tracker::{MicState, Section, Tracker};
use autoblink_core_beat::{DEFAULT_OFFSET_US, LONG_FLYWHEEL_MS, SAMPLE_RATE};
use autoblink_core_ui::BoxConfig;
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Gpio12, Gpio2};
use esp_idf_svc::hal::i2s::config::{
    Config, DataBitWidth, PdmDownsample, PdmRxClkConfig, PdmRxConfig, PdmRxGpioConfig,
    PdmRxSlotConfig, SlotMode,
};
use esp_idf_svc::hal::i2s::{I2sDriver, I2sRx, I2S0};
use esp_idf_svc::hal::task::queue::Queue;

use crate::midi::Beat;
use crate::{BeatOut, State};

/// The frames of one read: 16 ms. The DMA holds 6 reads (96 ms), so that a WiFi burst on
/// core 0 drops no sample.
const FRAMES: usize = 256;
/// The time between two writes of the offset to NVS.
const SAVE_EVERY: Duration = Duration::from_secs(300);
/// The offset change that the box writes to NVS, in µs.
const SAVE_CHANGE_US: i64 = 2_000;

/// The time until which `GET /mic` accepts one capture, in ms of `State::now_ms`. 0 is not armed.
/// A mutex, because the Xtensa core has no 64-bit atomics.
static ARMED_UNTIL: Mutex<u64> = Mutex::new(0);
/// The time that an arm stays valid, in ms.
const ARM_MS: u64 = 60_000;
/// The stereo frames that wait for the HTTP task (128 ms, 8 KB).
const RING: usize = 2048;
/// The frames that the thread holds back, so that a Pro DJ Link beat that arrives late still lands
/// on its frame (2 reads, 32 ms).
const HOLD: usize = 2 * FRAMES;

/// A running capture of `GET /mic`. It exists only during a capture.
struct Capture {
    ring: VecDeque<[i16; 2]>,
    /// The frames that wait for their beat mark. The first one has the sample index `hold_from`.
    hold: VecDeque<[i16; 2]>,
    hold_from: u64,
    dropped: u32,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);

/// The active page armed `GET /mic` (WebSocket command `mic_arm`).
pub fn arm(now_ms: u64) {
    *ARMED_UNTIL.lock().unwrap() = now_ms + ARM_MS;
}

/// True once for each arm, within `ARM_MS` of the arm.
pub fn take_arm(now_ms: u64) -> bool {
    now_ms < std::mem::take(&mut *ARMED_UNTIL.lock().unwrap())
}

/// Starts a capture. False while another capture runs.
pub fn capture_start() -> bool {
    let mut c = CAPTURE.lock().unwrap();
    if c.is_some() {
        return false;
    }
    *c = Some(Capture {
        ring: VecDeque::with_capacity(RING),
        hold: VecDeque::with_capacity(HOLD + FRAMES),
        hold_from: 0,
        dropped: 0,
    });
    true
}

/// Moves up to `buf.len() / 4` stereo frames into `buf` as 16-bit little-endian samples. Waits
/// up to `timeout_ms` for the first frame. Returns the number of bytes (4 for each frame).
pub fn capture_read(buf: &mut [u8], timeout_ms: u32) -> usize {
    let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
    loop {
        {
            let mut c = CAPTURE.lock().unwrap();
            let Some(c) = c.as_mut() else {
                return 0;
            };
            let mut n = 0;
            while n + 4 <= buf.len() {
                let Some(f) = c.ring.pop_front() else {
                    break;
                };
                buf[n..n + 2].copy_from_slice(&f[0].to_le_bytes());
                buf[n + 2..n + 4].copy_from_slice(&f[1].to_le_bytes());
                n += 4;
            }
            if n > 0 {
                return n;
            }
        }
        if Instant::now() >= deadline {
            return 0;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ends the capture and frees its buffers. Returns the frames that a full ring dropped.
pub fn capture_stop() -> u32 {
    CAPTURE.lock().unwrap().take().map_or(0, |c| c.dropped)
}

/// Adds one read to a running capture: the PCM of the samples from `first`, and the Pro DJ Link
/// beats as (sample index, beat in the bar).
fn capture_add(first: u64, pcm: &[i16], beats: &[(u64, u8)]) {
    let mut c = CAPTURE.lock().unwrap();
    let Some(c) = c.as_mut() else {
        return;
    };
    if c.hold.is_empty() {
        c.hold_from = first;
    }
    c.hold.extend(pcm.iter().map(|&s| [s, 0]));
    for &(at, beat) in beats {
        if let Some(f) = at
            .checked_sub(c.hold_from)
            .and_then(|i| c.hold.get_mut(i as usize))
        {
            f[1] = i16::from(beat) * 1000;
        }
    }
    while c.hold.len() > HOLD {
        let f = c.hold.pop_front().unwrap_or_default();
        c.hold_from += 1;
        if c.ring.len() < RING {
            c.ring.push_back(f);
        } else {
            c.dropped += 1;
        }
    }
}

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
    /// The time of the last kick on the grid, in µs of `State::now_us`. The `midi` thread gives
    /// the master to the mic only with a kick after the deck stopped.
    pub last_kick_us: Option<u64>,
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
    out: BeatOut,
    offset_us: Option<i64>,
) -> anyhow::Result<()> {
    // Core 0 and priority 4: below the WiFi task, above the `ui` thread (3).
    // ponytail: the thread body has a frame of 2 736 bytes (ENTRY in the ELF, 1.40.0), and 8 KB is an
    // estimate for the callees. Measure with /mem on a box with the mic.
    crate::spawn_rt(c"mic", 8192, 4, Some(Core::Core0), move || {
        let cfg = PdmRxConfig::new(
            Config::default()
                .dma_buffer_count(6)
                .frames_per_buffer(FRAMES as u32),
            // 16 times downsampling gives a PDM clock of 128 x 16 kHz = 2.048 MHz on IO2. A PDM
            // mic such as the MP34DT01 needs 1 to 3.25 MHz. The default 8 times gives 1.024 MHz.
            PdmRxClkConfig::from_sample_rate_hz(SAMPLE_RATE)
                .downsample_mode(PdmDownsample::Samples16),
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
            &out,
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
    out: &BeatOut,
    offset_us: i64,
) {
    let mut enabled = false;
    // The tracker and the calibrator keep their large buffers on the heap. Check the frame of
    // this thread in the ELF after a change: the `entry` of the thread body.
    let mut tracker = Tracker::new();
    let mut clock = SampleClock::new();
    let mut calib = Calibrator::new(offset_us);
    // The read buffers are on the heap, so that the frame of the thread stays small.
    let mut buf = vec![0u8; FRAMES * 2];
    let mut pcm = vec![0i16; FRAMES];
    let mut marks: Vec<(u64, u8)> = Vec::with_capacity(16);
    let mut last_state = MicState::Listening;
    let mut saved = offset_us;
    let mut next_save = Instant::now();
    let mut long_logged = false;
    let mut was_master = false;
    let mut emitter = Emitter::new();
    let mut last_section = None;
    let deck_playing = || {
        state
            .tracker
            .lock()
            .unwrap()
            .master()
            .is_some_and(|m| m.is_playing(state.now_ms()))
    };
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
        let first = tracker.samples();
        tracker.push(&pcm[..n]);
        let now_us = state.now_us();
        clock.observe(tracker.samples(), now_us);
        let raw = tracker.grid().map(|g| clock.grid(&g));
        marks.clear();
        while let Some((b, _)) = pdl.recv_front(0) {
            calib.pdl_beat(b.at_us, b.beat_in_bar, b.bpm, raw.as_ref());
            if marks.len() < marks.capacity() {
                let at = clock.to_sample(b.at_us as f64).max(0.0) as u64;
                marks.push((at, b.beat_in_bar));
            }
        }
        capture_add(first, &pcm[..n], &marks);
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
        // The `midi` thread decides the master from this view (`Arbiter`). It decides in the same
        // pass in which it sees the deck stop, so the clock sees no step without a master.
        let master = state.mic_master.load(Ordering::Relaxed);
        if master != was_master {
            log::info!(
                "Mic: {}",
                if master {
                    "the mic gives the beat"
                } else {
                    "the mic does not give the beat"
                }
            );
            emitter.reset();
            was_master = master;
        }
        calib.set_learning(!master);
        let section = tracker.section();
        if section == Some(Section::Drop) && last_section != section {
            calib.on_drop();
        }
        last_section = section;
        let grid = raw.map(|g| calib.map(&g));
        let dev = device.load(Ordering::Relaxed);
        if let (true, Some(g)) = (master, &grid) {
            if let Some(b) = emitter.next(g, now_us as f64) {
                // A deck that started to play in this pass wins: no beat goes out twice.
                if !deck_playing() {
                    out.send(Beat {
                        device: dev,
                        beat_in_bar: b.beat_in_bar,
                        bpm: b.bpm,
                        at_us: b.at_us,
                    });
                }
            }
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
                master,
                device: dev,
                grid,
                section,
                level: tracker.level(),
                confidence: tracker.confidence(),
                offset_ms: (off / 1000) as i16,
                stats: calib.stats(),
                last_kick_us: tracker.last_kick().map(|k| clock.to_us(k).max(0.0) as u64),
            },
        );
    }
}
