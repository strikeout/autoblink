//! MIDI output on UART2, TX on IO17, at 31250 baud.
//!
//! One thread runs the clock, the note and CC mapper and the DMX scenes of `autoblink-core-midi`. The receive thread
//! sends it each master beat through a FreeRTOS queue. The thread sleeps until 1 ms before the
//! next clock pulse, then waits in a loop, so the pulse goes out on time.
//!
//! Do not use `std::sync::mpsc` for the beats. Its receiver spins with `sched_yield` while a
//! sender writes a slot. On FreeRTOS, that yield does not let a task with a lower priority run.
//! This thread has priority 10 and the receive thread has priority 8, on the same core. Thus a
//! beat that arrived during a send stopped the two threads until the task watchdog restarted the
//! box.
//!
//! The task watchdog watches the thread. When a loop pass does not end in 5 s, the firmware
//! restarts (CONFIG_ESP_TASK_WDT_PANIC).
//!
//! ponytail: a pulse waits behind the CC bytes that are already in the UART, up to about 2 ms.
//! The host sends realtime bytes on a priority lane. To do the same, insert the 0xF8 bytes
//! between the bytes of the other messages.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use autoblink_core_dbserver::{current_phrase_idx, phrase_info};
use autoblink_core_midi::clock::Master;
use autoblink_core_midi::scenes::{SceneInput, ScenePhrase, SceneStatus, Scenes, ScenesConfig};
use autoblink_core_midi::{Clock, Mapper, MidiConfig, Out, Phrase, Smoother, Snapshot, CLOCK};
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Gpio17, Gpio35};
use esp_idf_svc::hal::task::queue::Queue;
use esp_idf_svc::hal::uart::{config, UartDriver, UART2};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys;

#[cfg(feature = "dmx")]
use crate::dmx::DmxMsg;
use crate::State;

/// The CCs go out at most this often.
const CC_INTERVAL: Duration = Duration::from_millis(10);

/// The counters and the clock state for the "output" and "clock" parts of the status.
#[derive(Default)]
pub struct MidiStatus {
    pub pulses: u64,
    pub notes: u64,
    pub ccs: u64,
    pub recent_notes: VecDeque<(u8, Instant)>,
    pub recent_ccs: VecDeque<(u8, u8, Instant)>,
    pub clock_state: &'static str,
    pub out16: u8,
    pub delta_ms: Option<f64>,
    /// The current DMX scene.
    pub scene: SceneStatus,
}

/// A master beat, as the receive thread saw it.
#[derive(Clone, Copy)]
pub struct Beat {
    pub device: u8,
    pub beat_in_bar: u8,
    pub bpm: f64,
    pub at_us: u64,
}

/// Starts the MIDI thread. Returns the queue for the master beats.
///
/// `rx` is not used yet. The UART driver of ESP-IDF 5.5 does not install without a receive
/// buffer, and `UartTxDriver` asks for none. IO35 is input-only, so it can be a MIDI input later.
pub fn start(
    uart: UART2<'static>,
    tx: Gpio17<'static>,
    rx: Gpio35<'static>,
    state: Arc<State>,
    cfg: Arc<Mutex<MidiConfig>>,
    scenes_cfg: Arc<Mutex<ScenesConfig>>,
    #[cfg(feature = "dmx")] dmx: Arc<Queue<DmxMsg>>,
) -> anyhow::Result<Arc<Queue<Beat>>> {
    let uart_cfg = config::Config::new().baudrate(Hertz(31_250));
    let uart = UartDriver::new(uart, tx, rx, None::<Gpio35>, None::<Gpio17>, &uart_cfg)?;
    let beats = Arc::new(Queue::<Beat>::new(16));
    let beat_rx = beats.clone();

    // Priority 10 is above the HTTP server task (5). Core 1 does not run the WiFi task.
    // ponytail: 6 KB is an estimate for the scenes (5 KB before). Measure with /mem on a box.
    crate::spawn_rt(c"midi", 6 * 1024, 10, Some(Core::Core1), move || {
        // SAFETY: a null handle subscribes the calling task.
        if let Err(e) = sys::esp!(unsafe { sys::esp_task_wdt_add(std::ptr::null_mut()) }) {
            log::warn!("The task watchdog does not watch the MIDI thread: {e}");
        }
        let mut clock = Clock::default();
        let mut mapper = Mapper::default();
        let mut scenes = Scenes::default();
        let mut smoother = Smoother::default();
        let mut smoothed_bpm = 0.0;
        let mut last_master = 0u8;
        // The last guess of the loop. The guess keeps a Drop or a Break near its threshold.
        let mut prev_guess = None;
        // The master deck of `prev_guess`.
        let mut guess_dev = 0u8;
        let mut next_pulse: Option<u64> = None;
        let mut next_cc = Instant::now();

        loop {
            // SAFETY: the task subscribed above. A reset of an unsubscribed task only gives an
            // error code.
            unsafe { sys::esp_task_wdt_reset() };
            // Wait for a beat, the next pulse or the next CC update, whichever comes first.
            // The thread spins only when the pulse is the next event and 2 ms or less away.
            let now_us = state.now_us();
            let until_pulse = next_pulse.map(|t| Duration::from_micros(t.saturating_sub(now_us)));
            let until_cc = next_cc.saturating_duration_since(Instant::now());
            let pulse_first = until_pulse.filter(|p| *p <= until_cc);
            let wait = match (pulse_first, next_pulse) {
                (Some(p), Some(t)) if p <= Duration::from_millis(2) => {
                    while state.now_us() < t {
                        std::hint::spin_loop();
                    }
                    None
                }
                (Some(p), _) => Some(p - Duration::from_millis(1)),
                (None, _) => Some(until_cc),
            };
            let ticks = wait.map_or(0, |w| TickType::from(w).ticks());
            let beat = beat_rx.recv_front(ticks).map(|(b, _)| b);

            let cfg = cfg.lock().unwrap().clone();
            let mut status = MidiStatus::default();
            let mut send = |o: Out| {
                let mut buf = [0u8; 6];
                let n = o.encode(&mut buf, cfg.off_vel0);
                if let Err(e) = uart.write(&buf[..n]) {
                    log::warn!("MIDI message not sent: {e}");
                }
                match o {
                    Out::Realtime(CLOCK) => status.pulses += 1,
                    Out::Note { note, .. } | Out::NoteOn { note, .. } => {
                        status.notes += 1;
                        status.recent_notes.push_front((note, Instant::now()));
                    }
                    Out::Cc { num, val, .. } => {
                        status.ccs += 1;
                        status.recent_ccs.push_front((num, val, Instant::now()));
                    }
                    _ => {}
                }
            };

            if let Some(b) = beat {
                if b.device != last_master {
                    smoother.clear();
                    last_master = b.device;
                }
                smoothed_bpm = smoother.push(b.bpm, cfg.smoothing_ms);
                clock.beat(
                    &cfg,
                    b.at_us,
                    b.device,
                    b.beat_in_bar,
                    smoothed_bpm,
                    &mut send,
                );
                mapper.beat(&cfg, b.beat_in_bar, &mut send);
                let scenes_cfg = scenes_cfg.lock().unwrap();
                scenes.beat(&scenes_cfg, state.now_ms(), b.beat_in_bar, &mut |o| {
                    #[cfg(feature = "dmx")]
                    to_dmx(&dmx, &scenes_cfg, o);
                    send(o);
                });
                #[cfg(feature = "dmx")]
                scenes.drain_fired(|e, p| {
                    let _ = dmx.send_back(DmxMsg::Fired(e, p), 0);
                });
            }

            let now_ms = state.now_ms();
            let mut scene_in = SceneInput::default();
            let (mut snap, track) = {
                let tracker = state.tracker.lock().unwrap();
                tracker.master().map_or((Snapshot::default(), None), |m| {
                    let snap = Snapshot {
                        device: m.num,
                        bpm: if m.num == last_master && smoothed_bpm > 0.0 {
                            smoothed_bpm
                        } else {
                            m.bpm
                        },
                        pitch: m.pitch,
                        phrase16: m.phrase16,
                        beat_phase: m.beat_phase(now_ms),
                        playing: m.is_playing(now_ms),
                        phrase: None,
                    };
                    scene_in = SceneInput {
                        device: m.num,
                        track: m.track.rekordbox_id,
                        playing: snap.playing,
                        bpm: snap.bpm,
                        phrase: None,
                        bass_cut: None,
                        guess: None,
                        phrase16: m.phrase16,
                    };
                    // The position in the track, 0.0 to 1.0, for the drop guess.
                    let pos = m
                        .playhead_ms
                        .zip(m.track_len_ms.filter(|&l| l > 0))
                        .map(|(p, l)| f64::from(p) / f64::from(l));
                    (snap, Some((m.track.rekordbox_id, m.beat_count, pos)))
                })
            };
            // Lock the tracks only after the tracker is free.
            if let Some((rbid, beat_count, pos)) = track {
                // A new master deck starts without a previous guess.
                if snap.device != guess_dev {
                    guess_dev = snap.device;
                    prev_guess = None;
                }
                let tracks = state.tracks.0.lock().unwrap();
                let data = tracks.get(&snap.device).filter(|t| t.rekordbox_id == rbid);
                // Only a track without phrases has a guess.
                scene_in.guess =
                    data.and_then(|t| Some(t.guess.as_ref()?.bucket_at(pos?, prev_guess)));
                snap.phrase = beat_count
                    .zip(data.and_then(|t| t.structure.as_ref()?.as_ref().ok()))
                    .and_then(|(beat_count, ss)| {
                        let idx = current_phrase_idx(ss, beat_count)?;
                        let info = phrase_info(ss, idx, beat_count)?;
                        let entry = &ss.phrases[idx];
                        scene_in.phrase = Some(ScenePhrase {
                            idx,
                            code: info.kind.code(),
                            beats_left: info.beats_left,
                            fill_beats_left: entry
                                .has_fill
                                .then(|| u32::from(entry.fill_beat).saturating_sub(beat_count)),
                        });
                        Some(Phrase {
                            rekordbox_id: rbid,
                            idx,
                            code: info.kind.code(),
                            progress: info.progress,
                            beats_left: info.beats_left,
                        })
                    });
            }
            prev_guess = scene_in.guess;
            let master = (snap.device != 0).then_some(Master {
                device: snap.device,
                playing: snap.playing,
                bpm: snap.bpm,
            });
            next_pulse = clock.poll(&cfg, state.now_us(), master, &mut send);
            if Instant::now() >= next_cc {
                mapper.update(&cfg, &snap, &mut send);
                next_cc = Instant::now() + CC_INTERVAL;
            }
            let scene = {
                let scenes_cfg = scenes_cfg.lock().unwrap();
                let test = state.scene_test.swap(u8::MAX, Ordering::Relaxed);
                if test != u8::MAX {
                    // The test always uses the 0x80 note off, as on the host. A DMX controller that
                    // ignores 0x80 then keeps the preset on, so that the user sees it.
                    let mut buf = [0u8; 6];
                    let o = Out::Note {
                        ch: scenes_cfg.channel,
                        note: scenes_cfg.note(test),
                        vel: 127,
                    };
                    let n = o.encode(&mut buf, false);
                    if let Err(e) = uart.write(&buf[..n]) {
                        log::warn!("MIDI message not sent: {e}");
                    }
                    #[cfg(feature = "dmx")]
                    let _ = dmx.send_back(DmxMsg::Test(test), 0);
                }
                scenes.update(&scenes_cfg, state.now_ms(), &scene_in, &mut |o| {
                    #[cfg(feature = "dmx")]
                    to_dmx(&dmx, &scenes_cfg, o);
                    send(o);
                });
                #[cfg(feature = "dmx")]
                scenes.drain_fired(|e, p| {
                    let _ = dmx.send_back(DmxMsg::Fired(e, p), 0);
                });
                scenes.status(&scenes_cfg)
            };

            let mut s = state.midi.lock().unwrap();
            s.pulses += status.pulses;
            s.notes += status.notes;
            s.ccs += status.ccs;
            for n in status.recent_notes.into_iter().rev() {
                s.recent_notes.push_front(n);
            }
            s.recent_notes.truncate(5);
            for c in status.recent_ccs.into_iter().rev() {
                s.recent_ccs.push_front(c);
            }
            s.recent_ccs.truncate(5);
            s.clock_state = clock.state_label();
            s.out16 = clock.out16();
            s.delta_ms = clock.last_error_ms().map(f64::abs);
            s.scene = scene;
        }
    })?;
    log::info!("MIDI output is on IO17");
    Ok(beats)
}

/// Queues a note of a preset for the `dmx` thread. Other messages and the extra notes go only to
/// MIDI. A full queue drops the message, so that the MIDI thread does not wait.
#[cfg(feature = "dmx")]
fn to_dmx(dmx: &Queue<DmxMsg>, c: &ScenesConfig, o: Out) {
    let preset = |ch: u8, note: u8| {
        note.checked_sub(c.first_note)
            .filter(|p| ch == c.channel && *p < c.count)
    };
    let msg = match o {
        Out::NoteOn { ch, note, .. } => preset(ch, note).map(DmxMsg::On),
        Out::NoteOff { ch, note } => preset(ch, note).map(DmxMsg::Off),
        _ => None,
    };
    if let Some(m) = msg {
        let _ = dmx.send_back(m, 0);
    }
}
