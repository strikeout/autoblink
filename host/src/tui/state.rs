//! TUI-specific state — UI selections, log ring buffer, activity counters.
//!
//! Separate from `DjState` because this tracks UI concerns (selected port,
//! scroll offsets, animation timers) that the audio/MIDI pipeline should
//! not know about.


use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::logbuf::LogBuffer;
use crate::midi::activity::MidiActivity;
use crate::settings::{NetworkIfaceInfo, MIDI_SETTINGS_START, SETTINGS};

// ── MIDI port info ───────────────────────────────────────────────────────────

/// A snapshot of an available MIDI output port.
#[derive(Debug, Clone)]
pub struct MidiPortInfo {
    /// Display name (from midir).
    pub name: String,
    /// Port index in the midir port list at the time of enumeration.
    pub index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePanel {
    MidiPorts,
    /// Network/device settings (left panel, indices 0..MIDI_SETTINGS_START).
    InputSettings,
    /// MIDI settings (right panel, indices MIDI_SETTINGS_START..).
    MidiSettings,
}

// ── TUI state ────────────────────────────────────────────────────────────────

/// UI state that the render loop owns.  Not shared across threads — only the
/// TUI task reads/writes this.
pub struct TuiState {
    /// Available MIDI output ports (refreshed on startup / manual rescan).
    pub midi_ports: Vec<MidiPortInfo>,
    /// Index into `midi_ports` for the currently active port.
    pub active_port_idx: usize,
    /// Index into `midi_ports` for the cursor (arrow-key highlight).
    pub cursor_port_idx: usize,
    /// Which lower-left panel is active.
    pub active_panel: ActivePanel,
    /// Cursor into the editable settings list.
    pub settings_cursor: usize,
    /// Cached network interfaces for the settings UI.
    pub interfaces: Vec<NetworkIfaceInfo>,
    /// Whether the settings panel is editing a numeric value.
    pub editing: bool,
    /// Temporary numeric edit buffer.
    pub edit_buffer: String,
    /// Log ring buffer (shared with tracing layer).
    pub log_buf: LogBuffer,
    /// MIDI activity counters (shared with MIDI tasks).
    pub midi_activity: Arc<Mutex<MidiActivity>>,
    /// Instant of the most recent beat (for flash animation).
    pub last_beat_flash: Option<Instant>,
    /// Whether we should quit the TUI.
    pub should_quit: bool,
}

impl TuiState {
    pub fn new(log_buf: LogBuffer, midi_activity: Arc<Mutex<MidiActivity>>) -> Self {
        Self {
            midi_ports: Vec::new(),
            active_port_idx: 0,
            cursor_port_idx: 0,
            active_panel: ActivePanel::MidiPorts,
            settings_cursor: 0,
            interfaces: Vec::new(),
            editing: false,
            edit_buffer: String::new(),
            log_buf,
            midi_activity,
            last_beat_flash: None,
            should_quit: false,
        }
    }

    /// Refresh the MIDI port list from midir.  Returns the number of ports found.
    pub fn refresh_midi_ports(&mut self) -> usize {
        let active_name = self
            .midi_ports
            .get(self.active_port_idx)
            .map(|port| port.name.clone());
        let cursor_name = self
            .midi_ports
            .get(self.cursor_port_idx)
            .map(|port| port.name.clone());

        self.midi_ports.clear();
        if let Ok(midi_out) = midir::MidiOutput::new("xdj-clock-tui") {
            let ports = midi_out.ports();
            for (i, port) in ports.iter().enumerate() {
                if let Ok(name) = midi_out.port_name(port) {
                    self.midi_ports.push(MidiPortInfo { name, index: i });
                }
            }
        }

        if let Some(active_name) = active_name {
            if let Some(idx) = self
                .midi_ports
                .iter()
                .position(|port| port.name == active_name)
            {
                self.active_port_idx = idx;
            } else if self.active_port_idx >= self.midi_ports.len() {
                self.active_port_idx = self.midi_ports.len().saturating_sub(1);
            }
        } else if self.active_port_idx >= self.midi_ports.len() {
            self.active_port_idx = self.midi_ports.len().saturating_sub(1);
        }

        if let Some(cursor_name) = cursor_name {
            if let Some(idx) = self
                .midi_ports
                .iter()
                .position(|port| port.name == cursor_name)
            {
                self.cursor_port_idx = idx;
            } else if self.cursor_port_idx >= self.midi_ports.len() {
                self.cursor_port_idx = self.midi_ports.len().saturating_sub(1);
            }
        } else if self.cursor_port_idx >= self.midi_ports.len() {
            self.cursor_port_idx = self.midi_ports.len().saturating_sub(1);
        }

        self.midi_ports.len()
    }

    pub fn refresh_interfaces(&mut self) {
        self.interfaces = crate::settings::list_interfaces();
    }

    #[allow(dead_code)]
    pub fn settings_count() -> usize {
        SETTINGS.len()
    }

    /// Move the port selector cursor up.
    pub fn cursor_up(&mut self) {
        if !self.midi_ports.is_empty() && self.cursor_port_idx > 0 {
            self.cursor_port_idx -= 1;
        }
    }

    /// Move the port selector cursor down.
    pub fn cursor_down(&mut self) {
        if !self.midi_ports.is_empty() && self.cursor_port_idx < self.midi_ports.len() - 1 {
            self.cursor_port_idx += 1;
        }
    }

    pub fn cursor_up_settings(&mut self) {
        let min = self.settings_panel_min();
        if self.settings_cursor > min {
            self.settings_cursor -= 1;
        }
    }

    pub fn cursor_down_settings(&mut self) {
        let max = self.settings_panel_max();
        if self.settings_cursor + 1 < max {
            self.settings_cursor += 1;
        }
    }

    /// Minimum SETTINGS index for the currently active settings panel.
    fn settings_panel_min(&self) -> usize {
        match self.active_panel {
            ActivePanel::InputSettings => 0,
            ActivePanel::MidiSettings => MIDI_SETTINGS_START,
            _ => 0,
        }
    }

    /// Exclusive upper bound of SETTINGS indices for the active panel.
    fn settings_panel_max(&self) -> usize {
        match self.active_panel {
            ActivePanel::InputSettings => MIDI_SETTINGS_START,
            ActivePanel::MidiSettings => SETTINGS.len(),
            _ => SETTINGS.len(),
        }
    }

    /// Returns true if the beat flash should be visible (< 100ms since last beat).
    pub fn beat_flash_active(&self) -> bool {
        self.last_beat_flash
            .map(|t| t.elapsed().as_millis() < 100)
            .unwrap_or(false)
    }
}
