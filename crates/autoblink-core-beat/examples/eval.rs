//! Runs the tracker and the calibrator on a recording of `GET /mic`.
//!
//! The left channel is the mic. A right-channel sample of 1000, 2000, 3000 or 4000 is a Pro DJ
//! Link master beat with that beat in the bar. The time axis is the sample index (62.5 µs).
//!
//! Run: `cargo run -p autoblink-core-beat --example eval -- rec.wav`

use autoblink_core_beat::calib::Calibrator;
use autoblink_core_beat::grid::SampleClock;
use autoblink_core_beat::tracker::{MicState, Tracker};
use autoblink_core_beat::{wav, DEFAULT_OFFSET_US, SAMPLE_RATE};

const BLOCK: usize = 256;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("Usage: eval <file.wav>");
        std::process::exit(2);
    };
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("Cannot read {path}: {e}");
        std::process::exit(1);
    });
    let w = wav::read(&bytes).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    if w.channels != 2 || w.rate != SAMPLE_RATE {
        eprintln!("{path}: the file must be stereo at {SAMPLE_RATE} Hz");
        std::process::exit(1);
    }
    let us = 1e6 / f64::from(SAMPLE_RATE);
    let mut tracker = Tracker::new();
    let mut clock = SampleClock::new();
    let mut calib = Calibrator::new(DEFAULT_OFFSET_US);
    let mut state = MicState::Listening;
    let mut last_beat: Option<f64> = None;
    let mut bpm = 0.0;
    let mut mono = [0i16; BLOCK];
    for (b, block) in w.samples.chunks(BLOCK * 2).enumerate() {
        let n = block.len() / 2;
        for (i, m) in mono.iter_mut().take(n).enumerate() {
            *m = block[i * 2];
        }
        tracker.push(&mono[..n]);
        let end = tracker.samples();
        clock.observe(end, (end as f64 * us) as u64);
        let raw = tracker.grid().map(|g| clock.grid(&g));
        for i in 0..n {
            let v = block[i * 2 + 1];
            if !(1..=4).contains(&(v / 1000)) || v % 1000 != 0 {
                continue;
            }
            let at = (b * BLOCK + i) as f64 * us;
            if let Some(prev) = last_beat {
                let now = 60e6 / (at - prev);
                bpm = if bpm > 0.0 && (now / bpm - 1.0).abs() < 0.1 {
                    bpm + 0.25 * (now - bpm)
                } else {
                    now
                };
            }
            last_beat = Some(at);
            if bpm > 0.0 {
                calib.pdl_beat(at as u64, (v / 1000) as u8, bpm, raw.as_ref());
            }
        }
        if let Some(g) = &raw {
            calib.track(g);
        }
        let now_s = end as f64 / f64::from(SAMPLE_RATE);
        if tracker.state() != state {
            state = tracker.state();
            println!("{now_s:7.2} s  {}", state.label());
            if state == MicState::Locked {
                if let Some(ms) = tracker.cold_lock_ms() {
                    calib.note_lock(ms);
                }
            }
        }
        if let Some(s) = calib.take_minute((end as f64 * us) as u64) {
            print_stats(now_s, &s, &calib);
        }
    }
    let end = (tracker.samples() as f64 * us) as u64;
    if let Some(s) = calib.take_minute(end + 60_000_000) {
        print_stats(
            tracker.samples() as f64 / f64::from(SAMPLE_RATE),
            &s,
            &calib,
        );
    }
}

fn print_stats(now_s: f64, s: &autoblink_core_beat::calib::Stats, c: &Calibrator) {
    println!(
        "{now_s:7.2} s  beats {}  tempo error {:.2} BPM  phase median {} ms  p95 {} ms  bar 1 {} %  \
         lock {}  offset {:.1} ms  rotation {}  ratio {:?}",
        s.beats,
        f64::from(s.tempo_err_cbpm) / 100.0,
        s.phase_med_ms,
        s.phase_p95_ms,
        s.bar1_pct,
        s.lock_ms
            .map_or("-".into(), |ms| format!("{:.1} s", f64::from(ms) / 1000.0)),
        c.offset_us() as f64 / 1000.0,
        c.rotation(),
        c.ratio()
    );
}
