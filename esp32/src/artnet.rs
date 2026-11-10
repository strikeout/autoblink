//! Art-Net from a lighting desk. The desk sends ArtDmx for universe 0 to UDP port 6454.
//!
//! The box answers an ArtPoll with an ArtPollReply, so that a desk finds it. The reply gives the
//! address of the interface in the subnet of the desk, and comes from port 6454.
//!
//! The socket receives on all interfaces: the own WiFi network, the joined WiFi network and the
//! Ethernet port. The `dmx` thread reads it without a wait on each loop pass (`drain`). It keeps
//! the last frame of universe 0 and merges it into its output (`autoblink_core_dmx::Merge`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use autoblink_core_dmx::{
    build_artpoll_reply, is_artpoll, parse_artdmx, Universe, ARTNET_PORT, UNIVERSE,
};

use crate::State;

/// The Art-Net header (18 bytes) and a full universe. The receive cuts a longer datagram, and
/// the parser then refuses it when its length field is too large.
pub const PACKET_LEN: usize = 18 + UNIVERSE;

/// The most packets that one `drain` reads. The lwIP receive queue of the socket holds 16
/// (`CONFIG_LWIP_UDP_RECVMBOX_SIZE`).
// ponytail: without DMX IN, the `dmx` loop drains once in each frame (24 ms), so the box takes
// about 660 packets each second. A desk that sends 16 or more universes at 44 Hz fills the queue,
// and lwIP drops frames of universe 0 too. The upgrade is a larger receive queue and DRAIN_MAX.
const DRAIN_MAX: usize = 16;

/// Opens the Art-Net socket and gives it to the `dmx` thread. Without the port, the firmware logs
/// a warning and continues without Art-Net.
pub fn start(state: &State) {
    let sock = match UdpSocket::bind(("0.0.0.0", ARTNET_PORT)) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Art-Net is off: port {ARTNET_PORT} is not available: {e}");
            return;
        }
    };
    if let Err(e) = sock.set_nonblocking(true) {
        log::warn!("Art-Net is off: the socket cannot be non-blocking: {e}");
        return;
    }
    log::info!("Art-Net: the firmware receives universe 0 on port {ARTNET_PORT}");
    let _ = state.artnet.set(sock);
}

/// Reads the waiting packets of `sock` without a wait. Copies the last frame of universe 0 into
/// `art`, and replies to each ArtPoll. Returns true when a frame of universe 0 came.
// The reads have their own stack frame. Thus the frame loop of the `dmx` thread does not hold
// them.
#[inline(never)]
pub fn drain(
    sock: &UdpSocket,
    state: &State,
    buf: &mut [u8; PACKET_LEN],
    art: &mut Universe,
) -> bool {
    let mut got = false;
    for _ in 0..DRAIN_MAX {
        let (len, from) = match sock.recv_from(buf) {
            Ok(r) => r,
            // An empty queue gives WouldBlock. The next loop pass tries again after an error.
            Err(_) => break,
        };
        if is_artpoll(&buf[..len]) {
            reply_to_poll(sock, state, from);
            continue;
        }
        let Some((0, data)) = parse_artdmx(&buf[..len]) else {
            continue;
        };
        art[..data.len()].copy_from_slice(data);
        art[data.len()..].fill(0);
        got = true;
    }
    got
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
#[inline(never)]
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
    // A desk polls every few seconds. Only the first failure after a good reply goes into the log,
    // so that the log ring keeps the other lines.
    match sent {
        Err(e) if !POLL_FAILED.swap(true, Ordering::Relaxed) => {
            log::warn!("Art-Net: the ArtPollReply to {from} failed: {e}")
        }
        Err(_) => {}
        Ok(_) if POLL_FAILED.swap(false, Ordering::Relaxed) => {
            log::info!("Art-Net: the ArtPollReply to {from} goes out again")
        }
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
/// True after a failed ArtPollReply, until a reply goes out again.
static POLL_FAILED: AtomicBool = AtomicBool::new(false);
