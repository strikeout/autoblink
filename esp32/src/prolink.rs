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

use autoblink_core_prolink::{
    build_announce, build_claim1, build_claim2, build_claim_final, build_keepalive,
    build_status_packet, has_magic, pad_name, parse_abs_position, parse_beat, parse_cdj_status,
    parse_keepalive, TrackRef, PKT_CONFLICT, PORT_BEAT, PORT_DISCOVERY, PORT_STATUS,
};
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::sys;

use crate::midi::Beat;
use crate::State;

const NAME: &str = "autoblink";
const CLAIM_INTERVAL: Duration = Duration::from_millis(300);
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(1500);
/// The number of join packets: 3 announces, then 3 packets of each of the three claims.
const JOIN_STEPS: u8 = 12;

/// The interface of the virtual CDJ and its address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkInfo {
    /// The lwIP name of the interface.
    pub netif: &'static str,
    pub ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
    pub mac: [u8; 6],
}

/// Starts the receive thread, which also runs the virtual CDJ.
///
/// `on_master_beat` gets each beat of the master deck. `on_track` gets each change of a loaded
/// track. `device_number` is the number that the virtual CDJ asks for.
///
/// One thread receives on the three sockets with `poll`. It has priority 8, above the HTTP server
/// task, and runs on core 1, which does not run the WiFi task. The task watchdog watches it.
/// Between two receives, the thread does the due step of the virtual CDJ (`VirtualCdj::tick`).
pub fn spawn(
    state: Arc<State>,
    device_number: Arc<AtomicU8>,
    on_master_beat: impl Fn(Beat) + Send + 'static,
    on_track: impl Fn(TrackRef) + Send + 'static,
) -> anyhow::Result<()> {
    let discovery = UdpSocket::bind(("0.0.0.0", PORT_DISCOVERY))?;
    discovery.set_broadcast(true)?;
    let beats = UdpSocket::bind(("0.0.0.0", PORT_BEAT))?;
    let status = UdpSocket::bind(("0.0.0.0", PORT_STATUS))?;
    // The sockets stay open for the run time of the firmware, so the descriptors stay valid.
    let fds = [discovery.as_raw_fd(), beats.as_raw_fd(), status.as_raw_fd()];

    // 1.32.3 used 3376 bytes of the stack without the virtual CDJ (/mem). The virtual CDJ thread
    // used about 2 KB. Its steps run in their own frame, beside the packet handlers.
    // ponytail: 5.5 KB is an estimate. Measure with /mem on a box after a join, and set the size
    // from the low point.
    crate::spawn_rt(c"prolink-rx", 5632, 8, Some(Core::Core1), move || {
        // SAFETY: a null handle subscribes the calling task.
        if let Err(e) = sys::esp!(unsafe { sys::esp_task_wdt_add(std::ptr::null_mut()) }) {
            log::warn!("The task watchdog does not watch the Pro DJ Link receive thread: {e}");
        }
        // A CDJ-3000 class status packet has more than 0x1c0 bytes.
        let mut buf = [0u8; 1500];
        let mut polls = fds.map(|fd| sys::pollfd {
            fd,
            events: sys::POLLIN as _,
            revents: 0,
        });
        let mut cdj = VirtualCdj {
            name: pad_name(NAME),
            bound: "",
            phase: Phase::Idle,
            next: Instant::now(),
        };
        loop {
            // SAFETY: the task subscribed above. A reset of an unsubscribed task only gives an
            // error code.
            unsafe { sys::esp_task_wdt_reset() };
            if Instant::now() >= cdj.next {
                cdj.tick(&discovery, &state, &device_number, &fds);
            }
            // The poll waits until the next step of the virtual CDJ, and at most 1 s. Thus each
            // loop pass stays shorter than the watchdog time of 5 s.
            let wait = cdj
                .next
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1));
            let timeout = wait.as_micros().div_ceil(1000) as i32;
            // SAFETY: `polls` is an array of valid `pollfd`, and the count is its length.
            if unsafe { sys::poll(polls.as_mut_ptr(), polls.len() as _, timeout) } < 0 {
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
                let sock = [&discovery, &beats, &status][i];
                let Ok((len, _)) = sock.recv_from(&mut buf) else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let data = &buf[..len];
                match i {
                    0 => {
                        if let Some(number) = on_discovery(&state, data) {
                            cdj.on_conflict(number);
                        }
                    }
                    1 => on_beat_port(&state, data, &on_master_beat),
                    _ => on_status(&state, data, &on_track),
                }
            }
        }
    })?;
    Ok(())
}

/// Handles a packet on port 50000: a conflict report or a keepalive. Returns the device number of
/// a conflict report.
fn on_discovery(st: &State, data: &[u8]) -> Option<u8> {
    if data.len() > 0x24 && has_magic(data) && data[0x0a] == PKT_CONFLICT {
        return Some(data[0x24]);
    }
    if let Some(ka) = parse_keepalive(data) {
        let now = st.now_ms();
        st.tracker.lock().unwrap().on_keepalive(&ka, now);
    }
    None
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
            log::debug!(
                "Bar: deck {} at {:.2} BPM",
                beat.device_number,
                beat.effective_bpm
            );
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
    let Some(cdj) = parse_cdj_status(data) else {
        return;
    };
    let now = st.now_ms();
    let change = st.tracker.lock().unwrap().on_cdj_status(&cdj, now);
    if let Some(track) = change {
        log::info!(
            "Deck {} loaded track {} (slot {}, player {})",
            track.device,
            track.rekordbox_id,
            track.slot,
            track.source_player
        );
        on_track(track);
    }
}

/// The step of the virtual CDJ.
#[derive(Clone, Copy)]
enum Phase {
    /// No join runs. The next tick binds the sockets to the interface of the link. When the link
    /// has an address, the tick starts a join.
    Idle,
    /// The join sends packet `step` next: 0 to 2 are the announces, 3 to 5 the first claim, 6 to
    /// 8 the second claim of `claimed`, and 9 to 11 the final claim.
    Join {
        link: LinkInfo,
        want: u8,
        step: u8,
        claimed: u8,
    },
    /// The virtual CDJ is on the network as `claimed`. `errors` counts the failed keepalives in a
    /// row.
    Run {
        link: LinkInfo,
        want: u8,
        claimed: u8,
        errors: u8,
    },
}

/// The virtual CDJ. It joins the network, then sends the keepalive and the status packet every
/// 1.5 s. It joins again when the interface, its address or the wanted device number changes.
///
/// The receive thread calls `tick` at the time `next`, and gives it each conflict report.
struct VirtualCdj {
    name: [u8; 20],
    /// The interface of the binding of the sockets. An empty name is for all interfaces.
    bound: &'static str,
    phase: Phase,
    /// The time of the next step.
    next: Instant,
}

impl VirtualCdj {
    /// Does the due step and sets the time of the next step. The step binds the sockets `fds` to
    /// the interface of the link.
    // The step has its own stack frame. Thus the frame of the receive loop does not hold its
    // packets.
    #[inline(never)]
    fn tick(&mut self, sock: &UdpSocket, state: &State, wanted: &AtomicU8, fds: &[RawFd]) {
        let now = Instant::now();
        let link = *state.link.lock().unwrap();
        let want = wanted.load(Ordering::Relaxed);
        if let Phase::Join {
            link: l, want: w, ..
        }
        | Phase::Run {
            link: l, want: w, ..
        } = self.phase
        {
            if Some(l) != link || w != want {
                self.leave(state);
            }
        }
        if let Phase::Idle = self.phase {
            let netif = link.map_or("", |l| l.netif);
            if netif != self.bound && bind_netif(fds, netif) {
                self.bound = netif;
            }
            let Some(link) = link else {
                self.next = now + Duration::from_secs(1);
                return;
            };
            self.phase = Phase::Join {
                link,
                want,
                step: 0,
                claimed: want,
            };
        }
        if let Phase::Join {
            link,
            want,
            step,
            claimed,
        } = self.phase
        {
            if step < JOIN_STEPS {
                let bcast = SocketAddrV4::new(link.broadcast, PORT_DISCOVERY);
                let (name, ip, mac) = (&self.name, link.ip.octets(), link.mac);
                let sent = match step {
                    0..=2 => sock.send_to(&build_announce(name), bcast),
                    3..=5 => sock.send_to(&build_claim1(name, &mac, step - 2), bcast),
                    6..=8 => sock.send_to(&build_claim2(name, &ip, &mac, claimed, step - 5), bcast),
                    _ => sock.send_to(&build_claim_final(name, claimed, step - 8), bcast),
                };
                if let Err(e) = sent {
                    log::warn!("The virtual CDJ cannot join: {e}");
                    self.phase = Phase::Idle;
                    self.next = now + Duration::from_secs(5);
                    return;
                }
                self.phase = Phase::Join {
                    link,
                    want,
                    step: step + 1,
                    claimed,
                };
                self.next = now + CLAIM_INTERVAL;
                return;
            }
            log::info!("The virtual CDJ is on the network as device {claimed}");
            state.own_device.store(claimed, Ordering::Relaxed);
            state.tracker.lock().unwrap().set_own_device(claimed);
            self.phase = Phase::Run {
                link,
                want,
                claimed,
                errors: 0,
            };
        }
        if let Phase::Run {
            link,
            want,
            claimed,
            mut errors,
        } = self.phase
        {
            let bcast = SocketAddrV4::new(link.broadcast, PORT_DISCOVERY);
            let keepalive = build_keepalive(&self.name, claimed, &link.mac, &link.ip.octets(), 1);
            if sock.send_to(&keepalive, bcast).is_err() {
                errors += 1;
                if errors >= 5 {
                    log::warn!("The keepalive cannot go out. The virtual CDJ joins again.");
                    self.leave(state);
                    self.next = now;
                    return;
                }
            } else {
                errors = 0;
            }
            // The players send their status only to a device that sends its own status to them.
            let status = build_status_packet(&self.name, claimed);
            let ips: Vec<[u8; 4]> = state
                .tracker
                .lock()
                .unwrap()
                .decks()
                .iter()
                .map(|d| d.ip)
                .collect();
            for ip in ips.into_iter().filter(|ip| *ip != [0; 4]) {
                let _ = sock.send_to(&status, SocketAddrV4::new(ip.into(), PORT_STATUS));
            }
            self.phase = Phase::Run {
                link,
                want,
                claimed,
                errors,
            };
            self.next = now + KEEPALIVE_INTERVAL;
        }
    }

    /// Takes a conflict report for device `number`. In the 300 ms after a second claim, a report
    /// for the claimed number makes the virtual CDJ claim the next number at once.
    fn on_conflict(&mut self, number: u8) {
        let Phase::Join {
            link,
            want,
            step: 7..=9,
            claimed,
        } = self.phase
        else {
            return;
        };
        if number != claimed {
            return;
        }
        if claimed >= 15 {
            log::warn!("The virtual CDJ cannot join: no free device number is available");
            self.phase = Phase::Idle;
            self.next = Instant::now() + Duration::from_secs(5);
            return;
        }
        log::warn!(
            "Device number {claimed} is in use. The virtual CDJ tries {}.",
            claimed + 1
        );
        self.phase = Phase::Join {
            link,
            want,
            step: 6,
            claimed: claimed + 1,
        };
        self.next = Instant::now();
    }

    /// Leaves the network. The next tick binds the sockets and joins again.
    fn leave(&mut self, state: &State) {
        state.own_device.store(0, Ordering::Relaxed);
        self.phase = Phase::Idle;
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
