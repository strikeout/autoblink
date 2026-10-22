#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
extern crate alloc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    NoTrack,
    Loading,
    Playing,
    PlayingLoop,
    Paused,
    PausedAtCue,
    CuePlaying,
    Searching,
    EndOfTrack,
    Unknown(u8),
}

impl From<u8> for PlayState {
    fn from(v: u8) -> Self {
        match v {
            0x00 => PlayState::NoTrack,
            0x02 => PlayState::Loading,
            0x03 => PlayState::Playing,
            0x04 => PlayState::PlayingLoop,
            0x05 => PlayState::Paused,
            0x06 => PlayState::PausedAtCue,
            0x07 => PlayState::CuePlaying,
            0x09 => PlayState::Searching,
            0x11 => PlayState::EndOfTrack,
            v => PlayState::Unknown(v),
        }
    }
}

impl core::fmt::Display for PlayState {
    /// The same text as the host status.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlayState::NoTrack => f.write_str("No Track"),
            PlayState::Loading => f.write_str("Loading"),
            PlayState::Playing => f.write_str("Playing"),
            PlayState::PlayingLoop => f.write_str("Playing Loop"),
            PlayState::Paused => f.write_str("Paused"),
            PlayState::PausedAtCue => f.write_str("Paused at Cue"),
            PlayState::CuePlaying => f.write_str("Cue Playing"),
            PlayState::Searching => f.write_str("Searching"),
            PlayState::EndOfTrack => f.write_str("End of Track"),
            PlayState::Unknown(v) => write!(f, "Unknown({v})"),
        }
    }
}

impl PlayState {
    pub fn is_playing(self) -> bool {
        matches!(
            self,
            PlayState::Playing | PlayState::PlayingLoop | PlayState::CuePlaying
        )
    }
}

#[derive(Debug, Clone)]
pub struct KeepAlive {
    pub device_number: u8,
    pub device_type: u8,
    pub name: alloc::string::String,
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    pub peer_count: u8,
}

#[derive(Debug, Clone)]
pub struct BeatPacket {
    pub device_number: u8,
    pub next_beat_ms: u32,
    pub second_beat_ms: u32,
    pub next_bar_ms: u32,
    pub pitch_raw: u32,
    pub bpm_raw: u16,
    pub beat_in_bar: u8,
    pub track_bpm: Option<f64>,
    pub effective_bpm: f64,
    pub pitch_pct: f64,
}

#[derive(Debug, Clone)]
pub struct AbsPositionPacket {
    pub device_number: u8,
    pub track_length_s: u32,
    pub playhead_ms: u32,
    pub pitch_raw_signed: i32,
    pub bpm_x10: u32,
    pub effective_bpm: f64,
    pub pitch_pct: f64,
}

#[derive(Debug, Clone)]
pub struct CdjStatus {
    pub device_number: u8,
    pub play_state: PlayState,
    pub is_master: bool,
    pub is_sync: bool,
    pub is_on_air: bool,
    pub is_playing_flag: bool,
    pub pitch_raw: u32,
    pub bpm_raw: u16,
    pub track_bpm: Option<f64>,
    pub effective_bpm: f64,
    pub pitch_pct: f64,
    pub beat_count: u32,
    pub beat_in_bar: u8,
    pub track_source_player: u8,
    pub track_slot: u8,
    pub track_type: u8,
    pub rekordbox_id: u32,
    /// The active loop as (start, end) in ms. Only a CDJ-3000 class packet holds it.
    pub loop_ms: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct MixerStatus {
    pub device_number: u8,
    pub is_master: bool,
    pub bpm_raw: u16,
    pub track_bpm: Option<f64>,
    pub beat_in_bar: u8,
}
