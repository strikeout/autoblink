use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ValueEnum;

use crate::config::MtcFrameRate;
use crate::midi::{open_midi_output, MidiOutHandle, MidiTransport, MidirOutConnection};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SoakMode {
    #[value(name = "midi-clock")]
    MidiClock,
    #[value(name = "mtc")]
    Mtc,
}

#[derive(Debug, Clone)]
pub struct SoakArgs {
    pub mode: SoakMode,
    pub duration_secs: u64,
    pub midi_out: String,
    pub report_path: PathBuf,
    pub fps: u8,
}

#[derive(Debug, Clone, Copy)]
struct Percentiles {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let p = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn compute_percentiles_ms(mut samples: Vec<f64>) -> Percentiles {
    if samples.is_empty() {
        return Percentiles {
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let max = *samples.last().unwrap_or(&0.0);
    Percentiles {
        p50: percentile_sorted(&samples, 0.50),
        p95: percentile_sorted(&samples, 0.95),
        p99: percentile_sorted(&samples, 0.99),
        max,
    }
}

fn ensure_parent_dir(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn write_report(path: &PathBuf, json: &str) -> anyhow::Result<()> {
    ensure_parent_dir(path)?;
    std::fs::write(path, json.as_bytes())?;
    Ok(())
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn mtc_frame_rate_from_fps(fps: u8) -> anyhow::Result<MtcFrameRate> {
    match fps {
        24 => Ok(MtcFrameRate::Fps24),
        25 => Ok(MtcFrameRate::Fps25),
        30 => Ok(MtcFrameRate::Fps30),
        other => anyhow::bail!("Unsupported --fps {other}; use 24, 25, or 30"),
    }
}

// ── MTC encoding (soak-local, deterministic) ─────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Timecode {
    hours: u8,
    minutes: u8,
    seconds: u8,
    frames: u8,
}

impl Timecode {
    fn from_elapsed(elapsed_secs: f64, fps: u8) -> Self {
        if elapsed_secs <= 0.0 {
            return Self {
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 0,
            };
        }

        let total_frames = (elapsed_secs * fps as f64).floor() as u64;
        let frames = (total_frames % fps as u64) as u8;
        let total_secs = total_frames / fps as u64;
        let seconds = (total_secs % 60) as u8;
        let minutes = ((total_secs / 60) % 60) as u8;
        let hours = ((total_secs / 3600) % 24) as u8;

        Self {
            hours,
            minutes,
            seconds,
            frames,
        }
    }
}

fn mtc_quarter_frame_data(tc: &Timecode, piece: u8, rate: MtcFrameRate) -> u8 {
    let value = match piece {
        0 => tc.frames & 0x0F,
        1 => (tc.frames >> 4) & 0x01,
        2 => tc.seconds & 0x0F,
        3 => (tc.seconds >> 4) & 0x03,
        4 => tc.minutes & 0x0F,
        5 => (tc.minutes >> 4) & 0x03,
        6 => tc.hours & 0x0F,
        7 => ((tc.hours >> 4) & 0x01) | (rate.rate_code() << 1),
        _ => 0,
    };
    (piece << 4) | (value & 0x0F)
}

fn mtc_full_frame_sysex(tc: &Timecode, rate: MtcFrameRate) -> [u8; 10] {
    let rh = (rate.rate_code() << 5) | (tc.hours & 0x1F);
    [
        0xF0, 0x7F, 0x7F, 0x01, 0x01, rh, tc.minutes, tc.seconds, tc.frames, 0xF7,
    ]
}

// ── Soak runners ─────────────────────────────────────────────────────────────

const CLOCK_BPM: f64 = 120.0;
const CLOCK_P99_JITTER_MS_THRESHOLD: f64 = 5.0;
const CLOCK_MAX_LATENESS_MS_THRESHOLD: f64 = 20.0;

const MTC_P99_JITTER_MS_THRESHOLD: f64 = 5.0;
const MTC_MAX_RESYNC_COUNT_THRESHOLD: u64 = 1;

pub async fn run(args: SoakArgs) -> anyhow::Result<i32> {
    let (conn, _) = open_midi_output(&args.midi_out)?;
    let initial_conn: Box<dyn crate::midi::MidiOutConnection> = Box::new(MidirOutConnection(conn));
    let midi_out = MidiOutHandle::start(4096, Some(initial_conn));

    let exit_code = match args.mode {
        SoakMode::MidiClock => run_clock_soak(&midi_out, &args).await?,
        SoakMode::Mtc => run_mtc_soak(&midi_out, &args).await?,
    };

    midi_out.stop().await;
    Ok(exit_code)
}

async fn run_clock_soak(midi: &MidiOutHandle, args: &SoakArgs) -> anyhow::Result<i32> {
    let tick_interval = Duration::from_secs_f64(60.0 / (CLOCK_BPM * 24.0));
    let ticks_expected = ((args.duration_secs as f64) * (CLOCK_BPM / 60.0) * 24.0).round() as u64;

    let start = Instant::now();
    let end = start + Duration::from_secs(args.duration_secs);
    let mut n: u64 = 0;

    let mut ticks_sent: u64 = 0;
    let mut samples_ms: Vec<f64> = Vec::with_capacity(ticks_expected.min(200_000) as usize);

    while Instant::now() < end && n < ticks_expected {
        let deadline = start + tick_interval * (n as u32);
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        let now = Instant::now();
        let lateness = now
            .checked_duration_since(deadline)
            .unwrap_or(Duration::ZERO);
        let lateness_ms = lateness.as_secs_f64() * 1000.0;
        samples_ms.push(lateness_ms);

        if midi.send_message(&[0xF8]).is_ok() {
            ticks_sent += 1;
        }
        n += 1;
    }

    let dropped_messages_total = midi.dropped_messages() as u64;
    let p = compute_percentiles_ms(samples_ms);

    let thresholds_p99 = CLOCK_P99_JITTER_MS_THRESHOLD;
    let thresholds_max = CLOCK_MAX_LATENESS_MS_THRESHOLD;
    let counts_ok = ticks_sent == ticks_expected;
    let thresholds_ok = p.p99 <= thresholds_p99 && p.max <= thresholds_max;
    let drops_ok = dropped_messages_total == 0;
    let pass = counts_ok && thresholds_ok && drops_ok;

    let json = format!(
        "{{\n  \"mode\": \"midi-clock\",\n  \"duration_secs\": {},\n  \"bpm\": {:.2},\n  \"ticks_sent\": {},\n  \"ticks_expected\": {},\n  \"p50_jitter_ms\": {:.6},\n  \"p95_jitter_ms\": {:.6},\n  \"p99_jitter_ms\": {:.6},\n  \"max_jitter_ms\": {:.6},\n  \"max_lateness_ms\": {:.6},\n  \"dropped_messages_total\": {},\n  \"thresholds\": {{\n    \"p99_jitter_ms\": {:.6},\n    \"max_lateness_ms\": {:.6}\n  }},\n  \"pass\": {}\n}}\n",
        args.duration_secs,
        CLOCK_BPM,
        ticks_sent,
        ticks_expected,
        p.p50,
        p.p95,
        p.p99,
        p.max,
        p.max,
        dropped_messages_total,
        thresholds_p99,
        thresholds_max,
        if pass { "true" } else { "false" }
    );

    write_report(&args.report_path, &json)?;
    Ok(if pass { 0 } else { 1 })
}

async fn run_mtc_soak(midi: &MidiOutHandle, args: &SoakArgs) -> anyhow::Result<i32> {
    let frame_rate = mtc_frame_rate_from_fps(args.fps)?;
    let fps = frame_rate.fps() as u64;
    let expected_quarter_frames = args.duration_secs.saturating_mul(fps).saturating_mul(4);
    let qf_interval = Duration::from_secs_f64(1.0 / ((frame_rate.fps() as f64) * 4.0));

    let start = Instant::now();
    let end = start + Duration::from_secs(args.duration_secs);

    // Deterministic: emit exactly one full-frame at the start.
    let tc0 = Timecode::from_elapsed(0.0, frame_rate.fps());
    let mut resync_count: u64 = 0;
    if midi
        .send_message(&mtc_full_frame_sysex(&tc0, frame_rate))
        .is_ok()
    {
        resync_count += 1;
    }

    let mut qf_sent: u64 = 0;
    let mut samples_ms: Vec<f64> =
        Vec::with_capacity(expected_quarter_frames.min(200_000) as usize);

    let mut n: u64 = 0;
    while Instant::now() < end && n < expected_quarter_frames {
        let deadline = start + qf_interval * (n as u32);
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        let now = Instant::now();
        let lateness = now
            .checked_duration_since(deadline)
            .unwrap_or(Duration::ZERO);
        samples_ms.push(lateness.as_secs_f64() * 1000.0);

        let elapsed = deadline.duration_since(start).as_secs_f64();
        let tc = Timecode::from_elapsed(elapsed, frame_rate.fps());
        let piece = (n % 8) as u8;
        let data = mtc_quarter_frame_data(&tc, piece, frame_rate);
        if midi.send_message(&[0xF1, data]).is_ok() {
            qf_sent += 1;
        }
        n += 1;
    }

    let dropped_messages_total = midi.dropped_messages() as u64;
    let p = compute_percentiles_ms(samples_ms);

    let thresholds_p99 = MTC_P99_JITTER_MS_THRESHOLD;
    let thresholds_resync = MTC_MAX_RESYNC_COUNT_THRESHOLD;

    let counts_ok = qf_sent == expected_quarter_frames;
    let thresholds_ok = p.p99 <= thresholds_p99 && resync_count <= thresholds_resync;
    let drops_ok = dropped_messages_total == 0;
    let pass = counts_ok && thresholds_ok && drops_ok;

    let json = format!(
        "{{\n  \"mode\": \"mtc\",\n  \"duration_secs\": {},\n  \"fps\": {},\n  \"quarter_frames_sent\": {},\n  \"expected_quarter_frames\": {},\n  \"resync_count\": {},\n  \"p50_jitter_ms\": {:.6},\n  \"p95_jitter_ms\": {:.6},\n  \"p99_jitter_ms\": {:.6},\n  \"max_jitter_ms\": {:.6},\n  \"dropped_messages_total\": {},\n  \"thresholds\": {{\n    \"p99_jitter_ms\": {:.6},\n    \"max_resync_count\": {}\n  }},\n  \"pass\": {}\n}}\n",
        args.duration_secs,
        args.fps,
        qf_sent,
        expected_quarter_frames,
        resync_count,
        p.p50,
        p.p95,
        p.p99,
        p.max,
        dropped_messages_total,
        thresholds_p99,
        thresholds_resync,
        if pass { "true" } else { "false" }
    );

    write_report(&args.report_path, &json)?;
    Ok(if pass { 0 } else { 1 })
}

#[allow(dead_code)]
fn _example_report_fields_for_docs(args: &SoakArgs) -> String {
    // Keep around as a quick local sanity check if formatting changes.
    format!(
        "mode={} duration_secs={} midi_out=\"{}\" report=\"{}\"",
        match args.mode {
            SoakMode::MidiClock => "midi-clock",
            SoakMode::Mtc => "mtc",
        },
        args.duration_secs,
        json_escape(&args.midi_out),
        json_escape(&args.report_path.display().to_string())
    )
}
