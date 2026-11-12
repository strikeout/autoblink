//! The beat tracker: the tempo, the beat phase and the lock state from the kick drum.
//!
//! - Every 0.5 s, an autocorrelation of the last 4 s of onsets gives the tempo, 80 to 160 BPM.
//! - A phase lock follows the kick near each predicted beat. It moves the phase by 0.1 and the
//!   period by 0.01 of the error.
//! - `Locked` needs a kick on 6 of the last 8 predicted beats.

use crate::onset::{Frame, Onsets};
use crate::{BPM_MAX, BPM_MIN, BREAK_BEATS, HOP, NO_MIC_MS, SAMPLE_RATE, SILENCE_DBFS, SILENCE_MS};

const SR: f64 = SAMPLE_RATE as f64;
/// The onset frames each second.
const FPS: f64 = SR / HOP as f64;
/// The frames of the tempo ring (4 s).
const LONG: usize = 500;
/// The frames of the phase rings (about 1 s).
const SHORT: usize = 128;
/// The lags of the tempo search, 80 to 160 BPM, and one on each side.
const LAGS: usize = 51;
/// The frames between two tempo estimates (about 0.5 s).
const TEMPO_EVERY: u64 = 62;
/// The frames before the first tempo estimate (2 s).
const TEMPO_MIN: u64 = 250;
/// The confidence that seeds the phase lock, 0 to 100.
const SEED_CONF: u8 = 20;
/// The confidence that `Locked` needs, 0 to 100.
const LOCK_CONF: u8 = 20;
/// The smallest kick onset that counts as a kick.
const KICK_FLOOR: f32 = 0.5;
/// The largest phase correction for one beat, in samples (10 ms).
const MAX_ERR: f64 = SR / 100.0;
/// The onset of a kick comes this many samples after the kick starts (filter and hop delay).
const DELAY: f64 = 128.0;

/// The state of the mic beat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicState {
    #[default]
    Listening,
    /// The PCM is constant: no module, or IO12 is open.
    NoMic,
    Locked,
    /// The kick stopped. The grid continues at the last tempo and phase.
    Flywheel,
}

impl MicState {
    /// The name in the status message.
    pub fn label(self) -> &'static str {
        match self {
            MicState::Listening => "listening",
            MicState::NoMic => "no_mic",
            MicState::Locked => "locked",
            MicState::Flywheel => "flywheel",
        }
    }
}

/// The part of the track that the kick shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// The kick stopped.
    Break,
    /// The kick came back after a Break.
    Drop,
}

/// The beat grid of the tracker, on the sample axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SampleGrid {
    /// The sample index of one beat of the grid.
    pub beat: f64,
    /// The samples of one beat.
    pub period: f64,
    /// The position of that beat in its bar, 0 to 3. 0 is bar 1.
    pub bar_pos: u8,
    /// The position of that beat in its 16-beat block, 0 to 15.
    pub phrase_pos: u8,
}

/// The phase lock.
#[derive(Clone, Copy, Debug)]
struct Pll {
    /// The samples of one beat.
    period: f64,
    /// The sample index of the next predicted beat, on the onset axis (with `DELAY`).
    next: f64,
    /// The index of the next beat since the seed.
    n: u64,
    /// Bit i is set when the beat i beats ago had a kick.
    hits: u8,
    /// The beats since the seed.
    beats: u32,
    /// The beats in a row without a kick on the grid but with a kick off the grid.
    off_grid: u8,
    /// The typical size of a kick onset.
    peak: f32,
    /// The last frame of the last beat window.
    last_hi: u64,
    /// The beat index and the onset time of the last kicks, for the period.
    hist: [(f64, f64); HIST],
    hist_len: usize,
    /// The class of the beat index (mod 4) that is bar 1.
    bar0: u8,
    /// The class of the beat index (mod 16) that starts the 16-beat block. `phrase0 % 4 == bar0`.
    phrase0: u8,
    /// The clap onsets and the high-band onsets of the 4 beat classes, with a decay of 16 bars.
    acc_clap: [f32; 4],
    acc_high: [f32; 4],
    /// The class that the bar scores give now, and the beats in a row that it won.
    cand: Option<u8>,
    streak: u32,
    /// The beats in a row without a kick on the grid and off the grid.
    quiet: u32,
    /// The kicks in a row on the grid during the flywheel, and the index of the first one.
    back: u8,
    back_n: u64,
}

/// The decay of the bar scores for each beat: a time constant of 16 bars.
const BAR_DECAY: f32 = 0.984_5;
/// The beats that a new bar 1 must win in a row (8 bars).
const BAR_STREAK: u32 = 32;
/// The margin of the best bar score over the second.
const BAR_MARGIN: f32 = 1.15;
/// The ratio of the clap scores of the two beat pairs that decides the pair of bar 1.
const CLAP_RATIO: f32 = 1.5;

/// The kicks that the period regression uses.
const HIST: usize = 16;

impl Pll {
    fn bar_pos(&self, n: u64) -> u8 {
        ((n + 4 - u64::from(self.bar0)) % 4) as u8
    }

    fn phrase_pos(&self, n: u64) -> u8 {
        ((n + 16 - u64::from(self.phrase0)) % 16) as u8
    }

    /// Adds the onsets of beat `n` to the bar scores. Moves bar 1 when another class wins for
    /// `BAR_STREAK` beats in a row.
    fn score_bar(&mut self, n: u64, clap: f32, high: f32) {
        for a in self.acc_clap.iter_mut().chain(self.acc_high.iter_mut()) {
            *a *= BAR_DECAY;
        }
        let i = (n % 4) as usize;
        self.acc_clap[i] += clap;
        self.acc_high[i] += high;
        let cand = self.bar_candidate();
        if cand.is_some() && cand == self.cand {
            self.streak += 1;
        } else {
            self.cand = cand;
            self.streak = u32::from(cand.is_some());
        }
        if let Some(c) = cand {
            if self.streak >= BAR_STREAK && c != self.bar0 {
                let delta = (c + 4 - self.bar0) % 4;
                self.bar0 = c;
                self.phrase0 = (self.phrase0 + delta) % 16;
            }
        }
    }

    /// The class of bar 1 that the scores give: the most high-band change, among the pair without
    /// the claps when one pair has them. `None` without a clear margin.
    fn bar_candidate(&self) -> Option<u8> {
        // A clap pair has a clap of about the same size on both of its beats. A crash on one beat
        // is no clap pair.
        let c = &self.acc_clap;
        let pair = |a: usize, b: usize, x: usize, y: usize| {
            let (lo, hi) = (c[a].min(c[b]), c[a].max(c[b]));
            lo > CLAP_RATIO * c[x].max(c[y]) && lo > 0.5 * hi
        };
        let cands: &[usize] = if pair(0, 2, 1, 3) {
            &[1, 3]
        } else if pair(1, 3, 0, 2) {
            &[0, 2]
        } else {
            &[0, 1, 2, 3]
        };
        let mut best = (0, f32::MIN);
        let mut second = f32::MIN;
        for &c in cands {
            let v = self.acc_high[c];
            if v > best.1 {
                second = best.1;
                best = (c, v);
            } else if v > second {
                second = v;
            }
        }
        (best.1 > 0.0 && best.1 > BAR_MARGIN * second).then_some(best.0 as u8)
    }

    fn add_hit(&mut self, t: f64) {
        self.hist[self.n as usize % HIST] = (self.n as f64, t);
        self.hist_len = (self.hist_len + 1).min(HIST);
        if self.hist_len < 8 {
            return;
        }
        let h = &self.hist[..self.hist_len];
        let k = h.len() as f64;
        let (mn, mt) = h
            .iter()
            .fold((0.0, 0.0), |a, x| (a.0 + x.0 / k, a.1 + x.1 / k));
        let (num, den) = h.iter().fold((0.0, 0.0), |a, x| {
            (a.0 + (x.0 - mn) * (x.1 - mt), a.1 + (x.0 - mn) * (x.0 - mn))
        });
        if den > 0.0 {
            let slope = num / den;
            if (slope / self.period - 1.0).abs() < 0.05 {
                self.period = slope;
            }
        }
    }
}

/// The beat tracker. It allocates nothing after `new`.
pub struct Tracker {
    onsets: Onsets,
    frames: u64,
    odf: [f32; LONG],
    kick: [f32; SHORT],
    clap: [f32; SHORT],
    high: [f32; SHORT],
    state: MicState,
    pll: Option<Pll>,
    tempo: Option<f64>,
    tempo_off: u8,
    confidence: u8,
    level_db: f32,
    flat: u64,
    quiet: u64,
    listen_from: u64,
    cold_lock_ms: Option<u32>,
    section: Option<Section>,
    flywheel_from: u64,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

fn frames_of(ms: u64) -> u64 {
    ms * FPS as u64 / 1000
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            onsets: Onsets::new(),
            frames: 0,
            odf: [0.0; LONG],
            kick: [0.0; SHORT],
            clap: [0.0; SHORT],
            high: [0.0; SHORT],
            state: MicState::Listening,
            pll: None,
            tempo: None,
            tempo_off: 0,
            confidence: 0,
            level_db: -120.0,
            flat: 0,
            quiet: 0,
            listen_from: 0,
            cold_lock_ms: None,
            section: None,
            flywheel_from: 0,
        }
    }

    /// Feeds PCM samples. The sample index continues from the last call.
    pub fn push(&mut self, samples: &[i16]) {
        let mut frames = [Frame::default(); 8];
        for chunk in samples.chunks(HOP * 4) {
            let mut n = 0;
            self.onsets.push(chunk, &mut |f| {
                frames[n] = f;
                n += 1;
            });
            for f in &frames[..n] {
                self.frame(*f);
            }
        }
    }

    /// The samples pushed so far.
    pub fn samples(&self) -> u64 {
        self.frames * HOP as u64 + self.onsets_partial()
    }

    fn onsets_partial(&self) -> u64 {
        self.onsets.partial() as u64
    }

    pub fn state(&self) -> MicState {
        self.state
    }

    /// The grid in `Locked` and `Flywheel`.
    pub fn grid(&self) -> Option<SampleGrid> {
        if !matches!(self.state, MicState::Locked | MicState::Flywheel) {
            return None;
        }
        self.pll.map(|p| SampleGrid {
            beat: p.next - DELAY,
            period: p.period,
            bar_pos: p.bar_pos(p.n),
            phrase_pos: p.phrase_pos(p.n),
        })
    }

    /// The tempo of the phase lock, or of the last estimate without a lock.
    pub fn bpm(&self) -> Option<f64> {
        self.pll.map(|p| 60.0 * SR / p.period).or(self.tempo)
    }

    /// The last Break or Drop since the lock.
    pub fn section(&self) -> Option<Section> {
        self.section
    }

    /// The strength of the periodicity of the onsets, 0 to 100.
    pub fn confidence(&self) -> u8 {
        self.confidence
    }

    /// The input level, 0 to 100 for -90 to 0 dBFS.
    pub fn level(&self) -> u8 {
        ((self.level_db + 90.0) / 90.0 * 100.0).clamp(0.0, 100.0) as u8
    }

    /// True while the bar scores give the current bar 1 with a margin for 8 bars.
    pub fn bar_confident(&self) -> bool {
        self.pll
            .is_some_and(|p| p.cand == Some(p.bar0) && p.streak >= BAR_STREAK)
    }

    /// The time in `Flywheel`, in ms. 0 outside `Flywheel`.
    pub fn flywheel_ms(&self) -> u64 {
        if self.state == MicState::Flywheel {
            (self.frames - self.flywheel_from) * 1000 / FPS as u64
        } else {
            0
        }
    }

    /// The time from `Listening` to `Locked` of the last lock, in ms.
    pub fn cold_lock_ms(&self) -> Option<u32> {
        self.cold_lock_ms
    }

    fn reset(&mut self, state: MicState) {
        self.state = state;
        self.pll = None;
        self.section = None;
        self.tempo_off = 0;
        self.listen_from = self.frames;
    }

    fn frame(&mut self, f: Frame) {
        let i = self.frames as usize;
        self.odf[i % LONG] = f.kick + 0.5 * f.full;
        self.kick[i % SHORT] = f.kick;
        self.clap[i % SHORT] = f.clap;
        self.high[i % SHORT] = f.high;
        self.frames += 1;
        self.level_db += (f.dbfs.max(-120.0) - self.level_db) * 0.05;

        self.flat = if f.span < 4 { self.flat + 1 } else { 0 };
        if self.flat >= frames_of(NO_MIC_MS) {
            if self.state != MicState::NoMic {
                self.reset(MicState::NoMic);
            }
            return;
        }
        if self.state == MicState::NoMic {
            self.reset(MicState::Listening);
        }
        self.quiet = if f.dbfs < SILENCE_DBFS {
            self.quiet + 1
        } else {
            0
        };
        if self.quiet >= frames_of(SILENCE_MS) {
            if self.state != MicState::Listening || self.pll.is_some() {
                self.reset(MicState::Listening);
            }
            return;
        }
        if self.frames >= TEMPO_MIN && self.frames.is_multiple_of(TEMPO_EVERY) {
            self.estimate_tempo();
        }
        self.run_pll();
    }

    /// The onset of frame `f` from a ring. 0 when the ring does not hold it any more.
    fn ring(&self, ring: &[f32], f: u64) -> f32 {
        if f < self.frames && self.frames - f <= ring.len() as u64 {
            ring[f as usize % ring.len()]
        } else {
            0.0
        }
    }

    fn estimate_tempo(&mut self) {
        let n = (self.frames as usize).min(LONG);
        let base = self.frames - n as u64;
        let x = |j: usize| self.odf[(base as usize + j) % LONG];
        let mean = (0..n).map(x).sum::<f32>() / n as f32;
        let y = |j: usize| x(j) - mean;
        let r = |lag: usize| -> f32 {
            if lag >= n {
                return 0.0;
            }
            (lag..n).map(|j| y(j) * y(j - lag)).sum::<f32>() / (n - lag) as f32
        };
        let r0 = r(0);
        if r0 <= 0.0 {
            return;
        }
        // The mean correlation at the lag and its multiples that fit in the ring.
        let score = |lag: usize| -> f32 {
            let (mut s, mut k) = (0.0, 0);
            for m in [1, 2, 4] {
                if lag * m <= n * 3 / 4 {
                    s += r(lag * m);
                    k += 1;
                }
            }
            if k == 0 {
                0.0
            } else {
                s / k as f32 / r0
            }
        };
        let lo = (60.0 * FPS / BPM_MAX).floor() as usize;
        let hi = (60.0 * FPS / BPM_MIN).ceil() as usize;
        let prior = |lag: f64| {
            let octaves = (60.0 * FPS / lag / 125.0).log2();
            (-0.5 * octaves * octaves).exp() as f32
        };
        let mut scores = [0.0f32; LAGS];
        for (i, s) in scores.iter_mut().enumerate().take(hi - lo + 3) {
            *s = score(lo - 1 + i);
        }
        let scores = &scores[..hi - lo + 3];
        let Some((k, &peak)) = scores[1..scores.len() - 1]
            .iter()
            .enumerate()
            .max_by(|a, b| {
                (a.1 * prior((a.0 + lo) as f64)).total_cmp(&(b.1 * prior((b.0 + lo) as f64)))
            })
        else {
            return;
        };
        let (a, b, c) = (scores[k], peak, scores[k + 2]);
        let den = a - 2.0 * b + c;
        let delta = if den < 0.0 {
            (0.5 * (a - c) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let lag = (k + lo) as f64 + f64::from(delta);
        let bpm = 60.0 * FPS / lag;
        self.confidence = (peak * 100.0).clamp(0.0, 100.0) as u8;
        self.tempo = Some(bpm);
        if self.confidence < SEED_CONF || self.state == MicState::Flywheel {
            self.tempo_off = 0;
            return;
        }
        match self.pll {
            None => self.seed(bpm),
            Some(p) => {
                let have = 60.0 * SR / p.period;
                if (bpm - have).abs() / have > 0.02 {
                    self.tempo_off += 1;
                    if self.tempo_off >= 2 {
                        self.seed(bpm);
                    }
                } else {
                    self.tempo_off = 0;
                }
            }
        }
    }

    /// Starts the phase lock at `bpm`. The phase is the best fit of a comb on the onsets.
    fn seed(&mut self, bpm: f64) {
        let p = 60.0 * FPS / bpm;
        let n = (self.frames as usize).min(LONG);
        let last = self.frames - 1;
        let comb = |phi: u64| -> f32 {
            let mut s = 0.0;
            let mut k = 0.0;
            loop {
                let back = phi as f64 + k * p;
                if back.round() as usize >= n {
                    return s;
                }
                s += self.ring(&self.odf, last - back.round() as u64);
                k += 1.0;
            }
        };
        let best = (0..p.ceil() as u64)
            .max_by(|a, b| comb(*a).total_cmp(&comb(*b)))
            .unwrap_or(0);
        let period = p * HOP as f64;
        let beat = (last - best) as f64 * HOP as f64 + HOP as f64 / 2.0;
        let now = (self.frames * HOP as u64) as f64;
        let mut next = beat;
        while next < now - period / 8.0 {
            next += period;
        }
        let peak = (0..SHORT as u64)
            .map(|b| self.ring(&self.kick, last.saturating_sub(b)))
            .fold(0.0, f32::max);
        if self.state == MicState::Locked {
            self.state = MicState::Listening;
            self.listen_from = self.frames;
        }
        self.tempo_off = 0;
        self.pll = Some(Pll {
            period,
            next,
            n: 0,
            hits: 0,
            beats: 0,
            off_grid: 0,
            peak,
            last_hi: ((next - period / 8.0) / HOP as f64).floor() as u64,
            hist: [(0.0, 0.0); HIST],
            hist_len: 0,
            bar0: 0,
            phrase0: 0,
            acc_clap: [0.0; 4],
            acc_high: [0.0; 4],
            cand: None,
            streak: 0,
            quiet: 0,
            back: 0,
            back_n: 0,
        });
    }

    fn run_pll(&mut self) {
        let now = (self.frames * HOP as u64) as f64;
        while let Some(mut p) = self.pll {
            let w = p.period / 8.0;
            if now < p.next + w {
                break;
            }
            let half = HOP as f64 / 2.0;
            let f_lo = ((p.next - w - half) / HOP as f64).ceil().max(0.0) as u64;
            let f_hi = ((p.next + w - half) / HOP as f64).floor().max(0.0) as u64;
            let (mut m, mut v) = (f_lo, -1.0f32);
            for f in f_lo..=f_hi {
                let k = self.ring(&self.kick, f);
                if k > v {
                    (m, v) = (f, k);
                }
            }
            let off = (p.last_hi + 1..f_lo)
                .map(|f| self.ring(&self.kick, f))
                .fold(0.0, f32::max);
            p.last_hi = f_hi;
            let thr = (0.35 * p.peak).max(KICK_FLOOR);
            let hit = v >= thr;
            if self.state != MicState::Flywheel {
                let max = |ring: &[f32]| {
                    (f_lo..=f_hi)
                        .map(|f| self.ring(ring, f))
                        .fold(0.0, f32::max)
                };
                p.score_bar(p.n, max(&self.clap), max(&self.high));
            }
            if hit {
                let a = self.ring(&self.kick, m.saturating_sub(1));
                let c = self.ring(&self.kick, m + 1);
                let den = a - 2.0 * v + c;
                let delta = if den < 0.0 {
                    f64::from((0.5 * (a - c) / den).clamp(-0.5, 0.5))
                } else {
                    0.0
                };
                let t = m as f64 * HOP as f64 + half + delta * HOP as f64;
                let e = t - p.next;
                p.next += (0.1 * e).clamp(-MAX_ERR, MAX_ERR);
                p.period += 0.01 * e;
                p.add_hit(t);
                p.period = p.period.clamp(60.0 * SR / BPM_MAX, 60.0 * SR / BPM_MIN);
                p.peak += 0.1 * (v - p.peak);
            }
            p.hits = (p.hits << 1) | u8::from(hit);
            p.off_grid = if !hit && off >= thr {
                p.off_grid + 1
            } else {
                0
            };
            p.quiet = if !hit && off < thr { p.quiet + 1 } else { 0 };
            let n = p.n;
            p.next += p.period;
            p.n += 1;
            p.beats += 1;
            self.pll = Some(p);
            self.after_beat(n, hit);
        }
    }

    /// Changes the state after the evaluation of the predicted beat `n`.
    fn after_beat(&mut self, n: u64, hit: bool) {
        let Some(p) = self.pll.as_mut() else {
            return;
        };
        let hits = p.hits.count_ones();
        match self.state {
            MicState::Listening if p.beats >= 6 => {
                if hits >= 6 && self.confidence >= LOCK_CONF {
                    self.state = MicState::Locked;
                    self.cold_lock_ms = Some(((self.frames - self.listen_from) * 8) as u32);
                } else if p.beats >= 8 && hits <= 2 {
                    self.pll = None;
                }
            }
            MicState::Locked | MicState::Flywheel if p.off_grid >= 4 => {
                self.reset(MicState::Listening)
            }
            MicState::Locked if p.quiet >= BREAK_BEATS => {
                p.back = 0;
                self.state = MicState::Flywheel;
                self.section = Some(Section::Break);
                self.flywheel_from = self.frames;
            }
            MicState::Flywheel if hit => {
                p.back += 1;
                if p.back == 1 {
                    p.back_n = n;
                }
                if p.back >= 2 {
                    // The first beat of the kick return starts a bar and a 16-beat block.
                    p.bar0 = (p.back_n % 4) as u8;
                    p.phrase0 = (p.back_n % 16) as u8;
                    p.cand = None;
                    p.streak = 0;
                    self.state = MicState::Locked;
                    self.section = Some(Section::Drop);
                }
            }
            MicState::Flywheel => p.back = 0,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{beat_times, render, Track};
    use crate::SAMPLE_RATE;

    const SR: f64 = SAMPLE_RATE as f64;

    /// Pushes `pcm` in blocks of 256 samples. Logs each state change with its time in s.
    fn feed(tr: &mut Tracker, pcm: &[i16], log: &mut Vec<(f64, MicState)>) {
        for block in pcm.chunks(256) {
            tr.push(block);
            let s = tr.state();
            if log.last().map(|l| l.1) != Some(s) {
                log.push((tr.samples() as f64 / SR, s));
            }
        }
    }

    fn run(t: &Track, s: f64) -> (Tracker, Vec<(f64, MicState)>) {
        let mut tr = Tracker::new();
        let mut log = vec![(0.0, tr.state())];
        feed(&mut tr, &render(t, s), &mut log);
        (tr, log)
    }

    /// The state at the time `at` (s).
    fn state_at(log: &[(f64, MicState)], at: f64) -> MicState {
        log.iter().rev().find(|l| l.0 <= at).unwrap().1
    }

    fn first(log: &[(f64, MicState)], s: MicState) -> Option<f64> {
        log.iter().find(|l| l.1 == s).map(|l| l.0)
    }

    /// The largest distance of the true beats `beats` from the nearest grid beat, in ms.
    fn grid_err_ms(tr: &Tracker, beats: &[(f64, u8)]) -> f64 {
        let g = tr.grid().expect("a grid");
        beats
            .iter()
            .map(|&(at, _)| {
                let x = at * SR;
                let n = ((x - g.beat) / g.period).round();
                (x - (g.beat + n * g.period)).abs() / SR * 1000.0
            })
            .fold(0.0, f64::max)
    }

    /// The largest distance of the last 8 true beats before `s` from the grid, in ms.
    fn phase_err_ms(tr: &Tracker, t: &Track, s: f64) -> f64 {
        let beats = beat_times(t, s);
        grid_err_ms(tr, &beats[beats.len() - 8..])
    }

    #[test]
    fn locks_within_4_bars() {
        let (_, log) = run(&Track::default(), 20.0);
        let at = first(&log, MicState::Locked).expect("no lock");
        assert!(at < 7.75, "locked at {at} s");
    }

    #[test]
    fn the_tempo_is_within_a_tenth() {
        let (tr, _) = run(&Track::default(), 30.0);
        let bpm = tr.bpm().unwrap();
        assert!((bpm - 124.0).abs() < 0.1, "bpm {bpm}");
    }

    #[test]
    fn the_phase_is_within_5_ms() {
        let t = Track::default();
        let (tr, _) = run(&t, 30.0);
        let e = phase_err_ms(&tr, &t, 30.0);
        assert!(e < 5.0, "phase error {e} ms");
    }

    #[test]
    fn noise_keeps_the_phase_within_10_ms() {
        let t = Track {
            noise_db: Some(-10.0),
            ..Track::default()
        };
        let (tr, log) = run(&t, 30.0);
        assert_eq!(state_at(&log, 30.0), MicState::Locked);
        let e = phase_err_ms(&tr, &t, 30.0);
        assert!(e < 10.0, "phase error {e} ms");
    }

    #[test]
    fn follows_a_tempo_ramp() {
        let t = Track {
            bpm_end: Some(126.0),
            ramp_beats: 128,
            ..Track::default()
        };
        let pcm = render(&t, 70.0);
        let mut tr = Tracker::new();
        let mut log = Vec::new();
        let bar = (4.0 * 60.0 / 124.0 * SR) as usize;
        let mut done = 0;
        while done + bar <= pcm.len() {
            feed(&mut tr, &pcm[done..done + bar], &mut log);
            done += bar;
            let now = done as f64 / SR;
            if now > 10.0 {
                let e = phase_err_ms(&tr, &t, now);
                assert!(e < 15.0, "phase error {e} ms at {now} s");
            }
        }
    }

    #[test]
    fn a_flam_does_not_move_the_phase() {
        let t = Track {
            flam: Some((30.0, 64, 8)),
            ..Track::default()
        };
        let end = beat_times(&t, 60.0)[73].0;
        let (tr, _) = run(&t, end);
        let e = phase_err_ms(&tr, &t, end);
        assert!(e < 5.0, "phase error {e} ms after the flam");
    }

    #[test]
    fn drum_and_bass_folds_to_half() {
        let t = Track {
            bpm: 174.0,
            ..Track::default()
        };
        let (tr, log) = run(&t, 30.0);
        assert_eq!(state_at(&log, 30.0), MicState::Locked);
        let bpm = tr.bpm().unwrap();
        assert!((86.0..=88.0).contains(&bpm), "bpm {bpm}");
    }

    #[test]
    fn dc_and_zero_give_no_mic() {
        let mut pcm = vec![0i16; 6 * SAMPLE_RATE as usize];
        pcm.extend(std::iter::repeat_n(1000, 6 * SAMPLE_RATE as usize));
        let mut tr = Tracker::new();
        let mut log = vec![(0.0, tr.state())];
        feed(&mut tr, &pcm, &mut log);
        assert_eq!(state_at(&log, 5.1), MicState::NoMic);
        assert_ne!(state_at(&log, 6.1), MicState::NoMic);
        assert_eq!(state_at(&log, 11.2), MicState::NoMic);
    }

    #[test]
    fn silence_after_music_goes_to_listening() {
        let mut pcm = render(&Track::default(), 20.0);
        // A quiet room: low noise, far below the silence level, but not a constant signal.
        let mut x: u32 = 1;
        pcm.extend((0..11 * SAMPLE_RATE).map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 28) as i16) - 8
        }));
        let mut tr = Tracker::new();
        let mut log = vec![(0.0, tr.state())];
        feed(&mut tr, &pcm, &mut log);
        assert_eq!(state_at(&log, 19.9), MicState::Locked);
        assert_eq!(tr.state(), MicState::Listening);
    }

    #[test]
    fn a_clipped_input_still_locks() {
        let t = Track {
            clip: true,
            ..Track::default()
        };
        let (tr, log) = run(&t, 20.0);
        let at = first(&log, MicState::Locked).expect("no lock");
        assert!(at < 8.0, "locked at {at} s");
        let e = phase_err_ms(&tr, &t, 20.0);
        assert!(e < 10.0, "phase error {e} ms");
    }

    #[test]
    fn a_hard_cut_to_a_new_tempo_relocks() {
        let mut pcm = render(&Track::default(), 30.0);
        let t2 = Track {
            bpm: 130.0,
            offset_s: 0.173,
            ..Track::default()
        };
        pcm.extend(render(&t2, 30.0));
        let mut tr = Tracker::new();
        let mut log = vec![(0.0, tr.state())];
        let cut = 30 * SAMPLE_RATE as usize;
        feed(&mut tr, &pcm[..cut], &mut log);
        // 8 bars at 130 BPM.
        let check = 30.0 * 60.0 / 130.0 + 0.173;
        let n = cut + (check * SR) as usize;
        feed(&mut tr, &pcm[cut..n], &mut log);
        assert_eq!(tr.state(), MicState::Locked);
        let bpm = tr.bpm().unwrap();
        assert!((bpm - 130.0).abs() < 0.5, "bpm {bpm}");
        let beats: Vec<(f64, u8)> = beat_times(&t2, check)
            .into_iter()
            .map(|(at, b)| (at + 30.0, b))
            .collect();
        let e = grid_err_ms(&tr, &beats[beats.len() - 8..]);
        assert!(e < 10.0, "phase error {e} ms");
    }

    /// The bar position and the phrase position of the grid beat nearest `at` (s).
    fn pos_at(tr: &Tracker, at: f64) -> (u8, u8) {
        let g = tr.grid().expect("a grid");
        let k = ((at * SR - g.beat) / g.period).round() as i64;
        (
            (i64::from(g.bar_pos) + k).rem_euclid(4) as u8,
            (i64::from(g.phrase_pos) + k).rem_euclid(16) as u8,
        )
    }

    /// The time of true beat `k` (s).
    fn beat_at(t: &Track, k: usize) -> f64 {
        beat_times(t, 600.0)[k].0
    }

    #[test]
    fn finds_bar_1_within_8_bars() {
        let t = Track::default();
        let (tr, _) = run(&t, beat_at(&t, 64));
        assert_eq!(pos_at(&tr, beat_at(&t, 60)).0, 0);
        assert!(tr.bar_confident());
    }

    #[test]
    fn without_a_clap_the_bar_change_decides() {
        let t = Track {
            clap_on_2_4: false,
            ..Track::default()
        };
        let (tr, _) = run(&t, beat_at(&t, 64));
        assert_eq!(pos_at(&tr, beat_at(&t, 60)).0, 0);
    }

    fn breakdown() -> Track {
        Track {
            breakdown: Some((64, 64)),
            ..Track::default()
        }
    }

    #[test]
    fn a_breakdown_runs_the_flywheel() {
        let t = breakdown();
        let (tr, log) = run(&t, beat_at(&t, 127));
        assert_eq!(state_at(&log, beat_at(&t, 72)), MicState::Flywheel);
        assert_eq!(tr.state(), MicState::Flywheel);
        assert_eq!(tr.section(), Some(Section::Break));
        let e = grid_err_ms(&tr, &[(beat_at(&t, 126), 3)]);
        assert!(e < 10.0, "phase error {e} ms at the end of the breakdown");
    }

    #[test]
    fn the_kick_return_is_a_drop() {
        let t = breakdown();
        let (tr, _) = run(&t, beat_at(&t, 130));
        assert_eq!(tr.state(), MicState::Locked);
        assert_eq!(tr.section(), Some(Section::Drop));
        assert_eq!(pos_at(&tr, beat_at(&t, 128)), (0, 0));
    }

    #[test]
    fn the_flywheel_counts_its_time() {
        let t = Track {
            breakdown: Some((64, 72)),
            ..Track::default()
        };
        let (tr, _) = run(&t, beat_at(&t, 135));
        assert_eq!(tr.state(), MicState::Flywheel);
        assert!(
            tr.flywheel_ms() >= 29_000,
            "flywheel {} ms",
            tr.flywheel_ms()
        );
    }

    #[test]
    fn the_bar_stays_without_confidence() {
        let t = Track {
            clap_on_2_4: false,
            bar_change: false,
            ..Track::default()
        };
        let (tr, _) = run(&t, beat_at(&t, 24));
        let early = pos_at(&tr, beat_at(&t, 20)).0;
        let (tr, _) = run(&t, beat_at(&t, 120));
        assert_eq!(pos_at(&tr, beat_at(&t, 20)).0, early);
        assert!(!tr.bar_confident());
    }
}
