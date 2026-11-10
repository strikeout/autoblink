//! The settings of the box hardware. Only the firmware has them, so they use the indices from 110.
//! The host uses indices up to 33, and the WiFi rows of the firmware use 100 to 103.

use autoblink_core_midi::SettingItem;
use serde::{Deserialize, Serialize};

use crate::DmxMode;

pub const IDX_DMX_MODE: usize = 110;
pub const IDX_BEAT_CHANNEL: usize = 111;
pub const IDX_DMX_IN: usize = 112;
pub const IDX_ROTATE: usize = 120;
pub const IDX_BRIGHTNESS: usize = 121;

/// The indices of the rows that `BoxConfig` has.
pub const INDICES: [usize; 5] = [
    IDX_DMX_MODE,
    IDX_BEAT_CHANNEL,
    IDX_DMX_IN,
    IDX_ROTATE,
    IDX_BRIGHTNESS,
];

/// One step of the brightness for the arrows of the page.
const BRIGHTNESS_STEP: u8 = 16;

/// The settings of the box hardware. NVS keeps them as JSON.
///
/// Do not add an `f32` field. LLVM for Xtensa cannot compile the serde code for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoxConfig {
    pub dmx_mode: DmxMode,
    /// The channel of the beat flash, 1 to 512.
    pub beat_channel: u16,
    /// Reads the frames of DMX IN on IO36. IO36 has no pull resistor, so without the RS485 module
    /// its noise can look like a frame. Thus the input is off until the user turns it on.
    pub dmx_in: bool,
    /// Turns the OLED by 180°, for a display that is upside down on the front panel.
    pub rotate: bool,
    /// The contrast of the OLED, 1 to 255.
    pub brightness: u8,
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            dmx_mode: DmxMode::Merge,
            beat_channel: 1,
            dmx_in: false,
            rotate: false,
            brightness: 128,
        }
    }
}

impl BoxConfig {
    /// Puts each value back into its range. A value from NVS can come from an older firmware.
    pub fn sanitize(&mut self) {
        self.beat_channel = self.beat_channel.clamp(1, 512);
        self.brightness = self.brightness.max(1);
    }

    /// Builds the rows of the settings message.
    pub fn items(&self) -> Vec<SettingItem> {
        let channel = self.beat_channel.to_string();
        let brightness = self.brightness.to_string();
        vec![
            SettingItem {
                idx: IDX_DMX_MODE,
                label: "DMX Mode",
                section: Some("DMX"),
                kind: "cycle",
                value: self.dmx_mode.label().into(),
                raw: None,
                step: false,
                on: None,
            },
            SettingItem {
                idx: IDX_BEAT_CHANNEL,
                label: "DMX Beat Channel",
                section: Some("DMX"),
                kind: "number",
                value: channel.clone(),
                raw: Some(channel),
                step: true,
                on: None,
            },
            SettingItem {
                idx: IDX_DMX_IN,
                label: "DMX IN",
                section: Some("DMX"),
                kind: "toggle",
                value: if self.dmx_in { "✓" } else { "✗" }.into(),
                raw: None,
                step: false,
                on: None,
            },
            SettingItem {
                idx: IDX_ROTATE,
                label: "Display Rotate 180°",
                section: Some("Display"),
                kind: "toggle",
                value: if self.rotate { "✓" } else { "✗" }.into(),
                raw: None,
                step: false,
                on: None,
            },
            SettingItem {
                idx: IDX_BRIGHTNESS,
                label: "Display Brightness",
                section: Some("Display"),
                kind: "number",
                value: brightness.clone(),
                raw: Some(brightness),
                step: true,
                on: None,
            },
        ]
    }

    /// Stores a typed value. Returns the error text when the value is not valid.
    pub fn set(&mut self, idx: usize, value: &str) -> Result<(), String> {
        let range = || format!("Value \"{value}\" is out of range for setting {idx}");
        let flag = || match value.trim() {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(range()),
        };
        match idx {
            IDX_BEAT_CHANNEL => {
                self.beat_channel = value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|v| (1..=512).contains(v))
                    .ok_or_else(range)?;
                Ok(())
            }
            IDX_BRIGHTNESS => {
                self.brightness = value
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|v| *v >= 1)
                    .ok_or_else(range)?;
                Ok(())
            }
            // The menu of the OLED sets the mode by its index in `DmxMode::ALL`.
            IDX_DMX_MODE => {
                self.dmx_mode = value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| DmxMode::ALL.get(i).copied())
                    .ok_or_else(range)?;
                Ok(())
            }
            IDX_DMX_IN => {
                self.dmx_in = flag()?;
                Ok(())
            }
            IDX_ROTATE => {
                self.rotate = flag()?;
                Ok(())
            }
            _ => Err(format!("Setting {idx} does not take a value")),
        }
    }

    /// Steps a setting. `dir` is 1 or -1.
    pub fn cycle(&mut self, idx: usize, dir: i8) -> Result<(), String> {
        match idx {
            IDX_DMX_MODE => self.dmx_mode = self.dmx_mode.step(dir),
            IDX_BEAT_CHANNEL => {
                self.beat_channel = self
                    .beat_channel
                    .saturating_add_signed(dir.into())
                    .clamp(1, 512)
            }
            IDX_DMX_IN => self.dmx_in = !self.dmx_in,
            IDX_ROTATE => self.rotate = !self.rotate,
            IDX_BRIGHTNESS => {
                self.brightness = if dir < 0 {
                    self.brightness.saturating_sub(BRIGHTNESS_STEP).max(1)
                } else {
                    self.brightness.saturating_add(BRIGHTNESS_STEP)
                }
            }
            _ => return Err(format!("Setting {idx} cannot cycle")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_channel_stays_in_range() {
        let mut cfg = BoxConfig::default();
        cfg.cycle(IDX_BEAT_CHANNEL, -1).unwrap();
        assert_eq!(cfg.beat_channel, 1);
        cfg.set(IDX_BEAT_CHANNEL, "512").unwrap();
        cfg.cycle(IDX_BEAT_CHANNEL, 1).unwrap();
        assert_eq!(cfg.beat_channel, 512);
        assert!(cfg.set(IDX_BEAT_CHANNEL, "0").is_err());
        assert!(cfg.set(IDX_BEAT_CHANNEL, "513").is_err());
        assert_eq!(cfg.beat_channel, 512);
    }

    #[test]
    fn the_menu_sets_the_mode_and_the_rotation_by_number() {
        let mut c = BoxConfig::default();
        c.set(IDX_DMX_MODE, "3").unwrap();
        assert_eq!(c.dmx_mode, DmxMode::Pass);
        assert!(c.set(IDX_DMX_MODE, "4").is_err());
        c.set(IDX_ROTATE, "1").unwrap();
        assert!(c.rotate);
        assert!(c.set(IDX_ROTATE, "2").is_err());
    }

    #[test]
    fn mode_cycles_and_shows_its_label() {
        let mut cfg = BoxConfig::default();
        cfg.cycle(IDX_DMX_MODE, 1).unwrap();
        assert_eq!(cfg.dmx_mode, DmxMode::Pass);
        assert_eq!(cfg.items()[0].value, "Pass");
    }

    #[test]
    fn display_rows_flip_and_step() {
        let mut cfg = BoxConfig::default();
        cfg.cycle(IDX_ROTATE, 1).unwrap();
        assert!(cfg.rotate);
        cfg.set(IDX_BRIGHTNESS, "10").unwrap();
        cfg.cycle(IDX_BRIGHTNESS, -1).unwrap();
        assert_eq!(cfg.brightness, 1);
        cfg.set(IDX_BRIGHTNESS, "250").unwrap();
        cfg.cycle(IDX_BRIGHTNESS, 1).unwrap();
        assert_eq!(cfg.brightness, 255);
        assert!(cfg.set(IDX_BRIGHTNESS, "0").is_err());
    }

    #[test]
    fn missing_fields_take_the_default() {
        let cfg: BoxConfig = serde_json::from_str(r#"{"beat_channel":5}"#).unwrap();
        assert_eq!(cfg.dmx_mode, DmxMode::Merge);
        assert_eq!(cfg.beat_channel, 5);
        assert!(!cfg.dmx_in);
    }

    #[test]
    fn dmx_in_flips() {
        let mut cfg = BoxConfig::default();
        cfg.cycle(IDX_DMX_IN, 1).unwrap();
        assert!(cfg.dmx_in);
        cfg.set(IDX_DMX_IN, "0").unwrap();
        assert!(!cfg.dmx_in);
        assert!(cfg.set(IDX_DMX_IN, "2").is_err());
    }
}
