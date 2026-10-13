//! The editable settings: the table, the display values, and the validated changes.
//!

use network_interface::{NetworkInterface, NetworkInterfaceConfig, V4IfAddr};
use tokio::sync::watch;

use crate::config::{Config, SharedConfig, Source};

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
        label: "Note Channel",
        kind: SettingKind::NumericU8,
        section: Some("MIDI Notes"),
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
        label: "CC Channel",
        kind: SettingKind::NumericU8,
        section: Some("MIDI CC"),
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
];

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

pub fn get_value(cfg: &Config, interfaces: &[NetworkIfaceInfo], idx: usize) -> String {
    match idx {
        0 => format_interface_value(cfg, interfaces),
        1 => format_source_value(&cfg.source).to_string(),
        2 => cfg.device_number.to_string(),
        3 => {
            if cfg.midi.clock_enabled {
                "✓".to_string()
            } else {
                "✗".to_string()
            }
        }
        4 => {
            if cfg.midi.clock_loop_enabled {
                "✓".to_string()
            } else {
                "✗".to_string()
            }
        }
        5 => format!("{} ms", cfg.midi.smoothing_ms),
        6 => format!("{} ms", cfg.midi.latency_compensation_ms),
        7 => format!("{} bars", cfg.midi.phrase_lock_stable_beats),
        8 => (cfg.midi.notes.channel + 1).to_string(),
        9 => cfg.midi.notes.beat.to_string(),
        10 => cfg.midi.notes.downbeat.to_string(),
        11 => cfg.midi.notes.phrase_change.to_string(),
        12 => (cfg.midi.cc.channel + 1).to_string(),
        13 => cfg.midi.cc.bpm_coarse.to_string(),
        14 => cfg.midi.cc.bpm_fine.to_string(),
        15 => cfg.midi.cc.pitch.to_string(),
        16 => cfg.midi.cc.bar_phase.to_string(),
        17 => cfg.midi.cc.beat_phase.to_string(),
        18 => cfg.midi.cc.playing.to_string(),
        19 => cfg.midi.cc.master_deck.to_string(),
        20 => cfg.midi.cc.phrase_16.to_string(),
        21 => {
            if cfg.midi.mtc.enabled {
                "✓".to_string()
            } else {
                "✗".to_string()
            }
        }
        22 => cfg.midi.mtc.frame_rate.label().to_string(),
        _ => String::new(),
    }
}

pub fn numeric_edit_value(cfg: &Config, idx: usize) -> Option<String> {
    match setting_kind(idx) {
        SettingKind::NumericU8 | SettingKind::NumericU64 => Some(match idx {
            2 => cfg.device_number.to_string(),
            5 => cfg.midi.smoothing_ms.to_string(),
            7 => cfg.midi.phrase_lock_stable_beats.to_string(),
            8 => (cfg.midi.notes.channel + 1).to_string(),
            9 => cfg.midi.notes.beat.to_string(),
            10 => cfg.midi.notes.downbeat.to_string(),
            11 => cfg.midi.notes.phrase_change.to_string(),
            12 => (cfg.midi.cc.channel + 1).to_string(),
            13 => cfg.midi.cc.bpm_coarse.to_string(),
            14 => cfg.midi.cc.bpm_fine.to_string(),
            15 => cfg.midi.cc.pitch.to_string(),
            16 => cfg.midi.cc.bar_phase.to_string(),
            17 => cfg.midi.cc.beat_phase.to_string(),
            18 => cfg.midi.cc.playing.to_string(),
            19 => cfg.midi.cc.master_deck.to_string(),
            20 => cfg.midi.cc.phrase_16.to_string(),
            _ => return None,
        }),
        SettingKind::NumericI64 => Some(match idx {
            6 => cfg.midi.latency_compensation_ms.to_string(),
            _ => return None,
        }),
        _ => None,
    }
}

pub fn apply_change(
    cfg: &mut Config,
    interfaces: &[NetworkIfaceInfo],
    idx: usize,
    direction: i8,
) -> bool {
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
        21 => {
            cfg.midi.mtc.enabled = !cfg.midi.mtc.enabled;
            true
        }
        22 => {
            cfg.midi.mtc.frame_rate = cfg.midi.mtc.frame_rate.next();
            true
        }
        _ => false,
    }
}

pub fn apply_numeric_input(cfg: &mut Config, idx: usize, value: &str) -> bool {
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
        8 => value
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=16).contains(v))
            .map(|v| cfg.midi.notes.channel = v - 1)
            .is_some(),
        9 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.notes.beat = v)
            .is_some(),
        10 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.notes.downbeat = v)
            .is_some(),
        11 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.notes.phrase_change = v)
            .is_some(),
        12 => value
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=16).contains(v))
            .map(|v| cfg.midi.cc.channel = v - 1)
            .is_some(),
        13 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.bpm_coarse = v)
            .is_some(),
        14 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.bpm_fine = v)
            .is_some(),
        15 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.pitch = v)
            .is_some(),
        16 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.bar_phase = v)
            .is_some(),
        17 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.beat_phase = v)
            .is_some(),
        18 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.playing = v)
            .is_some(),
        19 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.master_deck = v)
            .is_some(),
        20 => value
            .parse::<u8>()
            .ok()
            .filter(|v| *v <= 127)
            .map(|v| cfg.midi.cc.phrase_16 = v)
            .is_some(),
        _ => false,
    }
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
        3 => tracing::info!(enabled = guard.midi.clock_enabled, "MIDI clock setting changed"),
        _ => log_setting_change(idx, &guard),
    }
    true
}

/// Parses `value` and stores it in the numeric setting at `idx`.
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
    match idx {
        2 => tracing::info!(
            device_number = cfg.device_number,
            "Device number changed; restart required for network tasks"
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
        8 => tracing::info!(channel = cfg.midi.notes.channel + 1, "Note channel changed"),
        9 => tracing::info!(note = cfg.midi.notes.beat, "Beat note changed"),
        10 => tracing::info!(note = cfg.midi.notes.downbeat, "Downbeat note changed"),
        11 => tracing::info!(
            note = cfg.midi.notes.phrase_change,
            "Phrase change note changed"
        ),
        12 => tracing::info!(channel = cfg.midi.cc.channel + 1, "CC channel changed"),
        13 => tracing::info!(cc = cfg.midi.cc.bpm_coarse, "BPM coarse CC changed"),
        14 => tracing::info!(cc = cfg.midi.cc.bpm_fine, "BPM fine CC changed"),
        15 => tracing::info!(cc = cfg.midi.cc.pitch, "Pitch CC changed"),
        16 => tracing::info!(cc = cfg.midi.cc.bar_phase, "Bar phase CC changed"),
        17 => tracing::info!(cc = cfg.midi.cc.beat_phase, "Beat phase CC changed"),
        18 => tracing::info!(cc = cfg.midi.cc.playing, "Playing CC changed"),
        19 => tracing::info!(cc = cfg.midi.cc.master_deck, "Master deck CC changed"),
        20 => tracing::info!(cc = cfg.midi.cc.phrase_16, "Phrase 16 CC changed"),
        21 => tracing::info!(enabled = cfg.midi.mtc.enabled, "MTC setting changed"),
        22 => tracing::info!(
            frame_rate = cfg.midi.mtc.frame_rate.label(),
            "MTC frame rate changed"
        ),
        _ => {}
    }
}
