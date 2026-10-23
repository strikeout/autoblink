//! The settings that the web page can change. NVS keeps them across a restart.
//!
//! The MIDI rows and "Device Number" use the host indices (`xdj_core_midi::settings`). The WiFi
//! rows use 100 and up, so that they do not collide with the host settings. Rows 100 and 101 set
//! the own access point. Rows 102 and 103 set an existing network that the firmware joins.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use xdj_core_midi::{settings as midi_settings, MidiConfig, SettingItem};

/// The host index of "Device Number": the number that the virtual CDJ asks for.
const IDX_DEVICE: usize = 2;
const DEFAULT_DEVICE: u8 = 7;
const IDX_SSID: usize = 100;
const IDX_PASSWORD: usize = 101;
const IDX_JOIN_SSID: usize = 102;
const IDX_JOIN_PASSWORD: usize = 103;
const DEFAULT_PASSWORD: &str = "xdjclock123";
/// The MIDI configuration goes to NVS when it has not changed for this time.
const SAVE_DELAY: Duration = Duration::from_secs(2);

pub struct Settings {
    nvs: EspNvs<NvsDefault>,
    default_ssid: String,
    /// The MIDI thread reads this configuration.
    pub midi: Arc<Mutex<MidiConfig>>,
    /// The virtual CDJ joins again when this number changes.
    pub device_number: Arc<AtomicU8>,
    /// The time of the last MIDI change that is not in NVS yet.
    midi_changed: Option<Instant>,
}

impl Settings {
    pub fn new(partition: EspDefaultNvsPartition, default_ssid: String) -> anyhow::Result<Self> {
        let nvs = EspNvs::new(partition, "xdj", true)?;
        let mut buf = vec![0u8; 2048];
        let midi = nvs
            .get_str("midi", &mut buf)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let device = nvs.get_u8("devnum").ok().flatten().filter(|n| (1..=15).contains(n));
        Ok(Self {
            nvs,
            default_ssid,
            midi: Arc::new(Mutex::new(midi)),
            device_number: Arc::new(AtomicU8::new(device.unwrap_or(DEFAULT_DEVICE))),
            midi_changed: None,
        })
    }

    pub fn ssid(&self) -> String {
        self.get("ssid").unwrap_or_else(|| self.default_ssid.clone())
    }

    pub fn password(&self) -> String {
        self.get("pass").unwrap_or_else(|| DEFAULT_PASSWORD.into())
    }

    /// The name and the password of the existing network to join. `None` when NVS has no name.
    /// An empty password is for an open network.
    pub fn join(&self) -> Option<(String, String)> {
        let ssid = self.get("jssid")?;
        Some((ssid, self.get("jpass").unwrap_or_default()))
    }

    /// Builds the protocol v1 settings message as JSON text.
    pub fn message(&self) -> String {
        #[derive(serde::Serialize)]
        struct Message {
            #[serde(rename = "type")]
            kind: &'static str,
            items: Vec<SettingItem>,
        }
        let row = |idx, label, kind, value: String, raw: Option<String>| SettingItem {
            idx,
            label,
            section: Some("WiFi"),
            kind,
            value,
            raw,
            step: false,
            on: None,
        };
        // The page does not show the passwords. A new value replaces a password.
        let hidden = || "••••••••".to_string();
        let device = self.device_number.load(Ordering::Relaxed).to_string();
        let ssid = self.ssid();
        let join = self.join().map(|(s, _)| s).unwrap_or_default();
        let mut items = vec![SettingItem {
            section: None,
            ..row(IDX_DEVICE, "Device Number", "number", device.clone(), Some(device))
        }];
        items.extend(midi_settings::items(&self.midi.lock().unwrap()));
        items.push(row(IDX_SSID, "WiFi name (applies after restart)", "text", ssid.clone(), Some(ssid)));
        items.push(row(IDX_PASSWORD, "WiFi password (applies after restart)", "text", hidden(), None));
        let shown = if join.is_empty() { "off".to_string() } else { join.clone() };
        items.push(row(IDX_JOIN_SSID, "Join WiFi name (applies after restart)", "text", shown, Some(join)));
        items.push(row(IDX_JOIN_PASSWORD, "Join WiFi password (applies after restart)", "text", hidden(), None));
        serde_json::to_string(&Message { kind: "settings", items }).unwrap_or_default()
    }

    /// Stores a typed value. Returns the error text for the page when the value is not valid.
    pub fn set(&mut self, idx: usize, value: &str) -> Result<(), String> {
        match idx {
            IDX_DEVICE => {
                let n = value
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (1..=15).contains(n))
                    .ok_or_else(|| format!("Value \"{value}\" is out of range for setting {idx}"))?;
                self.nvs.set_u8("devnum", n).map_err(|e| format!("Cannot save the setting: {e}"))?;
                self.device_number.store(n, Ordering::Relaxed);
                log::info!("Device number changed to {n}. The virtual CDJ joins again.");
                Ok(())
            }
            IDX_SSID if (1..=32).contains(&value.len()) => self.put("ssid", value),
            IDX_SSID => Err("The WiFi name must have 1 to 32 bytes".into()),
            IDX_PASSWORD if (8..=63).contains(&value.len()) && value.is_ascii() => self.put("pass", value),
            IDX_PASSWORD => Err("The WiFi password must have 8 to 63 ASCII characters".into()),
            // An empty name stops the join. The page cannot send an empty password, so the
            // password goes too. Then the next name can be an open network.
            IDX_JOIN_SSID if value.is_empty() => self.remove("jpass").and_then(|()| self.remove("jssid")),
            IDX_JOIN_SSID if value.len() <= 32 => self.put("jssid", value),
            IDX_JOIN_SSID => Err("The WiFi name must have 0 to 32 bytes".into()),
            IDX_JOIN_PASSWORD if value.is_empty() => self.remove("jpass"),
            IDX_JOIN_PASSWORD if (8..=63).contains(&value.len()) && value.is_ascii() => self.put("jpass", value),
            IDX_JOIN_PASSWORD => {
                Err("The WiFi password must be empty (open network) or have 8 to 63 ASCII characters".into())
            }
            _ => self.change_midi(|cfg| midi_settings::set(cfg, idx, value)),
        }
    }

    /// Flips a toggle or a switch, or steps a number.
    pub fn cycle(&mut self, idx: usize, dir: i8) -> Result<(), String> {
        self.change_midi(|cfg| midi_settings::cycle(cfg, idx, dir))
    }

    /// Changes the MIDI configuration in RAM. `save_midi` writes it to NVS later.
    ///
    /// An NVS write stops the flash cache of both cores for up to some 100 ms, and the MIDI clock
    /// stops with it. Many fast clicks on the page then give many stops. The delay gives one.
    fn change_midi(&mut self, f: impl FnOnce(&mut MidiConfig) -> Result<(), String>) -> Result<(), String> {
        f(&mut self.midi.lock().unwrap())?;
        self.midi_changed = Some(Instant::now());
        Ok(())
    }

    /// Writes the MIDI configuration to NVS when it has not changed for `SAVE_DELAY`. The main
    /// loop calls this each second.
    pub fn save_midi(&mut self) {
        if !self.midi_changed.is_some_and(|t| t.elapsed() >= SAVE_DELAY) {
            return;
        }
        self.midi_changed = None;
        let json = serde_json::to_string(&*self.midi.lock().unwrap());
        match json.map_err(anyhow::Error::from).and_then(|j| Ok(self.nvs.set_str("midi", &j)?)) {
            Ok(()) => log::info!("The MIDI settings are saved"),
            Err(e) => log::warn!("Cannot save the MIDI settings: {e}"),
        }
    }

    fn put(&self, key: &str, value: &str) -> Result<(), String> {
        self.nvs.set_str(key, value).map_err(|e| format!("Cannot save the setting: {e}"))?;
        log::info!("Setting {key} changed. It applies after a restart.");
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<(), String> {
        self.nvs.remove(key).map_err(|e| format!("Cannot save the setting: {e}"))?;
        log::info!("Setting {key} removed. It applies after a restart.");
        Ok(())
    }

    fn get(&self, key: &str) -> Option<String> {
        let mut buf = [0u8; 65];
        self.nvs.get_str(key, &mut buf).ok().flatten().map(str::to_owned)
    }
}
