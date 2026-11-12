//! Test signals: a synthetic club track with a kick on each beat.
//!
//! The first beat is a downbeat at `offset_s`. The noise comes from a fixed seed, so each run
//! gives the same samples.

use crate::SAMPLE_RATE;
use std::f64::consts::TAU;

/// The parts of a synthetic track.
#[derive(Debug, Clone)]
pub struct Track {
    pub bpm: f64,
    /// The tempo at the end of a ramp. `None` keeps `bpm`.
    pub bpm_end: Option<f64>,
    /// The beats of the ramp from `bpm` to `bpm_end`.
    pub ramp_beats: u32,
    /// The time of the first beat, in s.
    pub offset_s: f64,
    pub kick: bool,
    pub clap_on_2_4: bool,
    /// A bass note that changes on each bar, and a crash on beat 1.
    pub bar_change: bool,
    /// White noise, in dB against the peak of the kick.
    pub noise_db: Option<f64>,
    /// The first beat and the number of beats without the kick and the clap.
    pub breakdown: Option<(u32, u32)>,
    /// A second kick at -6 dB: the delay in ms, the first beat and the number of beats.
    pub flam: Option<(f64, u32, u32)>,
    /// Multiplies the signal by 8 and saturates it.
    pub clip: bool,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            bpm: 124.0,
            bpm_end: None,
            ramp_beats: 0,
            offset_s: 0.0,
            kick: true,
            clap_on_2_4: true,
            bar_change: true,
            noise_db: None,
            breakdown: None,
            flam: None,
            clip: false,
        }
    }
}

impl Track {
    fn in_breakdown(&self, k: u32) -> bool {
        self.breakdown
            .is_some_and(|(first, n)| k >= first && k < first + n)
    }
}

/// The true beats: (time in s, beat in the bar 1 to 4).
pub fn beat_times(t: &Track, seconds: f64) -> Vec<(f64, u8)> {
    let mut out = Vec::new();
    let mut at = t.offset_s;
    let mut k = 0u32;
    while at < seconds {
        out.push((at, (k % 4 + 1) as u8));
        let bpm = match t.bpm_end {
            Some(end) if t.ramp_beats > 0 => {
                let f = (f64::from(k) / f64::from(t.ramp_beats)).min(1.0);
                t.bpm + (end - t.bpm) * f
            }
            _ => t.bpm,
        };
        at += 60.0 / bpm;
        k += 1;
    }
    out
}

struct Lcg(u64);

impl Lcg {
    /// A value from -1 to 1.
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
    }
}

/// A two-pole resonator, for band noise.
fn resonate(noise: &[f64], hz: f64, r: f64) -> Vec<f64> {
    let w = TAU * hz / f64::from(SAMPLE_RATE);
    let (a1, a2) = (2.0 * r * w.cos(), -r * r);
    let (mut y1, mut y2) = (0.0, 0.0);
    noise
        .iter()
        .map(|x| {
            let y = x * (1.0 - r) + a1 * y1 + a2 * y2;
            y2 = y1;
            y1 = y;
            y
        })
        .collect()
}

fn add_kick(buf: &mut [f64], start: usize, amp: f64) {
    let sr = f64::from(SAMPLE_RATE);
    let mut phase = 0.0;
    for (i, s) in buf
        .iter_mut()
        .skip(start)
        .take((0.4 * sr) as usize)
        .enumerate()
    {
        let t = i as f64 / sr;
        let hz = 45.0 + 10.0 * (-t / 0.05).exp();
        phase += TAU * hz / sr;
        *s += amp * (-t / 0.12).exp() * phase.sin();
    }
}

fn add_burst(buf: &mut [f64], band: &[f64], start: usize, amp: f64, decay_s: f64, len_s: f64) {
    let sr = f64::from(SAMPLE_RATE);
    let n = (len_s * sr) as usize;
    for i in 0..n {
        let Some(s) = buf.get_mut(start + i) else {
            break;
        };
        let t = i as f64 / sr;
        *s += amp * (-t / decay_s).exp() * band[(start + i) % band.len()];
    }
}

/// Renders `seconds` of the track at `SAMPLE_RATE`.
pub fn render(t: &Track, seconds: f64) -> Vec<i16> {
    let sr = f64::from(SAMPLE_RATE);
    let len = (seconds * sr) as usize;
    let mut buf = vec![0.0f64; len];
    let mut rng = Lcg(0x5eed);
    let white: Vec<f64> = (0..sr as usize).map(|_| rng.next()).collect();
    // The resonators lose gain at a narrow band, so scale them back to about full scale.
    let clap_band: Vec<f64> = resonate(&white, 2000.0, 0.9)
        .iter()
        .map(|x| x * 4.0)
        .collect();
    let crash_band: Vec<f64> = resonate(&white, 6000.0, 0.8)
        .iter()
        .map(|x| x * 3.0)
        .collect();
    let beats = beat_times(t, seconds);
    for (k, &(at, bib)) in beats.iter().enumerate() {
        let k = k as u32;
        let start = (at * sr) as usize;
        if !t.in_breakdown(k) {
            if t.kick {
                add_kick(&mut buf, start, 0.5);
                if let Some((ms, first, n)) = t.flam {
                    if k >= first && k < first + n {
                        add_kick(&mut buf, start + (ms / 1000.0 * sr) as usize, 0.25);
                    }
                }
            }
            if t.clap_on_2_4 && (bib == 2 || bib == 4) {
                add_burst(&mut buf, &clap_band, start, 0.25, 0.02, 0.06);
            }
        }
        if t.bar_change && bib == 1 {
            add_burst(&mut buf, &crash_band, start, 0.1, 0.1, 0.3);
        }
    }
    if t.bar_change {
        // A bass note that changes its pitch at each bar start.
        const STEPS: [f64; 4] = [1.0, 1.12, 0.89, 1.33];
        let bars: Vec<f64> = beats.iter().filter(|b| b.1 == 1).map(|b| b.0).collect();
        let mut phase = 0.0;
        let mut bar = 0;
        for (i, s) in buf.iter_mut().enumerate() {
            let now = i as f64 / sr;
            while bars.get(bar + 1).is_some_and(|&b| b <= now) {
                bar += 1;
            }
            phase += TAU * 70.0 * STEPS[bar % 4] / sr;
            *s += 0.1 * phase.sin();
        }
    }
    if let Some(db) = t.noise_db {
        // RMS of uniform noise in -1..1 is 1/sqrt(3).
        let amp = 0.5 * 10f64.powf(db / 20.0) * 3f64.sqrt();
        for s in buf.iter_mut() {
            *s += amp * rng.next();
        }
    }
    let gain = if t.clip { 4.0 } else { 1.0 };
    buf.iter()
        .map(|s| (s * gain * 32767.0).clamp(-32768.0, 32767.0) as i16)
        .collect()
}
