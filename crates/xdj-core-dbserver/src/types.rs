//! The track types and the phrase position helpers.
//!
//! The types are copies of `host/src/state/song_structure.rs`.

/// Mood classification rekordbox assigns to the phrase analysis of a track.
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
    /// MIDI code of the phrase kind: Intro 0, Verse 1-6, Bridge 7, Chorus 8, Up 9, Down 10, Outro 11.
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
    /// Beat number at which the fill-in starts (0 if no fill).
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

/// One phrase of a track on the time axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseSpan {
    pub kind: PhraseKind,
    pub start_ms: u32,
    pub end_ms: u32,
    /// The start of the fill-in, when the phrase has one.
    pub fill_ms: Option<u32>,
}

/// The waveform preview, the stored cues and the phrases of one track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackOverview {
    pub rekordbox_id: u32,
    /// The PWV4 colour preview: 6 bytes per column, usually 1200 columns.
    pub preview: Vec<u8>,
    pub cues: Vec<Cue>,
    pub phrases: Vec<PhraseSpan>,
}

/// Track metadata retrieved from the dbserver.
#[derive(Debug, Clone, Default)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub key: String,
    /// Original track BPM (not pitch-adjusted).
    pub bpm: Option<f64>,
    /// The track length in seconds.
    pub duration_s: Option<u32>,
}

/// The current phrase and its position. Equal to `PhraseInfo` of the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhraseInfo {
    pub idx: usize,
    pub kind: PhraseKind,
    /// Position in the phrase, 0.0 to 1.0.
    pub progress: f64,
    /// Beats until the next phrase starts.
    pub beats_left: u32,
}

/// The phrase data that the web status shows for a deck.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhraseStatus {
    pub kind: PhraseKind,
    pub progress: f64,
    /// True when the phrase has a fill-in and the beat count has reached it.
    pub fill: bool,
    pub beats_left: u32,
}

/// Returns the index of the phrase that holds `beat_count`.
///
/// The host sets `current_phrase_idx` in the same way. The beat count saturates at `u16::MAX`.
/// Returns `None` when no phrase starts at or before the beat.
pub fn current_phrase_idx(ss: &SongStructure, beat_count: u32) -> Option<usize> {
    let beat = u16::try_from(beat_count).unwrap_or(u16::MAX);
    ss.phrases
        .iter()
        .enumerate()
        .rev()
        .find(|(_, p)| p.beat <= beat)
        .map(|(i, _)| i)
}

/// Returns the phrase at `idx` and its position.
///
/// Returns `None` when `idx` is out of range or the beat count is `u32::MAX` (no beat known).
pub fn phrase_info(ss: &SongStructure, idx: usize, beat_count: u32) -> Option<PhraseInfo> {
    let phrase = ss.phrases.get(idx)?;
    if beat_count == u32::MAX {
        return None;
    }
    let start = u32::from(phrase.beat);
    let end = u32::from(ss.phrases.get(idx + 1).map_or(ss.end_beat, |p| p.beat));
    let len = end.saturating_sub(start).max(1);
    let pos = beat_count.saturating_sub(start);
    Some(PhraseInfo {
        idx,
        kind: phrase.kind,
        progress: (f64::from(pos) / f64::from(len)).clamp(0.0, 1.0),
        beats_left: end.saturating_sub(beat_count),
    })
}

/// Returns the phrase data of the web status for `beat_count`, or `None` when there is none.
pub fn phrase_status(ss: &SongStructure, beat_count: u32) -> Option<PhraseStatus> {
    let idx = current_phrase_idx(ss, beat_count)?;
    let info = phrase_info(ss, idx, beat_count)?;
    let phrase = ss.phrases.get(info.idx)?;
    Some(PhraseStatus {
        kind: info.kind,
        progress: info.progress,
        fill: phrase.has_fill && beat_count >= u32::from(phrase.fill_beat),
        beats_left: info.beats_left,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structure() -> SongStructure {
        let entry = |index, beat, kind, fill_beat| PhraseEntry {
            index,
            beat,
            kind,
            has_fill: fill_beat > 0,
            fill_beat,
        };
        SongStructure {
            mood: TrackMood::Mid,
            end_beat: 65,
            phrases: vec![
                entry(1, 1, PhraseKind::Intro, 0),
                entry(2, 33, PhraseKind::Chorus, 61),
            ],
        }
    }

    #[test]
    fn phrase_index_follows_the_beat() {
        let ss = structure();
        assert_eq!(current_phrase_idx(&ss, 0), None);
        assert_eq!(current_phrase_idx(&ss, 1), Some(0));
        assert_eq!(current_phrase_idx(&ss, 32), Some(0));
        assert_eq!(current_phrase_idx(&ss, 33), Some(1));
        assert_eq!(current_phrase_idx(&ss, u32::MAX), Some(1));
    }

    #[test]
    fn phrase_info_has_progress_and_beats_left() {
        let ss = structure();
        let info = phrase_info(&ss, 0, 17).unwrap();
        assert_eq!((info.kind, info.beats_left), (PhraseKind::Intro, 16));
        assert!((info.progress - 0.5).abs() < 1e-9);
        assert!(phrase_info(&ss, 2, 17).is_none());
        assert!(phrase_info(&ss, 0, u32::MAX).is_none());
    }

    #[test]
    fn phrase_status_sets_the_fill_flag_at_the_fill_beat() {
        let ss = structure();
        assert!(!phrase_status(&ss, 60).unwrap().fill);
        let s = phrase_status(&ss, 61).unwrap();
        assert!(s.fill);
        assert_eq!((s.kind, s.beats_left), (PhraseKind::Chorus, 4));
        assert!(!phrase_status(&ss, 10).unwrap().fill);
        assert!(phrase_status(&ss, 0).is_none());
    }

    #[test]
    fn phrase_kind_codes_and_names() {
        assert_eq!(PhraseKind::Outro.code(), Some(11));
        assert_eq!(PhraseKind::Unknown(9).code(), None);
        assert_eq!(PhraseKind::Verse3.to_string(), "VERSE 3");
        assert_eq!(PhraseKind::Unknown(9).to_string(), "?9");
    }
}
