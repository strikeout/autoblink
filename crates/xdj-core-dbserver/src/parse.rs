//! Pure parsers for the dbserver menu items and the analysis tags.
//!
//! Each function takes bytes or messages and returns a value. None of them touches the network.

use crate::protocol::{DbMessage, Field, MENU_DATA};
use crate::types::{
    Cue, PhraseEntry, PhraseKind, PhraseSpan, SongStructure, TrackMetadata, TrackMood,
};

/// Chooses the player number for the dbserver setup message.
///
/// The rules follow Beat Link `ConnectionManager.chooseAskingPlayerNumber`.
/// The dbserver accepts a player number from 1 to 4 that is present on the network.
/// A CDJ-3000 class player also accepts its own number.
/// A target above 15 is rekordbox, and it gets the own number.
/// The dbserver keeps one session for each player number. A player that plays a track
/// from the target holds that session, so the client does not pose as that player.
/// When no player is available, the own number is used, and the target can refuse it.
///
/// `players` holds one `(player number, track source player)` pair for each player.
pub fn asking_player(
    own: u8,
    target: u8,
    target_name: &str,
    players: impl IntoIterator<Item = (u8, u8)>,
) -> u8 {
    if target > 15 || (1..=4).contains(&own) {
        return own;
    }
    if target_name.starts_with("CDJ-3000") || target_name.starts_with("XDJ-AZ") {
        return target;
    }
    players
        .into_iter()
        .filter(|&(n, source)| (1..=4).contains(&n) && n != target && source != target)
        .map(|(n, _)| n)
        .min()
        .unwrap_or(own)
}

/// Extracts the available item count from a metadata response.
pub fn extract_menu_count(msg: &DbMessage) -> u32 {
    // The response to a 0x2002 request has the item count in the last numeric arg.
    // Typically arg index 1 is the count, but the search runs backwards for safety.
    for arg in msg.args.iter().rev() {
        if let Field::Number(n) = arg {
            if *n > 0 && *n < 100 {
                return *n;
            }
        }
    }
    // If no count is found, assume a standard 11-item response.
    11
}

/// Parses a menu item (message type 0x4101) into the metadata struct.
///
/// Menu items have 12 arguments:
///   [0]  numeric 1 (parent ID)
///   [1]  numeric 2 (item ID)
///   [2]  label 1 byte size
///   [3]  label 1 (string), the primary text
///   [4]  label 2 byte size
///   [5]  label 2 (string), the secondary text
///   [6..11] remaining fields including the item type at [6]
///
/// Item types that are used:
///   0x0004 = Track title  (item #1, label 1 = title)
///   0x0007 = Artist       (item #2, label 1 = artist)
///   0x000b = Duration     (numeric 2 = seconds)
///   0x000d = Tempo        (item #5, numeric 2 = BPM x 100)
///   0x000f = Key          (item #7, label 1 = key)
pub fn parse_menu_item(item_index: u32, msg: &DbMessage, meta: &mut TrackMetadata) {
    // At least 7 args are needed to get the item type.
    if msg.args.len() < 7 {
        return;
    }
    let item_type = match &msg.args[6] {
        Field::Number(v) => *v,
        _ => return,
    };
    let label1 = match msg.args.get(3) {
        Some(Field::Text(s)) => s.clone(),
        _ => String::new(),
    };

    match item_type {
        0x0004 => meta.title = label1.clone(),
        0x0007 => meta.artist = label1.clone(),
        0x000f => meta.key = label1.clone(),
        0x000b => {
            if let Some(Field::Number(secs)) = msg.args.get(1) {
                meta.duration_s = Some(*secs).filter(|s| *s > 0);
            }
        }
        0x000d => {
            if let Some(Field::Number(raw)) = msg.args.get(1) {
                if *raw > 0 {
                    meta.bpm = Some(*raw as f64 / 100.0);
                }
            }
        }
        // The other items are not used: album (0x02), comment (0x23), rating (0x0a),
        // colour (0x13-1b), original artist (0x28) and date (0x2e).
        _ => {}
    }

    // Fallback: match by item position if item_type was 0 or unrecognised.
    // This handles players that do not populate the type tag correctly.
    if item_type == 0 || (meta.title.is_empty() && meta.artist.is_empty()) {
        match item_index {
            1 if meta.title.is_empty() => meta.title = label1,
            2 if meta.artist.is_empty() => meta.artist = label1,
            7 if meta.key.is_empty() => meta.key = label1,
            _ => {}
        }
    }
}

/// Returns the column bytes of a PWV4 tag blob: 6 bytes per column.
///
/// The blob is `[len u32][PWV4][len_header u32][len_tag u32][len_entry_bytes u32]
/// [len_entries u32][unknown u32][entries]`. The entries start `len_header` bytes after `PWV4`.
/// Beat Link `WaveformPreview` skips the same 28 bytes.
pub fn parse_pwv4(data: &[u8]) -> Result<Vec<u8>, String> {
    let start = tag_start(data, b"PWV4")?;
    let u32_at = |off: usize| {
        data.get(start + off..start + off + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
            .ok_or_else(|| format!("PWV4 header too short: {} bytes", data.len()))
    };
    let (len_header, entry_bytes, entries) = (u32_at(4)?, u32_at(12)?, u32_at(16)?);
    if entry_bytes != 6 {
        return Err(format!("PWV4 entry size is {entry_bytes}, not 6"));
    }
    let body = start.saturating_add(len_header);
    let end = entries
        .checked_mul(6)
        .and_then(|n| body.checked_add(n))
        .ok_or_else(|| "PWV4 size is too large".to_string())?;
    data.get(body..end).map(<[u8]>::to_vec).ok_or_else(|| {
        format!(
            "PWV4 blob {} bytes is too short for {entries} columns",
            data.len()
        )
    })
}

/// Returns the start of a tag in a dbserver blob: byte 4 after the length prefix, or byte 0.
pub fn tag_start(data: &[u8], fourcc: &[u8; 4]) -> Result<usize, String> {
    if data.get(4..8) == Some(&fourcc[..]) {
        Ok(4)
    } else if data.get(0..4) == Some(&fourcc[..]) {
        Ok(0)
    } else {
        let name = String::from_utf8_lossy(fourcc);
        let head: String = data.iter().take(16).map(|b| format!("{b:02x}")).collect();
        Err(format!(
            "{name} blob {} bytes has no {name} code, head {head}",
            data.len()
        ))
    }
}

/// Returns the time in ms of each beat of a PQTZ (beat grid) tag blob. Index 0 is beat 1.
///
/// The tag is `[PQTZ][len_header u32][len_tag u32][unknown u32][unknown u32][len_beats u32]`.
/// The entries start `len_header` bytes after `PQTZ`. Each entry is 8 bytes:
/// `[beat_number u16][tempo u16][time u32]`, with the time in ms.
pub fn parse_pqtz(data: &[u8]) -> Result<Vec<u32>, String> {
    let start = tag_start(data, b"PQTZ")?;
    let u32_at = |off: usize| {
        off.checked_add(4)
            .and_then(|end| data.get(off..end))
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| format!("PQTZ blob {} bytes is too short", data.len()))
    };
    let len_header = u32_at(start + 4)? as usize;
    let beats = u32_at(start + 20)? as usize;
    let body = start.saturating_add(len_header);
    // The blob holds at most one entry for each 8 bytes. This check stops a huge count early.
    if beats > data.len() / 8 {
        return Err(format!("PQTZ blob {} bytes is too short", data.len()));
    }
    let grid = (0..beats)
        .map(|i| u32_at(body.saturating_add(i * 8 + 4)))
        .collect::<Result<Vec<u32>, String>>()?;
    if grid.is_empty() {
        return Err("PQTZ blob has no beats".into());
    }
    Ok(grid)
}

/// Puts the phrases of a song structure on the time axis.
///
/// A phrase beat N is at `grid[N - 1]`. Without a grid, or past its end, the beat time comes
/// from the track BPM, with beat 1 at 0 ms. Returns no phrases when no beat time is known.
pub fn phrase_spans(ss: &SongStructure, grid: Option<&[u32]>, bpm: Option<f64>) -> Vec<PhraseSpan> {
    let beat_ms = |beat: u16| -> Option<u32> {
        let i = usize::from(beat.max(1) - 1);
        grid.and_then(|g| g.get(i).copied()).or_else(|| {
            bpm.filter(|b| *b > 0.0)
                .map(|b| (i as f64 * 60_000.0 / b).round() as u32)
        })
    };
    let ends = ss
        .phrases
        .iter()
        .skip(1)
        .map(|p| p.beat)
        .chain(std::iter::once(ss.end_beat));
    ss.phrases
        .iter()
        .zip(ends)
        .filter_map(|(p, end)| {
            Some(PhraseSpan {
                kind: p.kind,
                start_ms: beat_ms(p.beat)?,
                end_ms: beat_ms(end)?,
                fill_ms: p.has_fill.then(|| beat_ms(p.fill_beat)).flatten(),
            })
        })
        .collect()
}

/// The arguments of the extended cue list request: RMST, rekordbox ID, 0.
pub fn cue_list_args(asking: u8, track_slot: u8, track_type: u8, rekordbox_id: u32) -> [Field; 3] {
    let rmst = (u32::from(asking) << 24)
        | (u32::from(MENU_DATA) << 16)
        | (u32::from(track_slot) << 8)
        | u32::from(track_type);
    [
        Field::Number(rmst),
        Field::Number(rekordbox_id),
        Field::Number(0),
    ]
}

/// Parses the entries of an extended cue list blob (Beat Link `CueList.parseNxs2Entries`).
///
/// Each entry starts with its length as a little-endian u32. The hot cue number is at byte 4
/// (0 = memory cue), the flag at byte 6 (1 = cue, 2 = loop), the start in ms at byte 12 and the
/// loop end in ms at byte 16. Both times are little-endian u32. An entry with another flag is
/// not used.
pub fn parse_cue_list(data: &[u8]) -> Vec<Cue> {
    let le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let mut cues = Vec::new();
    let mut off = 0usize;
    while let Some(e) = data.get(off..off + 20) {
        let len = le(&e[0..4]) as usize;
        let (hot_cue, flag) = (e[4], e[6]);
        let (start_ms, end_ms) = (le(&e[12..16]), le(&e[16..20]));
        if flag == 1 || flag == 2 {
            cues.push(Cue {
                hot_cue,
                start_ms,
                end_ms: (flag == 2).then_some(end_ms),
            });
        }
        if len < 20 {
            break;
        }
        off = off.saturating_add(len);
    }
    cues.sort_by_key(|c| c.start_ms);
    cues
}

/// Parses a raw PSSI tag blob into a song structure.
///
/// The dbserver blob starts with a header:
///   `[len u32][PSSI][len_header u32][len_tag u32]`
/// The parser skips 16 bytes when `PSSI` is at bytes 4..8. It skips 12 bytes when `PSSI` is at
/// bytes 0..4. Otherwise the blob is the bare tag body.
///
/// Layout of the tag body (from the Kaitai Struct spec):
///   `[0..4]`   len_entry_bytes (u32be), always 24
///   `[4..6]`   len_entries (u16be), the number of phrases
///   `[6..]`    body, XOR-masked if raw_mood > 20
///
/// The body after unmasking:
///   `[0..2]`   mood (u16be): 1 = High, 2 = Mid, 3 = Low
///   `[2..8]`   padding
///   `[8..10]`  end_beat (u16be)
///   `[10..12]` padding
///   `[12]`     raw_bank (u8)
///   `[13]`     padding
///   `[14..]`   entries (each 24 bytes)
pub fn parse_pssi_tag(data: &[u8]) -> Result<SongStructure, String> {
    parse_pssi_tag_inner(data).map_err(|e| {
        let head: String = data.iter().take(24).map(|b| format!("{b:02x}")).collect();
        format!("PSSI blob {} bytes, head {head}: {e}", data.len())
    })
}

fn parse_pssi_tag_inner(data: &[u8]) -> Result<SongStructure, String> {
    let start = if data.get(4..8) == Some(&b"PSSI"[..]) {
        16
    } else if data.get(0..4) == Some(&b"PSSI"[..]) {
        12
    } else {
        0
    };
    let data = data.get(start..).unwrap_or_default();
    if data.len() < 6 {
        return Err(format!("PSSI tag too short: {} bytes", data.len()));
    }

    let len_entries = u16::from_be_bytes([data[4], data[5]]);

    let body_raw = &data[6..];
    if body_raw.len() < 2 {
        return Err("PSSI body too short".into());
    }

    // The body is masked when raw_mood is larger than 20.
    let raw_mood = u16::from_be_bytes([body_raw[0], body_raw[1]]);
    let is_masked = raw_mood > 20;

    let body: Vec<u8> = if is_masked {
        // Build the 19-byte XOR mask from len_entries.
        let c = len_entries;
        let mask: [u8; 19] = [
            (0xCBu16.wrapping_add(c)) as u8,
            (0xE1u16.wrapping_add(c)) as u8,
            (0xEEu16.wrapping_add(c)) as u8,
            (0xFAu16.wrapping_add(c)) as u8,
            (0xE5u16.wrapping_add(c)) as u8,
            (0xEEu16.wrapping_add(c)) as u8,
            (0xADu16.wrapping_add(c)) as u8,
            (0xEEu16.wrapping_add(c)) as u8,
            (0xE9u16.wrapping_add(c)) as u8,
            (0xD2u16.wrapping_add(c)) as u8,
            (0xE9u16.wrapping_add(c)) as u8,
            (0xEBu16.wrapping_add(c)) as u8,
            (0xE1u16.wrapping_add(c)) as u8,
            (0xE9u16.wrapping_add(c)) as u8,
            (0xF3u16.wrapping_add(c)) as u8,
            (0xE8u16.wrapping_add(c)) as u8,
            (0xE9u16.wrapping_add(c)) as u8,
            (0xF4u16.wrapping_add(c)) as u8,
            (0xE1u16.wrapping_add(c)) as u8,
        ];
        body_raw
            .iter()
            .enumerate()
            .map(|(i, &b)| b ^ mask[i % mask.len()])
            .collect()
    } else {
        body_raw.to_vec()
    };

    if body.len() < 14 {
        return Err(format!(
            "PSSI unmasked body too short: {} bytes",
            body.len()
        ));
    }

    let mood = match u16::from_be_bytes([body[0], body[1]]) {
        1 => TrackMood::High,
        2 => TrackMood::Mid,
        3 => TrackMood::Low,
        _ => TrackMood::Mid, // fallback
    };
    let end_beat = u16::from_be_bytes([body[8], body[9]]);

    // The entries start at offset 14 in the body.
    let entry_data = &body[14..];
    let entry_size = 24usize;
    let mut phrases = Vec::with_capacity(len_entries as usize);

    for i in 0..len_entries as usize {
        let offset = i * entry_size;
        if offset + entry_size > entry_data.len() {
            break;
        }
        let e = &entry_data[offset..offset + entry_size];
        let index = u16::from_be_bytes([e[0], e[1]]);
        let beat = u16::from_be_bytes([e[2], e[3]]);
        let kind_id = u16::from_be_bytes([e[4], e[5]]);

        let kind = match mood {
            TrackMood::High => match kind_id {
                1 => PhraseKind::Intro,
                2 => PhraseKind::Up,
                3 => PhraseKind::Down,
                5 => PhraseKind::Chorus,
                6 => PhraseKind::Outro,
                other => PhraseKind::Unknown(other),
            },
            TrackMood::Mid => match kind_id {
                1 => PhraseKind::Intro,
                2 => PhraseKind::Verse1,
                3 => PhraseKind::Verse2,
                4 => PhraseKind::Verse3,
                5 => PhraseKind::Verse4,
                6 => PhraseKind::Verse5,
                7 => PhraseKind::Verse6,
                8 => PhraseKind::Bridge,
                9 => PhraseKind::Chorus,
                10 => PhraseKind::Outro,
                other => PhraseKind::Unknown(other),
            },
            TrackMood::Low => match kind_id {
                1 => PhraseKind::Intro,
                2..=4 => PhraseKind::Verse1,
                5..=7 => PhraseKind::Verse2,
                8 => PhraseKind::Bridge,
                9 => PhraseKind::Chorus,
                10 => PhraseKind::Outro,
                other => PhraseKind::Unknown(other),
            },
        };

        phrases.push(PhraseEntry {
            index,
            beat,
            kind,
            has_fill: e[21] != 0,
            fill_beat: u16::from_be_bytes([e[22], e[23]]),
        });
    }

    if phrases.is_empty() {
        return Err(format!(
            "PSSI: no phrase entries parsed (len_entries {len_entries}, raw_mood {raw_mood}, body {} bytes)",
            body.len()
        ));
    }

    Ok(SongStructure {
        mood,
        end_beat,
        phrases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EXT_EXT, MSG_MENU_ITEM, TAG_PSSI, TAG_PWV4};

    const MASK: [u8; 19] = [
        0xCB, 0xE1, 0xEE, 0xFA, 0xE5, 0xEE, 0xAD, 0xEE, 0xE9, 0xD2, 0xE9, 0xEB, 0xE1, 0xE9, 0xF3,
        0xE8, 0xE9, 0xF4, 0xE1,
    ];

    /// Builds the tag body: 14 bytes of header and one 24-byte entry per (beat, kind id).
    fn body(mood: u16, end_beat: u16, entries: &[(u16, u16)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&mood.to_be_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&end_beat.to_be_bytes());
        b.extend_from_slice(&[0; 4]);
        for (i, (beat, kind)) in entries.iter().enumerate() {
            let mut e = [0u8; 24];
            e[0..2].copy_from_slice(&(i as u16 + 1).to_be_bytes());
            e[2..4].copy_from_slice(&beat.to_be_bytes());
            e[4..6].copy_from_slice(&kind.to_be_bytes());
            b.extend_from_slice(&e);
        }
        b
    }

    /// Builds [len_entry_bytes][len_entries][body], with the body masked on request.
    fn tag(body: &[u8], len_entries: u16, masked: bool) -> Vec<u8> {
        let mut t = 24u32.to_be_bytes().to_vec();
        t.extend_from_slice(&len_entries.to_be_bytes());
        for (i, b) in body.iter().enumerate() {
            t.push(if masked {
                b ^ MASK[i % 19].wrapping_add(len_entries as u8)
            } else {
                *b
            });
        }
        t
    }

    /// Adds the dbserver header: [len][PSSI][len_header][len_tag].
    fn with_header(tag: &[u8]) -> Vec<u8> {
        let mut b = ((tag.len() + 12) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"PSSI");
        // The PSSI header is 0x20 bytes long. The tag length includes the 12-byte common header.
        b.extend_from_slice(&0x20u32.to_be_bytes());
        b.extend_from_slice(&((tag.len() + 12) as u32).to_be_bytes());
        b.extend_from_slice(tag);
        b
    }

    fn check(ss: &SongStructure) {
        assert_eq!(ss.mood, TrackMood::Mid);
        assert_eq!(ss.end_beat, 65);
        assert_eq!(ss.phrases.len(), 2);
        assert_eq!(
            (ss.phrases[0].beat, ss.phrases[0].kind),
            (1, PhraseKind::Intro)
        );
        assert_eq!(
            (ss.phrases[1].beat, ss.phrases[1].kind),
            (33, PhraseKind::Verse1)
        );
    }

    #[test]
    fn pssi_with_header_unmasked() {
        let t = tag(&body(2, 65, &[(1, 1), (33, 2)]), 2, false);
        check(&parse_pssi_tag(&with_header(&t)).unwrap());
    }

    #[test]
    fn pssi_with_header_masked() {
        let t = tag(&body(2, 65, &[(1, 1), (33, 2)]), 2, true);
        check(&parse_pssi_tag(&with_header(&t)).unwrap());
    }

    #[test]
    fn pssi_body_only() {
        let t = tag(&body(2, 65, &[(1, 1), (33, 2)]), 2, false);
        check(&parse_pssi_tag(&t).unwrap());
    }

    /// The PSSI blob that an XDJ-AZ sent for a rekordbox track with 20 phrases.
    #[test]
    fn pssi_from_an_xdj_az() {
        let ss = parse_pssi_tag(include_bytes!("../testdata/xdj_az_pssi.bin")).unwrap();
        assert_eq!(ss.mood, TrackMood::High);
        assert_eq!(ss.end_beat, 1008);
        assert_eq!(ss.phrases.len(), 20);
        let first = &ss.phrases[0];
        assert_eq!((first.beat, first.kind), (1, PhraseKind::Intro));
        let last = &ss.phrases[19];
        assert_eq!((last.beat, last.kind), (947, PhraseKind::Outro));
        assert_eq!(
            (ss.phrases[14].has_fill, ss.phrases[14].fill_beat),
            (true, 689)
        );
    }

    fn menu_item(second: u32, item_type: u32) -> DbMessage {
        DbMessage {
            msg_type: MSG_MENU_ITEM,
            args: vec![
                Field::Number(0),
                Field::Number(second),
                Field::Number(2),
                Field::Text(String::new()),
                Field::Number(2),
                Field::Text(String::new()),
                Field::Number(item_type),
            ],
        }
    }

    #[test]
    fn tempo_is_the_second_number_of_the_menu_item() {
        // The tempo item of an XDJ-AZ.
        let mut meta = TrackMetadata::default();
        parse_menu_item(5, &menu_item(12300, 0x0d), &mut meta);
        assert_eq!(meta.bpm, Some(123.0));
    }

    #[test]
    fn duration_is_the_second_number_of_the_menu_item() {
        let mut meta = TrackMetadata::default();
        parse_menu_item(4, &menu_item(312, 0x0b), &mut meta);
        assert_eq!(meta.duration_s, Some(312));
    }

    #[test]
    fn menu_count_is_the_last_small_number() {
        let msg = DbMessage {
            msg_type: 0x4000,
            args: vec![Field::Number(0x2002), Field::Number(11), Field::Number(0)],
        };
        assert_eq!(extract_menu_count(&msg), 11);
        let msg = DbMessage {
            msg_type: 0x4000,
            args: vec![Field::Number(0x2002), Field::Number(7)],
        };
        assert_eq!(extract_menu_count(&msg), 7);
    }

    #[test]
    fn pssi_error_names_the_blob() {
        let t = tag(&body(2, 65, &[]), 0, false);
        let e = parse_pssi_tag(&with_header(&t)).unwrap_err();
        assert!(e.contains("len_entries 0"), "{e}");
        assert!(e.contains("head 000000205053534900000020"), "{e}");
    }

    #[test]
    fn pssi_without_length_prefix() {
        let t = tag(&body(2, 65, &[(1, 1), (33, 2)]), 2, false);
        let mut b = b"PSSI".to_vec();
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(&(t.len() as u32).to_be_bytes());
        b.extend_from_slice(&t);
        check(&parse_pssi_tag(&b).unwrap());
    }

    /// Builds a PWV4 blob as the dbserver sends it: length prefix, tag header, entries.
    fn pwv4_blob(columns: &[[u8; 6]]) -> Vec<u8> {
        let mut b = ((24 + columns.len() * 6) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"PWV4");
        b.extend_from_slice(&0x18u32.to_be_bytes());
        b.extend_from_slice(&((24 + columns.len() * 6) as u32).to_be_bytes());
        b.extend_from_slice(&6u32.to_be_bytes());
        b.extend_from_slice(&(columns.len() as u32).to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        for c in columns {
            b.extend_from_slice(c);
        }
        b
    }

    #[test]
    fn pwv4_entries_start_28_bytes_into_the_blob() {
        let blob = pwv4_blob(&[[1, 2, 3, 4, 5, 6], [7, 8, 9, 10, 11, 12]]);
        assert_eq!(&blob[28..30], &[1, 2]);
        assert_eq!(
            parse_pwv4(&blob).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
    }

    #[test]
    fn pwv4_refuses_a_short_or_foreign_blob() {
        let blob = pwv4_blob(&[[1; 6], [2; 6]]);
        assert!(parse_pwv4(&blob[..blob.len() - 1]).is_err());
        let e = parse_pwv4(&with_header(&[0; 30])).unwrap_err();
        assert!(e.contains("no PWV4 code"), "{e}");
    }

    /// Builds one extended cue list entry of `len` bytes.
    fn cue_entry(len: u32, hot: u8, flag: u8, start: u32, end: u32) -> Vec<u8> {
        let mut e = vec![0u8; len as usize];
        e[0..4].copy_from_slice(&len.to_le_bytes());
        e[4] = hot;
        e[6] = flag;
        e[12..16].copy_from_slice(&start.to_le_bytes());
        e[16..20].copy_from_slice(&end.to_le_bytes());
        e
    }

    #[test]
    fn cue_list_reads_memory_cues_hot_cues_and_loops() {
        let mut blob = cue_entry(0x38, 0, 1, 30_000, 0);
        blob.extend(cue_entry(0x8c, 2, 2, 10_000, 12_000));
        blob.extend(cue_entry(0x38, 0, 0, 5, 0)); // not used
        assert_eq!(
            parse_cue_list(&blob),
            vec![
                Cue {
                    hot_cue: 2,
                    start_ms: 10_000,
                    end_ms: Some(12_000)
                },
                Cue {
                    hot_cue: 0,
                    start_ms: 30_000,
                    end_ms: None
                },
            ]
        );
    }

    #[test]
    fn cue_list_stops_on_a_bad_length() {
        let mut blob = cue_entry(0x38, 1, 1, 1_000, 0);
        blob.extend(cue_entry(20, 0, 1, 2_000, 0));
        blob[0x38..0x3c].copy_from_slice(&0u32.to_le_bytes());
        blob.extend(cue_entry(0x38, 0, 1, 3_000, 0));
        let starts: Vec<u32> = parse_cue_list(&blob).iter().map(|c| c.start_ms).collect();
        assert_eq!(starts, vec![1_000, 2_000]);
        assert!(parse_cue_list(&[0; 10]).is_empty());
    }

    /// Builds a PQTZ blob as the dbserver sends it, with one entry per beat time.
    fn pqtz_blob(times: &[u32]) -> Vec<u8> {
        let mut b = ((24 + times.len() * 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(b"PQTZ");
        b.extend_from_slice(&0x18u32.to_be_bytes());
        b.extend_from_slice(&((24 + times.len() * 8) as u32).to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0x0008_0000u32.to_be_bytes());
        b.extend_from_slice(&(times.len() as u32).to_be_bytes());
        for (i, t) in times.iter().enumerate() {
            b.extend_from_slice(&((i % 4 + 1) as u16).to_be_bytes());
            b.extend_from_slice(&12800u16.to_be_bytes());
            b.extend_from_slice(&t.to_be_bytes());
        }
        b
    }

    #[test]
    fn pqtz_gives_the_time_of_each_beat() {
        let blob = pqtz_blob(&[120, 589, 1058]);
        assert_eq!(parse_pqtz(&blob).unwrap(), vec![120, 589, 1058]);
        assert!(parse_pqtz(&blob[..blob.len() - 1]).is_err());
        assert!(parse_pqtz(&pqtz_blob(&[])).is_err());
    }

    #[test]
    fn phrases_use_the_grid_and_fall_back_to_the_bpm() {
        let entry = |index, beat, kind, fill_beat| PhraseEntry {
            index,
            beat,
            kind,
            has_fill: fill_beat > 0,
            fill_beat,
        };
        let mut ss = SongStructure {
            mood: TrackMood::Mid,
            end_beat: 5,
            phrases: vec![
                entry(1, 1, PhraseKind::Intro, 0),
                entry(2, 3, PhraseKind::Chorus, 4),
            ],
        };
        let grid = [100, 600, 1100, 1600, 2100];
        let spans = phrase_spans(&ss, Some(&grid), Some(120.0));
        let times: Vec<_> = spans
            .iter()
            .map(|p| (p.kind, p.start_ms, p.end_ms, p.fill_ms))
            .collect();
        assert_eq!(
            times,
            vec![
                (PhraseKind::Intro, 100, 1100, None),
                (PhraseKind::Chorus, 1100, 2100, Some(1600)),
            ]
        );
        // 120 BPM is 500 ms for each beat, with beat 1 at 0 ms.
        ss.end_beat = 9;
        let spans = phrase_spans(&ss, None, Some(120.0));
        assert_eq!((spans[1].start_ms, spans[1].end_ms), (1000, 4000));
        assert!(phrase_spans(&ss, None, None).is_empty());
    }

    #[test]
    fn analysis_tag_codes_are_the_reversed_ascii_bytes() {
        assert_eq!(TAG_PSSI, 0x4953_5350);
        assert_eq!(TAG_PWV4, 0x3456_5750);
        assert_eq!(EXT_EXT, 0x0054_5845);
    }

    #[test]
    fn cue_list_request_uses_the_data_menu() {
        assert_eq!(
            cue_list_args(1, 3, 1, 207),
            [
                Field::Number(0x0108_0301),
                Field::Number(207),
                Field::Number(0)
            ]
        );
    }

    #[test]
    fn asking_player_keeps_a_real_player_number() {
        assert_eq!(asking_player(2, 1, "CDJ-2000NXS2", [(1, 1), (3, 3)]), 2);
    }

    #[test]
    fn asking_player_poses_as_the_lowest_other_player() {
        assert_eq!(
            asking_player(5, 1, "CDJ-2000NXS2", [(1, 1), (3, 3), (5, 0), (16, 0)]),
            3
        );
    }

    #[test]
    fn asking_player_skips_a_player_that_plays_from_the_target() {
        assert_eq!(
            asking_player(5, 1, "CDJ-2000NXS2", [(1, 1), (2, 1), (3, 3)]),
            3
        );
    }

    #[test]
    fn asking_player_uses_the_number_of_a_cdj_3000_target() {
        assert_eq!(asking_player(5, 2, "CDJ-3000", [(1, 1), (2, 2)]), 2);
        assert_eq!(asking_player(5, 1, "XDJ-AZ", [(1, 1)]), 1);
    }

    #[test]
    fn asking_player_keeps_the_own_number_for_a_rekordbox_target() {
        assert_eq!(asking_player(5, 17, "rekordbox", [(1, 1)]), 5);
    }

    #[test]
    fn asking_player_falls_back_to_the_own_number() {
        assert_eq!(asking_player(5, 1, "CDJ-2000NXS2", [(1, 1)]), 5);
    }
}
