//! The beat notes and the CCs. The formulas are the same as in `host/src/midi/mapper.rs`.

use std::collections::VecDeque;

use crate::config::{MidiConfig, CC_COUNT};
use crate::Out;

const PHRASE_16: usize = 7;

/// The state of the master deck that the CCs show.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Snapshot {
    /// The player number of the master. 0 means no master.
    pub device: u8,
    pub bpm: f64,
    pub pitch: f64,
    /// The beat in the 16-beat phrase, 1 to 16. 0 means no beat yet.
    pub phrase16: u8,
    /// The position in the current beat, 0.0 to 1.0.
    pub beat_phase: f64,
    pub playing: bool,
}

fn scale01(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// 127 on beat 1, 64 on beat 3, 80 on beats 2 and 4.
fn beat_velocity(beat_in_bar: u8) -> u8 {
    match beat_in_bar {
        1 => 127,
        3 => 64,
        _ => 80,
    }
}

fn cc_value(i: usize, s: &Snapshot) -> u8 {
    match i {
        0 => (s.bpm - 60.0).clamp(0.0, 127.0) as u8,
        1 => (s.bpm.fract() * 127.0).round() as u8,
        2 => scale01((s.pitch / 10.0 + 1.0) / 2.0),
        // Like the host, the "bar" phase runs over the 16-beat phrase.
        3 if s.phrase16 > 0 => scale01(((s.phrase16 - 1) as f64 + s.beat_phase) / 16.0),
        3 => 0,
        4 => scale01(s.beat_phase),
        5 => if s.playing { 127 } else { 0 },
        _ => s.device.min(127),
    }
}

#[derive(Debug, Clone)]
pub struct Mapper {
    /// The last value sent for each CC. 255 marks a CC that is off, so that it goes out again
    /// when it is switched on. Like the host, the start value 0 is not sent.
    prev: [u8; CC_COUNT],
    prev_phrase16: u8,
}

impl Default for Mapper {
    fn default() -> Self {
        Self { prev: [0; CC_COUNT], prev_phrase16: 0 }
    }
}

impl Mapper {
    /// Sends the beat note on each master beat, and the downbeat note on beat 1.
    pub fn beat(&self, cfg: &MidiConfig, beat_in_bar: u8, out: &mut impl FnMut(Out)) {
        if !cfg.notes_enabled {
            return;
        }
        let ch = cfg.note_channel;
        if cfg.send_beat {
            out(Out::Note { ch, note: cfg.beat_note, vel: beat_velocity(beat_in_bar) });
        }
        if beat_in_bar == 1 && cfg.send_downbeat {
            out(Out::Note { ch, note: cfg.downbeat_note, vel: 127 });
        }
    }

    /// Sends each CC whose value changed.
    pub fn update(&mut self, cfg: &MidiConfig, s: &Snapshot, out: &mut impl FnMut(Out)) {
        let ch = cfg.cc_channel;
        for i in 0..PHRASE_16 {
            let cc = cfg.ccs[i];
            if !cfg.cc_enabled || !cc.on {
                self.prev[i] = 255;
                continue;
            }
            let val = cc_value(i, s);
            if val != self.prev[i] {
                self.prev[i] = val;
                out(Out::Cc { ch, num: cc.num, val });
            }
        }
        // The page help says: "Sends 0 at the start of each 16-beat block". The host never
        // sends it, because its dedupe starts at 0. The firmware sends it as the help says.
        let cc = cfg.ccs[PHRASE_16];
        if cfg.cc_enabled && cc.on && s.phrase16 == 1 && self.prev_phrase16 != 1 {
            out(Out::Cc { ch, num: cc.num, val: 0 });
        }
        self.prev_phrase16 = s.phrase16;
    }
}

/// The moving average of the master BPM. The window is `max(1, round(smoothing_ms / 200))`
/// samples, as on the host. With the default 30 ms, it is one sample.
#[derive(Debug, Clone, Default)]
pub struct Smoother {
    samples: VecDeque<f64>,
}

impl Smoother {
    pub fn push(&mut self, bpm: f64, smoothing_ms: u64) -> f64 {
        let window = ((smoothing_ms as f64 / 200.0).round() as usize).max(1);
        self.samples.push_back(bpm);
        while self.samples.len() > window {
            self.samples.pop_front();
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(f: impl FnOnce(&mut dyn FnMut(Out))) -> Vec<Out> {
        let mut v = Vec::new();
        f(&mut |o| v.push(o));
        v
    }

    fn snap() -> Snapshot {
        Snapshot { device: 2, bpm: 128.5, pitch: 2.0, phrase16: 5, beat_phase: 0.5, playing: true }
    }

    #[test]
    fn beat_notes_use_the_host_velocities() {
        let cfg = MidiConfig::default();
        let m = Mapper::default();
        let out = collect(|o| m.beat(&cfg, 1, &mut |x| o(x)));
        assert_eq!(out, [Out::Note { ch: 9, note: 36, vel: 127 }, Out::Note { ch: 9, note: 37, vel: 127 }]);
        let out = collect(|o| m.beat(&cfg, 3, &mut |x| o(x)));
        assert_eq!(out, [Out::Note { ch: 9, note: 36, vel: 64 }]);
    }

    #[test]
    fn ccs_use_the_host_formulas_and_send_only_changes() {
        let cfg = MidiConfig::default();
        let mut m = Mapper::default();
        let out = collect(|o| m.update(&cfg, &snap(), &mut |x| o(x)));
        let cc = |num, val| Out::Cc { ch: 0, num, val };
        assert_eq!(
            out,
            [cc(1, 68), cc(33, 64), cc(2, 76), cc(3, 36), cc(4, 64), cc(5, 127), cc(6, 2)]
        );
        assert!(collect(|o| m.update(&cfg, &snap(), &mut |x| o(x))).is_empty());
    }

    #[test]
    fn a_cc_that_is_switched_on_again_goes_out_again() {
        let mut cfg = MidiConfig::default();
        let mut m = Mapper::default();
        m.update(&cfg, &snap(), &mut |_| {});
        cfg.ccs[5].on = false;
        m.update(&cfg, &snap(), &mut |_| {});
        cfg.ccs[5].on = true;
        let out = collect(|o| m.update(&cfg, &snap(), &mut |x| o(x)));
        assert_eq!(out, [Out::Cc { ch: 0, num: 5, val: 127 }]);
    }

    #[test]
    fn phrase16_sends_0_at_each_block_start() {
        let cfg = MidiConfig::default();
        let mut m = Mapper::default();
        let at = |p| Snapshot { phrase16: p, ..snap() };
        m.update(&cfg, &at(16), &mut |_| {});
        let out = collect(|o| m.update(&cfg, &at(1), &mut |x| o(x)));
        assert!(out.contains(&Out::Cc { ch: 0, num: 7, val: 0 }));
        let out = collect(|o| m.update(&cfg, &at(1), &mut |x| o(x)));
        assert!(!out.contains(&Out::Cc { ch: 0, num: 7, val: 0 }));
    }

    #[test]
    fn smoother_window_follows_the_setting() {
        let mut s = Smoother::default();
        assert_eq!(s.push(120.0, 30), 120.0);
        assert_eq!(s.push(124.0, 30), 124.0);
        s.clear();
        s.push(120.0, 400);
        assert_eq!(s.push(124.0, 400), 122.0);
    }
}
