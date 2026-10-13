//! Pro DJ Link dbserver metadata client.
//!
//! Queries the TCP-based database server on CDJs/XDJs to retrieve track
//! metadata (title, artist, key, BPM) that is **not** available in the
//! UDP status packets.
//!
//! Protocol reference:
//!   <https://djl-analysis.deepsymmetry.org/djl-analysis/track_metadata.html>

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::discovery::DeviceTable;
use crate::state::{PhraseEntry, PhraseKind, SharedState, SongStructure, TrackChange, TrackMood};

// ── Constants ─────────────────────────────────────────────────────────────────

/// TCP port used to discover each player's dbserver port.
const DB_SERVER_QUERY_PORT: u16 = 12523;

/// Magic bytes that start every dbserver message.
const DB_MAGIC: [u8; 4] = [0x87, 0x23, 0x49, 0xae];

/// Timeout for the entire metadata fetch (connect + query).
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for individual TCP read/write operations.
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// The largest dbserver field that the reader accepts. Waveform blobs are smaller.
const MAX_FIELD_LEN: usize = 16 * 1024 * 1024;

// ── Message types ─────────────────────────────────────────────────────────────

const MSG_SETUP_REQ: u16 = 0x0000;
const MSG_METADATA_REQ: u16 = 0x2002;
const MSG_RENDER_MENU: u16 = 0x3000;
const MSG_MENU_HEADER: u16 = 0x4001;
const MSG_MENU_ITEM: u16 = 0x4101;
const MSG_MENU_FOOTER: u16 = 0x4201;
const MSG_ANLZ_TAG_REQ: u16 = 0x2c04;
/// The response to the analysis tag request has type 0x4f02, not 0x2c04.
const MSG_ANLZ_TAG_RESP: u16 = 0x4f02;

// ── Field type tags ───────────────────────────────────────────────────────────

const FIELD_U8: u8 = 0x0f;
const FIELD_U16: u8 = 0x10;
const FIELD_U32: u8 = 0x11;
const FIELD_BLOB: u8 = 0x14;
const FIELD_STRING: u8 = 0x26;

// ── Public result ─────────────────────────────────────────────────────────────

/// Track metadata retrieved from the dbserver.
#[derive(Debug, Clone, Default)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub key: String,
    /// Original track BPM (not pitch-adjusted).
    pub bpm: Option<f64>,
}

// ── In-flight request dedup ───────────────────────────────────────────────────

/// Tracks which (device, rekordbox_id) fetches are currently in progress so we
/// don't fire duplicate TCP queries for the same track.
type InflightMap = Arc<Mutex<HashMap<(u8, u32), ()>>>;

// ── Setup refusal ─────────────────────────────────────────────────────────────

/// The dbserver did not accept the player number in the setup message.
#[derive(Debug)]
struct SetupRefused {
    player: u8,
}

impl std::fmt::Display for SetupRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dbserver refused player number {}", self.player)
    }
}

impl std::error::Error for SetupRefused {}

/// Choose the player number for the dbserver setup message.
///
/// The rules follow Beat Link `ConnectionManager.chooseAskingPlayerNumber`.
/// The dbserver accepts a player number from 1 to 4 that is present on the network.
/// A CDJ-3000 class player also accepts its own number.
/// A target above 15 is rekordbox, and it gets the own number.
/// The dbserver keeps one session for each player number. A player that plays a track
/// from the target holds that session, so the virtual CDJ does not pose as that player.
/// When no player is available, the own number is used, and the target can refuse it.
fn asking_player(
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

// ── Public entry point ────────────────────────────────────────────────────────

/// Spawns the metadata fetcher loop.
///
/// Listens on `rx` for [`TrackChange`] events, looks up the player IP from the
/// device table, queries the dbserver, and writes the result into shared state.
pub async fn run(
    our_device_number: u8,
    device_table: DeviceTable,
    state: SharedState,
    mut rx: mpsc::Receiver<TrackChange>,
) {
    let inflight: InflightMap = Arc::new(Mutex::new(HashMap::new()));
    // The (target, asking player) pairs that already produced a refusal warning.
    let warned: Arc<Mutex<HashSet<(u8, u8)>>> = Arc::new(Mutex::new(HashSet::new()));

    while let Some(change) = rx.recv().await {
        // The track source player of each deck. Read it before the device table lock.
        let sources: HashMap<u8, u8> = state
            .read()
            .devices
            .iter()
            .map(|(n, d)| (*n, d.track_source_player))
            .collect();
        let (player_ip, asking) = {
            let table = device_table.lock();
            let target = table.get(&change.track_source_player);
            let ip = target.map(|d| Ipv4Addr::from(d.ip));
            let asking = asking_player(
                our_device_number,
                change.track_source_player,
                target.map_or("", |d| d.name.as_str()),
                table
                    .keys()
                    .map(|n| (*n, sources.get(n).copied().unwrap_or(0))),
            );
            (ip, asking)
        };
        let Some(ip) = player_ip else {
            tracing::warn!(
                device = change.device_number,
                source_player = change.track_source_player,
                rekordbox_id = change.rekordbox_id,
                "Track change: source player not in device table — check network connectivity"
            );
            continue;
        };

        tracing::debug!(
            device = change.device_number,
            rekordbox_id = change.rekordbox_id,
            track_type = change.track_type,
            track_slot = change.track_slot,
            source_player = change.track_source_player,
            "Metadata fetch: queuing track query"
        );

        // Deduplicate in-flight requests.
        let key = (change.device_number, change.rekordbox_id);
        {
            let mut map = inflight.lock();
            if map.contains_key(&key) {
                continue;
            }
            map.insert(key, ());
        }

        let inflight2 = Arc::clone(&inflight);
        let state2 = Arc::clone(&state);
        let warned2 = Arc::clone(&warned);
        let change2 = change.clone();

        tokio::spawn(async move {
            match timeout(FETCH_TIMEOUT, fetch_metadata(ip, asking, &change2)).await {
                Ok(Ok((meta, song_struct))) => {
                    tracing::info!(
                        device = change2.device_number,
                        rekordbox_id = change2.rekordbox_id,
                        title = %meta.title,
                        artist = %meta.artist,
                        key = %meta.key,
                        "Track metadata received"
                    );
                    let mut st = state2.write();
                    st.set_track_metadata(
                        change2.device_number,
                        change2.rekordbox_id,
                        meta.title,
                        meta.artist,
                        meta.key,
                        meta.bpm,
                    );
                    if let Some(ss) = song_struct {
                        tracing::info!(
                            device = change2.device_number,
                            phrases = ss.phrases.len(),
                            mood = ?ss.mood,
                            "Song structure received"
                        );
                        st.set_song_structure(change2.device_number, change2.rekordbox_id, ss);
                    }
                }
                Ok(Err(e)) if e.downcast_ref::<SetupRefused>().is_some() => {
                    if warned2.lock().insert((change2.track_source_player, asking)) {
                        tracing::warn!(
                            target_player = change2.track_source_player,
                            %ip,
                            "dbserver refused player number {asking}; phrase and metadata are not available"
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(
                        device = change2.device_number,
                        rekordbox_id = change2.rekordbox_id,
                        error = %e,
                        "Metadata fetch failed (device may not support dbserver)"
                    );
                }
                Err(_) => {
                    tracing::debug!(
                        device = change2.device_number,
                        rekordbox_id = change2.rekordbox_id,
                        "Metadata fetch timed out"
                    );
                }
            }
            inflight2.lock().remove(&key);
        });
    }
}

// ── Core fetch logic ──────────────────────────────────────────────────────────

async fn fetch_metadata(
    player_ip: Ipv4Addr,
    asking: u8,
    change: &TrackChange,
) -> anyhow::Result<(TrackMetadata, Option<SongStructure>)> {
    // Step 1: Discover the dbserver port.
    let db_port = discover_db_port(player_ip).await?;
    tracing::debug!(%player_ip, db_port, "dbserver port discovered");

    // Step 2: Connect to the dbserver.
    let addr = SocketAddrV4::new(player_ip, db_port);
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(addr)).await??;
    tracing::debug!(%addr, "dbserver TCP connected");

    // Step 3: Exchange greeting (send number field 1, expect number field 1 back).
    send_number_field(&mut stream, 1, 4).await?;
    let greeting = read_field(&mut stream).await?;
    if greeting != Field::Number(1) {
        anyhow::bail!("Unexpected greeting response: {:?}", greeting);
    }
    tracing::debug!("dbserver greeting OK");

    // Step 4: Setup exchange — message type 0x0000 with the asking player number.
    let mut tx_id: u32 = 1;
    send_message(
        &mut stream,
        tx_id,
        MSG_SETUP_REQ,
        &[Field::Number(asking as u32)],
        &[FIELD_U32],
    )
    .await?;
    // A player that refuses the number usually closes the connection and sends no reply.
    let setup_resp = match read_message(&mut stream).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(error = %e, "dbserver sent no setup response");
            return Err(SetupRefused { player: asking }.into());
        }
    };
    if setup_resp.msg_type != 0x4000 {
        tracing::debug!(
            msg_type = format!("0x{:04x}", setup_resp.msg_type).as_str(),
            "dbserver setup response is not 0x4000"
        );
        return Err(SetupRefused { player: asking }.into());
    }
    tracing::debug!("dbserver setup handshake OK");

    // Step 5: Request metadata — message type 0x2002.
    tx_id += 1;
    // Build the DMST argument: D|01|Sr|Tr packed as a u32.
    let dmst = ((asking as u32) << 24)
        | (0x01u32 << 16)
        | ((change.track_slot as u32) << 8)
        | (change.track_type as u32);
    send_message(
        &mut stream,
        tx_id,
        MSG_METADATA_REQ,
        &[Field::Number(dmst), Field::Number(change.rekordbox_id)],
        &[FIELD_U32, FIELD_U32],
    )
    .await?;
    let meta_resp = read_message(&mut stream).await?;
    let item_count = extract_menu_count(&meta_resp)?;
    tracing::debug!(
        rekordbox_id = change.rekordbox_id,
        item_count,
        "Metadata response received, requesting menu items"
    );

    if item_count == 0 {
        anyhow::bail!("Metadata query returned 0 items");
    }

    // Step 6: Render menu — message type 0x3000 to fetch the items.
    tx_id += 1;
    send_message(
        &mut stream,
        tx_id,
        MSG_RENDER_MENU,
        &[
            Field::Number(dmst),
            Field::Number(0),          // offset
            Field::Number(item_count), // limit
            Field::Number(0),
            Field::Number(item_count), // total
            Field::Number(0),
        ],
        &[
            FIELD_U32, FIELD_U32, FIELD_U32, FIELD_U32, FIELD_U32, FIELD_U32,
        ],
    )
    .await?;

    // Step 7: Read menu items until footer (0x4201).
    let mut metadata = TrackMetadata::default();
    let mut item_index = 0u32;
    loop {
        let msg = read_message(&mut stream).await?;
        match msg.msg_type {
            MSG_MENU_HEADER => continue,
            MSG_MENU_FOOTER => break,
            MSG_MENU_ITEM => {
                item_index += 1;
                parse_menu_item(item_index, &msg, &mut metadata);
            }
            other => {
                tracing::debug!(
                    msg_type = format!("0x{:04x}", other),
                    "Unknown menu message"
                );
            }
        }
    }

    // Step 8: Request PSSI (song structure / phrase analysis) from .EXT file.
    let song_structure = match fetch_pssi(&mut stream, &mut tx_id, asking, change).await {
        Ok(ss) => Some(ss),
        Err(e) => {
            tracing::debug!(
                device = change.device_number,
                error = %e,
                "PSSI fetch failed (phrase analysis not available)"
            );
            None
        }
    };

    Ok((metadata, song_structure))
}

// ── PSSI (song structure / phrase analysis) fetch ─────────────────────────────

/// Fetch the PSSI tag (song structure) from the player's .EXT analysis file
/// via the ANLZ_TAG_REQ (0x2c04) dbserver request.
async fn fetch_pssi(
    stream: &mut TcpStream,
    tx_id: &mut u32,
    asking: u8,
    change: &TrackChange,
) -> anyhow::Result<SongStructure> {
    *tx_id += 1;

    // Build DMST argument: D|01|Sr|Tr
    let dmst = ((asking as u32) << 24)
        | (0x01u32 << 16)
        | ((change.track_slot as u32) << 8)
        | (change.track_type as u32);

    // Tag type: "PSSI" reversed bytes → 0x49535350
    let tag_type: u32 = 0x49535350;

    // File extension: "EXT\0" reversed bytes → 0x00545845
    let file_ext: u32 = 0x00545845;

    send_message(
        stream,
        *tx_id,
        MSG_ANLZ_TAG_REQ,
        &[
            Field::Number(dmst),
            Field::Number(change.rekordbox_id),
            Field::Number(tag_type),
            Field::Number(file_ext),
        ],
        &[FIELD_U32, FIELD_U32, FIELD_U32, FIELD_U32],
    )
    .await?;
    tracing::debug!(
        rekordbox_id = change.rekordbox_id,
        dmst = format!("0x{:08x}", dmst).as_str(),
        "PSSI tag request sent"
    );

    let resp = read_message(stream).await?;
    tracing::debug!(
        msg_type = format!("0x{:04x}", resp.msg_type).as_str(),
        arg_count = resp.args.len(),
        "PSSI response received"
    );

    if resp.msg_type != MSG_ANLZ_TAG_RESP {
        anyhow::bail!(
            "Expected PSSI response 0x{:04x}, got 0x{:04x}",
            MSG_ANLZ_TAG_RESP,
            resp.msg_type
        );
    }

    // The response should contain a blob with the PSSI tag body.
    // Find the blob argument.
    let blob = resp
        .args
        .iter()
        .find_map(|a| match a {
            Field::Blob(data) if data.len() > 20 => Some(data.clone()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("No PSSI blob in response"))?;

    parse_pssi_tag(&blob)
}

/// Parse a raw PSSI tag blob into a SongStructure.
///
/// The dbserver blob starts with a header:
///   [len u32][PSSI][len_header u32][len_tag u32]
/// The parser skips 16 bytes when `PSSI` is at bytes 4..8. It skips 12 bytes when `PSSI` is at
/// bytes 0..4. Otherwise the blob is the bare tag body.
///
/// Layout of the tag body (from Kaitai Struct spec):
///   [0..4]   len_entry_bytes (u32be) — always 24
///   [4..6]   len_entries (u16be) — number of phrases
///   [6..]    body — XOR-masked if raw_mood > 20
///
/// The body after unmasking:
///   [0..2]   mood (u16be) — TrackMood enum (1=High, 2=Mid, 3=Low)
///   [2..8]   padding
///   [8..10]  end_beat (u16be)
///   [10..12] padding
///   [12]     raw_bank (u8)
///   [13]     padding
///   [14..]   entries (each 24 bytes)
fn parse_pssi_tag(data: &[u8]) -> anyhow::Result<SongStructure> {
    let start = if data.get(4..8) == Some(&b"PSSI"[..]) {
        16
    } else if data.get(0..4) == Some(&b"PSSI"[..]) {
        12
    } else {
        0
    };
    let data = data.get(start..).unwrap_or_default();
    if data.len() < 6 {
        anyhow::bail!("PSSI tag too short: {} bytes", data.len());
    }

    let _len_entry_bytes = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let len_entries = u16::from_be_bytes([data[4], data[5]]);

    let body_raw = &data[6..];
    if body_raw.len() < 2 {
        anyhow::bail!("PSSI body too short");
    }

    // Check if masked: read raw_mood from offset 0 of body.
    let raw_mood = u16::from_be_bytes([body_raw[0], body_raw[1]]);
    let is_masked = raw_mood > 20;

    let body: Vec<u8> = if is_masked {
        // Build 19-byte XOR mask based on len_entries.
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
        anyhow::bail!("PSSI unmasked body too short: {} bytes", body.len());
    }

    let mood_val = u16::from_be_bytes([body[0], body[1]]);
    let mood = match mood_val {
        1 => TrackMood::High,
        2 => TrackMood::Mid,
        3 => TrackMood::Low,
        _ => TrackMood::Mid, // fallback
    };
    let end_beat = u16::from_be_bytes([body[8], body[9]]);

    // Parse entries starting at offset 14 in the body.
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
                2 | 3 | 4 => PhraseKind::Verse1,
                5 | 6 | 7 => PhraseKind::Verse2,
                8 => PhraseKind::Bridge,
                9 => PhraseKind::Chorus,
                10 => PhraseKind::Outro,
                other => PhraseKind::Unknown(other),
            },
        };

        let fill = e[21];
        let fill_beat = u16::from_be_bytes([e[22], e[23]]);

        phrases.push(PhraseEntry {
            index,
            beat,
            kind,
            has_fill: fill != 0,
            fill_beat,
        });
    }

    if phrases.is_empty() {
        anyhow::bail!("PSSI: no phrase entries parsed");
    }

    Ok(SongStructure {
        mood,
        end_beat,
        phrases,
    })
}

// ── Dbserver port discovery ───────────────────────────────────────────────────

async fn discover_db_port(player_ip: Ipv4Addr) -> anyhow::Result<u16> {
    match try_discover_db_port(player_ip).await {
        Ok(port) if port != 0 => Ok(port),
        _ => {
            tracing::debug!(
                %player_ip,
                "dbserver discovery on port {} failed or returned 0, trying well-known port 1051",
                DB_SERVER_QUERY_PORT
            );
            Ok(1051)
        }
    }
}

async fn try_discover_db_port(player_ip: Ipv4Addr) -> anyhow::Result<u16> {
    let addr = SocketAddrV4::new(player_ip, DB_SERVER_QUERY_PORT);
    let mut stream = timeout(IO_TIMEOUT, TcpStream::connect(addr)).await??;

    // Send the discovery query: length-prefixed "RemoteDBServ\0".
    let payload = b"RemoteDBServ\0";
    let mut pkt = Vec::with_capacity(4 + payload.len());
    pkt.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    pkt.extend_from_slice(payload);
    timeout(IO_TIMEOUT, stream.write_all(&pkt)).await??;

    // Read the 2-byte port number.
    let mut port_buf = [0u8; 2];
    timeout(IO_TIMEOUT, stream.read_exact(&mut port_buf)).await??;
    let port = u16::from_be_bytes(port_buf);
    if port == 0 {
        anyhow::bail!("Player returned dbserver port 0");
    }
    Ok(port)
}

// ── Low-level field I/O ───────────────────────────────────────────────────────

/// A field value in the dbserver protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    /// Numeric field (1, 2, or 4 bytes).
    Number(u32),
    /// Binary blob.
    Blob(Vec<u8>),
    /// UTF-16BE string (decoded to Rust String).
    Text(String),
}

/// Send a bare number field (used for the greeting exchange).
async fn send_number_field(stream: &mut TcpStream, value: u32, size: u8) -> anyhow::Result<()> {
    let type_tag = match size {
        1 => FIELD_U8,
        2 => FIELD_U16,
        4 => FIELD_U32,
        _ => anyhow::bail!("Invalid number field size {}", size),
    };
    let mut buf = Vec::with_capacity(5);
    buf.push(type_tag);
    match size {
        1 => buf.push(value as u8),
        2 => buf.extend_from_slice(&(value as u16).to_be_bytes()),
        4 => buf.extend_from_slice(&value.to_be_bytes()),
        _ => unreachable!(),
    }
    timeout(IO_TIMEOUT, stream.write_all(&buf)).await??;
    Ok(())
}

/// Read a single field from the stream.
async fn read_field(stream: &mut TcpStream) -> anyhow::Result<Field> {
    let mut tag = [0u8; 1];
    timeout(IO_TIMEOUT, stream.read_exact(&mut tag)).await??;
    match tag[0] {
        FIELD_U8 => {
            let mut b = [0u8; 1];
            timeout(IO_TIMEOUT, stream.read_exact(&mut b)).await??;
            Ok(Field::Number(b[0] as u32))
        }
        FIELD_U16 => {
            let mut b = [0u8; 2];
            timeout(IO_TIMEOUT, stream.read_exact(&mut b)).await??;
            Ok(Field::Number(u16::from_be_bytes(b) as u32))
        }
        FIELD_U32 => {
            let mut b = [0u8; 4];
            timeout(IO_TIMEOUT, stream.read_exact(&mut b)).await??;
            Ok(Field::Number(u32::from_be_bytes(b)))
        }
        FIELD_BLOB => {
            let mut len_buf = [0u8; 4];
            timeout(IO_TIMEOUT, stream.read_exact(&mut len_buf)).await??;
            let len = u32::from_be_bytes(len_buf) as usize;
            // A bad peer can send any length. Refuse a length that is too large to allocate.
            if len > MAX_FIELD_LEN {
                anyhow::bail!("dbserver field length {len} is larger than {MAX_FIELD_LEN}");
            }
            let mut data = vec![0u8; len];
            if len > 0 {
                timeout(IO_TIMEOUT, stream.read_exact(&mut data)).await??;
            }
            Ok(Field::Blob(data))
        }
        FIELD_STRING => {
            let mut len_buf = [0u8; 4];
            timeout(IO_TIMEOUT, stream.read_exact(&mut len_buf)).await??;
            let len = u32::from_be_bytes(len_buf) as usize;
            // A bad peer can send any length. Refuse a length that is too large to allocate.
            if len > MAX_FIELD_LEN {
                anyhow::bail!("dbserver field length {len} is larger than {MAX_FIELD_LEN}");
            }
            let mut data = vec![0u8; len];
            if len > 0 {
                timeout(IO_TIMEOUT, stream.read_exact(&mut data)).await??;
            }
            Ok(Field::Text(decode_utf16be(&data)))
        }
        other => {
            anyhow::bail!("Unknown field type tag 0x{:02x}", other);
        }
    }
}

/// Decode UTF-16 big-endian bytes to a Rust String, stripping null terminators.
fn decode_utf16be(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u16s)
        .trim_end_matches('\0')
        .to_string()
}

// ── Message I/O ───────────────────────────────────────────────────────────────

/// A parsed dbserver message.
struct DbMessage {
    msg_type: u16,
    args: Vec<Field>,
}

/// Send a dbserver message.
async fn send_message(
    stream: &mut TcpStream,
    tx_id: u32,
    msg_type: u16,
    args: &[Field],
    type_tags: &[u8],
) -> anyhow::Result<()> {
    let arg_count = args.len() as u8;

    // Build type-tag area (12 bytes, padded with 0x00).
    let mut tags = [0u8; 12];
    for (i, &t) in type_tags.iter().enumerate().take(12) {
        tags[i] = t;
    }

    let mut buf = Vec::with_capacity(64);

    // Magic.
    buf.extend_from_slice(&DB_MAGIC);
    // Transaction ID (as a u32 field: type tag + value).
    buf.push(FIELD_U32);
    buf.extend_from_slice(&tx_id.to_be_bytes());
    // Message type (as a u16 field).
    buf.push(FIELD_U16);
    buf.extend_from_slice(&msg_type.to_be_bytes());
    // Argument count (as a u8 field).
    buf.push(FIELD_U8);
    buf.push(arg_count);
    // Type tags (as a blob: 4-byte length + 12 bytes).
    buf.push(FIELD_BLOB);
    buf.extend_from_slice(&12u32.to_be_bytes());
    buf.extend_from_slice(&tags);

    // Arguments.
    for (i, arg) in args.iter().enumerate() {
        let tag = type_tags.get(i).copied().unwrap_or(FIELD_U32);
        match arg {
            Field::Number(v) => match tag {
                FIELD_U8 => {
                    buf.push(FIELD_U8);
                    buf.push(*v as u8);
                }
                FIELD_U16 => {
                    buf.push(FIELD_U16);
                    buf.extend_from_slice(&(*v as u16).to_be_bytes());
                }
                _ => {
                    buf.push(FIELD_U32);
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            },
            Field::Blob(data) => {
                buf.push(FIELD_BLOB);
                buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
                buf.extend_from_slice(data);
            }
            Field::Text(s) => {
                let encoded = encode_utf16be(s);
                buf.push(FIELD_STRING);
                buf.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                buf.extend_from_slice(&encoded);
            }
        }
    }

    timeout(IO_TIMEOUT, stream.write_all(&buf)).await??;
    Ok(())
}

/// Read a dbserver message from the stream.
async fn read_message(stream: &mut TcpStream) -> anyhow::Result<DbMessage> {
    // Read magic.
    let mut magic = [0u8; 4];
    timeout(IO_TIMEOUT, stream.read_exact(&mut magic)).await??;
    if magic != DB_MAGIC {
        anyhow::bail!(
            "Bad dbserver magic: {:02x}{:02x}{:02x}{:02x}",
            magic[0],
            magic[1],
            magic[2],
            magic[3]
        );
    }

    // Transaction ID (field).
    let _tx_id = read_field(stream).await?;
    // Message type (field).
    let msg_type_field = read_field(stream).await?;
    let msg_type = match msg_type_field {
        Field::Number(v) => v as u16,
        _ => anyhow::bail!("Expected numeric message type"),
    };
    // Argument count (field).
    let arg_count_field = read_field(stream).await?;
    let arg_count = match arg_count_field {
        Field::Number(v) => v as usize,
        _ => anyhow::bail!("Expected numeric argument count"),
    };
    // Type tags blob.
    let _tags = read_field(stream).await?;

    // Read arguments.
    let mut args = Vec::with_capacity(arg_count);
    for _ in 0..arg_count {
        args.push(read_field(stream).await?);
    }

    Ok(DbMessage { msg_type, args })
}

/// Encode a Rust string as UTF-16BE with null terminator.
fn encode_utf16be(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in s.encode_utf16() {
        out.extend_from_slice(&c.to_be_bytes());
    }
    out.extend_from_slice(&[0x00, 0x00]); // null terminator
    out
}

// ── Menu item parsing ─────────────────────────────────────────────────────────

/// Extract the available item count from a metadata response.
fn extract_menu_count(msg: &DbMessage) -> anyhow::Result<u32> {
    // The response to a 0x2002 request has the item count in the last numeric arg.
    // Typically arg index 1 is the count, but we search backwards for safety.
    for arg in msg.args.iter().rev() {
        if let Field::Number(n) = arg {
            if *n > 0 && *n < 100 {
                return Ok(*n);
            }
        }
    }
    // If we can't find a count, assume a standard 11-item response.
    Ok(11)
}

/// Parse a menu item (message type 0x4101) into the metadata struct.
///
/// Menu items have 12 arguments:
///   [0]  numeric 1 (parent ID)
///   [1]  numeric 2 (item ID)
///   [2]  label 1 byte size
///   [3]  label 1 (string)  ← primary text
///   [4]  label 2 byte size
///   [5]  label 2 (string)  ← secondary text
///   [6..11] remaining fields including item type at [6]
///
/// Item types we care about:
///   0x0004 = Track title  (item #1, label 1 = title)
///   0x0007 = Artist       (item #2, label 1 = artist)
///   0x000d = Tempo        (item #5, numeric 1 = BPM × 100)
///   0x000f = Key          (item #7, label 1 = key)
fn parse_menu_item(item_index: u32, msg: &DbMessage, meta: &mut TrackMetadata) {
    // We need at least 7 args to get the item type.
    if msg.args.len() < 7 {
        return;
    }

    // The item type is typically in arg[6] as a number.
    let item_type = match &msg.args[6] {
        Field::Number(v) => *v,
        _ => return,
    };

    // Extract label 1 (arg[3] is the string).
    let label1 = match msg.args.get(3) {
        Some(Field::Text(s)) => s.clone(),
        _ => String::new(),
    };

    match item_type {
        0x0004 => {
            // Track title.
            meta.title = label1.clone();
        }
        0x0007 => {
            // Artist.
            meta.artist = label1.clone();
        }
        0x000f => {
            // Musical key.
            meta.key = label1.clone();
        }
        0x000d => {
            // Tempo — numeric 1 (arg[0]) contains BPM × 100.
            if let Some(Field::Number(raw)) = msg.args.first() {
                if *raw > 0 {
                    meta.bpm = Some(*raw as f64 / 100.0);
                }
            }
        }
        _ => {
            // Items we don't need: album (0x02), duration (0x0b), comment (0x23),
            // rating (0x0a), color (0x13-1b), original artist (0x28), date (0x2e).
        }
    }

    // Fallback: match by item position if item_type was 0 or unrecognised.
    // This handles players that don't populate the type tag correctly.
    if item_type == 0 || (meta.title.is_empty() && meta.artist.is_empty()) {
        match item_index {
            1 if meta.title.is_empty() => meta.title = label1.clone(),
            2 if meta.artist.is_empty() => meta.artist = label1.clone(),
            7 if meta.key.is_empty() => meta.key = label1,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn pssi_without_length_prefix() {
        let t = tag(&body(2, 65, &[(1, 1), (33, 2)]), 2, false);
        let mut b = b"PSSI".to_vec();
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(&(t.len() as u32).to_be_bytes());
        b.extend_from_slice(&t);
        check(&parse_pssi_tag(&b).unwrap());
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
