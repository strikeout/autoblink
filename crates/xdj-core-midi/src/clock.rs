//! The MIDI clock: 24 pulses per beat, locked to the beats of the master deck.
//!
//! The rules follow `host/src/midi/clock.rs`, in a simpler form:
//! - With Clock Loop Sync on, the clock sends Start on the first downbeat after the master
//!   starts to play, and Stop when the master stops.
//! - With Clock Loop Sync off, the clock starts on the first master beat and does not stop when
//!   the master pauses. A later start sends Song Position and Continue.
//! - Every "Phrase Lock Stable" bars, the clock measures the phase error on the downbeat. It
//!   moves the pulse grid by the error over `max(smoothing, 1 ms, 10 × |error|)`, so the tempo
//!   changes by 10 % or less.
//! - When more than one beat of pulses was late, the clock sends Stop, Start and a pulse on the
//!   next downbeat (a hard resync).
//! - Latency Comp moves the send time of each pulse. The grid stays on the beats.

use crate::config::MidiConfig;
use crate::{Out, CLOCK, CONTINUE, START, STOP};

const PPQ: u64 = 24;

/// The master deck as the clock sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Master {
    pub device: u8,
    pub playing: bool,
    pub bpm: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Waiting,
    Running,
}

#[derive(Debug, Clone, Copy)]
struct Slew {
    start: f64,
    duration: f64,
    total: f64,
    applied: f64,
}

#[derive(Debug, Clone)]
pub struct Clock {
    state: State,
    master: u8,
    has_started: bool,
    /// The time of the last pulse on the grid, in µs. The send time adds the latency.
    grid: f64,
    /// The pulses since Start. A multiple of 24 is on a beat.
    pulse_index: u64,
    bpm: f64,
    /// µs per pulse.
    interval: f64,
    bars: u8,
    dropped: bool,
    slew: Option<Slew>,
    last_error_ms: Option<f64>,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            state: State::Idle,
            master: 0,
            has_started: false,
            grid: 0.0,
            pulse_index: 0,
            bpm: 120.0,
            interval: interval_us(120.0),
            bars: 0,
            dropped: false,
            slew: None,
            last_error_ms: None,
        }
    }
}

fn interval_us(bpm: f64) -> f64 {
    60e6 / (bpm * PPQ as f64)
}

impl Clock {
    /// "lock", "wait" or "idle", like the clock state of the host status.
    pub fn state_label(&self) -> &'static str {
        match self.state {
            State::Running => "lock",
            State::Waiting => "wait",
            State::Idle => "idle",
        }
    }

    /// The beat in the 16-beat block of the clock, 1 to 16. 0 when the clock does not run.
    pub fn out16(&self) -> u8 {
        if self.state == State::Running {
            ((self.pulse_index / PPQ) % 16 + 1) as u8
        } else {
            0
        }
    }

    /// The phase error of the last correction, in ms.
    pub fn last_error_ms(&self) -> Option<f64> {
        self.last_error_ms
    }

    /// Sends the pulses that are due at `now` (µs). Returns the send time of the next pulse.
    pub fn poll(
        &mut self,
        cfg: &MidiConfig,
        now: u64,
        master: Option<Master>,
        out: &mut impl FnMut(Out),
    ) -> Option<u64> {
        if !cfg.clock_enabled {
            self.stop(out);
            self.state = State::Idle;
            self.has_started = false;
            return None;
        }
        let Some(m) = master else {
            self.stop(out);
            self.state = State::Idle;
            self.master = 0;
            return None;
        };
        if m.device != self.master {
            self.stop(out);
            self.master = m.device;
            self.state = State::Waiting;
        }
        if self.state == State::Idle {
            self.state = State::Waiting;
        }
        if cfg.clock_loop_sync && !m.playing && self.state == State::Running {
            self.stop(out);
            self.state = State::Waiting;
        }
        if self.state != State::Running {
            return None;
        }

        self.set_bpm(m.bpm);
        self.apply_slew(now as f64);
        let latency = cfg.latency_ms as f64 * 1000.0;
        let due = ((now as f64 - latency - self.grid) / self.interval).floor();
        if due >= 1.0 {
            let n = due as u64;
            self.grid += n as f64 * self.interval;
            self.pulse_index += n;
            let mut send = n;
            if n > PPQ {
                // The output fell behind by more than one beat. Resync on the next downbeat.
                self.dropped = true;
                send = n % PPQ;
            }
            for _ in 0..send {
                out(Out::Realtime(CLOCK));
            }
        }
        Some((self.grid + self.interval + latency).max(0.0) as u64)
    }

    /// Handles a beat of the master deck that arrived at `now` (µs).
    pub fn beat(
        &mut self,
        cfg: &MidiConfig,
        now: u64,
        device: u8,
        beat_in_bar: u8,
        bpm: f64,
        out: &mut impl FnMut(Out),
    ) {
        if !cfg.clock_enabled || device != self.master || self.state == State::Idle {
            return;
        }
        self.set_bpm(bpm);
        let now_f = now as f64;

        if self.state != State::Running {
            let on_downbeat = beat_in_bar == 1 || !(1..=4).contains(&beat_in_bar);
            if cfg.clock_loop_sync && !on_downbeat {
                return;
            }
            let beat = beat_in_bar.clamp(1, 4) as u64;
            if self.has_started && !cfg.clock_loop_sync {
                out(Out::SongPosition(((beat - 1) * 4) as u16));
                out(Out::Realtime(CONTINUE));
                self.pulse_index = (beat - 1) * PPQ;
            } else {
                out(Out::Realtime(START));
                self.pulse_index = 0;
            }
            out(Out::Realtime(CLOCK));
            self.state = State::Running;
            self.has_started = true;
            self.grid = now_f;
            self.bars = 0;
            self.dropped = false;
            self.slew = None;
            return;
        }

        if beat_in_bar != 1 {
            return;
        }
        if self.dropped {
            out(Out::Realtime(STOP));
            out(Out::Realtime(START));
            out(Out::Realtime(CLOCK));
            self.grid = now_f;
            self.pulse_index = 0;
            self.dropped = false;
            self.slew = None;
            self.bars = 0;
            return;
        }
        self.bars += 1;
        if self.bars < cfg.phrase_lock_bars.max(1) {
            return;
        }
        self.bars = 0;
        let beat_len = self.interval * PPQ as f64;
        let beat_start = self.grid - (self.pulse_index % PPQ) as f64 * self.interval;
        let mut error = now_f - beat_start;
        while error > beat_len / 2.0 {
            error -= beat_len;
        }
        while error <= -beat_len / 2.0 {
            error += beat_len;
        }
        self.last_error_ms = Some(error / 1000.0);
        let duration = (cfg.smoothing_ms as f64 * 1000.0).max(1000.0).max(10.0 * error.abs());
        self.slew = Some(Slew { start: now_f, duration, total: error, applied: 0.0 });
    }

    fn stop(&mut self, out: &mut impl FnMut(Out)) {
        if self.state == State::Running {
            out(Out::Realtime(STOP));
        }
        self.slew = None;
    }

    fn set_bpm(&mut self, bpm: f64) {
        if (20.0..=999.0).contains(&bpm) && (bpm - self.bpm).abs() > 0.01 {
            self.bpm = bpm;
            self.interval = interval_us(bpm);
        }
    }

    fn apply_slew(&mut self, now: f64) {
        let Some(s) = self.slew.as_mut() else { return };
        let frac = ((now - s.start) / s.duration).clamp(0.0, 1.0);
        let want = s.total * frac;
        self.grid += want - s.applied;
        s.applied = want;
        if frac >= 1.0 {
            self.slew = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEAT: u64 = 500_000; // 120 BPM

    fn master(playing: bool) -> Option<Master> {
        Some(Master { device: 1, playing, bpm: 120.0 })
    }

    fn run(f: impl FnOnce(&mut dyn FnMut(Out))) -> Vec<Out> {
        let mut v = Vec::new();
        f(&mut |o| v.push(o));
        v
    }

    fn pulses(v: &[Out]) -> usize {
        v.iter().filter(|o| **o == Out::Realtime(CLOCK)).count()
    }

    /// Starts a clock with Clock Loop Sync on, at a downbeat at time 0.
    fn started(cfg: &MidiConfig) -> Clock {
        let mut c = Clock::default();
        c.poll(cfg, 0, master(true), &mut |_| {});
        c.beat(cfg, 0, 1, 1, 120.0, &mut |_| {});
        c
    }

    #[test]
    fn loop_sync_waits_for_the_downbeat_then_starts() {
        let cfg = MidiConfig::default();
        let mut c = Clock::default();
        c.poll(&cfg, 0, master(true), &mut |_| {});
        assert!(run(|o| c.beat(&cfg, 0, 1, 3, 120.0, &mut |x| o(x))).is_empty());
        assert_eq!(c.state_label(), "wait");
        let out = run(|o| c.beat(&cfg, 1000, 1, 1, 120.0, &mut |x| o(x)));
        assert_eq!(out, [Out::Realtime(START), Out::Realtime(CLOCK)]);
        assert_eq!(c.state_label(), "lock");
    }

    #[test]
    fn sends_24_pulses_per_beat() {
        let cfg = MidiConfig::default();
        let mut c = started(&cfg);
        let mut n = 0;
        let mut now = 0;
        while now < BEAT {
            now += 1000;
            n += pulses(&run(|o| { c.poll(&cfg, now, master(true), &mut |x| o(x)); }));
        }
        // The pulse at time 0 came with Start, so one beat later 24 more are out.
        assert_eq!(n, 24);
        assert_eq!(c.out16(), 2);
    }

    #[test]
    fn latency_moves_the_send_time() {
        let cfg = MidiConfig { latency_ms: 5, ..MidiConfig::default() };
        let mut c = started(&cfg);
        let next = c.poll(&cfg, 0, master(true), &mut |_| {}).unwrap();
        assert_eq!(next, BEAT / 24 + 5_000);
    }

    #[test]
    fn loop_sync_stops_when_the_master_stops() {
        let cfg = MidiConfig::default();
        let mut c = started(&cfg);
        let out = run(|o| { c.poll(&cfg, 10, master(false), &mut |x| o(x)); });
        assert_eq!(out, [Out::Realtime(STOP)]);
        assert_eq!(c.state_label(), "wait");
    }

    #[test]
    fn without_loop_sync_a_restart_continues_at_the_beat() {
        let cfg = MidiConfig { clock_loop_sync: false, ..MidiConfig::default() };
        let mut c = Clock::default();
        c.poll(&cfg, 0, master(true), &mut |_| {});
        let out = run(|o| c.beat(&cfg, 0, 1, 3, 120.0, &mut |x| o(x)));
        assert_eq!(out, [Out::Realtime(START), Out::Realtime(CLOCK)]);
        // A pause does not stop the clock.
        assert!(run(|o| { c.poll(&cfg, 10, master(false), &mut |x| o(x)); }).is_empty());
        // A new master stops it. Its first beat continues from the song position.
        let other = Some(Master { device: 2, playing: true, bpm: 120.0 });
        assert_eq!(run(|o| { c.poll(&cfg, 20, other, &mut |x| o(x)); }), [Out::Realtime(STOP)]);
        let out = run(|o| c.beat(&cfg, 30, 2, 2, 120.0, &mut |x| o(x)));
        assert_eq!(out, [Out::SongPosition(4), Out::Realtime(CONTINUE), Out::Realtime(CLOCK)]);
    }

    #[test]
    fn a_late_downbeat_slews_the_grid() {
        let cfg = MidiConfig { phrase_lock_bars: 1, ..MidiConfig::default() };
        let mut c = started(&cfg);
        // The next downbeat comes 4 beats plus 10 ms later than the grid expects.
        let at = 4 * BEAT + 10_000;
        for t in (0..=at).step_by(1000) {
            c.poll(&cfg, t, master(true), &mut |_| {});
        }
        c.beat(&cfg, at, 1, 1, 120.0, &mut |_| {});
        assert!((c.last_error_ms().unwrap() - 10.0).abs() < 0.1);
        // After the slew time (10 × 10 ms), the grid is 10 ms later.
        let before = c.grid;
        c.poll(&cfg, at + 100_000, master(true), &mut |_| {});
        let moved = c.grid - before;
        let pulses_in_between = (moved - 10_000.0) / c.interval;
        assert!((pulses_in_between - pulses_in_between.round()).abs() < 1e-6);
    }

    #[test]
    fn a_long_stall_resyncs_on_the_next_downbeat() {
        let cfg = MidiConfig::default();
        let mut c = started(&cfg);
        let out = run(|o| { c.poll(&cfg, 2 * BEAT + 1000, master(true), &mut |x| o(x)); });
        // 48 pulses were due. The clock sends only 48 % 24 = 0 of them.
        assert_eq!(pulses(&out), 0);
        let out = run(|o| c.beat(&cfg, 4 * BEAT, 1, 1, 120.0, &mut |x| o(x)));
        assert_eq!(out, [Out::Realtime(STOP), Out::Realtime(START), Out::Realtime(CLOCK)]);
    }

    #[test]
    fn disabling_the_clock_sends_stop() {
        let mut cfg = MidiConfig::default();
        let mut c = started(&cfg);
        cfg.clock_enabled = false;
        let out = run(|o| { c.poll(&cfg, 10, master(true), &mut |x| o(x)); });
        assert_eq!(out, [Out::Realtime(STOP)]);
        assert_eq!(c.state_label(), "idle");
    }
}
