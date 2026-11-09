//! The `artnet` thread receives Art-Net from a lighting desk. The desk sends ArtDmx for universe 0
//! to UDP port 6454.
//!
//! The box answers an ArtPoll with an ArtPollReply, so that a desk finds it. The reply gives the
//! address of the interface in the subnet of the desk, and comes from port 6454.
//!
//! The thread receives on all interfaces: the own WiFi network, the joined WiFi network and the
//! Ethernet port. It keeps the last frame of universe 0 in `State::artnet`. The `dmx` thread
//! merges that frame into its output (`autoblink_core_dmx::Merge`).

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use autoblink_core_dmx::{
    build_artpoll_reply, is_artpoll, parse_artdmx, Universe, ARTNET_PORT, UNIVERSE,
};

use crate::State;

/// The Art-Net header (18 bytes) and a full universe. The receive cuts a longer datagram, and
/// the parser then refuses it when its length field is too large.
const PACKET_LEN: usize = 18 + UNIVERSE;

/// The last Art-Net frame of universe 0.
pub struct ArtIn {
    /// The data is on the heap, so that `State` stays small.
    pub data: Box<Universe>,
    /// The time of the frame, with the clock of `State::now_ms`. `None` before the first frame.
    pub at_ms: Option<u64>,
}

impl Default for ArtIn {
    fn default() -> Self {
        Self {
            data: Box::new([0; UNIVERSE]),
            at_ms: None,
        }
    }
}

/// Starts the `artnet` thread. When the thread does not start, the firmware logs a warning and
/// continues without Art-Net.
pub fn start(state: Arc<State>) {
    // 1.30.0 kept 2.1 KB of its 4 KB free (/mem). The ArtPollReply adds a 239-byte packet and a
    // second bind below the receive buffer, so the stack is 5 KB. Measure with /mem on a box.
    let started = crate::spawn(c"artnet", 5 * 1024, move || {
        let sock = match UdpSocket::bind(("0.0.0.0", ARTNET_PORT)) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Art-Net is off: port {ARTNET_PORT} is not available: {e}");
                return;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
        log::info!("Art-Net: the firmware receives universe 0 on port {ARTNET_PORT}");
        let mut buf = [0u8; PACKET_LEN];
        loop {
            let (len, from) = match sock.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue
                }
                // The sleep lets the lower-priority tasks run after a receive error.
                Err(_) => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            if is_artpoll(&buf[..len]) {
                reply_to_poll(&sock, &state, from);
                continue;
            }
            let Some((0, data)) = parse_artdmx(&buf[..len]) else {
                continue;
            };
            let now_ms = state.now_ms();
            // The lock is short. The `dmx` thread does not wait for it.
            let mut slot = state.artnet.lock().unwrap();
            slot.data[..data.len()].copy_from_slice(data);
            slot.data[data.len()..].fill(0);
            slot.at_ms = Some(now_ms);
        }
    });
    if let Err(e) = started {
        log::warn!("Art-Net is off: the thread did not start: {e}");
    }
}

/// A network interface of the box, for the ArtPollReply.
#[derive(Debug, Clone, Copy)]
pub struct Iface {
    /// The lwIP name of the interface.
    pub netif: &'static str,
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub mac: [u8; 6],
}

/// Sends an ArtPollReply to the desk at `from`, on port 6454.
///
/// The reply gives the address of the interface in the subnet of the desk. The Ethernet port
/// comes first, then the joined WiFi network, then the own WiFi network. Without such an
/// interface, the box does not reply.
///
/// The reply goes out from `sock`, so that its source port is 6454, as Art-Net requires. A desk
/// or a firewall can drop a reply from another port. For the send, `sock` is bound to the
/// interface. Without the binding, lwIP can send the reply out on the own WiFi network when the
/// joined network uses the same subnet. A packet that arrives on another interface during the
/// send is lost.
// ponytail: the joined WiFi network wins over the own WiFi network when the two use one subnet,
// so a desk on the own network gets the wrong address. The upgrade is to read the receive
// interface of the poll (lwIP IP_PKTINFO).
fn reply_to_poll(sock: &UdpSocket, state: &State, from: SocketAddr) {
    let IpAddr::V4(desk) = from.ip() else {
        return;
    };
    let ifaces = *state.artnet_ifaces.lock().unwrap();
    let Some(iface) = ifaces
        .into_iter()
        .flatten()
        .find(|i| crate::same_subnet(i.ip, desk, i.prefix))
    else {
        return;
    };
    let version = (
        env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
        env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
    );
    let reply = build_artpoll_reply(
        iface.ip.octets(),
        iface.mac,
        version,
        "#0001 [0000] AUTOBLINK OK",
    );
    let fd = sock.as_raw_fd();
    let bound = crate::net::bind_device(fd, iface.netif).is_ok();
    let sent = sock.send_to(&reply, (desk, ARTNET_PORT));
    let _ = crate::net::bind_device(fd, "");
    match sent {
        Err(e) => log::warn!("Art-Net: the ArtPollReply to {from} failed: {e}"),
        Ok(_) if !FIRST_POLL.swap(true, Ordering::Relaxed) => log::info!(
            "Art-Net: the first ArtPoll came from {from}. The reply gives {} (interface \"{}\", bound: {bound})",
            iface.ip,
            iface.netif
        ),
        Ok(_) => {}
    }
}

/// True after the first answered ArtPoll. The log shows only the first one.
static FIRST_POLL: AtomicBool = AtomicBool::new(false);
