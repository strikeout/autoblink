//! xdj-clock firmware for the WT32-ETH01 (ESP32 + LAN8720 Ethernet).
//!
//! The firmware joins the Pro DJ Link network on the Ethernet port as a virtual CDJ. It reads
//! the beats, the player status and the track data from the players. The MIDI output on IO17
//! sends the clock, the notes and the CCs. Two LEDs show the beats. The firmware serves the web
//! page on its own WiFi network at http://192.168.1.1.

#[cfg(feature = "dmx")]
mod dmx;
mod midi;
mod net;
mod prolink;
mod settings;
mod tracks;
mod webui;

use std::net::Ipv4Addr;
use std::sync::atomic::AtomicU8;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use xdj_core_prolink::Tracker;

use crate::midi::MidiStatus;
use crate::net::{EthPins, AP_IP};
use crate::prolink::EthInfo;
use crate::settings::Settings;
use crate::tracks::Tracks;
use crate::webui::Clients;

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
        log::error!("The firmware stops: {e:#}");
    }
}

fn run() -> anyhow::Result<()> {
    let p = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut mac = [0u8; 6];
    // SAFETY: the buffer has the 6 bytes that a MAC address needs.
    sys::esp!(unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_SOFTAP) })?;
    let default_ssid = format!("xdj-clock-{:02X}{:02X}", mac[4], mac[5]);
    let settings = Settings::new(nvs.clone(), default_ssid)?;
    let ssid = settings.ssid();
    let _wifi = net::start_access_point(p.modem, sysloop.clone(), nvs, &ssid, &settings.password())?;
    log::info!("WiFi \"{ssid}\" is on. The web page is at http://{AP_IP}");

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
        start: Instant::now(),
    });
    let clients = Arc::new(Clients::default());
    let midi_beats = midi::start(p.uart2, p.pins.gpio17, p.pins.gpio35, state.clone(), settings.midi.clone())?;
    let device_number = settings.device_number.clone();
    let _server = webui::start(state.clone(), clients.clone(), Arc::new(Mutex::new(settings)))?;
    let track_changes = tracks::start(state.clone())?;

    // IO32 pulses on each beat of the master deck, IO33 on the first beat of the bar.
    // IO33 has the label 485_EN on the board. When no beat arrives for IDLE_AFTER, IO33 blinks
    // two times every 2 s.
    const IDLE_AFTER: Duration = Duration::from_secs(2);
    // The idle pattern: the level of IO33 and its time in milliseconds.
    const IDLE_PATTERN: [(bool, u64); 4] = [(true, 60), (false, 140), (true, 60), (false, 1740)];
    let mut beat_led = PinDriver::output(p.pins.gpio32)?;
    let mut bar_led = PinDriver::output(p.pins.gpio33)?;
    let (beat_tx, beat_rx) = mpsc::channel::<u8>();
    thread::Builder::new()
        .name("leds".into())
        .stack_size(3 * 1024)
        .spawn(move || {
            let mut last_beat: Option<Instant> = None;
            let mut step = 0;
            loop {
                let since = last_beat.map_or(IDLE_AFTER, |t| t.elapsed());
                let idle = since >= IDLE_AFTER;
                let timeout = if idle {
                    let (on, ms) = IDLE_PATTERN[step];
                    let _ = bar_led.set_level(on.into());
                    Duration::from_millis(ms)
                } else {
                    IDLE_AFTER - since
                };
                let beat = match beat_rx.recv_timeout(timeout) {
                    Ok(beat) => beat,
                    Err(RecvTimeoutError::Timeout) => {
                        if idle {
                            step = (step + 1) % IDLE_PATTERN.len();
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                last_beat = Some(Instant::now());
                step = 0;
                let _ = bar_led.set_low();
                let _ = beat_led.set_high();
                if beat == 1 {
                    let _ = bar_led.set_high();
                }
                thread::sleep(Duration::from_millis(30));
                let _ = beat_led.set_low();
                if beat == 1 {
                    thread::sleep(Duration::from_millis(90));
                    let _ = bar_led.set_low();
                }
            }
        })?;

    #[cfg(feature = "dmx")]
    let dmx = dmx::Dmx::start(p.uart1, p.pins.gpio4, p.pins.gpio36)?;

    prolink::spawn(
        state.clone(),
        device_number,
        move |beat| {
            let msg = webui::beat_message(beat.device, beat.beat_in_bar);
            let _ = beat_tx.send(beat.beat_in_bar);
            #[cfg(feature = "dmx")]
            dmx.beat();
            let _ = midi_beats.send(beat);
            // The broadcast waits for the web server task, which can be slow on a weak WiFi
            // link. It therefore comes after the MIDI, the DMX and the LEDs.
            clients.broadcast(&msg);
        },
        move |track| {
            let _ = track_changes.send(track);
        },
    )?;

    // Log each change of the Ethernet address. A cable to the XDJ-AZ without a DHCP server
    // gives a 169.254.x.x address after some seconds.
    log::info!("Ethernet is waiting for a cable and an address");
    let eth_mac = eth.netif().get_mac()?;
    let mut last = None;
    loop {
        let info = eth.netif().get_ip_info().ok().filter(|i| !i.ip.is_unspecified());
        let ip = info.map(|i| i.ip);
        *state.eth.lock().unwrap() = info.map(|i| EthInfo {
            ip: i.ip,
            broadcast: broadcast(i.ip, i.subnet.mask.0),
            mac: eth_mac,
        });
        if ip != last {
            match ip {
                Some(ip) => {
                    log::info!("Ethernet address: {ip}");
                    if ip.octets()[..3] == AP_IP.octets()[..3] {
                        log::warn!(
                            "The Ethernet network uses the subnet of the WiFi network ({AP_IP}/24). \
                             Replies can go out on the wrong port."
                        );
                    }
                }
                None => log::info!("Ethernet has no address"),
            }
            *state.iface.lock().unwrap() =
                ip.map_or("Ethernet: no address".into(), |ip| format!("Ethernet {ip}"));
            last = ip;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// The broadcast address of the subnet of `ip` with a prefix of `prefix` bits.
fn broadcast(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let host_bits = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
    Ipv4Addr::from(u32::from(ip) | host_bits)
}
