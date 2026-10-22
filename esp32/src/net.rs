//! Ethernet to the Pro DJ Link network, and the WiFi for the web page: the own access point and,
//! when the settings have a join network, a connection to an existing WiFi network.

use std::net::Ipv4Addr;

use esp_idf_svc::eth::{EspEth, EthDriver, RmiiClockConfig, RmiiEth, RmiiEthChipset};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{self, Gpio0, Gpio16, Gpio18, Gpio19, Gpio21, Gpio22, Gpio23, Gpio25, Gpio26, Gpio27, Output, PinDriver};
use esp_idf_svc::hal::mac::MAC;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::ipv4::{self, Mask, RouterConfiguration, Subnet};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiDriver,
};

/// The address of the firmware on its own WiFi network. The web page is at http://192.168.1.1.
pub const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);

/// The mDNS name of the firmware. The web page is at http://xdj-clock.local.
pub const MDNS_NAME: &str = "xdj-clock";

/// The pins of the LAN8720 PHY on the WT32-ETH01.
pub struct EthPins {
    pub mac: MAC<'static>,
    pub rxd0: Gpio25<'static>,
    pub rxd1: Gpio26<'static>,
    pub crs_dv: Gpio27<'static>,
    pub mdc: Gpio23<'static>,
    pub txd1: Gpio22<'static>,
    pub tx_en: Gpio21<'static>,
    pub txd0: Gpio19<'static>,
    pub mdio: Gpio18<'static>,
    pub clk: Gpio0<'static>,
    pub osc_enable: Gpio16<'static>,
}

/// Starts the Ethernet port. DHCP gets the address. Without a DHCP server, the port takes a
/// 169.254.x.x address (CONFIG_LWIP_AUTOIP).
///
/// The returned pin driver keeps the 50 MHz oscillator of the PHY on. Keep it alive.
pub fn start_ethernet(
    pins: EthPins,
    sysloop: EspSystemEventLoop,
) -> anyhow::Result<(EspEth<'static, RmiiEth>, PinDriver<'static, Output>)> {
    // GPIO16 switches on the oscillator that clocks the PHY and GPIO0. It must be on before the
    // driver starts.
    let mut osc = PinDriver::output(pins.osc_enable)?;
    osc.set_high()?;
    std::thread::sleep(std::time::Duration::from_millis(10));

    let driver = EthDriver::new_rmii(
        pins.mac,
        pins.rxd0,
        pins.rxd1,
        pins.crs_dv,
        pins.mdc,
        pins.txd1,
        pins.tx_en,
        pins.txd0,
        pins.mdio,
        RmiiClockConfig::Input(pins.clk),
        None::<gpio::AnyOutputPin>,
        RmiiEthChipset::LAN87XX,
        Some(1),
        sysloop,
    )?;
    let mut eth = EspEth::wrap(driver)?;
    eth.start()?;
    Ok((eth, osc))
}

/// Starts the WiFi access point at `AP_IP`. Its DHCP server gives the clients 192.168.1.2 and up.
///
/// When `join` has a network name and a password, the firmware also connects to that network.
/// An empty password is for an open network. The access point then uses the channel of that
/// network. Call `EspWifi::connect` again when the connection is lost.
pub fn start_wifi(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
    ssid: &str,
    password: &str,
    join: Option<(String, String)>,
) -> anyhow::Result<EspWifi<'static>> {
    let driver = WifiDriver::new(modem, sysloop, Some(nvs))?;
    let ap_netif = EspNetif::new_with_conf(&NetifConfiguration {
        ip_configuration: Some(ipv4::Configuration::Router(RouterConfiguration {
            subnet: Subnet { gateway: AP_IP, mask: Mask(24) },
            dhcp_enabled: true,
            dns: None,
            secondary_dns: None,
        })),
        ..NetifConfiguration::wifi_default_router()
    })?;
    let mut wifi = EspWifi::wrap_all(driver, EspNetif::new(NetifStack::Sta)?, ap_netif)?;
    let ap = AccessPointConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow::anyhow!("The WiFi name is too long"))?,
        password: password.try_into().map_err(|_| anyhow::anyhow!("The WiFi password is too long"))?,
        auth_method: AuthMethod::WPA2Personal,
        channel: 6,
        max_connections: 4,
        ..Default::default()
    };
    let Some((join_ssid, join_password)) = join else {
        wifi.set_configuration(&Configuration::AccessPoint(ap))?;
        wifi.start()?;
        return Ok(wifi);
    };
    let client = ClientConfiguration {
        ssid: join_ssid.as_str().try_into().map_err(|_| anyhow::anyhow!("The join WiFi name is too long"))?,
        password: join_password
            .as_str()
            .try_into()
            .map_err(|_| anyhow::anyhow!("The join WiFi password is too long"))?,
        auth_method: if join_password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    };
    wifi.set_configuration(&Configuration::Mixed(client, ap))?;
    wifi.start()?;
    // The connection completes in the background. A failure must not stop the access point.
    if let Err(e) = wifi.connect() {
        log::warn!("WiFi \"{join_ssid}\": the connection did not start: {e}");
    }
    Ok(wifi)
}
