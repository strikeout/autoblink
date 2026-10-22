//! Firmware update over the air from the GitHub releases.
//!
//! The ESP32 release workflow keeps the prerelease `esp32-latest`. It has two files: the version
//! text and the app image. The page sends `ota_check` and `ota_install`. This thread reads the
//! version, and it writes the image into the free OTA slot. Then the firmware restarts.
//!
//! The new app starts in the state "pending verify". `mark_valid` ends that state. When the new
//! app restarts before `mark_valid`, the bootloader starts the previous app again.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::ota::EspOta;
use esp_idf_svc::sys;
use serde_json::{json, Value};

use crate::net::AP_IP;
use crate::State;

// ponytail: the page has no password, so each client on the networks of the box can start an
// update. The update comes only from the GitHub release over HTTPS, and the image has no
// signature. Add secure boot and a signed image if the box must refuse a changed release.
const BASE_URL: &str = "https://github.com/strikeout/xdj-midi-beat/releases/download/esp32-latest";

/// The size of an OTA slot in `partitions.csv`.
const SLOT_SIZE: usize = 0x1E_0000;

pub enum OtaCmd {
    Check,
    Install,
}

/// The update state that the page shows.
#[derive(Default)]
pub struct OtaStatus {
    /// "idle", "checking", "available", "up-to-date", "downloading", "restarting" or "error".
    pub state: &'static str,
    /// The version of the release, after a check.
    pub latest: Option<String>,
    /// The download progress in percent.
    pub progress: u8,
    pub error: Option<String>,
}

impl OtaStatus {
    pub fn json(&self) -> Value {
        json!({
            "state": if self.state.is_empty() { "idle" } else { self.state },
            "latest": self.latest,
            "progress": self.progress,
            "error": self.error,
        })
    }
}

/// `true` while an update thread runs.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Starts a thread for one command of the page. Returns an error while a thread runs.
///
/// The thread exists only for the command. Its stack does not stay on the heap after it.
pub fn run(state: Arc<State>, cmd: OtaCmd) -> Result<(), &'static str> {
    if BUSY.swap(true, Ordering::AcqRel) {
        return Err("The firmware update is busy");
    }
    let spawned = thread::Builder::new()
        .name("ota".into())
        // The TLS handshake of mbedTLS needs a large stack.
        .stack_size(12 * 1024)
        .spawn(move || {
            let result = match cmd {
                OtaCmd::Check => check(&state.ota).map(|_| ()),
                OtaCmd::Install => install(&state.ota),
            };
            if let Err(e) = result {
                log::warn!("Firmware update: {e:#}");
                let mut ota = state.ota.lock().unwrap();
                ota.state = "error";
                ota.error = Some(format!("{e:#}"));
            }
            BUSY.store(false, Ordering::Release);
        });
    spawned.map(|_| ()).map_err(|_| {
        BUSY.store(false, Ordering::Release);
        "The firmware update cannot start a thread"
    })
}

/// Marks the running app as valid, so that the bootloader keeps it after a restart.
pub fn mark_valid() {
    // EspOta::get_running_slot fails with ESP_ERR_INVALID_SIZE, because the app description does
    // not fit into its strings. Thus read the label of the partition directly.
    // SAFETY: the running partition is a static entry of the partition table, or null.
    let label = unsafe { sys::esp_ota_get_running_partition().as_ref() }
        .map(|p| unsafe { std::ffi::CStr::from_ptr(p.label.as_ptr()) }.to_string_lossy().into_owned())
        .unwrap_or_default();
    match EspOta::new().and_then(|mut ota| ota.mark_running_slot_valid()) {
        Ok(()) => log::info!("The app in slot {label} is marked valid"),
        Err(e) => log::warn!("The app in slot {label} cannot be marked valid: {e}"),
    }
}

/// Reads the version of the release. Returns `true` when it is newer than the running app.
fn check(state: &Mutex<OtaStatus>) -> anyhow::Result<bool> {
    set(state, "checking", None, 0);
    let mut conn = get(&format!("{BASE_URL}/esp32-version.txt"))?;
    let mut buf = [0u8; 32];
    let mut len = 0;
    while len < buf.len() {
        let n = conn.read(&mut buf[len..])?;
        if n == 0 {
            break;
        }
        len += n;
    }
    let latest = std::str::from_utf8(&buf[..len])?.trim().to_string();
    let newer = newer(&latest, env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| anyhow!("The release has the version {latest:?}. It is not x.y.z."))?;
    log::info!("Firmware update: the release has the version {latest}");
    set(state, if newer { "available" } else { "up-to-date" }, Some(latest), 0);
    Ok(newer)
}

/// Writes the image of the release into the free OTA slot. Then the firmware restarts.
fn install(state: &Mutex<OtaStatus>) -> anyhow::Result<()> {
    if !check(state)? {
        return Ok(());
    }
    let latest = state.lock().unwrap().latest.clone();
    set(state, "downloading", latest.clone(), 0);
    let mut conn = get(&format!("{BASE_URL}/xdj-clock-esp32-ota.bin"))?;
    let total: usize = conn
        .header("Content-Length")
        .and_then(|l| l.parse().ok())
        .context("The download has no Content-Length")?;
    if total > SLOT_SIZE {
        bail!("The image has {total} bytes. An OTA slot has {SLOT_SIZE} bytes.");
    }

    let mut ota = EspOta::new()?;
    // Erase each sector before the write to it. One erase of the full slot stops the other
    // threads, and the MIDI clock, for some seconds.
    let mut update = ota.initiate_update_with_known_size(sys::OTA_WITH_SEQUENTIAL_WRITES as usize)?;
    let mut buf = vec![0u8; 4096];
    let mut done = 0;
    loop {
        let n = conn.read(&mut buf)?;
        if n == 0 {
            break;
        }
        update.write(&buf[..n])?;
        done += n;
        state.lock().unwrap().progress = (done * 100 / total) as u8;
    }
    if done != total {
        bail!("The download stopped after {done} of {total} bytes");
    }
    // esp_ota_end examines the image. It refuses an image for another chip or with a bad checksum.
    update.complete()?;
    log::info!("Firmware update: {} is written. The firmware restarts.", latest.as_deref().unwrap_or("?"));
    set(state, "restarting", latest, 100);
    // The page gets the status "restarting" before the restart.
    thread::sleep(Duration::from_secs(1));
    // SAFETY: esp_restart has no preconditions.
    unsafe { sys::esp_restart() }
}

/// Sends a GET request. Returns the connection after a 200 response.
fn get(url: &str) -> anyhow::Result<EspHttpConnection> {
    fix_dns();
    let mut conn = EspHttpConnection::new(&Configuration {
        // GitHub redirects a release file to a URL with more than 500 characters.
        buffer_size: Some(4096),
        buffer_size_tx: Some(2048),
        timeout: Some(Duration::from_secs(15)),
        crt_bundle_attach: Some(sys::esp_crt_bundle_attach),
        ..Default::default()
    })?;
    conn.initiate_request(Method::Get, url, &[])
        .with_context(|| format!("No connection to {url}. The box needs a network with internet."))?;
    conn.initiate_response()?;
    match conn.status() {
        200 => Ok(conn),
        status => bail!("GET {url} gave the status {status}"),
    }
}

/// The DNS server for a joined network in the subnet of the own WiFi network.
const PUBLIC_DNS: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

/// Replaces a DNS server in the subnet of the own WiFi network with `PUBLIC_DNS`.
///
/// A router at 192.168.1.1 is often also the DNS server. The firmware has that address on its own
/// WiFi network, so lwIP sends the DNS query to the firmware itself. A public server goes out
/// through the default route, which is the joined network.
fn fix_dns() {
    // SAFETY: the key is a NUL-terminated string. The handle is null or a live netif.
    let sta = unsafe { sys::esp_netif_get_handle_from_ifkey(c"WIFI_STA_DEF".as_ptr()) };
    if sta.is_null() {
        return;
    }
    let mut dns = sys::esp_netif_dns_info_t::default();
    // SAFETY: `sta` is a live netif, and `dns` is a valid output buffer.
    if unsafe { sys::esp_netif_get_dns_info(sta, sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN, &mut dns) } != sys::ESP_OK {
        return;
    }
    // SAFETY: the ip4 member is valid for each address type, because it is at the start of the union.
    let current = Ipv4Addr::from(u32::from_be(unsafe { dns.ip.u_addr.ip4.addr }));
    if !crate::same_subnet(current, AP_IP, 24) {
        return;
    }
    log::warn!("DNS server {current} is in the subnet of the own WiFi network. The firmware uses {PUBLIC_DNS}.");
    dns.ip.u_addr.ip4.addr = u32::from_ne_bytes(PUBLIC_DNS.octets());
    dns.ip.type_ = sys::ESP_IPADDR_TYPE_V4 as u8;
    // SAFETY: `sta` is a live netif, and `dns` holds an IPv4 address.
    unsafe { sys::esp_netif_set_dns_info(sta, sys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN, &mut dns) };
}

fn set(state: &Mutex<OtaStatus>, name: &'static str, latest: Option<String>, progress: u8) {
    *state.lock().unwrap() = OtaStatus { state: name, latest, progress, error: None };
}

/// Returns `Some(true)` when the version `a` is newer than `b`. Returns `None` when `a` or `b` is
/// not in the format x.y.z.
fn newer(a: &str, b: &str) -> Option<bool> {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let mut parts = v.trim_start_matches('v').split('.').map(|p| p.parse().ok());
        let version = (parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    };
    Some(parse(a)? > parse(b)?)
}
