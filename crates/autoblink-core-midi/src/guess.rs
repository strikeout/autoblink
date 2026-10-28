//! A guess of the phrase buckets from the energy of the waveform preview.
//!
//! The classifier is for a track without rekordbox phrases. It reads the PWV4 preview once and
//! keeps one byte of energy for each column. It then gives a bucket for each position of the
//! track. The thresholds come from the distribution of the track itself.

use crate::scenes::Event;

/// The most values that the profile keeps.
pub const PROFILE_LEN: usize = 600;
/// The columns that the moving average takes on each side of a column.
const SMOOTH: usize = 4;
/// The preview needs at least this number of columns.
const MIN_COLUMNS: usize = 32;
/// The energy distance from a threshold in which the previous bucket stays.
const HYSTERESIS: u8 = 8;
/// Below this distance of `hi` and `lo`, the track has no loud and no quiet parts.
const MIN_SPREAD: u8 = 24;

/// The energy profile of one track and the thresholds of its buckets.
#[derive(Debug, Clone, PartialEq)]
pub struct DropGuess {
    profile: Vec<u8>,
    hi: u8,
    lo: u8,
    /// The first and the last column with an energy of at least the median.
    first_mid: usize,
    last_mid: usize,
}

impl DropGuess {
    /// Builds the profile from PWV4 preview bytes (6 bytes for each column). It returns `None`
    /// for a preview with fewer than 32 columns.
    pub fn from_preview(preview: &[u8]) -> Option<DropGuess> {
        let cols = preview.len() / 6;
        if cols < MIN_COLUMNS {
            return None;
        }
        let energy = |c: usize| {
            preview[c * 6..c * 6 + 6]
                .iter()
                .map(|&b| u32::from(b))
                .sum::<u32>()
                / 6
        };
        // Fold the columns down to at most `PROFILE_LEN` values.
        let n = cols.min(PROFILE_LEN);
        let folded: Vec<u32> = (0..n)
            .map(|i| {
                let (a, b) = (i * cols / n, ((i + 1) * cols / n).max(i * cols / n + 1));
                (a..b).map(energy).sum::<u32>() / (b - a) as u32
            })
            .collect();
        let profile: Vec<u8> = (0..n)
            .map(|i| {
                let w = &folded[i.saturating_sub(SMOOTH)..(i + SMOOTH + 1).min(n)];
                (w.iter().sum::<u32>() / w.len() as u32) as u8
            })
            .collect();
        let mut sorted = profile.clone();
        sorted.sort_unstable();
        let (hi, lo, mid) = (sorted[n * 70 / 100], sorted[n * 30 / 100], sorted[n / 2]);
        let first_mid = profile.iter().position(|&e| e >= mid)?;
        let last_mid = profile.iter().rposition(|&e| e >= mid)?;
        Some(DropGuess {
            profile,
            hi,
            lo,
            first_mid,
            last_mid,
        })
    }

    /// The bucket at the position `frac` (0.0 to 1.0) of the track. `prev` is the bucket that
    /// the guess gave last. The classifier keeps a Drop or a Break while the energy stays
    /// within a small band of the threshold that started it.
    pub fn bucket_at(&self, frac: f64, prev: Option<Event>) -> Event {
        let n = self.profile.len();
        let i = ((frac.clamp(0.0, 1.0) * n as f64) as usize).min(n - 1);
        if i < self.first_mid {
            return Event::Intro;
        }
        if i > self.last_mid {
            return Event::Outro;
        }
        if self.hi.saturating_sub(self.lo) < MIN_SPREAD {
            return Event::Groove;
        }
        let e = self.profile[i];
        if e >= self.hi || (prev == Some(Event::Drop) && e >= self.hi.saturating_sub(HYSTERESIS)) {
            return Event::Drop;
        }
        if e <= self.lo || (prev == Some(Event::Break) && e <= self.lo.saturating_add(HYSTERESIS)) {
            return Event::Break;
        }
        let ahead = (n / 50).max(1);
        let peak = self.profile[i..(i + 1 + ahead).min(n)]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        if peak >= self.hi {
            Event::Build
        } else {
            Event::Groove
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preview with 6 equal bytes for each column. `runs` is (columns, value).
    fn preview(runs: &[(usize, u8)]) -> Vec<u8> {
        runs.iter()
            .flat_map(|&(n, v)| std::iter::repeat_n(v, n * 6))
            .collect()
    }

    fn guess(runs: &[(usize, u8)]) -> DropGuess {
        DropGuess::from_preview(&preview(runs)).unwrap()
    }

    #[test]
    fn flat_track_is_groove() {
        let g = guess(&[(600, 120)]);
        for f in [0.0, 0.3, 0.5, 0.99, 1.0] {
            assert_eq!(g.bucket_at(f, None), Event::Groove, "{f}");
        }
    }

    #[test]
    fn drop_and_break() {
        let g = guess(&[(50, 20), (150, 200), (150, 20), (200, 200), (50, 20)]);
        let at = |f| g.bucket_at(f, None);
        assert_eq!(at(0.02), Event::Intro);
        assert_eq!(at(0.2), Event::Drop);
        assert_eq!(at(0.5), Event::Break);
        assert_eq!(at(0.7), Event::Drop);
        assert_eq!(at(0.98), Event::Outro);
    }

    #[test]
    fn build_before_drop() {
        let g = guess(&[(200, 100), (200, 120), (200, 200)]);
        let at = |f| g.bucket_at(f, None);
        assert_eq!(at(0.5), Event::Groove);
        assert_eq!(at(0.6), Event::Groove);
        assert_eq!(at(0.665), Event::Build);
        assert_eq!(at(0.8), Event::Drop);
    }

    #[test]
    fn hysteresis_holds_drop() {
        let g = guess(&[(100, 20), (150, 200), (50, 194), (200, 200), (100, 20)]);
        let f = 275.0 / 600.0;
        assert_eq!(g.bucket_at(f, None), Event::Groove);
        assert_eq!(g.bucket_at(f, Some(Event::Drop)), Event::Drop);
    }

    #[test]
    fn short_preview_is_none() {
        assert!(DropGuess::from_preview(&preview(&[(31, 100)])).is_none());
        assert!(DropGuess::from_preview(&preview(&[(32, 100)])).is_some());
    }

    #[test]
    fn folds_1200_to_600() {
        let g = guess(&[(1200, 100)]);
        assert_eq!(g.profile.len(), PROFILE_LEN);
        assert!(g.profile.iter().all(|&e| e == 100));
    }
}
