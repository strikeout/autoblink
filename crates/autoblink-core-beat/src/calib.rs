//! The calibrator: it compares the mic grid with the Pro DJ Link beats while a deck plays.
//!
//! The tracker never sees the Pro DJ Link beats. The calibrator learns the offset from the mic
//! grid to the Pro DJ Link grid, the bar rotation and the tempo ratio. While the mic is the
//! master, it stops learning and keeps the last values.

use crate::grid::Grid;

/// The Pro DJ Link beats of one offset window.
const WINDOW: u8 = 32;
/// The offset is the median of this many windows.
const WINDOWS: usize = 8;
/// A new ratio or rotation must agree on this many beats in a row.
const AGREE: u8 = 8;
/// The phase errors that one minute keeps.
const ERRS: usize = 512;
/// One minute, in µs.
const MINUTE_US: u64 = 60_000_000;

/// The tempo of the Pro DJ Link deck over the tempo of the mic grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ratio {
    #[default]
    One,
    /// The deck plays at twice the mic tempo, for example drum & bass at 174 BPM.
    Double,
    /// The deck plays at half the mic tempo, below 80 BPM.
    Half,
}

impl Ratio {
    fn factor(self) -> f64 {
        match self {
            Ratio::One => 1.0,
            Ratio::Double => 2.0,
            Ratio::Half => 0.5,
        }
    }

    fn of(r: f64) -> Option<Ratio> {
        if (0.9..=1.1).contains(&r) {
            Some(Ratio::One)
        } else if (1.8..=2.2).contains(&r) {
            Some(Ratio::Double)
        } else if (0.45..=0.55).contains(&r) {
            Some(Ratio::Half)
        } else {
            None
        }
    }
}

/// The accuracy of the mic against Pro DJ Link over one minute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// The Pro DJ Link beats of the minute.
    pub beats: u32,
    /// The mean tempo error, in 0.01 BPM.
    pub tempo_err_cbpm: u16,
    /// The median and the 95th percentile of the phase error, in ms.
    pub phase_med_ms: u16,
    pub phase_p95_ms: u16,
    /// The Pro DJ Link downbeats where the tracker had bar 1 without the rotation, in percent.
    pub bar1_pct: u8,
    /// The time of the last cold lock of the tracker, in ms.
    pub lock_ms: Option<u32>,
}

/// The calibrator. It allocates nothing after `new`.
pub struct Calibrator {
    offset: f64,
    rotation: u8,
    ratio: Ratio,
    learning: bool,
    ratio_cand: Option<Ratio>,
    ratio_n: u8,
    rot_cand: u8,
    rot_n: u8,
    win_max: f64,
    win_n: u8,
    windows: [f64; WINDOWS],
    windows_len: usize,
    windows_pos: usize,
    /// The beat time, the bar position and the period of the last raw grid of `track`.
    prev: Option<(f64, u8, f64)>,
    errs: [u16; ERRS],
    errs_len: usize,
    tempo_err: f64,
    beats: u32,
    downbeats: u32,
    bar_hits: u32,
    lock_ms: Option<u32>,
    minute_from: Option<u64>,
    last: Stats,
}

/// The beat of `g` nearest `at`.
fn nearest(g: &Grid, at: f64) -> f64 {
    g.beat_us + ((at - g.beat_us) / g.period_us).round() * g.period_us
}

impl Calibrator {
    /// Starts with the stored offset, in µs.
    pub fn new(offset_us: i64) -> Self {
        Self {
            offset: offset_us as f64,
            rotation: 0,
            ratio: Ratio::One,
            learning: true,
            ratio_cand: None,
            ratio_n: 0,
            rot_cand: 0,
            rot_n: 0,
            win_max: f64::MIN,
            win_n: 0,
            windows: [0.0; WINDOWS],
            windows_len: 0,
            windows_pos: 0,
            prev: None,
            errs: [0; ERRS],
            errs_len: 0,
            tempo_err: 0.0,
            beats: 0,
            downbeats: 0,
            bar_hits: 0,
            lock_ms: None,
            minute_from: None,
            last: Stats::default(),
        }
    }

    /// The raw grid with the ratio, `offset` and `rotation`.
    fn mapped(&self, raw: &Grid, offset: f64, rotation: u8) -> Grid {
        let (bar, phrase) = match self.ratio {
            Ratio::One => (raw.bar_pos, raw.phrase_pos),
            Ratio::Double => ((raw.bar_pos * 2) % 4, (raw.phrase_pos * 2) % 16),
            Ratio::Half => (raw.bar_pos / 2, raw.phrase_pos / 2),
        };
        Grid {
            beat_us: raw.beat_us - offset,
            period_us: raw.period_us / self.ratio.factor(),
            bar_pos: (bar + rotation) % 4,
            phrase_pos: (phrase + rotation) % 16,
        }
    }

    /// The calibrated grid: the grid that goes out while the mic is the master.
    pub fn map(&self, raw: &Grid) -> Grid {
        self.mapped(raw, self.offset, self.rotation)
    }

    /// A Pro DJ Link master beat at `at_us`. `raw` is the mic grid on the µs axis, before the
    /// calibration.
    pub fn pdl_beat(&mut self, at_us: u64, beat_in_bar: u8, bpm: f64, raw: Option<&Grid>) {
        let Some(raw) = raw else {
            return;
        };
        let at = at_us as f64;
        if self.learning {
            self.learn(at, beat_in_bar, bpm, raw);
        }
        let g = self.map(raw);
        let err_ms = ((nearest(&g, at) - at).abs() / 1000.0).round();
        if self.errs_len < ERRS {
            self.errs[self.errs_len] = err_ms.min(f64::from(u16::MAX)) as u16;
            self.errs_len += 1;
        }
        self.tempo_err += (bpm - g.bpm()).abs();
        self.beats += 1;
        if beat_in_bar == 1 {
            self.downbeats += 1;
            if self.mapped(raw, self.offset, 0).position(at).0 == 1 {
                self.bar_hits += 1;
            }
        }
    }

    fn learn(&mut self, at: f64, beat_in_bar: u8, bpm: f64, raw: &Grid) {
        if let Some(r) = Ratio::of(bpm / raw.bpm()) {
            if self.ratio_cand == Some(r) {
                self.ratio_n = self.ratio_n.saturating_add(1);
            } else {
                self.ratio_cand = Some(r);
                self.ratio_n = 1;
            }
            if self.ratio_n >= AGREE {
                self.ratio = r;
            }
        }
        // The earliest packet gives the largest distance from the mic beat to the packet.
        let d = nearest(&self.mapped(raw, 0.0, 0), at) - at;
        self.win_max = self.win_max.max(d);
        self.win_n += 1;
        if self.win_n >= WINDOW {
            self.windows[self.windows_pos] = self.win_max;
            self.windows_pos = (self.windows_pos + 1) % WINDOWS;
            self.windows_len = (self.windows_len + 1).min(WINDOWS);
            let mut v = self.windows;
            let v = &mut v[..self.windows_len];
            v.sort_by(f64::total_cmp);
            self.offset = v[v.len() / 2];
            self.win_max = f64::MIN;
            self.win_n = 0;
        }
        if (1..=4).contains(&beat_in_bar) {
            let pos = self.mapped(raw, self.offset, 0).position(at).0 - 1;
            let rot = (beat_in_bar - 1 + 4 - pos) % 4;
            if rot == self.rot_cand {
                self.rot_n = self.rot_n.saturating_add(1);
            } else {
                self.rot_cand = rot;
                self.rot_n = 1;
            }
            if self.rot_n >= AGREE {
                self.rotation = rot;
            }
        }
    }

    /// Notes the raw grid of each pass. While the calibrator does not learn, a move of bar 1 by
    /// the tracker sets the rotation to 0: the tracker is the only source of bar 1 then.
    pub fn track(&mut self, raw: &Grid) {
        if let Some((beat, bar, period)) = self.prev {
            let k = ((raw.beat_us - beat) / period).round() as i64;
            let expected = (i64::from(bar) + k).rem_euclid(4) as u8;
            if !self.learning && expected != raw.bar_pos {
                self.rotation = 0;
            }
        }
        self.prev = Some((raw.beat_us, raw.bar_pos, raw.period_us));
    }

    /// Turns the learning on while a deck plays and off while the mic is the master.
    pub fn set_learning(&mut self, on: bool) {
        self.learning = on;
    }

    /// A Drop after the takeover: the tracker set bar 1 at the kick return.
    pub fn on_drop(&mut self) {
        if !self.learning {
            self.rotation = 0;
        }
    }

    pub fn offset_us(&self) -> i64 {
        self.offset.round() as i64
    }

    pub fn rotation(&self) -> u8 {
        self.rotation
    }

    pub fn ratio(&self) -> Ratio {
        self.ratio
    }

    pub fn note_lock(&mut self, ms: u32) {
        self.lock_ms = Some(ms);
    }

    /// The numbers of the last minute, once each minute while Pro DJ Link beats came.
    pub fn take_minute(&mut self, now_us: u64) -> Option<Stats> {
        let Some(from) = self.minute_from else {
            self.minute_from = Some(now_us);
            return None;
        };
        if now_us < from + MINUTE_US {
            return None;
        }
        self.minute_from = Some(now_us);
        if self.beats == 0 {
            return None;
        }
        let mut v = self.errs;
        let v = &mut v[..self.errs_len];
        v.sort_unstable();
        let pick = |q: usize| {
            v.get((v.len() * q / 100).min(v.len().saturating_sub(1)))
                .copied()
        };
        let s = Stats {
            beats: self.beats,
            tempo_err_cbpm: (self.tempo_err / f64::from(self.beats) * 100.0).round() as u16,
            phase_med_ms: pick(50).unwrap_or(0),
            phase_p95_ms: pick(95).unwrap_or(0),
            bar1_pct: (self.bar_hits * 100)
                .checked_div(self.downbeats)
                .unwrap_or(0) as u8,
            lock_ms: self.lock_ms,
        };
        self.last = s;
        self.errs_len = 0;
        self.tempo_err = 0.0;
        self.beats = 0;
        self.downbeats = 0;
        self.bar_hits = 0;
        Some(s)
    }

    /// The numbers of the last minute.
    pub fn stats(&self) -> Stats {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 500_000.0;

    fn raw(beat_us: f64, period_us: f64, bar_pos: u8) -> Grid {
        Grid {
            beat_us,
            period_us,
            bar_pos,
            phrase_pos: bar_pos,
        }
    }

    fn feed(c: &mut Calibrator, g: &Grid, beats: u64, late: impl Fn(u64) -> f64) {
        for k in 0..beats {
            let at = (k as f64 * 60e6 / 120.0 + late(k)) as u64;
            c.pdl_beat(at, (k % 4 + 1) as u8, 120.0, Some(g));
        }
    }

    #[test]
    fn learns_the_offset_from_the_earliest_packet() {
        let mut c = Calibrator::new(0);
        let g = raw(40_000.0, P, 0);
        let mut x: u32 = 3;
        let mut jitter = |_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            f64::from(x >> 16) / 65_536.0 * 60_000.0
        };
        for k in 0..8 * 32 {
            let at = (k as f64 * P + jitter(k)) as u64;
            c.pdl_beat(at, (k % 4 + 1) as u8, 120.0, Some(&g));
        }
        assert!(
            (c.offset_us() - 40_000).abs() < 2_000,
            "offset {}",
            c.offset_us()
        );
        assert_eq!(c.rotation(), 0);
    }

    #[test]
    fn learns_the_bar_rotation() {
        let mut c = Calibrator::new(0);
        let g = raw(0.0, P, 2);
        feed(&mut c, &g, 8, |_| 0.0);
        assert_eq!(c.rotation(), 2);
        assert_eq!(c.map(&g).bar_pos, 0);
    }

    #[test]
    fn learns_a_double_tempo() {
        let mut c = Calibrator::new(0);
        let pdl = 60e6 / 174.0;
        let g = raw(0.0, 2.0 * pdl, 0);
        for k in 0..16u64 {
            c.pdl_beat((k as f64 * pdl) as u64, (k % 4 + 1) as u8, 174.0, Some(&g));
        }
        assert_eq!(c.ratio(), Ratio::Double);
        let bpm = c.map(&g).bpm();
        assert!((bpm - 174.0).abs() < 0.1, "bpm {bpm}");
    }

    #[test]
    fn a_frozen_calibrator_keeps_its_values() {
        let mut c = Calibrator::new(30_000);
        c.set_learning(false);
        feed(&mut c, &raw(40_000.0, P, 2), 100, |_| 0.0);
        assert_eq!(c.offset_us(), 30_000);
        assert_eq!(c.rotation(), 0);
    }

    #[test]
    fn a_drop_after_the_takeover_resets_the_rotation() {
        let mut c = Calibrator::new(0);
        feed(&mut c, &raw(0.0, P, 2), 8, |_| 0.0);
        assert_eq!(c.rotation(), 2);
        c.set_learning(false);
        c.on_drop();
        assert_eq!(c.rotation(), 0);
    }

    #[test]
    fn a_confident_bar_change_after_the_takeover_wins() {
        let mut c = Calibrator::new(0);
        feed(&mut c, &raw(0.0, P, 2), 8, |_| 0.0);
        c.set_learning(false);
        c.track(&raw(0.0, P, 2));
        // The next beat of the same grid: no change.
        c.track(&raw(P, P, 3));
        assert_eq!(c.rotation(), 2);
        // The tracker moved bar 1.
        c.track(&raw(2.0 * P, P, 2));
        assert_eq!(c.rotation(), 0);
    }

    #[test]
    fn the_minute_numbers() {
        let mut c = Calibrator::new(0);
        c.set_learning(false);
        assert_eq!(c.take_minute(0), None);
        feed(&mut c, &raw(3_000.0, P, 0), 120, |_| 0.0);
        c.note_lock(2_500);
        let s = c.take_minute(60_000_000).expect("a minute");
        assert_eq!(s.beats, 120);
        assert_eq!(s.phase_med_ms, 3);
        assert_eq!(s.phase_p95_ms, 3);
        assert_eq!(s.bar1_pct, 100);
        assert_eq!(s.tempo_err_cbpm, 0);
        assert_eq!(s.lock_ms, Some(2_500));
        assert_eq!(c.stats(), s);
    }

    #[test]
    fn without_beats_no_minute() {
        let mut c = Calibrator::new(0);
        assert_eq!(c.take_minute(0), None);
        assert_eq!(c.take_minute(60_000_000), None);
    }
}
