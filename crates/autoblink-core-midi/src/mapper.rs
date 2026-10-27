//! The beat notes and the CCs. The formulas are the same as in `host/src/midi/mapper.rs`.

use std::collections::VecDeque;

use crate::config::{MidiConfig, CC_COUNT};
use crate::Out;

const PHRASE_16: usize = 7;
const PHRASE_KIND: usize = 8;
const PHRASE_PROGRESS: usize = 9;
const BEATS_LEFT: usize = 10;

/// The current phrase of the master deck, from the song structure of its track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phrase {
    pub rekordbox_id: u32,
    /// The index of the phrase in the song structure.
    pub idx: usize,
    /// The kind code: Intro 0, Verse 1 to 6, Bridge 7, Chorus 8, Up 9, Down 10, Outro 11.
    /// `None` for a kind that has no code. The kind messages then do not go out.
    pub code: Option<u8>,
    pub progress: f64,
    pub beats_left: u32,
}

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
    /// `None` when the track has no song structure, or the beat number is not known.
    pub phrase: Option<Phrase>,
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
        5 => {
            if s.playing {
                127
            } else {
                0
            }
        }
        _ => s.device.min(127),
    }
}

#[derive(Debug, Clone)]
pub struct Mapper {
    /// The last value sent for each CC. 255 marks a CC that is off, so that it goes out again
    /// when it is switched on. Like the host, the start value 0 is not sent.
    prev: [u8; CC_COUNT],
    prev_phrase16: u8,
    prev_phrase_idx: Option<usize>,
    /// The (device, track, phrase) that the phrase kind messages went out for.
    kind_sent_for: Option<(u8, u32, usize)>,
}

impl Default for Mapper {
    fn default() -> Self {
        Self {
            prev: [0; CC_COUNT],
            prev_phrase16: 0,
            prev_phrase_idx: None,
            kind_sent_for: None,
        }
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
            out(Out::Note {
                ch,
                note: cfg.beat_note,
                vel: beat_velocity(beat_in_bar),
            });
        }
        if beat_in_bar == 1 && cfg.send_downbeat {
            out(Out::Note {
                ch,
                note: cfg.downbeat_note,
                vel: 127,
            });
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
                out(Out::Cc {
                    ch,
                    num: cc.num,
                    val,
                });
            }
        }
        // The page help says: "Sends 0 at the start of each 16-beat block". The host never
        // sends it, because its dedupe starts at 0. The firmware sends it as the help says.
        let cc = cfg.ccs[PHRASE_16];
        if cfg.cc_enabled && cc.on && s.phrase16 == 1 && self.prev_phrase16 != 1 {
            out(Out::Cc {
                ch,
                num: cc.num,
                val: 0,
            });
        }
        self.prev_phrase16 = s.phrase16;
        self.update_phrase(cfg, s, out);
    }

    /// The phrase messages, as on the host:
    /// - The phrase change note goes out when the phrase index changes from one phrase to another.
    /// - The phrase kind CC (code × 10) and note (base + code) go out once for each phrase of
    ///   each track. They therefore also go out after a track load or a master change.
    /// - The progress and beats left CCs go out when they change.
    fn update_phrase(&mut self, cfg: &MidiConfig, s: &Snapshot, out: &mut impl FnMut(Out)) {
        let idx = s.phrase.map(|p| p.idx);
        if let (Some(prev), Some(cur)) = (self.prev_phrase_idx, idx) {
            if prev != cur && cfg.notes_enabled && cfg.send_phrase_change {
                out(Out::Note {
                    ch: cfg.note_channel,
                    note: cfg.phrase_change_note,
                    vel: 127,
                });
            }
        }
        self.prev_phrase_idx = idx;

        let Some(p) = s.phrase else { return };
        let key = (s.device, p.rekordbox_id, p.idx);
        if let Some(code) = p.code.filter(|_| self.kind_sent_for != Some(key)) {
            self.kind_sent_for = Some(key);
            let cc = cfg.ccs[PHRASE_KIND];
            if cfg.cc_enabled && cc.on {
                out(Out::Cc {
                    ch: cfg.cc_channel,
                    num: cc.num,
                    val: (code * 10).min(127),
                });
            }
            if cfg.notes_enabled && cfg.send_phrase_kind {
                let note = cfg.phrase_base_note.saturating_add(code).min(127);
                out(Out::Note {
                    ch: cfg.note_channel,
                    note,
                    vel: 127,
                });
            }
        }
        for (i, val) in [
            (PHRASE_PROGRESS, scale01(p.progress)),
            (BEATS_LEFT, p.beats_left.min(127) as u8),
        ] {
            let cc = cfg.ccs[i];
            if !cfg.cc_enabled || !cc.on {
                self.prev[i] = 255;
            } else if val != self.prev[i] {
                self.prev[i] = val;
                out(Out::Cc {
                    ch: cfg.cc_channel,
                    num: cc.num,
                    val,
                });
            }
        }
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
        Snapshot {
            device: 2,
            bpm: 128.5,
            pitch: 2.0,
            phrase16: 5,
            beat_phase: 0.5,
            playing: true,
            phrase: None,
        }
    }

    #[test]
    fn beat_notes_use_the_host_velocities() {
        let cfg = MidiConfig::default();
        let m = Mapper::default();
        let out = collect(|o| m.beat(&cfg, 1, &mut |x| o(x)));
        assert_eq!(
            out,
            [
                Out::Note {
                    ch: 9,
                    note: 36,
                    vel: 127
                },
                Out::Note {
                    ch: 9,
                    note: 37,
                    vel: 127
                }
            ]
        );
        let out = collect(|o| m.beat(&cfg, 3, &mut |x| o(x)));
        assert_eq!(
            out,
            [Out::Note {
                ch: 9,
                note: 36,
                vel: 64
            }]
        );
    }

    #[test]
    fn ccs_use_the_host_formulas_and_send_only_changes() {
        let cfg = MidiConfig::default();
        let mut m = Mapper::default();
        let out = collect(|o| m.update(&cfg, &snap(), &mut |x| o(x)));
        let cc = |num, val| Out::Cc { ch: 0, num, val };
        assert_eq!(
            out,
            [
                cc(1, 68),
                cc(33, 64),
                cc(2, 76),
                cc(3, 36),
                cc(4, 64),
                cc(5, 127),
                cc(6, 2)
            ]
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
        assert_eq!(
            out,
            [Out::Cc {
                ch: 0,
                num: 5,
                val: 127
            }]
        );
    }

    #[test]
    fn phrase16_sends_0_at_each_block_start() {
        let cfg = MidiConfig::default();
        let mut m = Mapper::default();
        let at = |p| Snapshot {
            phrase16: p,
            ..snap()
        };
        m.update(&cfg, &at(16), &mut |_| {});
        let out = collect(|o| m.update(&cfg, &at(1), &mut |x| o(x)));
        assert!(out.contains(&Out::Cc {
            ch: 0,
            num: 7,
            val: 0
        }));
        let out = collect(|o| m.update(&cfg, &at(1), &mut |x| o(x)));
        assert!(!out.contains(&Out::Cc {
            ch: 0,
            num: 7,
            val: 0
        }));
    }

    #[test]
    fn phrase_messages_follow_the_host_rules() {
        let cfg = MidiConfig::default();
        let mut m = Mapper::default();
        let phrase = |idx, code, beats_left| Phrase {
            rekordbox_id: 207,
            idx,
            code: Some(code),
            progress: 0.5,
            beats_left,
        };
        let at = |p| Snapshot {
            phrase: Some(p),
            ..snap()
        };
        let out = collect(|o| m.update(&cfg, &at(phrase(0, 1, 16)), &mut |x| o(x)));
        // The first phrase: kind CC 10, kind note 40 + 1, progress 64, beats left 16. No change note.
        assert!(out.contains(&Out::Cc {
            ch: 0,
            num: 20,
            val: 10
        }));
        assert!(out.contains(&Out::Note {
            ch: 9,
            note: 41,
            vel: 127
        }));
        assert!(out.contains(&Out::Cc {
            ch: 0,
            num: 21,
            val: 64
        }));
        assert!(out.contains(&Out::Cc {
            ch: 0,
            num: 22,
            val: 16
        }));
        assert!(!out.contains(&Out::Note {
            ch: 9,
            note: 38,
            vel: 127
        }));
        // The same phrase again sends nothing new.
        let out = collect(|o| m.update(&cfg, &at(phrase(0, 1, 16)), &mut |x| o(x)));
        assert!(out.is_empty());
        // The next phrase: the change note and the new kind.
        let out = collect(|o| m.update(&cfg, &at(phrase(1, 8, 32)), &mut |x| o(x)));
        assert!(out.contains(&Out::Note {
            ch: 9,
            note: 38,
            vel: 127
        }));
        assert!(out.contains(&Out::Cc {
            ch: 0,
            num: 20,
            val: 80
        }));
        assert!(out.contains(&Out::Note {
            ch: 9,
            note: 48,
            vel: 127
        }));
        // A phrase with an unknown kind still sends the change note and the progress CCs.
        let unknown = Phrase {
            code: None,
            ..phrase(2, 0, 8)
        };
        let out = collect(|o| m.update(&cfg, &at(unknown), &mut |x| o(x)));
        assert!(out.contains(&Out::Note {
            ch: 9,
            note: 38,
            vel: 127
        }));
        assert!(out.contains(&Out::Cc {
            ch: 0,
            num: 22,
            val: 8
        }));
        assert!(!out.iter().any(|o| matches!(o, Out::Cc { num: 20, .. })));
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
