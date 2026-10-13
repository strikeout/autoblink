//! MIDI Timecode (MTC) generator — quarter-frame and full-frame messages.
//!
//! This task is **deadline-driven**: quarter-frame messages are scheduled
//! against monotonic `Instant` deadlines using `tokio::time::sleep_until`.
//! Deadlines advance monotonically; we never “catch up” by burst-sending
//! multiple quarter-frames when late.
//!
//! ## Authoritative timeline / playhead policy
//! MTC position is derived from an explicit policy:
//!
//! 1. **Preferred (authoritative):** ProLink AbsPosition-derived playhead
//!    (`TimingMeasurement.playhead_ms`) *when it is fresh and master-scoped*.
//!    “Master-scoped” here means the measurement's `device_number` matches the
//!    current `DjState.master.device_number`.
//!
//! 2. **Fallback (oscillator):** a monotonic oscillator anchored to the last
//!    known position and advanced using elapsed monotonic time and a speed
//!    factor derived from timing snapshots (similar philosophy to the clock
//!    scheduler: monotonic deadlines + corrections, no drift accumulation).
//!
//! If timing becomes stale/missing, MTC pauses rather than free-running.
//!
//! ## Discontinuity / seek resync
//! Large position discontinuities (seeks, major stalls) trigger exactly **one**
//! full-frame SysEx resync. Quarter-frames resume from the corrected position
//! immediately after.
//!
//! Reference: MIDI 1.0 Detailed Specification — MTC Quarter Frame (RP-004).
//! Timing: 8 quarter-frame messages encode one complete timecode over 2 frame
//! periods. Interval between QF messages = 1 / (fps × 4).

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::config::{MtcFrameRate, SharedConfig};
use crate::midi::MidiTransport;
use crate::prolink::beat_listener::BeatEvent;
use crate::state::timing::{MeasurementKind, TimingMeasurement, TimingSnapshot, TimingSource};
use crate::state::{BeatSource, SharedState};
use crate::tui::state::MidiActivity;

use tokio::time::{sleep, sleep_until, Instant as TokioInstant};

// ── Timecode representation ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timecode {
    hours: u8,
    minutes: u8,
    seconds: u8,
    frames: u8,
}

impl Timecode {
    fn zero() -> Self {
        Self {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        }
    }

    /// Derive timecode from wall-clock elapsed seconds at the given frame rate.
    fn from_elapsed(elapsed_secs: f64, fps: u8) -> Self {
        if elapsed_secs < 0.0 {
            return Self::zero();
        }
        let total_frames = (elapsed_secs * fps as f64) as u64;

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

    fn from_position_ms(position_ms: i64, fps: u8) -> Self {
        if position_ms <= 0 {
            return Self::zero();
        }
        Self::from_elapsed(position_ms as f64 / 1000.0, fps)
    }

    /// Total frame count for comparison / seek detection.
    fn total_frames(&self, fps: u8) -> u64 {
        let fps = fps as u64;
        self.hours as u64 * 3600 * fps
            + self.minutes as u64 * 60 * fps
            + self.seconds as u64 * fps
            + self.frames as u64
    }

    /// Increment by one frame, wrapping at 24h.
    #[allow(dead_code)]
    fn increment(&mut self, fps: u8) {
        self.frames += 1;
        if self.frames >= fps {
            self.frames = 0;
            self.seconds += 1;
        }
        if self.seconds >= 60 {
            self.seconds = 0;
            self.minutes += 1;
        }
        if self.minutes >= 60 {
            self.minutes = 0;
            self.hours += 1;
        }
        if self.hours >= 24 {
            self.hours = 0;
        }
    }
}

impl std::fmt::Display for Timecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds, self.frames
        )
    }
}

// ── Scheduler + time source policy ────────────────────────────────────────────

// How often to poll config/state when idle.
const IDLE_POLL: Duration = Duration::from_millis(25);

// Gating: allow MTC to run even if `master.is_playing` flaps false, as long as
// beats are still arriving.
const RECENT_BEAT_GRACE: Duration = Duration::from_millis(400);

// AbsPosition playhead freshness requirement.
const PLAYHEAD_MAX_AGE: Duration = Duration::from_millis(200);

// Discontinuity threshold (frames) for emitting a full-frame resync.
const DISCONTINUITY_THRESHOLD_FRAMES: u64 = 8;

fn qf_interval(frame_rate: MtcFrameRate) -> Duration {
    let fps = frame_rate.fps() as f64;
    Duration::from_secs_f64(1.0 / (fps * 4.0))
}

fn is_playing_like(
    now: Instant,
    bpm: f64,
    is_playing: bool,
    last_beat_at: Option<Instant>,
) -> bool {
    if !(bpm.is_finite() && bpm > 0.0) {
        return false;
    }
    if is_playing {
        return true;
    }
    let Some(t) = last_beat_at else {
        return false;
    };
    now.checked_duration_since(t).unwrap_or(Duration::ZERO) <= RECENT_BEAT_GRACE
}

fn speed_factor_from_master_pitch_pct(pitch_pct: f64) -> f64 {
    // Master pitch is the only master-scoped speed indicator available across
    // sources. Clamp to a sane range.
    (1.0 + (pitch_pct / 100.0)).clamp(0.5, 2.0)
}

fn measurement_matches_master(
    master_source: Option<BeatSource>,
    master_device_number: u8,
    measurement: &TimingMeasurement,
) -> bool {
    match measurement.source {
        TimingSource::AbletonLink => master_source == Some(BeatSource::AbletonLink),
        TimingSource::ProLink => {
            master_source != Some(BeatSource::AbletonLink)
                && (master_device_number == 0
                    || measurement.device_number == Some(master_device_number))
        }
    }
}

#[derive(Debug, Clone)]
struct PositionOscillator {
    anchor_at: Instant,
    anchor_pos_ms: i64,
    speed: f64,
}

impl PositionOscillator {
    fn new(now: Instant, pos_ms: i64, speed: f64) -> Self {
        Self {
            anchor_at: now,
            anchor_pos_ms: pos_ms,
            speed,
        }
    }

    fn position_ms_at(&self, now: Instant) -> i64 {
        let dt = now
            .checked_duration_since(self.anchor_at)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64();
        let adv_ms = dt * 1000.0 * self.speed;
        self.anchor_pos_ms.saturating_add(adv_ms.round() as i64)
    }

    fn retime(&mut self, now: Instant, pos_ms: i64, speed: f64) {
        self.anchor_at = now;
        self.anchor_pos_ms = pos_ms;
        self.speed = speed;
    }

    fn update_speed_preserving_phase(&mut self, now: Instant, new_speed: f64) {
        let pos = self.position_ms_at(now);
        self.retime(now, pos, new_speed);
    }
}

#[derive(Debug, Clone, Copy)]
enum MtcOut {
    QuarterFrame([u8; 2]),
    FullFrame([u8; 10]),
}

#[derive(Debug, Default, Clone)]
struct MtcStats {
    qf_sent: u64,
    full_frame_resyncs: u64,
    late_wakes: u64,
    max_lateness: Duration,
}

impl MtcStats {
    fn observe_lateness(&mut self, d: Duration) {
        if !d.is_zero() {
            self.late_wakes += 1;
            if d > self.max_lateness {
                self.max_lateness = d;
            }
        }
    }
}

struct MtcScheduler {
    last_enabled: bool,
    last_playing_like: bool,
    running: bool,

    frame_rate: MtcFrameRate,

    qf_index: u8,
    next_qf_deadline: Option<Instant>,

    cycle_tc: Timecode,
    last_cycle_frame: Option<u64>,

    last_position_ms: i64,
    osc: Option<PositionOscillator>,
    playing_like_override: Option<bool>,

    stats: MtcStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayingLikeState {
    Active,
    Inactive,
}

impl MtcScheduler {
    fn new(enabled: bool, frame_rate: MtcFrameRate) -> Self {
        Self {
            last_enabled: enabled,
            last_playing_like: false,
            running: false,
            frame_rate,
            qf_index: 0,
            next_qf_deadline: None,
            cycle_tc: Timecode::zero(),
            last_cycle_frame: None,
            last_position_ms: 0,
            osc: None,
            playing_like_override: None,
            stats: MtcStats::default(),
        }
    }

    fn reset(&mut self, enabled: bool, frame_rate: MtcFrameRate) {
        *self = Self::new(enabled, frame_rate);
    }

    /// Stops the quarter-frame cycle. The next wake sends a full-frame and
    /// starts the cycle again.
    fn force_resync(&mut self) {
        self.running = false;
        self.next_qf_deadline = None;
        self.qf_index = 0;
        self.last_cycle_frame = None;
    }

    fn take_stats(&mut self) -> MtcStats {
        std::mem::take(&mut self.stats)
    }

    fn set_playing_like_override(&mut self, value: Option<bool>) {
        self.playing_like_override = value;
    }

    fn playing_like_state(&self, playing_like: bool) -> PlayingLikeState {
        if playing_like {
            PlayingLikeState::Active
        } else {
            PlayingLikeState::Inactive
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        if self.running {
            self.next_qf_deadline
        } else {
            None
        }
    }

    fn derive_position_ms(
        &mut self,
        now: Instant,
        master_source: Option<BeatSource>,
        master_device: u8,
        master_pitch_pct: f64,
        snapshot: &TimingSnapshot,
    ) -> Option<(i64, bool)> {
        let TimingSnapshot::Fresh { measurement, age } = snapshot else {
            return None;
        };

        if !measurement_matches_master(master_source, master_device, measurement) {
            return None;
        }

        // Preferred: fresh, master-scoped AbsPosition measurement.
        let is_master_scoped = measurement.device_number == Some(master_device);
        if is_master_scoped
            && measurement.kind == MeasurementKind::ProLinkAbsPositionPacket
            && measurement.playhead_ms.is_some()
            && *age <= PLAYHEAD_MAX_AGE
        {
            let pos = measurement.playhead_ms.unwrap() as i64;
            let speed = speed_factor_from_master_pitch_pct(master_pitch_pct);
            match &mut self.osc {
                Some(osc) => osc.retime(now, pos, speed),
                None => self.osc = Some(PositionOscillator::new(now, pos, speed)),
            }
            self.last_position_ms = pos;
            return Some((pos, true));
        }

        // Fallback: monotonic oscillator based on the last master-scoped timing snapshot.
        let speed = speed_factor_from_master_pitch_pct(master_pitch_pct);
        match &mut self.osc {
            Some(osc) => osc.update_speed_preserving_phase(now, speed),
            None => self.osc = Some(PositionOscillator::new(now, self.last_position_ms, speed)),
        }
        let pos = self.osc.as_ref().unwrap().position_ms_at(now);
        self.last_position_ms = pos;
        Some((pos, false))
    }

    fn on_wake(
        &mut self,
        now: Instant,
        enabled: bool,
        frame_rate: MtcFrameRate,
        master_source: Option<BeatSource>,
        master_device: u8,
        master_bpm: f64,
        master_pitch_pct: f64,
        master_is_playing: bool,
        master_last_beat_at: Option<Instant>,
        snapshot: TimingSnapshot,
    ) -> Vec<MtcOut> {
        let mut out: Vec<MtcOut> = Vec::new();

        // Config transitions.
        if enabled != self.last_enabled {
            self.last_enabled = enabled;
            if !enabled {
                self.reset(false, frame_rate);
                return out;
            }
        }

        if frame_rate != self.frame_rate {
            // Treat frame-rate changes as a resync boundary.
            self.frame_rate = frame_rate;
            self.qf_index = 0;
            self.last_cycle_frame = None;
            self.running = false;
            self.next_qf_deadline = None;
        }

        if !enabled {
            return out;
        }

        let playing_like = self.playing_like_override.unwrap_or_else(|| {
            is_playing_like(now, master_bpm, master_is_playing, master_last_beat_at)
        });

        // Stop when transport is not playing-like.
        match self.playing_like_state(playing_like) {
            PlayingLikeState::Inactive => {
                if self.running {
                    self.running = false;
                    self.next_qf_deadline = None;
                    self.qf_index = 0;
                }
                self.last_playing_like = playing_like;
                return out;
            }
            PlayingLikeState::Active => {}
        }

        // Emit a full-frame and start QFs on the transition into playing-like.
        // Also restart after a pause from stale timing or a frame-rate change.
        if playing_like && (!self.last_playing_like || !self.running) {
            let Some((pos_ms, _auth)) = self.derive_position_ms(
                now,
                master_source,
                master_device,
                master_pitch_pct,
                &snapshot,
            ) else {
                self.last_playing_like = playing_like;
                return out;
            };

            let tc = Timecode::from_position_ms(pos_ms, frame_rate.fps());
            self.cycle_tc = tc;
            self.last_cycle_frame = Some(tc.total_frames(frame_rate.fps()));
            out.push(MtcOut::FullFrame(full_frame_sysex(&tc, &frame_rate)));
            self.stats.full_frame_resyncs += 1;

            tracing::trace!(
                target: "midi.mtc",
                frame_rate = frame_rate.label(),
                tc = %tc,
                pos_ms,
                "MTC transport start/resync (full-frame)"
            );

            self.running = true;
            self.qf_index = 0;
            self.next_qf_deadline = Some(now);
        }

        self.last_playing_like = playing_like;

        if !self.running {
            return out;
        }

        let interval = qf_interval(frame_rate);
        if self.next_qf_deadline.is_none() {
            self.next_qf_deadline = Some(now);
        }

        let Some(deadline) = self.next_qf_deadline else {
            return out;
        };

        if now < deadline {
            return out;
        }

        let lateness = now
            .checked_duration_since(deadline)
            .unwrap_or(Duration::ZERO);
        self.stats.observe_lateness(lateness);

        let nominal_next = deadline + interval;
        let next = if now >= nominal_next {
            now + interval
        } else {
            nominal_next
        };
        self.next_qf_deadline = Some(next);

        // At the start of each 8-QF cycle, derive the timecode for this cycle.
        if self.qf_index == 0 {
            let Some((pos_ms, _auth)) = self.derive_position_ms(
                now,
                master_source,
                master_device,
                master_pitch_pct,
                &snapshot,
            ) else {
                // No fresh timing -> pause and wait.
                self.running = false;
                self.next_qf_deadline = None;
                return out;
            };

            let tc = Timecode::from_position_ms(pos_ms, frame_rate.fps());
            let frame = tc.total_frames(frame_rate.fps());

            if let Some(prev) = self.last_cycle_frame {
                let diff = if frame >= prev {
                    frame - prev
                } else {
                    prev - frame
                };
                if diff > DISCONTINUITY_THRESHOLD_FRAMES {
                    // Exactly one full-frame resync, then restart QF cycle.
                    out.push(MtcOut::FullFrame(full_frame_sysex(&tc, &frame_rate)));
                    self.stats.full_frame_resyncs += 1;

                    tracing::trace!(
                        target: "midi.mtc",
                        frame_rate = frame_rate.label(),
                        tc = %tc,
                        prev_frame = prev,
                        frame,
                        diff_frames = diff,
                        "MTC discontinuity detected; emitting full-frame resync"
                    );
                    self.cycle_tc = tc;
                    self.last_cycle_frame = Some(frame);
                    self.qf_index = 0;
                    self.next_qf_deadline = Some(now + interval);
                    return out;
                }
            }

            self.cycle_tc = tc;
            self.last_cycle_frame = Some(frame);
        }

        let data_byte = quarter_frame_data(&self.cycle_tc, self.qf_index, &frame_rate);
        out.push(MtcOut::QuarterFrame([0xF1, data_byte]));
        self.stats.qf_sent += 1;

        self.qf_index = (self.qf_index + 1) % 8;
        out
    }
}

// ── Quarter-frame encoding ────────────────────────────────────────────────────

/// Build the data byte for a quarter-frame message.
///
/// Format: [PPP VVVV] where PPP = piece type (0–7), VVVV = 4-bit value.
fn quarter_frame_data(tc: &Timecode, piece: u8, rate: &MtcFrameRate) -> u8 {
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

/// Build a full-frame SysEx message: F0 7F 7F 01 01 rh mm ss ff F7.
fn full_frame_sysex(tc: &Timecode, rate: &MtcFrameRate) -> [u8; 10] {
    let rh = (rate.rate_code() << 5) | (tc.hours & 0x1F);
    [
        0xF0, 0x7F, 0x7F, 0x01, 0x01, rh, tc.minutes, tc.seconds, tc.frames, 0xF7,
    ]
}

fn maybe_mark_beat_driven_playing(
    state: &SharedState,
    clock_loop_enabled: bool,
    beat_driven_playing: &mut bool,
    evt: &BeatEvent,
) {
    if clock_loop_enabled {
        return;
    }

    let BeatEvent::Beat { packet: bp, .. } = evt else {
        return;
    };

    let master = state.read().master.clone();
    if master.source == Some(BeatSource::AbletonLink) {
        return;
    }

    let master_num = master.device_number;
    let master_is_set = master_num > 0;
    if !master_is_set || bp.device_number == master_num {
        *beat_driven_playing = true;
    }
}

// ── Main MTC task ─────────────────────────────────────────────────────────────

pub async fn run(
    midi: Arc<dyn MidiTransport>,
    state: SharedState,
    cfg: SharedConfig,
    activity: Arc<Mutex<MidiActivity>>,
    mut beat_rx: broadcast::Receiver<BeatEvent>,
    mut timing_rx: watch::Receiver<()>,
) {
    let (enabled0, frame_rate0, clock_loop_enabled0) = {
        let cfg_r = cfg.read();
        (
            cfg_r.midi.mtc.enabled,
            cfg_r.midi.mtc.frame_rate,
            cfg_r.midi.clock_loop_enabled,
        )
    };
    let mut scheduler = MtcScheduler::new(enabled0, frame_rate0);
    let mut clock_loop_enabled = clock_loop_enabled0;
    let mut beat_driven_playing = false;
    let mut last_master_key = {
        let st = state.read();
        (st.master.source, st.master.device_number)
    };

    let mut last_trace_at: Instant = Instant::now();
    let mut last_connected = midi.is_connected();

    tracing::info!("MTC timecode task started");
    if !enabled0 {
        tracing::info!("MTC disabled in config");
    }

    loop {
        let sleep_target_std = scheduler.next_wake();
        match sleep_target_std {
            Some(t) => {
                tokio::select! {
                    _ = sleep_until(TokioInstant::from_std(t)) => {}
                    evt = beat_rx.recv() => {
                        if let Ok(evt) = evt {
                            maybe_mark_beat_driven_playing(
                                &state,
                                clock_loop_enabled,
                                &mut beat_driven_playing,
                                &evt,
                            );
                        }
                    }
                    changed = timing_rx.changed() => {
                        if changed.is_err() {
                            // Sender dropped; keep running.
                        }
                    }
                }
            }
            None => {
                tokio::select! {
                    _ = sleep(IDLE_POLL) => {}
                    evt = beat_rx.recv() => {
                        if let Ok(evt) = evt {
                            maybe_mark_beat_driven_playing(
                                &state,
                                clock_loop_enabled,
                                &mut beat_driven_playing,
                                &evt,
                            );
                        }
                    }
                    changed = timing_rx.changed() => {
                        if changed.is_err() {
                            // Sender dropped; keep running.
                        }
                    }
                }
            }
        }

        let now_std: Instant = TokioInstant::now().into_std();

        let (enabled, frame_rate, current_clock_loop_enabled) = {
            let cfg_r = cfg.read();
            (
                cfg_r.midi.mtc.enabled,
                cfg_r.midi.mtc.frame_rate,
                cfg_r.midi.clock_loop_enabled,
            )
        };

        if current_clock_loop_enabled != clock_loop_enabled {
            clock_loop_enabled = current_clock_loop_enabled;
            beat_driven_playing = false;
        }

        // A new MIDI device needs a full-frame before quarter-frames make sense.
        let connected = midi.is_connected();
        if connected && !last_connected {
            tracing::info!("MTC full-frame resync after MIDI output reconnect");
            scheduler.force_resync();
        }
        last_connected = connected;

        let current_master_key = {
            let st = state.read();
            (st.master.source, st.master.device_number)
        };
        if !clock_loop_enabled && current_master_key != last_master_key {
            beat_driven_playing = false;
        }
        last_master_key = current_master_key;

        if clock_loop_enabled {
            scheduler.set_playing_like_override(None);
        } else {
            scheduler.set_playing_like_override(Some(beat_driven_playing));
        }

        let (
            master_source,
            master_device,
            master_bpm,
            master_pitch_pct,
            master_is_playing,
            master_last_beat_at,
            snapshot,
        ) = {
            let st = state.read();
            (
                st.master.source,
                st.master.device_number,
                st.master.bpm,
                st.master.pitch_pct,
                st.master.is_playing,
                st.master.last_beat_at,
                st.timing.snapshot_at(now_std),
            )
        };

        let msgs = scheduler.on_wake(
            now_std,
            enabled,
            frame_rate,
            master_source,
            master_device,
            master_bpm,
            master_pitch_pct,
            master_is_playing,
            master_last_beat_at,
            snapshot,
        );

        for m in msgs {
            match m {
                MtcOut::QuarterFrame(bytes) => {
                    let _ = midi.send_message(&bytes);
                    activity.lock().mtc_quarter_frames += 1;
                }
                MtcOut::FullFrame(bytes) => {
                    let _ = midi.send_message(&bytes);
                    activity.lock().mtc_full_frames += 1;
                }
            }
        }

        if tracing::level_enabled!(tracing::Level::TRACE)
            && now_std
                .checked_duration_since(last_trace_at)
                .unwrap_or(Duration::ZERO)
                >= Duration::from_secs(1)
        {
            let stats = scheduler.take_stats();
            let next_deadline_delta_ms: Option<i64> = scheduler.next_wake().map(|d| {
                if d >= now_std {
                    d.duration_since(now_std).as_millis() as i64
                } else {
                    -(now_std.duration_since(d).as_millis() as i64)
                }
            });

            tracing::trace!(
                target: "midi.mtc",
                enabled,
                frame_rate = frame_rate.label(),
                playing = master_is_playing,
                bpm = %format!("{master_bpm:.2}"),
                pitch_pct = %format!("{master_pitch_pct:.2}"),
                qf_sent = stats.qf_sent,
                full_frame_resyncs = stats.full_frame_resyncs,
                late_wakes = stats.late_wakes,
                max_lateness_ms = (stats.max_lateness.as_secs_f64() * 1000.0),
                next_deadline_delta_ms,
                "MTC dispatch timing"
            );

            last_trace_at = now_std;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MtcFrameRate;
    use crate::state::timing::{PlayingState, TimingMeasurement, TimingModel, TimingSource};

    #[test]
    fn timecode_from_elapsed_and_total_frames_roundtrip() {
        let tc = Timecode::from_elapsed(3661.5, 30);
        assert_eq!(tc.hours, 1);
        assert_eq!(tc.minutes, 1);
        assert_eq!(tc.seconds, 1);
        assert_eq!(tc.frames, 15);
        assert_eq!(tc.total_frames(30), 109_845);
    }

    #[test]
    fn timecode_increment_wraps_all_fields() {
        let mut tc = Timecode {
            hours: 23,
            minutes: 59,
            seconds: 59,
            frames: 29,
        };
        tc.increment(30);
        assert_eq!(tc.hours, 0);
        assert_eq!(tc.minutes, 0);
        assert_eq!(tc.seconds, 0);
        assert_eq!(tc.frames, 0);
    }

    #[test]
    fn quarter_frame_piece_encoding_is_correct() {
        let tc = Timecode {
            hours: 21,
            minutes: 43,
            seconds: 59,
            frames: 27,
        };
        let r = MtcFrameRate::Fps25;

        assert_eq!(quarter_frame_data(&tc, 0, &r), 0x0B);
        assert_eq!(quarter_frame_data(&tc, 1, &r), 0x11);
        assert_eq!(quarter_frame_data(&tc, 2, &r), 0x2B);
        assert_eq!(quarter_frame_data(&tc, 3, &r), 0x33);
        assert_eq!(quarter_frame_data(&tc, 4, &r), 0x4B);
        assert_eq!(quarter_frame_data(&tc, 5, &r), 0x52);
        assert_eq!(quarter_frame_data(&tc, 6, &r), 0x65);
        assert_eq!(quarter_frame_data(&tc, 7, &r), 0x73);
    }

    #[test]
    fn full_frame_sysex_contains_rate_and_time_fields() {
        let tc = Timecode {
            hours: 10,
            minutes: 11,
            seconds: 12,
            frames: 13,
        };
        let msg = full_frame_sysex(&tc, &MtcFrameRate::Fps30);
        assert_eq!(msg, [0xF0, 0x7F, 0x7F, 0x01, 0x01, 0x6A, 11, 12, 13, 0xF7]);
    }

    #[test]
    fn scheduler_qf_cadence_is_stable_and_never_bursts_when_driven_by_deadlines() {
        let base = Instant::now();
        let mut timing = TimingModel::default();
        timing.observe(TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, base));

        let mut sched = MtcScheduler::new(true, MtcFrameRate::Fps25);

        // First wake: should emit a full-frame (start/resync boundary) and a QF.
        let out0 = sched.on_wake(
            base,
            true,
            MtcFrameRate::Fps25,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(base),
        );
        assert!(out0.iter().any(|m| matches!(m, MtcOut::FullFrame(_))));
        assert!(out0.iter().any(|m| matches!(m, MtcOut::QuarterFrame(_))));

        let interval = qf_interval(MtcFrameRate::Fps25);
        let mut now = sched.next_wake().unwrap();

        // Drive a handful of quarter-frames at their deadlines.
        for _ in 0..32 {
            let out = sched.on_wake(
                now,
                true,
                MtcFrameRate::Fps25,
                Some(BeatSource::AbletonLink),
                0,
                120.0,
                0.0,
                true,
                Some(base),
                timing.snapshot_at(now),
            );
            // Exactly one quarter-frame per wake.
            assert_eq!(
                out.iter()
                    .filter(|m| matches!(m, MtcOut::QuarterFrame(_)))
                    .count(),
                1
            );

            let next = sched.next_wake().unwrap();
            assert_eq!(next - now, interval);
            now = next;
        }

        // Simulate a late wake: must not burst-send multiple QFs.
        let late = now + Duration::from_millis(150);
        let out_late = sched.on_wake(
            late,
            true,
            MtcFrameRate::Fps25,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(late),
        );
        assert_eq!(
            out_late
                .iter()
                .filter(|m| matches!(m, MtcOut::QuarterFrame(_)))
                .count(),
            1
        );
    }

    #[test]
    fn scheduler_seek_triggers_exactly_one_full_frame_then_quarter_frames_resume() {
        let base = Instant::now();
        let mut timing = TimingModel::default();

        // Start with a master-scoped AbsPosition measurement.
        let ap0 = TimingMeasurement {
            received_at: base,
            source: TimingSource::ProLink,
            kind: MeasurementKind::ProLinkAbsPositionPacket,
            device_number: Some(1),
            playing: PlayingState::Unknown,
            bpm: 128.0,
            effective_bpm: 128.0,
            beat_phase: None,
            bar_phase: None,
            beat_in_bar: None,
            playhead_ms: Some(1_000),
        };
        timing.observe(ap0);

        let mut sched = MtcScheduler::new(true, MtcFrameRate::Fps30);

        let out0 = sched.on_wake(
            base,
            true,
            MtcFrameRate::Fps30,
            Some(BeatSource::ProLink),
            1,
            128.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(base),
        );
        assert!(out0.iter().any(|m| matches!(m, MtcOut::FullFrame(_))));

        // Advance to the next cycle boundary and then jump the playhead far ahead.
        let interval = qf_interval(MtcFrameRate::Fps30);
        let cycle_start = base + interval * 8u32;

        // Force scheduler to be at qf_index==0 by driving to the boundary.
        let mut t = base;
        for _ in 0..7 {
            let next = sched.next_wake().unwrap_or(t);
            t = next;
            let _ = sched.on_wake(
                t,
                true,
                MtcFrameRate::Fps30,
                Some(BeatSource::ProLink),
                1,
                128.0,
                0.0,
                true,
                Some(base),
                timing.snapshot_at(t),
            );
        }

        let ap_seek = TimingMeasurement {
            received_at: cycle_start,
            source: TimingSource::ProLink,
            kind: MeasurementKind::ProLinkAbsPositionPacket,
            device_number: Some(1),
            playing: PlayingState::Unknown,
            bpm: 128.0,
            effective_bpm: 128.0,
            beat_phase: None,
            bar_phase: None,
            beat_in_bar: None,
            playhead_ms: Some(60_000),
        };
        timing.observe(ap_seek);

        let out_seek = sched.on_wake(
            cycle_start,
            true,
            MtcFrameRate::Fps30,
            Some(BeatSource::ProLink),
            1,
            128.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(cycle_start),
        );
        assert_eq!(
            out_seek
                .iter()
                .filter(|m| matches!(m, MtcOut::FullFrame(_)))
                .count(),
            1
        );

        // Next wake should resume with a quarter-frame (no repeated full-frame).
        let after = sched.next_wake().unwrap();
        let out_after = sched.on_wake(
            after,
            true,
            MtcFrameRate::Fps30,
            Some(BeatSource::ProLink),
            1,
            128.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(after),
        );
        assert_eq!(
            out_after
                .iter()
                .filter(|m| matches!(m, MtcOut::FullFrame(_)))
                .count(),
            0
        );
        assert_eq!(
            out_after
                .iter()
                .filter(|m| matches!(m, MtcOut::QuarterFrame(_)))
                .count(),
            1
        );
    }

    #[test]
    fn scheduler_allows_playing_like_output_when_master_not_playing_but_beat_within_grace() {
        let base = Instant::now();
        let mut timing = TimingModel::default();
        timing.observe(TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, base));

        let mut sched = MtcScheduler::new(true, MtcFrameRate::Fps25);

        // master_is_playing=false, but last_beat_at is recent -> playing_like=true.
        let out = sched.on_wake(
            base,
            true,
            MtcFrameRate::Fps25,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            false,
            Some(base - Duration::from_millis(100)),
            timing.snapshot_at(base),
        );

        assert!(out.iter().any(|m| matches!(m, MtcOut::QuarterFrame(_))));
        assert!(out.iter().any(|m| matches!(m, MtcOut::FullFrame(_))));
    }

    #[test]
    fn scheduler_stale_timing_pauses_quarter_frames_and_resumes_on_fresh_timing() {
        let base = Instant::now();
        let mut timing = TimingModel::default();
        timing.observe(TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, base));

        let frame_rate = MtcFrameRate::Fps25;
        let interval = qf_interval(frame_rate);

        let mut sched = MtcScheduler::new(true, frame_rate);

        // Start running.
        let out0 = sched.on_wake(
            base,
            true,
            frame_rate,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(base),
        );
        assert!(out0.iter().any(|m| matches!(m, MtcOut::QuarterFrame(_))));

        // Drive to the next cycle boundary so qf_index==0 on the next wake.
        let mut now = sched.next_wake().unwrap();
        for _ in 0..7 {
            let out = sched.on_wake(
                now,
                true,
                frame_rate,
                Some(BeatSource::AbletonLink),
                0,
                120.0,
                0.0,
                true,
                Some(base),
                timing.snapshot_at(now),
            );
            assert_eq!(
                out.iter()
                    .filter(|m| matches!(m, MtcOut::QuarterFrame(_)))
                    .count(),
                1
            );
            now = sched.next_wake().unwrap();
        }
        assert_eq!(now - base, interval * 8u32);

        // Jump ahead so the timing snapshot is stale: at qf_index==0, scheduler must pause.
        let stale_now = base + Duration::from_secs(2);
        let out_stale = sched.on_wake(
            stale_now,
            true,
            frame_rate,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(stale_now),
        );
        assert!(out_stale.is_empty());
        assert_eq!(sched.next_wake(), None);

        // While paused, transport gating must still apply. First transition to not-playing-like.
        let stop_now = stale_now + Duration::from_millis(600);
        let out_stop = sched.on_wake(
            stop_now,
            true,
            frame_rate,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            false,
            Some(base),
            timing.snapshot_at(stop_now),
        );
        assert!(out_stop.is_empty());

        // Fresh timing returns with a recent beat -> transition back to playing-like should restart output.
        let resume_now = stop_now + Duration::from_millis(1);
        timing.observe(TimingMeasurement::from_link(
            120.0, 1, 0.0, 0.0, true, resume_now,
        ));
        let out_resume = sched.on_wake(
            resume_now,
            true,
            frame_rate,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            false,
            Some(resume_now),
            timing.snapshot_at(resume_now),
        );
        assert!(out_resume.iter().any(|m| matches!(m, MtcOut::FullFrame(_))));
        assert!(out_resume
            .iter()
            .any(|m| matches!(m, MtcOut::QuarterFrame(_))));
        assert_eq!(
            out_resume
                .iter()
                .filter(|m| matches!(m, MtcOut::QuarterFrame(_)))
                .count(),
            1
        );
    }

    fn link_wake(
        sched: &mut MtcScheduler,
        timing: &TimingModel,
        now: Instant,
        frame_rate: MtcFrameRate,
    ) -> Vec<MtcOut> {
        sched.on_wake(
            now,
            true,
            frame_rate,
            Some(BeatSource::AbletonLink),
            0,
            120.0,
            0.0,
            true,
            Some(now),
            timing.snapshot_at(now),
        )
    }

    fn count_full_frames(out: &[MtcOut]) -> usize {
        out.iter()
            .filter(|m| matches!(m, MtcOut::FullFrame(_)))
            .count()
    }

    #[test]
    fn scheduler_restarts_after_stale_pause_while_still_playing_like() {
        let base = Instant::now();
        let frame_rate = MtcFrameRate::Fps25;
        let mut timing = TimingModel::default();
        timing.observe(TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, base));
        let mut sched = MtcScheduler::new(true, frame_rate);

        assert_eq!(
            count_full_frames(&link_wake(&mut sched, &timing, base, frame_rate)),
            1
        );
        for _ in 0..7 {
            let now = sched.next_wake().unwrap();
            link_wake(&mut sched, &timing, now, frame_rate);
        }

        // Stale timing at the cycle boundary pauses output. The transport stays playing-like.
        let stale_now = base + Duration::from_secs(2);
        assert!(link_wake(&mut sched, &timing, stale_now, frame_rate).is_empty());
        assert_eq!(sched.next_wake(), None);

        // Fresh timing returns without a playing-like edge. Output must restart.
        let resume_now = stale_now + Duration::from_millis(25);
        timing.observe(TimingMeasurement::from_link(
            120.0, 1, 0.0, 0.0, true, resume_now,
        ));
        let out = link_wake(&mut sched, &timing, resume_now, frame_rate);
        assert_eq!(count_full_frames(&out), 1);
        assert!(out.iter().any(|m| matches!(m, MtcOut::QuarterFrame(_))));
        assert!(sched.next_wake().is_some());
    }

    #[test]
    fn scheduler_restarts_after_frame_rate_change() {
        let base = Instant::now();
        let mut timing = TimingModel::default();
        timing.observe(TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, base));
        let mut sched = MtcScheduler::new(true, MtcFrameRate::Fps25);

        link_wake(&mut sched, &timing, base, MtcFrameRate::Fps25);
        let now = sched.next_wake().unwrap();

        let out = link_wake(&mut sched, &timing, now, MtcFrameRate::Fps30);
        let full_frame = out.iter().find_map(|m| match m {
            MtcOut::FullFrame(bytes) => Some(*bytes),
            MtcOut::QuarterFrame(_) => None,
        });
        let full_frame = full_frame.expect("frame-rate change must emit a full-frame");
        assert_eq!(full_frame[5] >> 5, MtcFrameRate::Fps30.rate_code());
        assert!(out.iter().any(|m| matches!(m, MtcOut::QuarterFrame(_))));
        assert!(sched.next_wake().is_some());
    }

    #[test]
    fn scheduler_ignores_fresh_non_master_timing() {
        let base = Instant::now();
        let mut timing = TimingModel::default();
        timing.observe(TimingMeasurement {
            received_at: base,
            source: TimingSource::ProLink,
            kind: MeasurementKind::ProLinkAbsPositionPacket,
            device_number: Some(2),
            playing: PlayingState::Unknown,
            bpm: 128.0,
            effective_bpm: 128.0,
            beat_phase: None,
            bar_phase: None,
            beat_in_bar: None,
            playhead_ms: Some(5_000),
        });

        let mut sched = MtcScheduler::new(true, MtcFrameRate::Fps25);
        let out = sched.on_wake(
            base,
            true,
            MtcFrameRate::Fps25,
            Some(BeatSource::ProLink),
            1,
            128.0,
            0.0,
            true,
            Some(base),
            timing.snapshot_at(base),
        );

        assert!(out.is_empty());
        assert_eq!(sched.next_wake(), None);
    }
}
