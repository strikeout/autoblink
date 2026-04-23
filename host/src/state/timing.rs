use std::time::{Duration, Instant};

use crate::prolink::packets::{AbsPositionPacket, BeatPacket};
use crate::prolink::BEAT_NONE;

const PHASE_EPSILON: f64 = 1e-6;

fn stable_unit_phase(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }

    let snapped = if v.abs() <= PHASE_EPSILON {
        0.0
    } else if (1.0 - v).abs() <= PHASE_EPSILON {
        1.0
    } else {
        v
    };

    snapped.clamp(0.0, 1.0)
}

/// Minimal log throttler (no deps): allow at most one log per `interval`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogThrottle {
    last: Option<Instant>,
}

impl LogThrottle {
    pub fn should_log(&mut self, now: Instant, interval: Duration) -> bool {
        match self.last {
            Some(prev) if now.checked_duration_since(prev).unwrap_or(Duration::ZERO) < interval => {
                false
            }
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingSource {
    ProLink,
    AbletonLink,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementKind {
    ProLinkBeatPacket,
    ProLinkAbsPositionPacket,
    AbletonLink,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayingState {
    Playing,
    Stopped,
    Unknown,
}

/// A normalized timing/transport measurement input that future MIDI Clock and MTC
/// dispatchers can consume.
///
/// Contract:
/// - `received_at` is monotonic (`Instant`) and captured as close to measurement
///   production/receipt as practical.
/// - phase fields are normalized to [0.0, 1.0] where available.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TimingMeasurement {
    pub received_at: Instant,
    pub source: TimingSource,
    pub kind: MeasurementKind,

    /// ProLink packets include a device number; Link does not.
    pub device_number: Option<u8>,

    /// Explicit playing indicator. ProLink beat/position packets do not carry this,
    /// so it may be `Unknown`.
    pub playing: PlayingState,

    /// The tempo value reported by the source (track BPM when known).
    pub bpm: f64,
    /// The effective tempo after pitch is applied.
    pub effective_bpm: f64,

    pub beat_phase: Option<f64>,
    pub bar_phase: Option<f64>,
    pub beat_in_bar: Option<u8>,
    pub playhead_ms: Option<u32>,
}

impl TimingMeasurement {
    pub fn from_prolink_beat(packet: &BeatPacket, received_at: Instant) -> Self {
        let effective_bpm = packet.effective_bpm;
        let bpm = packet.track_bpm.unwrap_or(effective_bpm);

        let beat_phase = if effective_bpm > 0.0 && packet.next_beat_ms != BEAT_NONE {
            let inv_beat_dur = effective_bpm / 60_000.0;
            // Equivalent to (beat_dur_ms - next_beat_ms) / beat_dur_ms, but avoids
            // subtracting similarly-sized values before normalization.
            let raw_phase = 1.0 - ((packet.next_beat_ms as f64) * inv_beat_dur);
            Some(stable_unit_phase(raw_phase))
        } else {
            None
        };

        let bar_phase = match (packet.beat_in_bar, beat_phase) {
            (b, Some(bp)) if b >= 1 => {
                // Normalize within a 4/4 bar.
                Some(stable_unit_phase((((b - 1) as f64) + bp) * 0.25))
            }
            _ => None,
        };

        Self {
            received_at,
            source: TimingSource::ProLink,
            kind: MeasurementKind::ProLinkBeatPacket,
            device_number: Some(packet.device_number),
            playing: PlayingState::Unknown,
            bpm,
            effective_bpm,
            beat_phase,
            bar_phase,
            beat_in_bar: Some(packet.beat_in_bar),
            playhead_ms: None,
        }
    }

    pub fn from_prolink_abs_position(packet: &AbsPositionPacket, received_at: Instant) -> Self {
        let effective_bpm = packet.effective_bpm;
        let bpm = effective_bpm;

        let beat_phase = if effective_bpm > 0.0 {
            let beat_dur_ms = 60_000.0 / effective_bpm;
            let inv_beat_dur = effective_bpm / 60_000.0;
            let within = (packet.playhead_ms as f64).rem_euclid(beat_dur_ms);
            Some(stable_unit_phase(within * inv_beat_dur))
        } else {
            None
        };

        Self {
            received_at,
            source: TimingSource::ProLink,
            kind: MeasurementKind::ProLinkAbsPositionPacket,
            device_number: Some(packet.device_number),
            playing: PlayingState::Unknown,
            bpm,
            effective_bpm,
            beat_phase,
            bar_phase: None,
            beat_in_bar: None,
            playhead_ms: Some(packet.playhead_ms),
        }
    }

    pub fn from_link(
        bpm: f64,
        beat_in_bar: u8,
        bar_phase: f64,
        beat_phase: f64,
        is_playing: bool,
        received_at: Instant,
    ) -> Self {
        Self {
            received_at,
            source: TimingSource::AbletonLink,
            kind: MeasurementKind::AbletonLink,
            device_number: None,
            playing: if is_playing {
                PlayingState::Playing
            } else {
                PlayingState::Stopped
            },
            bpm,
            effective_bpm: bpm,
            beat_phase: Some(stable_unit_phase(beat_phase)),
            bar_phase: Some(stable_unit_phase(bar_phase)),
            beat_in_bar: Some(beat_in_bar),
            playhead_ms: None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TimingModel {
    last: Option<TimingMeasurement>,
    stale_after: Duration,
}

impl Default for TimingModel {
    fn default() -> Self {
        Self::new(Duration::from_millis(750))
    }
}

impl TimingModel {
    pub fn new(stale_after: Duration) -> Self {
        Self {
            last: None,
            stale_after,
        }
    }

    pub fn observe(&mut self, m: TimingMeasurement) {
        self.last = Some(m);
    }

    #[allow(dead_code)]
    pub fn last(&self) -> Option<&TimingMeasurement> {
        self.last.as_ref()
    }

    #[allow(dead_code)]
    pub fn snapshot_at(&self, now: Instant) -> TimingSnapshot {
        let Some(m) = self.last.clone() else {
            return TimingSnapshot::NoMeasurement;
        };

        let age = now
            .checked_duration_since(m.received_at)
            .unwrap_or(Duration::ZERO);

        if age <= self.stale_after {
            TimingSnapshot::Fresh {
                measurement: m,
                age,
            }
        } else {
            TimingSnapshot::Stale {
                measurement: m,
                age,
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum TimingSnapshot {
    NoMeasurement,
    Fresh {
        measurement: TimingMeasurement,
        age: Duration,
    },
    Stale {
        measurement: TimingMeasurement,
        age: Duration,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prolink_beat_and_abs_position_normalize_without_losing_critical_fields() {
        let received_at = Instant::now();

        let bp = BeatPacket {
            device_number: 1,
            next_beat_ms: 250,
            second_beat_ms: 0,
            next_bar_ms: 0,
            pitch_raw: xdj_core_prolink::PITCH_NORMAL,
            bpm_raw: 12800,
            beat_in_bar: 3,
            track_bpm: Some(128.0),
            effective_bpm: 128.0,
            pitch_pct: 0.0,
        };

        let ap = AbsPositionPacket {
            device_number: 1,
            track_length_s: 180,
            playhead_ms: 12_345,
            pitch_raw_signed: 0,
            bpm_x10: 1280,
            effective_bpm: 128.0,
            pitch_pct: 0.0,
        };

        let m_bp = TimingMeasurement::from_prolink_beat(&bp, received_at);
        let m_ap = TimingMeasurement::from_prolink_abs_position(&ap, received_at);

        assert_eq!(m_bp.source, TimingSource::ProLink);
        assert_eq!(m_bp.kind, MeasurementKind::ProLinkBeatPacket);
        assert_eq!(m_bp.device_number, Some(1));
        assert_eq!(m_bp.playing, PlayingState::Unknown);
        assert_eq!(m_bp.bpm, 128.0);
        assert_eq!(m_bp.effective_bpm, 128.0);
        assert_eq!(m_bp.beat_in_bar, Some(3));
        assert!(m_bp.beat_phase.is_some());
        assert!(m_bp.bar_phase.is_some());
        assert_eq!(m_bp.playhead_ms, None);

        assert_eq!(m_ap.source, TimingSource::ProLink);
        assert_eq!(m_ap.kind, MeasurementKind::ProLinkAbsPositionPacket);
        assert_eq!(m_ap.device_number, Some(1));
        assert_eq!(m_ap.playing, PlayingState::Unknown);
        assert_eq!(m_ap.bpm, 128.0);
        assert_eq!(m_ap.effective_bpm, 128.0);
        assert_eq!(m_ap.beat_in_bar, None);
        assert!(m_ap.beat_phase.is_some());
        assert_eq!(m_ap.bar_phase, None);
        assert_eq!(m_ap.playhead_ms, Some(12_345));
    }

    #[test]
    fn timing_model_exposes_explicit_no_measurement_and_stale_snapshots_without_panicking() {
        let model = TimingModel::new(Duration::from_millis(100));
        assert!(matches!(
            model.snapshot_at(Instant::now()),
            TimingSnapshot::NoMeasurement
        ));

        let mut model = model;
        let now = Instant::now();
        let m = TimingMeasurement::from_link(120.0, 1, 0.0, 0.0, true, now);
        model.observe(m.clone());

        match model.snapshot_at(now) {
            TimingSnapshot::Fresh { measurement, age } => {
                assert_eq!(measurement.kind, MeasurementKind::AbletonLink);
                assert_eq!(age, Duration::ZERO);
            }
            other => panic!("expected Fresh, got {other:?}"),
        }

        let later = now + Duration::from_millis(250);
        match model.snapshot_at(later) {
            TimingSnapshot::Stale { measurement, age } => {
                assert_eq!(measurement.received_at, now);
                assert!(age >= Duration::from_millis(250));
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn prolink_beat_with_none_next_beat_keeps_phase_unknown() {
        let received_at = Instant::now();
        let bp = BeatPacket {
            device_number: 1,
            next_beat_ms: BEAT_NONE,
            second_beat_ms: 0,
            next_bar_ms: 0,
            pitch_raw: xdj_core_prolink::PITCH_NORMAL,
            bpm_raw: 12800,
            beat_in_bar: 2,
            track_bpm: Some(128.0),
            effective_bpm: 128.0,
            pitch_pct: 0.0,
        };

        let m = TimingMeasurement::from_prolink_beat(&bp, received_at);
        assert_eq!(m.beat_phase, None);
        assert_eq!(m.bar_phase, None);
    }
}
