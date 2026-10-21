//! Serves `webui/index.html` and speaks WebSocket protocol v1 (see `host/src/web/mod.rs`).
//!
//! The firmware sends the capabilities `beat`, `decks` and `settings`. The page hides the
//! sections of the other capabilities.

use std::ffi::CString;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use esp_idf_svc::http::server::ws::{EspHttpWsConnection, EspHttpWsDetachedSender};
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::sys::{self, EspError, ESP_ERR_INVALID_SIZE, ESP_FAIL};
use esp_idf_svc::ws::FrameType;
use serde_json::{json, Value};

use crate::settings::Settings;
use crate::State;

const INDEX_HTML: &str = include_str!("../../webui/index.html");

/// The firmware ignores a command that is longer than this.
const MAX_COMMAND_LEN: usize = 512;

/// The open WebSocket connections. A send that fails removes the connection.
#[derive(Default)]
pub struct Clients(Mutex<Vec<EspHttpWsDetachedSender>>);

impl Clients {
    pub fn broadcast(&self, text: &str) {
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

pub fn beat_message(device: u8, beat: u8) -> String {
    json!({"type": "beat", "device": device, "beat": beat}).to_string()
}

/// Starts the web server and the thread that sends the status 10 times a second.
pub fn start(
    state: Arc<State>,
    clients: Arc<Clients>,
    settings: Arc<Mutex<Settings>>,
) -> anyhow::Result<EspHttpServer<'static>> {
    let mut server = EspHttpServer::new(&Configuration {
        stack_size: 10 * 1024,
        ..Default::default()
    })?;

    for path in ["/", "/index.html"] {
        server.fn_handler(path, Method::Get, |req| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8")])?
                .write_all(INDEX_HTML.as_bytes())
        })?;
    }

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
                "caps": ["beat", "decks", "settings"],
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
        let reply = command(&buf, &settings);
        ws.send(FrameType::Text(false), reply.to_string().as_bytes())
    })?;

    thread::Builder::new()
        .name("web-status".into())
        .stack_size(8 * 1024)
        .spawn(move || loop {
            thread::sleep(Duration::from_millis(100));
            let now = state.now_ms();
            state.tracker.lock().unwrap().expire(now);
            if !clients.is_empty() {
                clients.broadcast(&status_message(&state).to_string());
            }
        })?;

    Ok(server)
}

/// Runs one command from the page. Returns the reply: the new settings, or an error.
fn command(buf: &[u8], settings: &Mutex<Settings>) -> Value {
    let error = |text: &str| json!({"type": "error", "text": text});
    let Ok(cmd) = serde_json::from_slice::<Value>(buf) else {
        return error("The command is not valid JSON");
    };
    match cmd["type"].as_str() {
        Some("set") => {
            let (Some(idx), Some(value)) = (cmd["idx"].as_u64(), cmd["value"].as_str()) else {
                return error("A set command needs idx and value");
            };
            let settings = settings.lock().unwrap();
            match settings.set(idx as usize, value.trim()) {
                Ok(()) => settings.message(),
                Err(text) => error(&text),
            }
        }
        Some(other) => error(&format!("This device does not support the command {other}")),
        None => error("The command has no type"),
    }
}

fn status_message(state: &State) -> Value {
    let now = state.now_ms();
    let tracker = state.tracker.lock().unwrap();
    let master = tracker.master();
    let decks: Vec<Value> = tracker
        .decks()
        .iter()
        .map(|d| {
            json!({
                "num": d.num,
                "name": if d.name.is_empty() { format!("Deck {}", d.num) } else { d.name.clone() },
                "playing": d.is_playing(now),
                "master": master.is_some_and(|m| m.num == d.num),
                "virtual_master": false,
                "bpm": (d.bpm > 0.0).then_some(d.bpm),
                "pitch": d.pitch,
                "beat": (1..=4).contains(&d.beat).then_some(d.beat),
            })
        })
        .collect();
    json!({
        "type": "status",
        "source": master.map(|_| "ProLink (network)"),
        "mode": "ProLink (network)",
        "iface": state.iface.lock().unwrap().clone(),
        "master": {
            "device": master.map_or(0, |m| m.num),
            "virtual_master": false,
            "bpm": master.map_or(0.0, |m| m.bpm),
            "pitch": master.map_or(0.0, |m| m.pitch),
            "beat": master.map_or(0, |m| m.beat),
            "phrase16": master.map_or(0, |m| m.phrase16),
            "playing": master.is_some_and(|m| m.is_playing(now)),
        },
        "no_devices": decks.is_empty(),
        "decks": decks,
        "link_peers": 0,
        "link_active": false,
        "clock": null,
        "output": null,
        "mixer": null,
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
