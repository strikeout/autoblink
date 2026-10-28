//! Web UI: serves the shared dashboard page and streams the app state over one WebSocket.
//!
//! Protocol v1 on `GET /ws`. Each message is a JSON object with a `type` field.
//!
//! Server to client:
//! - `hello`: `{proto, version, device, caps}`. The page shows only the panels in `caps`.
//! - `status`: a [`status::Status`], about 30 times per second.
//! - `beat`: `{device, beat}`, sent at the moment a beat arrives. Device 0 is Ableton Link.
//! - `settings`: `{items: [{idx, label, section, kind, value, raw, step, on}]}`, after each
//!   change. A note or CC setting has `on`, the state of its switch. Other settings have no `on`.
//! - `ports`: `{ports, active, connected}`, after each change.
//! - `log`: `{lines}`, the new log lines.
//! - `error`: `{text}`, when a command fails.
//! - `scenes`: the DMX scene settings (`autoblink_core_midi::scenes::ScenesConfig`), after each
//!   change. The `scene` field of the status names the current scene.
//! - `busy`: `{}`, only from the ESP32. The ESP32 sends the status to one page at a time, and
//!   another page has it. The page then shows a button that sends `take_over`.
//!
//! Client to server:
//! - `{type: "cycle", idx, dir}` steps a toggle, a cycle or a fixed-step setting. On a setting
//!   with `on`, it flips the switch.
//! - `{type: "set", idx, value}` stores a numeric or text setting.
//! - `{type: "port", name}` switches the MIDI output port.
//! - `{type: "scenes_set", ...}` stores the full DMX scene settings. The server makes them
//!   valid first (`ScenesConfig::sanitize`).
//! - `{type: "scene_test", preset}` sends the note of a preset (0-based) one time.
//! - `{type: "rescan"}` lists the MIDI ports and network interfaces again.
//! - `{type: "take_over"}`, only to the ESP32: this page gets the status. The ESP32 closes the
//!   connection of the page that had it, and replies with `settings`.
//!
//! HTTP, for the `waveform` capability:
//! - `GET /waveform/{deck}` gives `{rekordbox_id, preview, cues, phrases}` of the loaded track,
//!   or 404. `preview` is the PWV4 colour preview, 6 bytes per column. Each cue is
//!   `{hot, start, end}` in ms; `hot` 0 is a memory cue, and `end` is set for a loop. Each
//!   phrase is `{kind, start, end, fill}` in ms; `fill` is the start of the fill-in, or null.
//!   The status field `wave` of a deck names the rekordbox ID that this route gives.
//!
//! HTTP, for the DMX show of the host:
//! - `GET /show` gives the show as JSON (`autoblink_core_dmx::Show`).
//! - `POST /show` replaces the show and writes it to `show.json` next to the config file. The
//!   body is JSON, with any content type, so that the page can send a CORS simple request. The
//!   reply is 204, 400 for a bad show, 403 for a foreign `Origin`, or 413 above `MAX_SHOW_LEN`.
//!   The server keeps only the first entries of a long list (`Show::sanitize`).
//! - The two replies have `Access-Control-Allow-Origin: *`, as on the box.
//! - The `hello` has the capability `dmx` only when the config sets `[dmx] artnet_target`.
//! - The status field `box` gives the IP address of an AUTOBLINK box on the network, or null.

pub mod status;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch, Notify};

use crate::config::SharedConfig;
use crate::logbuf::LogBuffer;
use crate::midi::activity::MidiActivity;
use crate::midi::MidiOutHandle;
use crate::prolink::beat_listener::BeatEvent;
use crate::prolink::discovery::DeviceTable;
use crate::settings::{self, NetworkIfaceInfo, SettingKind, SETTINGS};
use crate::state::SharedState;
use autoblink_core_midi::scenes::ScenesConfig;

/// The page. The host, the emulator and the ESP32 serve the same file.
pub const PAGE: &str = include_str!("../../../webui/index.html");

/// The number of ports to try, from the configured port upwards.
const PORT_TRIES: u16 = 20;

/// The interval of the status messages (about 30 per second).
const STATUS_INTERVAL: Duration = Duration::from_millis(33);

/// The interval of the MIDI port and network interface scan.
const RESCAN_INTERVAL: Duration = Duration::from_secs(3);

/// The handles that the web UI reads and changes.
#[derive(Clone)]
pub struct WebCtx {
    pub dj_state: SharedState,
    pub cfg: SharedConfig,
    pub device_table: DeviceTable,
    pub midi_activity: Arc<Mutex<MidiActivity>>,
    pub log_buf: LogBuffer,
    pub beat_tx: broadcast::Sender<BeatEvent>,
    pub midi_out: MidiOutHandle,
    pub cfg_change_tx: watch::Sender<()>,
    /// The DMX show of the host. The DMX engine reads it.
    pub show: crate::dmx::SharedShow,
}

struct Shared {
    ctx: WebCtx,
    interfaces: RwLock<Vec<NetworkIfaceInfo>>,
    ports: RwLock<Vec<String>>,
    status_tx: watch::Sender<Arc<str>>,
    settings_tx: watch::Sender<Arc<str>>,
    ports_tx: watch::Sender<Arc<str>>,
    scenes_tx: watch::Sender<Arc<str>>,
    rescan: Notify,
}

/// Binds the web UI and starts it.
///
/// When the port is in use, the next port is tried, up to [`PORT_TRIES`] ports. When no port
/// is free, the app continues without the web UI: the MIDI clock must not stop for a UI.
/// Returns the bound address.
pub async fn spawn(ctx: WebCtx, addr: SocketAddr) -> Option<SocketAddr> {
    let Some(listener) = bind(addr).await else {
        tracing::error!(
            %addr,
            "The web UI is off: no free port in {}..{}",
            addr.port(),
            addr.port().saturating_add(PORT_TRIES - 1)
        );
        return None;
    };
    let local = listener.local_addr().ok()?;
    // The interface list is a blocking system call.
    tokio::task::spawn_blocking(move || log_urls(local));

    let shared = start(ctx);
    let app = router(shared);
    // The first run uses the bound listener. A restart binds the same address again.
    let mut first = Some(listener);
    crate::runtime::spawn_supervised("web UI server", move || {
        let app = app.clone();
        let listener = first.take();
        async move {
            let listener = match listener {
                Some(l) => l,
                None => match TcpListener::bind(local).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(%local, error = %e, "Cannot bind the web UI again");
                        return;
                    }
                },
            };
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "The web UI server stopped");
            }
        }
    });
    Some(local)
}

/// Binds `addr`, or the next free port above it.
async fn bind(addr: SocketAddr) -> Option<TcpListener> {
    for i in 0..PORT_TRIES {
        let port = addr.port().checked_add(i)?;
        match TcpListener::bind(SocketAddr::new(addr.ip(), port)).await {
            Ok(listener) => return Some(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                tracing::info!(port, "Web UI port in use; trying the next port");
            }
            Err(e) => {
                tracing::error!(%addr, error = %e, "Cannot bind the web UI");
                return None;
            }
        }
    }
    None
}

fn log_urls(local: SocketAddr) {
    let port = local.port();
    if local.ip().is_unspecified() {
        tracing::info!("Web UI: http://127.0.0.1:{port}");
        for iface in settings::list_interfaces() {
            tracing::info!("Web UI: http://{}:{port} ({})", iface.ip, iface.name);
        }
    } else {
        tracing::info!("Web UI: http://{local}");
    }
    if !local.ip().is_loopback() {
        tracing::warn!(
            "The web UI has no login. Anyone on the network can change the MIDI settings."
        );
    }
}

/// Starts the background tasks and returns the shared state for the router.
fn start(ctx: WebCtx) -> Arc<Shared> {
    let empty: Arc<str> = Arc::from("");
    let shared = Arc::new(Shared {
        ctx,
        interfaces: RwLock::new(Vec::new()),
        ports: RwLock::new(Vec::new()),
        status_tx: watch::channel(empty.clone()).0,
        settings_tx: watch::channel(empty.clone()).0,
        ports_tx: watch::channel(empty.clone()).0,
        scenes_tx: watch::channel(empty).0,
        rescan: Notify::new(),
    });
    let s = Arc::clone(&shared);
    crate::runtime::spawn_supervised("web UI scan", move || scan_loop(Arc::clone(&s)));
    let s = Arc::clone(&shared);
    crate::runtime::spawn_supervised("web UI status", move || produce_loop(Arc::clone(&s)));
    shared
}

fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/index.html", get(|| async { Html(PAGE) }))
        .route("/ws", get(ws_handler))
        .route("/waveform/{deck}", get(waveform_handler))
        .route(
            "/show",
            get(show_get)
                .post(show_post)
                .layer(DefaultBodyLimit::max(MAX_SHOW_LEN)),
        )
        .with_state(shared)
}

/// Lists the MIDI ports and the network interfaces every few seconds, and on request.
async fn scan_loop(shared: Arc<Shared>) {
    loop {
        // Both calls block on system APIs, so they run off the async workers.
        let scan = tokio::task::spawn_blocking(|| {
            (
                settings::list_interfaces(),
                crate::midi::ports::list_output_ports(),
            )
        })
        .await;
        if let Ok((interfaces, ports)) = scan {
            *shared.interfaces.write() = interfaces;
            *shared.ports.write() = ports;
        }
        tokio::select! {
            _ = tokio::time::sleep(RESCAN_INTERVAL) => {}
            _ = shared.rescan.notified() => {}
        }
    }
}

/// Builds the status, settings and ports messages, while at least one client is connected.
///
/// One task builds each message once for all clients. A settings or ports message is sent
/// only when it changed, so a change from another client or the MIDI reconnect loop also reaches
/// the page.
async fn produce_loop(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(STATUS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if shared.status_tx.receiver_count() == 0 {
            continue;
        }
        produce_once(&shared);
    }
}

fn produce_once(shared: &Shared) {
    let ctx = &shared.ctx;
    let interfaces = shared.interfaces.read().clone();

    let st = status::snapshot(
        &ctx.cfg,
        &ctx.dj_state,
        &ctx.device_table,
        &ctx.midi_activity,
        &interfaces,
        status::OutputLink {
            connected: ctx.midi_out.is_connected(),
            dropped: ctx.midi_out.dropped_messages(),
        },
    );
    shared.status_tx.send_replace(tagged("status", &st));

    let cfg = ctx.cfg.read().clone();
    let items: Vec<SettingItem> = SETTINGS
        .iter()
        .enumerate()
        .map(|(idx, def)| SettingItem {
            idx,
            label: def.label,
            section: def.section,
            kind: match def.kind {
                SettingKind::Toggle => "toggle",
                SettingKind::CycleInterface | SettingKind::CycleSource => "cycle",
                SettingKind::NumericU8 | SettingKind::NumericU64 | SettingKind::NumericI64 => {
                    "number"
                }
                SettingKind::Text => "text",
            },
            value: settings::get_value(&cfg, &interfaces, idx),
            raw: settings::numeric_edit_value(&cfg, idx),
            step: settings::has_step(idx),
            on: settings::switch_value(&cfg, idx),
        })
        .collect();
    send_if_changed(
        &shared.settings_tx,
        tagged("settings", &json!({ "items": items })),
    );

    send_if_changed(&shared.scenes_tx, tagged("scenes", &cfg.scenes));

    let ports = shared.ports.read().clone();
    send_if_changed(
        &shared.ports_tx,
        tagged(
            "ports",
            &json!({
                "ports": ports,
                "active": cfg.midi.output,
                "connected": ctx.midi_out.is_connected(),
            }),
        ),
    );
}

#[derive(Serialize)]
struct SettingItem {
    idx: usize,
    label: &'static str,
    section: Option<&'static str>,
    kind: &'static str,
    value: String,
    raw: Option<String>,
    /// True when `cycle` steps this numeric setting up or down.
    step: bool,
    /// The switch of a note or CC setting. When false, the app does not send the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    on: Option<bool>,
}

#[derive(Serialize)]
struct Tagged<'a, T> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(flatten)]
    body: &'a T,
}

fn tagged<T: Serialize>(kind: &str, body: &T) -> Arc<str> {
    serde_json::to_string(&Tagged { kind, body })
        .unwrap_or_default()
        .into()
}

fn send_if_changed(tx: &watch::Sender<Arc<str>>, msg: Arc<str>) {
    tx.send_if_modified(|cur| {
        if *cur == msg {
            false
        } else {
            *cur = msg;
            true
        }
    });
}

#[derive(Serialize)]
struct WaveformBody<'a> {
    rekordbox_id: u32,
    preview: &'a [u8],
    cues: Vec<CueBody>,
    phrases: Vec<PhraseBody>,
}

#[derive(Serialize)]
struct PhraseBody {
    kind: String,
    start: u32,
    end: u32,
    fill: Option<u32>,
}

#[derive(Serialize)]
struct CueBody {
    hot: u8,
    start: u32,
    end: Option<u32>,
}

/// Gives the waveform preview and the cues of a deck. See the module documentation.
async fn waveform_handler(Path(deck): Path<u8>, State(shared): State<Arc<Shared>>) -> Response {
    // Clone the Arc under the lock, and serialize after the lock is released.
    let overview = shared
        .ctx
        .dj_state
        .read()
        .devices
        .get(&deck)
        .and_then(|d| d.overview.clone());
    let Some(o) = overview else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = WaveformBody {
        rekordbox_id: o.rekordbox_id,
        preview: &o.preview,
        cues: o
            .cues
            .iter()
            .map(|c| CueBody {
                hot: c.hot_cue,
                start: c.start_ms,
                end: c.end_ms,
            })
            .collect(),
        phrases: o
            .phrases
            .iter()
            .map(|p| PhraseBody {
                kind: p.kind.to_string(),
                start: p.start_ms,
                end: p.end_ms,
                fill: p.fill_ms,
            })
            .collect(),
    };
    match serde_json::to_string(&body) {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `POST /show` refuses a longer body. The box uses the same limit.
const MAX_SHOW_LEN: usize = 60_000;

/// Lets a page from another origin read the replies of `/show`, as on the box.
const CORS: (header::HeaderName, &str) = (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");

/// Gives the DMX show of the host. See the module documentation.
async fn show_get(State(shared): State<Arc<Shared>>) -> Response {
    // Clone the show under the lock. Serialize the clone after the statement releases the lock.
    let show = shared.ctx.show.read().clone();
    match serde_json::to_string(&show) {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json"), CORS], json).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Replaces and stores the DMX show of the host. See the module documentation.
async fn show_post(headers: HeaderMap, State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let reply = |status: StatusCode, text: String| (status, [CORS], text).into_response();
    if !origin_allowed(&headers) {
        tracing::warn!(
            origin = ?headers.get(header::ORIGIN),
            "Web UI: refused a POST /show from a foreign origin"
        );
        return reply(
            StatusCode::FORBIDDEN,
            "The Origin of the page does not match the Host.".into(),
        );
    }
    // The show has bounded lists, so a long list uses no memory before `sanitize` cuts it.
    let mut show: autoblink_core_dmx::Show = match serde_json::from_slice(&body) {
        Ok(show) => show,
        Err(e) => {
            return reply(
                StatusCode::BAD_REQUEST,
                format!("The show is not valid: {e}"),
            )
        }
    };
    show.sanitize();
    let path = shared.ctx.cfg.read().show_path.clone();
    if let Some(path) = path {
        let copy = show.clone();
        // The file write is a blocking call.
        let saved =
            tokio::task::spawn_blocking(move || crate::config::save_show(&path, &copy)).await;
        let err = match saved {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(e) = err {
            tracing::warn!("The host cannot store the show: {e}");
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The host cannot store the show: {e}"),
            );
        }
    }
    *shared.ctx.show.write() = show;
    tracing::info!("The page saved a new DMX show");
    reply(StatusCode::NO_CONTENT, String::new())
}

/// Returns true when the request may open the socket.
///
/// Browsers do not apply CORS to WebSockets. Without this check, any page in a browser on the
/// network could open the socket and change the settings. A request without `Origin` does not
/// come from a browser page, so it passes.
///
/// The `Origin` must name the same host as `Host`. Also, `Host` must be an IP address,
/// `localhost` or a `.local` name. A public domain name can resolve to this computer (DNS
/// rebinding); then the origin and the host agree, but the page comes from the internet.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(|h| h.to_str()),
    ) else {
        return false;
    };
    let same = origin
        .split_once("://")
        .is_some_and(|(_, rest)| rest.trim_end_matches('/').eq_ignore_ascii_case(host));
    same && host_is_local(host)
}

/// Returns true when `host` (a `Host` header, with or without a port) is an IP address,
/// `localhost`, or an mDNS `.local` name.
fn host_is_local(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // "[::1]:11111"
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    let name = name.to_ascii_lowercase();
    name.parse::<std::net::IpAddr>().is_ok() || name == "localhost" || name.ends_with(".local")
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(shared): State<Arc<Shared>>,
) -> Response {
    if !origin_allowed(&headers) {
        tracing::warn!(
            origin = ?headers.get(header::ORIGIN),
            "Web UI: refused a WebSocket from a foreign origin"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    // A command has less than 1 KB. Without a limit, axum buffers a 64 MiB frame before the
    // command check refuses it.
    ws.max_message_size(MAX_MESSAGE_LEN)
        .max_frame_size(MAX_MESSAGE_LEN)
        .on_upgrade(move |socket| client(socket, shared))
}

/// The largest WebSocket message that the server accepts from a client.
const MAX_MESSAGE_LEN: usize = 16 * 1024;

/// A send to a client that takes longer ends the connection.
///
/// A phone that leaves the WiFi sends no FIN. Without a timeout, its task waits for the TCP
/// retransmit timeout of the kernel, which can be 15 minutes. The page connects again by itself.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Command {
    Cycle {
        idx: usize,
        dir: i8,
    },
    Set {
        idx: usize,
        value: String,
    },
    Port {
        name: String,
        #[serde(default)]
        index: Option<usize>,
    },
    Rescan,
    #[serde(rename = "scenes_set")]
    ScenesSet(ScenesConfig),
    #[serde(rename = "scene_test")]
    SceneTest {
        preset: u8,
    },
}

async fn send(socket: &mut WebSocket, msg: &str) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Text(msg.into()))).await,
        Ok(Ok(()))
    )
}

async fn client(mut socket: WebSocket, shared: Arc<Shared>) {
    let mut caps = vec![
        "beat", "decks", "mixer", "output", "settings", "ports", "logs", "waveform", "scenes",
    ];
    // The page shows the DMX show editor only when the host has a DMX engine.
    if shared.ctx.cfg.read().dmx.artnet_target.is_some() {
        caps.push("dmx");
    }
    let hello = json!({
        "type": "hello",
        "proto": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "device": "host",
        "caps": caps,
    });
    if !send(&mut socket, &hello.to_string()).await {
        return;
    }

    let mut status_rx = shared.status_tx.subscribe();
    let mut settings_rx = shared.settings_tx.subscribe();
    let mut ports_rx = shared.ports_tx.subscribe();
    let mut scenes_rx = shared.scenes_tx.subscribe();
    let mut beat_rx = shared.ctx.beat_tx.subscribe();
    let mut log_seq = 0;

    // The producer skips its work while no client is connected, so build fresh messages now.
    produce_once(&shared);
    for rx in [&mut settings_rx, &mut ports_rx, &mut scenes_rx] {
        let msg = rx.borrow_and_update().clone();
        if !send(&mut socket, &msg).await {
            return;
        }
    }

    loop {
        let msg: Arc<str> = tokio::select! {
            r = status_rx.changed() => {
                if r.is_err() {
                    return;
                }
                let msg = status_rx.borrow_and_update().clone();
                let (lines, next) = shared.ctx.log_buf.since(log_seq);
                log_seq = next;
                if !lines.is_empty()
                    && !send(&mut socket, &json!({ "type": "log", "lines": lines }).to_string()).await
                {
                    return;
                }
                msg
            }
            r = settings_rx.changed() => {
                if r.is_err() {
                    return;
                }
                settings_rx.borrow_and_update().clone()
            }
            r = ports_rx.changed() => {
                if r.is_err() {
                    return;
                }
                ports_rx.borrow_and_update().clone()
            }
            r = scenes_rx.changed() => {
                if r.is_err() {
                    return;
                }
                scenes_rx.borrow_and_update().clone()
            }
            beat = beat_rx.recv() => {
                let (device, beat) = match beat {
                    Ok(BeatEvent::Beat { packet, .. }) => (packet.device_number, packet.beat_in_bar),
                    Ok(BeatEvent::LinkBeat { beat_in_bar, .. }) => (0, beat_in_bar),
                    Ok(BeatEvent::AbsPosition { .. }) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                json!({ "type": "beat", "device": device, "beat": beat }).to_string().into()
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(err) = handle_command(&shared, text.as_str()).await {
                            json!({ "type": "error", "text": err }).to_string().into()
                        } else {
                            continue;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                }
            }
        };
        if !send(&mut socket, &msg).await {
            return;
        }
    }
}

/// Runs one client command. Returns an error text for the page, or `None` on success.
async fn handle_command(shared: &Shared, text: &str) -> Option<String> {
    let ctx = &shared.ctx;
    let cmd = match serde_json::from_str::<Command>(text) {
        Ok(cmd) => cmd,
        Err(e) => return Some(format!("Unknown command: {e}")),
    };
    match cmd {
        Command::Cycle { idx, dir } => {
            let interfaces = shared.interfaces.read().clone();
            let dir = if dir < 0 { -1 } else { 1 };
            settings::cycle(&ctx.cfg, &interfaces, &ctx.cfg_change_tx, idx, dir);
            None
        }
        Command::Set { idx, value } => {
            if settings::set_numeric(&ctx.cfg, &ctx.cfg_change_tx, idx, value.trim()) {
                None
            } else {
                let label = SETTINGS.get(idx).map_or("setting", |s| s.label);
                Some(format!("{label}: {value:?} is not a valid value"))
            }
        }
        Command::Port { name, index } => {
            // With two ports of the same name, the config name cannot tell which one is open.
            let unique = shared.ports.read().iter().filter(|p| **p == name).count() <= 1;
            switch_port(ctx, name, index, unique).await
        }
        Command::Rescan => {
            shared.rescan.notify_one();
            None
        }
        Command::ScenesSet(mut scenes) => {
            scenes.sanitize();
            let path = {
                let mut cfg = ctx.cfg.write();
                cfg.scenes = scenes.clone();
                cfg.scenes_path.clone()
            };
            let path = path?;
            // The file write is a blocking call.
            let saved =
                tokio::task::spawn_blocking(move || crate::config::save_scenes(&path, &scenes))
                    .await;
            match saved {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("Cannot store the scenes: {e}")),
                Err(e) => Some(format!("Cannot store the scenes: {e}")),
            }
        }
        Command::SceneTest { preset } => {
            let (ch, note) = {
                let cfg = ctx.cfg.read();
                (cfg.scenes.channel, cfg.scenes.note(preset))
            };
            use crate::midi::MidiTransport;
            let _ = ctx.midi_out.send_message(&[0x90 | ch, note, 127]);
            // The test uses the 0x80 note off, not the velocity 0 of the scenes. A DMX controller
            // that ignores 0x80 then keeps the preset on, so that the user sees it.
            let _ = ctx.midi_out.send_message(&[0x80 | ch, note, 0]);
            None
        }
    }
}

async fn switch_port(
    ctx: &WebCtx,
    name: String,
    index: Option<usize>,
    unique: bool,
) -> Option<String> {
    let _port = ctx.midi_out.port_lock().lock().await;
    if unique && ctx.midi_out.is_connected() && ctx.cfg.read().midi.output == name {
        return None;
    }
    let wanted = name.clone();
    let opened =
        tokio::task::spawn_blocking(move || crate::midi::ports::open_output(&wanted, index)).await;
    match opened {
        Ok(Ok((conn, full))) => {
            // Send Stop on the old port before the worker drops it.
            ctx.midi_out
                .switch_connection(Some(Box::new(crate::midi::MidirOutConnection(conn))), true)
                .await;
            ctx.cfg.write().midi.output = full.clone();
            tracing::info!("Switched MIDI output to: {full}");
            None
        }
        Ok(Err(e)) => {
            tracing::error!("Failed to open MIDI port {name}: {e}");
            Some(format!("Cannot open MIDI port {name}: {e}"))
        }
        Err(e) => Some(format!("Cannot open MIDI port {name}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    use super::*;
    use crate::config::{new_shared, Config};

    fn ctx() -> WebCtx {
        WebCtx {
            dj_state: crate::state::new_shared(0),
            cfg: new_shared(Config::default()),
            device_table: Arc::new(Mutex::new(HashMap::new())),
            midi_activity: Arc::new(Mutex::new(MidiActivity::default())),
            log_buf: LogBuffer::new(),
            beat_tx: broadcast::channel(16).0,
            midi_out: MidiOutHandle::start(16, None),
            cfg_change_tx: watch::channel(()).0,
            show: crate::dmx::SharedShow::default(),
        }
    }

    /// Sends one HTTP/1.1 request and returns the full response.
    async fn http(addr: SocketAddr, head: &str, body: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!(
                "{head}\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        // The server can refuse the request before it reads the body.
        let _ = s.write_all(body).await;
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    type Ws = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn next_of_type(ws: &mut Ws, kind: &str) -> serde_json::Value {
        wait_for(ws, kind, |_| true).await
    }

    /// Reads messages until one of type `kind` passes `pred`. A settings message can be a
    /// stale one from before the command, so the tests wait for the value they expect.
    async fn wait_for(
        ws: &mut Ws,
        kind: &str,
        pred: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("a message before the timeout")
                .expect("an open socket")
                .expect("a valid frame");
            if let WsMessage::Text(text) = msg {
                let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                if v["type"] == kind && pred(&v) {
                    return v;
                }
            }
        }
    }

    #[tokio::test]
    async fn bind_takes_a_higher_port_when_the_port_is_in_use() {
        // Keep the taken port away from 65535, so that higher ports exist.
        let (taken, port) = loop {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            if port < u16::MAX - PORT_TRIES {
                break (l, port);
            }
        };
        let listener = bind(SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("a free port above the taken one");
        // Another test can take port + 1 first, so accept any port in the range.
        let got = listener.local_addr().unwrap().port();
        assert!(got > port && got < port + PORT_TRIES, "{got} after {port}");
        drop(taken);
    }

    #[test]
    fn origin_must_match_host() {
        let mut h = HeaderMap::new();
        assert!(origin_allowed(&h), "no Origin: not a browser page");
        h.insert(header::HOST, "192.168.1.5:11111".parse().unwrap());
        h.insert(header::ORIGIN, "http://192.168.1.5:11111".parse().unwrap());
        assert!(origin_allowed(&h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!origin_allowed(&h));
        h.insert(header::ORIGIN, "http://192.168.1.5:11112".parse().unwrap());
        assert!(!origin_allowed(&h));

        // DNS rebinding: a public name that resolves to this computer.
        h.insert(header::HOST, "evil.example:11111".parse().unwrap());
        h.insert(header::ORIGIN, "http://evil.example:11111".parse().unwrap());
        assert!(!origin_allowed(&h));

        for host in [
            "localhost:11111",
            "pi.local:11111",
            "[::1]:11111",
            "10.0.0.2",
        ] {
            h.insert(header::HOST, host.parse().unwrap());
            h.insert(header::ORIGIN, format!("http://{host}").parse().unwrap());
            assert!(origin_allowed(&h), "{host}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socket_streams_state_and_applies_validated_settings() {
        let ctx = ctx();
        let cfg = Arc::clone(&ctx.cfg);
        let beat_tx = ctx.beat_tx.clone();
        ctx.log_buf.push("  1.0s  INFO xdj: hello log".into());
        let addr = spawn(ctx, SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let hello = next_of_type(&mut ws, "hello").await;
        assert_eq!(hello["proto"], 1);
        let settings = next_of_type(&mut ws, "settings").await;
        assert_eq!(settings["items"][5]["label"], "BPM Smoothing");
        // The first status tick sends the retained log lines, then the status.
        let log = next_of_type(&mut ws, "log").await;
        assert_eq!(log["lines"][0], "  1.0s  INFO xdj: hello log");
        let status = next_of_type(&mut ws, "status").await;
        assert_eq!(status["no_devices"], true);

        // Index 5 is BPM smoothing, 0..=1000 ms.
        ws.send(WsMessage::Text(
            r#"{"type":"set","idx":5,"value":"20"}"#.into(),
        ))
        .await
        .unwrap();
        wait_for(&mut ws, "settings", |v| v["items"][5]["raw"] == "20").await;
        assert_eq!(cfg.read().midi.smoothing_ms, 20);

        ws.send(WsMessage::Text(
            r#"{"type":"set","idx":5,"value":"5000"}"#.into(),
        ))
        .await
        .unwrap();
        let err = next_of_type(&mut ws, "error").await;
        assert!(err["text"].as_str().unwrap().contains("BPM Smoothing"));
        assert_eq!(cfg.read().midi.smoothing_ms, 20);

        // Index 3 is the MIDI clock toggle.
        let before = cfg.read().midi.clock_enabled;
        let want = if before { "✗" } else { "✓" };
        ws.send(WsMessage::Text(
            r#"{"type":"cycle","idx":3,"dir":1}"#.into(),
        ))
        .await
        .unwrap();
        wait_for(&mut ws, "settings", |v| v["items"][3]["value"] == want).await;
        assert_eq!(cfg.read().midi.clock_enabled, !before);

        // An index outside the table changes nothing and does not panic.
        ws.send(WsMessage::Text(
            r#"{"type":"cycle","idx":999,"dir":1}"#.into(),
        ))
        .await
        .unwrap();

        beat_tx
            .send(BeatEvent::LinkBeat {
                bpm: 120.0,
                beat_in_bar: 2,
                bar_phase: 0.25,
                beat_phase: 0.0,
                received_at: std::time::Instant::now(),
            })
            .unwrap();
        let beat = next_of_type(&mut ws, "beat").await;
        assert_eq!(beat["device"], 0);
        assert_eq!(beat["beat"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scenes_set_stores_valid_scenes_in_the_file() {
        let ctx = ctx();
        let cfg = Arc::clone(&ctx.cfg);
        let dir = std::env::temp_dir().join(format!("xdj-scenes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scenes.json");
        cfg.write().scenes_path = Some(path.clone());
        let addr = spawn(ctx, SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let hello = next_of_type(&mut ws, "hello").await;
        assert!(hello["caps"].as_array().unwrap().contains(&"scenes".into()));
        let scenes = next_of_type(&mut ws, "scenes").await;
        assert_eq!(scenes["enabled"], false);

        ws.send(WsMessage::Text(
            r#"{"type":"scenes_set","enabled":true,"count":15,"labels":["Strobe"],
                "rows":[{"event":"drop","presets":[4,40],"gate":"hold"}]}"#
                .into(),
        ))
        .await
        .unwrap();
        wait_for(&mut ws, "scenes", |v| v["enabled"] == true).await;
        let stored = crate::config::load_scenes(&path);
        assert_eq!(stored.rows[0].presets, [4]);
        assert_eq!(cfg.read().scenes, stored);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn waveform_route_gives_the_overview_of_a_deck() {
        let ctx = ctx();
        let state = Arc::clone(&ctx.dj_state);
        let addr = spawn(ctx, SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let get = |path: &'static str| async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            s.write_all(
                format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        assert!(get("/waveform/1").await.starts_with("HTTP/1.1 404"));

        {
            let mut st = state.write();
            let mut d = crate::state::DeviceState::new(1);
            d.rekordbox_id = 207;
            st.devices.insert(1, d);
            st.set_overview(
                1,
                crate::state::TrackOverview {
                    rekordbox_id: 207,
                    preview: vec![1, 2, 3, 4, 5, 6],
                    cues: vec![crate::state::Cue {
                        hot_cue: 1,
                        start_ms: 500,
                        end_ms: Some(900),
                    }],
                    phrases: vec![crate::state::PhraseSpan {
                        kind: crate::state::PhraseKind::Intro,
                        start_ms: 0,
                        end_ms: 400,
                        fill_ms: None,
                    }],
                },
            );
        }
        let resp = get("/waveform/1").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let json: serde_json::Value =
            serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(json["rekordbox_id"], 207);
        assert_eq!(json["preview"][5], 6);
        assert_eq!(json["cues"][0]["end"], 900);
        assert_eq!(json["phrases"][0]["kind"], "INTRO");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socket_from_a_foreign_origin_is_refused() {
        let addr = spawn(ctx(), SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
        req.headers_mut()
            .insert("Origin", "https://evil.example".parse().unwrap());
        let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn show_routes_store_a_bounded_show() {
        let ctx = ctx();
        let shared_show = ctx.show.clone();
        let cfg = Arc::clone(&ctx.cfg);
        let dir = std::env::temp_dir().join(format!("xdj-show-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("show.json");
        cfg.write().show_path = Some(path.clone());
        let addr = spawn(ctx, SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();

        let resp = http(addr, "GET /show HTTP/1.1", b"").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(
            resp.to_ascii_lowercase()
                .contains("access-control-allow-origin: *"),
            "{resp}"
        );

        // 40 fixtures: the server keeps the first 32 (MAX_FIXTURES).
        let fixtures: Vec<_> = (0..40)
            .map(|i| json!({"name": format!("f{i}"), "address": 1, "channels": ["dimmer"]}))
            .collect();
        let body = json!({ "fixtures": fixtures }).to_string();
        let resp = http(addr, "POST /show HTTP/1.1", body.as_bytes()).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert_eq!(shared_show.read().fixtures.len(), 32);
        assert_eq!(crate::config::load_show(&path), *shared_show.read());

        let resp = http(addr, "GET /show HTTP/1.1", b"").await;
        let json: serde_json::Value =
            serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(json["fixtures"].as_array().unwrap().len(), 32);
        assert_eq!(json["fixtures"][31]["name"], "f31");

        // A bad body changes nothing.
        let resp = http(addr, "POST /show HTTP/1.1", b"{not json").await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        assert_eq!(shared_show.read().fixtures.len(), 32);

        let big = vec![b' '; MAX_SHOW_LEN + 1];
        let resp = http(addr, "POST /show HTTP/1.1", &big).await;
        assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");

        let resp = http(
            addr,
            "POST /show HTTP/1.1\r\nOrigin: https://evil.example",
            b"{}",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        assert_eq!(shared_show.read().fixtures.len(), 32);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hello_has_dmx_only_with_an_artnet_target() {
        for (target, want) in [(None, false), (Some("10.0.0.5".to_string()), true)] {
            let ctx = ctx();
            ctx.cfg.write().dmx.artnet_target = target;
            let addr = spawn(ctx, SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();
            let hello = next_of_type(&mut ws, "hello").await;
            assert_eq!(
                hello["caps"].as_array().unwrap().contains(&"dmx".into()),
                want
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_names_the_box() {
        let ctx = ctx();
        let table = ctx.device_table.clone();
        let addr = spawn(ctx, SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let status = next_of_type(&mut ws, "status").await;
        assert!(status["box"].is_null());
        table.lock().insert(
            9,
            crate::prolink::discovery::Device {
                device_number: 9,
                device_type: 1,
                name: "autoblink".into(),
                mac: [0; 6],
                ip: [192, 168, 2, 50],
                last_seen: std::time::Instant::now(),
            },
        );
        wait_for(&mut ws, "status", |v| v["box"] == "192.168.2.50").await;
    }
}
