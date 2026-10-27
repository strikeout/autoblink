//! The editable settings: the table, the display values, and the validated changes.
//!

use network_interface::{NetworkInterface, NetworkInterfaceConfig, V4IfAddr};
use tokio::sync::watch;

use crate::config::{CcConfig, Config, NoteConfig, SharedConfig, Source};

#[derive(Debug, Clone)]
pub struct NetworkIfaceInfo {
    pub name: String,
    pub ip: String,
    pub priority: u8,
    pub priority_label: String,
}

#[derive(Debug, Clone, Copy)]
pub enum SettingKind {
    CycleInterface,
    CycleSource,
    Toggle,
    NumericU8,
    NumericU64,
    NumericI64,
    Text,
}

#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    pub label: &'static str,
    pub kind: SettingKind,
    pub section: Option<&'static str>,
}

pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        label: "Network Interface",
        kind: SettingKind::CycleInterface,
        section: None,
    },
    SettingDef {
        label: "Source Mode",
        kind: SettingKind::CycleSource,
        section: None,
    },
    SettingDef {
        label: "Device Number",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Clock Enabled",
        kind: SettingKind::Toggle,
        section: Some("MIDI Clock"),
    },
    SettingDef {
        label: "Clock Loop Sync",
        kind: SettingKind::Toggle,
        section: Some("MIDI Clock"),
    },
    SettingDef {
        label: "BPM Smoothing",
        kind: SettingKind::NumericU64,
        section: None,
    },
    SettingDef {
        label: "Latency Comp",
        kind: SettingKind::NumericI64,
        section: None,
    },
    SettingDef {
        label: "Phrase Lock Stable",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Notes Enabled",
        kind: SettingKind::Toggle,
        section: Some("MIDI Notes"),
    },
    SettingDef {
        label: "Note Channel",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Beat Note",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Downbeat Note",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Phrase Change Note",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Phrase Kind Base Note",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "No Bass Note",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "CC Enabled",
        kind: SettingKind::Toggle,
        section: Some("MIDI CC"),
    },
    SettingDef {
        label: "CC Channel",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "BPM Coarse CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "BPM Fine CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Pitch CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Bar Phase CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Beat Phase CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Playing CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Master Deck CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Phrase 16 CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Phrase Kind CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Phrase Progress CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    SettingDef {
        label: "Beats Left CC",
        kind: SettingKind::NumericU8,
        section: None,
    },
    // ── MTC settings ─────────────────────────────────────────────────────
    SettingDef {
        label: "MTC Enabled",
        kind: SettingKind::Toggle,
        section: Some("MIDI Timecode"),
    },
    SettingDef {
        label: "MTC Frame Rate",
        kind: SettingKind::CycleSource, // reuse cycle kind for frame rate cycling
        section: None,
    },
    // ── OSC settings ─────────────────────────────────────────────────────
    SettingDef {
        label: "OSC Enabled",
        kind: SettingKind::Toggle,
        section: Some("OSC Output"),
    },
    SettingDef {
        label: "OSC Targets",
        kind: SettingKind::Text,
        section: None,
    },
    SettingDef {
        label: "OSC Prefix",
        kind: SettingKind::Text,
        section: None,
    },
    // Index 33 is after the OSC rows, so that the indices of the other rows stay. The page puts
    // it into the group of its section.
    SettingDef {
        label: "Note Off Velocity 0",
        kind: SettingKind::Toggle,
        section: Some("MIDI Notes"),
    },
];

/// The index of the first note with a switch. The notes follow in [`note_slot`] order.
const NOTE_FIRST: usize = 10;
/// The index of the first CC with a switch. The CCs follow in [`cc_slot`] order.
const CC_FIRST: usize = 17;

/// Returns the number, the switch and the highest permitted number of the note at `i`.
fn note_slot(n: &mut NoteConfig, i: usize) -> Option<(&mut u8, &mut bool, u8)> {
    Some(match i {
        0 => (&mut n.beat, &mut n.send.beat, 127),
        1 => (&mut n.downbeat, &mut n.send.downbeat, 127),
        2 => (&mut n.phrase_change, &mut n.send.phrase_change, 127),
        // The phrase kind adds up to 11 to the base note.
        3 => (&mut n.phrase_base, &mut n.send.phrase_kind, 116),
        4 => (&mut n.no_bass, &mut n.send.no_bass, 127),
        _ => return None,
    })
}

/// Returns the number, the switch and the highest permitted number of the CC at `i`.
fn cc_slot(c: &mut CcConfig, i: usize) -> Option<(&mut u8, &mut bool, u8)> {
    Some(match i {
        0 => (&mut c.bpm_coarse, &mut c.send.bpm_coarse, 127),
        1 => (&mut c.bpm_fine, &mut c.send.bpm_fine, 127),
        2 => (&mut c.pitch, &mut c.send.pitch, 127),
        3 => (&mut c.bar_phase, &mut c.send.bar_phase, 127),
        4 => (&mut c.beat_phase, &mut c.send.beat_phase, 127),
        5 => (&mut c.playing, &mut c.send.playing, 127),
        6 => (&mut c.master_deck, &mut c.send.master_deck, 127),
        7 => (&mut c.phrase_16, &mut c.send.phrase_16, 127),
        8 => (&mut c.phrase_kind, &mut c.send.phrase_kind, 127),
        9 => (&mut c.phrase_progress, &mut c.send.phrase_progress, 127),
        10 => (&mut c.phrase_beats_left, &mut c.send.phrase_beats_left, 127),
        _ => return None,
    })
}

/// Returns the number, the switch and the highest number of the note or CC setting at `idx`.
fn slot<'a>(
    notes: &'a mut NoteConfig,
    cc: &'a mut CcConfig,
    idx: usize,
) -> Option<(&'a mut u8, &'a mut bool, u8)> {
    if idx >= CC_FIRST {
        cc_slot(cc, idx - CC_FIRST)
    } else if idx >= NOTE_FIRST {
        note_slot(notes, idx - NOTE_FIRST)
    } else {
        None
    }
}

/// Returns the number and the switch of the note or CC setting at `idx`.
fn slot_value(cfg: &Config, idx: usize) -> Option<(u8, bool)> {
    let (mut notes, mut cc) = (cfg.midi.notes, cfg.midi.cc);
    slot(&mut notes, &mut cc, idx).map(|(num, on, _)| (*num, *on))
}

/// Returns the switch state of a note or CC setting, or None for a setting without a switch.
pub fn switch_value(cfg: &Config, idx: usize) -> Option<bool> {
    slot_value(cfg, idx).map(|(_, on)| on)
}

pub fn interface_priority_label(priority: u8) -> &'static str {
    match priority {
        0 => "DJ",
        1 => "Eth",
        2 => "WiFi",
        3 => "Unknown",
        4 => "LinkLocal",
        5 => "VPN",
        _ => "?",
    }
}

/// Returns true for a numeric setting that [`apply_change`] steps up or down.
pub fn has_step(idx: usize) -> bool {
    matches!(idx, 6 | 7)
}

pub fn setting_kind(idx: usize) -> SettingKind {
    SETTINGS[idx].kind
}

pub fn selected_interface_index(cfg: &Config, interfaces: &[NetworkIfaceInfo]) -> Option<usize> {
    if interfaces.is_empty() {
        return None;
    }

    let wanted = cfg.interface.to_lowercase();
    interfaces
        .iter()
        .position(|iface| iface.name.to_lowercase() == wanted)
        .or_else(|| {
            if wanted == "auto" {
                Some(0)
            } else {
                interfaces
                    .iter()
                    .position(|iface| iface.name.to_lowercase().contains(&wanted))
            }
        })
        .or(Some(0))
}

pub fn format_interface_value(cfg: &Config, interfaces: &[NetworkIfaceInfo]) -> String {
    selected_interface_index(cfg, interfaces)
        .and_then(|idx| interfaces.get(idx))
        .map(|iface| {
            format!(
                "{} ({}) [prio {} {}]",
                iface.name, iface.ip, iface.priority, iface.priority_label
            )
        })
        .unwrap_or_else(|| cfg.interface.clone())
}

pub fn format_source_value(source: &Source) -> &'static str {
    match source {
        Source::Auto => "Auto",
        Source::ProLink => "ProLink (network)",
        Source::Link => "AbletonLink",
    }
}

/// Returns "✓" for true and "✗" for false.
fn tick(on: bool) -> String {
    if on { "✓" } else { "✗" }.to_string()
}

pub fn get_value(cfg: &Config, interfaces: &[NetworkIfaceInfo], idx: usize) -> String {
    if let Some((num, _)) = slot_value(cfg, idx) {
        return num.to_string();
    }
    match idx {
        0 => format_interface_value(cfg, interfaces),
        1 => format_source_value(&cfg.source).to_string(),
        2 => cfg.device_number.to_string(),
        3 => tick(cfg.midi.clock_enabled),
        4 => tick(cfg.midi.clock_loop_enabled),
        5 => format!("{} ms", cfg.midi.smoothing_ms),
        6 => format!("{} ms", cfg.midi.latency_compensation_ms),
        7 => format!("{} bars", cfg.midi.phrase_lock_stable_beats),
        8 => tick(cfg.midi.notes.enabled),
        9 => (cfg.midi.notes.channel + 1).to_string(),
        15 => tick(cfg.midi.cc.enabled),
        16 => (cfg.midi.cc.channel + 1).to_string(),
        28 => tick(cfg.midi.mtc.enabled),
        29 => cfg.midi.mtc.frame_rate.label().to_string(),
        30 => tick(cfg.osc.enabled),
        31 => cfg.osc.targets.join(", "),
        32 => cfg.osc.prefix.clone(),
        33 => tick(cfg.midi.notes.off_vel0),
        _ => String::new(),
    }
}

pub fn numeric_edit_value(cfg: &Config, idx: usize) -> Option<String> {
    match setting_kind(idx) {
        SettingKind::NumericU8 | SettingKind::NumericU64 => Some(match idx {
            2 => cfg.device_number.to_string(),
            5 => cfg.midi.smoothing_ms.to_string(),
            7 => cfg.midi.phrase_lock_stable_beats.to_string(),
            9 => (cfg.midi.notes.channel + 1).to_string(),
            16 => (cfg.midi.cc.channel + 1).to_string(),
            _ => slot_value(cfg, idx)?.0.to_string(),
        }),
        SettingKind::NumericI64 => Some(match idx {
            6 => cfg.midi.latency_compensation_ms.to_string(),
            _ => return None,
        }),
        SettingKind::Text => Some(get_value(cfg, &[], idx)),
        _ => None,
    }
}

pub fn apply_change(
    cfg: &mut Config,
    interfaces: &[NetworkIfaceInfo],
    idx: usize,
    direction: i8,
) -> bool {
    if let Some((_, on, _)) = slot(&mut cfg.midi.notes, &mut cfg.midi.cc, idx) {
        *on = !*on;
        return true;
    }
    match idx {
        0 => {
            let Some(current) = selected_interface_index(cfg, interfaces) else {
                return false;
            };
            let len = interfaces.len();
            if len == 0 {
                return false;
            }
            let next = if direction < 0 {
                if current == 0 {
                    len - 1
                } else {
                    current - 1
                }
            } else {
                (current + 1) % len
            };
            if let Some(iface) = interfaces.get(next) {
                cfg.interface = iface.name.clone();
                return true;
            }
            false
        }
        1 => {
            cfg.source = match (cfg.source.clone(), direction < 0) {
                (Source::Auto, false) => Source::ProLink,
                (Source::ProLink, false) => Source::Link,
                (Source::Link, false) => Source::Auto,
                (Source::Auto, true) => Source::Link,
                (Source::Link, true) => Source::ProLink,
                (Source::ProLink, true) => Source::Auto,
            };
            true
        }
        3 => {
            cfg.midi.clock_enabled = !cfg.midi.clock_enabled;
            true
        }
        4 => {
            cfg.midi.clock_loop_enabled = !cfg.midi.clock_loop_enabled;
            true
        }
        6 => {
            let step = if direction < 0 { -5 } else { 5 };
            cfg.midi.latency_compensation_ms =
                (cfg.midi.latency_compensation_ms + step).clamp(-1000, 1000);
            true
        }
        7 => {
            let step = if direction < 0 { -1 } else { 1 };
            let next = (cfg.midi.phrase_lock_stable_beats as i16 + step).clamp(1, 32) as u8;
            cfg.midi.phrase_lock_stable_beats = next;
            true
        }
        8 => {
            cfg.midi.notes.enabled = !cfg.midi.notes.enabled;
            true
        }
        33 => {
            cfg.midi.notes.off_vel0 = !cfg.midi.notes.off_vel0;
            true
        }
        15 => {
            cfg.midi.cc.enabled = !cfg.midi.cc.enabled;
            true
        }
        28 => {
            cfg.midi.mtc.enabled = !cfg.midi.mtc.enabled;
            true
        }
        29 => {
            cfg.midi.mtc.frame_rate = cfg.midi.mtc.frame_rate.next();
            true
        }
        30 => {
            cfg.osc.enabled = !cfg.osc.enabled;
            true
        }
        _ => false,
    }
}

pub fn apply_numeric_input(cfg: &mut Config, idx: usize, value: &str) -> bool {
    if let Some((num, _, max)) = slot(&mut cfg.midi.notes, &mut cfg.midi.cc, idx) {
        return value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= max)
            .map(|v| *num = v)
            .is_some();
    }
    match idx {
        2 => value
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=15).contains(v))
            .map(|v| cfg.device_number = v)
            .is_some(),
        5 => value
            .parse::<u64>()
            .ok()
            .filter(|v| *v <= 1000)
            .map(|v| cfg.midi.smoothing_ms = v)
            .is_some(),
        6 => value
            .parse::<i64>()
            .ok()
            .filter(|v| (-1000..=1000).contains(v))
            .map(|v| cfg.midi.latency_compensation_ms = v)
            .is_some(),
        7 => value
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=32).contains(v))
            .map(|v| cfg.midi.phrase_lock_stable_beats = v)
            .is_some(),
        9 => value
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=16).contains(v))
            .map(|v| cfg.midi.notes.channel = v - 1)
            .is_some(),
        16 => value
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=16).contains(v))
            .map(|v| cfg.midi.cc.channel = v - 1)
            .is_some(),
        31 => parse_osc_targets(value)
            .map(|v| cfg.osc.targets = v)
            .is_some(),
        32 => {
            let ok = value.starts_with('/') && !value.contains(char::is_whitespace);
            if ok {
                cfg.osc.prefix = value.to_string();
            }
            ok
        }
        _ => false,
    }
}

/// Parses a list of "IP:port" targets, separated by commas or spaces.
///
/// Returns None when one target is not an IP address with a port. An empty list is valid.
pub fn parse_osc_targets(value: &str) -> Option<Vec<String>> {
    value
        .split([',', ' '])
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.parse::<std::net::SocketAddr>()
                .ok()
                .map(|a| a.to_string())
        })
        .collect()
}

pub fn selected_interface_name_ip(cfg: &Config, interfaces: &[NetworkIfaceInfo]) -> String {
    selected_interface_index(cfg, interfaces)
        .and_then(|idx| interfaces.get(idx))
        .map(|iface| format!("{} ({})", iface.name, iface.ip))
        .unwrap_or_else(|| cfg.interface.clone())
}

/// Lists the IPv4 interfaces without loopback, best candidate first.
pub fn list_interfaces() -> Vec<NetworkIfaceInfo> {
    let Ok(ifaces) = NetworkInterface::show() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for iface in &ifaces {
        for addr in &iface.addr {
            if let network_interface::Addr::V4(V4IfAddr { ip, .. }) = addr {
                if ip.is_loopback() {
                    continue;
                }
                let priority = crate::app::interface_priority(&iface.name, ip);
                out.push(NetworkIfaceInfo {
                    name: iface.name.clone(),
                    ip: ip.to_string(),
                    priority,
                    priority_label: interface_priority_label(priority).to_string(),
                });
            }
        }
    }

    out.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(a.name.cmp(&b.name))
            .then(a.ip.cmp(&b.ip))
    });
    out
}

/// Steps the setting at `idx` in `direction` (toggle, cycle or a fixed step).
///
/// Returns true when the value changed. A change signals `cfg_change_tx` and writes a log line.
pub fn cycle(
    cfg: &SharedConfig,
    interfaces: &[NetworkIfaceInfo],
    cfg_change_tx: &watch::Sender<()>,
    idx: usize,
    direction: i8,
) -> bool {
    if idx >= SETTINGS.len() {
        return false;
    }
    let mut guard = cfg.write();
    if !apply_change(&mut guard, interfaces, idx, direction) {
        return false;
    }
    let _ = cfg_change_tx.send(());
    match idx {
        0 => tracing::info!(
            interface = %guard.interface,
            "Network interface changed; the network tasks re-bind within 2 s"
        ),
        1 => tracing::info!(
            mode = ?guard.source,
            "Source mode changed; restart may be required for running engines"
        ),
        3 => tracing::info!(
            enabled = guard.midi.clock_enabled,
            "MIDI clock setting changed"
        ),
        _ => log_setting_change(idx, &guard),
    }
    true
}

/// Parses `value` and stores it in the numeric or text setting at `idx`.
///
/// Returns false when the value is not a number or is out of range. Then the config does not change.
pub fn set_numeric(
    cfg: &SharedConfig,
    cfg_change_tx: &watch::Sender<()>,
    idx: usize,
    value: &str,
) -> bool {
    if idx >= SETTINGS.len() {
        return false;
    }
    let mut guard = cfg.write();
    if !apply_numeric_input(&mut guard, idx, value) {
        return false;
    }
    log_setting_change(idx, &guard);
    let _ = cfg_change_tx.send(());
    true
}

fn log_setting_change(idx: usize, cfg: &Config) {
    if let Some((num, on)) = slot_value(cfg, idx) {
        tracing::info!(
            setting = SETTINGS[idx].label,
            num,
            on,
            "MIDI mapping changed"
        );
        return;
    }
    match idx {
        2 => tracing::info!(
            device_number = cfg.device_number,
            "Device number changed; the virtual CDJ joins the network again"
        ),
        4 => tracing::info!(
            enabled = cfg.midi.clock_loop_enabled,
            "MIDI clock loop setting changed"
        ),
        5 => tracing::info!(
            smoothing_ms = cfg.midi.smoothing_ms,
            "BPM smoothing setting changed"
        ),
        6 => tracing::info!(
            latency_ms = cfg.midi.latency_compensation_ms,
            "Latency compensation changed"
        ),
        7 => tracing::info!(
            bars = cfg.midi.phrase_lock_stable_beats,
            "Phrase-lock bar resync interval changed"
        ),
        8 => tracing::info!(
            enabled = cfg.midi.notes.enabled,
            "MIDI notes setting changed"
        ),
        9 => tracing::info!(channel = cfg.midi.notes.channel + 1, "Note channel changed"),
        15 => tracing::info!(enabled = cfg.midi.cc.enabled, "MIDI CC setting changed"),
        16 => tracing::info!(channel = cfg.midi.cc.channel + 1, "CC channel changed"),
        28 => tracing::info!(enabled = cfg.midi.mtc.enabled, "MTC setting changed"),
        29 => tracing::info!(
            frame_rate = cfg.midi.mtc.frame_rate.label(),
            "MTC frame rate changed"
        ),
        30 => tracing::info!(enabled = cfg.osc.enabled, "OSC output setting changed"),
        31 => tracing::info!(targets = ?cfg.osc.targets, "OSC targets changed"),
        32 => tracing::info!(prefix = %cfg.osc.prefix, "OSC prefix changed"),
        33 => tracing::info!(
            off_vel0 = cfg.midi.notes.off_vel0,
            "Note off velocity 0 setting changed"
        ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OSC_ENABLED: usize = 30;
    const OSC_TARGETS: usize = 31;
    const OSC_PREFIX: usize = 32;

    #[test]
    fn osc_settings_are_validated() {
        let mut cfg = Config::default();
        assert!(apply_numeric_input(
            &mut cfg,
            OSC_TARGETS,
            "192.168.1.50:8000, 2.255.255.255:7000"
        ));
        assert_eq!(cfg.osc.targets, ["192.168.1.50:8000", "2.255.255.255:7000"]);
        assert_eq!(
            get_value(&cfg, &[], OSC_TARGETS),
            "192.168.1.50:8000, 2.255.255.255:7000"
        );

        assert!(!apply_numeric_input(
            &mut cfg,
            OSC_TARGETS,
            "desk.local:8000"
        ));
        assert!(!apply_numeric_input(&mut cfg, OSC_TARGETS, "192.168.1.50"));
        assert_eq!(cfg.osc.targets.len(), 2);
        assert!(apply_numeric_input(&mut cfg, OSC_TARGETS, ""));
        assert!(cfg.osc.targets.is_empty());

        assert!(apply_numeric_input(&mut cfg, OSC_PREFIX, "/lights"));
        assert!(!apply_numeric_input(&mut cfg, OSC_PREFIX, "lights"));
        assert!(!apply_numeric_input(&mut cfg, OSC_PREFIX, "/a b"));
        assert_eq!(cfg.osc.prefix, "/lights");

        assert!(apply_change(&mut cfg, &[], OSC_ENABLED, 1));
        assert!(cfg.osc.enabled);
        assert_eq!(
            numeric_edit_value(&cfg, OSC_PREFIX).as_deref(),
            Some("/lights")
        );
    }

    /// Every row of the table must reach its value: this catches an index that a new row shifted.
    #[test]
    fn every_setting_row_has_a_value() {
        let mut cfg = Config::default();
        for (idx, def) in SETTINGS.iter().enumerate() {
            match def.kind {
                SettingKind::NumericU8 | SettingKind::NumericU64 | SettingKind::NumericI64 => {
                    let raw = numeric_edit_value(&cfg, idx)
                        .unwrap_or_else(|| panic!("{} has no edit value", def.label));
                    assert!(
                        apply_numeric_input(&mut cfg, idx, &raw),
                        "{} rejects its own value {raw}",
                        def.label
                    );
                }
                SettingKind::Toggle => {
                    let before = get_value(&cfg, &[], idx);
                    assert!(apply_change(&mut cfg, &[], idx, 1), "{}", def.label);
                    assert_ne!(get_value(&cfg, &[], idx), before, "{}", def.label);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn note_and_cc_switches() {
        let mut cfg = Config::default();
        let label = |l: &str| SETTINGS.iter().position(|d| d.label == l).unwrap();

        let pitch = label("Pitch CC");
        assert_eq!(switch_value(&cfg, pitch), Some(true));
        assert!(apply_change(&mut cfg, &[], pitch, 1));
        assert!(!cfg.midi.cc.send.pitch);
        assert_eq!(switch_value(&cfg, pitch), Some(false));
        assert!(apply_numeric_input(&mut cfg, pitch, "12"));
        assert_eq!(cfg.midi.cc.pitch, 12);
        assert!(!apply_numeric_input(&mut cfg, pitch, "128"));

        let left = label("Beats Left CC");
        assert!(apply_numeric_input(&mut cfg, left, "99"));
        assert_eq!(cfg.midi.cc.phrase_beats_left, 99);

        let base = label("Phrase Kind Base Note");
        assert!(apply_numeric_input(&mut cfg, base, "116"));
        assert!(!apply_numeric_input(&mut cfg, base, "117"));
        assert!(apply_change(&mut cfg, &[], base, 1));
        assert!(!cfg.midi.notes.send.phrase_kind);

        let no_bass = label("No Bass Note");
        assert!(apply_change(&mut cfg, &[], no_bass, -1));
        assert!(!cfg.midi.notes.send.no_bass);

        assert!(apply_change(&mut cfg, &[], label("CC Enabled"), 1));
        assert!(!cfg.midi.cc.enabled);
        assert!(apply_change(&mut cfg, &[], label("Notes Enabled"), 1));
        assert!(!cfg.midi.notes.enabled);

        assert_eq!(switch_value(&cfg, label("CC Channel")), None);
        assert_eq!(switch_value(&cfg, label("MTC Enabled")), None);
    }
}
