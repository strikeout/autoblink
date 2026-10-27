use serde::{Deserialize, Deserializer, Serialize};

/// The number of CCs: BPM coarse, BPM fine, pitch, bar phase, beat phase, playing, master deck,
/// phrase 16, phrase kind, phrase progress and beats left. Their host setting indices are 17 to
/// 27, in this order.
pub const CC_COUNT: usize = 11;

const DEFAULT_CCS: [u8; CC_COUNT] = [1, 33, 2, 3, 4, 5, 6, 7, 20, 21, 22];

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
    pub phrase_change_note: u8,
    pub send_phrase_change: bool,
    /// The first note of the phrase kinds. The note is this value plus the kind code (0 to 11).
    pub phrase_base_note: u8,
    pub send_phrase_kind: bool,
    /// Sends each note off as a note on with velocity 0, not as 0x80. Some DMX controllers
    /// ignore the 0x80 note off. Host index 33.
    pub off_vel0: bool,
    pub cc_enabled: bool,
    /// 0-based. The page shows 1 to 16.
    pub cc_channel: u8,
    /// A stored configuration with fewer CCs gets the default for each missing CC.
    #[serde(deserialize_with = "ccs_or_defaults")]
    pub ccs: [Cc; CC_COUNT],
}

fn default_ccs() -> [Cc; CC_COUNT] {
    DEFAULT_CCS.map(|num| Cc { num, on: true })
}

fn ccs_or_defaults<'de, D: Deserializer<'de>>(d: D) -> Result<[Cc; CC_COUNT], D::Error> {
    let stored = Vec::<Cc>::deserialize(d)?;
    let mut ccs = default_ccs();
    for (cc, s) in ccs.iter_mut().zip(stored) {
        *cc = s;
    }
    Ok(ccs)
}

impl Default for MidiConfig {
    fn default() -> Self {
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
            phrase_change_note: 38,
            send_phrase_change: true,
            phrase_base_note: 40,
            send_phrase_kind: true,
            off_vel0: true,
            cc_enabled: true,
            cc_channel: 0,
            ccs: default_ccs(),
        }
    }
}
