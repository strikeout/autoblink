//! Pro DJ Link on the Ethernet port.
//!
//! The firmware joins the network as a virtual CDJ, like the host (`host/src/prolink/
//! virtual_cdj.rs`). The players then send it their CDJ status packets on port 50002. The
//! keepalive (port 50000), beat and absolute position (port 50001) packets are broadcasts.
//!
//! The XDJ-AZ sends the track ID in its status packets only to a virtual CDJ with device number
//! 7 or higher. The default is 7, the same as the host. Do not run the host and the firmware on
//! one network with the same number.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use xdj_core_prolink::{
    build_announce, build_claim1, build_claim2, build_claim_final, build_keepalive,
    build_status_packet, has_magic, pad_name, parse_abs_position, parse_beat, parse_cdj_status,
    parse_keepalive, TrackRef, PKT_CONFLICT, PORT_BEAT, PORT_DISCOVERY, PORT_STATUS,
};

use crate::midi::Beat;
use crate::State;

const NAME: &str = "xdj-clock";
const CLAIM_INTERVAL: Duration = Duration::from_millis(300);
const CLAIM_ROUNDS: u8 = 3;
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1500);

/// The address of the Ethernet port, for the packets of the virtual CDJ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthInfo {
    pub ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
    pub mac: [u8; 6],
}

/// Starts the receive threads and the virtual CDJ.
///
/// `on_master_beat` gets each beat of the master deck. `on_track` gets each change of a loaded
/// track. `device_number` is the number that the virtual CDJ asks for.
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

    let st = state.clone();
    spawn_loop("prolink-discovery", move |buf| {
        let Ok((len, _)) = discovery.recv_from(buf) else { return };
        let data = &buf[..len];
        if data.len() > 0x24 && has_magic(data) && data[0x0a] == PKT_CONFLICT {
            st.conflict.store(data[0x24], Ordering::Relaxed);
        } else if let Some(ka) = parse_keepalive(data) {
            let now = st.now_ms();
            st.tracker.lock().unwrap().on_keepalive(&ka, now);
        }
    })?;

    let st = state.clone();
    spawn_loop("prolink-beats", move |buf| {
        let Ok((len, _)) = beats.recv_from(buf) else { return };
        let at_us = st.now_us();
        let now = at_us / 1000;
        if let Some(p) = parse_abs_position(&buf[..len]) {
            st.tracker.lock().unwrap().on_abs_position(&p, now);
            return;
        }
        let Some(beat) = parse_beat(&buf[..len]) else { return };
        let is_master = st.tracker.lock().unwrap().on_beat(&beat, now);
        if is_master {
            if beat.beat_in_bar == 1 {
                log::info!("Bar: deck {} at {:.2} BPM", beat.device_number, beat.effective_bpm);
            }
            on_master_beat(Beat {
                device: beat.device_number,
                beat_in_bar: beat.beat_in_bar,
                bpm: beat.effective_bpm,
                at_us,
            });
        }
    })?;

    let st = state.clone();
    spawn_loop("prolink-status", move |buf| {
        let Ok((len, _)) = status.recv_from(buf) else { return };
        let Some(cdj) = parse_cdj_status(&buf[..len]) else { return };
        let now = st.now_ms();
        let change = st.tracker.lock().unwrap().on_cdj_status(&cdj, now);
        if let Some(track) = change {
            log::info!(
                "Deck {} loaded track {} (slot {}, player {})",
                track.device, track.rekordbox_id, track.slot, track.source_player
            );
            on_track(track);
        }
    })?;

    thread::Builder::new()
        .name("virtual-cdj".into())
        .stack_size(6 * 1024)
        .spawn(move || virtual_cdj(&sender, &state, &device_number))?;
    Ok(())
}

fn spawn_loop(name: &str, mut f: impl FnMut(&mut [u8]) + Send + 'static) -> anyhow::Result<()> {
    thread::Builder::new()
        .name(name.into())
        .stack_size(6 * 1024)
        .spawn(move || {
            // A CDJ-3000 class status packet has more than 0x1c0 bytes.
            let mut buf = [0u8; 1500];
            loop {
                f(&mut buf);
            }
        })?;
    Ok(())
}

/// Joins the network, then sends the keepalive and the status packet every 1.5 s. It joins again
/// when the Ethernet address or the wanted device number changes.
fn virtual_cdj(sock: &UdpSocket, state: &State, wanted: &AtomicU8) {
    let name = pad_name(NAME);
    loop {
        let Some(eth) = *state.eth.lock().unwrap() else {
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
        while *state.eth.lock().unwrap() == Some(eth) && wanted.load(Ordering::Relaxed) == want {
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

/// Sends the announce and claim packets. Returns the claimed device number. When another device
/// reports a conflict, the next number is tried.
fn join(sock: &UdpSocket, state: &State, name: &[u8; 20], eth: &EthInfo, want: u8) -> anyhow::Result<u8> {
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
