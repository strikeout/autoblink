use crate::{
    bpm_from_raw, effective_bpm, pitch_to_percent, scale_nominal_beat_ms, AbsPositionPacket,
    BeatPacket, CdjStatus, KeepAlive, MixerStatus, MAGIC, PKT_ABS_POSITION, PKT_BEAT,
    PKT_CDJ_STATUS, PKT_KEEPALIVE, PKT_MIXER_STATUS,
};

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

#[inline]
pub fn has_magic(data: &[u8]) -> bool {
    data.len() >= 11 && data[..10] == MAGIC
}

fn device_name(data: &[u8]) -> alloc::string::String {
    if data.len() < 0x20 {
        alloc::string::String::new()
    } else {
        let raw = &data[0x0c..0x20];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(20);
        let s = core::str::from_utf8(&raw[..end]).unwrap_or("");
        alloc::string::String::from(s)
    }
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

/// Reads a CDJ status packet. Like the host, it accepts the short packets of older players: the
/// parser reads up to offset 0xa6.
pub fn parse_cdj_status(data: &[u8]) -> Option<CdjStatus> {
    if data.len() < 0xa7 {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_CDJ_STATUS {
        return None;
    }
    let state = u16be(data, 0x88);
    let is_master = state & 0x0020 != 0;
    let is_sync = state & 0x0010 != 0;
    let is_on_air = state & 0x0008 != 0;
    let is_playing_flag = state & 0x0040 != 0;
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
        play_state: crate::types::PlayState::from(data[0x7b]),
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
        loop_ms: parse_loop_ms(data),
    })
}

/// Reads the active loop of a CDJ-3000 class status packet, in ms. Each raw value × 65536 / 1000
/// is the position in ms. Returns `None` when the packet is too short or when no loop plays.
fn parse_loop_ms(data: &[u8]) -> Option<(u32, u32)> {
    const START: usize = 0x1b6;
    const END: usize = 0x1be;
    if data.len() < END + 4 {
        return None;
    }
    let ms = |off| (u64::from(u32be(data, off)) * 65_536 / 1_000).min(u64::from(u32::MAX)) as u32;
    let (start, end) = (ms(START), ms(END));
    (start != 0 && end > start).then_some((start, end))
}

pub fn parse_mixer_status(data: &[u8]) -> Option<MixerStatus> {
    if data.len() < 0x38 {
        return None;
    }
    if !has_magic(data) || data[0x0a] != PKT_MIXER_STATUS {
        return None;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BEAT_NONE, PITCH_NORMAL};

    fn magic_pkt(kind: u8, len: usize) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec![0u8; len];
        v[..10].copy_from_slice(&MAGIC);
        v[0x0a] = kind;
        v
    }

    #[test]
    fn beat_timings_scale_by_pitch_and_preserve_sentinel() {
        let mut p = magic_pkt(PKT_BEAT, 0x60);
        p[0x21] = 1;
        p[0x5a] = 0x2e; // 120.00 BPM raw hi
        p[0x5b] = 0xe0; // 120.00 BPM raw lo

        let pitch_plus_8 = ((PITCH_NORMAL as f64) * 1.08).round() as u32;
        p[0x54..0x58].copy_from_slice(&pitch_plus_8.to_be_bytes());

        p[0x24..0x28].copy_from_slice(&500u32.to_be_bytes());
        p[0x28..0x2c].copy_from_slice(&1000u32.to_be_bytes());
        p[0x2c..0x30].copy_from_slice(&BEAT_NONE.to_be_bytes());
        p[0x5c] = 1;

        let bp = parse_beat(&p).expect("beat packet should parse");
        assert_eq!(bp.next_beat_ms, 463);
        assert_eq!(bp.second_beat_ms, 926);
        assert_eq!(bp.next_bar_ms, BEAT_NONE);
    }
}
