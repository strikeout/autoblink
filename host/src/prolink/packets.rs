//! Zero-copy packet parsing for the Pro DJ Link protocol.

use std::fmt;

use super::{
    bpm_from_raw, effective_bpm, pitch_to_percent, scale_nominal_beat_ms, MAGIC, PKT_ABS_POSITION,
    PKT_BEAT, PKT_CDJ_STATUS, PKT_KEEPALIVE, PKT_MIXER_STATUS,
};

/// The parser reads up to offset 0xa6.
const CDJ_STATUS_MIN_LEN: usize = 0xa7;
const CDJ_STATUS_STATE_OFFSET: usize = 0x88;

// ── Helpers ───────────────────────────────────────────────────────────────────

#[inline]
fn u16be(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32be(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn i32be(b: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Validate the 10-byte magic header.
pub fn has_magic(data: &[u8]) -> bool {
    data.len() >= 11 && data[..10] == MAGIC
}

/// Extract device name from keep-alive packets where model is at 0x0c (20 bytes).
pub fn device_name(data: &[u8]) -> String {
    if data.len() < 0x20 {
        return String::new();
    }
    let raw = &data[0x0c..0x20];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(20);
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// Extract device name from beat packets where it lives at offset 0x0b (17 bytes).
#[allow(dead_code)]
pub fn beat_device_name(data: &[u8]) -> String {
    if data.len() < 0x1c {
        return String::new();
    }
    let raw = &data[0x0b..0x1b];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

// ── Keep-alive packet (port 50000, type 0x06) ─────────────────────────────────

/// Parsed keep-alive / device-announcement packet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct KeepAlive {
    pub device_number: u8,
    pub device_type: u8,
    pub name: String,
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    pub peer_count: u8,
}

pub fn parse_keepalive(data: &[u8]) -> Option<KeepAlive> {
    if data.len() < 0x36 {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_KEEPALIVE {
        return None;
    }
    let mac = data[0x26..0x2c].try_into().ok()?;
    let ip = data[0x2c..0x30].try_into().ok()?;
    Some(KeepAlive {
        device_number: data[0x24],
        device_type: data[0x25],
        name: device_name(data),
        mac,
        ip,
        peer_count: data[0x31],
    })
}

// ── Beat packet (port 50001, type 0x28) ───────────────────────────────────────

/// Parsed beat packet — fired on every beat by CDJs playing an analysed track,
/// and periodically by the mixer.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BeatPacket {
    pub device_number: u8,
    /// Wall-clock ms until the next beat (pitch-adjusted; `BEAT_NONE` sentinel preserved).
    pub next_beat_ms: u32,
    /// Wall-clock ms until the 2nd upcoming beat (pitch-adjusted; `BEAT_NONE` preserved).
    pub second_beat_ms: u32,
    /// Wall-clock ms until next bar/downbeat (pitch-adjusted; `BEAT_NONE` preserved).
    pub next_bar_ms: u32,
    /// Raw pitch value (0x00100000 = 0%).
    pub pitch_raw: u32,
    /// Track BPM × 100 (0xFFFF = no track).
    pub bpm_raw: u16,
    /// Beat within bar (1–4).
    pub beat_in_bar: u8,
    /// Track BPM (None if no analysed track loaded).
    pub track_bpm: Option<f64>,
    /// Effective (pitch-adjusted) BPM.
    pub effective_bpm: f64,
    /// Pitch as percent (-100.0 … +100.0).
    pub pitch_pct: f64,
}

pub fn parse_beat(data: &[u8]) -> Option<BeatPacket> {
    if data.len() < 0x60 {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_BEAT {
        return None;
    }
    let bpm_raw = u16be(data, 0x5a);
    let pitch_raw = u32be(data, 0x54);
    let track_bpm = bpm_from_raw(bpm_raw);
    let eff_bpm = if let Some(b) = track_bpm {
        effective_bpm(b, pitch_raw)
    } else {
        0.0
    };
    Some(BeatPacket {
        device_number: data[0x21],
        next_beat_ms: scale_nominal_beat_ms(u32be(data, 0x24), pitch_raw),
        second_beat_ms: scale_nominal_beat_ms(u32be(data, 0x28), pitch_raw),
        next_bar_ms: scale_nominal_beat_ms(u32be(data, 0x2c), pitch_raw),
        pitch_raw,
        bpm_raw,
        beat_in_bar: data[0x5c],
        track_bpm,
        effective_bpm: eff_bpm,
        pitch_pct: pitch_to_percent(pitch_raw),
    })
}

// ── Absolute position packet (port 50001, type 0x0b) — CDJ-3000 ──────────────

/// Parsed absolute-position packet.  Sent every ~30 ms by CDJ-3000 / XDJ-XZ
/// even while paused; much more reliable than beat interpolation.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AbsPositionPacket {
    pub device_number: u8,
    /// Track length in seconds.
    pub track_length_s: u32,
    /// Playhead position in milliseconds.
    pub playhead_ms: u32,
    /// Signed pitch percent × 100 (e.g. 326 = 3.26%).
    pub pitch_raw_signed: i32,
    /// Effective BPM × 10 (e.g. 1202 = 120.2 BPM).
    pub bpm_x10: u32,
    /// Effective BPM as f64.
    pub effective_bpm: f64,
    /// Pitch as percent.
    pub pitch_pct: f64,
}

pub fn parse_abs_position(data: &[u8]) -> Option<AbsPositionPacket> {
    if data.len() < 0x3c {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_ABS_POSITION {
        return None;
    }
    let bpm_x10 = u32be(data, 0x38);
    let pitch_raw = i32be(data, 0x2c);
    Some(AbsPositionPacket {
        device_number: data[0x21],
        track_length_s: u32be(data, 0x24),
        playhead_ms: u32be(data, 0x28),
        pitch_raw_signed: pitch_raw,
        bpm_x10,
        effective_bpm: bpm_x10 as f64 / 10.0,
        pitch_pct: pitch_raw as f64 / 100.0,
    })
}

// ── CDJ status packet (port 50002, type 0x0a) ─────────────────────────────────

/// Play state decoded from byte 0x7b.
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

impl PlayState {
    pub fn is_playing(self) -> bool {
        matches!(
            self,
            PlayState::Playing | PlayState::PlayingLoop | PlayState::CuePlaying
        )
    }
}

impl fmt::Display for PlayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// Key fields from a CDJ status packet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CdjStatus {
    pub device_number: u8,
    pub play_state: PlayState,
    /// True when the CDJ is the network tempo master.
    pub is_master: bool,
    /// True when sync is enabled.
    pub is_sync: bool,
    /// True when the channel is audible (on-air).
    pub is_on_air: bool,
    /// True when state flags bit 6 (play) is set — more reliable than play_state byte on some hardware.
    pub is_playing_flag: bool,
    /// Effective pitch (0x00100000 = 0%).
    pub pitch_raw: u32,
    /// Track BPM × 100 (0xFFFF = none).
    pub bpm_raw: u16,
    /// Track BPM (None if no analysed track).
    pub track_bpm: Option<f64>,
    /// Effective BPM.
    pub effective_bpm: f64,
    /// Pitch as percent.
    pub pitch_pct: f64,
    /// Absolute beat counter from track start (0xFFFFFFFF = unavailable).
    pub beat_count: u32,
    /// Beat within bar (1–4; 0 if no rekordbox track).
    pub beat_in_bar: u8,
    /// Device number the track was loaded from (0x28: Dr).
    pub track_source_player: u8,
    /// Media slot: 0=none, 1=CD, 2=SD, 3=USB, 4=rekordbox (0x29: Sr).
    pub track_slot: u8,
    /// Track type: 0=none, 1=rekordbox, 2=unanalysed, 5=CD, 6=streaming (0x2a: Tr).
    pub track_type: u8,
    /// Rekordbox database ID (0x2c–0x2f, big-endian).  0 = no track.
    pub rekordbox_id: u32,
}

pub fn parse_cdj_status(data: &[u8]) -> Option<CdjStatus> {
    // The parser reads up to offset 0xa6. Older players send packets shorter than
    // the nexus-era 0xd4 bytes, so the guard checks only the bytes that it reads.
    if data.len() < CDJ_STATUS_MIN_LEN {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_CDJ_STATUS {
        return None;
    }
    // State flags at 0x88-0x89 (Int16ub / StateMask):
    //   bit 6 = play, bit 5 = master, bit 4 = sync, bit 3 = on-air
    let state = cdj_state_word(data)?;
    let is_master = state & 0x0020 != 0;
    let is_sync = state & 0x0010 != 0;
    let is_on_air = state & 0x0008 != 0;
    let is_playing_flag = state & 0x0040 != 0;
    // Actual pitch (including sync adjustments) at 0x98-0x9b.
    // (Physical/slider pitch is at 0x8c-0x8f but we want the effective one.)
    let pitch_raw = u32be(data, 0x98);
    let bpm_raw = u16be(data, 0x92);
    let track_bpm = bpm_from_raw(bpm_raw);
    let eff_bpm = if let Some(b) = track_bpm {
        effective_bpm(b, pitch_raw)
    } else {
        0.0
    };
    Some(CdjStatus {
        device_number: data[0x21],
        play_state: PlayState::from(data[0x7b]),
        is_master,
        is_sync,
        is_on_air,
        is_playing_flag,
        pitch_raw,
        bpm_raw,
        track_bpm,
        effective_bpm: eff_bpm,
        pitch_pct: pitch_to_percent(pitch_raw),
        beat_count: u32be(data, 0xa0),
        beat_in_bar: data[0xa6],
        track_source_player: data[0x28],
        track_slot: data[0x29],
        track_type: data[0x2a],
        rekordbox_id: u32be(data, 0x2c),
    })
}

pub(crate) fn cdj_state_word(data: &[u8]) -> Option<u16> {
    let bytes = data.get(CDJ_STATUS_STATE_OFFSET..CDJ_STATUS_STATE_OFFSET + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

// ── Mixer status packet (port 50002, type 0x29) ───────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MixerStatus {
    pub device_number: u8,
    /// True when the mixer is the tempo master.
    pub is_master: bool,
    /// BPM × 100 as reported by mixer (tracks master player).
    pub bpm_raw: u16,
    /// Track BPM (None if 0xFFFF).
    pub track_bpm: Option<f64>,
    /// Beat within bar (1–4; not always reliable).
    pub beat_in_bar: u8,
}

pub fn parse_mixer_status(data: &[u8]) -> Option<MixerStatus> {
    if data.len() < 0x38 {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_MIXER_STATUS {
        return None;
    }
    // State flags at 0x26-0x27 (Int16ub / StateMask):
    //   master = 0x0020, same as CDJ status.
    //   Mixer sends 0xf0 when master, 0xd0 when not.
    let state = u16be(data, 0x26);
    let bpm_raw = u16be(data, 0x2e);
    Some(MixerStatus {
        device_number: data[0x21],
        is_master: state & 0x0020 != 0,
        bpm_raw,
        track_bpm: bpm_from_raw(bpm_raw),
        beat_in_bar: data[0x37],
    })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prolink::{PITCH_NORMAL, PKT_BEAT, PKT_KEEPALIVE};

    fn magic_pkt(pkt_type: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[..10].copy_from_slice(&MAGIC);
        v[0x0a] = pkt_type;
        v
    }

    #[test]
    fn keepalive_roundtrip() {
        let mut p = magic_pkt(PKT_KEEPALIVE, 0x36);
        p[0x24] = 3; // device number
        p[0x25] = 0x01; // CDJ type
        p[0x26..0x2c].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        p[0x2c..0x30].copy_from_slice(&[192, 168, 1, 10]);
        p[0x31] = 2;
        let ka = parse_keepalive(&p).unwrap();
        assert_eq!(ka.device_number, 3);
        assert_eq!(ka.mac, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(ka.ip, [192, 168, 1, 10]);
        assert_eq!(ka.peer_count, 2);
    }

    #[test]
    fn beat_packet_bpm() {
        let mut p = magic_pkt(PKT_BEAT, 0x60);
        p[0x21] = 1;
        // BPM = 128.00 → raw = 12800 = 0x3200
        p[0x5a] = 0x32;
        p[0x5b] = 0x00;
        // pitch = 0% → 0x00100000
        p[0x54..0x58].copy_from_slice(&PITCH_NORMAL.to_be_bytes());
        p[0x5c] = 1; // beat in bar
        let bp = parse_beat(&p).unwrap();
        assert!((bp.track_bpm.unwrap() - 128.0).abs() < 0.01);
        assert!((bp.effective_bpm - 128.0).abs() < 0.01);
        assert!((bp.pitch_pct).abs() < 0.01);
    }

    #[test]
    fn beat_packet_timings_are_scaled_by_pitch_and_preserve_none_sentinel() {
        let mut p = magic_pkt(PKT_BEAT, 0x60);
        p[0x21] = 1;
        p[0x5a] = 0x2e; // 120.00
        p[0x5b] = 0xe0;

        // +8% pitch.
        let pitch_plus_8 = ((PITCH_NORMAL as f64) * 1.08).round() as u32;
        p[0x54..0x58].copy_from_slice(&pitch_plus_8.to_be_bytes());

        p[0x24..0x28].copy_from_slice(&500u32.to_be_bytes()); // nominal next beat
        p[0x28..0x2c].copy_from_slice(&1000u32.to_be_bytes());
        p[0x2c..0x30].copy_from_slice(&u32::MAX.to_be_bytes()); // sentinel
        p[0x5c] = 1;

        let bp = parse_beat(&p).expect("beat packet should parse");
        assert_eq!(bp.next_beat_ms, 463); // 500 / 1.08
        assert_eq!(bp.second_beat_ms, 926); // 1000 / 1.08
        assert_eq!(bp.next_bar_ms, u32::MAX);
    }

    #[test]
    fn pitch_conversion() {
        assert!((pitch_to_percent(PITCH_NORMAL)).abs() < 0.001);
        assert!((pitch_to_percent(PITCH_NORMAL * 2) - 100.0).abs() < 0.001);
        assert!((pitch_to_percent(0) - (-100.0)).abs() < 0.001);
    }

    #[test]
    fn cdj_state_word_short_packet_does_not_panic() {
        let packet = magic_pkt(PKT_CDJ_STATUS, 0x89);

        assert_eq!(cdj_state_word(&packet), None);
    }

    #[test]
    fn parse_cdj_status_rejects_truncated_packet() {
        let packet = magic_pkt(PKT_CDJ_STATUS, CDJ_STATUS_MIN_LEN - 1);

        assert!(parse_cdj_status(&packet).is_none());
    }

    #[test]
    fn parse_abs_position_reads_cdj3000_length_packet() {
        // A CDJ-3000 sends 0x3c bytes, with BPM x10 at offset 0x38 (Beat Link PrecisePosition).
        let mut packet = magic_pkt(PKT_ABS_POSITION, 0x3c);
        packet[0x21] = 2;
        packet[0x28..0x2c].copy_from_slice(&5000u32.to_be_bytes());
        packet[0x38..0x3c].copy_from_slice(&1245u32.to_be_bytes());

        let p = parse_abs_position(&packet).expect("0x3c packet parses");
        assert_eq!(p.device_number, 2);
        assert_eq!(p.playhead_ms, 5000);
        assert!((p.effective_bpm - 124.5).abs() < 1e-9);
        assert!(parse_abs_position(&packet[..0x3b]).is_none());
    }
}
