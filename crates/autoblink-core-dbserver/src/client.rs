//! The blocking dbserver client.
//!
//! [`fetch`] follows the sequence of `fetch_metadata` in the host: greeting, setup, metadata,
//! menu items, then the analysis tags. Each analysis part can fail alone.

use std::fmt;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

use crate::parse::*;
use crate::protocol::*;
use crate::types::*;

/// Timeout for the entire fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Timeout for the connect and for each TCP read or write.
const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// The well-known dbserver port. It is used when the port query fails.
const FALLBACK_DB_PORT: u16 = 1051;
/// The transaction ID of the setup message. An XDJ-AZ closes the connection on ID 1.
const SETUP_TX_ID: u32 = 0xffff_fffe;

/// The reason that a fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The dbserver did not accept the player number in the setup message.
    SetupRefused { player: u8 },
    /// Any other fault. The text names the step that failed.
    Other(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::SetupRefused { player } => {
                write!(f, "dbserver refused player number {player}")
            }
            FetchError::Other(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<String> for FetchError {
    fn from(text: String) -> Self {
        FetchError::Other(text)
    }
}

/// The result of a fetch.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub metadata: TrackMetadata,
    /// The song structure, or the reason that it is not available (for `phrase_note`).
    pub song_structure: Result<SongStructure, String>,
    /// The preview, the cues and the phrase times. `None` when no analysis part succeeded.
    pub overview: Option<TrackOverview>,
    /// One text for each analysis part that failed. The host logs these as warnings.
    pub warnings: Vec<String>,
}

/// Opens a TCP connection to an address, with a timeout. [`fetch`] uses
/// `TcpStream::connect_timeout`.
pub type Connect<'a> = &'a dyn Fn(&SocketAddr, Duration) -> std::io::Result<TcpStream>;

/// Returns the dbserver port of a player. Falls back to 1051 when the query fails or returns 0.
pub fn discover_db_port(ip: Ipv4Addr, timeout: Duration) -> u16 {
    discover_db_port_with(&TcpStream::connect_timeout, ip, timeout)
}

fn discover_db_port_with(connect: Connect, ip: Ipv4Addr, timeout: Duration) -> u16 {
    try_discover_db_port(connect, ip, timeout)
        .ok()
        .filter(|port| *port != 0)
        .unwrap_or(FALLBACK_DB_PORT)
}

fn try_discover_db_port(connect: Connect, ip: Ipv4Addr, timeout: Duration) -> std::io::Result<u16> {
    let addr = SocketAddr::V4(SocketAddrV4::new(ip, DB_SERVER_QUERY_PORT));
    let mut stream = connect(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // The query is the length-prefixed name "RemoteDBServer\0".
    // An XDJ-AZ replies with port 0xffff to a shorter name.
    let payload = b"RemoteDBServer\0";
    let mut pkt = Vec::with_capacity(4 + payload.len());
    pkt.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    pkt.extend_from_slice(payload);
    stream.write_all(&pkt)?;
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf)?;
    Ok(u16::from_be_bytes(port_buf))
}

/// Fetches the metadata and the analysis of one track from the player at `ip`.
///
/// `asking` is the player number for the setup message (see [`asking_player`]).
/// The call blocks for at most about 8 seconds.
pub fn fetch(
    ip: Ipv4Addr,
    asking: u8,
    track_slot: u8,
    track_type: u8,
    rekordbox_id: u32,
) -> Result<Fetched, FetchError> {
    fetch_with(&TcpStream::connect_timeout, ip, asking, track_slot, track_type, rekordbox_id)
}

/// Like [`fetch`], but opens the two TCP connections with `connect`. Use it to select the
/// network interface.
pub fn fetch_with(
    connect: Connect,
    ip: Ipv4Addr,
    asking: u8,
    track_slot: u8,
    track_type: u8,
    rekordbox_id: u32,
) -> Result<Fetched, FetchError> {
    let start = Instant::now();
    let port = discover_db_port_with(connect, ip, IO_TIMEOUT);
    run(
        connect,
        SocketAddr::V4(SocketAddrV4::new(ip, port)),
        asking,
        track_slot,
        track_type,
        rekordbox_id,
        start,
    )
}

/// Like [`fetch`], but connects to `addr` and skips the port discovery.
pub fn fetch_at(
    addr: SocketAddr,
    asking: u8,
    track_slot: u8,
    track_type: u8,
    rekordbox_id: u32,
) -> Result<Fetched, FetchError> {
    run(
        &TcpStream::connect_timeout,
        addr,
        asking,
        track_slot,
        track_type,
        rekordbox_id,
        Instant::now(),
    )
}

/// The state of one dbserver session.
struct Session {
    stream: TcpStream,
    tx_id: u32,
    asking: u8,
    track_slot: u8,
    track_type: u8,
    rekordbox_id: u32,
    start: Instant,
}

impl Session {
    /// The DMST argument: player, 0x01, slot and track type packed in a u32.
    fn dmst(&self) -> u32 {
        (u32::from(self.asking) << 24)
            | (0x01u32 << 16)
            | (u32::from(self.track_slot) << 8)
            | u32::from(self.track_type)
    }

    fn check_deadline(&self) -> Result<(), String> {
        if self.start.elapsed() > FETCH_TIMEOUT {
            Err("Metadata fetch timed out".into())
        } else {
            Ok(())
        }
    }

    fn send(&mut self, tx_id: u32, msg_type: u16, args: &[Field]) -> Result<(), String> {
        self.check_deadline()?;
        let tags = [FIELD_U32; 6];
        let buf = encode_message(tx_id, msg_type, args, &tags[..args.len().min(tags.len())]);
        self.stream
            .write_all(&buf)
            .map_err(|e| format!("dbserver write failed: {e}"))
    }

    fn next_tx(&mut self) -> u32 {
        self.tx_id += 1;
        self.tx_id
    }

    /// Fetches one tag of the analysis file (`file_ext` is .EXT or .DAT) with the
    /// ANLZ_TAG_REQ (0x2c04) request. `read` gets the length of the tag blob and a reader for it
    /// (see [`read_message_with`]). The blob includes the length prefix and the tag header.
    fn anlz_tag<T>(
        &mut self,
        tag_type: u32,
        file_ext: u32,
        read: impl FnOnce(usize, &mut dyn Read) -> Result<T, String>,
    ) -> Result<T, String> {
        let tx = self.next_tx();
        let tag_name = String::from_utf8_lossy(&tag_type.to_le_bytes()).into_owned();
        let args = [
            Field::Number(self.dmst()),
            Field::Number(self.rekordbox_id),
            Field::Number(tag_type),
            Field::Number(file_ext),
        ];
        self.send(tx, MSG_ANLZ_TAG_REQ, &args)?;
        let (resp, value) = read_message_with(&mut self.stream, read)?;
        if resp.msg_type != MSG_ANLZ_TAG_RESP {
            return Err(format!(
                "Expected {tag_name} response 0x{:04x}, got 0x{:04x} [{}]",
                MSG_ANLZ_TAG_RESP,
                resp.msg_type,
                describe_args(&resp.args)
            ));
        }
        value.unwrap_or_else(|| Err(format!("No {tag_name} blob in the response")))
    }

    /// Fetches the memory cues, hot cues and saved loops with the CUE_LIST_EXT_REQ (0x2b04)
    /// request. The arguments follow Beat Link `MetadataFinder.getCueList`.
    fn cue_list(&mut self) -> Result<Vec<Cue>, String> {
        let tx = self.next_tx();
        let args = cue_list_args(
            self.asking,
            self.track_slot,
            self.track_type,
            self.rekordbox_id,
        );
        self.send(tx, MSG_CUE_LIST_EXT_REQ, &args)?;
        let resp = read_message(&mut self.stream)?;
        if resp.msg_type != MSG_CUE_LIST_EXT_RESP {
            return Err(format!(
                "Expected cue list response 0x{:04x}, got 0x{:04x} [{}]",
                MSG_CUE_LIST_EXT_RESP,
                resp.msg_type,
                describe_args(&resp.args)
            ));
        }
        // Beat Link reads the entries from argument 3.
        match resp.args.get(3) {
            Some(Field::Blob(b)) => Ok(parse_cue_list(b)),
            _ => Err(format!(
                "No cue list blob in [{}]",
                describe_args(&resp.args)
            )),
        }
    }
}

fn run(
    connect: Connect,
    addr: SocketAddr,
    asking: u8,
    track_slot: u8,
    track_type: u8,
    rekordbox_id: u32,
    start: Instant,
) -> Result<Fetched, FetchError> {
    // Connect to the dbserver.
    let stream = connect(&addr, IO_TIMEOUT)
        .map_err(|e| FetchError::Other(format!("dbserver connect failed: {e}")))?;
    let io = |e: std::io::Error| FetchError::Other(format!("dbserver socket setup failed: {e}"));
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    let mut s = Session {
        stream,
        tx_id: 1,
        asking,
        track_slot,
        track_type,
        rekordbox_id,
        start,
    };

    // Greeting: send number field 1, expect number field 1 back.
    s.check_deadline()?;
    let greeting = encode_number_field(1, 4)?;
    s.stream
        .write_all(&greeting)
        .map_err(|e| FetchError::Other(format!("dbserver write failed: {e}")))?;
    let reply = read_field(&mut s.stream)?;
    if reply != Field::Number(1) {
        return Err(FetchError::Other(format!(
            "Unexpected greeting response: {reply:?}"
        )));
    }

    // Setup: message type 0x0000 with the asking player number.
    let refused = FetchError::SetupRefused { player: asking };
    s.send(
        SETUP_TX_ID,
        MSG_SETUP_REQ,
        &[Field::Number(u32::from(asking))],
    )?;
    // A player that refuses the number usually closes the connection and sends no reply.
    match read_message(&mut s.stream) {
        Ok(resp) if resp.msg_type == 0x4000 => {}
        _ => return Err(refused),
    }

    // Metadata: message type 0x2002.
    let dmst = s.dmst();
    s.send(
        s.tx_id,
        MSG_METADATA_REQ,
        &[Field::Number(dmst), Field::Number(rekordbox_id)],
    )?;
    let item_count = extract_menu_count(&read_message(&mut s.stream)?);
    if item_count == 0 {
        return Err(FetchError::Other("Metadata query returned 0 items".into()));
    }

    // Render menu: message type 0x3000 fetches the items.
    let tx = s.next_tx();
    s.send(
        tx,
        MSG_RENDER_MENU,
        &[
            Field::Number(dmst),
            Field::Number(0),          // offset
            Field::Number(item_count), // limit
            Field::Number(0),
            Field::Number(item_count), // total
            Field::Number(0),
        ],
    )?;

    // Read menu items until the footer (0x4201).
    let mut metadata = TrackMetadata::default();
    let mut item_index = 0u32;
    loop {
        s.check_deadline()?;
        let msg = read_message(&mut s.stream)?;
        match msg.msg_type {
            MSG_MENU_FOOTER => break,
            MSG_MENU_ITEM => {
                item_index += 1;
                parse_menu_item(item_index, &msg, &mut metadata);
            }
            // The header and any unknown message are skipped.
            _ => {}
        }
    }

    // Analysis data. A failure keeps the metadata. The caller shows the reason.
    // The client reads the beat grid and the preview as a stream. It keeps only the beats of the
    // phrase starts, the fill-ins and the end, and at most PREVIEW_COLUMNS preview columns.
    let song_structure = s
        .anlz_tag(TAG_PSSI, EXT_EXT, |len, r| read_sized(r, len))
        .and_then(|blob| parse_pssi_tag(&blob));
    // Without phrases, the beat grid has no use. The client then does not request it.
    let beat_grid = song_structure.as_ref().ok().map(|ss| {
        let mut wanted: Vec<u16> = ss
            .phrases
            .iter()
            .flat_map(|p| [Some(p.beat), p.has_fill.then_some(p.fill_beat)])
            .flatten()
            .chain([ss.end_beat])
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        s.anlz_tag(TAG_PQTZ, EXT_DAT, |len, r| read_pqtz(r, len, Some(&wanted)))
    });
    let preview = s.anlz_tag(TAG_PWV4, EXT_EXT, |len, r| {
        read_pwv4(r, len, PREVIEW_COLUMNS)
    });
    let cues = s.cue_list();

    let mut warnings = Vec::new();
    if let Some(Err(e)) = &beat_grid {
        warnings.push(format!(
            "Beat grid is not available; the phrase times use the track BPM: {e}"
        ));
    }
    if let Err(e) = &preview {
        warnings.push(format!("Waveform preview is not available: {e}"));
    }
    if let Err(e) = &cues {
        warnings.push(format!("Cue list is not available: {e}"));
    }
    let phrases = song_structure
        .as_ref()
        .map(|ss| {
            let grid = beat_grid.as_ref().and_then(|g| g.as_deref().ok());
            phrase_spans(ss, grid, metadata.bpm)
        })
        .unwrap_or_default();
    let overview =
        (preview.is_ok() || cues.is_ok() || !phrases.is_empty()).then(|| TrackOverview {
            rekordbox_id,
            preview: preview.unwrap_or_default(),
            cues: cues.unwrap_or_default(),
            phrases,
        });

    Ok(Fetched {
        metadata,
        song_structure,
        overview,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn send(stream: &mut TcpStream, msg_type: u16, args: &[Field]) {
        stream
            .write_all(&encode_message(1, msg_type, args, &[]))
            .unwrap();
    }

    fn menu_item(label: &str, item_type: u32) -> Vec<Field> {
        vec![
            Field::Number(0),
            Field::Number(0),
            Field::Number(2),
            Field::Text(label.into()),
            Field::Number(2),
            Field::Text(String::new()),
            Field::Number(item_type),
        ]
    }

    /// One cue entry: a memory cue at 30 s.
    fn cue_blob() -> Vec<u8> {
        let mut e = vec![0u8; 0x38];
        e[0..4].copy_from_slice(&0x38u32.to_le_bytes());
        e[6] = 1;
        e[12..16].copy_from_slice(&30_000u32.to_le_bytes());
        e
    }

    /// A scripted dbserver. It answers the cue list and refuses the other analysis tags.
    fn serve(listener: TcpListener, setup_ok: bool) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut greeting = [0u8; 5];
            s.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting, [0x11, 0, 0, 0, 1]);
            s.write_all(&greeting).unwrap();

            let setup = read_message(&mut s).unwrap();
            assert_eq!(setup.msg_type, MSG_SETUP_REQ);
            assert_eq!(setup.args, vec![Field::Number(3)]);
            if !setup_ok {
                return;
            }
            send(&mut s, 0x4000, &[Field::Number(0), Field::Number(3)]);

            let meta = read_message(&mut s).unwrap();
            assert_eq!(meta.msg_type, MSG_METADATA_REQ);
            assert_eq!(meta.args[0], Field::Number(0x0301_0301));
            assert_eq!(meta.args[1], Field::Number(207));
            send(
                &mut s,
                0x4000,
                &[Field::Number(MSG_METADATA_REQ.into()), Field::Number(2)],
            );

            let render = read_message(&mut s).unwrap();
            assert_eq!(render.msg_type, MSG_RENDER_MENU);
            send(
                &mut s,
                MSG_MENU_HEADER,
                &[Field::Number(0), Field::Number(0)],
            );
            send(&mut s, MSG_MENU_ITEM, &menu_item("Test Title", 4));
            send(&mut s, MSG_MENU_ITEM, &menu_item("Test Artist", 7));
            send(
                &mut s,
                MSG_MENU_FOOTER,
                &[Field::Number(0), Field::Number(0)],
            );

            // PSSI, PWV4, cue list. The PSSI refusal skips the PQTZ request.
            for i in 0..3 {
                let req = read_message(&mut s).unwrap();
                if i == 2 {
                    assert_eq!(req.msg_type, MSG_CUE_LIST_EXT_REQ);
                    let blob = Field::Blob(cue_blob());
                    send(
                        &mut s,
                        MSG_CUE_LIST_EXT_RESP,
                        &[Field::Number(0), Field::Number(0), Field::Number(0), blob],
                    );
                } else {
                    assert_eq!(req.msg_type, MSG_ANLZ_TAG_REQ);
                    // A response of the wrong type is a refusal.
                    send(&mut s, 0x4000, &[Field::Number(0)]);
                }
            }
        })
    }

    #[test]
    fn fetch_returns_the_metadata_when_the_analysis_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve(listener, true);

        let got = fetch_at(addr, 3, 3, 1, 207).unwrap();
        server.join().unwrap();

        assert_eq!(got.metadata.title, "Test Title");
        assert_eq!(got.metadata.artist, "Test Artist");
        assert!(got.song_structure.as_ref().unwrap_err().contains("0x4f02"));
        let overview = got.overview.unwrap();
        assert_eq!(overview.rekordbox_id, 207);
        assert!(overview.preview.is_empty());
        assert_eq!(overview.cues.len(), 1);
        assert_eq!(overview.cues[0].start_ms, 30_000);
        assert_eq!(got.warnings.len(), 1);
    }

    #[test]
    fn fetch_reports_a_refused_setup() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve(listener, false);
        let err = fetch_at(addr, 3, 3, 1, 207).unwrap_err();
        server.join().unwrap();
        assert_eq!(err, FetchError::SetupRefused { player: 3 });
        assert_eq!(err.to_string(), "dbserver refused player number 3");
    }

    #[test]
    fn fetch_fails_when_nothing_listens() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(matches!(
            fetch_at(addr, 3, 3, 1, 207),
            Err(FetchError::Other(_))
        ));
    }
}
