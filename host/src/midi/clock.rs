//! MIDI Clock generator — 24 pulses-per-quarter-note.
//!
//! Generates MIDI Timing Clock (0xF8) messages at the correct rate for the
//! current BPM, plus MIDI Start (0xFA), Stop (0xFC), and Continue (0xFB)
//! messages when the master deck starts or stops.
//!
//! Uses deadline-based sleeping: computes the exact instant of the next pulse
//! and sleeps until then (via `tokio::select!` with `sleep_until`), rather
//! than polling at a fixed interval.  Beat events arriving mid-sleep wake
//! the task immediately for phase correction.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::config::SharedConfig;
use crate::midi::MidiTransport;
use crate::prolink::beat_listener::BeatEvent;
use crate::state::timing::{TimingMeasurement, TimingSnapshot, TimingSource};
use crate::state::{BeatSource, SharedState};
use crate::tui::state::MidiActivity;

// ── MIDI byte constants ───────────────────────────────────────────────────────

const MSG_CLOCK: u8 = 0xF8;
const MSG_START: u8 = 0xFA;
const MSG_CONTINUE: u8 = 0xFB;
const MSG_STOP: u8 = 0xFC;

/// Pulses per quarter note — MIDI spec, always 24.
const PPQ: u64 = 24;

const AUTO_OFFSET_EWMA_ALPHA: f64 = 0.2;
const AUTO_OFFSET_DEADBAND_MS: f64 = 0.8;
const AUTO_OFFSET_CLAMP_MS: f64 = 20.0;

// ── Clock state ───────────────────────────────────────────────────────────────

struct ClockState {
    /// Current inter-pulse interval in nanoseconds (derived from BPM).
    interval_ns: u64,
    /// Time we last sent a clock pulse.
    last_pulse: Instant,
    /// Current pulse index within the quarter note (0–23).
    pulse_index: u64,
    /// Whether we are currently in the "running" state.
    running: bool,
    /// Whether the clock has been started at least once (for Continue vs Start).
    has_started: bool,
    /// BPM at last update (used to detect changes).
    last_bpm: f64,
    /// Whether we're waiting for a downbeat (beat 1) before starting.
    waiting_for_downbeat: bool,
    /// Phrase beat counter used for status/UI updates.
    beat_count: u8,
    /// Beats observed while waiting for phrase start.
    wait_beats_seen: u8,
    /// Last timing measurement timestamp processed from shared timing model.
    last_timing_received_at: Option<Instant>,
    /// Last observed beat-in-bar from timing snapshots (for edge detection).
    last_timing_beat_in_bar: Option<u8>,
    /// Downbeat edges since the last bar-based realignment.
    bars_since_realign: u8,
    /// Optional active phase resync slew used when smoothing is enabled.
    phase_slew: Option<PhaseSlew>,
    /// Adaptive software offset (ms) derived from phase-error EWMA.
    auto_offset_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockTransportState {
    Idle,
    WaitingForDownbeat { wait_beats_seen: u8 },
    Running,
}

#[derive(Debug, Clone, Copy)]
struct PhaseSlew {
    started_at: Instant,
    duration: Duration,
    total_correction_ns: i128,
    applied_correction_ns: i128,
}

impl ClockState {
    fn new() -> Self {
        Self {
            interval_ns: bpm_to_interval_ns(120.0),
            last_pulse: Instant::now(),
            pulse_index: 0,
            running: false,
            has_started: false,
            last_bpm: 0.0,
            waiting_for_downbeat: true,
            beat_count: 0,
            wait_beats_seen: 0,
            last_timing_received_at: None,
            last_timing_beat_in_bar: None,
            bars_since_realign: 0,
            phase_slew: None,
            auto_offset_ms: 0.0,
        }
    }

    fn set_bpm(&mut self, bpm: f64) {
        if bpm > 0.0 && (bpm - self.last_bpm).abs() > 0.01 {
            self.interval_ns = bpm_to_interval_ns(bpm);
            self.last_bpm = bpm;
        }
    }

    fn arm_wait_for_phrase_start(&mut self) {
        self.set_transport_state(ClockTransportState::WaitingForDownbeat { wait_beats_seen: 0 });
        self.reset_auto_offset();
        self.last_pulse = Instant::now();
    }

    fn reset_auto_offset(&mut self) {
        self.auto_offset_ms = 0.0;
    }

    fn transport_state(&self) -> ClockTransportState {
        if self.running {
            ClockTransportState::Running
        } else if self.waiting_for_downbeat {
            ClockTransportState::WaitingForDownbeat {
                wait_beats_seen: self.wait_beats_seen,
            }
        } else {
            ClockTransportState::Idle
        }
    }

    fn set_transport_state(&mut self, state: ClockTransportState) {
        match state {
            ClockTransportState::Idle => {
                self.running = false;
                self.waiting_for_downbeat = false;
                self.wait_beats_seen = 0;
                self.last_timing_beat_in_bar = None;
                self.bars_since_realign = 0;
                self.phase_slew = None;
                self.reset_auto_offset();
            }
            ClockTransportState::WaitingForDownbeat { wait_beats_seen } => {
                self.running = false;
                self.waiting_for_downbeat = true;
                self.wait_beats_seen = wait_beats_seen;
                self.last_timing_beat_in_bar = None;
                self.bars_since_realign = 0;
                self.phase_slew = None;
            }
            ClockTransportState::Running => {
                self.running = true;
                self.waiting_for_downbeat = false;
                self.wait_beats_seen = 0;
                self.last_timing_beat_in_bar = None;
                self.bars_since_realign = 0;
                self.phase_slew = None;
            }
        }
    }

    fn transition_to_running(&mut self) {
        self.set_transport_state(ClockTransportState::Running);
    }

    fn transition_to_idle(&mut self) {
        self.set_transport_state(ClockTransportState::Idle);
    }
}

fn bpm_to_interval_ns(bpm: f64) -> u64 {
    // interval = 60s / (bpm * PPQ) converted to nanoseconds
    let ns = 60.0e9 / (bpm * PPQ as f64);
    ns.round() as u64
}

fn signed_delta_ms(a: Instant, b: Instant) -> f64 {
    if a >= b {
        a.duration_since(b).as_secs_f64() * 1000.0
    } else {
        -(b.duration_since(a).as_secs_f64() * 1000.0)
    }
}

fn signed_delta_ns(a: Instant, b: Instant) -> i128 {
    if a >= b {
        a.duration_since(b).as_nanos() as i128
    } else {
        -((b.duration_since(a).as_nanos()) as i128)
    }
}

fn beat_timing_delta_ms(cs: &ClockState, beat_at: Instant) -> Option<f64> {
    if !cs.running || cs.interval_ns == 0 {
        return None;
    }

    let next_boundary = cs.last_pulse + Duration::from_nanos(cs.interval_ns);
    let prev_boundary = cs.last_pulse;
    let prev_delta = signed_delta_ms(prev_boundary, beat_at);
    let next_delta = signed_delta_ms(next_boundary, beat_at);

    if prev_delta.abs() <= next_delta.abs() {
        Some(prev_delta)
    } else {
        Some(next_delta)
    }
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

fn timing_snapshot_beat_edge(cs: &mut ClockState, measurement: &TimingMeasurement) -> bool {
    let Some(beat_in_bar) = measurement.beat_in_bar else {
        cs.last_timing_beat_in_bar = None;
        return false;
    };

    let edge = cs.last_timing_beat_in_bar != Some(beat_in_bar);
    cs.last_timing_beat_in_bar = Some(beat_in_bar);
    edge
}

fn beat_at_from_measurement(
    _cs: &ClockState,
    now: Instant,
    measurement: &TimingMeasurement,
) -> Option<Instant> {
    let beat_phase = measurement.beat_phase?;
    if measurement.effective_bpm <= 0.0 {
        return None;
    }

    let beat_dur_secs = 60.0 / measurement.effective_bpm;
    let age_secs = now
        .checked_duration_since(measurement.received_at)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64();
    let phase_now = (beat_phase.clamp(0.0, 1.0) + (age_secs / beat_dur_secs)).fract();
    let phase_secs = phase_now * beat_dur_secs;
    now.checked_sub(Duration::from_secs_f64(phase_secs))
}

fn update_auto_offset_ms(cs: &mut ClockState, phase_error_ms: f64) {
    if phase_error_ms.abs() <= AUTO_OFFSET_DEADBAND_MS {
        return;
    }

    let target_offset_ms = (-phase_error_ms).clamp(-AUTO_OFFSET_CLAMP_MS, AUTO_OFFSET_CLAMP_MS);
    cs.auto_offset_ms = ((1.0 - AUTO_OFFSET_EWMA_ALPHA) * cs.auto_offset_ms)
        + (AUTO_OFFSET_EWMA_ALPHA * target_offset_ms);
    cs.auto_offset_ms = cs
        .auto_offset_ms
        .clamp(-AUTO_OFFSET_CLAMP_MS, AUTO_OFFSET_CLAMP_MS);
}

fn handle_timing_snapshot(
    cs: &mut ClockState,
    state: &SharedState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
    resync_interval_bars: u8,
    smoothing_ms: u64,
    now: Instant,
) {
    let (master, snapshot) = {
        let st = state.read();
        (st.master.clone(), st.timing.snapshot_at(now))
    };

    let TimingSnapshot::Fresh { measurement, .. } = snapshot else {
        return;
    };

    if cs.last_timing_received_at == Some(measurement.received_at) {
        return;
    }
    cs.last_timing_received_at = Some(measurement.received_at);

    if !measurement_matches_master(master.source, master.device_number, &measurement) {
        return;
    }

    cs.set_bpm(measurement.effective_bpm);

    if let Some(mut a) = activity.try_lock() {
        a.clock_running = cs.running;
        a.clock_waiting_for_phrase = cs.waiting_for_downbeat;
        a.clock_wait_beats_seen = cs.wait_beats_seen;
        a.clock_phrase_beat = cs.beat_count;
        a.clock_pulse_index = cs.pulse_index;
        if let Some(beat_at) = beat_at_from_measurement(cs, now, &measurement) {
            a.clock_timing_delta_ms = beat_timing_delta_ms(cs, beat_at);
        }
    }

    let phrase_beat = if master.phrase_16_beat > 0 {
        master.phrase_16_beat as u32
    } else {
        ((cs.beat_count as u32) % 16) + 1
    };

    let beat_in_bar = measurement.beat_in_bar.unwrap_or(master.beat_in_bar);
    let is_beat_edge = timing_snapshot_beat_edge(cs, &measurement);

    if matches!(
        cs.transport_state(),
        ClockTransportState::WaitingForDownbeat { .. }
    ) {
        if is_beat_edge && beat_in_bar == 1 {
            cs.wait_beats_seen = cs.wait_beats_seen.saturating_add(1);
            let fallback_start = cs.wait_beats_seen >= resync_interval_bars.max(1);
            if phrase_beat == 1 || fallback_start {
                cs.transition_to_running();
                cs.beat_count = 1;
                cs.bars_since_realign = 0;
                cs.has_started = true;
                let _ = midi.send_message(&[MSG_START]);
                let _ = midi.send_message(&[MSG_CLOCK]);
                if let Some(mut a) = activity.try_lock() {
                    a.clock_last_start_at = Some(Instant::now());
                    a.clock_pulses += 1;
                    a.clock_last_pulse_at = Some(Instant::now());
                    a.clock_running = true;
                    a.clock_waiting_for_phrase = false;
                    a.clock_wait_beats_seen = 0;
                    a.clock_phrase_beat = cs.beat_count;
                    a.clock_pulse_index = cs.pulse_index;
                    if let Some(beat_at) = beat_at_from_measurement(cs, now, &measurement) {
                        a.clock_timing_delta_ms = beat_timing_delta_ms(cs, beat_at);
                    }
                }
                cs.pulse_index = 0;
                cs.last_pulse = now;
            }
        }
    } else if cs.running && is_beat_edge {
        if let Some(beat_at) = beat_at_from_measurement(cs, now, &measurement) {
            if let Some(phase_error_ms) = beat_timing_delta_ms(cs, beat_at) {
                update_auto_offset_ms(cs, phase_error_ms);
            }
            maybe_resync_on_bar_boundary(
                cs,
                midi,
                activity,
                resync_interval_bars,
                smoothing_ms,
                beat_in_bar,
                beat_at,
            );
        }
        cs.beat_count = phrase_beat as u8;
    }
}

fn apply_pending_phase_slew(cs: &mut ClockState, now: Instant) {
    let Some(mut slew) = cs.phase_slew else {
        return;
    };

    let elapsed = now
        .checked_duration_since(slew.started_at)
        .unwrap_or(Duration::ZERO)
        .min(slew.duration);
    let elapsed_ns = elapsed.as_nanos() as i128;
    let duration_ns = slew.duration.as_nanos() as i128;
    if duration_ns <= 0 {
        cs.phase_slew = None;
        return;
    }

    let target_correction_ns = (slew.total_correction_ns * elapsed_ns) / duration_ns;
    let delta_ns = target_correction_ns - slew.applied_correction_ns;
    if delta_ns > 0 {
        cs.last_pulse += Duration::from_nanos(delta_ns as u64);
    } else if delta_ns < 0 {
        let back = Duration::from_nanos((-delta_ns) as u64);
        cs.last_pulse = cs.last_pulse.checked_sub(back).unwrap_or(cs.last_pulse);
    }
    slew.applied_correction_ns = target_correction_ns;

    if elapsed >= slew.duration {
        cs.phase_slew = None;
        cs.pulse_index = 0;
    } else {
        cs.phase_slew = Some(slew);
    }
}

fn apply_full_bar_resync_hard(
    cs: &mut ClockState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
    beat_at: Instant,
) {
    cs.phase_slew = None;
    let _ = midi.send_message(&[MSG_STOP]);
    let _ = midi.send_message(&[MSG_START]);
    let _ = midi.send_message(&[MSG_CLOCK]);
    cs.running = true;
    cs.waiting_for_downbeat = false;
    cs.wait_beats_seen = 0;
    cs.has_started = true;
    cs.pulse_index = 0;
    cs.last_pulse = beat_at;

    if let Some(mut a) = activity.try_lock() {
        a.clock_last_start_at = Some(Instant::now());
        a.clock_pulses += 1;
        a.clock_last_pulse_at = Some(Instant::now());
        a.clock_running = true;
        a.clock_waiting_for_phrase = false;
        a.clock_wait_beats_seen = 0;
        a.clock_phrase_beat = cs.beat_count;
        a.clock_pulse_index = cs.pulse_index;
    }
}

fn start_full_bar_resync_slewed(cs: &mut ClockState, beat_at: Instant, smoothing_ms: u64) {
    apply_pending_phase_slew(cs, beat_at);
    let correction_ns = signed_delta_ns(beat_at, cs.last_pulse);
    if correction_ns == 0 {
        cs.phase_slew = None;
        cs.pulse_index = 0;
        return;
    }

    let duration = Duration::from_millis(smoothing_ms.max(1));
    cs.phase_slew = Some(PhaseSlew {
        started_at: beat_at,
        duration,
        total_correction_ns: correction_ns,
        applied_correction_ns: 0,
    });
}

fn maybe_resync_on_bar_boundary(
    cs: &mut ClockState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
    resync_interval_bars: u8,
    smoothing_ms: u64,
    beat_in_bar: u8,
    beat_at: Instant,
) {
    if beat_in_bar != 1 {
        return;
    }

    cs.bars_since_realign = cs.bars_since_realign.saturating_add(1);
    let interval = resync_interval_bars.max(1);
    if cs.bars_since_realign < interval {
        return;
    }

    if smoothing_ms == 0 {
        apply_full_bar_resync_hard(cs, midi, activity, beat_at);
    } else {
        start_full_bar_resync_slewed(cs, beat_at, smoothing_ms);
    }
    cs.bars_since_realign = 0;
}

// ── Beat event handler ────────────────────────────────────────────────────────

/// Process a single beat event, updating clock BPM and phase correction.
#[allow(dead_code)]
fn handle_beat_event(
    cs: &mut ClockState,
    state: &SharedState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
    resync_interval_bars: u8,
    smoothing_ms: u64,
    evt: BeatEvent,
) {
    let source = state.read().master.source;
    match evt {
        BeatEvent::Beat {
            packet: bp,
            received_at,
        } => {
            if source == Some(BeatSource::AbletonLink) {
                return;
            }

            let master = state.read().master.clone();
            let master_num = master.device_number;
            let master_is_set = master_num > 0;
            let from_master = bp.device_number == master_num;

            if !master_is_set || from_master {
                cs.set_bpm(bp.effective_bpm);
                {
                    if let Some(mut a) = activity.try_lock() {
                        a.clock_running = cs.running;
                        a.clock_waiting_for_phrase = cs.waiting_for_downbeat;
                        a.clock_wait_beats_seen = cs.wait_beats_seen;
                        a.clock_phrase_beat = cs.beat_count;
                        a.clock_pulse_index = cs.pulse_index;
                        a.clock_timing_delta_ms = beat_timing_delta_ms(&cs, Instant::now());
                    }
                }

                let phrase_beat = if master.phrase_16_beat > 0 {
                    master.phrase_16_beat as u32
                } else {
                    ((cs.beat_count as u32) % 16) + 1
                };
                let is_phrase_start = phrase_beat == 1 && bp.beat_in_bar == 1;

                if cs.waiting_for_downbeat {
                    if bp.beat_in_bar == 1 {
                        cs.wait_beats_seen = cs.wait_beats_seen.saturating_add(1);
                    }
                    let fallback_start =
                        cs.wait_beats_seen >= resync_interval_bars.max(1) && bp.beat_in_bar == 1;
                    if is_phrase_start || fallback_start {
                        cs.transition_to_running();
                        cs.beat_count = 1;
                        cs.bars_since_realign = 0;
                        cs.has_started = true;
                        let _ = midi.send_message(&[MSG_START]);
                        let _ = midi.send_message(&[MSG_CLOCK]);
                        {
                            if let Some(mut a) = activity.try_lock() {
                                a.clock_last_start_at = Some(Instant::now());
                                a.clock_pulses += 1;
                                a.clock_last_pulse_at = Some(Instant::now());
                                a.clock_running = true;
                                a.clock_waiting_for_phrase = false;
                                a.clock_wait_beats_seen = 0;
                                a.clock_phrase_beat = cs.beat_count;
                                a.clock_pulse_index = cs.pulse_index;
                                a.clock_timing_delta_ms = beat_timing_delta_ms(&cs, Instant::now());
                            }
                        }
                        cs.pulse_index = 0;
                        cs.last_pulse = Instant::now();
                    }
                } else if cs.running {
                    maybe_resync_on_bar_boundary(
                        cs,
                        midi,
                        activity,
                        resync_interval_bars,
                        smoothing_ms,
                        bp.beat_in_bar,
                        received_at,
                    );
                    cs.beat_count = phrase_beat as u8;
                }
            }
        }
        BeatEvent::AbsPosition { packet: ap, .. } => {
            if source == Some(BeatSource::AbletonLink) {
                return;
            }
            let master = state.read().master.clone();
            let master_num = master.device_number;
            let master_is_set = master_num > 0;
            let from_master = ap.device_number == master_num;
            if !master_is_set || from_master {
                cs.set_bpm(ap.effective_bpm);
            }
        }
        BeatEvent::LinkBeat {
            bpm,
            beat_in_bar,
            received_at,
            ..
        } => {
            if source == Some(BeatSource::ProLink) {
                return;
            }

            cs.set_bpm(bpm);

            {
                if let Some(mut a) = activity.try_lock() {
                    a.clock_running = cs.running;
                    a.clock_waiting_for_phrase = cs.waiting_for_downbeat;
                    a.clock_wait_beats_seen = cs.wait_beats_seen;
                    a.clock_phrase_beat = cs.beat_count;
                    a.clock_pulse_index = cs.pulse_index;
                    a.clock_timing_delta_ms = beat_timing_delta_ms(&cs, Instant::now());
                }
            }

            let master = state.read().master.clone();
            let phrase_beat = if master.phrase_16_beat > 0 {
                master.phrase_16_beat as u32
            } else {
                ((cs.beat_count as u32) % 16) + 1
            };
            let is_phrase_start = phrase_beat == 1 && beat_in_bar == 1;

            if cs.waiting_for_downbeat {
                if beat_in_bar == 1 {
                    cs.wait_beats_seen = cs.wait_beats_seen.saturating_add(1);
                }
                let fallback_start =
                    cs.wait_beats_seen >= resync_interval_bars.max(1) && beat_in_bar == 1;
                if is_phrase_start || fallback_start {
                    cs.transition_to_running();
                    cs.beat_count = 1;
                    cs.bars_since_realign = 0;
                    cs.has_started = true;
                    let _ = midi.send_message(&[MSG_START]);
                    let _ = midi.send_message(&[MSG_CLOCK]);
                    {
                        if let Some(mut a) = activity.try_lock() {
                            a.clock_last_start_at = Some(Instant::now());
                            a.clock_pulses += 1;
                            a.clock_last_pulse_at = Some(Instant::now());
                            a.clock_running = true;
                            a.clock_waiting_for_phrase = false;
                            a.clock_wait_beats_seen = 0;
                            a.clock_phrase_beat = cs.beat_count;
                            a.clock_pulse_index = cs.pulse_index;
                            a.clock_timing_delta_ms = beat_timing_delta_ms(&cs, Instant::now());
                        }
                    }
                    cs.pulse_index = 0;
                    cs.last_pulse = Instant::now();
                }
            } else if cs.running {
                maybe_resync_on_bar_boundary(
                    cs,
                    midi,
                    activity,
                    resync_interval_bars,
                    smoothing_ms,
                    beat_in_bar,
                    received_at,
                );
                cs.beat_count = phrase_beat as u8;
            }
        }
    }
}

/// Drain all queued beat events without blocking.
#[allow(dead_code)]
fn drain_beat_events(
    cs: &mut ClockState,
    state: &SharedState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
    resync_interval_bars: u8,
    smoothing_ms: u64,
    beat_rx: &mut broadcast::Receiver<BeatEvent>,
) {
    loop {
        match beat_rx.try_recv() {
            Ok(evt) => handle_beat_event(
                cs,
                state,
                midi,
                activity,
                resync_interval_bars,
                smoothing_ms,
                evt,
            ),
            Err(_) => break,
        }
    }
}

fn handle_master_change(
    cs: &mut ClockState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
) {
    if cs.running {
        let _ = midi.send_message(&[MSG_STOP]);
    }

    cs.transition_to_idle();
    cs.beat_count = 0;
    cs.arm_wait_for_phrase_start();

    if let Some(mut a) = activity.try_lock() {
        a.clock_running = false;
        a.clock_waiting_for_phrase = true;
        a.clock_wait_beats_seen = 0;
        a.clock_phrase_beat = 0;
        a.clock_pulse_index = 0;
    }
}

fn handle_master_change_without_phrase_wait(
    cs: &mut ClockState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
) {
    if cs.running {
        let _ = midi.send_message(&[MSG_STOP]);
    }

    cs.transition_to_idle();
    cs.beat_count = 0;
    cs.last_timing_received_at = None;
    cs.last_pulse = Instant::now();

    if let Some(mut a) = activity.try_lock() {
        a.clock_running = false;
        a.clock_waiting_for_phrase = false;
        a.clock_wait_beats_seen = 0;
        a.clock_phrase_beat = 0;
        a.clock_pulse_index = 0;
    }
}

fn handle_prolink_beat_loop_disabled(
    cs: &mut ClockState,
    state: &SharedState,
    midi: &Arc<dyn MidiTransport>,
    activity: &Arc<Mutex<MidiActivity>>,
    resync_interval_bars: u8,
    smoothing_ms: u64,
    evt: BeatEvent,
) {
    let BeatEvent::Beat {
        packet: bp,
        received_at,
    } = evt
    else {
        return;
    };

    let master = state.read().master.clone();
    if master.source == Some(BeatSource::AbletonLink) {
        return;
    }

    let master_num = master.device_number;
    let master_is_set = master_num > 0;
    let from_master = bp.device_number == master_num;
    if master_is_set && !from_master {
        return;
    }

    cs.set_bpm(bp.effective_bpm);
    if cs.running {
        cs.beat_count = cs.beat_count.wrapping_add(1).max(1);
        maybe_resync_on_bar_boundary(
            cs,
            midi,
            activity,
            resync_interval_bars,
            smoothing_ms,
            bp.beat_in_bar,
            received_at,
        );

        if let Some(mut a) = activity.try_lock() {
            a.clock_running = true;
            a.clock_waiting_for_phrase = false;
            a.clock_wait_beats_seen = 0;
            a.clock_phrase_beat = ((cs.beat_count.saturating_sub(1)) % 16) + 1;
            a.clock_pulse_index = cs.pulse_index;
            a.clock_timing_delta_ms = beat_timing_delta_ms(cs, received_at);
        }
    } else {
        cs.transition_to_running();
        cs.pulse_index = 0;
        cs.last_pulse = received_at;
        cs.beat_count = 1;
        let msg = if cs.has_started {
            MSG_CONTINUE
        } else {
            MSG_START
        };
        cs.has_started = true;
        let _ = midi.send_message(&[msg]);
        let _ = midi.send_message(&[MSG_CLOCK]);

        if let Some(mut a) = activity.try_lock() {
            a.clock_pulses += 1;
            a.clock_last_pulse_at = Some(Instant::now());
            a.clock_last_start_at = Some(Instant::now());
            a.clock_running = true;
            a.clock_waiting_for_phrase = false;
            a.clock_wait_beats_seen = 0;
            a.clock_phrase_beat = cs.beat_count;
            a.clock_pulse_index = cs.pulse_index;
            a.clock_timing_delta_ms = beat_timing_delta_ms(cs, received_at);
        }
    }
}

fn emit_overdue_clock_pulses(cs: &mut ClockState, midi: &Arc<dyn MidiTransport>) -> u64 {
    if cs.interval_ns == 0 {
        return 0;
    }

    let now = Instant::now();
    let elapsed = now.duration_since(cs.last_pulse).as_nanos() as u64;
    if elapsed < cs.interval_ns {
        return 0;
    }

    let intervals_elapsed = (elapsed / cs.interval_ns).max(1);
    cs.last_pulse += Duration::from_nanos(intervals_elapsed.saturating_mul(cs.interval_ns));
    cs.pulse_index = (cs.pulse_index + intervals_elapsed) % PPQ;
    for _ in 0..intervals_elapsed {
        let _ = midi.send_message(&[MSG_CLOCK]);
    }

    if intervals_elapsed > 1 {
        tracing::trace!(
            target: "midi.clock",
            intervals_elapsed,
            elapsed_ns = elapsed,
            interval_ns = cs.interval_ns,
            "Clock late wake: emitted catch-up pulses for overdue boundaries"
        );
    }

    intervals_elapsed
}

// ── Main clock task ───────────────────────────────────────────────────────────

pub async fn run(
    midi: Arc<dyn MidiTransport>,
    state: SharedState,
    mut beat_rx: broadcast::Receiver<BeatEvent>,
    cfg: SharedConfig,
    activity: Arc<Mutex<MidiActivity>>,
    mut cfg_change_rx: watch::Receiver<()>,
    mut timing_rx: watch::Receiver<()>,
) {
    let mut cs = ClockState::new();
    let mut clock_enabled = cfg.read().midi.clock_enabled;
    let mut clock_loop_enabled = cfg.read().midi.clock_loop_enabled;
    let mut cached_bpm = 0.0;
    let mut cached_is_playing = false;
    let mut last_master_key = {
        let st = state.read();
        (st.master.source.clone(), st.master.device_number)
    };

    if let Some(st) = state.try_read() {
        cached_bpm = st.master.bpm;
        cached_is_playing = st.master.is_playing;
    }

    tracing::info!("MIDI clock task started");
    if !clock_enabled {
        tracing::info!("MIDI clock disabled in config");
    } else if !clock_loop_enabled {
        tracing::info!("MIDI clock loop disabled in config");
        cs.transition_to_idle();
    } else {
        cs.arm_wait_for_phrase_start();
    }

    loop {
        let current_enabled = cfg.read().midi.clock_enabled;
        if current_enabled != clock_enabled {
            clock_enabled = current_enabled;
            if !clock_enabled {
                if cs.running {
                    cs.transition_to_idle();
                    cs.has_started = false;
                    let _ = midi.send_message(&[MSG_STOP]);
                    tracing::info!("MIDI clock disabled at runtime");
                }
            } else {
                tracing::info!("MIDI clock enabled at runtime");
                cs.arm_wait_for_phrase_start();
            }
        }

        let current_loop_enabled = cfg.read().midi.clock_loop_enabled;
        if current_loop_enabled != clock_loop_enabled {
            clock_loop_enabled = current_loop_enabled;
            if !clock_loop_enabled {
                tracing::info!("MIDI clock loop disabled at runtime");
                cs.transition_to_idle();
                cs.last_timing_received_at = None;
            } else {
                tracing::info!("MIDI clock loop enabled at runtime");
                if clock_enabled {
                    cs.arm_wait_for_phrase_start();
                }
            }
        }

        let current_master_key = {
            let st = state.read();
            (st.master.source.clone(), st.master.device_number)
        };

        if clock_enabled && current_master_key != last_master_key {
            tracing::info!(
                prev_source = ?last_master_key.0,
                prev_device = last_master_key.1,
                new_source = ?current_master_key.0,
                new_device = current_master_key.1,
                "MIDI clock re-arming on master change"
            );
            if clock_loop_enabled {
                handle_master_change(&mut cs, &midi, &activity);
            } else {
                handle_master_change_without_phrase_wait(&mut cs, &midi, &activity);
            }
            last_master_key = current_master_key;
        } else {
            last_master_key = current_master_key;
        }

        // Wake immediately on config changes (e.g. latency compensation edits).
        // We do not need the payload, only the notification.
        if cfg_change_rx.has_changed().unwrap_or(false) {
            let _ = cfg_change_rx.borrow_and_update();
        }

        apply_pending_phase_slew(&mut cs, Instant::now());

        // ── Compute next pulse deadline ──────────────────────────────────────
        let latency_offset_ms = cfg.read().midi.latency_compensation_ms as f64;
        let effective_offset_ms = latency_offset_ms + cs.auto_offset_ms;
        let next_pulse_at = if clock_enabled && cs.running && cs.interval_ns > 0 {
            let base = cs.last_pulse + Duration::from_nanos(cs.interval_ns);
            if effective_offset_ms > 0.0 {
                base + Duration::from_secs_f64(effective_offset_ms / 1000.0)
            } else if effective_offset_ms < 0.0 {
                base.checked_sub(Duration::from_secs_f64((-effective_offset_ms) / 1000.0))
                    .unwrap_or(base)
            } else {
                base
            }
        } else {
            // Not running: wake periodically to check state changes.
            Instant::now() + Duration::from_millis(10)
        };
        let sleep_dur = next_pulse_at.saturating_duration_since(Instant::now());
        let deadline = tokio::time::Instant::now() + sleep_dur;

        // ── Wait for next pulse deadline OR a beat event ─────────────────────
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {}
            changed = cfg_change_rx.changed() => {
                if changed.is_err() {
                    // Sender dropped; keep running.
                }
            }
            changed = timing_rx.changed() => {
                if changed.is_ok() && clock_loop_enabled {
                    let cfg_r = cfg.read();
                    let stable_bars = cfg_r.midi.phrase_lock_stable_beats;
                    let smoothing_ms = cfg_r.midi.smoothing_ms;
                    handle_timing_snapshot(
                        &mut cs,
                        &state,
                        &midi,
                        &activity,
                        stable_bars,
                        smoothing_ms,
                        Instant::now(),
                    );
                }
            }
            evt = beat_rx.recv() => {
                match evt {
                    Ok(evt) => {
                        if clock_enabled && !clock_loop_enabled {
                            let cfg_r = cfg.read();
                            let stable_bars = cfg_r.midi.phrase_lock_stable_beats;
                            let smoothing_ms = cfg_r.midi.smoothing_ms;
                            handle_prolink_beat_loop_disabled(
                                &mut cs,
                                &state,
                                &midi,
                                &activity,
                                stable_bars,
                                smoothing_ms,
                                evt,
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        }

        // ── Read master state ────────────────────────────────────────────────
        if let Some(st) = state.try_read() {
            cached_bpm = st.master.bpm;
            cached_is_playing = st.master.is_playing;
        }
        let (bpm, is_playing) = (cached_bpm, cached_is_playing);

        // ── Start / Stop / Continue messages ─────────────────────────────────
        if clock_loop_enabled {
            if clock_enabled && is_playing && !cs.running && !cs.waiting_for_downbeat {
                cs.transition_to_running();
                cs.pulse_index = 0;
                cs.last_pulse = Instant::now();
                if bpm > 0.0 {
                    cs.set_bpm(bpm);
                }
                let msg = if cs.has_started {
                    MSG_CONTINUE
                } else {
                    MSG_START
                };
                cs.has_started = true;
                // Send Start/Continue followed by the first clock pulse
                // immediately (MIDI spec: clock should follow Start without delay).
                let _ = midi.send_message(&[msg]);
                let _ = midi.send_message(&[MSG_CLOCK]);
                {
                    if let Some(mut a) = activity.try_lock() {
                        a.clock_pulses += 1;
                        a.clock_last_pulse_at = Some(Instant::now());
                        a.clock_running = cs.running;
                        a.clock_waiting_for_phrase = cs.waiting_for_downbeat;
                        a.clock_wait_beats_seen = cs.wait_beats_seen;
                        a.clock_phrase_beat = cs.beat_count;
                        a.clock_pulse_index = cs.pulse_index;
                        if msg == MSG_START {
                            a.clock_last_start_at = Some(Instant::now());
                        }
                    }
                }
                tracing::debug!(
                    msg = if msg == MSG_START {
                        "Start"
                    } else {
                        "Continue"
                    },
                    "MIDI transport sent"
                );
            } else if (!clock_enabled || !is_playing) && cs.running {
                cs.transition_to_idle();
                let _ = midi.send_message(&[MSG_STOP]);
                tracing::debug!("MIDI Stop sent");
            }
        } else if !clock_enabled && cs.running {
            cs.transition_to_idle();
            let _ = midi.send_message(&[MSG_STOP]);
            tracing::debug!("MIDI Stop sent");
        }

        // ── Emit clock pulse if deadline reached ─────────────────────────────
        let pulse_bpm = if clock_loop_enabled {
            if bpm > 0.0 {
                bpm
            } else {
                cs.last_bpm
            }
        } else {
            cs.last_bpm
        };

        if clock_enabled && cs.running && pulse_bpm > 0.0 {
            cs.set_bpm(pulse_bpm);
            let intervals_elapsed = emit_overdue_clock_pulses(&mut cs, &midi);
            if intervals_elapsed > 0 {
                if let Some(mut a) = activity.try_lock() {
                    a.clock_pulses += intervals_elapsed;
                    a.clock_last_pulse_at = Some(Instant::now());
                    a.clock_running = cs.running;
                    a.clock_waiting_for_phrase = cs.waiting_for_downbeat;
                    a.clock_wait_beats_seen = cs.wait_beats_seen;
                    a.clock_phrase_beat = cs.beat_count;
                    a.clock_pulse_index = cs.pulse_index;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::test_utils::MockMidiTransport;
    use crate::state::timing::{MeasurementKind, PlayingState, TimingMeasurement, TimingSource};
    use crate::state::BeatSource;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_test_state(device_number: u8, source: BeatSource) -> SharedState {
        let mut state = crate::state::DjState::new(30);
        state.master.device_number = device_number;
        state.master.source = Some(source);
        state.master.bpm = 120.0;
        state.master.is_playing = true;
        state.master.phrase_16_beat = 1;
        state.master.beat_in_bar = 1;
        Arc::new(parking_lot::RwLock::new(state))
    }

    fn make_beat(device: u8, beat_in_bar: u8) -> BeatEvent {
        make_beat_at(device, beat_in_bar, Instant::now())
    }

    fn make_beat_at(device: u8, beat_in_bar: u8, received_at: Instant) -> BeatEvent {
        BeatEvent::Beat {
            packet: crate::prolink::packets::BeatPacket {
                device_number: device,
                next_beat_ms: 500,
                second_beat_ms: 1000,
                next_bar_ms: 2000,
                pitch_raw: 0x00100000,
                bpm_raw: 12000,
                beat_in_bar,
                track_bpm: Some(120.0),
                effective_bpm: 120.0,
                pitch_pct: 0.0,
            },
            received_at,
        }
    }

    fn make_timing_measurement(device: u8, beat_in_bar: u8) -> TimingMeasurement {
        make_timing_measurement_at(device, beat_in_bar, Instant::now())
    }

    fn make_timing_measurement_at(
        device: u8,
        beat_in_bar: u8,
        received_at: Instant,
    ) -> TimingMeasurement {
        TimingMeasurement {
            received_at,
            source: TimingSource::ProLink,
            kind: MeasurementKind::ProLinkBeatPacket,
            device_number: Some(device),
            playing: PlayingState::Playing,
            bpm: 120.0,
            effective_bpm: 120.0,
            beat_phase: Some(0.5),
            bar_phase: Some(((beat_in_bar.saturating_sub(1)) as f64 + 0.5) / 4.0),
            beat_in_bar: Some(beat_in_bar),
            playhead_ms: None,
        }
    }

    #[test]
    fn test_beat_from_master_triggers_start_on_beat_1() {
        let mut cs = ClockState::new();
        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        handle_beat_event(&mut cs, &state, &midi, &activity, 1, 0, make_beat(1, 1));

        assert!(cs.running);
        assert!(cs.has_started);
    }

    #[test]
    fn test_beat_from_non_master_does_not_start() {
        let mut cs = ClockState::new();
        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        handle_beat_event(&mut cs, &state, &midi, &activity, 1, 0, make_beat(2, 1));

        assert!(!cs.running);
    }

    #[test]
    fn test_no_master_accepts_any_beat() {
        let mut cs = ClockState::new();
        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(0, BeatSource::ProLink);

        handle_beat_event(&mut cs, &state, &midi, &activity, 1, 0, make_beat(5, 1));

        assert!(cs.running);
    }

    #[test]
    fn test_beat_2_does_not_start() {
        let mut cs = ClockState::new();
        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));

        // Create state with phrase at beat 2 (not start of phrase)
        let mut state = crate::state::DjState::new(30);
        state.master.device_number = 1;
        state.master.source = Some(BeatSource::ProLink);
        state.master.bpm = 120.0;
        state.master.is_playing = true;
        state.master.phrase_16_beat = 2; // Not start of 16-beat phrase
        state.master.beat_in_bar = 2;
        let state = Arc::new(parking_lot::RwLock::new(state));

        handle_beat_event(&mut cs, &state, &midi, &activity, 1, 0, make_beat(1, 2));

        assert!(!cs.running);
        assert!(cs.waiting_for_downbeat);
    }

    #[test]
    fn test_link_beat_works_when_source_is_link() {
        let mut cs = ClockState::new();
        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(0, BeatSource::AbletonLink);

        let evt = BeatEvent::LinkBeat {
            bpm: 120.0,
            beat_in_bar: 1,
            bar_phase: 0.0,
            beat_phase: 0.0,
            received_at: Instant::now(),
        };
        handle_beat_event(&mut cs, &state, &midi, &activity, 1, 0, evt);

        assert!(cs.running);
    }

    #[test]
    fn test_link_beat_ignored_when_source_is_prolink() {
        let mut cs = ClockState::new();
        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(0, BeatSource::ProLink);

        let evt = BeatEvent::LinkBeat {
            bpm: 120.0,
            beat_in_bar: 1,
            bar_phase: 0.0,
            beat_phase: 0.0,
            received_at: Instant::now(),
        };
        handle_beat_event(&mut cs, &state, &midi, &activity, 1, 0, evt);

        assert!(!cs.running);
    }

    #[test]
    fn test_master_change_rearms_phrase_lock() {
        let mut cs = ClockState::new();
        cs.running = true;
        cs.waiting_for_downbeat = false;
        cs.beat_count = 9;
        cs.wait_beats_seen = 3;
        cs.pulse_index = 11;

        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));

        handle_master_change(&mut cs, &midi, &activity);

        assert!(!cs.running);
        assert!(cs.waiting_for_downbeat);
        assert_eq!(cs.beat_count, 0);
        assert_eq!(cs.wait_beats_seen, 0);
        assert_eq!(activity.lock().clock_phrase_beat, 0);
        assert!(activity.lock().clock_waiting_for_phrase);
    }

    #[test]
    fn test_emit_overdue_clock_pulses_sends_each_elapsed_interval() {
        let mut cs = ClockState::new();
        cs.running = true;
        cs.pulse_index = 22;
        let overdue = Duration::from_nanos(cs.interval_ns.saturating_mul(3).saturating_add(1));
        cs.last_pulse = Instant::now()
            .checked_sub(overdue)
            .unwrap_or(Instant::now());

        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();

        let sent = emit_overdue_clock_pulses(&mut cs, &transport);

        assert_eq!(sent, 3);
        assert_eq!(cs.pulse_index, 1);
        let msgs = midi.get_messages();
        assert_eq!(msgs.len(), 3);
        assert!(msgs.iter().all(|m| m.as_slice() == [MSG_CLOCK]));
    }

    #[test]
    fn waiting_start_emits_start_followed_by_first_clock_pulse() {
        let mut cs = ClockState::new();
        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        handle_beat_event(
            &mut cs,
            &state,
            &transport,
            &activity,
            1,
            0,
            make_beat(1, 1),
        );

        let msgs = midi.get_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], vec![MSG_START]);
        assert_eq!(msgs[1], vec![MSG_CLOCK]);
    }

    #[test]
    fn loop_disabled_running_does_not_restart_transport_each_beat() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.has_started = true;
        cs.beat_count = 3;
        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        handle_prolink_beat_loop_disabled(
            &mut cs,
            &state,
            &transport,
            &activity,
            4,
            0,
            make_beat_at(1, 4, Instant::now()),
        );

        let msgs = midi.get_messages();
        assert!(msgs.is_empty());
        assert!(cs.running);
        assert_eq!(cs.beat_count, 4);
    }

    #[test]
    fn timing_snapshot_start_emits_start_then_clock() {
        let mut cs = ClockState::new();
        cs.arm_wait_for_phrase_start();

        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        let measurement = make_timing_measurement(1, 1);
        state.write().timing.observe(measurement);
        handle_timing_snapshot(&mut cs, &state, &transport, &activity, 1, 0, Instant::now());

        let msgs = midi.get_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], vec![MSG_START]);
        assert_eq!(msgs[1], vec![MSG_CLOCK]);
    }

    #[test]
    fn timing_snapshot_realigns_only_at_configured_bar_interval() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.pulse_index = 9;
        cs.last_pulse = Instant::now()
            .checked_sub(Duration::from_millis(20))
            .unwrap_or(Instant::now());

        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        for _bar in 0..3 {
            for beat_in_bar in 1..=4 {
                let measurement = make_timing_measurement(1, beat_in_bar);
                state.write().timing.observe(measurement.clone());
                handle_timing_snapshot(&mut cs, &state, &midi, &activity, 4, 0, Instant::now());
                assert_eq!(cs.pulse_index, 9);
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        let measurement = make_timing_measurement(1, 1);
        state.write().timing.observe(measurement.clone());
        handle_timing_snapshot(&mut cs, &state, &midi, &activity, 4, 0, Instant::now());
        assert_eq!(cs.pulse_index, 0);
    }

    #[test]
    fn timing_snapshot_downbeat_does_not_bypass_interval() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.pulse_index = 11;
        cs.last_pulse = Instant::now()
            .checked_sub(Duration::from_millis(20))
            .unwrap_or(Instant::now());

        let midi: Arc<dyn MidiTransport> = Arc::new(MockMidiTransport::new());
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        let measurement = make_timing_measurement(1, 1);
        state.write().timing.observe(measurement.clone());
        handle_timing_snapshot(&mut cs, &state, &midi, &activity, 8, 0, Instant::now());

        assert_eq!(cs.pulse_index, 11);
    }

    #[test]
    fn timing_snapshot_link_downbeat_triggers_bar_resync() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.has_started = true;
        cs.pulse_index = 11;
        cs.last_timing_beat_in_bar = Some(4);
        let beat_at = Instant::now();
        cs.last_pulse = beat_at
            .checked_sub(Duration::from_millis(40))
            .unwrap_or(beat_at);

        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(0, BeatSource::AbletonLink);

        let measurement = TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, beat_at);
        state.write().timing.observe(measurement);
        handle_timing_snapshot(&mut cs, &state, &transport, &activity, 1, 0, beat_at);

        let msgs = midi.get_messages();
        assert_eq!(msgs, vec![vec![MSG_STOP], vec![MSG_START], vec![MSG_CLOCK]]);
        assert_eq!(cs.pulse_index, 0);
        assert!(cs.running);
    }

    #[test]
    fn loop_disabled_resync_hard_restarts_transport_when_smoothing_is_zero() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.has_started = true;
        cs.pulse_index = 7;

        let beat_at = Instant::now();
        cs.last_pulse = beat_at
            .checked_sub(Duration::from_millis(40))
            .unwrap_or(beat_at);

        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        handle_prolink_beat_loop_disabled(
            &mut cs,
            &state,
            &transport,
            &activity,
            1,
            0,
            make_beat_at(1, 1, beat_at),
        );

        let msgs = midi.get_messages();
        assert_eq!(msgs, vec![vec![MSG_STOP], vec![MSG_START], vec![MSG_CLOCK]]);
        assert!(cs.running);
        assert_eq!(cs.pulse_index, 0);
        assert!(cs.phase_slew.is_none());
        assert_eq!(cs.last_pulse, beat_at);
    }

    #[test]
    fn loop_disabled_resync_smoothed_slews_without_transport_restart() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.has_started = true;
        cs.pulse_index = 5;

        let beat_at = Instant::now();
        cs.last_pulse = beat_at
            .checked_sub(Duration::from_millis(40))
            .unwrap_or(beat_at);
        let pre_slew_last_pulse = cs.last_pulse;

        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        handle_prolink_beat_loop_disabled(
            &mut cs,
            &state,
            &transport,
            &activity,
            1,
            200,
            make_beat_at(1, 1, beat_at),
        );

        assert!(midi.get_messages().is_empty());
        assert!(cs.phase_slew.is_some());

        apply_pending_phase_slew(&mut cs, beat_at + Duration::from_millis(100));
        assert!(cs.last_pulse > pre_slew_last_pulse);

        apply_pending_phase_slew(&mut cs, beat_at + Duration::from_millis(200));
        assert!(cs.phase_slew.is_none());
        assert_eq!(cs.pulse_index, 0);
        assert_eq!(cs.last_pulse, beat_at);
    }

    #[test]
    fn auto_offset_filter_clamp_and_deadband() {
        let mut cs = ClockState::new();

        update_auto_offset_ms(&mut cs, 0.6);
        assert_eq!(cs.auto_offset_ms, 0.0);

        update_auto_offset_ms(&mut cs, -100.0);
        assert!((cs.auto_offset_ms - 4.0).abs() < 1e-9);

        update_auto_offset_ms(&mut cs, -100.0);
        assert!((cs.auto_offset_ms - 7.2).abs() < 1e-9);

        update_auto_offset_ms(&mut cs, 100.0);
        assert!((cs.auto_offset_ms - 1.76).abs() < 1e-9);

        assert!(cs.auto_offset_ms.abs() <= AUTO_OFFSET_CLAMP_MS);

        update_auto_offset_ms(&mut cs, 0.5);
        assert!((cs.auto_offset_ms - 1.76).abs() < 1e-9);
    }

    #[test]
    fn stale_or_nonmaster_measurements_do_not_update_auto_offset() {
        let mut cs = ClockState::new();
        cs.transition_to_running();
        cs.last_pulse = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap_or(Instant::now());
        cs.last_timing_beat_in_bar = Some(4);

        let midi = Arc::new(MockMidiTransport::new());
        let transport: Arc<dyn MidiTransport> = midi.clone();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let state = make_test_state(1, BeatSource::ProLink);

        let now = Instant::now();
        let stale_received_at = now.checked_sub(Duration::from_secs(2)).unwrap_or(now);
        let stale = make_timing_measurement_at(1, 1, stale_received_at);
        state.write().timing.observe(stale);
        handle_timing_snapshot(&mut cs, &state, &transport, &activity, 1, 0, now);
        assert_eq!(cs.auto_offset_ms, 0.0);

        cs.last_timing_beat_in_bar = Some(4);
        let fresh_non_master = make_timing_measurement_at(2, 1, Instant::now());
        state.write().timing.observe(fresh_non_master);
        handle_timing_snapshot(&mut cs, &state, &transport, &activity, 1, 0, Instant::now());
        assert_eq!(cs.auto_offset_ms, 0.0);
    }
}
