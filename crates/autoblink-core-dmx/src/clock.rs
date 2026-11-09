//! The beat clock of the DMX planner.
//!
//! On WiFi, a beat packet arrives late by up to about 300 ms, because the access
//! point holds a broadcast until the next DTIM beacon. A late packet is never
//! early. Thus the clock keeps an anchor, the predicted time of the last beat,
//! and moves it only to an earlier time.

/// Number of packets that the clock compares to find a constant lateness.
const WINDOW: usize = 8;
/// The lateness in ms above which the whole window moves the anchor.
const LATE_MS: f64 = 20.0;
/// The time in ms after the last packet at which the clock stops.
const STOP_MS: f64 = 2000.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat {
    pub phase: f64,
    pub beat_in_bar: u8,
    pub count: u32,
    pub ms_per_beat: f64,
}

#[derive(Default)]
pub struct BeatClock {
    anchor_ms: Option<f64>,
    count: u32,
    beat_in_bar: u8,
    period_ms: f64,
    last_packet_ms: f64,
    lateness: Vec<f64>,
}

impl BeatClock {
    pub fn beat(&mut self, now_ms: u64, beat_in_bar: u8, bpm: f64) {
        let t = now_ms as f64;
        let old_period = self.period_ms;
        self.period_ms = if bpm > 0.0 { 60000.0 / bpm } else { 500.0 };
        self.last_packet_ms = t;
        let Some(anchor) = self.anchor_ms else {
            self.anchor_ms = Some(t);
            self.count = 0;
            self.beat_in_bar = beat_in_bar;
            return;
        };
        // The prediction uses the period of the last beat, not the new one.
        let period = if old_period > 0.0 {
            old_period
        } else {
            self.period_ms
        };
        let predicted = anchor + period;
        let expected = self.beat_in_bar % 4 + 1;
        let bib_ok = self.beat_in_bar == 0 || beat_in_bar == 0 || beat_in_bar == expected;
        if bib_ok && t >= predicted - period / 2.0 && t <= predicted + period {
            self.count = self.count.wrapping_add(1);
            if beat_in_bar != 0 {
                self.beat_in_bar = beat_in_bar;
            } else if self.beat_in_bar != 0 {
                self.beat_in_bar = expected;
            }
            self.anchor_ms = Some(t.min(predicted));
            self.lateness.push((t - predicted).max(0.0));
            if self.lateness.len() > WINDOW {
                self.lateness.remove(0);
            }
            if self.lateness.len() == WINDOW {
                let min = self.lateness.iter().copied().fold(f64::MAX, f64::min);
                if min > LATE_MS {
                    self.anchor_ms = self.anchor_ms.map(|a| a + min);
                    self.lateness.clear();
                }
            }
        } else {
            // A lost packet leaves a gap. The count goes on by the beats of the gap.
            let gap = ((t - anchor) / period).round().max(1.0) as u32;
            self.anchor_ms = Some(t);
            self.count = self.count.wrapping_add(gap);
            self.beat_in_bar = beat_in_bar;
            self.lateness.clear();
        }
    }

    /// `None` before the first beat, and 2000 ms after the last beat packet.
    pub fn now(&self, now_ms: u64) -> Option<Beat> {
        let anchor = self.anchor_ms?;
        let t = now_ms as f64;
        if t - self.last_packet_ms > STOP_MS {
            return None;
        }
        let n = ((t - anchor) / self.period_ms).floor().max(0.0);
        let phase = ((t - anchor) / self.period_ms - n).clamp(0.0, 1.0);
        let n = n as u32;
        let beat_in_bar = if self.beat_in_bar == 0 {
            0
        } else {
            ((u32::from(self.beat_in_bar) - 1 + n) % 4) as u8 + 1
        };
        Some(Beat {
            phase,
            beat_in_bar,
            count: self.count.wrapping_add(n),
            ms_per_beat: self.period_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_packets_keep_phase() {
        let mut c = BeatClock::default();
        for (i, late) in [0u64, 300, 100, 250, 180].iter().enumerate() {
            c.beat(i as u64 * 500 + late, (i % 4) as u8 + 1, 120.0);
        }
        let b = c.now(2000).unwrap();
        assert!(b.phase < 0.02, "{}", b.phase);
        assert_eq!(b.count, 4);
    }

    #[test]
    fn early_packet_moves_anchor() {
        let mut c = BeatClock::default();
        for i in 0..4u64 {
            c.beat(i * 500, (i % 4) as u8 + 1, 120.0);
        }
        c.beat(2000 - 40, 1, 120.0);
        assert_eq!(c.now(1960).unwrap().phase, 0.0);
    }

    #[test]
    fn resync_counts_lost_beats() {
        let mut c = BeatClock::default();
        c.beat(0, 1, 120.0);
        c.beat(500, 2, 120.0);
        c.beat(1500, 4, 120.0);
        let b = c.now(1500).unwrap();
        assert_eq!(b.count, 3);
        assert_eq!(b.beat_in_bar, 4);
    }

    #[test]
    fn stops_after_2_s() {
        let mut c = BeatClock::default();
        assert!(c.now(0).is_none());
        c.beat(1000, 1, 120.0);
        assert!(c.now(3000).is_some());
        assert!(c.now(3001).is_none());
    }

    #[test]
    fn predicts_between_packets() {
        let mut c = BeatClock::default();
        c.beat(0, 4, 120.0);
        let b = c.now(750).unwrap();
        assert_eq!(b.count, 1);
        assert_eq!(b.beat_in_bar, 1);
        assert_eq!(b.phase, 0.5);
    }
}
