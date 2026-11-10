//! DMX512 on UART1: output on IO4, input on IO36. The feature `dmx` is on by default.
//!
//! Two RS-485 modules (MAX3485 or SP3485) turn the UART signals into DMX. The `dmx` thread runs
//! the `Lights` engine of `autoblink-core-dmx` and sends one frame after the other. The `midi`
//! thread queues the scene notes, the fired events, the preset tests and the actions of the
//! autopilot. The receive thread queues the master beats. The show in the `show` partition sets
//! the looks.
//!
//! A show without lights keeps the effect of the older firmware: the channel of the setting "DMX
//! Beat Channel" goes to full on each master beat and fades out in `FLASH_MS`.
//!
//! The input receives the frames of a console. One frame is a break, the start code and up to 512
//! channels. The thread puts the channels of each good frame into the input buffer. Without a
//! DMX IN signal, the last Art-Net frame is the outside source (`artnet.rs`). The setting "DMX
//! Mode" sets how the output uses the outside source and the box. In the mode Merge, a show with
//! lights uses the HTP and LTP rules of `autoblink_core_dmx::Merge`. A show without lights uses
//! HTP for each channel (`autoblink_core_ui::merge`).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use autoblink_core_dmx::{
    apply_room, Action, BeatClock, Lights, Live, Merge, PilotStatus, Show, Tier, Universe,
    ARTNET_TIMEOUT_MS, UNIVERSE,
};
use autoblink_core_midi::scenes::Event;
use autoblink_core_ui::{frame_channels, merge, BoxConfig, DmxMode};
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Gpio36, Gpio4};
use esp_idf_svc::hal::task::queue::Queue;
use esp_idf_svc::hal::uart::{config, UartDriver, UartEvent, UartEventPayload, UART1};
use esp_idf_svc::hal::units::Hertz;
use serde::Serialize;

use crate::State;

/// The fade time of the beat flash of a show without lights.
const FLASH_MS: u64 = 200;

/// The time between the starts of two output frames. The UART sends one frame (512 channels) in
/// about 23 ms. The idle time after the break is the mark after break of the next frame. Thus the
/// rate is about 41 Hz.
const FRAME_PERIOD: Duration = Duration::from_millis(24);

/// The break length in bit times. At 250 kbaud, one bit is 4 µs, so 25 bits give 100 µs.
/// DMX512 needs at least 88 µs.
const BREAK_BITS: i32 = 25;

/// The start code and the 512 channels.
const FRAME_LEN: usize = 513;

/// The input has no signal when no good frame arrives for this time.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(1);

/// After this many rejected frames in a row, the thread clears the receive buffer. Then a lost
/// data event cannot shift the channels for a long time.
const RESYNC_AFTER: u32 = 8;

/// The outside source when DMX IN and Art-Net are both silent.
static NO_SOURCE: Universe = [0; UNIVERSE];

/// The number of messages that the queue to the `dmx` thread holds. A full queue drops a message.
const QUEUE_LEN: usize = 32;

/// A message to the `dmx` thread. The FreeRTOS queue copies it, so it must be `Copy`.
#[derive(Clone, Copy)]
pub enum DmxMsg {
    /// The note on of a preset. The value is the preset index.
    On(u8),
    /// The note off of a preset.
    Off(u8),
    /// An event of the scenes fires a preset.
    Fired(Event, u8),
    /// The page tests a preset.
    Test(u8),
    /// The fog button of the page.
    Fog,
    /// The page aims a head by hand.
    Aim { fixture: u8, pan: u16, tilt: u16 },
    /// A master beat. `at_ms` uses the clock of `State::now_ms`.
    Beat {
        beat_in_bar: u8,
        bpm: f64,
        at_ms: u64,
    },
    /// An action of the autopilot. The `midi` thread sends it only while `show.auto.on` is true.
    Auto(Action),
    /// The page tries the move with this index for 8 bars.
    TryMove(u8),
}

/// The live controls of the page. The `dmx` thread reads them for each frame.
pub struct DmxLive {
    /// 255 is full.
    pub master: AtomicU8,
    pub strobe_hold: AtomicBool,
    /// True while the page holds the whole room dark.
    pub blackout: AtomicBool,
}

impl Default for DmxLive {
    fn default() -> Self {
        Self {
            master: AtomicU8::new(255),
            strobe_hold: AtomicBool::new(false),
            blackout: AtomicBool::new(false),
        }
    }
}

/// The autopilot part of the status. The `midi` thread writes it.
#[derive(Clone, Copy, Serialize)]
pub struct AutoStatus {
    /// True while the autopilot of the show is on.
    pub on: bool,
    pub tier: Tier,
    /// The energy spot of the music in the night, 0 to 100.
    pub spot: u8,
    pub pinned: Option<Tier>,
    /// The index into `show.auto.programs` of the last pick.
    pub program: Option<u8>,
    /// Bit i is 1 while `show.auto.moves[i]` runs.
    pub moves: u32,
    pub drop_in_beats: Option<u16>,
}

impl AutoStatus {
    pub fn new(on: bool, s: PilotStatus) -> Self {
        Self {
            on,
            tier: s.tier,
            spot: s.spot,
            pinned: s.pinned,
            program: s.program,
            moves: s.moves,
            drop_in_beats: s.drop_in_beats,
        }
    }
}

/// The DMX part of the status for the page and the display.
#[derive(Default, Clone, Serialize)]
pub struct DmxStatus {
    /// The presets that show now, bottom first.
    pub layers: Vec<u8>,
    pub master: u8,
    pub strobe_hold: bool,
    /// `true` while the page holds the whole room dark.
    pub blackout: bool,
    /// `true` while Art-Net is live: the last frame is not older than `ARTNET_TIMEOUT_MS`.
    pub artnet: bool,
    /// The key of the setting "DMX Mode", for example "merge".
    pub mode: &'static str,
    /// The good DMX IN frames in the last second.
    pub in_fps: u16,
    /// `true` while a good DMX IN frame arrived in the last second.
    pub signal: bool,
    /// The channel (1 to 512) and the value of the last change in the output. When one frame
    /// changes more than one channel, this is the lowest channel. `None` before the first change.
    pub last_change: Option<(u16, u8)>,
}

/// Starts the `dmx` thread. Returns the queue for the messages to the thread.
pub fn start(
    uart: UART1<'static>,
    tx: Gpio4<'static>,
    rx: Gpio36<'static>,
    state: Arc<State>,
    show: Arc<Mutex<Show>>,
    box_cfg: Arc<Mutex<BoxConfig>>,
) -> anyhow::Result<Arc<Queue<DmxMsg>>> {
    // A receive timeout of one character time puts the bytes before a break into the receive
    // buffer while the break is on the line. Then the data event that follows a break event
    // closes the frame.
    let cfg = config::Config {
        event_config: config::EventConfig {
            receive_timeout: Some(1),
            ..Default::default()
        },
        ..config::Config::new()
            .baudrate(Hertz(250_000))
            .stop_bits(config::StopBits::STOP2)
            .rx_fifo_size(2048)
            .tx_fifo_size(1024)
            .queue_size(32)
    };
    let uart = UartDriver::new(uart, tx, rx, None::<Gpio36>, None::<Gpio4>, &cfg)?;
    let queue = Arc::new(Queue::<DmxMsg>::new(QUEUE_LEN));
    let msgs = queue.clone();

    // Priority 6 is above the HTTP server task (5) and below the `prolink-rx` (8) and `midi` (10)
    // threads on core 1.
    // 1.32.3 used 4096 bytes of the stack (/mem) with the engine and DMX IN. 6 KB keeps a
    // margin of 2 KB.
    crate::spawn_rt(c"dmx", 6 * 1024, 6, Some(Core::Core1), move || {
        run(&uart, &state, &show, &box_cfg, &msgs)
    })?;
    log::info!("DMX output is on IO4, DMX input is on IO36");
    Ok(queue)
}

fn run(
    uart: &UartDriver,
    state: &State,
    show: &Mutex<Show>,
    cfg: &Mutex<BoxConfig>,
    msgs: &Queue<DmxMsg>,
) {
    let Some(events) = uart.event_queue() else {
        log::error!("DMX: the UART has no event queue");
        return;
    };
    // The buffers and the merge state (about 1.5 KB) are on the heap, not on the stack of the
    // thread. The receive buffer has space for a frame, two break bytes and one byte that shows a
    // frame that is too long.
    let mut out: Box<[u8; FRAME_LEN]> = Box::new([0; FRAME_LEN]);
    // The channels of the last sent frame, to find the last change for the display.
    let mut prev: Box<[u8; FRAME_LEN]> = Box::new([0; FRAME_LEN]);
    let mut last_change: Option<(u16, u8)> = None;
    let mut input: Box<Universe> = Box::new([0; UNIVERSE]);
    let mut art: Box<Universe> = Box::new([0; UNIVERSE]);
    let mut rx: Box<[u8; FRAME_LEN + 3]> = Box::new([0; FRAME_LEN + 3]);
    let mut lts = Box::<Merge>::default();
    let mut lights = Lights::default();
    let mut clock = BeatClock::default();
    let mut playing = None;
    let mut last_beat_ms: Option<u64> = None;
    // The time of the Art-Net frame in `art`. The thread keeps the old copy when the lock is busy.
    let mut art_at: Option<u64> = None;
    let mut rx_len = 0;
    let mut rx_bad = false;
    // The break events since the last frame end.
    let mut breaks = 0usize;
    let mut bad_in_row = 0u32;
    let mut last_good: Option<Instant> = None;
    let mut frames = 0u16;
    let mut in_fps = 0u16;
    let mut rejected = 0u32;
    let mut was_signal = false;
    let mut next_count = Instant::now() + Duration::from_secs(1);
    let mut next_send = Instant::now();
    loop {
        let wait = next_send.saturating_duration_since(Instant::now());
        if let Some((event, _)) = events.recv_front(TickType::from(wait).ticks()) {
            match event.payload() {
                UartEventPayload::Data { size, timeout } => {
                    // Read exactly the bytes of this event. Thus a frame ends at the event that
                    // closes it, also when the thread is late and the next frame is in the buffer.
                    let mut left = size;
                    while left > 0 {
                        let room = rx.len() - rx_len;
                        let n = if room == 0 {
                            rx_bad = true;
                            let mut skip = [0u8; 64];
                            uart.read(&mut skip[..left.min(64)], 0).unwrap_or(0)
                        } else {
                            let n = uart
                                .read(&mut rx[rx_len..rx_len + left.min(room)], 0)
                                .unwrap_or(0);
                            rx_len += n;
                            n
                        };
                        if n == 0 {
                            break;
                        }
                        left -= n;
                    }
                    if timeout && breaks > 0 {
                        let good = (!rx_bad)
                            .then(|| frame_channels(&rx[..rx_len], breaks))
                            .flatten();
                        match good {
                            Some(channels) => {
                                input[..channels.len()].copy_from_slice(channels);
                                // A shorter frame does not keep the old values of the other channels.
                                input[channels.len()..].fill(0);
                                last_good = Some(Instant::now());
                                frames = frames.saturating_add(1);
                                bad_in_row = 0;
                            }
                            None => {
                                rejected = rejected.wrapping_add(1);
                                bad_in_row += 1;
                            }
                        }
                        rx_len = 0;
                        rx_bad = false;
                        breaks = 0;
                        if bad_in_row >= RESYNC_AFTER {
                            bad_in_row = 0;
                            resync(uart, events);
                        }
                    }
                }
                UartEventPayload::Break => breaks = (breaks + 1).min(2),
                UartEventPayload::RxFifoOverflow | UartEventPayload::RxBufferFull => {
                    resync(uart, events);
                    rx_len = 0;
                    rx_bad = false;
                    breaks = 0;
                    rejected = rejected.wrapping_add(1);
                }
                _ => {}
            }
        }

        let signal = last_good.is_some_and(|t| t.elapsed() < SIGNAL_TIMEOUT);
        if !signal {
            input.fill(0);
        }
        if Instant::now() >= next_count {
            next_count += Duration::from_secs(1);
            if signal != was_signal {
                if signal {
                    log::info!("DMX input: signal, {frames} frames/s");
                } else {
                    log::info!("DMX input: no signal. {rejected} frames rejected");
                }
                was_signal = signal;
            }
            in_fps = frames;
            frames = 0;
        }

        if Instant::now() < next_send {
            continue;
        }
        next_send += FRAME_PERIOD;
        // After a long stop, do not send the missed frames.
        if next_send < Instant::now() {
            next_send = Instant::now() + FRAME_PERIOD;
        }

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
            blackout: state.dmx_live.blackout.load(Ordering::Relaxed),
        };
        // The thread does not wait for the `artnet` thread. It copies the frame and releases the
        // lock before the merge.
        if let Ok(slot) = state.artnet.try_lock() {
            art.copy_from_slice(&slot.data[..]);
            art_at = slot.at_ms;
        }
        let art_live = art_at.is_some_and(|t| now_ms.saturating_sub(t) <= ARTNET_TIMEOUT_MS);
        let (mode, beat_channel) = {
            let cfg = cfg.lock().unwrap();
            (
                cfg.dmx_mode,
                usize::from(cfg.beat_channel).clamp(1, UNIVERSE),
            )
        };
        let send = {
            let show = show.lock().unwrap();
            while let Some((msg, _)) = msgs.recv_front(0) {
                match msg {
                    DmxMsg::On(p) => lights.on(p, now_ms),
                    DmxMsg::Off(p) => lights.off(p, now_ms),
                    DmxMsg::Fired(e, p) => lights.fired(&show, e, p, now_ms),
                    DmxMsg::Test(p) => lights.test(&show, p, now_ms),
                    DmxMsg::Fog => lights.fog(now_ms),
                    DmxMsg::Aim { fixture, pan, tilt } => lights.aim(fixture, pan, tilt, now_ms),
                    DmxMsg::Beat {
                        beat_in_bar,
                        bpm,
                        at_ms,
                    } => {
                        clock.beat(at_ms, beat_in_bar, bpm);
                        last_beat_ms = Some(at_ms);
                    }
                    DmxMsg::Auto(a) => lights.auto(a, now_ms),
                    DmxMsg::TryMove(i) => lights.try_move(i, now_ms),
                }
            }
            // Byte 0 is the start code (0 for dimmer data). Bytes 1 to 512 are the channels.
            let channels: &mut Universe = (&mut out[1..]).try_into().expect("512 channels");
            let beat = clock.now(now_ms);
            let mut effect = [0u8; UNIVERSE];
            lights.frame(&show, beat.as_ref(), &live, now_ms, &mut effect);
            let lit = !show.fixtures.is_empty();
            if !lit {
                // A show without lights keeps the beat flash of the older firmware.
                let level = last_beat_ms.map_or(0, |t| {
                    FLASH_MS.saturating_sub(now_ms.saturating_sub(t)) * 255 / FLASH_MS
                });
                effect[beat_channel - 1] = level as u8;
            }
            // DMX IN is the outside source. Without its signal, the last live Art-Net frame is.
            let outside: Option<&Universe> = if signal {
                Some(&input)
            } else if art_live {
                Some(&art)
            } else {
                None
            };
            let send = if mode == DmxMode::Merge && lit {
                lts.apply(&show, outside.map(|o| (o, now_ms)), now_ms, &mut effect);
                channels.copy_from_slice(&effect);
                true
            } else {
                merge(
                    mode,
                    outside.is_some(),
                    outside.unwrap_or(&NO_SOURCE),
                    &effect,
                    channels,
                )
            };
            // The room level comes after the merge. Thus the dip and the blackout also apply
            // to the light of the outside source.
            // A trial move of the page acts also while the autopilot is off.
            let (console, all) = lights.room(&show, beat.as_ref(), &live, now_ms);
            apply_room(&show, console, all, channels);
            send
        };
        if send {
            // The lowest channel that changed since the last sent frame.
            if let Some(i) = (1..FRAME_LEN).find(|&i| out[i] != prev[i]) {
                last_change = Some((i as u16, out[i]));
            }
            prev.copy_from_slice(&out[..]);
        }
        // The page and the display read the status. The thread does not wait for the lock, and
        // it writes the layers only after a change. The next frame tries again.
        if let Ok(mut s) = state.dmx_status.try_lock() {
            if !s.layers.iter().copied().eq(lights.layers()) {
                s.layers.clear();
                s.layers.extend(lights.layers());
            }
            s.master = live.master;
            s.strobe_hold = live.strobe_hold;
            s.blackout = live.blackout;
            s.artnet = art_live;
            s.mode = mode.key();
            s.in_fps = in_fps;
            s.signal = signal;
            s.last_change = last_change;
        }
        // The UART sends the frame, then the break. The break before the next frame and the idle
        // time after it make the reset sequence of DMX512.
        if send {
            if let Err(e) = uart.write_with_break(&out[..], BREAK_BITS) {
                log::warn!("DMX frame not sent: {e}");
            }
        }
    }
}

/// Drops the bytes in the receive buffer and their events.
///
/// The queue goes first. A data event that arrives after the drain has its bytes in the buffer,
/// and `clear_rx` removes them. In the other order, such an event goes and its bytes stay. Then
/// each later read is late by these bytes.
fn resync(uart: &UartDriver, events: &Queue<UartEvent>) {
    while events.recv_front(0).is_some() {}
    let _ = uart.clear_rx();
}
