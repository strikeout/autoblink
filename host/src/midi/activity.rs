//! MIDI output activity counters, which the UI shows.

use std::time::Instant;

// ── MIDI activity counters ───────────────────────────────────────────────────

/// Counters the TUI displays in the output panel.  Updated atomically from
/// MIDI tasks, read by the renderer.
#[derive(Debug, Clone, Default)]
pub struct MidiActivity {
    pub clock_pulses: u64,
    pub mtc_quarter_frames: u64,
    pub mtc_full_frames: u64,
    pub notes_sent: u64,
    pub cc_sent: u64,
    /// Last note-on fired: (note_number, when).
    pub last_note: Option<(u8, Instant)>,
    /// Last CC sent: (cc_number, value, when).
    pub last_cc: Option<(u8, u8, Instant)>,

    pub clock_running: bool,
    pub clock_waiting_for_phrase: bool,
    pub clock_wait_beats_seen: u8,
    pub clock_phrase_beat: u8,
    pub clock_pulse_index: u64,
    pub clock_last_start_at: Option<Instant>,
    pub clock_last_pulse_at: Option<Instant>,

    /// Latest beat timing delta in milliseconds.
    pub clock_timing_delta_ms: Option<f64>,
}
