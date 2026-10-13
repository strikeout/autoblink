//! Application bootstrap — pure synchronous setup.
//!
//! This moduleinitializes config, logging, network interface, MIDI, and shared
//! state.  No async code, no `tokio::spawn`.  All constructed handles are
//! packaged into [`AppContext`] which is passed to [`runtime::run`](crate::runtime::run).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use midir::{MidiOutput, MidiOutputConnection};
use network_interface::{NetworkInterface, NetworkInterfaceConfig, V4IfAddr};
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use crate::config;
use crate::prolink::discovery::DeviceTable;
use crate::prolink::{MAGIC, PKT_KEEPALIVE};
use crate::state::{SharedState, TrackChange};
use crate::{midi::soak::SoakArgs, midi::soak::SoakMode};

// ── CLI ───────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "xdj-clock", version, about = "Pioneer CDJ/XDJ → MIDI bridge")]
pub struct Cli {
    /// Path to TOML config file.
    #[arg(short, long, default_value = "config.toml")]
    pub config: PathBuf,

    /// Override network interface (e.g. "Ethernet", "en0").
    #[arg(short, long)]
    pub interface: Option<String>,

    /// Override MIDI output port name (substring match).
    #[arg(short, long)]
    pub midi: Option<String>,

    /// Override virtual CDJ device number (1–15).
    #[arg(short, long)]
    pub device_number: Option<u8>,

    /// Beat source: prolink (Ethernet CDJs), link (Ableton Link / rekordbox), auto.
    #[arg(long)]
    pub source: Option<String>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    /// List available MIDI output ports and exit.
    #[arg(long)]
    pub list_midi: bool,

    /// List detected network interfaces with priority ranking and exit.
    #[arg(long)]
    pub list_interfaces: bool,

    /// Disable the TUI and use plain log output.
    #[arg(long)]
    pub no_tui: bool,

    /// Web UI address (default from config `web.bind`, else 0.0.0.0:11111).
    /// When the port is in use, the next free port is used.
    #[arg(long, value_name = "ADDR")]
    pub web_bind: Option<String>,

    /// Run a non-interactive MIDI output soak and write a JSON report.
    #[arg(long, value_enum)]
    pub soak: Option<SoakMode>,

    /// Soak duration in seconds.
    #[arg(long, default_value_t = 300, requires = "soak")]
    pub duration_secs: u64,

    /// MIDI output port name for soak mode (substring match).
    #[arg(long, default_value = "auto", requires = "soak")]
    pub midi_out: String,

    /// Output path for the soak JSON report.
    #[arg(long, requires = "soak")]
    pub report: Option<PathBuf>,

    /// MTC frame rate for soak mode (frames per second). Only used with --soak mtc.
    #[arg(long, default_value_t = 25, requires = "soak")]
    pub fps: u8,
}

// ── AppContext ───────────────────────────────────────────────────────────────

/// All handles produced by bootstrap.  Passed to [`runtime::run`] which
/// takes ownership and spawns the async tasks.
pub struct AppContext {
    pub cli: Cli,
    pub startup_cfg: config::Config,
    pub midi_conn: Arc<Mutex<Option<MidiOutputConnection>>>,
    pub dj_state: SharedState,
    pub cfg: config::SharedConfig,
    pub device_table: DeviceTable,
    pub log_buf: crate::logbuf::LogBuffer,
    // broadcast senders
    pub device_tx: broadcast::Sender<crate::prolink::discovery::DeviceEvent>,
    pub beat_tx: broadcast::Sender<crate::prolink::beat_listener::BeatEvent>,
    pub status_tx: broadcast::Sender<crate::prolink::status_listener::StatusEvent>,
    pub vcdjready_tx: broadcast::Sender<crate::prolink::virtual_cdj::VirtualCdjReady>,
    // track change
    pub track_change_tx: mpsc::Sender<TrackChange>,
    pub track_change_rx: mpsc::Receiver<TrackChange>,
}

/// Initialize logging, load config, detect interface, open MIDI, build state.
pub fn init() -> anyhow::Result<AppContext> {
    let cli = Cli::parse();

    // ── Logging ──────────────────────────────────────────────────────────────
    let use_tui = !cli.no_tui;
    let log_buf = crate::logbuf::LogBuffer::new();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::from_str(&cli.log_level).unwrap_or_default()
    });

    if use_tui {
        use tracing_subscriber::fmt;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .with_target(true)
                    .with_ansi(false)
                    .with_timer(fmt::time::Uptime::default())
                    .with_writer(crate::logbuf::MakeLogWriter::new(log_buf.clone())),
            )
            .init();
    } else {
        use tracing_subscriber::fmt;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        // Stdout for the journal, and the buffer for the web UI log panel.
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                fmt::layer()
                    .with_target(true)
                    .with_timer(fmt::time::Uptime::default()),
            )
            .with(
                fmt::layer()
                    .with_target(true)
                    .with_ansi(false)
                    .with_timer(fmt::time::Uptime::default())
                    .with_writer(crate::logbuf::MakeLogWriter::new(log_buf.clone())),
            )
            .init();
    }

    // A panic in a spawned task does not stop the process. Log it, so that the
    // TUI log panel and the headless log show it.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        prev_hook(info);
    }));

    // ── MIDI port list shortcut ──────────────────────────────────────────────
    if cli.list_midi {
        list_midi_ports()?;
        std::process::exit(0);
    }
    if cli.list_interfaces {
        list_interfaces()?;
        std::process::exit(0);
    }

    // ── Soak mode shortcut (non-interactive) ─────────────────────────────────
    if let Some(mode) = cli.soak {
        let report_path = cli
            .report
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--report is required when using --soak"))?;

        let args = SoakArgs {
            mode,
            duration_secs: cli.duration_secs,
            midi_out: cli.midi_out.clone(),
            report_path,
            fps: cli.fps,
        };

        tracing::info!(
            ?mode,
            duration_secs = args.duration_secs,
            midi_out = %args.midi_out,
            report = %args.report_path.display(),
            fps = args.fps,
            "Running MIDI soak"
        );

        let res = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(crate::midi::soak::run(args))
        });

        match res {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                tracing::error!(error = %err, "Soak failed");
                eprintln!("soak failed: {err:?}");
                std::process::exit(2);
            }
        }
    }

    // ── Config ───────────────────────────────────────────────────────────────
    let mut initial_cfg = config::load(&cli.config)?;
    if let Some(iface) = &cli.interface {
        initial_cfg.interface = iface.clone();
    }
    if let Some(midi) = &cli.midi {
        initial_cfg.midi.output = midi.clone();
    }
    if let Some(dn) = cli.device_number {
        initial_cfg.device_number = dn;
    }
    if let Some(src) = &cli.source {
        initial_cfg.source = match src.to_lowercase().as_str() {
            "prolink" => config::Source::ProLink,
            "link" => config::Source::Link,
            "auto" => config::Source::Auto,
            other => anyhow::bail!("Unknown --source {:?}: use prolink, link, or auto", other),
        };
    }

    // The CLI overrides bypass the check in config::load, so check again.
    config::validate(&initial_cfg)?;

    let cfg = config::new_shared(initial_cfg.clone());
    let startup_cfg = cfg.read().clone();

    tracing::info!(
        source = ?startup_cfg.source,
        interface = %startup_cfg.interface,
        device_number = startup_cfg.device_number,
        device_name = %startup_cfg.device_name,
        midi_port = %startup_cfg.midi.output,
        "xdj-clock starting"
    );

    // The Pro DJ Link supervisor detects the network interface (runtime/prolink.rs).

    // ── MIDI output ──────────────────────────────────────────────────────────
    let midi_conn: Arc<Mutex<Option<MidiOutputConnection>>> =
        match crate::midi::open_midi_output(&startup_cfg.midi.output) {
            Ok((conn, name)) => {
                // Keep the full port name, so that a reconnect finds the same device again.
                cfg.write().midi.output = name;
                Arc::new(Mutex::new(Some(conn)))
            }
            Err(e) => {
                tracing::warn!("Could not open MIDI output: {e}");
                tracing::warn!("Running without MIDI. The app connects when the port appears.");
                Arc::new(Mutex::new(None))
            }
        };

    // ── Shared state ─────────────────────────────────────────────────────────
    let dj_state = crate::state::new_shared(startup_cfg.midi.smoothing_ms);

    // ── Broadcast channels ───────────────────────────────────────────────────
    let (device_tx, _device_rx) = broadcast::channel::<crate::prolink::discovery::DeviceEvent>(64);
    let (beat_tx, _beat_rx1) = broadcast::channel::<crate::prolink::beat_listener::BeatEvent>(256);
    let (status_tx, _status_rx1) =
        broadcast::channel::<crate::prolink::status_listener::StatusEvent>(256);
    let (vcdjready_tx, _vcdjready_rx) =
        broadcast::channel::<crate::prolink::virtual_cdj::VirtualCdjReady>(4);

    let device_table: DeviceTable = Arc::new(Mutex::new(HashMap::default()));

    let (track_change_tx, track_change_rx) = mpsc::channel::<TrackChange>(64);

    Ok(AppContext {
        cli,
        startup_cfg,
        midi_conn,
        dj_state,
        cfg,
        device_table,
        log_buf,
        device_tx,
        beat_tx,
        status_tx,
        vcdjready_tx,
        track_change_tx,
        track_change_rx,
    })
}

// ── Interface detection ───────────────────────────────────────────────────────

/// An IPv4 address on a network interface that can carry Pro DJ Link.
pub struct Candidate {
    pub name: String,
    pub ip: Ipv4Addr,
    pub bcast: Ipv4Addr,
    pub mac: [u8; 6],
    pub priority: u8,
}

/// Lists the IPv4 interface addresses that match `hint`, best priority first.
pub fn interface_candidates(hint: &str) -> anyhow::Result<Vec<Candidate>> {
    let ifaces = NetworkInterface::show()?;

    let mut candidates: Vec<Candidate> = Vec::new();

    for iface in &ifaces {
        if hint != "auto" && !iface.name.to_lowercase().contains(&hint.to_lowercase()) {
            continue;
        }
        for addr in &iface.addr {
            if let network_interface::Addr::V4(V4IfAddr { ip, broadcast, .. }) = addr {
                if ip.is_loopback() {
                    continue;
                }

                let bcast = broadcast.unwrap_or_else(|| {
                    let o = ip.octets();
                    Ipv4Addr::new(o[0], o[1], o[2], 255)
                });

                let mac = iface
                    .mac_addr
                    .as_ref()
                    .and_then(|mac_str| {
                        let bytes: Vec<u8> = mac_str
                            .split(':')
                            .filter_map(|s| u8::from_str_radix(s, 16).ok())
                            .collect();
                        <[u8; 6]>::try_from(bytes.as_slice()).ok()
                    })
                    .unwrap_or([0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x02]);

                let priority = interface_priority(&iface.name, ip);

                tracing::debug!(
                    name = %iface.name,
                    %ip,
                    %bcast,
                    priority,
                    "Discovered network interface"
                );

                candidates.push(Candidate {
                    name: iface.name.clone(),
                    ip: *ip,
                    bcast,
                    mac,
                    priority,
                });
            }
        }
    }

    candidates.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));
    Ok(candidates)
}

/// Detect the network interface to use for Pro DJ Link.
/// Returns (bind_ip, broadcast_ip, mac_address).
pub fn detect_interface(hint: &str) -> anyhow::Result<(Ipv4Addr, Ipv4Addr, [u8; 6])> {
    let candidates = interface_candidates(hint)?;

    if hint == "auto" {
        let best_priority = candidates.first().map(|c| c.priority);
        let tied_best: Vec<_> = candidates
            .iter()
            .take_while(|c| Some(c.priority) == best_priority)
            .collect();

        if tied_best.len() > 1 {
            tracing::info!(
                priority = tied_best[0].priority,
                count = tied_best.len(),
                "Multiple equal-priority interfaces found; probing for Pro DJ Link keepalives"
            );

            for cand in tied_best {
                tracing::debug!(name = %cand.name, ip = %cand.ip, "Probing interface for keepalives");
                if interface_has_keepalive(cand.ip, cand.bcast) {
                    if cand.mac == [0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x02] {
                        tracing::warn!(
                            "Could not detect MAC address for {}, using placeholder",
                            cand.name
                        );
                    }
                    tracing::info!(
                        name = %cand.name,
                        ip = %cand.ip,
                        bcast = %cand.bcast,
                        priority = cand.priority,
                        "Selected network interface"
                    );
                    return Ok((cand.ip, cand.bcast, cand.mac));
                }
            }

            tracing::info!(
                "No keepalives detected on equal-priority interfaces; falling back to the first candidate"
            );
        }
    }

    if let Some(best) = candidates.first() {
        if best.mac == [0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x02] {
            tracing::warn!(
                "Could not detect MAC address for {}, using placeholder",
                best.name
            );
        }
        tracing::info!(
            name = %best.name,
            ip = %best.ip,
            bcast = %best.bcast,
            priority = best.priority,
            "Selected network interface"
        );
        return Ok((best.ip, best.bcast, best.mac));
    }

    let available: Vec<String> = NetworkInterface::show()?
        .into_iter()
        .map(|i| i.name)
        .collect();
    anyhow::bail!(
        "No suitable network interface found. \
         Available: {:?}. Use --interface to specify one.",
        available
    )
}

fn interface_has_keepalive(ip: Ipv4Addr, bcast: Ipv4Addr) -> bool {
    use socket2::{Domain, Protocol, Socket, Type};
    let raw = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(%ip, error = %e, "Unable to create probe socket");
            return false;
        }
    };
    let _ = raw.set_reuse_address(true);
    #[cfg(not(windows))]
    let _ = raw.set_reuse_port(true);

    let socket = match raw
        .bind(&std::net::SocketAddrV4::new(bcast, crate::prolink::PORT_DISCOVERY).into())
        .or_else(|_| {
            raw.bind(
                &std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, crate::prolink::PORT_DISCOVERY)
                    .into(),
            )
        }) {
        Ok(_) => std::net::UdpSocket::from(raw),
        Err(err) => {
            tracing::debug!(%ip, error = %err, "Unable to bind probe socket");
            return false;
        }
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut buf = [0u8; 2048];

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }

        if let Err(err) = socket.set_read_timeout(Some(deadline.saturating_duration_since(now))) {
            tracing::debug!(%ip, error = %err, "Unable to set probe socket timeout");
            return false;
        }

        match socket.recv_from(&mut buf) {
            Ok((len, peer)) => {
                if len > 0x0a && buf[..10] == MAGIC && buf[0x0a] == PKT_KEEPALIVE {
                    tracing::info!(%ip, %peer, "Detected Pro DJ Link keepalive");
                    return true;
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return false;
            }
            Err(err) => {
                tracing::debug!(%ip, error = %err, "Probe socket error");
                return false;
            }
        }
    }
}

/// Assign a priority to a network interface by name and IP (lower = better).
pub fn interface_priority(name: &str, ip: &Ipv4Addr) -> u8 {
    let lower = name.to_lowercase();

    const VPN_KEYWORDS: &[&str] = &[
        "pangp",
        "globalprotect",
        "vpn",
        "tunnel",
        "tun",
        "tap",
        "virtual",
        "vmware",
        "vmnet",
        "vethernet",
        "hyper-v",
        "docker",
        "vbox",
        "virtualbox",
        "wsl",
        "loopback",
        "npcap",
        "winpcap",
    ];
    for kw in VPN_KEYWORDS {
        if lower.contains(kw) {
            return 5;
        }
    }

    const WIFI_KEYWORDS: &[&str] = &["wi-fi", "wifi", "wlan", "wireless", "80211"];
    const ETH_KEYWORDS: &[&str] = &["ethernet", "eth", "en0", "en1", "enp", "eno"];

    let mut base = 3u8;
    for kw in WIFI_KEYWORDS {
        if lower.contains(kw) {
            base = 2;
            break;
        }
    }
    for kw in ETH_KEYWORDS {
        if lower.contains(kw) {
            base = 1;
            break;
        }
    }

    let octets = ip.octets();
    if octets[0] == 169 && octets[1] == 254 {
        return 0;
    }

    const DJ_KEYWORDS: &[&str] = &["alphatheta", "pioneer", "pro dj", "prodj", "rekordbox"];
    for kw in DJ_KEYWORDS {
        if lower.contains(kw) {
            return 0;
        }
    }

    if octets[0] == 10 && base == 1 {
        let stripped = lower.replace("ethernet", "").trim().to_string();
        if stripped
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_whitespace())
        {
            return 5;
        }
    }

    base
}

pub fn list_midi_ports() -> anyhow::Result<()> {
    let midi_out = MidiOutput::new("xdj-clock")?;
    let ports = midi_out.ports();
    if ports.is_empty() {
        println!("No MIDI output ports found.");
        return Ok(());
    }
    println!("Available MIDI output ports:");
    for (i, port) in ports.iter().enumerate() {
        println!("  [{i}] {}", midi_out.port_name(port)?);
    }
    Ok(())
}

pub fn list_interfaces() -> anyhow::Result<()> {
    let ifaces = NetworkInterface::show()?;
    let mut found = false;

    println!("Detected network interfaces:");
    println!(
        "{:<6} {:<30} {:<18} {:<18} {}",
        "Prio", "Name", "IPv4", "Broadcast", "MAC"
    );
    println!("{}", "-".repeat(95));

    for iface in &ifaces {
        for addr in &iface.addr {
            if let network_interface::Addr::V4(V4IfAddr { ip, broadcast, .. }) = addr {
                if ip.is_loopback() {
                    continue;
                }
                let bcast = broadcast
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "-".into());
                let mac_str = iface
                    .mac_addr
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "-".into());
                let prio = interface_priority(&iface.name, ip);
                let prio_label = match prio {
                    0 => "0 (DJ)",
                    1 => "1 (Eth)",
                    2 => "2 (WiFi)",
                    3 => "3 (?)",
                    4 => "4 (LnkL)",
                    5 => "5 (VPN)",
                    _ => "?",
                };
                println!(
                    "{:<6} {:<30} {:<18} {:<18} {}",
                    prio_label, iface.name, ip, bcast, mac_str
                );
                found = true;
            }
        }
    }

    if !found {
        println!("  (no non-loopback IPv4 interfaces found)");
    }

    println!();
    println!("Priority: 0=Link-local/DJ, 1=Ethernet, 2=Wi-Fi, 3=Unknown, 5=VPN/Virtual");
    println!("Auto-select picks the lowest priority number. Use --interface to override.");
    Ok(())
}

/// Resolves on Ctrl+C, and on SIGTERM from a service manager such as systemd.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!("Cannot listen for SIGTERM: {e}"),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn headless_loop(
    state: SharedState,
    midi_out: crate::midi::MidiOutHandle,
) -> anyhow::Result<()> {
    tracing::info!("All tasks running (headless mode). Ctrl+C to quit.");
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    let terminate = shutdown_signal();
    tokio::pin!(terminate);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let master = state.read().master.clone();
                if master.device_number > 0 || master.source.is_some() {
                    tracing::info!(
                        source = ?master.source,
                        master_deck = master.device_number,
                        bpm = %format!("{:.2}", master.bpm),
                        pitch = %format!("{:+.2}%", master.pitch_pct),
                        beat = master.beat_in_bar,
                        playing = master.is_playing,
                        "Master status"
                    );
                } else {
                    tracing::info!("Waiting for tempo master…");
                }
            }
            _ = &mut terminate => {
                tracing::info!("Shutting down…");
                midi_out.stop().await;
                tracing::info!("MIDI Stop sent, exiting.");
                return Ok(());
            }
        }
    }
}
