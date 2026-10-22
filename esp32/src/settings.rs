//! The settings that the web page can change. NVS keeps them across a restart.
//!
//! The MIDI rows and "Device Number" use the host indices (`xdj_core_midi::settings`). The WiFi
//! rows use 100 and up, so that they do not collide with the host settings.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use serde_json::{json, Value};
use xdj_core_midi::{settings as midi_settings, MidiConfig};

/// The host index of "Device Number": the number that the virtual CDJ asks for.
const IDX_DEVICE: usize = 2;
const DEFAULT_DEVICE: u8 = 7;
const IDX_SSID: usize = 100;
const IDX_PASSWORD: usize = 101;
const DEFAULT_PASSWORD: &str = "xdjclock123";

pub struct Settings {
    nvs: EspNvs<NvsDefault>,
    default_ssid: String,
    /// The MIDI thread reads this configuration.
    pub midi: Arc<Mutex<MidiConfig>>,
    /// The virtual CDJ joins again when this number changes.
    pub device_number: Arc<AtomicU8>,
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
        })
    }

    pub fn ssid(&self) -> String {
        self.get("ssid").unwrap_or_else(|| self.default_ssid.clone())
    }

    pub fn password(&self) -> String {
        self.get("pass").unwrap_or_else(|| DEFAULT_PASSWORD.into())
    }

    /// Builds the protocol v1 settings message.
    pub fn message(&self) -> Value {
        let ssid = self.ssid();
        let mut items = serde_json::to_value(midi_settings::items(&self.midi.lock().unwrap()))
            .unwrap_or_default();
        if let Value::Array(rows) = &mut items {
            let device = self.device_number.load(Ordering::Relaxed).to_string();
            rows.insert(0, json!({"idx": IDX_DEVICE, "label": "Device Number", "section": null,
                "kind": "number", "value": device, "raw": device, "step": false}));
            rows.push(json!({"idx": IDX_SSID, "label": "WiFi name (applies after restart)",
                "section": "Device", "kind": "text", "value": ssid, "raw": ssid, "step": false}));
            // The page does not show the password. A new value replaces it.
            rows.push(json!({"idx": IDX_PASSWORD, "label": "WiFi password (applies after restart)",
                "section": "Device", "kind": "text", "value": "••••••••", "raw": null, "step": false}));
        }
        json!({"type": "settings", "items": items})
    }

    /// Stores a typed value. Returns the error text for the page when the value is not valid.
    pub fn set(&self, idx: usize, value: &str) -> Result<(), String> {
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
            _ => self.change_midi(|cfg| midi_settings::set(cfg, idx, value)),
        }
    }

    /// Flips a toggle or a switch, or steps a number.
    pub fn cycle(&self, idx: usize, dir: i8) -> Result<(), String> {
        self.change_midi(|cfg| midi_settings::cycle(cfg, idx, dir))
    }

    fn change_midi(&self, f: impl FnOnce(&mut MidiConfig) -> Result<(), String>) -> Result<(), String> {
        let json = {
            let mut cfg = self.midi.lock().unwrap();
            f(&mut cfg)?;
            serde_json::to_string(&*cfg).map_err(|e| e.to_string())?
        };
        self.nvs.set_str("midi", &json).map_err(|e| format!("Cannot save the setting: {e}"))
    }

    fn put(&self, key: &str, value: &str) -> Result<(), String> {
        self.nvs.set_str(key, value).map_err(|e| format!("Cannot save the setting: {e}"))?;
        log::info!("Setting {key} changed. It applies after a restart.");
        Ok(())
    }

    fn get(&self, key: &str) -> Option<String> {
        let mut buf = [0u8; 65];
        self.nvs.get_str(key, &mut buf).ok().flatten().map(str::to_owned)
    }
}
