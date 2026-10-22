//! Serves `webui/index.html` and speaks WebSocket protocol v1 (see `host/src/web/mod.rs`).
//!
//! The firmware sends the capabilities `beat`, `decks`, `output`, `settings`, `waveform` and
//! `ota`. The page hides the sections of the other capabilities: `mixer`, `ports` and `logs`.

use std::ffi::CString;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::http::server::ws::{EspHttpWsConnection, EspHttpWsDetachedSender};
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::sys::{self, EspError, ESP_ERR_INVALID_SIZE, ESP_FAIL};
use esp_idf_svc::ws::FrameType;
use serde_json::{json, Value};

use crate::ota::OtaCmd;
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
    fn broadcast(&self, text: &str) {
        // A send waits for the HTTP server task, and that task takes this lock when a connection
        // closes. Send without the lock, so that the two tasks cannot block each other.
        let clients = self.0.lock().unwrap().clone();
        let failed: Vec<i32> = clients
            .into_iter()
            .filter_map(|mut c| c.send(FrameType::Text(false), text.as_bytes()).err().map(|_| c.session()))
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
    ota: SyncSender<OtaCmd>,
) -> anyhow::Result<(EspHttpServer<'static>, SyncSender<(u8, u8)>)> {
    // Without a limit, a fourth connection closes the oldest one, which is often a live
    // WebSocket. CONFIG_LWIP_MAX_SOCKETS must have space for these sockets.
    let mut server = EspHttpServer::new(&Configuration {
        stack_size: 10 * 1024,
        uri_match_wildcard: true,
        max_open_sockets: 10,
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

    for path in ["/", "/index.html"] {
        server.fn_handler(path, Method::Get, |req| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8")])?
                .write_all(INDEX_HTML.as_bytes())
        })?;
    }

    let midi_cfg = settings.lock().unwrap().midi.clone();
    let ws_clients = clients.clone();
    server.ws_handler("/ws", None, move |ws| -> Result<(), EspError> {
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
            let msg = settings.lock().unwrap().message().to_string();
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
        let reply = command(text, &settings, &ota);
        ws.send(FrameType::Text(false), reply.to_string().as_bytes())
    })?;

    // This thread does all the sends to the page. When the queue is full, `try_send` drops the beat.
    let (beat_tx, beat_rx) = mpsc::sync_channel::<(u8, u8)>(8);
    thread::Builder::new()
        .name("web-status".into())
        .stack_size(8 * 1024)
        .spawn(move || {
            let mut next_status = Instant::now();
            loop {
                match beat_rx.recv_timeout(next_status.saturating_duration_since(Instant::now())) {
                    Ok((device, beat)) => clients.broadcast(
                        &json!({"type": "beat", "device": device, "beat": beat}).to_string(),
                    ),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                if Instant::now() >= next_status {
                    next_status = Instant::now() + STATUS_INTERVAL;
                    if !clients.is_empty() {
                        let clock_enabled = midi_cfg.lock().unwrap().clock_enabled;
                        clients.broadcast(&status_message(&state, clock_enabled).to_string());
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

/// Runs one command from the page. Returns the reply: the new settings, or an error.
fn command(buf: &[u8], settings: &Mutex<Settings>, ota: &SyncSender<OtaCmd>) -> Value {
    let error = |text: &str| json!({"type": "error", "text": text});
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
            match ota.try_send(cmd) {
                Ok(()) => settings.lock().unwrap().message(),
                Err(_) => error("The firmware update is busy"),
            }
        }
        Some(other) => error(&format!("This device does not support the command {other}")),
        None => error("The command has no type"),
    }
}

/// The track fields of a deck in the status.
#[derive(Default)]
struct TrackFields {
    wave: Option<u32>,
    track: Option<String>,
    phrase: Option<Value>,
    phrase_note: Option<String>,
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
            f.phrase = d.beat_count.and_then(|bc| phrase_status(ss, bc)).map(|p| {
                json!({"kind": p.kind.to_string(), "progress": p.progress, "fill": p.fill,
                       "beats_left": p.beats_left})
            });
        }
        Some(Err(reason)) => f.phrase_note = Some(format!("phrase info unavailable: {reason}")),
        // The fetch is still in progress.
        None => f.phrase_note = Some("phrase info loading…".into()),
    }
    f
}

/// Builds the status message. It takes each lock alone, never two at once, like the host.
fn status_message(state: &State, clock_enabled: bool) -> Value {
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
    let deck_json: Vec<Value> = decks
        .iter()
        .zip(fields)
        .map(|(d, f)| {
            let is_master = master_num == Some(d.num);
            let slot = match d.track.slot {
                1 => "CD",
                2 => "SD",
                3 => "USB",
                4 => "rekordbox",
                _ => "?",
            };
            json!({
                "num": d.num,
                "name": if d.name.is_empty() { format!("Deck {}", d.num) } else { d.name.clone() },
                "playing": d.is_playing(now),
                "master": is_master,
                "virtual_master": is_master && master_virtual,
                "on_air": d.on_air,
                "sync": d.sync,
                "bpm": (d.bpm > 0.0).then_some(d.bpm),
                "pitch": d.pitch,
                "play_state": d.play_state.map(|p| p.to_string()),
                "beat": (1..=4).contains(&d.beat).then_some(d.beat),
                "beat_count": d.beat_count,
                "playhead_ms": d.playhead_ms,
                "track_len_ms": d.track_len_ms,
                "loop_ms": d.loop_ms.map(|(a, b)| [a, b]),
                "wave": f.wave,
                "track": f.track,
                "src": (d.track.rekordbox_id != 0).then(|| format!(
                    "deck#{} {} (rb#{})", d.track.source_player, slot, d.track.rekordbox_id)),
                "phrase": f.phrase,
                "phrase_note": f.phrase_note,
            })
        })
        .collect();
    let midi = state.midi.lock().unwrap();
    json!({
        "type": "status",
        "source": master.map(|_| "ProLink (network)"),
        "mode": "ProLink (network)",
        "iface": state.iface.lock().unwrap().clone(),
        "master": {
            "device": master.map_or(0, |m| m.num),
            "virtual_master": master.is_some() && master_virtual,
            "bpm": master.map_or(0.0, |m| m.bpm),
            "pitch": master.map_or(0.0, |m| m.pitch),
            "beat": master.map_or(0, |m| m.beat),
            "phrase16": master.map_or(0, |m| m.phrase16),
            "playing": master.is_some_and(|m| m.is_playing(now)),
        },
        "no_devices": deck_json.is_empty(),
        "decks": deck_json,
        "link_peers": 0,
        "link_active": false,
        "clock": {
            "out16": midi.out16,
            "state": midi.clock_state,
            "delta_ms": midi.delta_ms,
        },
        "output": {
            "port": "UART2 TX (IO17)",
            "connected": true,
            "dropped": 0,
            "clock_enabled": clock_enabled,
            "pulses": midi.pulses,
            "notes": midi.notes,
            "ccs": midi.ccs,
            // Like the host: a note shows for 200 ms, a CC for 300 ms.
            "last_note": midi.last_note.filter(|(_, t)| t.elapsed().as_millis() < 200).map(|(n, _)| n),
            "last_cc": midi.last_cc.filter(|(_, _, t)| t.elapsed().as_millis() < 300).map(|(n, v, _)| [n, v]),
            "mtc": null,
            "mtc_qf": 0,
            "mtc_full": 0,
        },
        "mixer": null,
        "ota": state.ota.lock().unwrap().json(),
        // SAFETY: the two heap calls only read counters.
        "system": {
            "uptime_s": now / 1000,
            "reset_reason": state.reset_reason,
            "heap_free": unsafe { sys::esp_get_free_heap_size() },
            "heap_min": unsafe { sys::esp_get_minimum_free_heap_size() },
        },
    })
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
