use serde::{Deserialize, Serialize};

/// The number of CCs: BPM coarse, BPM fine, pitch, bar phase, beat phase, playing, master deck
/// and phrase 16. Their host setting indices are 17 to 24, in this order.
pub const CC_COUNT: usize = 8;

/// One CC: its controller number, and the switch that the page shows next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cc {
    pub num: u8,
    pub on: bool,
}

/// The MIDI settings. The defaults are the host defaults (`host/src/config.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    pub clock_enabled: bool,
    /// When true, the clock stops when the master stops, and starts again on a downbeat.
    pub clock_loop_sync: bool,
    pub smoothing_ms: u64,
    pub latency_ms: i64,
    /// The number of bars between two phase corrections of the clock.
    pub phrase_lock_bars: u8,
    pub notes_enabled: bool,
    /// 0-based. The page shows 1 to 16.
    pub note_channel: u8,
    pub beat_note: u8,
    pub send_beat: bool,
    pub downbeat_note: u8,
    pub send_downbeat: bool,
    pub cc_enabled: bool,
    /// 0-based. The page shows 1 to 16.
    pub cc_channel: u8,
    pub ccs: [Cc; CC_COUNT],
}

impl Default for MidiConfig {
    fn default() -> Self {
        let cc = |num| Cc { num, on: true };
        Self {
            clock_enabled: true,
            clock_loop_sync: true,
            smoothing_ms: 30,
            latency_ms: 0,
            phrase_lock_bars: 4,
            notes_enabled: true,
            note_channel: 9,
            beat_note: 36,
            send_beat: true,
            downbeat_note: 37,
            send_downbeat: true,
            cc_enabled: true,
            cc_channel: 0,
            ccs: [cc(1), cc(33), cc(2), cc(3), cc(4), cc(5), cc(6), cc(7)],
        }
    }
}
