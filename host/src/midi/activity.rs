//! MIDI output activity counters, which the UI shows.

use std::collections::VecDeque;
use std::time::Instant;

/// The number of notes, CCs and OSC messages that the history of the UI keeps.
pub const HISTORY: usize = 5;

/// Puts `v` at the front of `q` and keeps the `HISTORY` newest entries.
pub fn remember<T>(q: &mut VecDeque<T>, v: T) {
    q.push_front(v);
    q.truncate(HISTORY);
}

// ── MIDI activity counters ───────────────────────────────────────────────────

/// Counters the web UI displays in the output panel.  Updated atomically from
/// MIDI tasks, read by the renderer.
#[derive(Debug, Clone, Default)]
pub struct MidiActivity {
    pub clock_pulses: u64,
    pub mtc_quarter_frames: u64,
    pub mtc_full_frames: u64,
    pub notes_sent: u64,
    pub cc_sent: u64,
    /// The last note-ons, newest first: (note_number, when).
    pub recent_notes: VecDeque<(u8, Instant)>,
    /// The last CCs, newest first: (cc_number, value, when).
    pub recent_ccs: VecDeque<(u8, u8, Instant)>,
    /// The OSC messages sent, counted once per target.
    pub osc_sent: u64,
    /// The OSC sends that failed, and the targets that are not an IP address with a port.
    pub osc_errors: u64,
    /// The last OSC messages, newest first, as "address args", and when they were sent.
    pub recent_osc: VecDeque<(String, Instant)>,

    pub clock_running: bool,
    pub clock_waiting_for_phrase: bool,
    pub clock_wait_beats_seen: u8,
    pub clock_phrase_beat: u8,
    pub clock_pulse_index: u64,
    pub clock_last_start_at: Option<Instant>,
    pub clock_last_pulse_at: Option<Instant>,

    /// Latest beat timing delta in milliseconds.
    pub clock_timing_delta_ms: Option<f64>,

    /// The current DMX scene, for the page.
    pub scene: autoblink_core_midi::scenes::SceneStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_keeps_the_newest_first() {
        let mut q = VecDeque::new();
        for i in 0..6 {
            remember(&mut q, i);
        }
        assert_eq!(q, [5, 4, 3, 2, 1]);
    }
}
