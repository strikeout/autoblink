//! Art-Net from a lighting desk. The desk sends ArtDmx to UDP port 6454. The setting "Art-Net
//! Universe" gives the port-address that the box receives.
//!
//! The box answers an ArtPoll with an Art-Net 4 ArtPollReply, so that a desk finds it. The reply
//! gives the address of the interface in the subnet of the desk, and comes from port 6454.
//!
//! The socket receives on all interfaces: the own WiFi network, the joined WiFi network and the
//! Ethernet port. The `dmx` thread reads it without a wait on each loop pass (`drain`). Each frame
//! of the universe goes to `autoblink_core_dmx::Source`, which follows one sender at a time.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use autoblink_core_dmx::{
    build_artpoll_reply, parse_artdmx, parse_artpoll, NodeInfo, Proto, Sender, Source, ARTNET_PORT,
    ARTNET_PRIORITY, UNIVERSE,
};

use crate::State;

/// The Art-Net header (18 bytes) and a full universe. The receive cuts a longer datagram, and
/// the parser then refuses it when its length field is too large.
pub const PACKET_LEN: usize = 18 + UNIVERSE;

/// The most packets that one `drain` reads. The lwIP receive queue of the socket holds 16
/// (`CONFIG_LWIP_UDP_RECVMBOX_SIZE`).
// ponytail: without DMX IN, the `dmx` loop drains once in each frame (24 ms), so the box takes
// about 660 packets each second. A desk that sends 16 or more universes at 44 Hz fills the queue,
// and lwIP drops frames of our universe too. The upgrade is a larger receive queue and DRAIN_MAX.
const DRAIN_MAX: usize = 16;

/// The state of the DMX output for the ArtPollReply. The `dmx` thread builds it for each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Node {
    /// The port-address that the box receives (setting "Art-Net Universe").
    pub port_address: u16,
    /// The DMX output sends frames.
    pub sending: bool,
    /// The output takes its data from sACN.
    pub sacn: bool,
    /// The output merges a network source with the box.
    pub merging: bool,
    /// The output sends a frame each 24 ms ("DMX Refresh" 40 Hz).
    pub continuous: bool,
    /// The output fades out after a loss of data ("Signal Loss" Fade out).
    pub failsafe_zero: bool,
}

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
    // A poll from a desk in another subnet gets a broadcast reply.
    if let Err(e) = sock.set_broadcast(true) {
        log::warn!("Art-Net: the socket cannot send a broadcast: {e}");
    }
    log::info!("Art-Net: the firmware receives on port {ARTNET_PORT}");
    let _ = state.artnet.set(sock);
}

/// Reads the waiting packets of `sock` without a wait. Gives each frame of `node.port_address`
/// to `src`, and replies to each ArtPoll. Returns the number of frames of other universes.
// The reads have their own stack frame. Thus the frame loop of the `dmx` thread does not hold
// them.
#[inline(never)]
pub fn drain(
    sock: &UdpSocket,
    state: &State,
    buf: &mut [u8; PACKET_LEN],
    src: &mut Source,
    node: &Node,
    now_ms: u64,
) -> u32 {
    let mut other = 0u32;
    for _ in 0..DRAIN_MAX {
        let (len, from) = match sock.recv_from(buf) {
            Ok(r) => r,
            // An empty queue gives WouldBlock. The next loop pass tries again after an error.
            Err(_) => break,
        };
        let IpAddr::V4(ip) = from.ip() else {
            continue;
        };
        if let Some(poll) = parse_artpoll(&buf[..len]) {
            if poll.wants(node.port_address) {
                // A poll of another desk without the flag keeps the desk with the flag.
                {
                    let mut poller = POLLER.lock().unwrap();
                    if poll.reply_on_change() {
                        *poller = Some(ip);
                    } else if *poller == Some(ip) {
                        *poller = None;
                    }
                }
                reply(sock, state, ip, node);
            }
            continue;
        }
        let Some((universe, data)) = parse_artdmx(&buf[..len]) else {
            continue;
        };
        if universe != node.port_address {
            other = other.saturating_add(1);
            continue;
        }
        let from = Sender {
            proto: Proto::Artnet,
            addr: ip.octets(),
            priority: ARTNET_PRIORITY,
            cid: [0; 16],
        };
        // Byte 12 is the sequence. 0 means that the desk sends no sequence.
        let seq = Some(buf[12]).filter(|&s| s != 0);
        src.accept(from, seq, data, now_ms);
    }
    other
}

/// Sends an ArtPollReply to the desk that asked for a reply after each change. The `dmx` thread
/// calls it when `node` changes.
pub fn changed(sock: &UdpSocket, state: &State, node: &Node) {
    let poller = *POLLER.lock().unwrap();
    if let Some(ip) = poller {
        reply(sock, state, ip, node);
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
    /// True when the address came from DHCP.
    pub dhcp: bool,
}

/// Sends an ArtPollReply to the desk at `desk`, on port 6454.
///
/// The reply gives the address of the interface in the subnet of the desk. The Ethernet port
/// comes first, then the joined WiFi network, then the own WiFi network. When no interface is in
/// the subnet of the desk, each interface with an address sends a broadcast reply. A desk with a
/// 2.x.x.x or 10.x.x.x address then also finds the box.
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
fn reply(sock: &UdpSocket, state: &State, desk: Ipv4Addr, node: &Node) {
    let ifaces = *state.artnet_ifaces.lock().unwrap();
    // The own WiFi network is always the last entry. Its MAC address names the box, like the
    // name of its WiFi network.
    let name_mac = ifaces[2].map_or([0; 6], |i| i.mac);
    match ifaces
        .into_iter()
        .flatten()
        .find(|i| crate::same_subnet(i.ip, desk, i.prefix))
    {
        Some(iface) => send(sock, &iface, desk, node, name_mac),
        None => {
            for iface in ifaces.into_iter().flatten() {
                send(sock, &iface, Ipv4Addr::BROADCAST, node, name_mac);
            }
        }
    }
}

/// Sends one ArtPollReply for `iface` to `to`.
#[inline(never)]
fn send(sock: &UdpSocket, iface: &Iface, to: Ipv4Addr, node: &Node, name_mac: [u8; 6]) {
    let count = REPLIES.fetch_add(1, Ordering::Relaxed).wrapping_add(1) % 10_000;
    let short = SHORT_NAME
        .get_or_init(|| format!("AUTOBLINK-{:02X}{:02X}", name_mac[4], name_mac[5]))
        .as_str();
    let report = format!("#0001 [{count:04}] AUTOBLINK OK");
    let reply = build_artpoll_reply(&NodeInfo {
        ip: iface.ip.octets(),
        mac: iface.mac,
        version: (
            env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
            env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
        ),
        short_name: short,
        long_name: "AUTOBLINK beat-driven DMX box",
        report: &report,
        port_address: node.port_address,
        sending: node.sending,
        sacn: node.sacn,
        merging: node.merging,
        continuous: node.continuous,
        dhcp: iface.dhcp,
        failsafe_zero: node.failsafe_zero,
    });
    let fd = sock.as_raw_fd();
    let bound = crate::net::bind_device(fd, iface.netif).is_ok();
    let sent = sock.send_to(&reply, SocketAddr::from((to, ARTNET_PORT)));
    if let Err(e) = crate::net::bind_device(fd, "") {
        log::warn!(
            "Art-Net: the socket stays bound to \"{}\": {e}",
            iface.netif
        );
    }
    // A desk polls every few seconds. Only the first failure after a good reply goes into the log,
    // so that the log ring keeps the other lines.
    match sent {
        Err(e) if !POLL_FAILED.swap(true, Ordering::Relaxed) => {
            log::warn!("Art-Net: the ArtPollReply to {to} failed: {e}")
        }
        Err(_) => {}
        Ok(_) if POLL_FAILED.swap(false, Ordering::Relaxed) => {
            log::info!("Art-Net: the ArtPollReply to {to} goes out again")
        }
        Ok(_) if !FIRST_POLL.swap(true, Ordering::Relaxed) => log::info!(
            "Art-Net: the first ArtPollReply went to {to}. It gives {} (interface \"{}\", bound: {bound})",
            iface.ip,
            iface.netif
        ),
        Ok(_) => {}
    }
}

/// The desk that asked for an ArtPollReply after each change (ArtPoll flags bit 1).
static POLLER: Mutex<Option<Ipv4Addr>> = Mutex::new(None);
/// The ArtPollReply counter of the node report.
static REPLIES: AtomicU32 = AtomicU32::new(0);
/// The short name of the node, `AUTOBLINK-XXXX`.
static SHORT_NAME: OnceLock<String> = OnceLock::new();
/// True after the first reply. The log shows only the first one.
static FIRST_POLL: AtomicBool = AtomicBool::new(false);
/// True after a failed ArtPollReply, until a reply goes out again.
static POLL_FAILED: AtomicBool = AtomicBool::new(false);
