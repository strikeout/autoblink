//! Track metadata types.

/// Mood classification rekordbox assigns to a track's phrase analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackMood {
    High,
    Mid,
    Low,
}

/// Kind of phrase within a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhraseKind {
    Intro,
    Verse1,
    Verse2,
    Verse3,
    Verse4,
    Verse5,
    Verse6,
    Bridge,
    Chorus,
    Up,
    Down,
    Outro,
    Unknown(u16),
}

impl PhraseKind {
    /// MIDI code of the phrase kind: Intro 0, Verse 1–6, Bridge 7, Chorus 8, Up 9, Down 10, Outro 11.
    /// Returns `None` for an unknown kind.
    pub fn code(self) -> Option<u8> {
        match self {
            PhraseKind::Intro => Some(0),
            PhraseKind::Verse1 => Some(1),
            PhraseKind::Verse2 => Some(2),
            PhraseKind::Verse3 => Some(3),
            PhraseKind::Verse4 => Some(4),
            PhraseKind::Verse5 => Some(5),
            PhraseKind::Verse6 => Some(6),
            PhraseKind::Bridge => Some(7),
            PhraseKind::Chorus => Some(8),
            PhraseKind::Up => Some(9),
            PhraseKind::Down => Some(10),
            PhraseKind::Outro => Some(11),
            PhraseKind::Unknown(_) => None,
        }
    }
}

impl std::fmt::Display for PhraseKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhraseKind::Intro => write!(f, "INTRO"),
            PhraseKind::Verse1 => write!(f, "VERSE 1"),
            PhraseKind::Verse2 => write!(f, "VERSE 2"),
            PhraseKind::Verse3 => write!(f, "VERSE 3"),
            PhraseKind::Verse4 => write!(f, "VERSE 4"),
            PhraseKind::Verse5 => write!(f, "VERSE 5"),
            PhraseKind::Verse6 => write!(f, "VERSE 6"),
            PhraseKind::Bridge => write!(f, "BRIDGE"),
            PhraseKind::Chorus => write!(f, "CHORUS"),
            PhraseKind::Up => write!(f, "UP"),
            PhraseKind::Down => write!(f, "DOWN"),
            PhraseKind::Outro => write!(f, "OUTRO"),
            PhraseKind::Unknown(id) => write!(f, "?{id}"),
        }
    }
}

/// A single phrase entry from the song structure analysis.
#[derive(Debug, Clone)]
pub struct PhraseEntry {
    /// 1-based index of this phrase.
    pub index: u16,
    /// Beat number at which this phrase starts.
    pub beat: u16,
    /// The kind of phrase.
    pub kind: PhraseKind,
    /// Whether a fill-in is present at the end of the phrase.
    pub has_fill: bool,
    /// Beat number at which fill-in starts (0 if no fill).
    pub fill_beat: u16,
}

/// Complete song structure for a track.
#[derive(Debug, Clone)]
pub struct SongStructure {
    pub mood: TrackMood,
    /// Beat number at which the last phrase ends.
    pub end_beat: u16,
    /// Ordered list of phrases.
    pub phrases: Vec<PhraseEntry>,
}

/// A stored cue point or loop of a track, from the extended cue list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cue {
    /// The hot cue number: 1 is A, 2 is B. 0 is a memory cue.
    pub hot_cue: u8,
    pub start_ms: u32,
    /// The loop end. `None` for a cue that is not a loop.
    pub end_ms: Option<u32>,
}

/// The waveform preview and the stored cues of one track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackOverview {
    pub rekordbox_id: u32,
    /// The PWV4 colour preview: 6 bytes per column, usually 1200 columns.
    pub preview: Vec<u8>,
    pub cues: Vec<Cue>,
}
