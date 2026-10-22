//! The MIDI rows of the settings message of protocol v1. The indices, labels, values and ranges
//! are the same as in `host/src/settings.rs`, so the page shows the same rows and help texts.
//!
//! The firmware leaves out the rows that need data that it does not receive: the No Bass note
//! (14, from the USB MIDI of the XDJ-AZ), MTC and OSC.

use serde::Serialize;

use crate::config::{MidiConfig, CC_COUNT};

const CC_FIRST: usize = 17;

const CC_LABELS: [&str; CC_COUNT] = [
    "BPM Coarse CC",
    "BPM Fine CC",
    "Pitch CC",
    "Bar Phase CC",
    "Beat Phase CC",
    "Playing CC",
    "Master Deck CC",
    "Phrase 16 CC",
    "Phrase Kind CC",
    "Phrase Progress CC",
    "Beats Left CC",
];

/// The host indices that the firmware supports.
pub const INDICES: [usize; 24] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27];

/// One row of the settings message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingItem {
    pub idx: usize,
    pub label: &'static str,
    pub section: Option<&'static str>,
    pub kind: &'static str,
    pub value: String,
    pub raw: Option<String>,
    /// True when `cycle` steps this numeric setting up or down.
    pub step: bool,
    /// The switch of a note or CC. When false, the firmware does not send the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
}

fn tick(b: bool) -> String {
    if b { "✓" } else { "✗" }.into()
}

/// Builds the rows for the current configuration.
pub fn items(cfg: &MidiConfig) -> Vec<SettingItem> {
    INDICES.iter().map(|&idx| item(cfg, idx)).collect()
}

fn item(cfg: &MidiConfig, idx: usize) -> SettingItem {
    let toggle = |label, section, v: bool| SettingItem {
        idx,
        label,
        section,
        kind: "toggle",
        value: tick(v),
        raw: None,
        step: false,
        on: None,
    };
    let number = |label, section, value: String, raw: String, on| SettingItem {
        idx,
        label,
        section,
        kind: "number",
        value,
        raw: Some(raw),
        step: idx == 6 || idx == 7,
        on,
    };
    let plain = |label, n: String, on| number(label, None, n.clone(), n, on);
    match idx {
        3 => toggle("Clock Enabled", Some("MIDI Clock"), cfg.clock_enabled),
        4 => toggle("Clock Loop Sync", None, cfg.clock_loop_sync),
        5 => number("BPM Smoothing", None, format!("{} ms", cfg.smoothing_ms), cfg.smoothing_ms.to_string(), None),
        6 => number("Latency Comp", None, format!("{} ms", cfg.latency_ms), cfg.latency_ms.to_string(), None),
        7 => number("Phrase Lock Stable", None, format!("{} bars", cfg.phrase_lock_bars), cfg.phrase_lock_bars.to_string(), None),
        8 => toggle("Notes Enabled", Some("MIDI Notes"), cfg.notes_enabled),
        9 => plain("Note Channel", (cfg.note_channel + 1).to_string(), None),
        10 => plain("Beat Note", cfg.beat_note.to_string(), Some(cfg.send_beat)),
        11 => plain("Downbeat Note", cfg.downbeat_note.to_string(), Some(cfg.send_downbeat)),
        12 => plain("Phrase Change Note", cfg.phrase_change_note.to_string(), Some(cfg.send_phrase_change)),
        13 => plain("Phrase Kind Base Note", cfg.phrase_base_note.to_string(), Some(cfg.send_phrase_kind)),
        15 => toggle("CC Enabled", Some("MIDI CC"), cfg.cc_enabled),
        16 => plain("CC Channel", (cfg.cc_channel + 1).to_string(), None),
        _ => {
            let cc = cfg.ccs[idx - CC_FIRST];
            plain(CC_LABELS[idx - CC_FIRST], cc.num.to_string(), Some(cc.on))
        }
    }
}

/// Stores a value that the page typed. Returns the error text when the value is not valid. The
/// configuration does not change on an error.
pub fn set(cfg: &mut MidiConfig, idx: usize, value: &str) -> Result<(), String> {
    fn parse<T: core::str::FromStr + PartialOrd>(v: &str, lo: T, hi: T) -> Option<T> {
        v.trim().parse::<T>().ok().filter(|v| *v >= lo && *v <= hi)
    }
    let ok = match idx {
        5 => parse(value, 0u64, 1000).map(|v| cfg.smoothing_ms = v),
        6 => parse(value, -1000i64, 1000).map(|v| cfg.latency_ms = v),
        7 => parse(value, 1u8, 32).map(|v| cfg.phrase_lock_bars = v),
        9 => parse(value, 1u8, 16).map(|v| cfg.note_channel = v - 1),
        10 => parse(value, 0u8, 127).map(|v| cfg.beat_note = v),
        11 => parse(value, 0u8, 127).map(|v| cfg.downbeat_note = v),
        12 => parse(value, 0u8, 127).map(|v| cfg.phrase_change_note = v),
        // The highest kind code is 11, so the base note is 116 or lower.
        13 => parse(value, 0u8, 116).map(|v| cfg.phrase_base_note = v),
        16 => parse(value, 1u8, 16).map(|v| cfg.cc_channel = v - 1),
        i if (CC_FIRST..CC_FIRST + CC_COUNT).contains(&i) => {
            parse(value, 0u8, 127).map(|v| cfg.ccs[i - CC_FIRST].num = v)
        }
        _ => return Err(format!("Setting {idx} does not take a value")),
    };
    ok.ok_or_else(|| format!("Value \"{value}\" is out of range for setting {idx}"))
}

/// Flips a toggle or a note or CC switch, or steps a number. `dir` is -1 or 1.
pub fn cycle(cfg: &mut MidiConfig, idx: usize, dir: i8) -> Result<(), String> {
    match idx {
        3 => cfg.clock_enabled = !cfg.clock_enabled,
        4 => cfg.clock_loop_sync = !cfg.clock_loop_sync,
        6 => cfg.latency_ms = (cfg.latency_ms + if dir < 0 { -5 } else { 5 }).clamp(-1000, 1000),
        7 => cfg.phrase_lock_bars = (cfg.phrase_lock_bars as i16 + dir.signum() as i16).clamp(1, 32) as u8,
        8 => cfg.notes_enabled = !cfg.notes_enabled,
        10 => cfg.send_beat = !cfg.send_beat,
        11 => cfg.send_downbeat = !cfg.send_downbeat,
        12 => cfg.send_phrase_change = !cfg.send_phrase_change,
        13 => cfg.send_phrase_kind = !cfg.send_phrase_kind,
        15 => cfg.cc_enabled = !cfg.cc_enabled,
        i if (CC_FIRST..CC_FIRST + CC_COUNT).contains(&i) => {
            let cc = &mut cfg.ccs[i - CC_FIRST];
            cc.on = !cc.on;
        }
        _ => return Err(format!("Setting {idx} cannot be cycled")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_match_the_host_format() {
        let cfg = MidiConfig::default();
        let rows = items(&cfg);
        assert_eq!(rows.len(), INDICES.len());
        let v = serde_json::to_value(&rows).unwrap();
        assert_eq!(v[0]["label"], "Clock Enabled");
        assert_eq!(v[0]["section"], "MIDI Clock");
        assert_eq!(v[0]["value"], "✓");
        assert!(v[0]["raw"].is_null());
        assert!(v[0].get("on").is_none());
        let latency = rows.iter().find(|r| r.idx == 6).unwrap();
        assert_eq!((latency.value.as_str(), latency.step), ("0 ms", true));
        let channel = rows.iter().find(|r| r.idx == 9).unwrap();
        assert_eq!(channel.raw.as_deref(), Some("10"));
        let phrase16 = rows.iter().find(|r| r.idx == 24).unwrap();
        assert_eq!((phrase16.label, phrase16.on), ("Phrase 16 CC", Some(true)));
    }

    #[test]
    fn set_applies_the_host_ranges() {
        let mut cfg = MidiConfig::default();
        set(&mut cfg, 9, "16").unwrap();
        assert_eq!(cfg.note_channel, 15);
        assert!(set(&mut cfg, 9, "17").is_err());
        assert!(set(&mut cfg, 7, "0").is_err());
        assert!(set(&mut cfg, 5, "1001").is_err());
        set(&mut cfg, 6, "-20").unwrap();
        assert_eq!(cfg.latency_ms, -20);
        set(&mut cfg, 18, "99").unwrap();
        assert_eq!(cfg.ccs[1].num, 99);
        assert!(set(&mut cfg, 3, "1").is_err());
        assert!(set(&mut cfg, 13, "117").is_err());
        set(&mut cfg, 27, "50").unwrap();
        assert_eq!(cfg.ccs[10].num, 50);
        assert!(set(&mut cfg, 14, "39").is_err());
    }

    #[test]
    fn a_stored_config_with_8_ccs_keeps_them_and_gets_the_new_defaults() {
        let mut v = serde_json::to_value(MidiConfig::default()).unwrap();
        v["ccs"].as_array_mut().unwrap().truncate(8);
        v["ccs"][0]["num"] = 99.into();
        let cfg: MidiConfig = serde_json::from_value(v).unwrap();
        assert_eq!((cfg.ccs[0].num, cfg.ccs[8].num, cfg.ccs[10].num), (99, 20, 22));
    }

    #[test]
    fn cycle_flips_switches_and_steps_numbers() {
        let mut cfg = MidiConfig::default();
        cycle(&mut cfg, 3, 1).unwrap();
        assert!(!cfg.clock_enabled);
        cycle(&mut cfg, 6, -1).unwrap();
        assert_eq!(cfg.latency_ms, -5);
        cycle(&mut cfg, 7, 1).unwrap();
        assert_eq!(cfg.phrase_lock_bars, 5);
        cycle(&mut cfg, 24, 1).unwrap();
        assert!(!cfg.ccs[7].on);
        cycle(&mut cfg, 10, -1).unwrap();
        assert!(!cfg.send_beat);
        assert!(cycle(&mut cfg, 9, 1).is_err());
    }
}
