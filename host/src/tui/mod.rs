//! TUI module — terminal user interface for xdj-clock.
//!
//! Renders a real-time dashboard showing input devices, BPM/phase info,
//! MIDI output status, and a log panel.  Provides interactive MIDI port
//! selection.

pub mod render;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use midir::{MidiOutput, MidiOutputConnection};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_stream::StreamExt;

use crate::config::SharedConfig;
use crate::midi::{MidiOutConnection, MidiOutHandle, MidirOutConnection};
use crate::prolink::discovery::DeviceTable;
use crate::state::SharedState;
use crate::logbuf::LogBuffer;
use crate::midi::activity::MidiActivity;
use crate::settings::{numeric_edit_value, setting_kind, SettingKind, MIDI_SETTINGS_START};
use state::{ActivePanel, TuiState};

/// Run the TUI.  This replaces the status-display + ctrl-c loop in main.
///
/// Returns when the user presses `q` or Ctrl+C.
pub async fn run(
    dj_state: SharedState,
    device_table: DeviceTable,
    cfg: SharedConfig,
    midi_out: MidiOutHandle,
    log_buf: LogBuffer,
    midi_activity: Arc<Mutex<MidiActivity>>,
    cfg_change_tx: watch::Sender<()>,
) -> anyhow::Result<()> {
    // ── TUI state ────────────────────────────────────────────────────────────
    let mut tui = TuiState::new(log_buf, midi_activity);
    refresh_connectivity(&mut tui);

    // Try to match the active port to the config name.
    let startup_cfg = cfg.read().clone();
    if let Some(idx) = tui.midi_ports.iter().position(|p| {
        p.name
            .to_lowercase()
            .contains(&startup_cfg.midi.output.to_lowercase())
    }) {
        tui.active_port_idx = idx;
        tui.cursor_port_idx = idx;
    }

    // ── Terminal setup ───────────────────────────────────────────────────────
    let mut terminal = ratatui::init();
    let mut result = Ok(());

    // ── Event stream (non-blocking keyboard via crossterm) ───────────────────
    let mut events = EventStream::new();

    // ── Render ticker (~30 fps) ──────────────────────────────────────────────
    let mut render_tick = tokio::time::interval(Duration::from_millis(33));
    render_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // ── Connectivity refresh ticker (~3 s) ───────────────────────────────────
    let mut refresh_tick = tokio::time::interval(Duration::from_secs(3));
    refresh_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // ── Beat flash updater: subscribe to dj_state changes ────────────────────
    // We just poll the shared state each frame — cheap with parking_lot reads.

    loop {
        tokio::select! {
            biased;

            // ── Keyboard / mouse events ──────────────────────────────────────
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match key.code {
                            KeyCode::Char('q') => {
                                tui.should_quit = true;
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                tui.should_quit = true;
                            }
                            KeyCode::Tab | KeyCode::BackTab if !tui.editing => {
                                let next_panel = match tui.active_panel {
                                    ActivePanel::InputSettings => ActivePanel::MidiSettings,
                                    ActivePanel::MidiSettings => ActivePanel::MidiPorts,
                                    ActivePanel::MidiPorts => ActivePanel::InputSettings,
                                };
                                focus_panel(&mut tui, next_panel);
                            }
                            KeyCode::Up | KeyCode::Char('k') if tui.active_panel == ActivePanel::MidiPorts => {
                                tui.cursor_up();
                            }
                            KeyCode::Down | KeyCode::Char('j') if tui.active_panel == ActivePanel::MidiPorts => {
                                tui.cursor_down();
                            }
                            KeyCode::Enter if tui.active_panel == ActivePanel::MidiPorts => {
                                switch_midi_port(&mut tui, &midi_out, &cfg).await;
                            }
                            KeyCode::Char('r') => refresh_connectivity(&mut tui),
                            _ if matches!(tui.active_panel, ActivePanel::InputSettings | ActivePanel::MidiSettings) => {
                                handle_settings_key(&mut tui, &cfg, &cfg_change_tx, key.code);
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {} // mouse / resize — ignore
                    Some(Err(_)) => {} // crossterm error — ignore
                    None => break,     // stream closed
                }
            }

            // ── Render tick ──────────────────────────────────────────────────
            _ = render_tick.tick() => {
                // Update beat flash from shared state.
                let last_beat = dj_state.read().master.last_beat_at;
                tui.last_beat_flash = last_beat;

                if let Err(e) = terminal.draw(|f| {
                    render::draw(f, &tui, &dj_state, &device_table, &cfg);
                }) {
                    // Leave the loop so that the teardown below still runs.
                    result = Err(e.into());
                    break;
                }
            }

            // ── Periodic connectivity refresh ───────────────────────────────
            _ = refresh_tick.tick() => {
                refresh_connectivity(&mut tui);
            }
        }

        if tui.should_quit {
            break;
        }
    }

    // ── Terminal teardown ────────────────────────────────────────────────────
    ratatui::restore();

    // Send MIDI Stop so external gear doesn't get orphaned clocks.
    midi_out.stop().await;

    result
}

fn handle_settings_key(
    tui: &mut TuiState,
    cfg: &SharedConfig,
    cfg_change_tx: &watch::Sender<()>,
    code: KeyCode,
) {
    if tui.editing {
        match code {
            KeyCode::Esc => {
                tui.editing = false;
                tui.edit_buffer.clear();
            }
            KeyCode::Backspace => {
                tui.edit_buffer.pop();
            }
            KeyCode::Enter => {
                crate::settings::set_numeric(cfg, cfg_change_tx, tui.settings_cursor, &tui.edit_buffer);
                tui.editing = false;
                tui.edit_buffer.clear();
            }
            KeyCode::Char(c) if c.is_ascii_digit() || (c == '-' && tui.edit_buffer.is_empty()) => {
                tui.edit_buffer.push(c);
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::Up | KeyCode::Char('k') => tui.cursor_up_settings(),
        KeyCode::Down | KeyCode::Char('j') => tui.cursor_down_settings(),
        KeyCode::Left | KeyCode::Char('h') => cycle_setting(tui, cfg, cfg_change_tx, -1),
        KeyCode::Right | KeyCode::Char('l') => cycle_setting(tui, cfg, cfg_change_tx, 1),
        KeyCode::Enter => activate_setting(tui, cfg, cfg_change_tx),
        _ => {}
    }
}

fn focus_panel(tui: &mut TuiState, panel: ActivePanel) {
    tui.editing = false;
    tui.edit_buffer.clear();
    tui.active_panel = panel;
    match panel {
        ActivePanel::InputSettings => tui.settings_cursor = 0,
        ActivePanel::MidiSettings => tui.settings_cursor = MIDI_SETTINGS_START,
        ActivePanel::MidiPorts => {}
    }
}

fn refresh_connectivity(tui: &mut TuiState) {
    tui.refresh_midi_ports();
    tui.refresh_interfaces();
}

fn cycle_setting(
    tui: &mut TuiState,
    cfg: &SharedConfig,
    cfg_change_tx: &watch::Sender<()>,
    direction: i8,
) {
    crate::settings::cycle(cfg, &tui.interfaces, cfg_change_tx, tui.settings_cursor, direction);
}

fn activate_setting(tui: &mut TuiState, cfg: &SharedConfig, cfg_change_tx: &watch::Sender<()>) {
    match setting_kind(tui.settings_cursor) {
        SettingKind::Toggle | SettingKind::CycleInterface | SettingKind::CycleSource => {
            cycle_setting(tui, cfg, cfg_change_tx, 1);
        }
        SettingKind::NumericU8 | SettingKind::NumericU64 | SettingKind::NumericI64 => {
            let guard = cfg.read();
            if let Some(value) = numeric_edit_value(&guard, tui.settings_cursor) {
                tui.editing = true;
                tui.edit_buffer = value;
            }
        }
    }
}

/// Switch the active MIDI port to the one under the cursor.
async fn switch_midi_port(tui: &mut TuiState, midi_out: &MidiOutHandle, cfg: &SharedConfig) {
    let target_idx = tui.cursor_port_idx;
    if target_idx == tui.active_port_idx {
        return; // already selected
    }

    let Some(port_info) = tui.midi_ports.get(target_idx) else {
        return;
    };

    // Open a new connection.
    let new_conn: Box<dyn MidiOutConnection> = match open_midi_by_index(port_info.index) {
        Ok(conn) => Box::new(MidirOutConnection(conn)),
        Err(e) => {
            tracing::error!("Failed to open MIDI port {}: {e}", port_info.name);
            return;
        }
    };

    // Swap (worker-owned): send Stop on old connection before dropping.
    midi_out.switch_connection(Some(new_conn), true).await;

    tui.active_port_idx = target_idx;
    cfg.write().midi.output = port_info.name.clone();
    tracing::info!("Switched MIDI output to: {}", port_info.name);
}

/// Open a MIDI output connection by port index.
fn open_midi_by_index(index: usize) -> anyhow::Result<MidiOutputConnection> {
    let midi_out = MidiOutput::new("xdj-clock")?;
    let ports = midi_out.ports();
    let port = ports
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("MIDI port index {index} no longer valid"))?;
    midi_out
        .connect(port, "xdj-clock")
        .map_err(|e| anyhow::anyhow!("{}", e))
}
