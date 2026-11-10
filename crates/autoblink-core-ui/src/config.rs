//! The settings of the box hardware. Only the firmware has them, so they use the indices from 110.
//! The host uses indices up to 33, and the WiFi rows of the firmware use 100 to 103.

use autoblink_core_midi::SettingItem;
use serde::{Deserialize, Serialize};

use crate::DmxMode;

pub const IDX_DMX_MODE: usize = 110;
pub const IDX_BEAT_CHANNEL: usize = 111;
pub const IDX_DMX_IN: usize = 112;
pub const IDX_DMX_CHANNELS: usize = 113;
pub const IDX_DMX_REFRESH: usize = 114;
pub const IDX_LOSS_ACTION: usize = 115;
pub const IDX_LOSS_TIME: usize = 116;
pub const IDX_ARTNET_UNIVERSE: usize = 117;
pub const IDX_SACN_UNIVERSE: usize = 118;
pub const IDX_ROTATE: usize = 120;
pub const IDX_BRIGHTNESS: usize = 121;

/// The indices of the rows that `BoxConfig` has.
pub const INDICES: [usize; 11] = [
    IDX_DMX_MODE,
    IDX_BEAT_CHANNEL,
    IDX_DMX_IN,
    IDX_DMX_CHANNELS,
    IDX_DMX_REFRESH,
    IDX_LOSS_ACTION,
    IDX_LOSS_TIME,
    IDX_ARTNET_UNIVERSE,
    IDX_SACN_UNIVERSE,
    IDX_ROTATE,
    IDX_BRIGHTNESS,
];

/// One step of the brightness for the arrows of the page.
const BRIGHTNESS_STEP: u8 = 16;

/// The values of "DMX Channels" for the arrows of the page. 0 is Auto.
const CHANNEL_STEPS: [u16; 13] = [0, 8, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512];

/// The range of the Art-Net port-address (15 bits).
const ARTNET_MAX: u16 = 32767;

/// The range of a sACN universe.
const SACN_RANGE: std::ops::RangeInclusive<u16> = 1..=63999;

/// The range of "Signal Loss Time" in seconds.
const LOSS_RANGE: std::ops::RangeInclusive<u8> = 5..=60;

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
    /// The channels in each output frame, 1 to 512. 0 is Auto: the last channel of the show, or
    /// the slot count of the live outside frame when that is larger. A show without lights gives
    /// 512.
    ///
    /// A cheap light can flicker and get dim when each frame has all 512 channels. Its chip
    /// then reads 22 500 bytes each second, and its dimming loses time.
    pub dmx_channels: u16,
    /// Sends a frame each 24 ms. When it is false, the box sends a frame after a change of a
    /// channel. It repeats a frame without a change 4 times each second.
    pub dmx_refresh_always: bool,
    /// What the box does when the network or DMX IN source stops. It holds its last frame for
    /// `loss_s` seconds. Then it keeps holding (false) or fades the light out to all 0 (true).
    pub loss_black: bool,
    /// The seconds that the box holds the last frame after the source stops, 1 to 60.
    pub loss_s: u8,
    /// The Art-Net port-address that the box receives, 0 to 32767.
    pub artnet_universe: u16,
    /// The sACN universe that the box receives, 1 to 63999.
    pub sacn_universe: u16,
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
            dmx_channels: 0,
            dmx_refresh_always: false,
            loss_black: false,
            loss_s: 10,
            artnet_universe: 0,
            sacn_universe: 1,
            rotate: false,
            brightness: 128,
        }
    }
}

impl BoxConfig {
    /// Puts each value back into its range. A value from NVS can come from an older firmware.
    pub fn sanitize(&mut self) {
        self.beat_channel = self.beat_channel.clamp(1, 512);
        if self.dmx_channels != 0 {
            self.dmx_channels = self.dmx_channels.clamp(1, 512);
        }
        self.loss_s = self.loss_s.clamp(*LOSS_RANGE.start(), *LOSS_RANGE.end());
        self.artnet_universe = self.artnet_universe.min(ARTNET_MAX);
        self.sacn_universe = self
            .sacn_universe
            .clamp(*SACN_RANGE.start(), *SACN_RANGE.end());
        self.brightness = self.brightness.max(1);
    }

    /// The channels in each output frame. `last` is the last channel of the show, or `None` for
    /// a show without lights. `source` is the slot count of the live outside frame (DMX IN,
    /// Art-Net or sACN), or `None` without one. In Auto, the larger of the two counts wins, so
    /// that a desk channel after the last light of the show still goes out. An explicit count
    /// always wins.
    pub fn frame_channels(&self, last: Option<usize>, source: Option<usize>) -> usize {
        match self.dmx_channels {
            0 => last.unwrap_or(512).max(source.unwrap_or(0)).clamp(1, 512),
            n => usize::from(n).clamp(1, 512),
        }
    }

    /// Builds the rows of the settings message.
    pub fn items(&self) -> Vec<SettingItem> {
        let channel = self.beat_channel.to_string();
        let frame = self.dmx_channels.to_string();
        let brightness = self.brightness.to_string();
        let artnet = self.artnet_universe.to_string();
        let sacn = self.sacn_universe.to_string();
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
                idx: IDX_DMX_CHANNELS,
                label: "DMX Channels",
                section: Some("DMX"),
                kind: "number",
                value: if self.dmx_channels == 0 {
                    "Auto".into()
                } else {
                    frame.clone()
                },
                raw: Some(frame),
                step: true,
                on: None,
            },
            SettingItem {
                idx: IDX_DMX_REFRESH,
                label: "DMX Refresh",
                section: Some("DMX"),
                kind: "cycle",
                value: if self.dmx_refresh_always {
                    "40 Hz"
                } else {
                    "On change"
                }
                .into(),
                raw: None,
                step: false,
                on: None,
            },
            SettingItem {
                idx: IDX_LOSS_ACTION,
                label: "Signal Loss",
                section: Some("DMX"),
                kind: "cycle",
                value: if self.loss_black { "Fade out" } else { "Hold" }.into(),
                raw: None,
                step: false,
                on: None,
            },
            SettingItem {
                idx: IDX_LOSS_TIME,
                label: "Signal Loss Time",
                section: Some("DMX"),
                kind: "number",
                value: format!("{} s", self.loss_s),
                raw: Some(self.loss_s.to_string()),
                step: true,
                on: None,
            },
            SettingItem {
                idx: IDX_ARTNET_UNIVERSE,
                label: "Art-Net Universe",
                section: Some("DMX"),
                kind: "number",
                value: artnet.clone(),
                raw: Some(artnet),
                step: true,
                on: None,
            },
            SettingItem {
                idx: IDX_SACN_UNIVERSE,
                label: "sACN Universe",
                section: Some("DMX"),
                kind: "number",
                value: sacn.clone(),
                raw: Some(sacn),
                step: true,
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
            IDX_DMX_REFRESH => {
                self.dmx_refresh_always = flag()?;
                Ok(())
            }
            IDX_DMX_CHANNELS => {
                self.dmx_channels = value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|v| *v <= 512)
                    .ok_or_else(range)?;
                Ok(())
            }
            IDX_LOSS_ACTION => {
                self.loss_black = flag()?;
                Ok(())
            }
            IDX_LOSS_TIME => {
                self.loss_s = value
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|v| LOSS_RANGE.contains(v))
                    .ok_or_else(range)?;
                Ok(())
            }
            IDX_ARTNET_UNIVERSE => {
                self.artnet_universe = value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|v| *v <= ARTNET_MAX)
                    .ok_or_else(range)?;
                Ok(())
            }
            IDX_SACN_UNIVERSE => {
                self.sacn_universe = value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|v| SACN_RANGE.contains(v))
                    .ok_or_else(range)?;
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
            IDX_DMX_REFRESH => self.dmx_refresh_always = !self.dmx_refresh_always,
            IDX_DMX_CHANNELS => {
                // A value between two steps goes to the next step in the direction.
                let v = self.dmx_channels;
                self.dmx_channels = if dir < 0 {
                    CHANNEL_STEPS
                        .iter()
                        .rev()
                        .find(|&&s| s < v)
                        .copied()
                        .unwrap_or(0)
                } else {
                    CHANNEL_STEPS
                        .iter()
                        .find(|&&s| s > v)
                        .copied()
                        .unwrap_or(512)
                };
            }
            IDX_LOSS_ACTION => self.loss_black = !self.loss_black,
            IDX_LOSS_TIME => {
                self.loss_s = self
                    .loss_s
                    .saturating_add_signed(dir)
                    .clamp(*LOSS_RANGE.start(), *LOSS_RANGE.end())
            }
            IDX_ARTNET_UNIVERSE => {
                self.artnet_universe = self
                    .artnet_universe
                    .saturating_add_signed(dir.into())
                    .min(ARTNET_MAX)
            }
            IDX_SACN_UNIVERSE => {
                self.sacn_universe = self
                    .sacn_universe
                    .saturating_add_signed(dir.into())
                    .clamp(*SACN_RANGE.start(), *SACN_RANGE.end())
            }
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
    fn dmx_channels_take_auto_or_a_count() {
        let mut cfg = BoxConfig::default();
        assert_eq!(cfg.frame_channels(Some(5), None), 5);
        assert_eq!(cfg.frame_channels(Some(100), None), 100);
        assert_eq!(cfg.frame_channels(None, None), 512);
        assert_eq!(cfg.items()[3].value, "Auto");
        cfg.set(IDX_DMX_CHANNELS, "64").unwrap();
        assert_eq!(cfg.frame_channels(Some(5), None), 64);
        assert_eq!(cfg.frame_channels(None, None), 64);
        cfg.set(IDX_DMX_CHANNELS, "5").unwrap();
        assert_eq!(cfg.frame_channels(None, None), 5);
        assert!(cfg.set(IDX_DMX_CHANNELS, "513").is_err());
        cfg.set(IDX_DMX_CHANNELS, "0").unwrap();
        assert_eq!(cfg.dmx_channels, 0);
    }

    #[test]
    fn dmx_channels_step_through_the_list() {
        let mut cfg = BoxConfig::default();
        cfg.cycle(IDX_DMX_CHANNELS, 1).unwrap();
        assert_eq!(cfg.dmx_channels, 8);
        cfg.cycle(IDX_DMX_CHANNELS, -1).unwrap();
        assert_eq!(cfg.dmx_channels, 0);
        cfg.cycle(IDX_DMX_CHANNELS, -1).unwrap();
        assert_eq!(cfg.dmx_channels, 0);
        cfg.set(IDX_DMX_CHANNELS, "100").unwrap();
        cfg.cycle(IDX_DMX_CHANNELS, 1).unwrap();
        assert_eq!(cfg.dmx_channels, 128);
        cfg.set(IDX_DMX_CHANNELS, "512").unwrap();
        cfg.cycle(IDX_DMX_CHANNELS, 1).unwrap();
        assert_eq!(cfg.dmx_channels, 512);
    }

    #[test]
    fn dmx_refresh_cycles_and_shows_its_label() {
        let mut cfg = BoxConfig::default();
        assert_eq!(cfg.items()[4].value, "On change");
        cfg.cycle(IDX_DMX_REFRESH, 1).unwrap();
        assert!(cfg.dmx_refresh_always);
        assert_eq!(cfg.items()[4].value, "40 Hz");
        cfg.set(IDX_DMX_REFRESH, "0").unwrap();
        assert!(!cfg.dmx_refresh_always);
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

    #[test]
    fn auto_channels_follow_the_source() {
        let mut cfg = BoxConfig::default();
        assert_eq!(cfg.frame_channels(Some(5), Some(512)), 512);
        assert_eq!(cfg.frame_channels(Some(5), Some(24)), 24);
        assert_eq!(cfg.frame_channels(Some(100), Some(24)), 100);
        assert_eq!(cfg.frame_channels(None, Some(24)), 512);
        cfg.set(IDX_DMX_CHANNELS, "8").unwrap();
        assert_eq!(cfg.frame_channels(Some(5), Some(512)), 8);
    }

    #[test]
    fn signal_loss_rows() {
        let mut cfg = BoxConfig::default();
        assert!(!cfg.loss_black);
        assert_eq!(cfg.loss_s, 10);
        let items = cfg.items();
        assert_eq!(items[5].label, "Signal Loss");
        assert_eq!(items[5].value, "Hold");
        assert_eq!(items[6].value, "10 s");
        assert_eq!(items[6].raw.as_deref(), Some("10"));
        cfg.cycle(IDX_LOSS_ACTION, 1).unwrap();
        assert!(cfg.loss_black);
        assert_eq!(cfg.items()[5].value, "Fade out");
        cfg.set(IDX_LOSS_ACTION, "0").unwrap();
        assert!(!cfg.loss_black);
        assert!(cfg.set(IDX_LOSS_ACTION, "2").is_err());
        cfg.set(IDX_LOSS_TIME, "60").unwrap();
        cfg.cycle(IDX_LOSS_TIME, 1).unwrap();
        assert_eq!(cfg.loss_s, 60);
        cfg.set(IDX_LOSS_TIME, "5").unwrap();
        cfg.cycle(IDX_LOSS_TIME, -1).unwrap();
        assert_eq!(cfg.loss_s, 5);
        assert!(cfg.set(IDX_LOSS_TIME, "4").is_err());
        assert!(cfg.set(IDX_LOSS_TIME, "61").is_err());
    }

    #[test]
    fn universe_rows() {
        let mut cfg = BoxConfig::default();
        assert_eq!(cfg.items()[7].value, "0");
        assert_eq!(cfg.items()[8].value, "1");
        cfg.cycle(IDX_ARTNET_UNIVERSE, -1).unwrap();
        assert_eq!(cfg.artnet_universe, 0);
        cfg.set(IDX_ARTNET_UNIVERSE, "32767").unwrap();
        cfg.cycle(IDX_ARTNET_UNIVERSE, 1).unwrap();
        assert_eq!(cfg.artnet_universe, 32767);
        assert!(cfg.set(IDX_ARTNET_UNIVERSE, "32768").is_err());
        cfg.cycle(IDX_SACN_UNIVERSE, -1).unwrap();
        assert_eq!(cfg.sacn_universe, 1);
        cfg.set(IDX_SACN_UNIVERSE, "63999").unwrap();
        cfg.cycle(IDX_SACN_UNIVERSE, 1).unwrap();
        assert_eq!(cfg.sacn_universe, 63999);
        assert!(cfg.set(IDX_SACN_UNIVERSE, "0").is_err());
        assert!(cfg.set(IDX_SACN_UNIVERSE, "64000").is_err());
    }

    #[test]
    fn old_json_gets_the_new_defaults() {
        let cfg: BoxConfig = serde_json::from_str(r#"{"dmx_channels":8,"rotate":true}"#).unwrap();
        assert!(!cfg.loss_black);
        assert_eq!(cfg.loss_s, 10);
        assert_eq!(cfg.artnet_universe, 0);
        assert_eq!(cfg.sacn_universe, 1);
    }

    #[test]
    fn sanitize_clamps_the_new_numbers() {
        let mut cfg = BoxConfig {
            loss_s: 0,
            artnet_universe: 40000,
            sacn_universe: 0,
            ..BoxConfig::default()
        };
        cfg.sanitize();
        assert_eq!(
            (cfg.loss_s, cfg.artnet_universe, cfg.sacn_universe),
            (5, 32767, 1)
        );
        cfg.loss_s = 200;
        cfg.sacn_universe = 65000;
        cfg.sanitize();
        assert_eq!((cfg.loss_s, cfg.sacn_universe), (60, 63999));
    }

    #[test]
    fn labels_are_ascii() {
        for i in BoxConfig::default().items() {
            if i.idx >= IDX_LOSS_ACTION && i.idx <= IDX_SACN_UNIVERSE {
                assert!(i.label.is_ascii() && i.value.is_ascii());
            }
        }
    }
}
