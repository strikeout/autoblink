//! Serves `webui/index.html` and speaks WebSocket protocol v1 (see `host/src/web/mod.rs`).
//!
//! The firmware sends the capabilities `beat`, `decks`, `output`, `settings`, `waveform` and
//! `ota`. The page hides the sections of the other capabilities: `mixer`, `ports` and `logs`.

use std::ffi::CString;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::http::server::ws::{EspHttpWsConnection, EspHttpWsDetachedSender};
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::sys::{self, EspError, ESP_ERR_INVALID_SIZE, ESP_FAIL};
use esp_idf_svc::ws::FrameType;
use serde::Serialize;
use serde_json::{json, Value};

use crate::ota::{self, OtaCmd, OtaStatus};
use crate::settings::Settings;
use crate::tracks::TrackData;
use crate::State;
use xdj_core_dbserver::{phrase_status, TrackOverview};
use xdj_core_prolink::Deck;

const INDEX_HTML: &str = include_str!("../../webui/index.html");

/// The firmware ignores a command that is longer than this.
const MAX_COMMAND_LEN: usize = 512;

/// The page gets the status this often.
const STATUS_INTERVAL: Duration = Duration::from_millis(100);

/// The open WebSocket connections. A send that fails removes the connection.
#[derive(Default)]
struct Clients(Mutex<Vec<EspHttpWsDetachedSender>>);

impl Clients {
    fn broadcast(&self, text: &[u8]) {
        // A send waits for the HTTP server task, and that task takes this lock when a connection
        // closes. Send without the lock, so that the two tasks cannot block each other.
        let clients = self.0.lock().unwrap().clone();
        let failed: Vec<i32> = clients
            .into_iter()
            .filter_map(|mut c| c.send(FrameType::Text(false), text).err().map(|_| c.session()))
            .collect();
        if !failed.is_empty() {
            self.0.lock().unwrap().retain(|c| !failed.contains(&c.session()));
        }
    }

    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
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
) -> anyhow::Result<(EspHttpServer<'static>, SyncSender<(u8, u8)>)> {
    // With the default of 3 sockets, a fourth connection closes the oldest one, which is often a
    // live WebSocket. Each open socket takes heap, so 6 is the limit: 2 pages with a WebSocket
    // and some requests. CONFIG_LWIP_MAX_SOCKETS must have space for these sockets. The HTTP
    // server task used 5.9 KB of stack (GET /mem, 2026-10-23).
    let mut server = EspHttpServer::new(&Configuration {
        stack_size: 8 * 1024,
        uri_match_wildcard: true,
        max_open_sockets: 6,
        ..Default::default()
    })?;
    let clients = Arc::new(Clients::default());

    let wave_state = state.clone();
    server.fn_handler("/waveform/*", Method::Get, move |req| {
        let deck = req.uri().rsplit('/').next().and_then(|n| n.parse::<u8>().ok());
        let overview = deck.and_then(|n| {
            let tracks = wave_state.tracks.0.lock().unwrap();
            tracks.get(&n).and_then(|t| t.overview.clone())
        });
        match overview {
            Some(o) => {
                let mut resp = req.into_response(200, None, &[("Content-Type", "application/json")])?;
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

    for path in ["/", "/index.html"] {
        server.fn_handler(path, Method::Get, |req| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8")])?
                .write_all(INDEX_HTML.as_bytes())
        })?;
    }

    let midi_cfg = settings.lock().unwrap().midi.clone();
    let ws_clients = clients.clone();
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
                "caps": ["beat", "decks", "output", "settings", "waveform", "ota"],
            });
            ws.send(FrameType::Text(false), hello.to_string().as_bytes())?;
            let msg = settings.lock().unwrap().message();
            ws.send(FrameType::Text(false), msg.as_bytes())?;
            ws_clients.0.lock().unwrap().push(ws.create_detached_sender()?);
            return Ok(());
        }
        if ws.is_closed() {
            let session = ws.session();
            ws_clients.0.lock().unwrap().retain(|c| c.session() != session);
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
        let reply = command(text, &settings, &ws_state);
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
        // The thread keeps one buffer for the status, so that each status does not allocate it again.
        let mut status = Vec::with_capacity(4096);
        loop {
            match beat_rx.recv_timeout(next_status.saturating_duration_since(Instant::now())) {
                Ok((device, beat)) => clients.broadcast(
                    format!(r#"{{"type":"beat","device":{device},"beat":{beat}}}"#).as_bytes(),
                ),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if Instant::now() >= next_status {
                next_status = Instant::now() + STATUS_INTERVAL;
                // The OTA check takes about 50 KB of heap for TLS. The status message can then
                // fail to allocate, so the page gets no status until the check ends.
                if !clients.is_empty() && !ota::busy() {
                    let clock_enabled = midi_cfg.lock().unwrap().clock_enabled;
                    status_message(&state, clock_enabled, &mut status);
                    clients.broadcast(&status);
                }
            }
        }
    })?;

    Ok((server, beat_tx))
}

/// Writes the waveform route body in the host format (`host/src/web/mod.rs`).
///
/// The preview has about 7200 bytes. As a `serde_json::Value`, it needs a block of more than
/// 115 KB, which the heap of the ESP32 often does not have. The function therefore writes the
/// preview as text in small chunks.
fn write_waveform<W: Write>(w: &mut W, o: &TrackOverview) -> Result<(), W::Error> {
    w.write_all(format!("{{\"rekordbox_id\":{},\"preview\":[", o.rekordbox_id).as_bytes())?;
    for (i, chunk) in o.preview.chunks(256).enumerate() {
        let mut text = String::with_capacity(chunk.len() * 4);
        for (k, b) in chunk.iter().enumerate() {
            if i > 0 || k > 0 {
                text.push(',');
            }
            text.push_str(&b.to_string());
        }
        w.write_all(text.as_bytes())?;
    }
    let cues: Vec<Value> = o
        .cues
        .iter()
        .map(|c| json!({"hot": c.hot_cue, "start": c.start_ms, "end": c.end_ms}))
        .collect();
    let phrases: Vec<Value> = o
        .phrases
        .iter()
        .map(|p| json!({"kind": p.kind.to_string(), "start": p.start_ms, "end": p.end_ms, "fill": p.fill_ms}))
        .collect();
    let tail = format!("],\"cues\":{},\"phrases\":{}}}", Value::from(cues), Value::from(phrases));
    w.write_all(tail.as_bytes())
}

/// Runs one command from the page. Returns the reply as JSON text: the new settings, or an error.
fn command(buf: &[u8], settings: &Mutex<Settings>, state: &Arc<State>) -> String {
    let error = |text: &str| json!({"type": "error", "text": text}).to_string();
    let Ok(cmd) = serde_json::from_slice::<Value>(buf) else {
        return error("The command is not valid JSON");
    };
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
                settings.cycle(idx, if cmd["dir"].as_i64() == Some(-1) { -1 } else { 1 })
            };
            match result {
                Ok(()) => settings.message(),
                Err(text) => error(&text),
            }
        }
        // The update thread does the work. The page sees the result in the "ota" field of the status.
        Some(kind @ ("ota_check" | "ota_install")) => {
            let cmd = if kind == "ota_check" { OtaCmd::Check } else { OtaCmd::Install };
            // The reply is small on purpose. The TLS handshake of the OTA thread takes about
            // 40 KB of heap, and the full settings message can then fail to allocate.
            match ota::run(state.clone(), cmd) {
                Ok(()) => r#"{"type":"ota"}"#.into(),
                Err(text) => error(&text),
            }
        }
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

#[derive(Serialize)]
struct PhraseJson {
    kind: String,
    progress: f64,
    fill: bool,
    beats_left: u32,
}

/// Builds the track fields like `host/src/web/status.rs`.
fn track_fields(d: &Deck, t: Option<&TrackData>) -> TrackFields {
    let Some(t) = t.filter(|t| t.rekordbox_id == d.track.rekordbox_id && t.rekordbox_id != 0) else {
        return TrackFields::default();
    };
    let mut f = TrackFields { wave: t.overview.as_ref().map(|o| o.rekordbox_id), ..Default::default() };
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
            f.phrase = d.beat_count.and_then(|bc| phrase_status(ss, bc)).map(|p| PhraseJson {
                kind: p.kind.to_string(),
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
struct Status {
    #[serde(rename = "type")]
    kind: &'static str,
    source: Option<&'static str>,
    mode: &'static str,
    iface: String,
    master: MasterJson,
    no_devices: bool,
    decks: Vec<DeckJson>,
    link_peers: u8,
    link_active: bool,
    clock: ClockJson,
    output: OutputJson,
    mixer: Option<()>,
    ota: OtaStatus,
    system: SystemJson,
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
struct DeckJson {
    num: u8,
    name: String,
    playing: bool,
    master: bool,
    virtual_master: bool,
    on_air: bool,
    sync: bool,
    bpm: Option<f64>,
    pitch: f64,
    play_state: Option<String>,
    beat: Option<u8>,
    beat_count: Option<u32>,
    playhead_ms: Option<u32>,
    track_len_ms: Option<u32>,
    loop_ms: Option<[u32; 2]>,
    #[serde(flatten)]
    fields: TrackFields,
    src: Option<String>,
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
    last_note: Option<u8>,
    last_cc: Option<[u8; 2]>,
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
fn status_message(state: &State, clock_enabled: bool, out: &mut Vec<u8>) {
    let now = state.now_ms();
    let (decks, master_num, master_virtual) = {
        let tracker = state.tracker.lock().unwrap();
        (tracker.decks().to_vec(), tracker.master().map(|m| m.num), tracker.master_is_virtual())
    };
    let fields: Vec<TrackFields> = {
        let tracks = state.tracks.0.lock().unwrap();
        decks.iter().map(|d| track_fields(d, tracks.get(&d.num))).collect()
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
                name: if d.name.is_empty() { format!("Deck {}", d.num) } else { d.name.clone() },
                playing: d.is_playing(now),
                master: is_master,
                virtual_master: is_master && master_virtual,
                on_air: d.on_air,
                sync: d.sync,
                bpm: (d.bpm > 0.0).then_some(d.bpm),
                pitch: d.pitch,
                play_state: d.play_state.map(|p| p.to_string()),
                beat: (1..=4).contains(&d.beat).then_some(d.beat),
                beat_count: d.beat_count,
                playhead_ms: d.playhead_ms,
                track_len_ms: d.track_len_ms,
                loop_ms: d.loop_ms.map(|(a, b)| [a, b]),
                fields,
                src: (d.track.rekordbox_id != 0).then(|| {
                    format!("deck#{} {} (rb#{})", d.track.source_player, slot, d.track.rekordbox_id)
                }),
            }
        })
        .collect();
    let (clock, output) = {
        let midi = state.midi.lock().unwrap();
        let clock = ClockJson { out16: midi.out16, state: midi.clock_state, delta_ms: midi.delta_ms };
        let output = OutputJson {
            port: "UART2 TX (IO17)",
            connected: true,
            dropped: 0,
            clock_enabled,
            pulses: midi.pulses,
            notes: midi.notes,
            ccs: midi.ccs,
            // Like the host: a note shows for 200 ms, a CC for 300 ms.
            last_note: midi.last_note.filter(|(_, t)| t.elapsed().as_millis() < 200).map(|(n, _)| n),
            last_cc: midi.last_cc.filter(|(_, _, t)| t.elapsed().as_millis() < 300).map(|(n, v, _)| [n, v]),
            mtc: None,
            mtc_qf: 0,
            mtc_full: 0,
        };
        (clock, output)
    };
    let mut ota = state.ota.lock().unwrap().clone();
    if ota.state.is_empty() {
        ota.state = "idle";
    }
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
        mixer: None,
        ota,
        // SAFETY: the two heap calls only read counters.
        system: SystemJson {
            uptime_s: now / 1000,
            reset_reason: state.reset_reason,
            heap_free: unsafe { sys::esp_get_free_heap_size() },
            heap_min: unsafe { sys::esp_get_minimum_free_heap_size() },
        },
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
fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
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
    same && (name.parse::<std::net::IpAddr>().is_ok() || name == "localhost" || name.ends_with(".local"))
}
