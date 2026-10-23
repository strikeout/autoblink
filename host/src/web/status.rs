//! The status message: one snapshot of the app state for the web UI.
//!
//! [`snapshot`] takes each lock alone, copies the data, and releases the lock before it formats
//! anything. It never holds two locks at the same time. The MIDI clock writes the activity
//! counters with `try_lock`, so a long hold here would make the clock skip an update.

use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;

use crate::config::SharedConfig;
use crate::midi::activity::MidiActivity;
use crate::prolink::discovery::DeviceTable;
use crate::settings::{format_source_value, selected_interface_name_ip, NetworkIfaceInfo};
use crate::state::{BeatSource, MixerChannel, SharedState};

#[derive(Debug, Serialize)]
pub struct Status {
    /// The source of the current master, or `None` when there is no master.
    pub source: Option<&'static str>,
    /// The configured source mode.
    pub mode: &'static str,
    pub iface: String,
    pub master: Master,
    pub clock: Clock,
    pub decks: Vec<Deck>,
    /// True when no Pro DJ Link device is on the network and Ableton Link is not the source.
    pub no_devices: bool,
    pub link_peers: usize,
    pub link_active: bool,
    /// The XDJ-AZ mixer. `None` until a mixer control moves.
    pub mixer: Option<Vec<Mixer>>,
    pub output: Output,
}

#[derive(Debug, Serialize)]
pub struct Master {
    pub device: u8,
    pub virtual_master: bool,
    pub bpm: f64,
    pub pitch: f64,
    pub beat: u8,
    /// The beat in the 16-beat phrase, 1 to 16. It is 0 when no master is known.
    pub phrase16: u8,
    pub playing: bool,
}

#[derive(Debug, Serialize)]
pub struct Clock {
    /// The beat in the 16-beat phrase of the MIDI clock output.
    pub out16: u8,
    /// "lock", "idle" or "wait n/m".
    pub state: String,
    /// The latest beat timing difference, in ms.
    pub delta_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Deck {
    pub num: u8,
    pub name: String,
    pub playing: bool,
    pub master: bool,
    pub virtual_master: bool,
    pub on_air: bool,
    pub sync: bool,
    pub bpm: Option<f64>,
    pub pitch: f64,
    pub play_state: Option<String>,
    pub beat: Option<u8>,
    pub beat_count: Option<u32>,
    pub playhead_ms: Option<u32>,
    pub track_len_ms: Option<u32>,
    /// The active loop as [start, end] in ms.
    pub loop_ms: Option<[u32; 2]>,
    /// The rekordbox ID of the waveform at `GET /waveform/{deck}`. The page fetches it again
    /// when this value changes.
    pub wave: Option<u32>,
    /// "artist - title - key - NNNbpm", from the fields that are known.
    pub track: Option<String>,
    /// "deck#N USB (rb#123)" when a rekordbox track is loaded.
    pub src: Option<String>,
    pub phrase: Option<Phrase>,
    /// "phrase info loading…" or "phrase info unavailable: reason".
    pub phrase_note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Phrase {
    pub kind: String,
    pub progress: f64,
    pub fill: bool,
    /// Beats until the next phrase starts.
    pub beats_left: u32,
}

#[derive(Debug, Serialize)]
pub struct Mixer {
    /// The EQ offsets from the centre detent at 64.
    pub hi: Option<i16>,
    pub mid: Option<i16>,
    pub low: Option<i16>,
    pub fader_pct: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub port: String,
    pub connected: bool,
    pub dropped: usize,
    pub clock_enabled: bool,
    pub pulses: u64,
    pub notes: u64,
    pub ccs: u64,
    /// The last notes, newest first, as (note, age in ms). Each stays for `HISTORY_MS`.
    pub notes_recent: Vec<(u8, u32)>,
    /// The last CCs, newest first, as (number, value, age in ms). Each stays for `HISTORY_MS`.
    pub ccs_recent: Vec<(u8, u8, u32)>,
    /// The MTC frame rate label, or `None` when MTC is off.
    pub mtc: Option<&'static str>,
    pub mtc_qf: u64,
    pub mtc_full: u64,
    pub osc: Osc,
}

#[derive(Debug, Serialize)]
pub struct Osc {
    pub enabled: bool,
    pub targets: Vec<String>,
    pub prefix: String,
    pub sent: u64,
    pub errors: u64,
    /// The last messages, newest first, as ("address args", age in ms). Each stays for `HISTORY_MS`.
    pub recent: Vec<(String, u32)>,
}

/// The MIDI output state that only the output worker knows.
pub struct OutputLink {
    pub connected: bool,
    pub dropped: usize,
}

/// Builds a status snapshot. See the module documentation for the lock rules.
pub fn snapshot(
    cfg: &SharedConfig,
    dj_state: &SharedState,
    device_table: &DeviceTable,
    activity: &Mutex<MidiActivity>,
    interfaces: &[NetworkIfaceInfo],
    out_link: OutputLink,
) -> Status {
    let cfg = cfg.read().clone();
    let (master, mixer, link_peers, mut dev_states) = {
        let st = dj_state.read();
        (
            st.master.clone(),
            st.mixer,
            st.link_peer_count,
            st.devices.clone(),
        )
    };
    let act = activity.lock().clone();
    let mut devices: Vec<_> = device_table
        .lock()
        .values()
        .filter(|d| d.device_number != cfg.device_number)
        .cloned()
        .collect();
    devices.sort_by_key(|d| d.device_number);

    let link_active = master.source == Some(BeatSource::AbletonLink);

    let decks = devices
        .into_iter()
        .map(|dev| {
            let ds = dev_states.remove(&dev.device_number);
            let is_master = ds.as_ref().is_some_and(|d| d.is_master);
            let mut deck = Deck {
                num: dev.device_number,
                name: dev.name,
                playing: ds.as_ref().is_some_and(|d| d.is_playing),
                master: is_master,
                virtual_master: !is_master
                    && master.is_virtual_master
                    && master.device_number == dev.device_number,
                on_air: ds.as_ref().is_some_and(|d| d.is_on_air),
                sync: ds.as_ref().is_some_and(|d| d.is_sync),
                bpm: ds.as_ref().map(|d| d.effective_bpm).filter(|b| *b > 0.0),
                pitch: ds.as_ref().map_or(0.0, |d| d.pitch_pct),
                play_state: None,
                beat: None,
                beat_count: None,
                playhead_ms: None,
                track_len_ms: None,
                loop_ms: None,
                wave: None,
                track: None,
                src: None,
                phrase: None,
                phrase_note: None,
            };
            let Some(ds) = ds else {
                return deck;
            };
            deck.play_state = Some(ds.play_state.to_string());
            deck.beat = Some(ds.beat_in_bar).filter(|b| (1..=4).contains(b));
            deck.beat_count = Some(ds.beat_count).filter(|c| *c != u32::MAX && *c > 0);
            deck.playhead_ms = ds.playhead_ms;
            deck.track_len_ms = ds.track_length_ms;
            deck.loop_ms = ds.loop_ms.map(|(a, b)| [a, b]);
            deck.wave = ds.overview.as_ref().map(|o| o.rekordbox_id);

            let mut parts: Vec<String> = [&ds.track_artist, &ds.track_title, &ds.track_key]
                .into_iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect();
            if !ds.track_title.is_empty() || !ds.track_artist.is_empty() {
                if let Some(bpm) = ds.track_bpm_meta {
                    parts.push(format!("{bpm:.0}bpm"));
                }
                deck.track = Some(parts.join(" - "));
            }

            if ds.rekordbox_id != 0 {
                let slot = match ds.track_slot {
                    1 => "CD",
                    2 => "SD",
                    3 => "USB",
                    4 => "rekordbox",
                    _ => "?",
                };
                deck.src = Some(format!(
                    "deck#{} {} (rb#{})",
                    ds.track_source_player, slot, ds.rekordbox_id
                ));
            }

            if let (Some(ss), Some(info)) = (&ds.song_structure, ds.phrase_info()) {
                if let Some(phrase) = ss.phrases.get(info.idx) {
                    deck.phrase = Some(Phrase {
                        kind: info.kind.to_string(),
                        progress: info.progress,
                        fill: phrase.has_fill && ds.beat_count >= u32::from(phrase.fill_beat),
                        beats_left: info.beats_left,
                    });
                }
            } else if ds.rekordbox_id != 0 && ds.song_structure.is_none() {
                // No error yet means that the fetch is still in progress.
                deck.phrase_note = Some(match &ds.phrase_error {
                    Some(reason) => format!("phrase info unavailable: {reason}"),
                    None => "phrase info loading…".to_string(),
                });
            }
            deck
        })
        .collect::<Vec<_>>();

    let mixer = mixer
        .iter()
        .any(|ch| *ch != MixerChannel::default())
        .then(|| {
            let eq = |v: Option<u8>| v.map(|v| i16::from(v) - 64);
            mixer
                .iter()
                .map(|ch| Mixer {
                    hi: eq(ch.hi),
                    mid: eq(ch.mid),
                    low: eq(ch.low),
                    fader_pct: ch.fader.map(|v| u16::from(v) * 100 / 127),
                })
                .collect()
        });

    let clock_state = if act.clock_waiting_for_phrase {
        format!(
            "wait {}/{}",
            act.clock_wait_beats_seen, cfg.midi.phrase_lock_stable_beats
        )
    } else if act.clock_running {
        "lock".to_string()
    } else {
        "idle".to_string()
    };

    Status {
        source: match master.source {
            Some(BeatSource::ProLink) => Some("ProLink (network)"),
            Some(BeatSource::AbletonLink) => Some("AbletonLink"),
            None => None,
        },
        mode: format_source_value(&cfg.source),
        iface: selected_interface_name_ip(&cfg, interfaces),
        master: Master {
            device: master.device_number,
            virtual_master: master.is_virtual_master,
            bpm: master.bpm,
            pitch: master.pitch_pct,
            beat: master.beat_in_bar,
            // DjState stores 0..=15.
            phrase16: if master.bpm > 0.0 {
                master.phrase_16_beat.min(15) + 1
            } else {
                0
            },
            playing: master.is_playing,
        },
        clock: Clock {
            out16: act.clock_phrase_beat.min(16),
            state: clock_state,
            delta_ms: act.clock_timing_delta_ms.map(f64::abs),
        },
        no_devices: decks.is_empty() && !link_active,
        decks,
        link_peers,
        link_active,
        mixer,
        output: Output {
            port: cfg.midi.output.clone(),
            connected: out_link.connected,
            dropped: out_link.dropped,
            clock_enabled: cfg.midi.clock_enabled,
            pulses: act.clock_pulses,
            notes: act.notes_sent,
            ccs: act.cc_sent,
            notes_recent: act
                .recent_notes
                .iter()
                .filter_map(|&(n, t)| Some((n, age_ms(t)?)))
                .collect(),
            ccs_recent: act
                .recent_ccs
                .iter()
                .filter_map(|&(n, v, t)| Some((n, v, age_ms(t)?)))
                .collect(),
            mtc: cfg
                .midi
                .mtc
                .enabled
                .then(|| cfg.midi.mtc.frame_rate.label()),
            mtc_qf: act.mtc_quarter_frames,
            mtc_full: act.mtc_full_frames,
            osc: Osc {
                enabled: cfg.osc.enabled,
                targets: cfg.osc.targets.clone(),
                prefix: cfg.osc.prefix.clone(),
                sent: act.osc_sent,
                errors: act.osc_errors,
                recent: act
                    .recent_osc
                    .iter()
                    .filter_map(|(m, t)| Some((m.clone(), age_ms(*t)?)))
                    .collect(),
            },
        },
    }
}

/// The output history keeps an entry in the status for this time after it was sent.
const HISTORY_MS: u32 = 3000;

/// Gives the age of an entry in ms, or `None` when it is older than `HISTORY_MS`.
fn age_ms(t: Instant) -> Option<u32> {
    let ms = u32::try_from(t.elapsed().as_millis()).ok()?;
    (ms < HISTORY_MS).then_some(ms)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::config::{new_shared, Config};
    use crate::prolink::discovery::Device;
    use crate::state::{new_shared as new_shared_state, DeviceState};

    fn device(n: u8) -> Device {
        Device {
            device_number: n,
            device_type: 1,
            name: format!("XDJ-{n}"),
            mac: [0; 6],
            ip: [10, 0, 0, n],
            last_seen: Instant::now(),
        }
    }

    #[test]
    fn snapshot_lists_decks_sorted_without_our_own_device_and_shows_the_mixer() {
        let cfg = new_shared(Config::default());
        let ours = cfg.read().device_number;
        let dj_state = new_shared_state(0);
        {
            let mut st = dj_state.write();
            let mut d2 = DeviceState::new(2);
            d2.is_master = true;
            d2.is_playing = true;
            d2.effective_bpm = 128.0;
            d2.beat_in_bar = 3;
            d2.track_artist = "Artist".into();
            d2.track_title = "Title".into();
            d2.loop_ms = Some((1_000, 3_000));
            d2.overview = Some(Arc::new(crate::state::TrackOverview {
                rekordbox_id: 207,
                ..Default::default()
            }));
            st.devices.insert(2, d2);
            st.mixer[0].hi = Some(70);
            st.mixer[0].fader = Some(127);
        }
        let table: DeviceTable = Arc::new(Mutex::new(HashMap::from([
            (4, device(4)),
            (2, device(2)),
            (ours, device(ours)),
            (1, device(1)),
        ])));
        let activity = Mutex::new(MidiActivity::default());

        let st = snapshot(
            &cfg,
            &dj_state,
            &table,
            &activity,
            &[],
            OutputLink {
                connected: false,
                dropped: 0,
            },
        );

        let nums: Vec<u8> = st.decks.iter().map(|d| d.num).collect();
        assert_eq!(nums, vec![1, 2, 4]);
        let d2 = &st.decks[1];
        assert!(d2.master && d2.playing);
        assert_eq!(d2.bpm, Some(128.0));
        assert_eq!(d2.beat, Some(3));
        assert_eq!(d2.track.as_deref(), Some("Artist - Title"));
        assert_eq!(d2.loop_ms, Some([1_000, 3_000]));
        assert_eq!(d2.wave, Some(207));
        assert_eq!(st.decks[0].wave, None);
        assert!(st.decks[0].play_state.is_none());
        assert!(!st.no_devices);

        let mixer = st.mixer.expect("a moved control shows the mixer");
        assert_eq!(mixer[0].hi, Some(6));
        assert_eq!(mixer[0].fader_pct, Some(100));
        assert_eq!(mixer[1].hi, None);
    }

    #[test]
    fn snapshot_without_devices_reports_no_devices_and_no_mixer() {
        let st = snapshot(
            &new_shared(Config::default()),
            &new_shared_state(0),
            &Arc::new(Mutex::new(HashMap::new())),
            &Mutex::new(MidiActivity::default()),
            &[],
            OutputLink {
                connected: true,
                dropped: 0,
            },
        );
        assert!(st.no_devices);
        assert!(st.mixer.is_none());
        assert_eq!(st.master.phrase16, 0);
        assert_eq!(st.clock.state, "idle");
    }
}
