use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use crate::prolink::packets::PlayState;

use super::beat_source::BeatSource;
use super::song_structure::{PhraseKind, SongStructure, TrackOverview};

/// The phrase that a deck plays now, with its position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhraseInfo {
    pub idx: usize,
    pub kind: PhraseKind,
    /// Position in the phrase, 0.0 to 1.0.
    pub progress: f64,
    /// Beats until the next phrase starts.
    pub beats_left: u32,
}

/// The position of the EQ knobs and the channel fader of one mixer channel.
///
/// Each value is the 7-bit MIDI value. `None` means that the control did not move since the
/// start. The XDJ-AZ sends a value only when a control moves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MixerChannel {
    pub hi: Option<u8>,
    pub mid: Option<u8>,
    pub low: Option<u8>,
    pub fader: Option<u8>,
}

/// Per-device state tracked for each CDJ/mixer on the network.
#[derive(Debug, Clone)]
pub struct DeviceState {
    pub device_number: u8,
    pub is_master: bool,
    pub is_playing: bool,
    pub is_on_air: bool,
    pub is_sync: bool,
    pub play_state: PlayState,
    pub effective_bpm: f64,
    pub pitch_pct: f64,
    pub beat_in_bar: u8,
    pub beat_count: u32,
    pub playhead_ms: Option<u32>,
    /// The track length. The metadata gives it, or else the absolute position packet.
    pub track_length_ms: Option<u32>,
    /// The active loop as (start, end) in ms.
    pub loop_ms: Option<(u32, u32)>,
    pub bar_phase: f64,
    pub beat_phase: f64,
    pub phrase_16_beat: u8,
    pub last_beat_at: Option<Instant>,
    bpm_history: VecDeque<f64>,
    pub rekordbox_id: u32,
    pub track_slot: u8,
    pub track_type: u8,
    pub track_source_player: u8,
    pub track_title: String,
    pub track_artist: String,
    pub track_key: String,
    pub track_bpm_meta: Option<f64>,
    pub song_structure: Option<SongStructure>,
    /// The reason that the song structure fetch failed for the loaded track.
    pub phrase_error: Option<String>,
    /// The waveform preview and the cues. An `Arc`, because the web status clones each device.
    pub overview: Option<Arc<TrackOverview>>,
    pub current_phrase_idx: Option<usize>,
    pub prev_phrase_idx: Option<usize>,
}

impl DeviceState {
    pub(crate) fn new(device_number: u8) -> Self {
        Self {
            device_number,
            is_master: false,
            is_playing: false,
            is_on_air: false,
            is_sync: false,
            play_state: PlayState::NoTrack,
            effective_bpm: 0.0,
            pitch_pct: 0.0,
            beat_in_bar: 0,
            beat_count: u32::MAX,
            playhead_ms: None,
            track_length_ms: None,
            loop_ms: None,
            bar_phase: 0.0,
            beat_phase: 0.0,
            phrase_16_beat: 0,
            last_beat_at: None,
            bpm_history: VecDeque::with_capacity(8),
            rekordbox_id: 0,
            track_slot: 0,
            track_type: 0,
            track_source_player: 0,
            track_title: String::new(),
            track_artist: String::new(),
            track_key: String::new(),
            track_bpm_meta: None,
            song_structure: None,
            phrase_error: None,
            overview: None,
            current_phrase_idx: None,
            prev_phrase_idx: None,
        }
    }

    /// Returns the current phrase and its position.
    /// Returns `None` when the deck has no song structure, no phrase index or no beat count.
    pub fn phrase_info(&self) -> Option<PhraseInfo> {
        let ss = self.song_structure.as_ref()?;
        let idx = self.current_phrase_idx?;
        let phrase = ss.phrases.get(idx)?;
        if self.beat_count == u32::MAX {
            return None;
        }
        let start = u32::from(phrase.beat);
        let end = u32::from(ss.phrases.get(idx + 1).map_or(ss.end_beat, |p| p.beat));
        let len = end.saturating_sub(start).max(1);
        let pos = self.beat_count.saturating_sub(start);
        Some(PhraseInfo {
            idx,
            kind: phrase.kind,
            progress: (f64::from(pos) / f64::from(len)).clamp(0.0, 1.0),
            beats_left: end.saturating_sub(self.beat_count),
        })
    }

    pub(crate) fn smooth_bpm(&mut self, raw: f64, window: usize) -> f64 {
        if window == 0 || raw <= 0.0 {
            return raw;
        }
        self.bpm_history.push_back(raw);
        while self.bpm_history.len() > window {
            self.bpm_history.pop_front();
        }
        self.bpm_history.iter().sum::<f64>() / self.bpm_history.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PhraseEntry, TrackMood};

    fn phrase(beat: u16, kind: PhraseKind) -> PhraseEntry {
        PhraseEntry {
            index: 1,
            beat,
            kind,
            has_fill: false,
            fill_beat: 0,
        }
    }

    fn deck(beat_count: u32, idx: Option<usize>, end_beat: u16) -> DeviceState {
        let mut d = DeviceState::new(1);
        d.beat_count = beat_count;
        d.current_phrase_idx = idx;
        d.song_structure = Some(SongStructure {
            mood: TrackMood::Mid,
            end_beat,
            phrases: vec![phrase(1, PhraseKind::Intro), phrase(33, PhraseKind::Verse1)],
        });
        d
    }

    #[test]
    fn first_phrase_ends_at_the_next_phrase() {
        let info = deck(17, Some(0), 65).phrase_info().unwrap();
        assert_eq!(info.idx, 0);
        assert_eq!(info.kind, PhraseKind::Intro);
        assert_eq!(info.progress, 0.5);
        assert_eq!(info.beats_left, 16);
    }

    #[test]
    fn last_phrase_ends_at_end_beat() {
        let info = deck(49, Some(1), 65).phrase_info().unwrap();
        assert_eq!(info.kind, PhraseKind::Verse1);
        assert_eq!(info.progress, 0.5);
        assert_eq!(info.beats_left, 16);
    }

    #[test]
    fn end_beat_below_last_start_gives_no_nan() {
        let info = deck(40, Some(1), 10).phrase_info().unwrap();
        assert!(info.progress.is_finite());
        assert_eq!(info.progress, 1.0);
        assert_eq!(info.beats_left, 0);
    }

    #[test]
    fn no_info_without_structure_index_or_beat_count() {
        let mut d = deck(17, Some(0), 65);
        d.song_structure = None;
        assert!(d.phrase_info().is_none());
        assert!(deck(17, None, 65).phrase_info().is_none());
        assert!(deck(u32::MAX, Some(0), 65).phrase_info().is_none());
    }
}

/// The distilled state of the tempo master.
#[derive(Debug, Clone)]
pub struct MasterState {
    pub device_number: u8,
    pub source: Option<BeatSource>,
    pub bpm: f64,
    pub pitch_pct: f64,
    pub beat_in_bar: u8,
    pub bar_phase: f64,
    pub beat_phase: f64,
    pub is_playing: bool,
    pub last_beat_at: Option<Instant>,
    pub is_virtual_master: bool,
    pub phrase_16_beat: u8,
}

impl Default for MasterState {
    fn default() -> Self {
        Self {
            device_number: 0,
            source: None,
            bpm: 0.0,
            pitch_pct: 0.0,
            beat_in_bar: 0,
            bar_phase: 0.0,
            beat_phase: 0.0,
            is_playing: false,
            last_beat_at: None,
            is_virtual_master: false,
            phrase_16_beat: 0,
        }
    }
}
