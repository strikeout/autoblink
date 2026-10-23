//! xdj-clock firmware for the WT32-ETH01 (ESP32 + LAN8720 Ethernet).
//!
//! The firmware joins the Pro DJ Link network on the Ethernet port as a virtual CDJ. It reads
//! the beats, the player status and the track data from the players. The MIDI output on IO17
//! sends the clock, the notes and the CCs. IO32 shows the beats. IO33 shows the beats and the
//! state of the box (see `led.rs`). The firmware serves the web
//! page on its own WiFi network at http://192.168.1.1. When the settings have a join network,
//! the page is also on that network, at the address from its DHCP server and at
//! http://xdj-clock.local.

#[cfg(feature = "dmx")]
mod dmx;
mod led;
mod logring;
mod mem;
mod midi;
mod net;
mod ota;
mod prolink;
mod settings;
mod tracks;
mod webui;

use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use xdj_core_prolink::Tracker;

use crate::midi::MidiStatus;
use crate::net::{EthPins, AP_IP};
use crate::ota::OtaStatus;
use crate::prolink::LinkInfo;
use crate::settings::Settings;
use crate::tracks::Tracks;

/// The state that the receive threads write and the web page reads.
pub struct State {
    pub tracker: Mutex<Tracker>,
    /// The Ethernet address, for the "iface" field of the status.
    pub iface: Mutex<String>,
    pub midi: Mutex<MidiStatus>,
    pub tracks: Tracks,
    /// The interface of Pro DJ Link: the Ethernet port, or the joined WiFi network when the port
    /// has no address or has no player. `None` while the two interfaces have no address.
    pub link: Mutex<Option<LinkInfo>>,
    /// The device number of the last conflict packet. The virtual CDJ reads it while it claims.
    pub conflict: AtomicU8,
    /// The device number of the virtual CDJ. 0 while it is not on the network.
    pub own_device: AtomicU8,
    /// `true` while the settings have a join network and the firmware is not connected to it.
    pub wifi_join_down: AtomicBool,
    /// The reason for the last restart, for example "power-on" or "panic".
    pub reset_reason: &'static str,
    /// The state of the firmware update for the page.
    pub ota: Mutex<OtaStatus>,
    start: Instant,
}

impl State {
    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

fn main() {
    sys::link_patches();
    logring::init();
    if let Err(e) = run() {
        // A start failure can be temporary, for example an Ethernet or NVS error. A restart
        // tries again. Without it, the box stays dead until a power cycle.
        log::error!("The firmware restarts: {e:#}");
        thread::sleep(Duration::from_secs(1));
        // SAFETY: esp_restart has no preconditions.
        unsafe { sys::esp_restart() };
    }
}

fn run() -> anyhow::Result<()> {
    // SAFETY: esp_reset_reason has no preconditions.
    let reset_reason = reset_reason(unsafe { sys::esp_reset_reason() });
    log::info!("xdj-clock {} starts. Reason for the restart: {reset_reason}", env!("CARGO_PKG_VERSION"));
    logring::report_core_dump();
    let p = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let (install, ota_error) = ota::take_request(&nvs)?;
    // IO33 stays on until the LED thread starts. Then it shows the state of the box.
    let mut status_led = PinDriver::output(p.pins.gpio33)?;
    status_led.set_high()?;

    let mut mac = [0u8; 6];
    // SAFETY: the buffer has the 6 bytes that a MAC address needs.
    sys::esp!(unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_SOFTAP) })?;
    let default_ssid = format!("xdj-clock-{:02X}{:02X}", mac[4], mac[5]);
    let settings = Settings::new(nvs.clone(), default_ssid)?;
    let ssid = settings.ssid();
    let join = settings.join();
    let join_ssid = join.as_ref().map(|(s, _)| s.clone());
    let mut wifi = net::start_wifi(p.modem, sysloop.clone(), nvs, &ssid, &settings.password(), join)?;
    log::info!("WiFi \"{ssid}\" is on. The web page is at http://{AP_IP}");
    if let Some(join) = &join_ssid {
        log::info!("WiFi \"{join}\": the firmware joins this network");
    }
    // mDNS answers on all networks. Without it, the page is still at its IP addresses.
    let _mdns = start_mdns().inspect_err(|e| log::warn!("mDNS is off: {e}")).ok();

    let (eth, _osc) = net::start_ethernet(
        EthPins {
            mac: p.mac,
            rxd0: p.pins.gpio25,
            rxd1: p.pins.gpio26,
            crs_dv: p.pins.gpio27,
            mdc: p.pins.gpio23,
            txd1: p.pins.gpio22,
            tx_en: p.pins.gpio21,
            txd0: p.pins.gpio19,
            mdio: p.pins.gpio18,
            clk: p.pins.gpio0,
            osc_enable: p.pins.gpio16,
        },
        sysloop,
    )?;

    // The maintenance start: only the network runs, so that TLS gets enough heap.
    if install {
        log::info!("Firmware update: the firmware starts only the network for the download");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let sta = wifi.sta_netif().get_ip_info().is_ok_and(|i| !i.ip.is_unspecified());
            let eth_ip = eth.netif().get_ip_info().is_ok_and(|i| !i.ip.is_unspecified() && !i.ip.is_link_local());
            if sta || eth_ip {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        // TLS needs more stack than the main task has, so a thread with 12 KB does the download.
        let _ = crate::spawn(c"ota", 12 * 1024, || -> () { ota::install_and_restart() })?.join();
        // SAFETY: esp_restart has no preconditions. The join returns only after a panic of the thread.
        unsafe { sys::esp_restart() };
    }

    let state = Arc::new(State {
        tracker: Mutex::new(Tracker::new()),
        iface: Mutex::new("Ethernet: no address".into()),
        midi: Mutex::new(MidiStatus::default()),
        tracks: Tracks::default(),
        link: Mutex::new(None),
        conflict: AtomicU8::new(0),
        own_device: AtomicU8::new(0),
        wifi_join_down: AtomicBool::new(false),
        reset_reason,
        ota: Mutex::new(OtaStatus {
            state: if ota_error.is_some() { "error" } else { "" },
            error: ota_error,
            ..Default::default()
        }),
        start: Instant::now(),
    });
    let midi_beats = midi::start(p.uart2, p.pins.gpio17, p.pins.gpio35, state.clone(), settings.midi.clone())?;
    let device_number = settings.device_number.clone();
    let settings = Arc::new(Mutex::new(settings));
    let (_server, web_beats) = webui::start(state.clone(), settings.clone())?;
    let track_changes = tracks::start(state.clone())?;

    let beat_tx = led::start(PinDriver::output(p.pins.gpio32)?, status_led, state.clone())?;

    #[cfg(feature = "dmx")]
    let dmx = dmx::Dmx::start(p.uart1, p.pins.gpio4, p.pins.gpio36)?;

    prolink::spawn(
        state.clone(),
        device_number,
        move |beat| {
            let _ = beat_tx.send(beat.beat_in_bar);
            #[cfg(feature = "dmx")]
            dmx.beat();
            let page_beat = (beat.device, beat.beat_in_bar);
            let _ = midi_beats.send(beat);
            // The web thread sends the beat to the page. This thread does not wait for it.
            let _ = web_beats.try_send(page_beat);
        },
        move |track| {
            let _ = track_changes.send(track);
        },
    )?;

    // The web server and the receive threads run. A restart before this line makes the
    // bootloader start the previous app.
    ota::mark_valid();

    // Log each change of the Ethernet address. A cable to the XDJ-AZ without a DHCP server
    // gives a 169.254.x.x address after some seconds.
    log::info!("Ethernet is waiting for a cable and an address");
    let eth_mac = eth.netif().get_mac()?;
    let sta_mac = wifi.sta_netif().get_mac()?;
    // The link info is `Copy`, so it keeps the interface names as static text. The two leaks
    // happen once.
    let eth_name: &'static str = String::leak(eth.netif().get_name().to_string());
    let sta_name: &'static str = String::leak(wifi.sta_netif().get_name().to_string());
    let mut last = (None, None);
    let mut next_heap_log = Instant::now();
    let mut heap_low = u32::MAX;
    let mut next_join = Instant::now() + JOIN_RETRY;
    // The interface choice while Ethernet and the joined WiFi network both have an address.
    let mut use_sta = false;
    let mut quiet_since: Option<Instant> = None;
    let mut dwell = SWITCH_AFTER;
    loop {
        state.tracker.lock().unwrap().expire(state.now_ms());
        // Free the track data of a deck that left the network.
        let decks: Vec<u8> = state.tracker.lock().unwrap().decks().iter().map(|d| d.num).collect();
        state.tracks.0.lock().unwrap().retain(|n, _| decks.contains(n));
        settings.lock().unwrap().save_midi();
        // SAFETY: the heap calls only read counters.
        let (free, min, block) = unsafe {
            (
                sys::esp_get_free_heap_size(),
                sys::esp_get_minimum_free_heap_size(),
                sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT),
            )
        };
        // The time of this line in GET /log shows the event that used the heap in the last second.
        if min.saturating_add(HEAP_LOW_STEP) <= heap_low {
            heap_low = min;
            log::warn!("Heap low point: {min} bytes. Now {free} bytes free, largest block {block} bytes");
        }
        if Instant::now() >= next_heap_log {
            next_heap_log += Duration::from_secs(60);
            log::info!(
                "Up {} s. Heap: {free} bytes free, {min} bytes minimum, largest block {block} bytes",
                state.now_ms() / 1000
            );
        }
        // ponytail: a fixed retry interval. Each try scans all channels, and the phones on the
        // access point can lose the page during a scan. Use an exponential back-off if 30 s is
        // too often.
        if join_ssid.is_some() && Instant::now() >= next_join {
            next_join = Instant::now() + JOIN_RETRY;
            if !wifi.is_connected().unwrap_or(false) {
                if let Err(e) = wifi.connect() {
                    log::warn!("WiFi: the connection did not start: {e}");
                }
            }
        }

        let info = eth.netif().get_ip_info().ok().filter(|i| !i.ip.is_unspecified());
        let eth_ip = info.map(|i| (i.ip, i.subnet.mask.0));
        let sta_info = join_ssid
            .as_ref()
            .and_then(|_| wifi.sta_netif().get_ip_info().ok())
            .filter(|i| !i.ip.is_unspecified());
        let sta_ip = sta_info.map(|i| i.ip);
        // The XDJ-AZ sends Pro DJ Link on one interface only. When it is also on the joined WiFi
        // network (for example with rekordbox), it can stop the packets on the cable. The virtual
        // CDJ then changes to the other interface after `dwell` without a player.
        // ponytail: the virtual CDJ tries the two interfaces in turn. If it changes too often,
        // read the interface of the players from an unbound keepalive socket.
        if info.is_some() && sta_info.is_some() {
            if !state.tracker.lock().unwrap().decks().is_empty() {
                quiet_since = None;
                dwell = SWITCH_AFTER;
            } else if quiet_since.get_or_insert_with(Instant::now).elapsed() >= dwell {
                let (from, to) = if use_sta { (sta_name, eth_name) } else { (eth_name, sta_name) };
                log::info!("No player on \"{from}\" for {} s. Pro DJ Link tries \"{to}\".", dwell.as_secs());
                use_sta = !use_sta;
                quiet_since = None;
                // Without a player on the two interfaces, the changes and their log lines become rare.
                dwell = (dwell * 2).min(SWITCH_AFTER_MAX);
            }
        } else {
            use_sta = false;
            quiet_since = None;
            dwell = SWITCH_AFTER;
        }
        let link = match (info, sta_info) {
            (Some(_), Some(i)) if use_sta => Some((sta_name, i, sta_mac)),
            (Some(i), _) => Some((eth_name, i, eth_mac)),
            (None, Some(i)) => Some((sta_name, i, sta_mac)),
            (None, None) => None,
        };
        *state.link.lock().unwrap() = link.map(|(netif, i, mac)| LinkInfo {
            netif,
            ip: i.ip,
            broadcast: broadcast(i.ip, i.subnet.mask.0),
            mac,
        });
        state.wifi_join_down.store(join_ssid.is_some() && sta_ip.is_none(), Ordering::Relaxed);
        let now = (eth_ip.map(|(ip, _)| ip), sta_ip);
        if now != last {
            if now.0 != last.0 {
                match eth_ip {
                    Some((ip, prefix)) => {
                        log::info!("Ethernet address: {ip}");
                        if same_subnet(ip, AP_IP, 24) {
                            log::warn!(
                                "The Ethernet network uses the subnet of the WiFi network ({AP_IP}/24). \
                                 Replies can go out on the wrong port."
                            );
                        }
                        if sta_ip.is_some_and(|sta| same_subnet(ip, sta, prefix)) {
                            log::warn!(
                                "The Ethernet network and the joined WiFi network use one subnet. \
                                 Pro DJ Link uses only the Ethernet port."
                            );
                        }
                    }
                    None => log::info!("Ethernet has no address"),
                }
            }
            if now.1 != last.1 {
                let name = join_ssid.as_deref().unwrap_or_default();
                match sta_ip {
                    Some(ip) => {
                        log::info!(
                            "WiFi \"{name}\" address: {ip}. The web page is at http://{ip} and http://{}.local",
                            net::MDNS_NAME
                        );
                        if same_subnet(ip, AP_IP, 24) {
                            log::warn!(
                                "The WiFi network \"{name}\" uses the subnet of the own WiFi network \
                                 ({AP_IP}/24). The page can fail on one of the two networks."
                            );
                        }
                    }
                    None => log::info!("WiFi \"{name}\" is not connected"),
                }
            }
            let mut iface = now.0.map_or("Ethernet: no address".into(), |ip| format!("Ethernet {ip}"));
            if let Some(ip) = now.1 {
                iface.push_str(&format!(" · WiFi {ip}"));
            }
            *state.iface.lock().unwrap() = iface;
            last = now;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Restarts the firmware after 1 s. In that time, the page gets the reply and the last status.
pub fn restart_soon() -> std::io::Result<()> {
    spawn(c"restart", 2 * 1024, || {
        thread::sleep(Duration::from_secs(1));
        // SAFETY: esp_restart has no preconditions.
        unsafe { sys::esp_restart() }
    })
    .map(|_| ())
}

/// Starts a thread. `name` is also the FreeRTOS task name, which `GET /mem` shows.
///
/// Without the spawn configuration, each Rust thread has the task name "pthread".
pub fn spawn<F, T>(name: &'static CStr, stack_size: usize, f: F) -> std::io::Result<thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // The configuration applies to the threads that the calling thread starts. Reset it after
    // the start, so that no other thread gets this name.
    let _ = ThreadSpawnConfiguration { name: Some(name), ..Default::default() }.set();
    let handle = thread::Builder::new()
        .name(name.to_string_lossy().into_owned())
        .stack_size(stack_size)
        .spawn(f);
    let _ = ThreadSpawnConfiguration::default().set();
    handle
}

/// The time between two tries to join the WiFi network.
const JOIN_RETRY: Duration = Duration::from_secs(30);

/// The time without a player after which Pro DJ Link tries the other interface.
const SWITCH_AFTER: Duration = Duration::from_secs(5);

/// The longest time between two interface changes while no interface has a player.
const SWITCH_AFTER_MAX: Duration = Duration::from_secs(60);

/// Starts mDNS with the name `MDNS_NAME` and an HTTP service on port 80.
fn start_mdns() -> anyhow::Result<EspMdns> {
    let mut mdns = EspMdns::take()?;
    mdns.set_hostname(net::MDNS_NAME)?;
    mdns.add_service(None, "_http", "_tcp", 80, &[])?;
    Ok(mdns)
}

/// Returns `true` when `a` and `b` are in one subnet with a prefix of `prefix` bits.
fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix.min(32))).unwrap_or(0);
    u32::from(a) & mask == u32::from(b) & mask
}

/// The main loop logs a new heap low point when the minimum falls by this many bytes.
const HEAP_LOW_STEP: u32 = 2048;

/// The name of a reset reason of `esp_reset_reason`.
fn reset_reason(reason: sys::esp_reset_reason_t) -> &'static str {
    match reason {
        sys::esp_reset_reason_t_ESP_RST_POWERON => "power-on",
        sys::esp_reset_reason_t_ESP_RST_EXT => "external pin",
        sys::esp_reset_reason_t_ESP_RST_SW => "software restart",
        sys::esp_reset_reason_t_ESP_RST_PANIC => "panic",
        sys::esp_reset_reason_t_ESP_RST_INT_WDT => "interrupt watchdog",
        sys::esp_reset_reason_t_ESP_RST_TASK_WDT => "task watchdog",
        sys::esp_reset_reason_t_ESP_RST_WDT => "other watchdog",
        sys::esp_reset_reason_t_ESP_RST_BROWNOUT => "brownout",
        _ => "unknown",
    }
}

/// The broadcast address of the subnet of `ip` with a prefix of `prefix` bits.
fn broadcast(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let host_bits = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
    Ipv4Addr::from(u32::from(ip) | host_bits)
}
