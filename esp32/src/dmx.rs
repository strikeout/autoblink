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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use autoblink_core_dmx::{
    apply_room, dim, Action, BeatClock, Changes, Lights, Live, Loss, Merge, PilotStatus, Proto,
    Sender, Show, Source, SourceState, Tier, Universe, DMX_IN_PRIORITY, UNIVERSE,
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

/// The time from one frame write to the next. One frame is the break, the mark after break and
/// the start code with 512 channels. It takes 22.8 ms. Thus the rate is about 41 Hz.
///
/// The thread measures the time from the last write, not from a fixed grid. Thus the UART has
/// sent the frame before the next write comes.
const FRAME_PERIOD: Duration = Duration::from_millis(24);

/// The log counts a write that starts this much after its time. On a fixed grid, such a write
/// makes the next gap shorter than the wire time.
const LATE_LIMIT: Duration = Duration::from_micros(1300);

/// The break length in bit times. At 250 kbaud, one bit is 4 µs, so 44 bits give 176 µs, the
/// break of most consoles. DMX512 needs at least 88 µs.
const BREAK_BITS: u32 = 44;

/// The mark after break in µs. DMX512-A needs at least 12 µs from a transmitter. The UART write
/// adds about 140 to 200 µs: the scope shows a mark of 155 to 227 µs.
const MAB_US: u32 = 20;

/// The UART idle time after its FIFO is empty, in bit times. With 0, the slots of a long frame
/// stay back to back when the FIFO runs empty for a moment.
const IDLE_BITS: u32 = 0;

/// The time after which the thread sends a frame again without a change. The bars of the owner
/// flickered with 40 frames each second, but they were stable with 4 frames. DMX512 allows up to
/// 1 s from break to break.
const REPEAT: Duration = Duration::from_millis(250);

/// With "DMX Refresh" On change, the frames after a change also go out at once, this many. When
/// a light drops a damaged frame, the next frame then comes after 24 ms, not after 250 ms.
const BURST: u8 = 3;

/// The longest wait of the loop. The thread resets the task watchdog (5 s) on each pass, also
/// when `GET /dmxtune` sets a long period.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// The start code and the 512 channels.
const FRAME_LEN: usize = 513;

/// The timing of the DMX output. `GET /dmxtune` changes it at runtime, for tests with a light.
/// The values are not stored. A restart sets the defaults again.
pub struct Tune {
    /// The break length in bit times (4 µs).
    pub break_bits: AtomicU32,
    /// The mark after break in µs.
    pub mab_us: AtomicU32,
    /// The idle time of the UART after its FIFO is empty, in bit times (`uart_set_tx_idle_num`).
    pub idle_bits: AtomicU32,
    /// The time from one write to the next, in µs. With 0, the thread writes the next frame when
    /// the TX buffer has space for it. The frames then go out back to back.
    pub period_us: AtomicU32,
    /// The channels in each frame, 1 to 512. With 0, the setting "DMX Channels" decides.
    pub slots: AtomicU32,
    /// The idle time between two slots in µs. With 0, the slots go out back to back. Otherwise
    /// the thread writes each slot alone and waits.
    pub gap_us: AtomicU32,
    /// The baud rate. DMX512 uses 250 000. A test can move it by a few percent.
    pub baud: AtomicU32,
}

pub static TUNE: Tune = Tune {
    break_bits: AtomicU32::new(BREAK_BITS),
    mab_us: AtomicU32::new(MAB_US),
    idle_bits: AtomicU32::new(IDLE_BITS),
    period_us: AtomicU32::new(FRAME_PERIOD.as_micros() as u32),
    slots: AtomicU32::new(0),
    gap_us: AtomicU32::new(0),
    baud: AtomicU32::new(250_000),
};

/// The input has no signal when no good frame arrives for this time.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(1);

/// After this many rejected frames in a row, the thread clears the receive buffer. Then a lost
/// data event cannot shift the channels for a long time.
const RESYNC_AFTER: u32 = 8;

/// The DMX IN sender for `Source`. It wins over each network sender.
const DMX_IN: Sender = Sender {
    proto: Proto::DmxIn,
    addr: [0; 4],
    priority: DMX_IN_PRIORITY,
    cid: [0; 16],
};

/// The outside source before the first frame of DMX IN, Art-Net or sACN.
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
    /// `true` while a network sender (Art-Net or sACN) is live: its last frame is less than 1 s
    /// old.
    pub artnet: bool,
    /// The protocol of the outside source, or `None` before its first frame.
    pub source: Option<Proto>,
    /// The state of the outside source: live, held, fading or black.
    pub hold: Option<SourceState>,
    /// The key of the setting "DMX Mode", for example "merge".
    pub mode: &'static str,
    /// The good DMX IN frames in the last second.
    pub in_fps: u16,
    /// `true` while a good DMX IN frame arrived in the last second.
    pub signal: bool,
    /// The channel (1 to 512) and the value of the last change in the output. When one frame
    /// changes more than one channel, this is the lowest channel. `None` before the first change.
    pub last_change: Option<(u16, u8)>,
    /// The last changed channels of the outside source (DMX IN, else live Art-Net), newest first,
    /// as (channel, value, age in ms). Each stays for 3 s, like `ccs_recent`.
    pub in_recent: Vec<(u16, u8, u32)>,
    /// The last changed channels of the output, in the same form as `in_recent`.
    pub out_recent: Vec<(u16, u8, u32)>,
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
    let queue = Arc::new(Queue::<DmxMsg>::new(QUEUE_LEN));
    let msgs = queue.clone();

    // Priority 6 is above the HTTP server task (5) and below the `prolink-rx` (8) and `midi` (10)
    // threads on core 1.
    // 1.39.0 used 6208 bytes of the 6656-byte stack (/mem) with Art-Net from Lightkey, ArtPoll
    // and the sACN joins. `dim` and the ArtPollReply path add frames beside the engine calls.
    // 8 KB keeps more than 1.5 KB above that low point. Measure again with a fade out running.
    //
    // The thread installs the driver, so the UART interrupt runs on core 1. On core 0, the WiFi
    // interrupts delayed it, and a long frame had gaps of up to 30 µs between two slots.
    crate::spawn_rt(
        c"dmx",
        8192,
        6,
        Some(Core::Core1),
        move || match UartDriver::new(uart, tx, rx, None::<Gpio36>, None::<Gpio4>, &cfg) {
            Ok(uart) => run(&uart, &state, &show, &box_cfg, &msgs),
            Err(e) => log::error!("DMX: the UART driver does not start: {e}"),
        },
    )?;
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
    // The buffers and the merge state (about 5 KB) are on the heap, not on the stack of the
    // thread. The receive buffer has space for a frame, two break bytes and one byte that shows a
    // frame that is too long.
    let mut out: Box<[u8; FRAME_LEN]> = Box::new([0; FRAME_LEN]);
    // The channels of the last sent frame and of the last outside frame, for the change logs of
    // the page and the display.
    let mut prev: Box<Universe> = Box::new([0; UNIVERSE]);
    let mut prev_in: Box<Universe> = Box::new([0; UNIVERSE]);
    let mut out_log = Changes::default();
    let mut in_log = Changes::default();
    // The outside source, its output frame and the sent slots for the change log.
    let mut src = Box::<Source>::default();
    let mut src_out: Box<Universe> = Box::new([0; UNIVERSE]);
    let mut shown: Box<Universe> = Box::new([0; UNIVERSE]);
    let mut art_buf: Box<[u8; crate::artnet::PACKET_LEN]> =
        Box::new([0; crate::artnet::PACKET_LEN]);
    let mut sacn_buf: Box<[u8; crate::sacn::PACKET_LEN]> = Box::new([0; crate::sacn::PACKET_LEN]);
    let mut groups = crate::sacn::Groups::default();
    // The state of the output for the ArtPollReply, from the last frame.
    let mut node = crate::artnet::Node::default();
    let mut sacn_universe = 1u16;
    // The frames of other universes since the last report.
    let mut other = 0u32;
    // The changes from live to held since the last report.
    let mut holds = 0u32;
    let mut prev_state: Option<SourceState> = None;
    let mut next_stats_log = Instant::now() + Duration::from_secs(60);
    let mut next_groups = Instant::now();
    let mut burst = 0u8;
    // The mode and the sender of the last frame, and the last time that the source was live.
    let mut prev_mode: Option<DmxMode> = None;
    let mut prev_sender: Option<Sender> = None;
    let mut live_at = 0u64;
    let mut rx: Box<[u8; FRAME_LEN + 3]> = Box::new([0; FRAME_LEN + 3]);
    let mut lts = Box::<Merge>::default();
    let mut lights = Lights::default();
    let mut clock = BeatClock::default();
    let mut playing = None;
    let mut last_beat_ms: Option<u64> = None;
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
    // The late writes since the last report, and the latest one.
    let mut late_count = 0u32;
    let mut late_max = Duration::ZERO;
    // The frames that the UART did not take since the last report, and the last error.
    let mut unsent = 0u32;
    let mut unsent_err = None;
    let mut next_late_log = Instant::now() + Duration::from_secs(60);
    // No value: the first frame sets the idle time.
    let mut idle_set = u16::MAX;
    // The bytes of the last frame on the wire, and the time of its write.
    let mut sent: Box<[u8; FRAME_LEN]> = Box::new([0; FRAME_LEN]);
    let mut sent_len = 0usize;
    let mut sent_at: Option<Instant> = None;
    let mut baud_set = 250_000u32;
    // The setting "DMX IN". The thread reads it with the other settings before each frame.
    let mut dmx_in = false;
    // The task watchdog restarts the box when this loop stops for 5 s.
    let watched = match esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::esp_task_wdt_add(std::ptr::null_mut())
    }) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("DMX: the task watchdog does not watch the thread: {e}");
            false
        }
    };
    loop {
        if watched {
            // SAFETY: the thread is a subscriber of the watchdog.
            unsafe { esp_idf_svc::sys::esp_task_wdt_reset() };
        }
        let wait = next_send
            .saturating_duration_since(Instant::now())
            .min(MAX_WAIT);
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
                            // While DMX IN is off, a frame is noise on the open pin.
                            Some(_) if !dmx_in => {}
                            Some(channels) => {
                                src.accept(DMX_IN, None, channels, state.now_ms());
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

        // `main` gives the sockets after the network starts. Before that, there is no Art-Net and
        // no sACN.
        let now_ms = state.now_ms();
        if let Some(sock) = state.artnet.get() {
            other += crate::artnet::drain(sock, state, &mut art_buf, &mut src, &node, now_ms);
        }
        if let Some(sock) = state.sacn.get() {
            other += crate::sacn::drain(sock, &mut sacn_buf, &mut src, sacn_universe, now_ms);
            if Instant::now() >= next_groups {
                next_groups = Instant::now() + Duration::from_secs(1);
                let ifaces = *state.artnet_ifaces.lock().unwrap();
                groups.update(sock, &ifaces, sacn_universe);
            }
        }

        let signal = last_good.is_some_and(|t| t.elapsed() < SIGNAL_TIMEOUT);
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
        let period_us = TUNE.period_us.load(Ordering::Relaxed);
        // With period 0, the next frame starts when the UART has sent the last frame.
        if period_us == 0 && uart.wait_tx_done(0).is_err() {
            next_send = Instant::now() + Duration::from_millis(1);
            continue;
        }
        let baud = TUNE.baud.load(Ordering::Relaxed);
        if baud != baud_set {
            // SAFETY: the driver of the port is installed.
            unsafe { esp_idf_svc::sys::uart_set_baudrate(uart.port(), baud) };
            baud_set = baud;
        }
        let idle = TUNE.idle_bits.load(Ordering::Relaxed) as u16;
        if idle != idle_set {
            // SAFETY: the driver of the port is installed.
            unsafe { esp_idf_svc::sys::uart_set_tx_idle_num(uart.port(), idle) };
            idle_set = idle;
        }
        let late = Instant::now() - next_send;
        if late > LATE_LIMIT {
            late_count += 1;
            late_max = late_max.max(late);
        }
        if Instant::now() >= next_late_log {
            next_late_log = Instant::now() + Duration::from_secs(60);
            if late_count > 0 {
                log::info!(
                    "DMX output: {late_count} frames started late in 60 s, the latest by {} µs",
                    late_max.as_micros()
                );
            }
            if let Some(e) = unsent_err.take() {
                log::warn!("DMX output: {unsent} frames not sent in 60 s, the last with: {e}");
            }
            unsent = 0;
            late_count = 0;
            late_max = Duration::ZERO;
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
        let (mode, beat_channel, frame_cfg) = {
            let cfg = cfg.lock().unwrap();
            dmx_in = cfg.dmx_in;
            (
                cfg.dmx_mode,
                usize::from(cfg.beat_channel).clamp(1, UNIVERSE),
                cfg.clone(),
            )
        };
        sacn_universe = frame_cfg.sacn_universe;
        // A change of the mode, or "DMX IN" off while DMX IN is the source, forgets the held
        // frame. Thus a frame of noise on IO36 or an old desk look does not stay for ever.
        let dmx_in_off = !dmx_in && src.sender().is_some_and(|s| s.proto == Proto::DmxIn);
        if dmx_in_off || prev_mode.is_some_and(|m| m != mode) {
            src.clear();
        }
        prev_mode = Some(mode);
        let loss = Loss {
            black: frame_cfg.loss_black,
            after_ms: u64::from(frame_cfg.loss_s) * 1000,
        };
        // The channels of this frame. The setting "DMX Channels" gives them from the show and the
        // outside source.
        let slots;
        let src_state;
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
            src_state = src.frame(&show, loss, now_ms, &mut src_out);
            // Only Merge and Pass send the outside frame. After the fade out, its length does
            // not count.
            let src_slots = (matches!(mode, DmxMode::Merge | DmxMode::Pass)
                && src_state.is_some_and(|s| s != SourceState::Black))
            .then(|| src.slots())
            .flatten();
            slots = frame_cfg.frame_channels(show.last_channel(), src_slots);
            if !lit {
                // A show without lights keeps the beat flash of the older firmware.
                let level = last_beat_ms.map_or(0, |t| {
                    FLASH_MS.saturating_sub(now_ms.saturating_sub(t)) * 255 / FLASH_MS
                });
                effect[beat_channel - 1] = level as u8;
            }
            // The source follows DMX IN while it has a signal, else one network sender. After the
            // sender stops, it holds or fades the last frame (settings "Signal Loss").
            let outside: Option<&Universe> = src_state.map(|_| &*src_out);
            // Without a source, the next source shows each of its channels that is not 0.
            match outside {
                Some(o) => in_log.frame(&mut prev_in, o, now_ms),
                None => prev_in.fill(0),
            }
            // A source that comes back after the loss time, or a new sender, claims each of its
            // channels that is not 0 again.
            if src_state == Some(SourceState::Live) {
                let back = prev_state != Some(SourceState::Live)
                    && now_ms.saturating_sub(live_at) >= loss.after_ms;
                if back || src.sender() != prev_sender {
                    lts.rearm();
                }
                live_at = now_ms;
            }
            prev_sender = src.sender();
            let send = if mode == DmxMode::Merge && lit {
                lts.apply(&show, outside, &mut effect);
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
            if mode == DmxMode::Pass && outside.is_some() {
                // Pass sends the outside source without the effects of the box. Only the
                // blackout of the page acts on it.
                if live.blackout {
                    dim(&show, 0, channels);
                }
            } else {
                // The room level comes after the merge. Thus the dip and the blackout also apply
                // to the light of the outside source.
                // A trial move of the page acts also while the autopilot is off.
                let (console, all) = lights.room(&show, beat.as_ref(), &live, now_ms);
                apply_room(&show, console, all, channels);
            }
            send
        };
        // The UART sends the frame, then the break. The break before the next frame and the idle
        // time after it make the reset sequence of DMX512.
        let len = 1 + match TUNE.slots.load(Ordering::Relaxed) as usize {
            0 => slots,
            n => n.clamp(1, 512),
        };
        if send {
            // The change log shows only the channels on the wire.
            shown[..len - 1].copy_from_slice(&out[1..len]);
            shown[len - 1..].fill(0);
            out_log.frame(&mut prev, &shown, now_ms);
        }
        let net_live = src_state == Some(SourceState::Live)
            && src.sender().is_some_and(|s| s.proto != Proto::DmxIn);
        log_source(&src, src_state, &mut prev_state, &mut holds);
        if Instant::now() >= next_stats_log {
            next_stats_log = Instant::now() + Duration::from_secs(60);
            let st = src.take_stats();
            if st.frames > 0 || st.ignored > 0 || other > 0 {
                log::info!(
                    "DMX source in 60 s: {} frames, {} late, {} of other senders, {} switches, {} of other universes, {holds} holds, gap up to {} ms",
                    st.frames,
                    st.late,
                    st.ignored,
                    st.switches,
                    other,
                    st.max_gap_ms
                );
            }
            other = 0;
            holds = 0;
        }
        let now_node = crate::artnet::Node {
            port_address: frame_cfg.artnet_universe,
            sending: mode != DmxMode::Off,
            sacn: src_state.is_some() && src.sender().is_some_and(|s| s.proto == Proto::Sacn),
            merging: mode == DmxMode::Merge && src_state.is_some(),
            continuous: frame_cfg.dmx_refresh_always,
            failsafe_zero: frame_cfg.loss_black,
        };
        if now_node != node {
            node = now_node;
            if let Some(sock) = state.artnet.get() {
                crate::artnet::changed(sock, state, &node);
            }
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
            s.artnet = net_live;
            s.source = src_state.and(src.sender()).map(|s| s.proto);
            s.hold = src_state;
            s.mode = mode.key();
            s.in_fps = in_fps;
            s.signal = signal;
            s.last_change = out_log.newest();
            in_log.recent(now_ms, &mut s.in_recent);
            out_log.recent(now_ms, &mut s.out_recent);
        }
        // The next write comes one period after this write.
        next_send = Instant::now() + Duration::from_micros(u64::from(period_us));
        // With "DMX Refresh" On change, a frame without a change waits until `REPEAT` is over.
        // The `BURST` frames after a change go out at once.
        let changed = len != sent_len || out[..len] != sent[..len];
        if changed {
            burst = BURST;
        }
        let due = frame_cfg.dmx_refresh_always
            || changed
            || burst > 0
            || sent_at.is_none_or(|t| t.elapsed() >= REPEAT);
        if send && due {
            let bit_us = 1_000_000 / baud_set.max(1);
            let brk_us = TUNE.break_bits.load(Ordering::Relaxed) * bit_us;
            let mab_us = TUNE.mab_us.load(Ordering::Relaxed);
            let gap = TUNE.gap_us.load(Ordering::Relaxed);
            // The last frame is normally out, because the period is longer than a frame. A late
            // frame gets 30 ms.
            // ESP-IDF gives the TX done event also when the FIFO runs empty in a frame, for
            // example during a flash write. The call with 0 ticks reads the state of the UART.
            let done = uart
                .wait_tx_done(TickType::from(Duration::from_millis(30)).ticks())
                .and_then(|()| uart.wait_tx_done(0));
            let result = done.and_then(|()| {
                send_break(uart.port(), brk_us, mab_us);
                if gap == 0 {
                    uart.write(&out[..len]).map(|_| ())
                } else {
                    // One slot takes 11 bits. The wait after each write is the slot and the gap.
                    for &b in &out[..len] {
                        uart.write(&[b])?;
                        esp_idf_svc::hal::delay::Ets::delay_us(11 * bit_us + gap);
                    }
                    Ok(())
                }
            });
            match result {
                Ok(()) => {
                    if !changed {
                        burst = burst.saturating_sub(1);
                    }
                    sent[..len].copy_from_slice(&out[..len]);
                    sent_len = len;
                    sent_at = Some(Instant::now());
                }
                // The next pass tries again. The log shows the count once a minute.
                Err(e) => {
                    unsent += 1;
                    unsent_err = Some(e);
                }
            }
        }
        // Each pass waits at least 1 ms. Thus the idle task runs, also with `GET /dmxtune`
        // values that make a pass long.
        next_send = next_send.max(Instant::now() + Duration::from_millis(1));
    }
}

/// Sends the break and the mark after break of a frame. The TX line is inverted for the break.
/// The TX FIFO of the UART must be empty.
///
/// The break comes before the data of its frame, as on a console. The ESP-IDF break comes after
/// the data, so the time between two frames was the mark after break. With a short frame, that
/// mark was about 25 ms at 40 Hz and up to 250 ms with "DMX Refresh" On change.
// ponytail: a task with a higher priority can make the break or the mark longer. DMX512 allows
// both up to 1 s. The upgrade is a hardware timer, as in the esp_dmx library.
fn send_break(port: esp_idf_svc::sys::uart_port_t, break_us: u32, mab_us: u32) {
    use esp_idf_svc::hal::delay::Ets;
    use esp_idf_svc::sys::{uart_set_line_inverse, uart_signal_inv_t_UART_SIGNAL_TXD_INV};
    // SAFETY: the driver of the port is installed, and the UART sends nothing now.
    unsafe { uart_set_line_inverse(port, uart_signal_inv_t_UART_SIGNAL_TXD_INV as u32) };
    Ets::delay_us(break_us);
    // SAFETY: as above. The mask 0 turns off each inversion.
    unsafe { uart_set_line_inverse(port, 0) };
    Ets::delay_us(mab_us);
}

/// Logs a change of the state of the outside source. A change between live and held only adds 1
/// to `holds`. A desk that sends only on a change gives such a change each few seconds.
fn log_source(
    src: &Source,
    state: Option<SourceState>,
    prev: &mut Option<SourceState>,
    holds: &mut u32,
) {
    if state == *prev {
        return;
    }
    let was = std::mem::replace(prev, state);
    let Some(state) = state else {
        return;
    };
    match (was, state) {
        (Some(SourceState::Live), SourceState::Held) => *holds += 1,
        (Some(SourceState::Held), SourceState::Live) => {}
        _ => {
            if let Some(s) = src.sender() {
                let [a, b, c, d] = s.addr;
                log::info!(
                    "DMX source: {:?} from {a}.{b}.{c}.{d} is {state:?}",
                    s.proto
                );
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
