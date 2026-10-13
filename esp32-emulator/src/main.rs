use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch, RwLock};
use tokio::time::{interval, Duration};

/// The shared page. The host and the ESP32 serve the same file.
const PAGE: &str = include_str!("../../webui/index.html");

const LOG_ENTRIES: usize = 100;

/// The settings that the emulator models: (host index, label, section, kind).
const SETTINGS: &[(usize, &str, Option<&str>, &str)] = &[
    (3, "Clock Enabled", Some("MIDI Clock"), "toggle"),
    (5, "BPM Smoothing", None, "number"),
    (6, "Latency Comp", None, "number"),
    (8, "Note Channel", Some("MIDI Notes"), "number"),
    (9, "Beat Note", None, "number"),
    (10, "Downbeat Note", None, "number"),
    (12, "CC Channel", Some("MIDI CC"), "number"),
    (13, "BPM Coarse CC", None, "number"),
    (14, "BPM Fine CC", None, "number"),
    (15, "Pitch CC", None, "number"),
    (16, "Bar Phase CC", None, "number"),
    (17, "Beat Phase CC", None, "number"),
    (18, "Playing CC", None, "number"),
];

#[derive(Debug, Clone)]
struct DeckState {
    device_number: u8,
    device_type: u8,
    is_master: bool,
    is_playing: bool,
    is_sync: bool,
    is_on_air: bool,
    bpm: f64,
    pitch: f64,
    beat: u8,
    phrase_16: u8,
    name: String,
    ip: Option<[u8; 4]>,
    last_seen: std::time::Instant,
}

impl Default for DeckState {
    fn default() -> Self {
        Self {
            device_number: 0,
            device_type: 0,
            is_master: false,
            is_playing: false,
            is_sync: false,
            is_on_air: false,
            bpm: 0.0,
            pitch: 0.0,
            beat: 0,
            phrase_16: 0,
            name: String::new(),
            ip: None,
            last_seen: std::time::Instant::now(),
        }
    }
}

struct LogEntry {
    message: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MidiConfig {
    output: String,
    clock_enabled: bool,
    smoothing_ms: u64,
    latency_ms: i64,
    note_channel: u8,
    beat_note: u8,
    downbeat_note: u8,
    cc_channel: u8,
    bpm_coarse: u8,
    bpm_fine: u8,
    pitch_cc: u8,
    bar_phase_cc: u8,
    beat_phase_cc: u8,
    playing_cc: u8,
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self {
            output: "auto".to_string(),
            clock_enabled: true,
            smoothing_ms: 30,
            latency_ms: 0,
            note_channel: 10,
            beat_note: 36,
            downbeat_note: 37,
            cc_channel: 0,
            bpm_coarse: 1,
            bpm_fine: 33,
            pitch_cc: 2,
            bar_phase_cc: 3,
            beat_phase_cc: 4,
            playing_cc: 5,
        }
    }
}

struct EmulatorState {
    virtual_device_num: u8,
    decks: Vec<DeckState>,
    tick: u64,
    master_idx: usize,
    logs: Vec<LogEntry>,
    /// The number of log lines since the start. It is the sequence number of the next line.
    log_total: u64,
    started: Instant,
    midi_config: MidiConfig,
    midi_clock_count: u64,
    midi_note_count: u64,
    midi_cc_count: u64,
    last_note: u8,
    last_note_ch: u8,
    last_cc: u8,
    last_cc_val: u8,
    last_cc_ch: u8,
}

impl EmulatorState {
    fn new() -> Self {
        let mut decks = Vec::with_capacity(16);
        for _ in 0..16 {
            decks.push(DeckState::default());
        }

        let mut state = Self {
            virtual_device_num: 5,
            decks,
            tick: 0,
            master_idx: 0,
            logs: Vec::with_capacity(LOG_ENTRIES),
            log_total: 0,
            started: Instant::now(),
            midi_config: MidiConfig::default(),
            midi_clock_count: 0,
            midi_note_count: 0,
            midi_cc_count: 0,
            last_note: 0,
            last_note_ch: 0,
            last_cc: 0,
            last_cc_val: 0,
            last_cc_ch: 0,
        };

        state.add_log(0, "INFO", "xdj-clock ESP32 v1.1.7 starting");
        state.add_log(0, "INFO", "WiFi AP: xdj-midi-setup (pass: xdjclock123)");
        state.add_log(0, "INFO", "Ethernet: 10.10.10.50/24");
        state.add_log(0, "INFO", "MIDI: GPIO1(IN) GPIO3(OUT) @ 31250");
        state.add_log(0, "INFO", "Pro DJ Link virtual CDJ #5");
        state.add_log(0, "INFO", "Listening on UDP:50000,50001,50002");
        state.add_log(0, "INFO", "Proactive participation enabled (keep-alive + status templates)");
        state.add_log(0, "INFO", "Ready for Pro DJ Link packets...");
        state
    }

    fn add_log(&mut self, _level: u8, prefix: &str, msg: impl Into<String>) {
        // The host log format: "  12.345678s  INFO target: message".
        let full_msg = format!(
            "{:>12.6}s {:>5} emulator: {}",
            self.started.elapsed().as_secs_f64(),
            prefix,
            msg.into()
        );
        println!("{}", full_msg);
        if self.logs.len() >= LOG_ENTRIES {
            self.logs.remove(0);
        }
        self.logs.push(LogEntry { message: full_msg });
        self.log_total += 1;
    }

    fn refresh_master(&mut self) {
        let prev_master = self.master_idx;
        
        // 1. Priority: Explicit master flag from a deck
        let explicit_master = self.decks.iter().position(|d| d.is_master && d.device_number != 0);
        
        if let Some(idx) = explicit_master {
            if self.master_idx != idx {
                let deck = &self.decks[idx];
                self.add_log(0, "INFO", format!("Master handoff: #{} ({}) is now MASTER", deck.device_number, deck.name));
            }
            self.master_idx = idx;
            return;
        }

        // 2. Fallback: First playing deck (if no explicit master)
        let playing_deck = self.decks.iter().position(|d| d.is_playing && d.device_number != 0);
        if let Some(idx) = playing_deck {
            if self.master_idx != idx {
                let deck = &self.decks[idx];
                self.add_log(0, "INFO", format!("Master fallback: #{} ({}) is PLAYING", deck.device_number, deck.name));
            }
            self.master_idx = idx;
        } else {
            // 3. Last resort: Keep current master if it still exists, otherwise reset to 0
            if self.master_idx >= self.decks.len() || self.decks[self.master_idx].device_number == 0 {
                if prev_master != 0 {
                    self.add_log(0, "WARN", "Master lost: no active or playing decks");
                }
                self.master_idx = 0;
            }
        }
    }

    fn handle_packet(&mut self, addr: SocketAddr, port: u16, data: &[u8]) {
        if data.len() < 11 {
            return;
        }

        if &data[..10] != xdj_core_prolink::MAGIC {
            return;
        }

        let pkt_type = data[10];

        match port {
            xdj_core_prolink::PORT_DISCOVERY => self.handle_discovery_packet(addr, pkt_type, data),
            xdj_core_prolink::PORT_BEAT => self.handle_beat_packet(addr, pkt_type, data),
            xdj_core_prolink::PORT_STATUS => self.handle_status_packet(addr, pkt_type, data),
            _ => {}
        }

        self.refresh_master();
    }

    fn handle_discovery_packet(&mut self, addr: SocketAddr, pkt_type: u8, data: &[u8]) {
        use xdj_core_prolink::parse as packets;
        match pkt_type {
            xdj_core_prolink::PKT_KEEPALIVE => {
                if let Some(ka) = packets::parse_keepalive(data) {
                    let type_name = if ka.device_number == 33 {
                        "Mixer"
                    } else if ka.device_number >= 1 && ka.device_number <= 4 {
                        "CDJ"
                    } else {
                        match ka.device_type {
                            1 => "Mixer",
                            2 | 4 => "CDJ",
                            3 => "XDJ",
                            _ => "Unknown",
                        }
                    };

                    if let Some(idx) = self.find_deck(ka.device_number) {
                        self.decks[idx].ip = Some(ka.ip);
                        self.decks[idx].last_seen = std::time::Instant::now();
                        if self.decks[idx].name.is_empty() {
                            self.decks[idx].device_type = ka.device_type;
                            self.decks[idx].name = ka.name.clone();
                            self.add_log(0, "INFO", format!("Identified: {} #{} ({})", ka.name, ka.device_number, type_name));
                        }
                    } else if let Some(slot) = self.find_empty_slot() {
                        self.decks[slot].device_number = ka.device_number;
                        self.decks[slot].device_type = ka.device_type;
                        self.decks[slot].name = ka.name.clone();
                        self.decks[slot].ip = Some(ka.ip);
                        self.decks[slot].last_seen = std::time::Instant::now();
                        self.add_log(0, "INFO", format!("Discovered: {} #{} ({})", ka.name, ka.device_number, type_name));
                    }
                }
            }
            xdj_core_prolink::PKT_ANNOUNCE => {
                if data.len() >= 12 {
                    let device_num = data[11];
                    self.add_log(2, "DEBUG", format!("Announce #{}", device_num));
                }
            }
            0x00 | 0x02 | 0x04 => {
                if data.len() >= 12 {
                    let device_num = data[11];
                    self.add_log(2, "DEBUG", format!("Claim {}: device #{}", pkt_type, device_num));
                }
            }
            0x08 => {
                self.add_log(1, "WARN", format!("Conflict: {}", addr));
            }
            _ => {}
        }
    }

    fn handle_beat_packet(&mut self, _addr: SocketAddr, pkt_type: u8, data: &[u8]) {
        use xdj_core_prolink::parse as packets;
        match pkt_type {
            xdj_core_prolink::PKT_BEAT => {
                if let Some(beat) = packets::parse_beat(data) {
                    if let Some(idx) = self.find_deck(beat.device_number) {
                        self.decks[idx].bpm = beat.effective_bpm;
                        self.decks[idx].pitch = beat.pitch_pct;
                        self.decks[idx].beat = if beat.beat_in_bar == 0 { 4 } else { beat.beat_in_bar };
                        self.decks[idx].last_seen = std::time::Instant::now();

                        let old_beat = self.decks[idx].beat;
                        if old_beat == 4 && self.decks[idx].beat == 1 {
                            self.decks[idx].phrase_16 = (self.decks[idx].phrase_16 % 16) + 1;
                        }

                        // Beat packets are authoritative for "playing" state
                        self.decks[idx].is_playing = true;
                    }

                    if self.decks.get(self.master_idx).map(|d| d.device_number) == Some(beat.device_number) {
                        self.midi_note_count += 1;
                        self.midi_cc_count += 1;
                        self.last_note = self.midi_config.beat_note;
                        self.last_note_ch = self.midi_config.note_channel;
                        self.last_cc = self.midi_config.beat_phase_cc;
                        self.last_cc_val = (beat.beat_in_bar * 31) as u8;
                        self.last_cc_ch = self.midi_config.cc_channel;
                    }

                    self.add_log(2, "DEBUG", format!("Beat #{} bpm={:.2} beat={}/4", beat.device_number, beat.effective_bpm, beat.beat_in_bar));
                }
            }
            xdj_core_prolink::PKT_ABS_POSITION => {
                if let Some(abs) = packets::parse_abs_position(data) {
                    if let Some(idx) = self.find_deck(abs.device_number) {
                        self.decks[idx].bpm = abs.effective_bpm;
                        self.decks[idx].pitch = abs.pitch_pct;
                        self.decks[idx].last_seen = std::time::Instant::now();
                    }
                    if self.decks.get(self.master_idx).map(|d| d.device_number) == Some(abs.device_number) {
                        self.midi_cc_count += 1;
                        self.last_cc = self.midi_config.bar_phase_cc;
                        self.last_cc_val = 64;
                        self.last_cc_ch = self.midi_config.cc_channel;
                    }
                    self.add_log(2, "DEBUG", format!("AbsPosition #{} pos={}ms bpm={:.2}", abs.device_number, abs.playhead_ms, abs.effective_bpm));
                }
            }
            _ => {}
        }
    }

    fn handle_status_packet(&mut self, _addr: SocketAddr, pkt_type: u8, data: &[u8]) {
        use xdj_core_prolink::parse as packets;
        match pkt_type {
            xdj_core_prolink::PKT_CDJ_STATUS => {
                if let Some(s) = packets::parse_cdj_status(data) {
                    if let Some(idx) = self.find_deck(s.device_number) {
                        self.decks[idx].is_playing = s.play_state.is_playing() || s.is_playing_flag;
                        self.decks[idx].is_sync = s.is_sync;
                        self.decks[idx].is_on_air = s.is_on_air;
                        self.decks[idx].pitch = s.pitch_pct;
                        self.decks[idx].last_seen = std::time::Instant::now();
                        
                        if s.is_master {
                            // Clear master flag from all other decks if this one claims it
                            for i in 0..self.decks.len() {
                                if i != idx {
                                    self.decks[i].is_master = false;
                                }
                            }
                        }
                        
                        self.decks[idx].is_master = s.is_master;
                        self.decks[idx].bpm = s.effective_bpm;
                        self.decks[idx].beat = s.beat_in_bar;
                    }

                    let playing_str = if s.play_state.is_playing() || s.is_playing_flag { "playing" } else { "stopped" };
                    let master_str = if s.is_master { " MASTER" } else { "" };
                    self.add_log(0, "INFO", format!("Status #{} {} bpm={:.2}{} (beat={})", s.device_number, playing_str, s.effective_bpm, master_str, s.beat_in_bar));
                }
            }
            xdj_core_prolink::PKT_MIXER_STATUS => {
                if let Some(s) = packets::parse_mixer_status(data) {
                    if let Some(idx) = self.find_deck(s.device_number) {
                        self.decks[idx].last_seen = std::time::Instant::now();
                        if s.is_master {
                            for i in 0..self.decks.len() {
                                if i != idx {
                                    self.decks[i].is_master = false;
                                }
                            }
                        }
                        self.decks[idx].is_master = s.is_master;
                        if let Some(bpm) = s.track_bpm {
                            self.decks[idx].bpm = bpm;
                        }
                    }
                    self.add_log(0, "DEBUG", format!("Mixer #{} is {} (bpm={:?})", s.device_number, if s.is_master { "master" } else { "slave" }, s.track_bpm));
                }
            }
            _ => {}
        }
    }

    fn find_empty_slot(&self) -> Option<usize> {
        for (i, d) in self.decks.iter().enumerate() {
            if d.device_number == 0 { return Some(i); }
        }
        None
    }

    fn find_deck(&self, device_num: u8) -> Option<usize> {
        self.decks.iter().position(|d| d.device_number == device_num)
    }

    fn tick(&mut self) {
        self.tick += 1;
        
        let now = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);

        // Expire "playing" state and master flag if no packets for a while
        for deck in self.decks.iter_mut().filter(|d| d.device_number != 0) {
            if now.duration_since(deck.last_seen) > timeout {
                if deck.is_playing {
                    deck.is_playing = false;
                }
                if deck.is_master {
                    deck.is_master = false;
                }
            }
        }

        self.refresh_master();

        let master_idx = self.master_idx;
        let master_bpm = if master_idx < self.decks.len() { self.decks[master_idx].bpm } else { 0.0 };
        let master_playing = if master_idx < self.decks.len() { self.decks[master_idx].is_playing } else { false };
        
        if master_bpm > 0.0 && master_playing {
            let clocks_to_add = ((24.0 * master_bpm) / 600.0).round() as u64;
            if clocks_to_add > 0 {
                self.midi_clock_count += clocks_to_add;
            } else if self.tick % 5 == 0 {
                self.midi_clock_count += 1;
            }
        }

        if self.tick % 500 == 0 {
            self.add_log(0, "INFO", "MIDI clock: 24PPQ running");
        }
    }

    fn visible_decks(&self) -> Vec<&DeckState> {
        self.decks.iter()
            .filter(|d| d.device_number != 0 && d.device_number != self.virtual_device_num)
            .collect()
    }

    fn device_type_name(t: u8) -> &'static str {
        match t {
            1 => "Mixer",
            2 => "CDJ",
            3 => "XDJ",
            _ => "Unknown",
        }
    }

    fn master(&self) -> Option<&DeckState> {
        self.decks
            .get(self.master_idx)
            .filter(|d| d.device_number != 0)
    }

    /// Builds the protocol v1 status message. The emulator models a subset of the host fields.
    fn status_json(&self) -> String {
        let master = self.master();
        let decks: Vec<Value> = self
            .visible_decks()
            .iter()
            .map(|d| {
                json!({
                    "num": d.device_number,
                    "name": if d.name.is_empty() { Self::device_type_name(d.device_type).to_string() } else { d.name.replace('\0', " ") },
                    "playing": d.is_playing,
                    "master": d.is_master,
                    "virtual_master": false,
                    "on_air": d.is_on_air,
                    "sync": d.is_sync,
                    "bpm": (d.bpm > 0.0).then_some(d.bpm),
                    "pitch": d.pitch,
                    "beat": (1..=4).contains(&d.beat).then_some(d.beat),
                })
            })
            .collect();
        let playing = master.is_some_and(|m| m.is_playing);
        json!({
            "type": "status",
            "source": master.map(|_| "ProLink (network)"),
            "mode": "ProLink (network)",
            "iface": "emulated 10.10.10.50",
            "master": {
                "device": master.map_or(0, |m| m.device_number),
                "virtual_master": false,
                "bpm": master.map_or(0.0, |m| m.bpm),
                "pitch": master.map_or(0.0, |m| m.pitch),
                "beat": master.map_or(0, |m| m.beat),
                "phrase16": master.map_or(0, |m| m.phrase_16),
                "playing": playing,
            },
            "clock": {
                "out16": master.map_or(0, |m| m.phrase_16),
                "state": if playing && self.midi_config.clock_enabled { "lock" } else { "idle" },
                "delta_ms": null,
            },
            "no_devices": decks.is_empty(),
            "decks": decks,
            "link_peers": 0,
            "link_active": false,
            "mixer": null,
            "output": {
                "port": self.midi_config.output,
                "connected": true,
                "dropped": 0,
                "clock_enabled": self.midi_config.clock_enabled,
                "pulses": self.midi_clock_count,
                "notes": self.midi_note_count,
                "ccs": self.midi_cc_count,
                "last_note": (self.midi_note_count > 0).then_some(self.last_note),
                "last_cc": (self.midi_cc_count > 0).then_some((self.last_cc, self.last_cc_val)),
                "mtc": null,
                "mtc_qf": 0,
                "mtc_full": 0,
            },
        })
        .to_string()
    }

    /// Builds the protocol v1 settings message. The indices match the host settings table.
    fn settings_json(&self) -> String {
        let c = &self.midi_config;
        let items: Vec<Value> = SETTINGS
            .iter()
            .map(|&(idx, label, section, kind)| {
                let raw = match idx {
                    3 => return json!({"idx": idx, "label": label, "section": section, "kind": kind,
                        "value": if c.clock_enabled { "✓" } else { "✗" }, "raw": null}),
                    5 => c.smoothing_ms.to_string(),
                    6 => c.latency_ms.to_string(),
                    8 => c.note_channel.to_string(),
                    9 => c.beat_note.to_string(),
                    10 => c.downbeat_note.to_string(),
                    12 => c.cc_channel.to_string(),
                    13 => c.bpm_coarse.to_string(),
                    14 => c.bpm_fine.to_string(),
                    15 => c.pitch_cc.to_string(),
                    16 => c.bar_phase_cc.to_string(),
                    17 => c.beat_phase_cc.to_string(),
                    _ => c.playing_cc.to_string(),
                };
                let value = match idx {
                    5 | 6 => format!("{raw} ms"),
                    _ => raw.clone(),
                };
                json!({"idx": idx, "label": label, "section": section, "kind": kind, "value": value, "raw": raw})
            })
            .collect();
        json!({ "type": "settings", "items": items }).to_string()
    }

    /// Stores a numeric setting with the same ranges as the host. Returns false when the value
    /// is not valid.
    fn set_setting(&mut self, idx: usize, value: &str) -> bool {
        let c = &mut self.midi_config;
        let note = |v: &str| v.parse::<u8>().ok().filter(|v| *v <= 127);
        let channel = |v: &str| v.parse::<u8>().ok().filter(|v| (1..=16).contains(v));
        let ok = match idx {
            5 => value.parse::<u64>().ok().filter(|v| *v <= 1000).map(|v| c.smoothing_ms = v),
            6 => value.parse::<i64>().ok().filter(|v| (-1000..=1000).contains(v)).map(|v| c.latency_ms = v),
            8 => channel(value).map(|v| c.note_channel = v),
            9 => note(value).map(|v| c.beat_note = v),
            10 => note(value).map(|v| c.downbeat_note = v),
            12 => channel(value).map(|v| c.cc_channel = v),
            13 => note(value).map(|v| c.bpm_coarse = v),
            14 => note(value).map(|v| c.bpm_fine = v),
            15 => note(value).map(|v| c.pitch_cc = v),
            16 => note(value).map(|v| c.bar_phase_cc = v),
            17 => note(value).map(|v| c.beat_phase_cc = v),
            18 => note(value).map(|v| c.playing_cc = v),
            _ => None,
        }
        .is_some();
        if ok {
            self.add_log(0, "INFO", format!("Setting {idx} changed to {value}"));
        }
        ok
    }

    /// Steps a toggle or a fixed-step setting. Returns false for other settings.
    fn cycle_setting(&mut self, idx: usize, dir: i8) -> bool {
        match idx {
            3 => self.midi_config.clock_enabled = !self.midi_config.clock_enabled,
            6 => {
                let step = if dir < 0 { -5 } else { 5 };
                self.midi_config.latency_ms = (self.midi_config.latency_ms + step).clamp(-1000, 1000);
            }
            _ => return false,
        }
        self.add_log(0, "INFO", format!("Setting {idx} changed"));
        true
    }

    /// Returns the log lines with a sequence number of `seq` or more, and the next number.
    fn logs_since(&self, seq: u64) -> (Vec<String>, u64) {
        let first = self.log_total - self.logs.len() as u64;
        let skip = seq.saturating_sub(first) as usize;
        (
            self.logs.iter().skip(skip).map(|l| l.message.clone()).collect(),
            self.log_total,
        )
    }
}

pub async fn run_emulator() -> Result<()> {
    println!("╔══════════════════════════════════════════╗");
    println!("║     xdj-clock ESP32 Emulator v1.1.2     ║");
    println!("╚══════════════════════════════════════════╝");

    fn create_reuse_socket(port: u16, broadcast: bool) -> Result<tokio::net::UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::net::{Ipv4Addr, SocketAddrV4};

        let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        raw.set_reuse_address(true)?;
        raw.set_nonblocking(true)?;
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        raw.bind(&bind_addr.into())?;
        let std_sock = std::net::UdpSocket::from(raw);
        if broadcast {
            std_sock.set_broadcast(true)?;
        }
        let tokio_sock = tokio::net::UdpSocket::from_std(std_sock)?;
        Ok(tokio_sock)
    }

    let state = Arc::new(RwLock::new(EmulatorState::new()));
    // (device number, beat in bar) for each beat packet.
    let (beat_tx, _) = broadcast::channel::<(u8, u8)>(64);

    let state_net = Arc::clone(&state);
    let sock_50000 = create_reuse_socket(50000, true).unwrap();
    let sock_50000 = Arc::new(sock_50000);
    let sock_50000_recv = Arc::clone(&sock_50000);
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            if let Ok((len, addr)) = sock_50000_recv.recv_from(&mut buf).await {
                state_net.write().await.handle_packet(addr, 50000, &buf[..len]);
            }
        }
    });

    let state_net2 = Arc::clone(&state);
    let beat_tx_net = beat_tx.clone();
    tokio::spawn(async move {
        let sock = create_reuse_socket(50001, false).unwrap();
        let mut buf = [0u8; 4096];
        loop {
            if let Ok((len, addr)) = sock.recv_from(&mut buf).await {
                let data = &buf[..len];
                state_net2.write().await.handle_packet(addr, 50001, data);
                if data.len() > 10 && data[10] == xdj_core_prolink::PKT_BEAT {
                    if let Some(beat) = xdj_core_prolink::parse::parse_beat(data) {
                        let _ = beat_tx_net.send((beat.device_number, beat.beat_in_bar));
                    }
                }
            }
        }
    });

    let state_net3 = Arc::clone(&state);
    tokio::spawn(async move {
        let sock = create_reuse_socket(50002, false).unwrap();
        let mut buf = [0u8; 4096];
        loop {
            if let Ok((len, addr)) = sock.recv_from(&mut buf).await {
                state_net3.write().await.handle_packet(addr, 50002, &buf[..len]);
            }
        }
    });

    // Participation loop: keep-alive broadcasts and status unicasts
    let state_part = Arc::clone(&state);
    let sock_part = Arc::clone(&sock_50000);
    tokio::spawn(async move {
        use xdj_core_prolink::build;
        let mut interval = interval(Duration::from_millis(1500));
        let mac = [0x02, 0xAB, 0xCD, 0xEF, 0x01, 0x02];
        let name = build::pad_name("xdj-clock-emu");
        
        loop {
            interval.tick().await;
            let s = state_part.read().await;
            let dev_num = s.virtual_device_num;
            
            // 1. Broadcast keep-alive
            // We use 10.10.10.50 as our IP in the packet for the emulator
            let my_ip = [10, 10, 10, 50];
            let ka_pkt = build::build_keepalive(&name, dev_num, &mac, &my_ip, 1);
            if let Err(e) = sock_part.send_to(&ka_pkt, "255.255.255.255:50000").await {
                eprintln!("Failed to send keep-alive: {}", e);
            }
            
            // 2. Unicast status template to all discovered decks on port 50002
            let status_pkt = build::build_status_packet(&name, dev_num);
            for deck in s.decks.iter().filter(|d| d.device_number != 0 && d.ip.is_some()) {
                if let Some(ip) = deck.ip {
                    let addr = SocketAddr::from((ip, 50002));
                    if let Err(e) = sock_part.send_to(&status_pkt, addr).await {
                        eprintln!("Failed to send status template to {}: {}", addr, e);
                    }
                }
            }
        }
    });

    let (status_tx, _) = watch::channel(String::new());
    let (settings_tx, _) = watch::channel(state.read().await.settings_json());
    let status_tx = Arc::new(status_tx);
    let settings_tx = Arc::new(settings_tx);

    let state_clone = Arc::clone(&state);
    let (status_pub, settings_pub) = (Arc::clone(&status_tx), Arc::clone(&settings_tx));
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(50));
        loop {
            ticker.tick().await;
            let mut s = state_clone.write().await;
            s.tick();
            if s.tick % 2 == 0 {
                status_pub.send_replace(s.status_json());
                let settings = s.settings_json();
                settings_pub.send_if_modified(|cur| {
                    let changed = *cur != settings;
                    if changed {
                        *cur = settings;
                    }
                    changed
                });
            }
        }
    });

    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/index.html", get(|| async { Html(PAGE) }))
        .route("/ws", get(ws_handler))
        .with_state(Shared {
            state,
            status_tx,
            settings_tx,
            beat_tx,
        });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("Web UI listening on http://0.0.0.0:8080");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Clone)]
struct Shared {
    state: Arc<RwLock<EmulatorState>>,
    status_tx: Arc<watch::Sender<String>>,
    settings_tx: Arc<watch::Sender<String>>,
    beat_tx: broadcast::Sender<(u8, u8)>,
}

/// Returns true when the request may open the socket. See `origin_allowed` in the host web module.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(|h| h.to_str()),
    ) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, rest)| rest.trim_end_matches('/').eq_ignore_ascii_case(host))
}

async fn ws_handler(ws: WebSocketUpgrade, headers: HeaderMap, State(sh): State<Shared>) -> Response {
    if !origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| client(socket, sh))
}

async fn send(socket: &mut WebSocket, msg: String) -> bool {
    socket.send(Message::Text(msg.into())).await.is_ok()
}

async fn client(mut socket: WebSocket, sh: Shared) {
    let hello = json!({
        "type": "hello",
        "proto": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "device": "emulator",
        "caps": ["beat", "decks", "output", "settings", "logs"],
    });
    let mut status_rx = sh.status_tx.subscribe();
    let mut settings_rx = sh.settings_tx.subscribe();
    let mut beat_rx = sh.beat_tx.subscribe();
    let mut log_seq = 0;
    let settings = settings_rx.borrow_and_update().clone();
    if !send(&mut socket, hello.to_string()).await || !send(&mut socket, settings).await {
        return;
    }

    loop {
        let msg = tokio::select! {
            r = status_rx.changed() => {
                if r.is_err() {
                    return;
                }
                let (lines, next) = sh.state.read().await.logs_since(log_seq);
                log_seq = next;
                if !lines.is_empty() && !send(&mut socket, json!({"type": "log", "lines": lines}).to_string()).await {
                    return;
                }
                status_rx.borrow_and_update().clone()
            }
            r = settings_rx.changed() => {
                if r.is_err() {
                    return;
                }
                settings_rx.borrow_and_update().clone()
            }
            beat = beat_rx.recv() => match beat {
                Ok((device, beat)) => json!({"type": "beat", "device": device, "beat": beat}).to_string(),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => match handle_command(&sh, text.as_str()).await {
                    Some(err) => json!({"type": "error", "text": err}).to_string(),
                    None => continue,
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
        };
        if !send(&mut socket, msg).await {
            return;
        }
    }
}

/// Runs one client command. Returns an error text for the page, or `None` on success.
async fn handle_command(sh: &Shared, text: &str) -> Option<String> {
    let cmd: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Some(format!("Unknown command: {e}")),
    };
    let idx = cmd["idx"].as_u64().unwrap_or(u64::MAX) as usize;
    let mut s = sh.state.write().await;
    match cmd["type"].as_str() {
        Some("cycle") => {
            let dir = if cmd["dir"].as_i64().unwrap_or(1) < 0 { -1 } else { 1 };
            s.cycle_setting(idx, dir);
            None
        }
        Some("set") => {
            let value = cmd["value"].as_str().unwrap_or("").trim().to_string();
            if s.set_setting(idx, &value) {
                None
            } else {
                Some(format!("{value:?} is not a valid value"))
            }
        }
        Some("rescan") => None,
        _ => Some("The emulator does not support this command".to_string()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    run_emulator().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_apply_the_host_ranges() {
        let mut s = EmulatorState::new();
        assert!(s.set_setting(5, "20"));
        assert_eq!(s.midi_config.smoothing_ms, 20);
        assert!(!s.set_setting(5, "5000"));
        assert!(!s.set_setting(8, "0"));
        assert!(!s.set_setting(99, "1"));
        let before = s.midi_config.clock_enabled;
        assert!(s.cycle_setting(3, 1));
        assert_ne!(s.midi_config.clock_enabled, before);
        let v: Value = serde_json::from_str(&s.settings_json()).unwrap();
        assert_eq!(v["items"][1]["raw"], "20");
    }

    #[test]
    fn logs_since_returns_only_new_lines_in_the_host_format() {
        let s = EmulatorState::new();
        let (lines, next) = s.logs_since(0);
        assert_eq!(next, lines.len() as u64);
        // The page reads the level from the second word.
        assert_eq!(lines[0].split_whitespace().nth(1), Some("INFO"));
        assert!(s.logs_since(next).0.is_empty());
    }

    #[test]
    fn status_is_valid_protocol_json_without_decks() {
        let v: Value = serde_json::from_str(&EmulatorState::new().status_json()).unwrap();
        assert_eq!(v["type"], "status");
        assert_eq!(v["no_devices"], true);
        assert_eq!(v["master"]["phrase16"], 0);
    }
}
