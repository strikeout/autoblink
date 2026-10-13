//! Web UI: serves the shared dashboard page and streams the app state over one WebSocket.
//!
//! Protocol v1 on `GET /ws`. Each message is a JSON object with a `type` field.
//!
//! Server to client:
//! - `hello`: `{proto, version, device, caps}`. The page shows only the panels in `caps`.
//! - `status`: a [`status::Status`], about 30 times per second.
//! - `beat`: `{device, beat}`, sent at the moment a beat arrives. Device 0 is Ableton Link.
//! - `settings`: `{items: [{idx, label, section, kind, value, raw}]}`, after each change.
//! - `ports`: `{ports, active, connected}`, after each change.
//! - `log`: `{lines}`, the new log lines.
//! - `error`: `{text}`, when a command fails.
//!
//! Client to server:
//! - `{type: "cycle", idx, dir}` steps a toggle, a cycle or a fixed-step setting.
//! - `{type: "set", idx, value}` stores a numeric setting.
//! - `{type: "port", name}` switches the MIDI output port.
//! - `{type: "rescan"}` lists the MIDI ports and network interfaces again.

pub mod status;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
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
}

struct Shared {
    ctx: WebCtx,
    interfaces: RwLock<Vec<NetworkIfaceInfo>>,
    ports: RwLock<Vec<String>>,
    status_tx: watch::Sender<Arc<str>>,
    settings_tx: watch::Sender<Arc<str>>,
    ports_tx: watch::Sender<Arc<str>>,
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
        ports_tx: watch::channel(empty).0,
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
            },
            value: settings::get_value(&cfg, &interfaces, idx),
            raw: settings::numeric_edit_value(&cfg, idx),
            step: settings::has_step(idx),
        })
        .collect();
    send_if_changed(
        &shared.settings_tx,
        tagged("settings", &json!({ "items": items })),
    );

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
    ws.on_upgrade(move |socket| client(socket, shared))
}

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
}

async fn send(socket: &mut WebSocket, msg: &str) -> bool {
    socket.send(Message::Text(msg.into())).await.is_ok()
}

async fn client(mut socket: WebSocket, shared: Arc<Shared>) {
    let hello = json!({
        "type": "hello",
        "proto": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "device": "host",
        "caps": ["beat", "decks", "mixer", "output", "settings", "ports", "logs"],
    });
    if !send(&mut socket, &hello.to_string()).await {
        return;
    }

    let mut status_rx = shared.status_tx.subscribe();
    let mut settings_rx = shared.settings_tx.subscribe();
    let mut ports_rx = shared.ports_tx.subscribe();
    let mut beat_rx = shared.ctx.beat_tx.subscribe();
    let mut log_seq = 0;

    // The producer skips its work while no client is connected, so build fresh messages now.
    produce_once(&shared);
    for rx in [&mut settings_rx, &mut ports_rx] {
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
        }
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
}
