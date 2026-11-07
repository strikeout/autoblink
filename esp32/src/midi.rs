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
//! With the `dmx` feature, the thread also runs the pilot of the light autopilot. The pilot
//! always runs, so that the night memory learns also while the autopilot is off. While
//! `show.auto.on` is false, the thread drops each action of the pilot. While it is true, the
//! scene engine sends no notes and no DMX messages. The engine still runs for its events and
//! its status.
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
use esp_idf_svc::hal::gpio::{Gpio17, Gpio2};
use esp_idf_svc::hal::task::queue::Queue;
use esp_idf_svc::hal::uart::{config, UartDriver, UART2};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys;

#[cfg(feature = "dmx")]
use crate::dmx::{AutoStatus, DmxMsg};
#[cfg(feature = "dmx")]
use crate::tracks::TrackData;
use crate::State;
#[cfg(feature = "dmx")]
use autoblink_core_dmx::{Action, Auto, EventSet, Night, Pilot, PilotInput, Tier, NIGHT_BYTES};
#[cfg(feature = "dmx")]
use autoblink_core_midi::scenes::Event;

/// The CCs go out at most this often.
const CC_INTERVAL: Duration = Duration::from_millis(10);

/// The thread gives the night memory to the main loop this often, when it changed.
#[cfg(feature = "dmx")]
const NIGHT_SAVE_MS: u64 = 5 * 60 * 1000;

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
/// buffer, and `UartTxDriver` asks for none. IO2 has no wire. The joystick uses IO35.
pub fn start(
    uart: UART2<'static>,
    tx: Gpio17<'static>,
    rx: Gpio2<'static>,
    state: Arc<State>,
    cfg: Arc<Mutex<MidiConfig>>,
    scenes_cfg: Arc<Mutex<ScenesConfig>>,
    #[cfg(feature = "dmx")] dmx: Arc<Queue<DmxMsg>>,
    #[cfg(feature = "dmx")] night: Night,
) -> anyhow::Result<Arc<Queue<Beat>>> {
    let uart_cfg = config::Config::new().baudrate(Hertz(31_250));
    let uart = UartDriver::new(uart, tx, rx, None::<Gpio2>, None::<Gpio17>, &uart_cfg)?;
    let beats = Arc::new(Queue::<Beat>::new(16));
    let beat_rx = beats.clone();

    // Priority 10 is above the HTTP server task (5). Core 1 does not run the WiFi task.
    // ponytail: 6 KB is an estimate for the scenes (5 KB before). The autopilot adds 1 KB. This
    // value is an estimate too. Measure with /mem on a box, and set the size from the low point.
    let stack = if cfg!(feature = "dmx") { 7 } else { 6 } * 1024;
    crate::spawn_rt(c"midi", stack, 10, Some(Core::Core1), move || {
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
        // The master deck and the track of `prev_guess`.
        let mut guess_key = (0u8, 0u32);
        let mut next_pulse: Option<u64> = None;
        let mut next_cc = Instant::now();
        // The pilot holds about 1 KB of arrays. It is on the heap, not on the stack.
        #[cfg(feature = "dmx")]
        let mut ap = Box::new(Autopilot::new(night, state.now_ms()));

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
            // While the autopilot drives, the scene engine sends nothing.
            #[cfg(feature = "dmx")]
            let (quiet, take_over) = {
                let was_on = ap.auto.on;
                ap.refresh(&state);
                (ap.auto.on, ap.auto.on && !was_on)
            };
            #[cfg(not(feature = "dmx"))]
            let quiet = false;
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

            // When the autopilot turns on, the engine releases its held notes and its open
            // flashes. Without this release, the console and the `dmx` thread keep the held
            // preset on under the autopilot.
            #[cfg(feature = "dmx")]
            if take_over {
                let scenes_cfg = scenes_cfg.lock().unwrap();
                scenes.release_all(&mut |o| {
                    to_dmx(&dmx, &scenes_cfg, o);
                    send(o);
                });
            }

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
                    if quiet {
                        return;
                    }
                    #[cfg(feature = "dmx")]
                    to_dmx(&dmx, &scenes_cfg, o);
                    send(o);
                });
                #[cfg(feature = "dmx")]
                scenes.drain_fired(|e, p| {
                    if !quiet {
                        let _ = dmx.send_back(DmxMsg::Fired(e, p), 0);
                    }
                });
            }

            let now_ms = state.now_ms();
            let mut scene_in = SceneInput::default();
            // The track energy and the time of the next drop, for the pilot.
            #[cfg(feature = "dmx")]
            let mut facts = (None, None);
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
                    // The playhead and the length of the track, for the drop guess.
                    let pos = m.playhead_ms.zip(m.track_len_ms);
                    (snap, Some((m.track.rekordbox_id, m.beat_count, pos)))
                })
            };
            // Lock the tracks only after the tracker is free.
            if let Some((rbid, beat_count, pos)) = track {
                // A new master deck or a new track starts without a previous guess.
                if (snap.device, rbid) != guess_key {
                    guess_key = (snap.device, rbid);
                    prev_guess = None;
                }
                let tracks = state.tracks.0.lock().unwrap();
                let data = tracks.get(&snap.device).filter(|t| t.rekordbox_id == rbid);
                #[cfg(feature = "dmx")]
                {
                    facts = pilot_facts(data, pos, beat_count, snap.bpm, now_ms);
                }
                // The scenes use the guess only for a track without phrases.
                let no_phrases = |t: &&crate::tracks::TrackData| {
                    !t.structure
                        .as_ref()
                        .is_some_and(|s| s.as_ref().is_ok_and(|ss| !ss.phrases.is_empty()))
                };
                scene_in.guess = data.filter(no_phrases).and_then(|t| {
                    let (p, l) = pos?;
                    t.guess.as_ref()?.bucket_ahead(p, l, snap.bpm, prev_guess)
                });
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
                    if quiet {
                        return;
                    }
                    #[cfg(feature = "dmx")]
                    to_dmx(&dmx, &scenes_cfg, o);
                    send(o);
                });
                #[cfg(feature = "dmx")]
                scenes.drain_fired(|e, p| {
                    if !quiet {
                        let _ = dmx.send_back(DmxMsg::Fired(e, p), 0);
                    }
                });
                scenes.status(&scenes_cfg)
            };
            #[cfg(feature = "dmx")]
            ap.tick(
                &state,
                &dmx,
                PilotInput {
                    now_ms,
                    playing: snap.playing,
                    bpm: snap.bpm,
                    track_energy: facts.0,
                    // `Autopilot::tick` sets the events.
                    events: EventSet::default(),
                    drop_at_ms: facts.1,
                },
                &scene.events,
                &mut send,
            );

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

/// The autopilot state of the `midi` thread.
#[cfg(feature = "dmx")]
struct Autopilot {
    pilot: Pilot,
    /// The thread keeps a copy of `show.auto`, because it never waits for the show lock.
    auto: Auto,
    /// The value of `State::show_rev` at the copy of `auto`. It is `None` before the first copy.
    rev: Option<u32>,
    /// The value of `auto.on` at the last pass.
    was_on: bool,
    /// The event counters of the scene engine at the last pass.
    events: [u8; 16],
    /// The night memory that the thread gave to the main loop last.
    saved: [u8; NIGHT_BYTES],
    next_save_ms: u64,
}

#[cfg(feature = "dmx")]
impl Autopilot {
    fn new(night: Night, now_ms: u64) -> Self {
        let saved = night.to_bytes();
        let mut pilot = Pilot::default();
        pilot.set_night(night);
        Self {
            pilot,
            auto: Auto::default(),
            rev: None,
            was_on: false,
            events: [0; 16],
            saved,
            next_save_ms: now_ms + NIGHT_SAVE_MS,
        }
    }

    /// Copies `show.auto` after a change of the show. When the `dmx` thread holds the show lock,
    /// the thread keeps the old copy and tries again at the next pass.
    fn refresh(&mut self, state: &State) {
        let rev = state.show_rev.load(Ordering::Relaxed);
        if self.rev == Some(rev) {
            return;
        }
        if let Ok(show) = state.show.try_lock() {
            self.auto = show.auto.clone();
            self.rev = Some(rev);
        }
    }

    /// Runs the pilot for one pass. `events` are the event counters of the scene engine. The
    /// actions go out only while `show.auto.on` is true.
    fn tick(
        &mut self,
        state: &State,
        dmx: &Queue<DmxMsg>,
        mut input: PilotInput,
        events: &[u8; 16],
        send: &mut impl FnMut(Out),
    ) {
        let now_ms = input.now_ms;
        let pin = state.auto_pin.swap(crate::AUTO_PIN_NONE, Ordering::Relaxed);
        if pin != crate::AUTO_PIN_NONE {
            let tier = match pin {
                1 => Some(Tier::Calm),
                2 => Some(Tier::Groove),
                3 => Some(Tier::Peak),
                _ => None,
            };
            self.pilot.pin(tier, now_ms);
        }
        input.events = EventSet::from_counts(&self.events, events);
        self.events = *events;
        let (on, ch) = (self.auto.on, self.auto.channel);
        let mut out = |a: Action| {
            if let Action::Program(prog) = a {
                send(Out::Program { ch, prog });
            }
            // A full queue drops the action, so that this thread does not wait.
            let _ = dmx.send_back(DmxMsg::Auto(a), 0);
        };
        // The thread dropped the actions while the autopilot was off. When it turns on, the
        // thread sends the last picks of the pilot again. Without them, the room shows nothing
        // until the next phrase change.
        if on && !self.was_on {
            let st = self.pilot.status();
            let program = st
                .program
                .and_then(|i| self.auto.programs.get(usize::from(i)));
            if let Some(p) = program {
                out(Action::Program(p.pc));
            }
            out(Action::Moves(st.moves));
            out(Action::Energy {
                tier: st.tier,
                spot: st.spot,
            });
        }
        self.was_on = on;
        self.pilot.tick(&self.auto, &input, &mut |a| {
            if on {
                out(a);
            }
        });
        if let Ok(mut s) = state.auto_status.try_lock() {
            *s = Some(AutoStatus::new(on, self.pilot.status()));
        }
        // The main loop writes the night memory to NVS. Thus a restart loses at most 5 minutes.
        if now_ms >= self.next_save_ms {
            let bytes = self.pilot.night().to_bytes();
            if bytes == self.saved {
                self.next_save_ms = now_ms + NIGHT_SAVE_MS;
            } else if let Ok(mut slot) = state.night.try_lock() {
                *slot = Some(bytes);
                self.saved = bytes;
                self.next_save_ms = now_ms + NIGHT_SAVE_MS;
            }
        }
    }
}

/// The track energy at the playhead and the time of the next Drop phrase, for the pilot.
#[cfg(feature = "dmx")]
fn pilot_facts(
    data: Option<&TrackData>,
    pos: Option<(u32, u32)>,
    beat_count: Option<u32>,
    bpm: f64,
    now_ms: u64,
) -> (Option<u8>, Option<u64>) {
    let Some(t) = data else {
        return (None, None);
    };
    let energy = pos
        .filter(|&(_, len)| len > 0)
        .zip(t.guess.as_ref())
        .map(|((p, len), g)| g.energy_at(f64::from(p) / f64::from(len)));
    let drop_at = beat_count
        .filter(|_| bpm > 0.0)
        .zip(t.structure.as_ref().and_then(|s| s.as_ref().ok()))
        .and_then(|(bc, ss)| {
            let next = ss.phrases.iter().find(|p| {
                u32::from(p.beat) > bc && p.kind.code().and_then(Event::bucket) == Some(Event::Drop)
            })?;
            let beats = f64::from(u32::from(next.beat) - bc);
            Some(now_ms + (beats * 60_000.0 / bpm) as u64)
        });
    (energy, drop_at)
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
