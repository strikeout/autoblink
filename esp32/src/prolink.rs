//! Pro DJ Link on the Ethernet port, or on the joined WiFi network.
//!
//! The firmware joins the network as a virtual CDJ, like the host (`host/src/prolink/
//! virtual_cdj.rs`). The players then send it their CDJ status packets on port 50002. The
//! keepalive (port 50000), beat and absolute position (port 50001) packets are broadcasts.
//!
//! The virtual CDJ uses the Ethernet port while it has an address. Without an Ethernet address,
//! it uses the joined WiFi network. When the two interfaces have an address and the virtual CDJ
//! gets no packet from a player for 5 s, it changes to the other interface (see `main.rs`). The
//! XDJ-AZ sends Pro DJ Link on one interface only, and it can leave the cable for its WiFi
//! network. The sockets receive and send only on that interface (lwIP
//! `SO_BINDTODEVICE`). Without this binding, a joined WiFi network in the subnet of the Ethernet
//! port gives a second, later copy of each broadcast. Also, a joined WiFi network in the subnet
//! of the own WiFi network (192.168.1.x) gets no packets: lwIP can send them on the own network.
//! Without an address on the two interfaces, the sockets use all interfaces.
//!
//! The XDJ-AZ sends the track ID in its status packets only to a virtual CDJ with device number
//! 7 or higher. The default is 7, the same as the host. Do not run the host and the firmware on
//! one network with the same number.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::sys;
use autoblink_core_prolink::{
    build_announce, build_claim1, build_claim2, build_claim_final, build_keepalive,
    build_status_packet, has_magic, pad_name, parse_abs_position, parse_beat, parse_cdj_status,
    parse_keepalive, TrackRef, PKT_CONFLICT, PORT_BEAT, PORT_DISCOVERY, PORT_STATUS,
};

use crate::midi::Beat;
use crate::State;

const NAME: &str = "autoblink";
const CLAIM_INTERVAL: Duration = Duration::from_millis(300);
const CLAIM_ROUNDS: u8 = 3;
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1500);

/// The interface of the virtual CDJ and its address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkInfo {
    /// The lwIP name of the interface.
    pub netif: &'static str,
    pub ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
    pub mac: [u8; 6],
}

/// Starts the receive thread and the virtual CDJ.
///
/// `on_master_beat` gets each beat of the master deck. `on_track` gets each change of a loaded
/// track. `device_number` is the number that the virtual CDJ asks for.
///
/// One thread receives on the three sockets with `poll`. It has priority 8, above the HTTP server
/// task, and runs on core 1, which does not run the WiFi task. The task watchdog watches it.
pub fn spawn(
    state: Arc<State>,
    device_number: Arc<AtomicU8>,
    on_master_beat: impl Fn(Beat) + Send + 'static,
    on_track: impl Fn(TrackRef) + Send + 'static,
) -> anyhow::Result<()> {
    // ESP-IDF has no dup() for sockets, so `try_clone` fails. The receive thread and the virtual
    // CDJ share one socket through an `Arc`.
    let discovery = Arc::new(UdpSocket::bind(("0.0.0.0", PORT_DISCOVERY))?);
    discovery.set_broadcast(true)?;
    let sender = discovery.clone();
    let beats = UdpSocket::bind(("0.0.0.0", PORT_BEAT))?;
    let status = UdpSocket::bind(("0.0.0.0", PORT_STATUS))?;
    // The sockets stay open for the run time of the firmware, so the descriptors stay valid.
    let fds = [discovery.as_raw_fd(), beats.as_raw_fd(), status.as_raw_fd()];

    let st = state.clone();
    crate::spawn_rt(c"prolink-rx", 6 * 1024, 8, Some(Core::Core1), move || {
        // SAFETY: a null handle subscribes the calling task.
        if let Err(e) = sys::esp!(unsafe { sys::esp_task_wdt_add(std::ptr::null_mut()) }) {
            log::warn!("The task watchdog does not watch the Pro DJ Link receive thread: {e}");
        }
        // A CDJ-3000 class status packet has more than 0x1c0 bytes.
        let mut buf = [0u8; 1500];
        let mut polls = fds.map(|fd| sys::pollfd { fd, events: sys::POLLIN as _, revents: 0 });
        loop {
            // SAFETY: the task subscribed above. A reset of an unsubscribed task only gives an
            // error code.
            unsafe { sys::esp_task_wdt_reset() };
            // The timeout of 1 s keeps each loop pass shorter than the watchdog time of 5 s.
            // SAFETY: `polls` is an array of valid `pollfd`, and the count is its length.
            if unsafe { sys::poll(polls.as_mut_ptr(), polls.len() as _, 1000) } < 0 {
                // Do not spin at this priority. The idle task must run, or the watchdog restarts
                // the firmware.
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            for (i, p) in polls.iter_mut().enumerate() {
                if p.revents == 0 {
                    continue;
                }
                p.revents = 0;
                let sock = [&*discovery, &beats, &status][i];
                let Ok((len, _)) = sock.recv_from(&mut buf) else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let data = &buf[..len];
                match i {
                    0 => on_discovery(&st, data),
                    1 => on_beat_port(&st, data, &on_master_beat),
                    _ => on_status(&st, data, &on_track),
                }
            }
        }
    })?;

    crate::spawn(c"virtual-cdj", 4608, move || virtual_cdj(&sender, &state, &device_number, &fds))?;
    Ok(())
}

/// Handles a packet on port 50000: a conflict report or a keepalive.
fn on_discovery(st: &State, data: &[u8]) {
    if data.len() > 0x24 && has_magic(data) && data[0x0a] == PKT_CONFLICT {
        st.conflict.store(data[0x24], Ordering::Relaxed);
    } else if let Some(ka) = parse_keepalive(data) {
        let now = st.now_ms();
        st.tracker.lock().unwrap().on_keepalive(&ka, now);
    }
}

/// Handles a packet on port 50001: an absolute position or a beat.
fn on_beat_port(st: &State, data: &[u8], on_master_beat: &impl Fn(Beat)) {
    let at_us = st.now_us();
    let now = at_us / 1000;
    if let Some(p) = parse_abs_position(data) {
        st.tracker.lock().unwrap().on_abs_position(&p, now);
        return;
    }
    let Some(beat) = parse_beat(data) else { return };
    let is_master = st.tracker.lock().unwrap().on_beat(&beat, now);
    if is_master {
        if beat.beat_in_bar == 1 {
            log::debug!("Bar: deck {} at {:.2} BPM", beat.device_number, beat.effective_bpm);
        }
        on_master_beat(Beat {
            device: beat.device_number,
            beat_in_bar: beat.beat_in_bar,
            bpm: beat.effective_bpm,
            at_us,
        });
    }
}

/// Handles a packet on port 50002: the CDJ status of a player.
fn on_status(st: &State, data: &[u8], on_track: &impl Fn(TrackRef)) {
    let Some(cdj) = parse_cdj_status(data) else { return };
    let now = st.now_ms();
    let change = st.tracker.lock().unwrap().on_cdj_status(&cdj, now);
    if let Some(track) = change {
        log::info!(
            "Deck {} loaded track {} (slot {}, player {})",
            track.device, track.rekordbox_id, track.slot, track.source_player
        );
        on_track(track);
    }
}

/// Joins the network, then sends the keepalive and the status packet every 1.5 s. It joins again
/// when the interface, its address or the wanted device number changes.
///
/// It binds the sockets `fds` to the interface of the link.
fn virtual_cdj(sock: &UdpSocket, state: &State, wanted: &AtomicU8, fds: &[RawFd]) {
    let name = pad_name(NAME);
    // The interface of the binding. An empty name is for all interfaces.
    let mut bound = "";
    loop {
        let link = *state.link.lock().unwrap();
        let netif = link.map_or("", |l| l.netif);
        if netif != bound && bind_netif(fds, netif) {
            bound = netif;
        }
        let Some(eth) = link else {
            thread::sleep(Duration::from_secs(1));
            continue;
        };
        let want = wanted.load(Ordering::Relaxed);
        let claimed = match join(sock, state, &name, &eth, want) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("The virtual CDJ cannot join: {e}");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        log::info!("The virtual CDJ is on the network as device {claimed}");
        state.own_device.store(claimed, Ordering::Relaxed);
        state.tracker.lock().unwrap().set_own_device(claimed);

        let bcast = SocketAddrV4::new(eth.broadcast, PORT_DISCOVERY);
        let keepalive = build_keepalive(&name, claimed, &eth.mac, &eth.ip.octets(), 1);
        let status = build_status_packet(&name, claimed);
        let mut errors = 0;
        while *state.link.lock().unwrap() == Some(eth) && wanted.load(Ordering::Relaxed) == want {
            if sock.send_to(&keepalive, bcast).is_err() {
                errors += 1;
                if errors >= 5 {
                    log::warn!("The keepalive cannot go out. The virtual CDJ joins again.");
                    break;
                }
            } else {
                errors = 0;
            }
            // The players send their status only to a device that sends its own status to them.
            let ips: Vec<[u8; 4]> = state.tracker.lock().unwrap().decks().iter().map(|d| d.ip).collect();
            for ip in ips.into_iter().filter(|ip| *ip != [0; 4]) {
                let _ = sock.send_to(&status, SocketAddrV4::new(ip.into(), PORT_STATUS));
            }
            thread::sleep(KEEPALIVE_INTERVAL);
        }
        state.own_device.store(0, Ordering::Relaxed);
    }
}

/// Binds each socket in `fds` to the network interface `netif`. An empty `netif` removes the
/// binding. Returns true when all calls succeed.
fn bind_netif(fds: &[RawFd], netif: &str) -> bool {
    let mut ok = true;
    for &fd in fds {
        if let Err(e) = crate::net::bind_device(fd, netif) {
            log::warn!("Socket {fd}: the binding to \"{netif}\" failed: {e}");
            ok = false;
        }
    }
    if ok && netif.is_empty() {
        log::info!("Pro DJ Link uses all interfaces");
    } else if ok {
        log::info!("Pro DJ Link uses only the interface \"{netif}\"");
    }
    ok
}

/// Sends the announce and claim packets. Returns the claimed device number. When another device
/// reports a conflict, the next number is tried.
fn join(sock: &UdpSocket, state: &State, name: &[u8; 20], eth: &LinkInfo, want: u8) -> anyhow::Result<u8> {
    let bcast = SocketAddrV4::new(eth.broadcast, PORT_DISCOVERY);
    let ip = eth.ip.octets();
    let send = |pkt: &[u8]| -> anyhow::Result<()> {
        sock.send_to(pkt, bcast)?;
        thread::sleep(CLAIM_INTERVAL);
        Ok(())
    };
    let announce = build_announce(name);
    for _ in 0..CLAIM_ROUNDS {
        send(&announce)?;
    }
    for n in 1..=CLAIM_ROUNDS {
        send(&build_claim1(name, &eth.mac, n))?;
    }
    let mut claimed = want;
    'claim: loop {
        state.conflict.store(0, Ordering::Relaxed);
        for n in 1..=CLAIM_ROUNDS {
            sock.send_to(&build_claim2(name, &ip, &eth.mac, claimed, n), bcast)?;
            let deadline = Instant::now() + CLAIM_INTERVAL;
            while Instant::now() < deadline {
                if state.conflict.swap(0, Ordering::Relaxed) == claimed {
                    log::warn!("Device number {claimed} is in use. The virtual CDJ tries {}.", claimed + 1);
                    claimed += 1;
                    anyhow::ensure!(claimed <= 15, "No free device number is available");
                    continue 'claim;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        break;
    }
    for n in 1..=CLAIM_ROUNDS {
        send(&build_claim_final(name, claimed, n))?;
    }
    Ok(claimed)
}
