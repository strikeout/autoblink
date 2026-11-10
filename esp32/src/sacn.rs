//! sACN (E1.31) from a lighting desk. The desk sends to UDP port 5568, as multicast to
//! 239.255.hi.lo of the universe, or as unicast to the box. The setting "sACN Universe" gives the
//! universe that the box receives.
//!
//! The `dmx` thread reads the socket without a wait on each loop pass (`drain`). Each frame of
//! the universe goes to `autoblink_core_dmx::Source`, like an Art-Net frame. Once a second, the
//! thread joins the multicast group on each interface with an address (`Groups::update`).

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use autoblink_core_dmx::{multicast_group, parse_sacn, Proto, Sender, Source, SACN_PORT};

use crate::artnet::Iface;
use crate::State;

/// The root, framing and DMP layers (126 bytes with the start code) and 512 slots.
pub const PACKET_LEN: usize = 126 + 512;

/// The most packets that one `drain` reads, as for Art-Net.
const DRAIN_MAX: usize = 16;

/// Opens the sACN socket and gives it to the `dmx` thread. Without the port, the firmware logs a
/// warning and continues without sACN.
pub fn start(state: &State) {
    let sock = match UdpSocket::bind(("0.0.0.0", SACN_PORT)) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("sACN is off: port {SACN_PORT} is not available: {e}");
            return;
        }
    };
    if let Err(e) = sock.set_nonblocking(true) {
        log::warn!("sACN is off: the socket cannot be non-blocking: {e}");
        return;
    }
    log::info!("sACN: the firmware receives on port {SACN_PORT}");
    let _ = state.sacn.set(sock);
}

/// Reads the waiting packets of `sock` without a wait. Gives each frame of `universe` to `src`.
/// Returns the number of frames of other universes.
#[inline(never)]
pub fn drain(
    sock: &UdpSocket,
    buf: &mut [u8; PACKET_LEN],
    src: &mut Source,
    universe: u16,
    now_ms: u64,
) -> u32 {
    let mut other = 0u32;
    for _ in 0..DRAIN_MAX {
        let (len, from) = match sock.recv_from(buf) {
            Ok(r) => r,
            Err(_) => break,
        };
        let SocketAddr::V4(from) = from else {
            continue;
        };
        let Some(p) = parse_sacn(&buf[..len]) else {
            continue;
        };
        if p.universe != universe {
            other = other.saturating_add(1);
            continue;
        }
        let sender = Sender {
            proto: Proto::Sacn,
            addr: from.ip().octets(),
            priority: p.priority,
            cid: p.cid,
        };
        if p.terminated {
            src.end(sender, now_ms);
        } else {
            src.accept(sender, Some(p.sequence), p.data, now_ms);
        }
    }
    other
}

/// The multicast memberships of the socket: the group and the interface address of each join.
#[derive(Default)]
pub struct Groups {
    joined: [Option<(Ipv4Addr, Ipv4Addr)>; 3],
}

impl Groups {
    /// Joins the group of `universe` on each interface with an address, and leaves the old
    /// memberships. A change of an address or of the universe changes the memberships.
    pub fn update(&mut self, sock: &UdpSocket, ifaces: &[Option<Iface>; 3], universe: u16) {
        let group = multicast_group(universe);
        for (slot, iface) in self.joined.iter_mut().zip(ifaces) {
            let want = iface.map(|i| (group, i.ip));
            if *slot == want {
                continue;
            }
            if let Some((g, ip)) = slot.take() {
                let _ = sock.leave_multicast_v4(&g, &ip);
            }
            if let Some((g, ip)) = want {
                match sock.join_multicast_v4(&g, &ip) {
                    Ok(()) => {
                        log::info!("sACN: the firmware joins {g} on {ip}");
                        JOIN_FAILED.store(false, Relaxed);
                        *slot = want;
                    }
                    // lwIP keeps the membership entry of a failed join. The leave frees it. The
                    // next update tries again, and only the first failure goes into the log.
                    Err(e) => {
                        let _ = sock.leave_multicast_v4(&g, &ip);
                        if !JOIN_FAILED.swap(true, Relaxed) {
                            log::warn!("sACN: the join of {g} on {ip} failed: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// True after the first failed join.
static JOIN_FAILED: AtomicBool = AtomicBool::new(false);
