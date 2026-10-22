//! Virtual CDJ — joins the Pro DJ Link network so real hardware sends us
//! detailed status packets on UDP port 50002.
//!
//! Implements the full 4-stage channel-claim sequence, then broadcasts
//! keep-alive packets every 1500 ms.  Uses CDJ-3000-compatible byte 0x35 = 0x64
//! to avoid disrupting players on channels 5 and 6.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

use super::builder::{
    build_announce, build_claim1, build_claim2, build_claim_final, build_keepalive,
    build_status_packet, pad_name,
};
use super::discovery::DeviceTable;
use super::packets::has_magic;
use super::{PKT_CONFLICT, PORT_DISCOVERY, PORT_STATUS};

// ── Timing constants ──────────────────────────────────────────────────────────

const CLAIM_INTERVAL: Duration = Duration::from_millis(300);
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1500);
const CLAIM_ROUNDS: usize = 3;
/// After this many keepalive send errors in a row, the task stops, so that
/// the supervisor finds the network again. 5 × 1.5 s is about 7.5 s.
const MAX_KEEPALIVE_FAILURES: u32 = 5;

/// Aborts the task when the handle goes out of scope.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ── Signals ───────────────────────────────────────────────────────────────────

/// Sent once the virtual CDJ has successfully claimed a channel.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct VirtualCdjReady {
    pub device_number: u8,
    pub ip: [u8; 4],
    pub mac: [u8; 6],
}

// ── Main task ─────────────────────────────────────────────────────────────────

/// Run the virtual CDJ: claim a channel number, then broadcast keep-alive
/// packets forever.
///
/// Fires `ready_tx` once the claim sequence succeeds so downstream tasks
/// (status listener, etc.) know they can start.
pub async fn run(
    bind_ip: Ipv4Addr,
    broadcast_ip: Ipv4Addr,
    mac: [u8; 6],
    device_number: u8,
    device_name: &str,
    device_table: DeviceTable,
    ready_tx: broadcast::Sender<VirtualCdjReady>,
) -> anyhow::Result<()> {
    let ip: [u8; 4] = bind_ip.octets();
    let name = pad_name(device_name);
    let bcast_addr = SocketAddrV4::new(broadcast_ip, PORT_DISCOVERY);
    let bcast_addr6: std::net::SocketAddr = bcast_addr.into();

    // Build the discovery socket
    let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    raw.set_reuse_address(true)?;
    #[cfg(not(windows))]
    raw.set_reuse_port(true)?;
    raw.set_broadcast(true)?;
    raw.set_nonblocking(true)?;
    raw.bind(&SocketAddrV4::new(bind_ip, PORT_DISCOVERY).into())?;
    let sock = UdpSocket::from_std(std::net::UdpSocket::from(raw))?;
    let sock = Arc::new(sock);

    // ── Stage 1: Initial announcements ───────────────────────────────────────
    tracing::info!(device = device_number, "Virtual CDJ: sending announcements");
    let announce_pkt = build_announce(&name);
    for _ in 0..CLAIM_ROUNDS {
        tracing::trace!(
            len = announce_pkt.len(),
            hex = %super::hex_preview(&announce_pkt, 48),
            "Sending announce packet"
        );
        sock.send_to(&announce_pkt, bcast_addr6).await?;
        tokio::time::sleep(CLAIM_INTERVAL).await;
    }

    // ── Stage 2: First-stage claim ────────────────────────────────────────────
    tracing::info!(device = device_number, "Virtual CDJ: first-stage claim");
    for n in 1..=(CLAIM_ROUNDS as u8) {
        let pkt = build_claim1(&name, &mac, n);
        tracing::trace!(
            len = pkt.len(),
            iteration = n,
            hex = %super::hex_preview(&pkt, 48),
            "Sending claim1 packet"
        );
        sock.send_to(&pkt, bcast_addr6).await?;
        tokio::time::sleep(CLAIM_INTERVAL).await;
    }

    // ── Stage 3: Second-stage claim + conflict detection ──────────────────
    tracing::info!(device = device_number, "Virtual CDJ: second-stage claim");
    let mut claimed = device_number;
    let mut buf = [0u8; 256];
    'stage3: loop {
        for n in 1..=(CLAIM_ROUNDS as u8) {
            let pkt = build_claim2(&name, &ip, &mac, claimed, n);
            tracing::trace!(
                len = pkt.len(),
                iteration = n,
                hex = %super::hex_preview(&pkt, 48),
                "Sending claim2 packet"
            );
            sock.send_to(&pkt, bcast_addr6).await?;
            // Listen briefly for conflict packets.
            let deadline = tokio::time::Instant::now() + CLAIM_INTERVAL;
            loop {
                let sleep = tokio::time::sleep_until(deadline);
                tokio::select! {
                    _ = sleep => break,
                    result = sock.recv_from(&mut buf) => {
                        if let Ok((len, _)) = result {
                            let data = &buf[..len];
                            if data.len() > 0x24
                                && has_magic(data)
                                && data[0x0a] == PKT_CONFLICT
                                && data[0x24] == claimed
                            {
                                // Our number is taken. Increment it and restart stage 3.
                                let old = claimed;
                                claimed += 1;
                                if claimed > 15 {
                                    anyhow::bail!("No free device numbers available");
                                }
                                tracing::warn!(old, new = claimed, "Device number conflict, retrying");
                                continue 'stage3;
                            }
                        }
                    }
                }
            }
        }
        break;
    }

    // ── Stage 4: Final claim ──────────────────────────────────────────────────
    tracing::info!(device = claimed, "Virtual CDJ: final claim");
    for n in 1..=(CLAIM_ROUNDS as u8) {
        let pkt = build_claim_final(&name, claimed, n);
        tracing::trace!(
            len = pkt.len(),
            iteration = n,
            hex = %super::hex_preview(&pkt, 48),
            "Sending claim_final packet"
        );
        sock.send_to(&pkt, bcast_addr6).await?;
        tokio::time::sleep(CLAIM_INTERVAL).await;
    }

    tracing::info!(device = claimed, ip = ?ip, "Virtual CDJ online");
    let _ = ready_tx.send(VirtualCdjReady {
        device_number: claimed,
        ip,
        mac,
    });

    let status_pkt = build_status_packet(&name, claimed);
    let table = Arc::clone(&device_table);
    let listener_sock = Arc::clone(&sock);
    let own_name = device_name.to_string();
    // The listener stops with this task, so a rebuilt task group does not
    // leave it behind on the old address.
    let _listener = AbortOnDrop(tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            match listener_sock.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let data = &buf[..len];
                    if let Some(ka) = super::packets::parse_keepalive(data) {
                        // The kernel loops our own broadcast keep-alives back. Like the
                        // discovery task, compare the name too, because rekordbox can run on
                        // the same host.
                        if ka.ip == ip && ka.name == own_name {
                            continue;
                        }
                        let dev = super::discovery::Device::from_keepalive(ka);
                        let num = dev.device_number;
                        let mut t = table.lock();
                        let is_new = !t.contains_key(&num);
                        if is_new {
                            tracing::info!(
                                device = num,
                                name = %dev.name,
                                ip = ?dev.ip,
                                "Virtual CDJ: discovered real CDJ on network"
                            );
                        }
                        t.insert(num, dev);
                    }
                    let status_addr: std::net::SocketAddr = match src.ip() {
                        std::net::IpAddr::V4(v4) => SocketAddrV4::new(v4, PORT_STATUS).into(),
                        std::net::IpAddr::V6(_) => continue,
                    };
                    if let Err(e) = listener_sock.send_to(&status_pkt, status_addr).await {
                        tracing::debug!("Status send error to {status_addr}: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Virtual CDJ listener recv error: {e}");
                    tokio::time::sleep(super::RECV_ERROR_BACKOFF).await;
                }
            }
        }
    }));

    // ── Keep-alive loop ───────────────────────────────────────────────────────
    let status_pkt = build_status_packet(&name, claimed);
    let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);
    let mut keepalive_failures: u32 = 0;
    loop {
        interval.tick().await;

        // 1. Broadcast keep-alive on port 50000 using the interface subnet broadcast
        //    so it routes out the correct physical network interface.
        let pkt = build_keepalive(&name, claimed, &mac, &ip, 1);
        tracing::trace!(
            len = pkt.len(),
            hex = %super::hex_preview(&pkt, 48),
            "Sending keepalive packet"
        );
        match sock.send_to(&pkt, bcast_addr6).await {
            Ok(_) => keepalive_failures = 0,
            Err(e) => {
                tracing::warn!("Keep-alive send error: {e}");
                keepalive_failures += 1;
                if keepalive_failures >= MAX_KEEPALIVE_FAILURES {
                    anyhow::bail!("{keepalive_failures} keep-alive sends failed in a row: {e}");
                }
            }
        }

        // 2. Unicast status packet to each known device on port 50002.
        //    Real CDJs require this to share detailed metadata and to avoid
        //    "older firmware" warnings (matching prolink-go behaviour).
        {
            let devices: Vec<[u8; 4]> = device_table.lock().values().map(|d| d.ip).collect();
            for dev_ip in devices {
                let addr: std::net::SocketAddr =
                    SocketAddrV4::new(Ipv4Addr::from(dev_ip), PORT_STATUS).into();
                match sock.send_to(&status_pkt, addr).await {
                    Ok(n) => {
                        tracing::trace!(
                            target = %addr,
                            len = status_pkt.len(),
                            sent = n,
                            "Status unicast sent"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target = %addr,
                            error = %e,
                            "Status unicast FAILED"
                        );
                    }
                }
            }
        }
    }
}
