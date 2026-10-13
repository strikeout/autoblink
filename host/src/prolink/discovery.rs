//! Device discovery — listens on UDP 50000 for Pro DJ Link keep-alive packets
//! and maintains a live device table.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

use super::packets::KeepAlive;
use super::PORT_DISCOVERY;

// ── Device record ─────────────────────────────────────────────────────────────

/// A Pioneer device seen on the Pro DJ Link network.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Device {
    pub device_number: u8,
    pub device_type: u8,
    pub name: String,
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    pub last_seen: Instant,
}

impl Device {
    pub(crate) fn from_keepalive(ka: KeepAlive) -> Self {
        Self {
            device_number: ka.device_number,
            device_type: ka.device_type,
            name: ka.name,
            mac: ka.mac,
            ip: ka.ip,
            last_seen: Instant::now(),
        }
    }
}

// ── Events ────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    Appeared(Device),
    Disappeared(u8), // device_number
}

// ── Device table ──────────────────────────────────────────────────────────────

/// Shared, lock-protected table of all known devices.
pub type DeviceTable = Arc<Mutex<HashMap<u8, Device>>>;

/// How long without a keep-alive before we consider a device gone.
const EXPIRY: Duration = Duration::from_secs(5);

// ── Main discovery task ───────────────────────────────────────────────────────

/// Bind to port 50000, parse keep-alive packets, maintain the device table,
/// and emit [DeviceEvent]s on the returned channel.
///
/// The returned `broadcast::Receiver` can be cloned to share events with
/// multiple consumers.
pub async fn run(
    bind_addr: Ipv4Addr,
    own_name: String,
    table: DeviceTable,
    tx: broadcast::Sender<DeviceEvent>,
) -> anyhow::Result<()> {
    use socket2::{Domain, Protocol, Socket, Type};

    // Use socket2 so we can set SO_REUSEADDR/SO_REUSEPORT before binding.
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(not(windows))]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT_DISCOVERY);
    sock.bind(&addr.into())?;

    let udp = UdpSocket::from_std(std::net::UdpSocket::from(sock))?;

    let mut buf = [0u8; 2048];
    let mut expiry_check = tokio::time::interval(Duration::from_secs(2));

    tracing::info!(%bind_addr, port = PORT_DISCOVERY, "Discovery listener started");

    loop {
        tokio::select! {
            result = udp.recv_from(&mut buf) => {
                match result {
                    Ok((len, src)) => handle_packet(&buf[..len], src, bind_addr, &own_name, &table, &tx),
                    Err(e) => tracing::warn!("Discovery recv error: {e}"),
                }
            }
            _ = expiry_check.tick() => {
                expire_devices(&table, &tx);
            }
        }
    }
}

fn handle_packet(
    data: &[u8],
    src: std::net::SocketAddr,
    own_ip: Ipv4Addr,
    own_name: &str,
    table: &DeviceTable,
    tx: &broadcast::Sender<DeviceEvent>,
) {
    if data.len() < 0x24 || !super::packets::has_magic(data) {
        return;
    }
    if data[0x0a] != super::PKT_KEEPALIVE {
        tracing::debug!(
            src = %src,
            pkt_type = format!("0x{:02x}", data[0x0a]).as_str(),
            len = data.len(),
            "Discovery received non-keepalive packet"
        );
        return;
    }

    if let Some(ka) = super::packets::parse_keepalive(data) {
        // The kernel loops our own broadcast keepalives back. Our virtual CDJ is not a peer.
        // Compare the name too, because rekordbox can run on the same host.
        if ka.ip == own_ip.octets() && ka.name == own_name {
            return;
        }
        let dev = Device::from_keepalive(ka);
        let num = dev.device_number;
        let mut t = table.lock();
        let is_new = !t.contains_key(&num);
        if is_new {
            let _ = tx.send(DeviceEvent::Appeared(dev.clone()));
        }
        t.insert(num, dev);
    }
}

fn expire_devices(table: &DeviceTable, tx: &broadcast::Sender<DeviceEvent>) {
    let mut map = table.lock();
    let expired: Vec<u8> = map
        .iter()
        .filter(|(_, d)| d.last_seen.elapsed() > EXPIRY)
        .map(|(&n, _)| n)
        .collect();
    for num in expired {
        if let Some(dev) = map.remove(&num) {
            tracing::info!(device = num, name = %dev.name, "Device disappeared");
            let _ = tx.send(DeviceEvent::Disappeared(num));
        }
    }
}

/// Return a snapshot of the current device table.
#[allow(dead_code)]
pub fn snapshot(table: &DeviceTable) -> Vec<Device> {
    table.lock().values().cloned().collect()
}
