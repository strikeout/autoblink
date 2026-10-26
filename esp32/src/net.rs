//! Ethernet to the Pro DJ Link network, and the WiFi for the web page: the own access point and,
//! when the settings have a join network, a connection to an existing WiFi network.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::fd::{FromRawFd, RawFd};
use std::time::Duration;

use esp_idf_svc::eth::{EspEth, EthDriver, RmiiClockConfig, RmiiEth, RmiiEthChipset};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{self, Gpio0, Gpio16, Gpio18, Gpio19, Gpio21, Gpio22, Gpio23, Gpio25, Gpio26, Gpio27, Output, PinDriver};
use esp_idf_svc::hal::mac::MAC;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::ipv4::{self, Mask, RouterConfiguration, Subnet};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiDriver,
};

/// The address of the firmware on its own WiFi network. The web page is at http://192.168.1.1.
pub const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);

/// The mDNS name of the firmware. The web page is at http://autoblink.local.
pub const MDNS_NAME: &str = "autoblink";

/// Binds the socket `fd` to the network interface `netif` (lwIP `SO_BINDTODEVICE`). An empty
/// `netif` removes the binding.
pub fn bind_device(fd: RawFd, netif: &str) -> io::Result<()> {
    let mut req = sys::ifreq::default();
    for (dst, src) in req.ifr_name.iter_mut().zip(netif.bytes().take(sys::IFNAMSIZ as usize - 1)) {
        *dst = src as _;
    }
    // SAFETY: `req` is a valid `ifreq`, and the length is its size.
    let r = unsafe {
        sys::lwip_setsockopt(
            fd,
            sys::SOL_SOCKET as _,
            sys::SO_BINDTODEVICE as _,
            (&req as *const sys::ifreq).cast(),
            std::mem::size_of::<sys::ifreq>() as _,
        )
    };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Opens a TCP connection to `addr` on the network interface `netif`. An empty `netif` is for
/// all interfaces.
///
/// The own WiFi network and a joined 192.168.1.x network have one subnet. lwIP then can send
/// the connection out on the own network. The binding prevents this.
pub fn connect_on(netif: &str, addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let (SocketAddr::V4(v4), false) = (addr, netif.is_empty()) else {
        return TcpStream::connect_timeout(addr, timeout);
    };
    // SAFETY: the call has no preconditions.
    let fd = unsafe { sys::lwip_socket(sys::AF_INET as _, sys::SOCK_STREAM as _, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new socket. The stream owns it and closes it.
    let stream = unsafe { TcpStream::from_raw_fd(fd) };
    bind_device(fd, netif)?;
    stream.set_nonblocking(true)?;
    let sa = sys::sockaddr_in {
        sin_len: std::mem::size_of::<sys::sockaddr_in>() as _,
        sin_family: sys::AF_INET as _,
        sin_port: v4.port().to_be(),
        sin_addr: sys::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) },
        sin_zero: Default::default(),
    };
    // SAFETY: `sa` is a valid `sockaddr_in`, and the length is its size.
    let r = unsafe {
        sys::lwip_connect(
            fd,
            (&sa as *const sys::sockaddr_in).cast(),
            std::mem::size_of::<sys::sockaddr_in>() as _,
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(sys::EINPROGRESS as i32) {
            return Err(e);
        }
        let mut p = sys::pollfd { fd, events: sys::POLLOUT as _, revents: 0 };
        // SAFETY: `p` is one valid `pollfd`.
        match unsafe { sys::poll(&mut p, 1, timeout.as_millis() as _) } {
            0 => return Err(io::ErrorKind::TimedOut.into()),
            n if n < 0 => return Err(io::Error::last_os_error()),
            _ => {}
        }
        if let Some(e) = stream.take_error()? {
            return Err(e);
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

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
    ssid: &str,
    password: &str,
    join: Option<(String, String)>,
) -> anyhow::Result<EspWifi<'static>> {
    // The firmware sets the WiFi configuration at each start. Without NVS, the driver does not
    // write it to the flash, and the tasks do not stop for a flash write.
    let driver = WifiDriver::new(modem, sysloop, None)?;
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
    // In power save, the station receives a broadcast only at a DTIM beacon. A beat packet is a
    // broadcast, so power save makes the beats late by up to 300 ms. The box has mains power.
    // SAFETY: the WiFi driver has started.
    sys::esp!(unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    // The connection completes in the background. A failure must not stop the access point.
    if let Err(e) = wifi.connect() {
        log::warn!("WiFi \"{join_ssid}\": the connection did not start: {e}");
    }
    Ok(wifi)
}
