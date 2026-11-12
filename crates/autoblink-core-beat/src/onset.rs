//! The onsets of four bands: the kick, the clap, the high band and the full band.
//!
//! For each hop of `HOP` samples, each band gives the rise of its log energy. A fall gives 0.

use crate::{HOP, SAMPLE_RATE};
use std::f32::consts::TAU;

/// The coefficient of a one-pole envelope with a time constant of 6 ms (each kick stage).
const KICK_C: f32 = 1.0 / (0.006 * SAMPLE_RATE as f32);
/// The coefficient of a one-pole envelope with a time constant of 3 ms.
const HIGH_C: f32 = 1.0 / (0.003 * SAMPLE_RATE as f32);

/// One hop of the onsets.
#[derive(Clone, Copy, Debug, Default)]
pub struct Frame {
    /// The rise of the log energy below about 150 Hz.
    pub kick: f32,
    /// The rise of the log energy from about 1 to 4 kHz.
    pub clap: f32,
    /// The rise of the log energy above 30 Hz.
    pub full: f32,
    /// The rise of the log energy above about 4 kHz: crashes, hats and new layers.
    pub high: f32,
    /// The level of the hop, in dBFS.
    pub dbfs: f32,
    /// The largest sample minus the smallest sample of the hop.
    pub span: u16,
}

/// A second-order filter (RBJ cookbook), direct form 1.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    fn new(hz: f32, low: bool) -> Self {
        let w = TAU * hz / SAMPLE_RATE as f32;
        let alpha = w.sin() / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let c = w.cos();
        let a0 = 1.0 + alpha;
        let b = if low {
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0]
        } else {
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0]
        };
        Self {
            b: b.map(|v| v / a0),
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            ..Self::default()
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// The band filters and the energy of the hop that is not full yet.
pub struct Onsets {
    kick: [Biquad; 2],
    clap: [Biquad; 3],
    full: Biquad,
    high: [Biquad; 2],
    /// The smoothed power of the kick (two stages), clap, full and high bands.
    env: [f32; 5],
    /// The sum of squares of the full band of the hop.
    sum: f32,
    min: i16,
    max: i16,
    /// The last sample, so that a step between two hops counts in `span`.
    prev: i16,
    n: usize,
    /// The log energy of the last hop for each band.
    last: [f32; 4],
}

impl Default for Onsets {
    fn default() -> Self {
        Self::new()
    }
}

impl Onsets {
    pub fn new() -> Self {
        Self {
            kick: [Biquad::new(150.0, true); 2],
            clap: [
                Biquad::new(1000.0, false),
                Biquad::new(1000.0, false),
                Biquad::new(4000.0, true),
            ],
            full: Biquad::new(30.0, false),
            high: [Biquad::new(4000.0, false); 2],
            env: [0.0; 5],
            sum: 0.0,
            min: 0,
            max: 0,
            prev: 0,
            n: 0,
            last: [0.0; 4],
        }
    }

    /// Filters `samples`. Calls `out` once for each full hop. A partial hop waits for the next
    /// call.
    pub fn push(&mut self, samples: &[i16], out: &mut impl FnMut(Frame)) {
        for &s in samples {
            let x = f32::from(s);
            let k = self.kick.iter_mut().fold(x, |v, f| f.run(v));
            let c = self.clap.iter_mut().fold(x, |v, f| f.run(v));
            let f = self.full.run(x);
            let h = self.high.iter_mut().fold(x, |v, f| f.run(v));
            // A power over 8 ms ripples with a 50 Hz kick. The envelopes smooth that ripple.
            self.env[0] += KICK_C * (k * k - self.env[0]);
            self.env[1] += KICK_C * (self.env[0] - self.env[1]);
            self.env[2] += HIGH_C * (c * c - self.env[2]);
            self.env[3] += HIGH_C * (f * f - self.env[3]);
            self.env[4] += HIGH_C * (h * h - self.env[4]);
            // The level uses the full band, so that a DC offset of the mic is no level.
            self.sum += f * f;
            self.min = self.min.min(s);
            self.max = self.max.max(s);
            self.prev = s;
            self.n += 1;
            if self.n == HOP {
                out(self.finish());
            }
        }
    }

    /// The samples of the hop that is not full yet.
    pub fn partial(&self) -> usize {
        self.n
    }

    fn finish(&mut self) -> Frame {
        let n = HOP as f32;
        let mut rise = [0.0f32; 4];
        for (r, (env, last)) in rise.iter_mut().zip(
            [self.env[1], self.env[2], self.env[3], self.env[4]]
                .iter()
                .zip(&mut self.last),
        ) {
            let e = (1.0 + env / 1e3).ln();
            *r = (e - *last).max(0.0);
            *last = e;
        }
        let power = self.sum / n / (32768.0 * 32768.0);
        let frame = Frame {
            kick: rise[0],
            clap: rise[1],
            full: rise[2],
            high: rise[3],
            dbfs: if power > 0.0 {
                10.0 * power.log10()
            } else {
                -120.0
            },
            span: (i32::from(self.max) - i32::from(self.min)) as u16,
        };
        self.sum = 0.0;
        self.min = self.prev;
        self.max = self.prev;
        self.n = 0;
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{beat_times, render, Track};
    use crate::{HOP, SAMPLE_RATE};

    fn frames(pcm: &[i16]) -> Vec<Frame> {
        let mut o = Onsets::new();
        let mut v = Vec::new();
        for block in pcm.chunks(250) {
            o.push(block, &mut |f| v.push(f));
        }
        v
    }

    fn frame_at(t: f64) -> usize {
        (t * f64::from(SAMPLE_RATE) / HOP as f64) as usize
    }

    fn max_in(v: &[Frame], at: usize, f: impl Fn(&Frame) -> f32) -> f32 {
        v[at.saturating_sub(2)..(at + 3).min(v.len())]
            .iter()
            .map(f)
            .fold(0.0, f32::max)
    }

    #[test]
    fn the_kick_band_peaks_on_each_kick() {
        let t = Track {
            clap_on_2_4: false,
            bar_change: false,
            ..Track::default()
        };
        let v = frames(&render(&t, 10.0));
        let top = v.iter().map(|f| f.kick).fold(0.0, f32::max);
        assert!(top > 0.0);
        let beats = beat_times(&t, 9.5);
        for w in beats.windows(2).skip(1) {
            let at = frame_at(w[0].0);
            assert!(
                max_in(&v, at, |f| f.kick) > 0.5 * top,
                "no kick at {}",
                w[0].0
            );
            let mid = frame_at((w[0].0 + w[1].0) / 2.0);
            assert!(
                max_in(&v, mid, |f| f.kick) < 0.1 * top,
                "kick between beats at {}",
                w[0].0
            );
        }
    }

    #[test]
    fn the_clap_band_peaks_on_2_and_4() {
        let t = Track {
            bar_change: false,
            ..Track::default()
        };
        let v = frames(&render(&t, 10.0));
        let (mut on, mut off) = (0.0, 0.0);
        for (at, bib) in beat_times(&t, 9.5).into_iter().skip(1) {
            let c = max_in(&v, frame_at(at), |f| f.clap);
            if bib % 2 == 0 {
                on += c;
            } else {
                off += c;
            }
        }
        assert!(on > 4.0 * off, "clap on 2 and 4 {on}, on 1 and 3 {off}");
    }

    #[test]
    fn a_dc_offset_is_no_level() {
        let v = frames(&vec![5000; SAMPLE_RATE as usize]);
        assert!(
            v.last().unwrap().dbfs < -90.0,
            "dbfs {}",
            v.last().unwrap().dbfs
        );
    }

    #[test]
    fn silence_gives_a_low_dbfs_and_zero_span() {
        let v = frames(&vec![0; SAMPLE_RATE as usize]);
        assert_eq!(v.len(), SAMPLE_RATE as usize / HOP);
        for f in v {
            assert!(f.dbfs < -90.0);
            assert_eq!(f.span, 0);
        }
    }
}
