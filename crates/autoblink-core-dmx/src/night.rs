//! The energy spot: where the music stands relative to this night.
//!
//! The spot is the percentile of the current raw energy in the histogram of this night, plus a
//! prior from past nights. The prior makes the warm-up of a new night read as calm. The night
//! itself rules after about 10 minutes. The module has no clock: the caller gives `now_ms`.

/// The number of histogram bins. A bin covers 8 raw values.
pub const BINS: usize = 32;
/// A deck that plays after this gap without play starts a new night.
pub const NIGHT_GAP_MS: u64 = 2 * 60 * 60 * 1000;
/// The length of `Night::to_bytes`: one version byte, then `now` and `prior` as u16 LE.
pub const NIGHT_BYTES: usize = 4 * BINS + 1;

/// The version byte of the stored format.
const VERSION: u8 = 1;
/// The prior counts as at most this many samples (10 minutes) in `spot`.
const PRIOR_WEIGHT: f64 = 600.0;
/// A bin of `now` that reaches this count halves all bins of `now`.
const BIN_LIMIT: u16 = 30_000;
/// The sum of the prior bins after a fold.
const PRIOR_SUM: f64 = 1000.0;
/// The BPM that maps to raw energy 0 and to raw energy 255.
const BPM_LOW: f64 = 90.0;
const BPM_HIGH: f64 = 150.0;

/// Gives 0..=255 from the BPM and the track energy. Without a track energy, the BPM alone.
pub fn raw_energy(bpm: f64, track: Option<u8>) -> u8 {
    let bpm_part = ((bpm - BPM_LOW) / (BPM_HIGH - BPM_LOW) * 255.0).clamp(0.0, 255.0);
    let v = match track {
        Some(t) => (bpm_part + f64::from(t)) / 2.0,
        None => bpm_part,
    };
    v.round() as u8
}

/// The histograms of this night and of the past nights.
#[derive(Clone, Debug)]
pub struct Night {
    now: [u16; BINS],
    prior: [u16; BINS],
    last_play_ms: Option<u64>,
    last_sample_ms: Option<u64>,
}

impl Default for Night {
    /// The prior is a broad hump around the middle, so that the first night works.
    fn default() -> Self {
        let mut prior = [0u16; BINS];
        for (i, p) in prior.iter_mut().enumerate() {
            let d = (i as f64 - 14.0) / 8.0;
            *p = (100.0 * (-d * d).exp()).round() as u16 + 1;
        }
        Night {
            now: [0; BINS],
            prior,
            last_play_ms: None,
            last_sample_ms: None,
        }
    }
}

impl Night {
    /// Call it for each tick. It counts one sample each second while `playing`. When a deck
    /// plays after `NIGHT_GAP_MS` without play, it folds this night into the prior and returns
    /// true (a new night starts).
    pub fn sample(&mut self, raw: u8, playing: bool, now_ms: u64) -> bool {
        if !playing {
            return false;
        }
        let new_night = self
            .last_play_ms
            .is_some_and(|l| now_ms.saturating_sub(l) > NIGHT_GAP_MS);
        if new_night {
            self.fold();
            self.last_sample_ms = None;
        }
        self.last_play_ms = Some(now_ms);
        if self
            .last_sample_ms
            .is_none_or(|l| now_ms.saturating_sub(l) >= 1000 || now_ms < l)
        {
            self.last_sample_ms = Some(now_ms);
            let bin = usize::from(raw) * BINS / 256;
            self.now[bin] = self.now[bin].saturating_add(1);
            if self.now[bin] >= BIN_LIMIT {
                for b in &mut self.now {
                    *b /= 2;
                }
            }
        }
        new_night
    }

    /// Folds this night into the prior and starts an empty night. The caller uses it after a
    /// restored night that is from an earlier night, for example after a power-on.
    pub fn start_new_night(&mut self) {
        self.fold();
        self.last_play_ms = None;
        self.last_sample_ms = None;
    }

    /// Keeps half of the old prior, adds this night (normalized), and clears this night.
    fn fold(&mut self) {
        let n_sum: f64 = self.now.iter().map(|&b| f64::from(b)).sum();
        if n_sum > 0.0 {
            let p_sum: f64 = self
                .prior
                .iter()
                .map(|&b| f64::from(b))
                .sum::<f64>()
                .max(1.0);
            for (p, &n) in self.prior.iter_mut().zip(&self.now) {
                let v = f64::from(*p) * PRIOR_SUM / 2.0 / p_sum
                    + f64::from(n) * PRIOR_SUM / 2.0 / n_sum;
                *p = v.round() as u16;
            }
        }
        self.now = [0; BINS];
    }

    /// The percentile 0..=100 of `raw` in this night plus the prior.
    pub fn spot(&self, raw: u8) -> u8 {
        let p_sum: f64 = self.prior.iter().map(|&b| f64::from(b)).sum();
        let n_sum: f64 = self.now.iter().map(|&b| f64::from(b)).sum();
        let p_scale = if p_sum > 0.0 {
            PRIOR_WEIGHT / p_sum
        } else {
            0.0
        };
        let total = n_sum + p_sum * p_scale;
        if total <= 0.0 {
            return 50;
        }
        let bin = usize::from(raw) * BINS / 256;
        let w = |i: usize| f64::from(self.now[i]) + f64::from(self.prior[i]) * p_scale;
        let below: f64 = (0..bin).map(w).sum::<f64>() + w(bin) / 2.0;
        (below / total * 100.0).round().clamp(0.0, 100.0) as u8
    }

    /// The stored form: the version byte, then `now` and `prior` as u16 LE. It has no times.
    pub fn to_bytes(&self) -> [u8; NIGHT_BYTES] {
        let mut out = [0u8; NIGHT_BYTES];
        out[0] = VERSION;
        for (i, v) in self.now.iter().chain(&self.prior).enumerate() {
            out[1 + 2 * i..3 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        out
    }

    // `as_chunks` needs Rust 1.88. The esp toolchain is older, so the chunk size stays constant.
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    /// A wrong length or version gives `Night::default()`. The restored night has no play time,
    /// so the next play continues this night. To start a new night, call `start_new_night`.
    pub fn from_bytes(b: &[u8]) -> Night {
        if b.len() != NIGHT_BYTES || b[0] != VERSION {
            return Night::default();
        }
        let mut n = Night::default();
        for (i, c) in b[1..].chunks_exact(2).enumerate() {
            let v = u16::from_le_bytes([c[0], c[1]]);
            if i < BINS {
                n.now[i] = v;
            } else {
                n.prior[i - BINS] = v;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;

    #[test]
    fn raw_energy_orders() {
        assert!(raw_energy(100.0, None) < raw_energy(130.0, None));
        assert!(raw_energy(125.0, Some(50)) < raw_energy(125.0, Some(220)));
        assert_eq!(raw_energy(0.0, None), 0);
    }

    #[test]
    fn warm_up_starts_calm() {
        let mut n = Night::default();
        let mut t = 0;
        for _ in 0..3 {
            for raw in 60..=220u8 {
                n.sample(raw, true, t);
                t += S;
            }
        }
        t += NIGHT_GAP_MS + S;
        assert!(n.sample(70, true, t));
        assert!(n.spot(70) < 34, "spot {}", n.spot(70));
    }

    #[test]
    fn night_adapts() {
        let mut n = Night::default();
        let mut t = 0;
        for _ in 0..1800 {
            n.sample(120, true, t);
            t += S;
        }
        for _ in 0..600 {
            n.sample(200, true, t);
            t += S;
        }
        assert!(n.spot(200) >= 80, "spot {}", n.spot(200));
        assert!(n.spot(120) < 80);
    }

    #[test]
    fn gap_starts_new_night() {
        let mut n = Night::default();
        let t = 5 * S;
        assert!(!n.sample(100, true, t));
        assert!(!n.sample(100, false, t + 3_600_000));
        assert!(n.sample(100, true, t + NIGHT_GAP_MS + S));
        assert!(!n.sample(100, true, t + NIGHT_GAP_MS + 3 * S));
    }

    #[test]
    fn restored_night_continues() {
        let mut n = Night::default();
        n.sample(100, true, 0);
        let mut r = Night::from_bytes(&n.to_bytes());
        assert!(!r.sample(100, true, 10));
    }

    #[test]
    fn start_new_night_folds_into_prior() {
        let mut n = Night::default();
        for i in 0..600u64 {
            n.sample(230, true, i * S);
        }
        let before = Night::default().spot(230);
        n.start_new_night();
        assert!(n.now.iter().all(|&b| b == 0));
        // The prior now holds the loud night, so the same energy reads lower than before.
        assert!(n.spot(230) < before, "{} {}", n.spot(230), before);
        assert!(!n.sample(230, true, 0));
    }

    #[test]
    fn bin_count_saturates() {
        let mut n = Night::default();
        n.now[3] = u16::MAX;
        n.sample(3 * 8, true, 0);
        assert!(n.now[3] < u16::MAX);
    }

    #[test]
    fn bytes_round_trip() {
        let mut n = Night::default();
        for i in 0..500u64 {
            n.sample((i % 200) as u8, true, i * S);
        }
        let r = Night::from_bytes(&n.to_bytes());
        for x in (0..=255u8).step_by(15) {
            assert_eq!(r.spot(x), n.spot(x));
        }
    }

    #[test]
    fn bad_bytes_give_default() {
        assert_eq!(
            Night::from_bytes(&[1, 2, 3]).spot(128),
            Night::default().spot(128)
        );
    }
}
