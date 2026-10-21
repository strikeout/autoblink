//! xdj-clock firmware for the WT32-ETH01 (ESP32 + LAN8720 Ethernet).
//!
//! The Ethernet port receives the Pro DJ Link beats. The MIDI output on IO17 sends the clock,
//! the beat notes and the CCs. Two LEDs show the beats. The firmware serves the web page on its
//! own WiFi network at http://192.168.1.1.

#[cfg(feature = "dmx")]
mod dmx;
mod midi;
mod net;
mod prolink;
mod settings;
mod webui;

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
use crate::settings::Settings;
use crate::webui::Clients;

/// The state that the receive threads write and the web page reads.
pub struct State {
    pub tracker: Mutex<Tracker>,
    /// The Ethernet address, for the "iface" field of the status.
    pub iface: Mutex<String>,
    pub midi: Mutex<MidiStatus>,
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

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

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
        start: Instant::now(),
    });
    let clients = Arc::new(Clients::default());
    let midi_beats = midi::start(p.uart2, p.pins.gpio17, p.pins.gpio35, state.clone(), settings.midi.clone())?;
    let _server = webui::start(state.clone(), clients.clone(), Arc::new(Mutex::new(settings)))?;

    // IO32 pulses on each beat of the master deck, IO33 on the first beat of the bar.
    let mut beat_led = PinDriver::output(p.pins.gpio32)?;
    let mut bar_led = PinDriver::output(p.pins.gpio33)?;
    let (beat_tx, beat_rx) = mpsc::channel::<u8>();
    thread::Builder::new()
        .name("leds".into())
        .stack_size(3 * 1024)
        .spawn(move || {
            for beat in beat_rx {
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

    prolink::spawn(state.clone(), move |beat| {
        let _ = beat_tx.send(beat.beat_in_bar);
        #[cfg(feature = "dmx")]
        dmx.beat();
        clients.broadcast(&webui::beat_message(beat.device, beat.beat_in_bar));
        let _ = midi_beats.send(beat);
    })?;

    // Log each change of the Ethernet address. A cable to the XDJ-AZ without a DHCP server
    // gives a 169.254.x.x address after some seconds.
    log::info!("Ethernet is waiting for a cable and an address");
    let mut last = None;
    loop {
        let ip = eth.netif().get_ip_info().ok().map(|i| i.ip).filter(|ip| !ip.is_unspecified());
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
