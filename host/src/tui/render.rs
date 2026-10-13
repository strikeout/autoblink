//! TUI rendering — draws every frame using ratatui widgets.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph};
use ratatui::Frame;

use super::state::{
    get_value, selected_interface_name_ip, ActivePanel, SettingKind, TuiState, MIDI_SETTINGS_START,
    SETTINGS,
};
use crate::config::SharedConfig;
use crate::prolink::discovery::DeviceTable;
use crate::state::SharedState;

const CLR_ACCENT: Color = Color::Cyan;
const CLR_BEAT_FLASH: Color = Color::Yellow;
const CLR_PLAYING: Color = Color::Green;
const CLR_STOPPED: Color = Color::Red;
const CLR_DIM: Color = Color::DarkGray;
const CLR_HIGHLIGHT: Color = Color::White;

/// Compute the first visible row index so that `cursor` stays on screen.
fn scroll_offset(cursor: usize, visible: usize, total: usize) -> usize {
    if total <= visible {
        return 0;
    }
    if cursor >= visible {
        (cursor + 1).saturating_sub(visible)
    } else {
        0
    }
}

// ── Top-level layout ─────────────────────────────────────────────────────────
//
//  ┌─────────────────────── Header (4 rows) ───────────────────────────┐
//  │ overview + BPM + beat animation + bar gauge                       │
//  ├──────────── Left 50% ──────────┬──────────── Right 50% ──────────┤
//  │  Deck Information (top-left)   │  Output Status (top-right)      │
//  │  min 12 rows                   │  8 rows                         │
//  ├────────────────────────────────┼────────────────┬────────────────┤
//  │  Input Settings (bot-left)     │ MIDI Settings  │ Output Port    │
//  │  7 rows (network/device)       │ 60% width      │ 40% width      │
//  │                                │ min 10 rows    │                 │
//  ├─────────────────────── Footer (8 rows) ───────────────────────────┤
//  │ Logs                                                              │
//  └───────────────────────────────────────────────────────────────────┘

pub fn draw(
    f: &mut Frame,
    tui: &TuiState,
    dj_state: &SharedState,
    device_table: &DeviceTable,
    cfg: &SharedConfig,
) {
    let area = f.area();

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Min(20),   // body
            Constraint::Length(8), // footer / logs
        ])
        .split(area);

    draw_header(f, main_chunks[0], dj_state, cfg, tui);
    draw_body(f, main_chunks[1], tui, dj_state, device_table, cfg);
    draw_log_panel(f, main_chunks[2], tui);
}

// ── Header ───────────────────────────────────────────────────────────────────

fn draw_header(
    f: &mut Frame,
    area: Rect,
    dj_state: &SharedState,
    cfg: &SharedConfig,
    tui: &TuiState,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLR_DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 1 {
        return;
    }

    let st = dj_state.read();
    let cfg_r = cfg.read();
    let master = &st.master;
    let version = env!("CARGO_PKG_VERSION");

    let flash = tui.beat_flash_active();
    let bpm_color = if flash { CLR_BEAT_FLASH } else { CLR_ACCENT };
    let play_color = if master.is_playing {
        CLR_PLAYING
    } else {
        CLR_STOPPED
    };

    let source_str = match &master.source {
        Some(crate::state::BeatSource::ProLink) => "ProLink (network)",
        Some(crate::state::BeatSource::AbletonLink) => "AbletonLink",
        None => "—",
    };

    let state_text = if master.is_playing {
        "▶ PLAY"
    } else {
        "■ STOP"
    };

    let bpm_text = if master.bpm > 0.0 {
        format!("{:.2}", master.bpm)
    } else {
        "---.-".to_string()
    };

    let beat = master.beat_in_bar;
    let beat_blocks: String = (1..=4)
        .map(|i| if i == beat { "▮" } else { "▯" })
        .collect::<Vec<_>>()
        .join(" ");

    let pitch_str = if master.pitch_pct.abs() < 0.005 {
        "±0.00%".to_string()
    } else {
        format!("{:+.2}%", master.pitch_pct)
    };

    // Row 1: title + source + mode + interface + state
    let row1 = Line::from(vec![
        Span::styled(
            format!(" xdj-clock v{} ", version),
            Style::default().fg(CLR_ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ "),
        Span::styled(source_str, Style::default().fg(CLR_HIGHLIGHT)),
        Span::raw(" │ mode: "),
        Span::styled(
            format!("{:?}", cfg_r.source),
            Style::default().fg(CLR_HIGHLIGHT),
        ),
        Span::raw(" │ iface: "),
        Span::styled(
            selected_interface_name_ip(&cfg_r, &tui.interfaces),
            Style::default().fg(CLR_HIGHLIGHT),
        ),
        Span::raw(" │ "),
        Span::styled(
            state_text,
            Style::default().fg(play_color).add_modifier(Modifier::BOLD),
        ),
    ]);

    f.render_widget(
        Paragraph::new(row1),
        Rect {
            y: inner.y,
            height: 1,
            ..inner
        },
    );

    // Row 2: BPM + beat blocks + pitch + bar gauge
    if inner.height >= 2 {
        let gauge_width = inner.width.saturating_sub(40);
        let left_area = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width.min(40),
            height: 1,
        };
        let row2 = Line::from(vec![
            Span::raw(" bpm "),
            Span::styled(
                &bpm_text,
                Style::default().fg(bpm_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  beat "),
            Span::styled(&beat_blocks, Style::default().fg(bpm_color)),
            Span::raw("  pitch "),
            Span::styled(pitch_str, Style::default().fg(CLR_HIGHLIGHT)),
        ]);
        f.render_widget(Paragraph::new(row2), left_area);

        if gauge_width > 8 {
            let gauge_area = Rect {
                x: inner.x + 40,
                y: inner.y + 1,
                width: gauge_width,
                height: 1,
            };
            let a = tui.midi_activity.lock();

            let in16 = if master.phrase_16_beat > 0 {
                master.phrase_16_beat
            } else {
                0
            };
            let in16 = if in16 == 0 { 0 } else { in16.min(16) };

            let out16 = a.clock_phrase_beat.min(16);
            let stable = cfg_r.midi.phrase_lock_stable_beats;
            let waiting = a.clock_waiting_for_phrase;
            let wait_seen = a.clock_wait_beats_seen;
            let diff = if let Some(delta_ms) = a.clock_timing_delta_ms {
                format!("{:.1}ms", delta_ms.abs())
            } else {
                "—".to_string()
            };

            let status = if waiting {
                format!("wait {}/{}", wait_seen, stable)
            } else if a.clock_running {
                "lock".to_string()
            } else {
                "idle".to_string()
            };

            let label = format!(
                "in {:02} b{}  out {:02}  Δ{}  {}",
                in16, master.beat_in_bar, out16, diff, status
            );

            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default().fg(CLR_ACCENT),
                ))),
                gauge_area,
            );
        }
    }
}

// ── Body ─────────────────────────────────────────────────────────────────────

fn draw_body(
    f: &mut Frame,
    area: Rect,
    tui: &TuiState,
    dj_state: &SharedState,
    device_table: &DeviceTable,
    cfg: &SharedConfig,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Left column: deck info (top) + input/network config (bottom)
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(7)])
        .split(cols[0]);

    draw_deck_panel(f, left[0], dj_state, device_table, cfg);
    draw_input_config_panel(f, left[1], tui, cfg);

    // Right column: output status (top) + [MIDI config | output port] (bottom row)
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // output status
            Constraint::Min(10),   // MIDI config + output port side by side
        ])
        .split(cols[1]);

    draw_output_status_panel(f, right[0], tui, cfg, dj_state);

    // Bottom-right: MIDI Settings (left) + Output Port (right), side by side
    let right_bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(right[1]);

    draw_midi_config_panel(f, right_bottom[0], tui, cfg);
    draw_output_config_panel(f, right_bottom[1], tui);
}

// ── Deck Information (top-left) ──────────────────────────────────────────────

fn draw_deck_panel(
    f: &mut Frame,
    area: Rect,
    dj_state: &SharedState,
    device_table: &DeviceTable,
    cfg: &SharedConfig,
) {
    let block = Block::default()
        .title(" Deck Information ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLR_DIM));

    let devices = device_table.lock();
    let st = dj_state.read();
    let our_device_number = cfg.read().device_number;

    let mut lines: Vec<Line> = Vec::new();
    let mut phrase_gauges: Vec<(usize, f64, String)> = Vec::new();

    if st.link_peer_count > 0 {
        lines.push(Line::from(vec![
            Span::raw("  Ableton Link: "),
            Span::styled(
                format!("{} peer(s) connected", st.link_peer_count),
                Style::default()
                    .fg(CLR_PLAYING)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
    }

    if devices.is_empty() && st.master.source != Some(crate::state::BeatSource::AbletonLink) {
        lines.push(Line::from(Span::styled(
            "  No devices on network",
            Style::default().fg(CLR_DIM),
        )));
    } else {
        let mut sorted_devs: Vec<_> = devices
            .iter()
            .filter(|(_, dev)| dev.device_number != our_device_number)
            .collect();
        sorted_devs.sort_by_key(|(_, dev)| dev.device_number);

        for (_, dev) in &sorted_devs {
            let dev_state = st.devices.get(&dev.device_number);
            let playing = dev_state.map(|d| d.is_playing).unwrap_or(false);
            let is_master = dev_state.map(|d| d.is_master).unwrap_or(false);
            let is_virtual_master = !is_master
                && st.master.is_virtual_master
                && st.master.device_number == dev.device_number;
            let is_on_air = dev_state.map(|d| d.is_on_air).unwrap_or(false);
            let is_sync = dev_state.map(|d| d.is_sync).unwrap_or(false);
            let status_icon = if playing { "▶" } else { "■" };
            let master_tag = if is_master {
                " ★MASTER"
            } else if is_virtual_master {
                " vM"
            } else {
                ""
            };
            let color = if playing { CLR_PLAYING } else { CLR_DIM };

            // BPM and pitch for this deck.
            let deck_bpm = dev_state.map(|d| d.effective_bpm).filter(|b| *b > 0.0);
            let pitch_pct = dev_state.map(|d| d.pitch_pct).unwrap_or(0.0);

            // Row 1: status icon, device number, name, master tag, bpm
            let mut spans = vec![
                Span::styled(format!("  {} ", status_icon), Style::default().fg(color)),
                Span::styled(
                    format!("#{} {}", dev.device_number, dev.name),
                    Style::default().fg(CLR_HIGHLIGHT),
                ),
                Span::styled(
                    master_tag.to_string(),
                    Style::default()
                        .fg(if is_virtual_master {
                            CLR_ACCENT
                        } else {
                            CLR_BEAT_FLASH
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ];

            if let Some(bpm) = deck_bpm {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{:.1} bpm", bpm),
                    Style::default().fg(CLR_ACCENT),
                ));
                if pitch_pct.abs() > 0.005 {
                    spans.push(Span::styled(
                        format!(" ({:+.2}%)", pitch_pct),
                        Style::default().fg(CLR_DIM),
                    ));
                }
            }

            if let Some(ds) = dev_state {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    ds.play_state.to_string(),
                    Style::default().fg(CLR_DIM),
                ));
            }

            lines.push(Line::from(spans));

            // Row 2: flags + beat position
            if let Some(ds) = dev_state {
                let mut flags: Vec<Span> = vec![Span::raw("    ")];

                if is_on_air {
                    flags.push(Span::styled("ON-AIR ", Style::default().fg(CLR_PLAYING)));
                }
                if is_sync {
                    flags.push(Span::styled("SYNC ", Style::default().fg(CLR_ACCENT)));
                }

                // Beat indicator for this deck
                if ds.beat_in_bar >= 1 && ds.beat_in_bar <= 4 {
                    let beats: String = (1..=4)
                        .map(|i| if i == ds.beat_in_bar { "▮" } else { "▯" })
                        .collect::<Vec<_>>()
                        .join("");
                    flags.push(Span::styled(
                        format!("beat {} ", beats),
                        Style::default().fg(CLR_DIM),
                    ));
                }

                // Beat count
                if ds.beat_count != u32::MAX && ds.beat_count > 0 {
                    flags.push(Span::styled(
                        format!("#{}", ds.beat_count),
                        Style::default().fg(CLR_DIM),
                    ));
                }

                // Playhead (CDJ-3000 only)
                if let Some(ms) = ds.playhead_ms {
                    let secs = ms / 1000;
                    let mins = secs / 60;
                    let s = secs % 60;
                    flags.push(Span::styled(
                        format!(" {}:{:02}", mins, s),
                        Style::default().fg(CLR_DIM),
                    ));
                }

                if flags.len() > 1 {
                    lines.push(Line::from(flags));
                }

                // Row 3: track metadata: artist - title - key - bpm
                if !ds.track_title.is_empty() || !ds.track_artist.is_empty() {
                    let mut parts: Vec<String> = Vec::new();
                    if !ds.track_artist.is_empty() {
                        parts.push(ds.track_artist.clone());
                    }
                    if !ds.track_title.is_empty() {
                        parts.push(ds.track_title.clone());
                    }
                    if !ds.track_key.is_empty() {
                        parts.push(ds.track_key.clone());
                    }
                    if let Some(bpm) = ds.track_bpm_meta {
                        parts.push(format!("{:.0}bpm", bpm));
                    }
                    let track_info = parts.join(" - ");

                    let max_width = area.width.saturating_sub(8) as usize;
                    // Count chars, not bytes: a byte slice can split a UTF-8 character and panic.
                    let display = if track_info.chars().count() > max_width && max_width > 3 {
                        let head: String = track_info.chars().take(max_width - 1).collect();
                        format!("{head}…")
                    } else {
                        track_info
                    };
                    lines.push(Line::from(Span::styled(
                        format!("    ♫ {}", display),
                        Style::default().fg(CLR_ACCENT),
                    )));
                }

                // Row 4: source slot info
                if ds.rekordbox_id != 0 {
                    let slot_name = match ds.track_slot {
                        1 => "CD",
                        2 => "SD",
                        3 => "USB",
                        4 => "rekordbox",
                        _ => "?",
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    src: deck#{} {} (rb#{})",
                            ds.track_source_player, slot_name, ds.rekordbox_id
                        ),
                        Style::default().fg(CLR_DIM),
                    )));
                }

                // Phrase information: show current phrase as a gauge progress bar.
                if let (Some(ss), Some(idx)) = (&ds.song_structure, ds.current_phrase_idx) {
                    if let Some(phrase) = ss.phrases.get(idx) {
                        let next_beat = ss
                            .phrases
                            .get(idx + 1)
                            .map(|p| p.beat)
                            .unwrap_or(ss.end_beat);
                        let phrase_len = next_beat.saturating_sub(phrase.beat).max(1);
                        let beat_count = ds.beat_count as u16;
                        let beats_in = beat_count.saturating_sub(phrase.beat);
                        let progress = (beats_in as f64 / phrase_len as f64).clamp(0.0, 1.0);
                        let pct = (progress * 100.0) as u8;

                        let fill_tag = if phrase.has_fill && beat_count >= phrase.fill_beat {
                            " [FILL]"
                        } else {
                            ""
                        };
                        let label = format!("{} {}%{}", phrase.kind, pct, fill_tag);

                        phrase_gauges.push((lines.len(), progress, label));
                        lines.push(Line::from(""));
                    }
                } else if ds.rekordbox_id != 0 && ds.song_structure.is_none() {
                    lines.push(Line::from(Span::styled(
                        "    phrase info unavailable",
                        Style::default().fg(CLR_DIM),
                    )));
                }

                // Blank line separator between decks.
                lines.push(Line::from(""));
            }
        }
    }

    if st.master.source == Some(crate::state::BeatSource::AbletonLink) {
        lines.push(Line::from(Span::styled(
            "  ♪ AbletonLink active",
            Style::default().fg(CLR_PLAYING),
        )));
    }

    let inner = block.inner(area);
    let widget = Paragraph::new(lines).block(block);
    f.render_widget(widget, area);

    // Overlay Gauge widgets for phrase progress bars.
    for (line_idx, ratio, label) in phrase_gauges {
        let y = inner.y + line_idx as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let gauge_x = inner.x + 4;
        let gauge_w = inner.width.saturating_sub(4);
        if gauge_w < 8 {
            continue;
        }
        let gauge_area = Rect {
            x: gauge_x,
            y,
            width: gauge_w,
            height: 1,
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(CLR_ACCENT))
            .label(label)
            .ratio(ratio.clamp(0.0, 1.0));
        f.render_widget(gauge, gauge_area);
    }
}

// ── Input Config (bottom-left) ───────────────────────────────────────────────

fn draw_input_config_panel(f: &mut Frame, area: Rect, tui: &TuiState, cfg: &SharedConfig) {
    let border_color = if tui.active_panel == ActivePanel::InputSettings {
        CLR_ACCENT
    } else {
        CLR_DIM
    };
    let block = Block::default()
        .title(" Input Settings ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    draw_settings_inner(f, inner, tui, cfg, 0, MIDI_SETTINGS_START);
}

// ── MIDI Configuration (mid-right) ───────────────────────────────────────────

fn draw_midi_config_panel(f: &mut Frame, area: Rect, tui: &TuiState, cfg: &SharedConfig) {
    let border_color = if tui.active_panel == ActivePanel::MidiSettings {
        CLR_ACCENT
    } else {
        CLR_DIM
    };
    let block = Block::default()
        .title(" MIDI Settings ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    draw_settings_inner(f, inner, tui, cfg, MIDI_SETTINGS_START, SETTINGS.len());
}

fn draw_settings_inner(
    f: &mut Frame,
    area: Rect,
    tui: &TuiState,
    cfg: &SharedConfig,
    start_idx: usize,
    end_idx: usize,
) {
    let cfg = cfg.read().clone();
    let mut rows: Vec<ListItem> = Vec::new();
    let mut cursor_row: usize = 0;

    for (idx, setting) in SETTINGS
        .iter()
        .enumerate()
        .skip(start_idx)
        .take(end_idx - start_idx)
    {
        if let Some(section) = setting.section {
            rows.push(ListItem::new(Line::from(Span::styled(
                format!("───── {} ─────", section),
                Style::default().fg(CLR_DIM),
            ))));
        }

        if idx == tui.settings_cursor {
            cursor_row = rows.len();
        }

        let value = if tui.editing && tui.settings_cursor == idx {
            format!("{}▌", tui.edit_buffer)
        } else {
            get_value(&cfg, &tui.interfaces, idx)
        };
        let line = format!("  {}: {}", setting.label, value);
        let selected = tui.settings_cursor == idx;
        let style = if selected {
            Style::default()
                .fg(CLR_HIGHLIGHT)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(match setting.kind {
                SettingKind::CycleInterface | SettingKind::CycleSource | SettingKind::Toggle => {
                    CLR_ACCENT
                }
                SettingKind::NumericU8 | SettingKind::NumericU64 | SettingKind::NumericI64 => {
                    CLR_HIGHLIGHT
                }
            })
        };
        rows.push(ListItem::new(Line::from(Span::styled(line, style))));
    }

    let visible = area.height as usize;
    let total = rows.len();
    let offset = scroll_offset(cursor_row, visible, total);

    let visible_rows: Vec<ListItem> = rows.into_iter().skip(offset).take(visible).collect();
    f.render_widget(List::new(visible_rows), area);
}

// ── Output Status (top-right) ────────────────────────────────────────────────

fn draw_output_status_panel(
    f: &mut Frame,
    area: Rect,
    tui: &TuiState,
    cfg: &SharedConfig,
    dj_state: &SharedState,
) {
    let block = Block::default()
        .title(" Output Status ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLR_DIM));

    let activity = tui.midi_activity.lock();
    let cfg_r = cfg.read();
    let st = dj_state.read();

    let port_name = tui
        .midi_ports
        .get(tui.active_port_idx)
        .map(|p| p.name.as_str())
        .unwrap_or("—");

    let clock_status = if cfg_r.midi.clock_enabled {
        "✓ enabled"
    } else {
        "✗ disabled"
    };

    // Note animation: show last note for 200ms after firing.
    let note_anim = activity
        .last_note
        .as_ref()
        .filter(|(_, t)| t.elapsed().as_millis() < 200)
        .map(|(n, _)| format!("♪ note {}", n))
        .unwrap_or_default();

    // CC animation: show last CC for 300ms.
    let cc_anim = activity
        .last_cc
        .as_ref()
        .filter(|(_, _, t)| t.elapsed().as_millis() < 300)
        .map(|(num, val, _)| format!("↗ CC{}={}", num, val))
        .unwrap_or_default();

    let master_source = match st.master.source {
        Some(crate::state::BeatSource::ProLink) => {
            if st.master.is_virtual_master {
                format!("deck #{} (virtual)", st.master.device_number)
            } else {
                format!("deck #{}", st.master.device_number)
            }
        }
        Some(crate::state::BeatSource::AbletonLink) => "AbletonLink".to_string(),
        None => "—".to_string(),
    };

    let mut lines = vec![
        Line::from(vec![
            Span::raw("  port: "),
            Span::styled(port_name, Style::default().fg(CLR_ACCENT)),
            Span::raw("  source: "),
            Span::styled(&master_source, Style::default().fg(CLR_HIGHLIGHT)),
        ]),
        Line::from(vec![
            Span::raw("  clock: "),
            Span::styled(clock_status, Style::default().fg(CLR_HIGHLIGHT)),
            Span::raw(format!("  (pulses: {})", activity.clock_pulses)),
        ]),
        Line::from(vec![
            Span::raw("  notes: "),
            Span::styled(
                format!("{}", activity.notes_sent),
                Style::default().fg(CLR_HIGHLIGHT),
            ),
            Span::raw("  CCs: "),
            Span::styled(
                format!("{}", activity.cc_sent),
                Style::default().fg(CLR_HIGHLIGHT),
            ),
        ]),
    ];

    // Animation row: show note/CC activity.
    if !note_anim.is_empty() || !cc_anim.is_empty() {
        let mut spans = vec![Span::raw("  ")];
        if !note_anim.is_empty() {
            spans.push(Span::styled(
                &note_anim,
                Style::default()
                    .fg(CLR_BEAT_FLASH)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw("  "));
        }
        if !cc_anim.is_empty() {
            spans.push(Span::styled(&cc_anim, Style::default().fg(CLR_ACCENT)));
        }
        lines.push(Line::from(spans));
    } else {
        lines.push(Line::from(Span::styled(
            "  idle",
            Style::default().fg(CLR_DIM),
        )));
    }

    // MTC status row.
    let mtc_status = if cfg_r.midi.mtc.enabled {
        format!(
            "✓ {} (qf: {}, full: {})",
            cfg_r.midi.mtc.frame_rate.label(),
            activity.mtc_quarter_frames,
            activity.mtc_full_frames
        )
    } else {
        "✗ disabled".to_string()
    };
    lines.push(Line::from(vec![
        Span::raw("  MTC: "),
        Span::styled(mtc_status, Style::default().fg(CLR_HIGHLIGHT)),
    ]));

    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Output Port (bottom-right) ───────────────────────────────────────────────

fn draw_output_config_panel(f: &mut Frame, area: Rect, tui: &TuiState) {
    let border_color = if tui.active_panel == ActivePanel::MidiPorts {
        CLR_ACCENT
    } else {
        CLR_DIM
    };
    let block = Block::default()
        .title(" Output Port ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height > 0 {
        draw_midi_selector_inner(f, inner, tui);
    }
}

fn draw_midi_selector_inner(f: &mut Frame, area: Rect, tui: &TuiState) {
    if tui.midi_ports.is_empty() {
        f.render_widget(Paragraph::new("  No MIDI ports found"), area);
        return;
    }

    let visible = area.height as usize;
    let total = tui.midi_ports.len();
    let cursor = tui.cursor_port_idx;
    let offset = scroll_offset(cursor, visible, total);

    let items: Vec<ListItem> = tui
        .midi_ports
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, port)| {
            let is_active = i == tui.active_port_idx;
            let is_cursor = i == tui.cursor_port_idx;
            let marker = if is_active { " ● " } else { "   " };
            let style = if is_cursor {
                Style::default()
                    .fg(CLR_HIGHLIGHT)
                    .add_modifier(Modifier::REVERSED)
            } else if is_active {
                Style::default().fg(CLR_ACCENT)
            } else {
                Style::default().fg(CLR_DIM)
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(&port.name, style),
            ]))
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, area);
}

// ── Logs (footer) ────────────────────────────────────────────────────────────

fn draw_log_panel(f: &mut Frame, area: Rect, tui: &TuiState) {
    let block = Block::default()
        .title(" Logs ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CLR_DIM));

    let all_lines = tui.log_buf.lines();
    let visible = area.height.saturating_sub(2) as usize;
    let start = all_lines.len().saturating_sub(visible);
    let lines: Vec<Line> = all_lines[start..]
        .iter()
        .map(|s| {
            let color = if s.contains("ERROR") || s.contains("error") {
                Color::Red
            } else if s.contains("WARN") || s.contains("warn") {
                Color::Yellow
            } else if s.contains("TRACE") || s.contains("trace") {
                CLR_DIM
            } else if s.contains("DEBUG") || s.contains("debug") {
                CLR_DIM
            } else if s.contains("INFO") || s.contains("info") {
                Color::White
            } else {
                Color::White
            };
            Line::from(Span::styled(format!(" {s}"), Style::default().fg(color)))
        })
        .collect();

    f.render_widget(Paragraph::new(lines).block(block), area);
}
