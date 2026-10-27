//! DMX512 output on UART1, TX on IO4. The `dmx` feature is on by default.
//!
//! An RS-485 module (MAX3485 or SP3485) turns the UART signal into DMX. The `dmx` thread runs
//! the `Lights` engine of `autoblink-core-dmx` and sends one frame after the other. The `midi`
//! thread queues the scene notes, the fired events and the preset tests. The receive thread
//! queues the master beats. The show in the `show` partition sets the looks. Without a stored
//! show, all channels stay at 0.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use autoblink_core_dmx::{BeatClock, Lights, Live, Show, Universe};
use autoblink_core_midi::scenes::Event;
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::gpio::{Gpio36, Gpio4};
use esp_idf_svc::hal::task::queue::Queue;
use esp_idf_svc::hal::uart::{config, UartDriver, UART1};
use esp_idf_svc::hal::units::Hertz;
use serde::Serialize;

use crate::State;

/// The idle time between two frames. The UART sends one frame (break, start code and 512
/// channels) in about 23 ms, so the rate is about 42 Hz.
const FRAME_GAP: Duration = Duration::from_millis(1);

/// The break length in bit times. At 250 kbaud, one bit is 4 µs, so 25 bits give 100 µs.
/// DMX512 needs at least 88 µs.
const BREAK_BITS: i32 = 25;

/// The number of messages that the queue to the `dmx` thread holds. A full queue drops a message.
const QUEUE_LEN: usize = 32;

/// A message to the `dmx` thread. The FreeRTOS queue copies it, so it must be `Copy`.
#[derive(Clone, Copy)]
pub enum DmxMsg {
    /// The note on of a preset. The value is the preset index.
    On(u8),
    /// The note off of a preset.
    Off(u8),
    /// An event of the scenes fired a preset.
    Fired(Event, u8),
    /// The page tests a preset.
    Test(u8),
    /// The fog button of the page.
    Fog,
    /// A master beat. `at_ms` uses the clock of `State::now_ms`.
    Beat {
        beat_in_bar: u8,
        bpm: f64,
        at_ms: u64,
    },
}

/// The live controls of the page. The `dmx` thread reads them for each frame.
pub struct DmxLive {
    /// 255 is full.
    pub master: AtomicU8,
    pub strobe_hold: AtomicBool,
}

impl Default for DmxLive {
    fn default() -> Self {
        Self {
            master: AtomicU8::new(255),
            strobe_hold: AtomicBool::new(false),
        }
    }
}

/// The DMX part of the status for the page.
#[derive(Default, Clone, Serialize)]
pub struct DmxStatus {
    /// The presets that show now, bottom first.
    pub layers: Vec<u8>,
    pub master: u8,
    pub strobe_hold: bool,
}

/// Starts the `dmx` thread. Returns the queue for the messages to the thread.
///
/// `rx` is not used. The UART driver of ESP-IDF 5.5 does not install without a receive buffer,
/// so the driver gets the input-only pin IO36.
pub fn start(
    uart: UART1<'static>,
    tx: Gpio4<'static>,
    rx: Gpio36<'static>,
    state: Arc<State>,
    show: Arc<Mutex<Show>>,
) -> anyhow::Result<Arc<Queue<DmxMsg>>> {
    let cfg = config::Config::new()
        .baudrate(Hertz(250_000))
        .stop_bits(config::StopBits::STOP2);
    let uart = UartDriver::new(uart, tx, rx, None::<Gpio36>, None::<Gpio4>, &cfg)?;
    let queue = Arc::new(Queue::<DmxMsg>::new(QUEUE_LEN));
    let msgs = queue.clone();

    // Priority 6 is above the HTTP server task (5) and below the `prolink-rx` (8) and `midi` (10)
    // threads on core 1.
    // ponytail: 6 KB is an estimate (two 512-byte frames on the stack). Measure with /mem on a box.
    crate::spawn_rt(c"dmx", 6 * 1024, 6, Some(Core::Core1), move || {
        // Byte 0 is the start code (0 for dimmer data). Bytes 1 to 512 are the channels.
        let mut frame = [0u8; 513];
        let mut lights = Lights::default();
        let mut clock = BeatClock::default();
        let mut playing = None;
        loop {
            let now_ms = state.now_ms();
            let is_playing = state
                .tracker
                .lock()
                .unwrap()
                .master()
                .is_some_and(|m| m.is_playing(now_ms));
            // `set_playing` keeps the time of the stop. Thus the thread calls it only on a change.
            if playing != Some(is_playing) {
                lights.set_playing(is_playing, now_ms);
                playing = Some(is_playing);
            }
            let live = Live {
                master: state.dmx_live.master.load(Ordering::Relaxed),
                strobe_hold: state.dmx_live.strobe_hold.load(Ordering::Relaxed),
            };
            {
                let show = show.lock().unwrap();
                while let Some((msg, _)) = msgs.recv_front(0) {
                    match msg {
                        DmxMsg::On(p) => lights.on(p, now_ms),
                        DmxMsg::Off(p) => lights.off(p, now_ms),
                        DmxMsg::Fired(e, p) => lights.fired(&show, e, p, now_ms),
                        DmxMsg::Test(p) => lights.test(&show, p, now_ms),
                        DmxMsg::Fog => lights.fog(now_ms),
                        DmxMsg::Beat {
                            beat_in_bar,
                            bpm,
                            at_ms,
                        } => clock.beat(at_ms, beat_in_bar, bpm),
                    }
                }
                let universe: &mut Universe = (&mut frame[1..]).try_into().unwrap();
                let beat = clock.now(now_ms);
                lights.frame(&show, beat.as_ref(), &live, now_ms, universe);
            }
            // The page reads the status. The thread does not wait for the lock, and it writes
            // the layers only after a change. The next frame tries again.
            if let Ok(mut s) = state.dmx_status.try_lock() {
                if !s.layers.iter().copied().eq(lights.layers()) {
                    s.layers.clear();
                    s.layers.extend(lights.layers());
                }
                s.master = live.master;
                s.strobe_hold = live.strobe_hold;
            }
            // The UART sends the frame, then the break. The break before the next frame
            // and the idle time after it make the reset sequence of DMX512.
            if let Err(e) = uart.write_with_break(&frame, BREAK_BITS) {
                log::warn!("DMX frame not sent: {e}");
            }
            let _ = uart.wait_tx_done(100);
            thread::sleep(FRAME_GAP);
        }
    })?;
    log::info!("DMX output is on IO4");
    Ok(queue)
}
