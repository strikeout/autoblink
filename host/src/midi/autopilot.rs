//! The light autopilot on the host. The mapper task owns one [`HostPilot`].
//!
//! The pilot ticks at each pass of the mapper, also while `show.auto.on` is false, so that the
//! night memory learns. It sends its actions only while `show.auto.on` is true.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use autoblink_core_dmx::{Action, Auto, EventSet, Night, Pilot, PilotInput, Tier, NIGHT_BYTES};
use autoblink_core_midi::scenes::Event;

use crate::dmx::{AutoCtl, AutoStatus, DmxMsg, DmxTx, PIN_NONE};
use crate::midi::MidiTransport;
use crate::state::SharedState;

/// The pilot writes the night memory to the disk this often, when it changed.
const NIGHT_SAVE_MS: u64 = 5 * 60 * 1000;

/// What the pilot needs from the master deck at one pass.
#[derive(Clone, Copy, Default)]
pub struct Facts {
    pub playing: bool,
    pub bpm: f64,
    /// The track energy at the playhead, from the drop guess.
    pub energy: Option<u8>,
    /// The time of the next Drop phrase, in ms of the mapper clock.
    pub drop_at_ms: Option<u64>,
}

pub struct HostPilot {
    ctl: Arc<AutoCtl>,
    pilot: Box<Pilot>,
    /// The copy of `show.auto`. The mapper never waits for the show lock.
    auto: Auto,
    /// The value of `ctl.show_rev` at the copy. It is `None` before the first copy.
    rev: Option<u32>,
    /// The value of `auto.on` at the last tick.
    was_on: bool,
    events: [u8; 16],
    saved: [u8; NIGHT_BYTES],
    next_save_ms: u64,
}

impl HostPilot {
    pub fn new(ctl: Arc<AutoCtl>, now_ms: u64) -> Self {
        let saved = *ctl.night.lock();
        let mut pilot = Box::<Pilot>::default();
        pilot.set_night(Night::from_bytes(&saved));
        Self {
            ctl,
            pilot,
            auto: Auto::default(),
            rev: None,
            was_on: false,
            events: [0; 16],
            saved,
            next_save_ms: now_ms + NIGHT_SAVE_MS,
        }
    }

    /// True while the autopilot drives the lights.
    pub fn on(&self) -> bool {
        self.auto.on
    }

    /// Copies `show.auto` after a change of the show. When the show is locked, the pilot keeps
    /// the old copy and tries again at the next pass. Returns true when the pilot turned on.
    pub fn refresh(&mut self) -> bool {
        let was_on = self.auto.on;
        let rev = self.ctl.show_rev.load(Ordering::Relaxed);
        if self.rev != Some(rev) {
            if let Some(show) = self.ctl.show.try_read() {
                self.auto = show.auto.clone();
                self.rev = Some(rev);
            }
        }
        self.auto.on && !was_on
    }

    /// Runs the pilot for one pass. `events` are the event counters of the scene engine.
    pub fn tick(
        &mut self,
        now_ms: u64,
        facts: Facts,
        events: &[u8; 16],
        midi: &dyn MidiTransport,
        dmx: Option<&DmxTx>,
    ) {
        let pin = self.ctl.pin.swap(PIN_NONE, Ordering::Relaxed);
        if pin != PIN_NONE {
            let tier = match pin {
                1 => Some(Tier::Calm),
                2 => Some(Tier::Groove),
                3 => Some(Tier::Peak),
                _ => None,
            };
            self.pilot.pin(tier, now_ms);
        }
        let input = PilotInput {
            now_ms,
            playing: facts.playing,
            bpm: facts.bpm,
            track_energy: facts.energy,
            events: EventSet::from_counts(&self.events, events),
            drop_at_ms: facts.drop_at_ms,
        };
        self.events = *events;
        let (on, ch) = (self.auto.on, self.auto.channel);
        let out = |a: Action| {
            if let Action::Program(prog) = a {
                let _ = midi.send_message(&super::mapper::program_change(ch, prog));
            }
            // A full channel drops the action, so that the mapper does not wait.
            if let Some(tx) = dmx {
                let _ = tx.try_send(DmxMsg::Auto(a));
            }
        };
        // The pilot dropped its actions while it was off. When it turns on, it sends the last
        // picks again. Without them, the room shows nothing until the next phrase change.
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
        if let Some(mut s) = self.ctl.status.try_lock() {
            *s = Some(AutoStatus::new(on, self.pilot.status()));
        }
        if now_ms >= self.next_save_ms {
            self.next_save_ms = now_ms + NIGHT_SAVE_MS;
            self.save_night();
        }
    }

    /// Writes the night memory on a blocking thread, when it changed. A restart of the host then
    /// loses at most 5 minutes.
    fn save_night(&mut self) {
        let bytes = self.pilot.night().to_bytes();
        if bytes == self.saved {
            return;
        }
        self.saved = bytes;
        *self.ctl.night.lock() = bytes;
        if let Some(path) = self.ctl.night_path.clone() {
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::config::save_night(&path, &bytes) {
                    tracing::warn!("The host cannot store the night memory: {e}");
                }
            });
        }
    }
}

/// The track energy at the playhead and the time of the next Drop phrase of the master deck.
/// `now_ms` is the time of the mapper clock.
pub fn facts(state: &SharedState, now_ms: u64) -> Facts {
    let st = state.read();
    let mut f = Facts {
        playing: st.master.is_playing,
        bpm: st.master.bpm,
        ..Facts::default()
    };
    let Some(d) = st.devices.get(&st.master.device_number) else {
        return f;
    };
    f.energy = d
        .playhead_ms
        .zip(d.track_length_ms)
        .filter(|&(_, len)| len > 0)
        .zip(d.guess.as_ref())
        .map(|((p, len), g)| g.energy_at(f64::from(p) / f64::from(len)));
    f.drop_at_ms = (f.bpm > 0.0)
        .then_some(d.beat_count)
        .zip(d.song_structure.as_ref())
        .and_then(|(bc, ss)| {
            let next = ss.phrases.iter().find(|p| {
                u32::from(p.beat) > bc && p.kind.code().and_then(Event::bucket) == Some(Event::Drop)
            })?;
            let beats = f64::from(u32::from(next.beat) - bc);
            Some(now_ms + (beats * 60_000.0 / f.bpm) as u64)
        });
    f
}
