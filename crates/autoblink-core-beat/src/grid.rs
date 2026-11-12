//! The beat grid on the µs axis of the firmware clock, and the beats that go out.

use crate::tracker::SampleGrid;
use crate::SAMPLE_RATE;

/// The µs of one sample.
const US: f64 = 1e6 / SAMPLE_RATE as f64;
/// The length of one window of the sample clock, in µs.
const WINDOW_US: u64 = 2_000_000;

/// A beat grid on the µs axis of `State::now_us`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    /// The time of one beat of the grid, in µs.
    pub beat_us: f64,
    /// The µs of one beat.
    pub period_us: f64,
    /// The position of that beat in its bar, 0 to 3. 0 is bar 1.
    pub bar_pos: u8,
    /// The position of that beat in its 16-beat block, 0 to 15.
    pub phrase_pos: u8,
}

impl Grid {
    pub fn bpm(&self) -> f64 {
        60e6 / self.period_us
    }

    /// The part of the beat that passed at `now_us`, 0 to 1.
    pub fn beat_phase(&self, now_us: f64) -> f64 {
        ((now_us - self.beat_us) / self.period_us).rem_euclid(1.0)
    }

    /// The beat in the bar (1 to 4) and the beat of the 16-beat block (1 to 16) of the last grid
    /// beat at or before `now_us`.
    pub fn current(&self, now_us: f64) -> (u8, u8) {
        let last =
            self.beat_us + ((now_us - self.beat_us) / self.period_us).floor() * self.period_us;
        self.position(last)
    }

    /// The beat in the bar (1 to 4) and the beat of the 16-beat block (1 to 16) of the grid beat
    /// nearest `at_us`.
    pub fn position(&self, at_us: f64) -> (u8, u8) {
        let k = ((at_us - self.beat_us) / self.period_us).round() as i64;
        (
            (i64::from(self.bar_pos) + k).rem_euclid(4) as u8 + 1,
            (i64::from(self.phrase_pos) + k).rem_euclid(16) as u8 + 1,
        )
    }
}

/// Puts the sample index of the I2S stream on the µs axis.
///
/// The offset is `now_us - samples_end * 62.5` at each read. A late wake of the thread only makes
/// the offset larger. Thus the clock keeps the smallest offset of each window of 2 s.
#[derive(Clone, Copy, Debug, Default)]
pub struct SampleClock {
    win_start_us: u64,
    cur: Option<f64>,
    last: Option<f64>,
}

impl SampleClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that the stream had `samples_end` samples at `now_us`.
    pub fn observe(&mut self, samples_end: u64, now_us: u64) {
        let off = now_us as f64 - samples_end as f64 * US;
        match self.cur {
            Some(cur) if now_us < self.win_start_us + WINDOW_US => self.cur = Some(cur.min(off)),
            _ => {
                self.last = self.cur;
                self.cur = Some(off);
                self.win_start_us = now_us;
            }
        }
    }

    fn offset(&self) -> f64 {
        match (self.cur, self.last) {
            (Some(c), Some(l)) => c.min(l),
            (Some(c), None) => c,
            _ => 0.0,
        }
    }

    pub fn to_us(&self, sample: f64) -> f64 {
        sample * US + self.offset()
    }

    pub fn to_sample(&self, us: f64) -> f64 {
        (us - self.offset()) / US
    }

    pub fn grid(&self, g: &SampleGrid) -> Grid {
        Grid {
            beat_us: self.to_us(g.beat),
            period_us: g.period * US,
            bar_pos: g.bar_pos,
            phrase_pos: g.phrase_pos,
        }
    }
}

/// A beat that goes out to the MIDI clock, the DMX and the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutBeat {
    pub at_us: u64,
    /// 1 to 4.
    pub beat_in_bar: u8,
    /// 1 to 16.
    pub phrase16: u8,
    pub bpm: f64,
}

/// Gives each beat of a grid once, in order.
#[derive(Clone, Copy, Debug, Default)]
pub struct Emitter {
    last_us: Option<f64>,
}

impl Emitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The earliest grid beat after `last + period / 2` and at or before `now_us`. After a gap of
    /// more than 2 periods, only the latest due beat. Thus a phase move never doubles a beat, and
    /// a gap gives no burst.
    pub fn next(&mut self, g: &Grid, now_us: f64) -> Option<OutBeat> {
        let p = g.period_us;
        let latest = g.beat_us + ((now_us - g.beat_us) / p).floor() * p;
        let at = match self.last_us {
            Some(last) if now_us - last <= 2.0 * p => {
                let from = last + p / 2.0;
                let mut t = g.beat_us + ((from - g.beat_us) / p).ceil() * p;
                if t <= from {
                    t += p;
                }
                t
            }
            _ => latest,
        };
        if at > now_us || self.last_us.is_some_and(|last| at <= last + p / 2.0) {
            return None;
        }
        self.last_us = Some(at);
        let (beat_in_bar, phrase16) = g.position(at);
        Some(OutBeat {
            at_us: at.max(0.0) as u64,
            beat_in_bar,
            phrase16,
            bpm: g.bpm(),
        })
    }

    pub fn reset(&mut self) {
        self.last_us = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 500_000.0;

    fn grid(beat_us: f64) -> Grid {
        Grid {
            beat_us,
            period_us: P,
            bar_pos: 0,
            phrase_pos: 0,
        }
    }

    #[test]
    fn late_wakes_do_not_move_the_time() {
        let mut c = SampleClock::new();
        let base = 1_000_000.0;
        let mut x: u32 = 7;
        for block in 1..=625u64 {
            let end = block * 256;
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let late = f64::from(x >> 16) / 65_536.0 * 20_000.0;
            c.observe(end, (base + end as f64 * 62.5 + late) as u64);
        }
        let g = c.grid(&SampleGrid {
            beat: 100_000.0,
            period: 8_000.0,
            bar_pos: 1,
            phrase_pos: 5,
        });
        let want = base + 100_000.0 * 62.5;
        assert!(
            (g.beat_us - want).abs() < 1_000.0,
            "{} µs off",
            g.beat_us - want
        );
        assert!((c.to_us(100_000.0) - want).abs() < 1_000.0);
        assert!((c.to_sample(want) - 100_000.0).abs() < 16.0);
        assert_eq!(g.period_us, 500_000.0);
        assert_eq!((g.bar_pos, g.phrase_pos), (1, 5));
    }

    fn drive(e: &mut Emitter, g: impl Fn(f64) -> Grid, until: f64) -> Vec<OutBeat> {
        let mut out = Vec::new();
        let mut now = 0.0;
        while now < until {
            if let Some(b) = e.next(&g(now), now) {
                out.push(b);
            }
            now += 16_000.0;
        }
        out
    }

    #[test]
    fn emits_each_beat_once() {
        let mut e = Emitter::new();
        let v = drive(&mut e, |_| grid(0.0), 10e6);
        assert_eq!(v.len(), 20);
        for (i, b) in v.iter().enumerate() {
            assert_eq!(b.at_us, i as u64 * 500_000);
        }
    }

    #[test]
    fn a_phase_slew_never_doubles_a_beat() {
        let mut e = Emitter::new();
        let shift = |now: f64| {
            if now < 3e6 {
                grid(0.0)
            } else if now < 6e6 {
                grid(200_000.0)
            } else {
                grid(0.0)
            }
        };
        let v = drive(&mut e, shift, 10e6);
        for w in v.windows(2) {
            assert!(
                w[1].at_us - w[0].at_us >= 250_000,
                "{} then {}",
                w[0].at_us,
                w[1].at_us
            );
        }
        assert!(v.len() >= 18, "{} beats", v.len());
    }

    #[test]
    fn a_gap_emits_no_burst() {
        let mut e = Emitter::new();
        let g = grid(0.0);
        assert_eq!(e.next(&g, 0.0).map(|b| b.at_us), Some(0));
        assert_eq!(e.next(&g, 3e6).map(|b| b.at_us), Some(3_000_000));
        assert_eq!(e.next(&g, 3.016e6), None);
    }

    #[test]
    fn the_position_counts_bars_and_blocks() {
        let g = Grid {
            beat_us: 1e6,
            period_us: P,
            bar_pos: 2,
            phrase_pos: 6,
        };
        assert_eq!(g.position(1e6 + 2.0 * P), (1, 9));
        // The current beat is the last beat at or before the time, also after half a beat.
        assert_eq!(g.current(1e6 + 2.0 * P + 0.75 * P), (1, 9));
        assert_eq!(g.current(1e6 + 2.0 * P), (1, 9));
        assert_eq!(g.position(1e6 - P), (2, 6));
        assert!((g.bpm() - 120.0).abs() < 1e-9);
        assert!((g.beat_phase(1e6 + 1.25 * P) - 0.25).abs() < 1e-9);
    }
}
