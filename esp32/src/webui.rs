//! Serves `webui/index.html` and speaks WebSocket protocol v1 (see `host/src/web/mod.rs`).
//!
//! The firmware sends the capabilities `beat`, `decks`, `output`, `settings`, `waveform`, `ota`
//! and `scenes`. The page hides the sections of the other capabilities: `mixer`, `ports` and
//! `logs`. Without `mixer`, the page also hides the mixer rows of the scenes.
//! With `ota`, the page shows the System panel: the firmware update, the restart button and the
//! WiFi settings. The page sends `reboot` for the restart button.
//!
//! With the `dmx` feature, the firmware also sends the capability `dmx` and the status field
//! `dmx`. `GET /show` gives the DMX show as JSON, and `POST /show` replaces it. The commands
//! `dmx_live` and `dmx_fog` set the live controls. They get no reply.
//!
//! `GET /ofl/search?q=<text>` and `GET /ofl/fixture?key=<manufacturer>/<model>` pass the JSON of
//! the Open Fixture Library through to the page (see `ofl.rs`).
//!
//! The firmware sends the status and the beats to one page at a time, the active page. Another
//! page gets `busy` and shows a button. The button sends `take_over`, and the firmware closes the
//! connection of the active page. That page then connects again and gets `busy`.

use std::borrow::Cow;
use std::ffi::CString;
use std::fmt::{Display, Write as _};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::http::server::ws::{EspHttpWsConnection, EspHttpWsDetachedSender};
use esp_idf_svc::http::server::{Configuration, EspHttpServer, KeepAlive};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::sys::{self, EspError, ESP_ERR_INVALID_SIZE, ESP_FAIL};
use esp_idf_svc::ws::FrameType;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};

#[cfg(feature = "dmx")]
use crate::dmx::{DmxMsg, DmxStatus};
use crate::ofl;
use crate::ota::{self, OtaCmd, OtaStatus};
use crate::settings::Settings;
use crate::tracks::TrackData;
use crate::State;
use autoblink_core_dbserver::{phrase_status, PhraseKind, TrackOverview};
use autoblink_core_midi::scenes::{SceneStatus, ScenesConfig};
use autoblink_core_prolink::{Deck, PlayState};
#[cfg(feature = "dmx")]
use esp_idf_svc::hal::task::queue::Queue;
#[cfg(feature = "dmx")]
use esp_idf_svc::http::server::{EspHttpConnection, Request};

/// The page, compressed with gzip by `build.rs`.
const INDEX_HTML_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

/// The firmware ignores a command that is longer than this.
/// The scenes command is the longest command, about 1.5 KB with 16 rows. A row without holds leaves out the `holds` key.
const MAX_COMMAND_LEN: usize = 3072;

/// The capabilities in the hello message.
const CAPS: &[&str] = &[
    "beat",
    "decks",
    "output",
    "settings",
    "waveform",
    "ota",
    "scenes",
    #[cfg(feature = "dmx")]
    "dmx",
];

/// `POST /show` refuses a longer body.
#[cfg(feature = "dmx")]
const MAX_SHOW_LEN: usize = 60_000;

/// The page gets the status this often.
const STATUS_INTERVAL: Duration = Duration::from_millis(100);

/// The WebSocket connection of the active page. The status and the beats go only to it. A send
/// that fails removes the connection.
///
/// lwIP gives a new connection the lowest free socket number. Thus a socket number can name a
/// new page soon after the old page closes. The number of each change of the active page lets a
/// failed send remove only the page that it sent to.
#[derive(Default)]
struct Active(Mutex<(u32, Option<EspHttpWsDetachedSender>)>);

impl Active {
    /// Returns true when the send failed and removed the active page.
    fn send(&self, text: &[u8]) -> bool {
        // A send waits for the HTTP server task, and that task takes this lock when a connection
        // closes. Send without the lock, so that the two tasks cannot block each other.
        let (generation, Some(mut sender)) = self.0.lock().unwrap().clone() else {
            return false;
        };
        if sender.send(FrameType::Text(false), text).is_err() {
            let mut active = self.0.lock().unwrap();
            if active.0 == generation {
                active.1 = None;
                return true;
            }
        }
        false
    }

    /// Makes `sender` the active connection. Returns the old active connection.
    fn set(&self, sender: EspHttpWsDetachedSender) -> Option<EspHttpWsDetachedSender> {
        let mut active = self.0.lock().unwrap();
        active.0 = active.0.wrapping_add(1);
        active.1.replace(sender)
    }

    /// Removes the connection `session` if it is the active connection. Call it only on the HTTP
    /// server task, where the socket number of a closed connection is not yet free.
    /// Returns true when the session was the active page.
    fn remove(&self, session: i32) -> bool {
        let mut active = self.0.lock().unwrap();
        let was_active = active.1.as_ref().is_some_and(|a| a.session() == session);
        if was_active {
            active.1 = None;
        }
        was_active
    }

    fn is_some(&self) -> bool {
        self.0.lock().unwrap().1.is_some()
    }
}

/// Starts the web server and the thread that sends the beats and the status to the page.
///
/// Returns the server and the sender for the beats: (device number, beat in the bar). Use
/// `try_send` for a beat. A send to a slow WiFi client can wait up to 5 s, and the beat receive
/// thread must not wait for it.
pub fn start(
    state: Arc<State>,
    settings: Arc<Mutex<Settings>>,
    #[cfg(feature = "dmx")] dmx_q: Arc<Queue<DmxMsg>>,
) -> anyhow::Result<(EspHttpServer<'static>, SyncSender<(u8, u8)>)> {
    // When all sockets are open, a new connection closes the socket that received data least
    // recently. The active WebSocket receives no data, so it is that socket. The page loads the
    // waveforms of all decks at the same time: 1 WebSocket and up to 4 requests. A waiting page
    // adds 1 WebSocket. Thus the limit is 6. CONFIG_LWIP_MAX_SOCKETS must have space for these sockets. The HTTP
    // server task used 5.9 KB of stack (GET /mem, 2026-10-23).
    //
    // TCP keepalive finds a page that left the network in about 20 s (5 s idle, then 3 probes
    // at 5 s). Without it, lwIP keeps the dead connection for minutes, and each send to it waits
    // up to 5 s.
    let mut server = EspHttpServer::new(&Configuration {
        stack_size: 8 * 1024,
        uri_match_wildcard: true,
        max_open_sockets: 6,
        keep_alive: Some(KeepAlive {
            idle_secs: 5,
            interval_secs: 5,
            probe_count: 3,
        }),
        ..Default::default()
    })?;
    let active = Arc::new(Active::default());

    let wave_state = state.clone();
    server.fn_handler("/waveform/*", Method::Get, move |req| {
        let deck = req
            .uri()
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<u8>().ok());
        let overview = deck.and_then(|n| {
            let tracks = wave_state.tracks.0.lock().unwrap();
            tracks.get(&n).and_then(|t| t.overview.clone())
        });
        match overview {
            Some(o) => {
                // A page from the computer of the developer reads the waveform too (see `dev_origin`).
                let headers = [
                    ("Content-Type", "application/json"),
                    ("Access-Control-Allow-Origin", "*"),
                ];
                let mut resp = req.into_response(200, None, &headers)?;
                write_waveform(&mut resp, &o)
            }
            None => req.into_status_response(404)?.write_all(b"no waveform"),
        }
    })?;

    server.fn_handler("/log", Method::Get, |req| {
        req.into_response(200, None, &[("Content-Type", "text/plain; charset=utf-8")])?
            .write_all(crate::logring::text().as_bytes())
    })?;

    server.fn_handler("/mem", Method::Get, |req| {
        req.into_response(200, None, &[("Content-Type", "text/plain; charset=utf-8")])?
            .write_all(crate::mem::report().as_bytes())
    })?;

    #[cfg(feature = "dmx")]
    show_routes(&mut server, &state)?;
    #[cfg(feature = "dmx")]
    ofl_routes(&mut server)?;

    for path in ["/", "/index.html"] {
        server.fn_handler(path, Method::Get, |req| {
            req.into_response(
                200,
                None,
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Content-Encoding", "gzip"),
                ],
            )?
            .write_all(INDEX_HTML_GZ)
        })?;
    }

    let midi_cfg = settings.lock().unwrap().midi.clone();
    let ws_active = active.clone();
    let ws_state = state.clone();
    let handle = move |ws: &mut EspHttpWsConnection| -> Result<(), EspError> {
        if let EspHttpWsConnection::New(_, req) = ws {
            let req = *req;
            let (origin, host) = (header(req, "Origin"), header(req, "Host"));
            // ESP-IDF sends the handshake reply before this handler runs, so the firmware
            // cannot send a 403. An error closes the connection before any data goes out.
            if !origin_allowed(origin.as_deref(), host.as_deref()) {
                log::warn!("WebSocket refused: Origin {origin:?} does not match Host {host:?}");
                return Err(EspError::from_infallible::<ESP_FAIL>());
            }
            let hello = json!({
                "type": "hello",
                "proto": 1,
                "version": env!("CARGO_PKG_VERSION"),
                "device": "esp32",
                "caps": CAPS,
            });
            ws.send(FrameType::Text(false), hello.to_string().as_bytes())?;
            // Only the HTTP server task sets the active page, so the check and the set cannot race.
            if ws_active.is_some() {
                return ws.send(FrameType::Text(false), br#"{"type":"busy"}"#);
            }
            ws_active.set(ws.create_detached_sender()?);
            let msg = settings.lock().unwrap().message();
            ws.send(FrameType::Text(false), msg.as_bytes())?;
            let msg = settings.lock().unwrap().scenes_message();
            return ws.send(FrameType::Text(false), msg.as_bytes());
        }
        if ws.is_closed() {
            // The active page can leave the network while it holds the strobe. It then does not send the release.
            if ws_active.remove(ws.session()) {
                release_strobe(&ws_state);
            }
            return Ok(());
        }

        let (kind, len) = ws.recv(&mut [])?;
        if !matches!(kind, FrameType::Text(_)) {
            return Ok(());
        }
        if len > MAX_COMMAND_LEN {
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_SIZE>());
        }
        let mut buf = vec![0u8; len];
        ws.recv(&mut buf)?;
        // For a text frame, esp-idf-svc adds 1 to the length and puts a NUL byte at the end.
        // serde_json refuses that byte, so remove it.
        let text = buf.strip_suffix(&[0]).unwrap_or(&buf);
        // A `Value` tree has a heap block for each field. The scenes are large, so they go
        // into their struct at once.
        #[derive(serde::Deserialize)]
        struct Kind<'a> {
            #[serde(rename = "type", borrow)]
            kind: Cow<'a, str>,
        }
        if serde_json::from_slice::<Kind>(text).is_ok_and(|k| k.kind == "scenes_set") {
            let reply = match serde_json::from_slice::<ScenesConfig>(text) {
                Ok(scenes) => {
                    let mut settings = settings.lock().unwrap();
                    settings.set_scenes(scenes);
                    settings.scenes_message()
                }
                Err(e) => error(&format!("The scenes are not valid: {e}")),
            };
            return ws.send(FrameType::Text(false), reply.as_bytes());
        }
        let Ok(cmd) = serde_json::from_slice::<Value>(text) else {
            return ws.send(
                FrameType::Text(false),
                error("The command is not valid JSON").as_bytes(),
            );
        };
        if cmd["type"] == "take_over" {
            take_over(ws, &ws_active, &ws_state)?;
            let msg = settings.lock().unwrap().message();
            ws.send(FrameType::Text(false), msg.as_bytes())?;
            let msg = settings.lock().unwrap().scenes_message();
            return ws.send(FrameType::Text(false), msg.as_bytes());
        }
        let reply = command(
            &cmd,
            &settings,
            &ws_state,
            #[cfg(feature = "dmx")]
            &dmx_q,
        );
        // A command without a reply gives an empty text.
        if reply.is_empty() {
            return Ok(());
        }
        ws.send(FrameType::Text(false), reply.as_bytes())
    };
    server.ws_handler("/ws", None, move |ws| -> Result<(), EspError> {
        if let Err(e) = handle(ws) {
            close_session(ws, e);
        }
        Ok(())
    })?;

    // This thread does all the sends to the page. When the queue is full, `try_send` drops the beat.
    let (beat_tx, beat_rx) = mpsc::sync_channel::<(u8, u8)>(8);
    crate::spawn(c"web-status", 6 * 1024, move || {
        let mut next_status = Instant::now();
        // The thread keeps one buffer for the status and one for the beat. Thus a message does
        // not allocate a new buffer.
        let mut status = Vec::with_capacity(4096);
        let mut beat_msg = String::with_capacity(48);
        loop {
            match beat_rx.recv_timeout(next_status.saturating_duration_since(Instant::now())) {
                Ok((device, beat)) => {
                    beat_msg.clear();
                    let _ = write!(
                        beat_msg,
                        r#"{{"type":"beat","device":{device},"beat":{beat}}}"#
                    );
                    if active.send(beat_msg.as_bytes()) {
                        release_strobe(&state);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if Instant::now() >= next_status {
                next_status = Instant::now() + STATUS_INTERVAL;
                // The OTA check and an OFL fetch take about 50 KB of heap for TLS. The status
                // message can then fail to allocate, so the page gets no status until they end.
                if active.is_some() && !ota::busy() && !ofl::busy() {
                    let clock_enabled = midi_cfg.lock().unwrap().clock_enabled;
                    status_message(&state, clock_enabled, &mut status);
                    if active.send(&status) {
                        release_strobe(&state);
                    }
                }
            }
        }
    })?;

    Ok((server, beat_tx))
}

/// Releases the strobe hold. The active page can leave the network while it holds the strobe,
/// and it cannot send the release.
fn release_strobe(_state: &State) {
    #[cfg(feature = "dmx")]
    _state
        .dmx_live
        .strobe_hold
        .store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Adds `GET /ofl/search?q=<text>` and `GET /ofl/fixture?key=<manufacturer>/<model>`.
///
/// The handler waits for the fetch of `ofl.rs`. Each step of the fetch can wait up to 10 s.
/// Meanwhile the HTTP server task serves no other request, and the page gets no beat and no
/// status. A page from the computer of the developer uses the two routes too (see `dev_origin`).
/// Thus each reply has `Access-Control-Allow-Origin: *`.
#[cfg(feature = "dmx")]
fn ofl_routes(server: &mut EspHttpServer<'static>) -> anyhow::Result<()> {
    type Fetch = fn(&str) -> Result<Vec<u8>, (u16, String)>;
    let routes: [(&str, &str, Fetch); 2] = [
        ("/ofl/search", "q", ofl::search),
        ("/ofl/fixture", "key", ofl::fixture),
    ];
    for (path, name, fetch) in routes {
        server.fn_handler(path, Method::Get, move |req| -> anyhow::Result<()> {
            let (origin, host) = (req.header("Origin"), req.header("Host"));
            let reply = if !origin_allowed(origin, host) {
                log::warn!("GET {path} refused: Origin {origin:?} does not match Host {host:?}");
                Err((
                    403,
                    "The Origin of the page does not match the Host.".to_string(),
                ))
            } else {
                match query_value(req.uri(), name) {
                    Some(value) => fetch(&value),
                    None => Err((400, format!("The request has no valid value for {name}."))),
                }
            };
            let cors = ("Access-Control-Allow-Origin", "*");
            match reply {
                Ok(body) => req
                    .into_response(200, None, &[("Content-Type", "application/json"), cors])?
                    .write_all(&body)?,
                Err((status, text)) => req
                    .into_response(
                        status,
                        None,
                        &[("Content-Type", "text/plain; charset=utf-8"), cors],
                    )?
                    .write_all(text.as_bytes())?,
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Returns the URL-decoded value of the query parameter `name` in `uri`. A `+` is a space.
///
/// Returns `None` when `uri` has no such parameter, when an escape is not `%` and two hex
/// digits, or when the text is not UTF-8.
#[cfg_attr(not(feature = "dmx"), allow(dead_code))]
fn query_value(uri: &str, name: &str) -> Option<String> {
    let (_, query) = uri.split_once('?')?;
    let raw = query
        .split('&')
        .find_map(|p| p.strip_prefix(name)?.strip_prefix('='))?;
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'+' => b' ',
            b'%' => {
                let hi = char::from(bytes.next()?).to_digit(16)?;
                let lo = char::from(bytes.next()?).to_digit(16)?;
                (hi * 16 + lo) as u8
            }
            b => b,
        });
    }
    String::from_utf8(out).ok()
}

/// Adds `GET /show` and `POST /show`.
///
/// A page from the computer of the developer uses the two routes too (see `dev_origin`). The page
/// sends the POST body as `text/plain`, so the browser sends no preflight request. Thus each
/// reply has `Access-Control-Allow-Origin: *`, and the route accepts each content type.
#[cfg(feature = "dmx")]
fn show_routes(server: &mut EspHttpServer<'static>, state: &Arc<State>) -> anyhow::Result<()> {
    const CORS: (&str, &str) = ("Access-Control-Allow-Origin", "*");

    let get_state = state.clone();
    server.fn_handler("/show", Method::Get, move |req| -> anyhow::Result<()> {
        // The `dmx` thread locks the show for each frame. A clone keeps the lock short.
        let show = get_state.show.lock().unwrap().clone();
        let mut resp =
            req.into_response(200, None, &[("Content-Type", "application/json"), CORS])?;
        // The buffer sends the JSON in chunks of 512 bytes. The full JSON is not on the heap.
        let mut w = std::io::BufWriter::with_capacity(512, StdWrite(&mut resp));
        serde_json::to_writer(&mut w, &show)?;
        std::io::Write::flush(&mut w)?;
        Ok(())
    })?;

    let post_state = state.clone();
    server.fn_handler(
        "/show",
        Method::Post,
        move |mut req| -> anyhow::Result<()> {
            let (status, text) = post_show(&mut req, &post_state);
            let headers = [("Content-Type", "text/plain; charset=utf-8"), CORS];
            req.into_response(status, None, &headers)?
                .write_all(text.as_bytes())?;
            Ok(())
        },
    )?;
    Ok(())
}

/// Reads, checks and stores the show of `POST /show`. Returns the status and the text of the reply.
#[cfg(feature = "dmx")]
fn post_show(req: &mut Request<&mut EspHttpConnection>, state: &State) -> (u16, String) {
    let (origin, host) = (req.header("Origin"), req.header("Host"));
    if !origin_allowed(origin, host) {
        log::warn!("POST /show refused: Origin {origin:?} does not match Host {host:?}");
        return (
            403,
            "The Origin of the page does not match the Host.".into(),
        );
    }
    let len = req
        .header("Content-Length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_SHOW_LEN {
        return (
            413,
            format!("The show has {len} bytes. The limit is {MAX_SHOW_LEN} bytes."),
        );
    }
    // The parsed show and the scratch buffer of serde_json for a long string need heap. A body
    // larger than half of the largest free block can fail to allocate, and the box restarts.
    // SAFETY: the heap call only reads counters.
    let block = unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT) };
    if len > block / 2 {
        log::warn!("POST /show refused: {len} bytes, largest free block {block} bytes");
        return (503, "Not enough memory for the show. Try again.".into());
    }
    // serde_json reads the body in small blocks. A full copy of the body is not on the heap.
    let body = std::io::BufReader::with_capacity(512, StdRead(&mut *req));
    let mut show: autoblink_core_dmx::Show = match serde_json::from_reader(body) {
        Ok(show) => show,
        Err(e) => return (400, format!("The show is not valid: {e}")),
    };
    show.sanitize();
    // The flash erase and write take time. The firmware holds only the store lock for them, so
    // the `dmx` thread does not stop.
    let saved = match state.show_store.lock().unwrap().as_mut() {
        Some(store) => store.save(&show),
        None => {
            return (
                500,
                "The box has no show partition. Flash the box over USB to add it.".into(),
            )
        }
    };
    if let Err(e) = saved {
        log::warn!("The firmware cannot save the show: {e:#}");
        return (500, format!("The firmware cannot save the show: {e:#}"));
    }
    *state.show.lock().unwrap() = show;
    log::info!("The page saved a new DMX show");
    (204, String::new())
}

/// Gives an `esp_idf_svc::io::Read` to serde_json as a `std::io::Read`.
#[cfg(feature = "dmx")]
struct StdRead<R>(R);

#[cfg(feature = "dmx")]
impl<R: esp_idf_svc::io::Read> std::io::Read for StdRead<R>
where
    R::Error: std::error::Error + Send + Sync + 'static,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf).map_err(std::io::Error::other)
    }
}

/// Gives an `esp_idf_svc::io::Write` to serde_json as a `std::io::Write`.
#[cfg(feature = "dmx")]
struct StdWrite<W>(W);

#[cfg(feature = "dmx")]
impl<W: Write> std::io::Write for StdWrite<W>
where
    W::Error: std::error::Error + Send + Sync + 'static,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(buf).map_err(std::io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writes the waveform route body in the host format (`host/src/web/mod.rs`).
///
/// The preview has about 3600 bytes. As a `serde_json::Value`, it needs a large heap block. The
/// heap of the ESP32 often does not have that block. Thus the function writes the preview as text
/// in small chunks, with one buffer for all chunks.
fn write_waveform<W: Write>(w: &mut W, o: &TrackOverview) -> Result<(), W::Error> {
    #[derive(Serialize)]
    struct CueJson {
        hot: u8,
        start: u32,
        end: Option<u32>,
    }
    #[derive(Serialize)]
    struct SpanJson {
        kind: Disp<PhraseKind>,
        start: u32,
        end: u32,
        fill: Option<u32>,
    }

    let mut text = String::with_capacity(256 * 4);
    let _ = write!(text, "{{\"rekordbox_id\":{},\"preview\":[", o.rekordbox_id);
    for (i, chunk) in o.preview.chunks(256).enumerate() {
        for (k, b) in chunk.iter().enumerate() {
            let sep = if i > 0 || k > 0 { "," } else { "" };
            let _ = write!(text, "{sep}{b}");
        }
        w.write_all(text.as_bytes())?;
        text.clear();
    }
    // The response is not a `std::io::Write`, so serde_json writes the tail into a buffer first.
    let cues: Vec<CueJson> = o
        .cues
        .iter()
        .map(|c| CueJson {
            hot: c.hot_cue,
            start: c.start_ms,
            end: c.end_ms,
        })
        .collect();
    let phrases: Vec<SpanJson> = o
        .phrases
        .iter()
        .map(|p| SpanJson {
            kind: Disp(p.kind),
            start: p.start_ms,
            end: p.end_ms,
            fill: p.fill_ms,
        })
        .collect();
    let mut tail = br#"],"cues":"#.to_vec();
    let _ = serde_json::to_writer(&mut tail, &cues);
    tail.extend_from_slice(br#","phrases":"#);
    let _ = serde_json::to_writer(&mut tail, &phrases);
    tail.push(b'}');
    w.write_all(&tail)
}

/// Makes the page that sends this command the active page. Closes the connection of the old
/// active page.
fn take_over(ws: &mut EspHttpWsConnection, active: &Active, state: &State) -> Result<(), EspError> {
    let EspHttpWsConnection::Receiving(sd, _, _) = ws else {
        return Ok(());
    };
    let sd = *sd;
    let session = ws.session();
    let old = active.set(ws.create_detached_sender()?);
    if let Some(old) = old.map(|o| o.session()).filter(|&o| o != session) {
        log::info!("WebSocket {session} takes over the status from WebSocket {old}");
        // SAFETY: `sd` is the live server handle of this handler call. A closed `old` socket
        // gives only an error code.
        unsafe { sys::httpd_sess_trigger_close(sd, old) };
        // The page that loses control also loses its strobe hold.
        release_strobe(state);
    }
    Ok(())
}

/// The JSON text of an error reply.
fn error(text: &str) -> String {
    json!({"type": "error", "text": text}).to_string()
}

/// Runs one command from the page. Returns the reply as JSON text: the new settings, or an error.
/// Returns an empty text for a command without a reply.
fn command(
    cmd: &Value,
    settings: &Mutex<Settings>,
    state: &Arc<State>,
    #[cfg(feature = "dmx")] dmx_q: &Queue<DmxMsg>,
) -> String {
    match cmd["type"].as_str() {
        Some(kind @ ("set" | "cycle")) => {
            let Some(idx) = cmd["idx"].as_u64().map(|i| i as usize) else {
                return error("The command needs idx");
            };
            let mut settings = settings.lock().unwrap();
            let result = if kind == "set" {
                match cmd["value"].as_str() {
                    Some(value) => settings.set(idx, value.trim()),
                    None => Err("A set command needs a value".into()),
                }
            } else {
                settings.cycle(
                    idx,
                    if cmd["dir"].as_i64() == Some(-1) {
                        -1
                    } else {
                        1
                    },
                )
            };
            match result {
                Ok(()) => settings.message(),
                Err(text) => error(&text),
            }
        }
        // The update thread does the work. The page sees the result in the "ota" field of the status.
        Some(kind @ ("ota_check" | "ota_install")) => {
            let cmd = if kind == "ota_check" {
                OtaCmd::Check
            } else {
                OtaCmd::Install
            };
            // The reply is small on purpose. The TLS handshake of the OTA thread takes about
            // 40 KB of heap, and the full settings message can then fail to allocate.
            match ota::run(state.clone(), cmd) {
                Ok(()) => r#"{"type":"ota"}"#.into(),
                Err(text) => error(&text),
            }
        }
        Some("scene_test") => match cmd["preset"].as_u64().and_then(|p| u8::try_from(p).ok()) {
            // The MIDI thread sends the note. The reply is empty.
            Some(p) if p < u8::MAX => {
                state
                    .scene_test
                    .store(p, std::sync::atomic::Ordering::Relaxed);
                r#"{"type":"scene_test"}"#.into()
            }
            _ => error("The command needs a preset"),
        },
        Some("reboot") => match crate::restart_soon() {
            Ok(()) => {
                log::info!("The page asked for a restart");
                r#"{"type":"reboot"}"#.into()
            }
            Err(e) => error(&format!("The firmware cannot restart: {e}")),
        },
        // The `dmx` thread reads the live controls for each frame.
        #[cfg(feature = "dmx")]
        Some("dmx_live") => match (cmd["master"].as_f64(), cmd["strobe"].as_bool()) {
            (Some(master), Some(strobe)) => {
                use std::sync::atomic::Ordering::Relaxed;
                let live = &state.dmx_live;
                live.master.store(master.clamp(0.0, 255.0) as u8, Relaxed);
                live.strobe_hold.store(strobe, Relaxed);
                String::new()
            }
            _ => error("The command needs master and strobe"),
        },
        // A full queue drops the fog. The page can send it again.
        #[cfg(feature = "dmx")]
        Some("dmx_fog") => {
            let _ = dmx_q.send_back(DmxMsg::Fog, 0);
            String::new()
        }
        // A full queue drops the aim. The page sends the next one soon.
        #[cfg(feature = "dmx")]
        Some("dmx_aim") => match cmd["fixture"].as_u64().filter(|f| *f < 32) {
            Some(fixture) => {
                let axis = |key: &str| cmd[key].as_u64().unwrap_or(0).min(65535) as u16;
                let msg = DmxMsg::Aim {
                    fixture: fixture as u8,
                    pan: axis("pan"),
                    tilt: axis("tilt"),
                };
                let _ = dmx_q.send_back(msg, 0);
                String::new()
            }
            None => error("The command needs a fixture below 32"),
        },
        Some(other) => error(&format!("This device does not support the command {other}")),
        None => error("The command has no type"),
    }
}

/// The track fields of a deck in the status.
#[derive(Default, Serialize)]
struct TrackFields {
    wave: Option<u32>,
    track: Option<String>,
    phrase: Option<PhraseJson>,
    phrase_note: Option<String>,
}

/// Serializes a value as the text of its `Display`, without a `String` on the heap.
struct Disp<T>(T);

impl<T: Display> Serialize for Disp<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

/// The source of the track on a deck, for example "deck#2 USB (rb#207)".
struct Src {
    player: u8,
    slot: &'static str,
    id: u32,
}

impl Serialize for Src {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!(
            "deck#{} {} (rb#{})",
            self.player, self.slot, self.id
        ))
    }
}

#[derive(Serialize)]
struct PhraseJson {
    kind: Disp<PhraseKind>,
    progress: f64,
    fill: bool,
    beats_left: u32,
}

/// Builds the track fields like `host/src/web/status.rs`.
fn track_fields(d: &Deck, t: Option<&TrackData>) -> TrackFields {
    let Some(t) = t.filter(|t| t.rekordbox_id == d.track.rekordbox_id && t.rekordbox_id != 0)
    else {
        return TrackFields::default();
    };
    let mut f = TrackFields {
        wave: t.overview.as_ref().map(|o| o.rekordbox_id),
        ..Default::default()
    };
    if let Some(m) = &t.metadata {
        let mut parts: Vec<String> = [&m.artist, &m.title, &m.key]
            .into_iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect();
        if !m.title.is_empty() || !m.artist.is_empty() {
            if let Some(bpm) = m.bpm {
                parts.push(format!("{bpm:.0}bpm"));
            }
            f.track = Some(parts.join(" - "));
        }
    }
    match &t.structure {
        Some(Ok(ss)) => {
            f.phrase = d
                .beat_count
                .and_then(|bc| phrase_status(ss, bc))
                .map(|p| PhraseJson {
                    kind: Disp(p.kind),
                    progress: p.progress,
                    fill: p.fill,
                    beats_left: p.beats_left,
                });
        }
        Some(Err(reason)) => f.phrase_note = Some(format!("phrase info unavailable: {reason}")),
        // The fetch is still in progress.
        None => f.phrase_note = Some("phrase info loading…".into()),
    }
    f
}

/// The status message, like `host/src/web/status.rs`.
///
/// The firmware serializes these structs in one pass. A `serde_json::Value` tree has a heap block
/// for each field and each text, and the many small blocks fragment the heap.
#[derive(Serialize)]
struct Status<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    source: Option<&'static str>,
    mode: &'static str,
    iface: String,
    master: MasterJson,
    no_devices: bool,
    decks: Vec<DeckJson<'a>>,
    link_peers: u8,
    link_active: bool,
    clock: ClockJson,
    output: OutputJson,
    scene: SceneStatus,
    mixer: Option<()>,
    ota: OtaStatus,
    system: SystemJson,
    /// `None` while the `dmx` thread holds the lock. The page then keeps the last value.
    #[cfg(feature = "dmx")]
    #[serde(skip_serializing_if = "Option::is_none")]
    dmx: Option<DmxStatus>,
}

#[derive(Serialize)]
struct MasterJson {
    device: u8,
    virtual_master: bool,
    bpm: f64,
    pitch: f64,
    beat: u8,
    phrase16: u8,
    playing: bool,
}

#[derive(Serialize)]
struct DeckJson<'a> {
    num: u8,
    name: Cow<'a, str>,
    playing: bool,
    master: bool,
    virtual_master: bool,
    on_air: bool,
    sync: bool,
    bpm: Option<f64>,
    pitch: f64,
    play_state: Option<Disp<PlayState>>,
    beat: Option<u8>,
    beat_count: Option<u32>,
    playhead_ms: Option<u32>,
    track_len_ms: Option<u32>,
    loop_ms: Option<[u32; 2]>,
    #[serde(flatten)]
    fields: TrackFields,
    src: Option<Src>,
}

#[derive(Serialize)]
struct ClockJson {
    out16: u8,
    state: &'static str,
    delta_ms: Option<f64>,
}

#[derive(Serialize)]
struct OutputJson {
    port: &'static str,
    connected: bool,
    dropped: u8,
    clock_enabled: bool,
    pulses: u64,
    notes: u64,
    ccs: u64,
    notes_recent: Vec<(u8, u32)>,
    ccs_recent: Vec<(u8, u8, u32)>,
    mtc: Option<()>,
    mtc_qf: u8,
    mtc_full: u8,
}

#[derive(Serialize)]
struct SystemJson {
    uptime_s: u64,
    reset_reason: &'static str,
    heap_free: u32,
    heap_min: u32,
}

/// Writes the status message into `out`. It takes each lock alone, never two at once, like the
/// host.
/// The output history keeps an entry in the status for this time after it was sent.
const HISTORY_MS: u32 = 3000;

/// Gives the age of an entry in ms, or `None` when it is older than `HISTORY_MS`.
fn age_ms(t: Instant) -> Option<u32> {
    let ms = u32::try_from(t.elapsed().as_millis()).ok()?;
    (ms < HISTORY_MS).then_some(ms)
}

fn status_message(state: &State, clock_enabled: bool, out: &mut Vec<u8>) {
    let now = state.now_ms();
    let (decks, master_num, master_virtual) = {
        let tracker = state.tracker.lock().unwrap();
        (
            tracker.decks().to_vec(),
            tracker.master().map(|m| m.num),
            tracker.master_is_virtual(),
        )
    };
    let fields: Vec<TrackFields> = {
        let tracks = state.tracks.0.lock().unwrap();
        decks
            .iter()
            .map(|d| track_fields(d, tracks.get(&d.num)))
            .collect()
    };
    let master = master_num.and_then(|n| decks.iter().find(|d| d.num == n));
    let deck_json: Vec<DeckJson> = decks
        .iter()
        .zip(fields)
        .map(|(d, fields)| {
            let is_master = master_num == Some(d.num);
            let slot = match d.track.slot {
                1 => "CD",
                2 => "SD",
                3 => "USB",
                4 => "rekordbox",
                _ => "?",
            };
            DeckJson {
                num: d.num,
                name: if d.name.is_empty() {
                    Cow::Owned(format!("Deck {}", d.num))
                } else {
                    Cow::Borrowed(&d.name)
                },
                playing: d.is_playing(now),
                master: is_master,
                virtual_master: is_master && master_virtual,
                on_air: d.on_air,
                sync: d.sync,
                bpm: (d.bpm > 0.0).then_some(d.bpm),
                pitch: d.pitch,
                play_state: d.play_state.map(Disp),
                beat: (1..=4).contains(&d.beat).then_some(d.beat),
                beat_count: d.beat_count,
                playhead_ms: d.playhead_ms,
                track_len_ms: d.track_len_ms,
                loop_ms: d.loop_ms.map(|(a, b)| [a, b]),
                fields,
                src: (d.track.rekordbox_id != 0).then_some(Src {
                    player: d.track.source_player,
                    slot,
                    id: d.track.rekordbox_id,
                }),
            }
        })
        .collect();
    let (clock, output, scene) = {
        let midi = state.midi.lock().unwrap();
        let clock = ClockJson {
            out16: midi.out16,
            state: midi.clock_state,
            delta_ms: midi.delta_ms,
        };
        let output = OutputJson {
            port: "UART2 TX (IO17)",
            connected: true,
            dropped: 0,
            clock_enabled,
            pulses: midi.pulses,
            notes: midi.notes,
            ccs: midi.ccs,
            // Like the host: the history keeps an entry for 3 s, newest first.
            notes_recent: midi
                .recent_notes
                .iter()
                .filter_map(|&(n, t)| Some((n, age_ms(t)?)))
                .collect(),
            ccs_recent: midi
                .recent_ccs
                .iter()
                .filter_map(|&(n, v, t)| Some((n, v, age_ms(t)?)))
                .collect(),
            mtc: None,
            mtc_qf: 0,
            mtc_full: 0,
        };
        (clock, output, midi.scene)
    };
    let mut ota = state.ota.lock().unwrap().clone();
    if ota.state.is_empty() {
        ota.state = "idle";
    }
    // The `dmx` thread does not wait for this lock, so the status does not wait either.
    #[cfg(feature = "dmx")]
    let dmx = state.dmx_status.try_lock().ok().map(|s| s.clone());
    let status = Status {
        kind: "status",
        source: master.map(|_| "ProLink (network)"),
        mode: "ProLink (network)",
        iface: state.iface.lock().unwrap().clone(),
        master: MasterJson {
            device: master.map_or(0, |m| m.num),
            virtual_master: master.is_some() && master_virtual,
            bpm: master.map_or(0.0, |m| m.bpm),
            pitch: master.map_or(0.0, |m| m.pitch),
            beat: master.map_or(0, |m| m.beat),
            phrase16: master.map_or(0, |m| m.phrase16),
            playing: master.is_some_and(|m| m.is_playing(now)),
        },
        no_devices: deck_json.is_empty(),
        decks: deck_json,
        link_peers: 0,
        link_active: false,
        clock,
        output,
        scene,
        mixer: None,
        ota,
        // SAFETY: the two heap calls only read counters.
        system: SystemJson {
            uptime_s: now / 1000,
            reset_reason: state.reset_reason,
            heap_free: unsafe { sys::esp_get_free_heap_size() },
            heap_min: unsafe { sys::esp_get_minimum_free_heap_size() },
        },
        #[cfg(feature = "dmx")]
        dmx,
    };
    out.clear();
    // Serialization to a Vec fails only when the heap is full.
    let _ = serde_json::to_writer(out, &status);
}

/// Closes a WebSocket session after an error of the handler.
///
/// esp-idf-svc logs a handler error and keeps the socket open. The unread data then calls the
/// handler again every 30 ms, and each call logs the error again. Thus the firmware closes the
/// session itself.
fn close_session(ws: &EspHttpWsConnection, e: EspError) {
    let (EspHttpWsConnection::New(sd, _) | EspHttpWsConnection::Receiving(sd, _, _)) = ws else {
        return;
    };
    let fd = ws.session();
    log::warn!("WebSocket {fd}: the firmware closes the connection after an error: {e}");
    // SAFETY: `sd` is the live server handle of this handler call, and `fd` is its socket.
    unsafe { sys::httpd_sess_trigger_close(*sd, fd) };
}

fn header(req: *mut sys::httpd_req_t, name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    // SAFETY: `req` is the live handshake request of this handler call.
    let len = unsafe { sys::httpd_req_get_hdr_value_len(req, name.as_ptr()) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len + 1];
    let res = unsafe {
        sys::httpd_req_get_hdr_value_str(req, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
    };
    if res != sys::ESP_OK {
        return None;
    }
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

/// The same rule as the host: no Origin passes. Otherwise the Origin must name the Host, and the
/// Host must be an IP address, `localhost` or a `.local` name. A public name can resolve to this
/// device (DNS rebinding), so it does not pass.
/// Returns true for a page from `localhost` or `127.0.0.1`. A developer opens `webui/index.html`
/// from the computer with `?box=<address>`, and the page then needs no flash for a change. Only
/// a program on the computer of the user can serve such a page.
fn dev_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .map(|rest| rest.trim_end_matches('/'))
        .map(|rest| rest.rsplit_once(':').map_or(rest, |(name, _)| name))
        .is_some_and(|name| name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1")
}

fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    if dev_origin(origin) {
        return true;
    }
    let Some(host) = host else {
        return false;
    };
    let same = origin
        .split_once("://")
        .is_some_and(|(_, rest)| rest.trim_end_matches('/').eq_ignore_ascii_case(host));
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    }
    .to_ascii_lowercase();
    same && (name.parse::<std::net::IpAddr>().is_ok()
        || name == "localhost"
        || name.ends_with(".local"))
}
