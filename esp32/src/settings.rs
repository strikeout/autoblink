//! The device settings that the web page can change. NVS keeps them across a restart.

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use serde_json::{json, Value};

const IDX_SSID: usize = 100;
const IDX_PASSWORD: usize = 101;
const DEFAULT_PASSWORD: &str = "xdjclock123";

pub struct Settings {
    nvs: EspNvs<NvsDefault>,
    default_ssid: String,
}

impl Settings {
    pub fn new(partition: EspDefaultNvsPartition, default_ssid: String) -> anyhow::Result<Self> {
        Ok(Self { nvs: EspNvs::new(partition, "xdj", true)?, default_ssid })
    }

    pub fn ssid(&self) -> String {
        self.get("ssid").unwrap_or_else(|| self.default_ssid.clone())
    }

    pub fn password(&self) -> String {
        self.get("pass").unwrap_or_else(|| DEFAULT_PASSWORD.into())
    }

    /// Builds the protocol v1 settings message. The indices start at 100, so that they do not
    /// collide with the host settings.
    pub fn message(&self) -> Value {
        let ssid = self.ssid();
        json!({
            "type": "settings",
            "items": [
                {"idx": IDX_SSID, "label": "WiFi name (applies after restart)", "section": "Device",
                 "kind": "text", "value": ssid, "raw": ssid, "step": false},
                // The page does not show the password. A new value replaces it.
                {"idx": IDX_PASSWORD, "label": "WiFi password (applies after restart)", "section": "Device",
                 "kind": "text", "value": "••••••••", "raw": null, "step": false},
            ],
        })
    }

    /// Stores a setting. Returns the error text for the page when the value is not valid.
    pub fn set(&self, idx: usize, value: &str) -> Result<(), String> {
        let (key, ok) = match idx {
            IDX_SSID => ("ssid", (1..=32).contains(&value.len())),
            IDX_PASSWORD => ("pass", (8..=63).contains(&value.len()) && value.is_ascii()),
            _ => return Err(format!("Setting {idx} is not on this device")),
        };
        if !ok {
            return Err(match idx {
                IDX_SSID => "The WiFi name must have 1 to 32 bytes".into(),
                _ => "The WiFi password must have 8 to 63 ASCII characters".into(),
            });
        }
        self.nvs.set_str(key, value).map_err(|e| format!("Cannot save the setting: {e}"))?;
        log::info!("Setting {idx} changed. It applies after a restart.");
        Ok(())
    }

    fn get(&self, key: &str) -> Option<String> {
        let mut buf = [0u8; 65];
        self.nvs.get_str(key, &mut buf).ok().flatten().map(str::to_owned)
    }
}
