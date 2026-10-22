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
mod midi;
mod net;
mod prolink;
mod settings;
mod tracks;
mod webui;

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use xdj_core_prolink::Tracker;

use crate::midi::MidiStatus;
use crate::net::{EthPins, AP_IP};
use crate::prolink::EthInfo;
use crate::settings::Settings;
use crate::tracks::Tracks;

/// The state that the receive threads write and the web page reads.
pub struct State {
    pub tracker: Mutex<Tracker>,
    /// The Ethernet address, for the "iface" field of the status.
    pub iface: Mutex<String>,
    pub midi: Mutex<MidiStatus>,
    pub tracks: Tracks,
    /// The address of the Ethernet port. `None` while the port has no address.
    pub eth: Mutex<Option<EthInfo>>,
    /// The device number of the last conflict packet. The virtual CDJ reads it while it claims.
    pub conflict: AtomicU8,
    /// The device number of the virtual CDJ. 0 while it is not on the network.
    pub own_device: AtomicU8,
    /// `true` while the settings have a join network and the firmware is not connected to it.
    pub wifi_join_down: AtomicBool,
    /// The reason for the last restart, for example "power-on" or "panic".
    pub reset_reason: &'static str,
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
    esp_idf_svc::log::EspLogger::initialize_default();
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
    let p = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
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

    let state = Arc::new(State {
        tracker: Mutex::new(Tracker::new()),
        iface: Mutex::new("Ethernet: no address".into()),
        midi: Mutex::new(MidiStatus::default()),
        tracks: Tracks::default(),
        eth: Mutex::new(None),
        conflict: AtomicU8::new(0),
        own_device: AtomicU8::new(0),
        wifi_join_down: AtomicBool::new(false),
        reset_reason,
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
        eth.netif().get_name().to_string(),
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

    // Log each change of the Ethernet address. A cable to the XDJ-AZ without a DHCP server
    // gives a 169.254.x.x address after some seconds.
    log::info!("Ethernet is waiting for a cable and an address");
    let eth_mac = eth.netif().get_mac()?;
    let mut last = (None, None);
    let mut next_heap_log = Instant::now();
    let mut next_join = Instant::now() + JOIN_RETRY;
    loop {
        state.tracker.lock().unwrap().expire(state.now_ms());
        settings.lock().unwrap().save_midi();
        if Instant::now() >= next_heap_log {
            next_heap_log += Duration::from_secs(60);
            // SAFETY: the heap calls only read counters.
            let (free, min, block) = unsafe {
                (
                    sys::esp_get_free_heap_size(),
                    sys::esp_get_minimum_free_heap_size(),
                    sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT),
                )
            };
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
        *state.eth.lock().unwrap() = info.map(|i| EthInfo {
            ip: i.ip,
            broadcast: broadcast(i.ip, i.subnet.mask.0),
            mac: eth_mac,
        });
        let sta_ip = join_ssid
            .as_ref()
            .and_then(|_| wifi.sta_netif().get_ip_info().ok())
            .map(|i| i.ip)
            .filter(|ip| !ip.is_unspecified());
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

/// The time between two tries to join the WiFi network.
const JOIN_RETRY: Duration = Duration::from_secs(30);

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
