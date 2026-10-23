use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::config::{CcConfig, NoteConfig, SharedConfig};
use crate::midi::activity::{remember, MidiActivity};
use crate::midi::MidiTransport;
use crate::osc::{Arg, OscOut};
use crate::prolink::beat_listener::BeatEvent;
use crate::prolink::status_listener::StatusEvent;
use crate::state::{BeatSource, SharedState};

// ── MIDI byte helpers ─────────────────────────────────────────────────────────

fn note_on(ch: u8, note: u8, vel: u8) -> [u8; 3] {
    [0x90 | (ch & 0x0f), note & 0x7f, vel & 0x7f]
}

fn note_off(ch: u8, note: u8) -> [u8; 3] {
    [0x80 | (ch & 0x0f), note & 0x7f, 0]
}

fn cc(ch: u8, num: u8, val: u8) -> [u8; 3] {
    [0xB0 | (ch & 0x0f), num & 0x7f, val & 0x7f]
}

fn program_change(ch: u8, prog: u8) -> [u8; 2] {
    [0xC0 | (ch & 0x0f), prog & 0x7f]
}

/// Scale a float in [0.0, 1.0] to a 7-bit MIDI value (0–127).
fn scale01(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// Map BPM range 60–187 to 0–127.
fn bpm_coarse(bpm: f64) -> u8 {
    ((bpm - 60.0).clamp(0.0, 127.0)) as u8
}

/// Map fractional BPM (0.00–0.99) to 0–127.
fn bpm_fine(bpm: f64) -> u8 {
    let frac = bpm.fract();
    (frac * 127.0).round() as u8
}

/// Map pitch percent (-10.0 … +10.0) to 0–127, centre = 64.
/// Clamps at ±10 % (standard pitch fader range).
fn pitch_cc(pct: f64) -> u8 {
    let normalised = (pct / 10.0 + 1.0) / 2.0; // 0.0–1.0
    scale01(normalised)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{new_shared, Config};
    use crate::midi::test_utils::MockMidiTransport;
    use crate::state::{new_shared as new_shared_state, BeatSource, MasterState};
    use std::time::Duration;

    #[test]
    fn test_program_change_helper() {
        assert_eq!(program_change(2, 11), [0xC2, 11]);
        assert_eq!(program_change(17, 200), [0xC1, 72]);
    }

    fn cdj_at(rekordbox_id: u32, beat_count: u32) -> crate::prolink::packets::CdjStatus {
        crate::prolink::packets::CdjStatus {
            device_number: 1,
            play_state: crate::prolink::packets::PlayState::Playing,
            is_master: true,
            is_sync: false,
            is_on_air: true,
            is_playing_flag: true,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: 12800,
            track_bpm: Some(128.0),
            effective_bpm: 128.0,
            pitch_pct: 0.0,
            beat_count,
            beat_in_bar: 1,
            track_source_player: 1,
            track_slot: 3,
            track_type: 1,
            rekordbox_id,
            loop_ms: None,
        }
    }

    #[tokio::test]
    async fn master_phrase_kind_is_sent_as_cc_note_and_program() {
        use crate::state::{PhraseEntry, PhraseKind, SongStructure, TrackMood};

        let midi = Arc::new(MockMidiTransport::new());
        let midi_transport: Arc<dyn MidiTransport> = midi.clone();
        let cfg = new_shared(Config::default());
        cfg.write().midi.program.enabled = true;
        let state = new_shared_state(30);
        let phrase = |beat, kind| PhraseEntry {
            index: 1,
            beat,
            kind,
            has_fill: false,
            fill_beat: 0,
        };
        {
            let mut st = state.write();
            st.master.device_number = 1;
            st.apply_cdj_status(&cdj_at(7, 5));
            st.set_song_structure(
                1,
                7,
                SongStructure {
                    mood: TrackMood::Mid,
                    end_beat: 65,
                    phrases: vec![phrase(1, PhraseKind::Intro), phrase(33, PhraseKind::Verse1)],
                },
            );
            st.apply_cdj_status(&cdj_at(7, 5));
        }
        let (status_tx, status_rx) = broadcast::channel(8);
        let (_beat_tx, beat_rx) = broadcast::channel(8);
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let handle = tokio::spawn(run_with_midi(
            midi_transport,
            state.clone(),
            beat_rx,
            status_rx,
            cfg,
            activity,
        ));
        let wake = || {
            let _ = status_tx.send(StatusEvent::Mixer(crate::prolink::packets::MixerStatus {
                device_number: 16,
                is_master: false,
                bpm_raw: 12800,
                track_bpm: Some(128.0),
                beat_in_bar: 1,
            }));
        };

        wake();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let msgs = midi.get_messages();
        assert!(msgs.contains(&cc(0, 20, 0).to_vec()));
        assert!(msgs.contains(&note_on(9, 40, 127).to_vec()));
        assert!(msgs.contains(&note_off(9, 40).to_vec()));
        assert!(msgs.contains(&program_change(0, 0).to_vec()));
        assert!(msgs.contains(&cc(0, 22, 28).to_vec())); // 33 - 5 beats left

        midi.clear_messages();
        state.write().apply_cdj_status(&cdj_at(7, 40));
        wake();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let msgs = midi.get_messages();
        assert!(msgs.contains(&cc(0, 20, 10).to_vec()));
        assert!(msgs.contains(&note_on(9, 41, 127).to_vec()));
        assert!(msgs.contains(&program_change(0, 1).to_vec()));
        assert!(msgs.contains(&cc(0, 22, 25).to_vec())); // 65 - 40 beats left

        handle.abort();
    }

    #[test]
    fn test_scale01() {
        assert_eq!(scale01(0.0), 0);
        assert_eq!(scale01(0.5), 64);
        assert_eq!(scale01(1.0), 127);
        assert_eq!(scale01(-0.1), 0); // clamped
        assert_eq!(scale01(1.1), 127); // clamped
    }

    #[test]
    fn test_bpm_coarse() {
        assert_eq!(bpm_coarse(60.0), 0);
        assert_eq!(bpm_coarse(120.0), 60);
        assert_eq!(bpm_coarse(187.0), 127);
        assert_eq!(bpm_coarse(59.0), 0); // clamped
        assert_eq!(bpm_coarse(200.0), 127); // clamped
    }

    #[test]
    fn test_bpm_fine() {
        assert_eq!(bpm_fine(120.0), 0); // 120.00
        assert_eq!(bpm_fine(120.5), 64); // 120.50
        assert_eq!(bpm_fine(120.99), 126); // 120.99
        assert_eq!(bpm_fine(120.999), 127); // rounds to 127
    }

    #[test]
    fn test_pitch_cc() {
        assert_eq!(pitch_cc(-10.0), 0);
        assert_eq!(pitch_cc(0.0), 64);
        assert_eq!(pitch_cc(10.0), 127);
        assert_eq!(pitch_cc(-15.0), 0); // clamped
        assert_eq!(pitch_cc(15.0), 127); // clamped
        assert_eq!(pitch_cc(5.0), 95); // 0.75 * 127 = 95.25 rounded to 95
    }

    #[test]
    fn test_midi_message_helpers() {
        assert_eq!(note_on(0, 60, 127), [0x90, 60, 127]);
        assert_eq!(note_off(0, 60), [0x80, 60, 0]);
        assert_eq!(cc(2, 10, 99), [0xB2, 10, 99]);
        assert_eq!(note_on(17, 200, 200), [0x91, 72, 72]);
        assert_eq!(cc(31, 255, 255), [0xBF, 127, 127]);
    }

    #[test]
    fn beat_notes_follow_the_active_master_source() {
        assert!(prolink_beat_allowed(Some(BeatSource::ProLink), 1, 1));
        assert!(!prolink_beat_allowed(Some(BeatSource::ProLink), 1, 2));
        assert!(!prolink_beat_allowed(Some(BeatSource::AbletonLink), 0, 1));
        assert!(link_beat_allowed(Some(BeatSource::AbletonLink)));
        assert!(!link_beat_allowed(Some(BeatSource::ProLink)));
        assert!(!link_beat_allowed(None));
    }

    #[test]
    fn test_beat_velocity() {
        assert_eq!(beat_velocity(1), 127);
        assert_eq!(beat_velocity(2), 80);
        assert_eq!(beat_velocity(3), 64);
        assert_eq!(beat_velocity(4), 80);
    }

    #[test]
    fn test_fire_beat_notes_regular_and_downbeat() {
        let midi = MockMidiTransport::new();
        let note_cfg = NoteConfig::default();
        let activity = Arc::new(Mutex::new(MidiActivity::default()));

        fire_beat_notes(&midi, &note_cfg, 2, &activity);
        let msgs = midi.get_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs[0],
            note_on(note_cfg.channel, note_cfg.beat, 80).to_vec()
        );
        assert_eq!(msgs[1], note_off(note_cfg.channel, note_cfg.beat).to_vec());

        midi.clear_messages();
        fire_beat_notes(&midi, &note_cfg, 1, &activity);
        let msgs = midi.get_messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[0],
            note_on(note_cfg.channel, note_cfg.beat, 127).to_vec()
        );
        assert_eq!(msgs[1], note_off(note_cfg.channel, note_cfg.beat).to_vec());
        assert_eq!(
            msgs[2],
            note_on(note_cfg.channel, note_cfg.downbeat, 127).to_vec()
        );
        assert_eq!(
            msgs[3],
            note_off(note_cfg.channel, note_cfg.downbeat).to_vec()
        );
    }

    #[tokio::test]
    async fn no_bass_note_pulses_on_cut_and_on_return() {
        let midi = Arc::new(MockMidiTransport::new());
        let midi_transport: Arc<dyn MidiTransport> = midi.clone();
        let state = new_shared_state(30);
        state.write().apply_cdj_status(&cdj_at(7, 5));
        let (status_tx, status_rx) = broadcast::channel(8);
        let (_beat_tx, beat_rx) = broadcast::channel(8);
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let handle = tokio::spawn(run_with_midi(
            midi_transport,
            state.clone(),
            beat_rx,
            status_rx,
            new_shared(Config::default()),
            activity,
        ));
        let wake = || {
            let _ = status_tx.send(StatusEvent::Mixer(crate::prolink::packets::MixerStatus {
                device_number: 16,
                is_master: false,
                bpm_raw: 12800,
                track_bpm: Some(128.0),
                beat_in_bar: 1,
            }));
        };

        state.write().mixer[0].low = Some(0);
        wake();
        let pulse = vec![note_on(9, 39, 127).to_vec(), note_off(9, 39).to_vec()];
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi.get_messages().windows(2).any(|w| w == pulse));

        midi.clear_messages();
        state.write().mixer[0].low = Some(64);
        wake();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi.get_messages().windows(2).any(|w| w == pulse));

        handle.abort();
    }

    #[tokio::test]
    async fn runtime_note_config_changes_take_effect() {
        let midi = Arc::new(MockMidiTransport::new());
        let midi_transport: Arc<dyn MidiTransport> = midi.clone();
        let cfg = new_shared(Config::default());
        let state = new_shared_state(30);
        state.write().master.source = Some(BeatSource::AbletonLink);
        let (_status_tx, status_rx) = broadcast::channel(8);
        let (beat_tx, beat_rx) = broadcast::channel(8);
        let activity = Arc::new(Mutex::new(MidiActivity::default()));

        let handle = tokio::spawn(run_with_midi(
            midi_transport,
            state,
            beat_rx,
            status_rx,
            cfg.clone(),
            activity,
        ));

        let _ = beat_tx.send(BeatEvent::LinkBeat {
            bpm: 120.0,
            beat_in_bar: 2,
            bar_phase: 0.25,
            beat_phase: 0.5,
            received_at: Instant::now(),
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi.get_messages().contains(&note_on(9, 36, 80).to_vec()));

        midi.clear_messages();
        {
            let mut guard = cfg.write();
            guard.midi.notes.channel = 1;
            guard.midi.notes.beat = 40;
        }
        let _ = beat_tx.send(BeatEvent::LinkBeat {
            bpm: 120.0,
            beat_in_bar: 2,
            bar_phase: 0.25,
            beat_phase: 0.5,
            received_at: Instant::now(),
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi.get_messages().contains(&note_on(1, 40, 80).to_vec()));

        handle.abort();
    }

    #[tokio::test]
    async fn link_downbeat_is_sent_over_osc() {
        use crate::osc::{encode, Arg};

        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let cfg = new_shared(Config::default());
        {
            let mut c = cfg.write();
            c.osc.enabled = true;
            c.osc.targets = vec![rx.local_addr().unwrap().to_string()];
        }
        let state = new_shared_state(30);
        state.write().master.source = Some(BeatSource::AbletonLink);
        let (_status_tx, status_rx) = broadcast::channel(8);
        let (beat_tx, beat_rx) = broadcast::channel(8);
        let handle = tokio::spawn(run_with_midi(
            Arc::new(MockMidiTransport::new()),
            state,
            beat_rx,
            status_rx,
            cfg,
            Arc::new(Mutex::new(MidiActivity::default())),
        ));

        let _ = beat_tx.send(BeatEvent::LinkBeat {
            bpm: 120.0,
            beat_in_bar: 1,
            bar_phase: 0.0,
            beat_phase: 0.0,
            received_at: Instant::now(),
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        rx.set_nonblocking(true).unwrap();
        while let Ok(n) = rx.recv(&mut buf) {
            got.push(buf[..n].to_vec());
        }
        assert!(got.contains(&encode("/xdj/beat", &[Arg::I(1)])));
        assert!(got.contains(&encode("/xdj/downbeat", &[])));

        handle.abort();
    }

    #[tokio::test]
    async fn runtime_cc_config_changes_take_effect() {
        let midi = Arc::new(MockMidiTransport::new());
        let midi_transport: Arc<dyn MidiTransport> = midi.clone();
        let cfg = new_shared(Config::default());
        let state = new_shared_state(30);
        state.write().master = MasterState {
            source: Some(BeatSource::ProLink),
            bpm: 128.0,
            pitch_pct: 0.0,
            bar_phase: 0.5,
            beat_phase: 0.25,
            is_playing: true,
            device_number: 1,
            ..Default::default()
        };
        let (status_tx, status_rx) = broadcast::channel(8);
        let (_beat_tx, beat_rx) = broadcast::channel(8);
        let activity = Arc::new(Mutex::new(MidiActivity::default()));

        let handle = tokio::spawn(run_with_midi(
            midi_transport,
            state.clone(),
            beat_rx,
            status_rx,
            cfg.clone(),
            activity,
        ));

        let _ = status_tx.send(StatusEvent::Mixer(crate::prolink::packets::MixerStatus {
            device_number: 16,
            is_master: true,
            bpm_raw: 12800,
            track_bpm: Some(128.0),
            beat_in_bar: 1,
        }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi
            .get_messages()
            .contains(&cc(0, 2, pitch_cc(0.0)).to_vec()));

        midi.clear_messages();
        {
            let mut guard = cfg.write();
            guard.midi.cc.channel = 2;
            guard.midi.cc.pitch = 12;
        }
        state.write().master.pitch_pct = 5.0;
        let _ = status_tx.send(StatusEvent::Mixer(crate::prolink::packets::MixerStatus {
            device_number: 16,
            is_master: true,
            bpm_raw: 12800,
            track_bpm: Some(128.0),
            beat_in_bar: 1,
        }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi
            .get_messages()
            .contains(&cc(2, 12, pitch_cc(5.0)).to_vec()));

        handle.abort();
    }

    #[tokio::test]
    async fn note_switches_stop_the_notes() {
        let midi = Arc::new(MockMidiTransport::new());
        let midi_transport: Arc<dyn MidiTransport> = midi.clone();
        let cfg = new_shared(Config::default());
        cfg.write().midi.notes.send.beat = false;
        let state = new_shared_state(30);
        state.write().master.source = Some(BeatSource::AbletonLink);
        let (_status_tx, status_rx) = broadcast::channel(8);
        let (beat_tx, beat_rx) = broadcast::channel(8);
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let handle = tokio::spawn(run_with_midi(
            midi_transport,
            state,
            beat_rx,
            status_rx,
            cfg.clone(),
            activity,
        ));
        let downbeat = || {
            let _ = beat_tx.send(BeatEvent::LinkBeat {
                bpm: 120.0,
                beat_in_bar: 1,
                bar_phase: 0.0,
                beat_phase: 0.0,
                received_at: Instant::now(),
            });
        };

        downbeat();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let msgs = midi.get_messages();
        assert!(!msgs.contains(&note_on(9, 36, 127).to_vec()));
        assert!(msgs.contains(&note_on(9, 37, 127).to_vec()));

        midi.clear_messages();
        cfg.write().midi.notes.enabled = false;
        downbeat();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi.get_messages().iter().all(|m| m[0] & 0xF0 != 0x90));

        handle.abort();
    }

    #[tokio::test]
    async fn cc_switches_stop_the_ccs_and_resend_when_on() {
        let midi = Arc::new(MockMidiTransport::new());
        let midi_transport: Arc<dyn MidiTransport> = midi.clone();
        let cfg = new_shared(Config::default());
        cfg.write().midi.cc.send.pitch = false;
        let state = new_shared_state(30);
        state.write().master = MasterState {
            source: Some(BeatSource::ProLink),
            bpm: 128.0,
            is_playing: true,
            device_number: 1,
            ..Default::default()
        };
        let (status_tx, status_rx) = broadcast::channel(8);
        let (_beat_tx, beat_rx) = broadcast::channel(8);
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let handle = tokio::spawn(run_with_midi(
            midi_transport,
            state.clone(),
            beat_rx,
            status_rx,
            cfg.clone(),
            activity,
        ));
        let wake = || {
            let _ = status_tx.send(StatusEvent::Mixer(crate::prolink::packets::MixerStatus {
                device_number: 16,
                is_master: true,
                bpm_raw: 12800,
                track_bpm: Some(128.0),
                beat_in_bar: 1,
            }));
        };
        let pitch_ccs = |msgs: Vec<Vec<u8>>| msgs.iter().filter(|m| m[..2] == [0xB0, 2]).count();

        wake();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let msgs = midi.get_messages();
        assert_eq!(pitch_ccs(msgs.clone()), 0);
        assert!(msgs.contains(&cc(0, 1, bpm_coarse(128.0)).to_vec()));

        // The pitch does not change. The CC goes out when its switch comes on.
        midi.clear_messages();
        cfg.write().midi.cc.send.pitch = true;
        wake();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi
            .get_messages()
            .contains(&cc(0, 2, pitch_cc(0.0)).to_vec()));

        midi.clear_messages();
        cfg.write().midi.cc.enabled = false;
        state.write().master.pitch_pct = 5.0;
        wake();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(midi.get_messages().iter().all(|m| m[0] & 0xF0 != 0xB0));

        handle.abort();
    }
}

// ── Beat note helper ──────────────────────────────────────────────────────────

/// Beat velocity by position: 127 on beat 1, 64 on beat 3, 80 on beats 2/4.
fn beat_velocity(beat_in_bar: u8) -> u8 {
    match beat_in_bar {
        1 => 127,
        3 => 64,
        _ => 80,
    }
}

/// Return true if a ProLink beat from `device` may fire beat notes.
/// The active master source must not be Ableton Link.
fn prolink_beat_allowed(master_source: Option<BeatSource>, master_num: u8, device: u8) -> bool {
    master_source != Some(BeatSource::AbletonLink) && (device == master_num || master_num == 0)
}

/// Return true if a Link beat may fire beat notes.
/// Link must be the active master source.
fn link_beat_allowed(master_source: Option<BeatSource>) -> bool {
    master_source == Some(BeatSource::AbletonLink)
}

/// Sends a note on and a note off when `on` is true.
fn pulse_note(
    midi: &dyn MidiTransport,
    ch: u8,
    note: u8,
    vel: u8,
    on: bool,
    activity: &Arc<Mutex<MidiActivity>>,
) {
    if !on {
        return;
    }
    let _ = midi.send_message(&note_on(ch, note, vel));
    let _ = midi.send_message(&note_off(ch, note));
    // The UI counters are optional. Do not wait for the lock.
    if let Some(mut act) = activity.try_lock() {
        act.notes_sent += 1;
        remember(&mut act.recent_notes, (note, Instant::now()));
    }
}

/// Fire beat + downbeat notes through the MIDI connection.
fn fire_beat_notes(
    midi: &dyn MidiTransport,
    note_cfg: &NoteConfig,
    beat_in_bar: u8,
    activity: &Arc<Mutex<MidiActivity>>,
) {
    let n = note_cfg;
    let vel = beat_velocity(beat_in_bar);
    pulse_note(
        midi,
        n.channel,
        n.beat,
        vel,
        n.enabled && n.send.beat,
        activity,
    );
    if beat_in_bar == 1 {
        let on = n.enabled && n.send.downbeat;
        pulse_note(midi, n.channel, n.downbeat, 127, on, activity);
    }
}

/// Send `/beat` on every beat, and `/downbeat` on beat 1.
fn send_osc_beat(osc: &OscOut, cfg: &SharedConfig, beat_in_bar: u8) {
    osc.send(cfg, "/beat", &[Arg::I(beat_in_bar.into())]);
    if beat_in_bar == 1 {
        osc.send(cfg, "/downbeat", &[]);
    }
}

// ── Previous CC values (to suppress duplicate sends) ─────────────────────────

#[derive(Default)]
struct PrevCc {
    bpm_coarse: u8,
    bpm_fine: u8,
    pitch: u8,
    bar_phase: u8,
    beat_phase: u8,
    playing: u8,
    master_deck: u8,
    phrase_16: u8,
    phrase_progress: u8,
    phrase_beats_left: u8,
}

// ── Main mapper task ──────────────────────────────────────────────────────────

async fn run_with_midi(
    midi: Arc<dyn MidiTransport>,
    state: SharedState,
    mut beat_rx: broadcast::Receiver<BeatEvent>,
    mut status_rx: broadcast::Receiver<StatusEvent>,
    cfg: SharedConfig,
    activity: Arc<Mutex<MidiActivity>>,
) {
    let mut prev = PrevCc::default();
    let mut prev_phrase_idx: Option<usize> = None;
    // (device, rekordbox_id, phrase index) of the last phrase kind that was sent.
    let mut prev_phrase_key: Option<(u8, u32, usize)> = None;
    let mut prev_phrase_16_beat: u8 = 0;
    let mut prev_no_bass = false;
    let osc = OscOut::with_activity(Arc::clone(&activity));
    let mut prev_osc_bpm = 0.0f32;
    // The OSC values have their own previous values: a CC switch must not change the OSC output.
    let mut prev_osc_playing = 0u8;
    let mut prev_osc_master = 0u8;
    let mut prev_osc_progress = 0u8;
    let mut prev_osc_beats_left = u32::MAX;
    // (device, rekordbox_id, title) of the last track that was sent over OSC.
    let mut prev_osc_track: Option<(u8, u32, String)> = None;
    tracing::info!("MIDI mapper task started");

    loop {
        tokio::select! {
            evt = beat_rx.recv() => {
                match evt {
                    Ok(BeatEvent::Beat { packet: bp, .. }) => {
                        // Only trigger notes / CCs for the master deck of the active source.
                        let (master_num, master_source) = {
                            let st = state.read();
                            (st.master.device_number, st.master.source)
                        };
                        if !prolink_beat_allowed(master_source, master_num, bp.device_number) {
                            continue;
                        }

                        let note_cfg = cfg.read().midi.notes;
                        fire_beat_notes(midi.as_ref(), &note_cfg, bp.beat_in_bar, &activity);
                        send_osc_beat(&osc, &cfg, bp.beat_in_bar);
                        tracing::debug!(
                            device = bp.device_number,
                            beat = bp.beat_in_bar,
                            bpm = %format!("{:.2}", bp.effective_bpm),
                            "Beat"
                        );
                    }
                    Ok(BeatEvent::AbsPosition { .. }) => {
                        // Phase updates handled via shared state CCs below.
                    }
                    Ok(BeatEvent::LinkBeat { bpm, beat_in_bar, .. }) => {
                        if !link_beat_allowed(state.read().master.source) {
                            continue;
                        }
                        let note_cfg = cfg.read().midi.notes;
                        fire_beat_notes(midi.as_ref(), &note_cfg, beat_in_bar, &activity);
                        send_osc_beat(&osc, &cfg, beat_in_bar);
                        tracing::debug!(
                            beat = beat_in_bar,
                            bpm = %format!("{:.2}", bpm),
                            "Link beat"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                }
            }

            evt = status_rx.recv() => {
                match evt {
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        }

        // ── CC updates ────────────────────────────────────────────────────────
        let master = state.read().master.clone();
        let cc_cfg: CcConfig = cfg.read().midi.cc;

        let new_bpm_coarse = bpm_coarse(master.bpm);
        let new_bpm_fine = bpm_fine(master.bpm);
        let new_pitch = pitch_cc(master.pitch_pct);
        let new_bar_phase = scale01(master.bar_phase);
        let new_beat_phase = scale01(master.beat_phase);
        let new_playing: u8 = if master.is_playing { 127 } else { 0 };
        let new_master_deck = master.device_number.min(127);

        // ── OSC state updates ─────────────────────────────────────────────────
        let osc_bpm = (master.bpm * 100.0).round() as f32 / 100.0;
        if osc_bpm != prev_osc_bpm {
            prev_osc_bpm = osc_bpm;
            osc.send(&cfg, "/bpm", &[Arg::F(osc_bpm)]);
        }
        if new_playing != prev_osc_playing {
            prev_osc_playing = new_playing;
            osc.send(&cfg, "/playing", &[Arg::I((new_playing > 0).into())]);
        }
        if new_master_deck != prev_osc_master {
            prev_osc_master = new_master_deck;
            osc.send(&cfg, "/master", &[Arg::I(new_master_deck.into())]);
        }
        let track = {
            let st = state.read();
            st.devices
                .get(&master.device_number)
                .filter(|d| d.rekordbox_id != 0)
                .map(|d| {
                    (
                        d.rekordbox_id,
                        d.track_title.clone(),
                        d.track_artist.clone(),
                    )
                })
        };
        if let Some((id, title, artist)) = track {
            let key = (master.device_number, id, title.clone());
            if prev_osc_track.as_ref() != Some(&key) {
                prev_osc_track = Some(key);
                osc.send(
                    &cfg,
                    "/track",
                    &[Arg::S(&title), Arg::S(&artist), Arg::I(id as i32)],
                );
            }
        }

        // A CC that is off forgets its previous value, so the CC is sent again when it comes on.
        macro_rules! send_cc_if_changed {
            ($prev:expr, $new:expr, $num:expr, $ch:expr, $on:expr) => {
                if !(cc_cfg.enabled && $on) {
                    $prev = u8::MAX;
                } else if $prev != $new {
                    $prev = $new;
                    let _ = midi.send_message(&cc($ch, $num, $new));
                    // The UI counters are optional. Do not wait for the lock.
                    if let Some(mut act) = activity.try_lock() {
                        act.cc_sent += 1;
                        remember(&mut act.recent_ccs, ($num, $new, Instant::now()));
                    }
                }
            };
        }

        send_cc_if_changed!(
            prev.bpm_coarse,
            new_bpm_coarse,
            cc_cfg.bpm_coarse,
            cc_cfg.channel,
            cc_cfg.send.bpm_coarse
        );
        send_cc_if_changed!(
            prev.bpm_fine,
            new_bpm_fine,
            cc_cfg.bpm_fine,
            cc_cfg.channel,
            cc_cfg.send.bpm_fine
        );
        send_cc_if_changed!(
            prev.pitch,
            new_pitch,
            cc_cfg.pitch,
            cc_cfg.channel,
            cc_cfg.send.pitch
        );
        send_cc_if_changed!(
            prev.bar_phase,
            new_bar_phase,
            cc_cfg.bar_phase,
            cc_cfg.channel,
            cc_cfg.send.bar_phase
        );
        send_cc_if_changed!(
            prev.beat_phase,
            new_beat_phase,
            cc_cfg.beat_phase,
            cc_cfg.channel,
            cc_cfg.send.beat_phase
        );
        send_cc_if_changed!(
            prev.playing,
            new_playing,
            cc_cfg.playing,
            cc_cfg.channel,
            cc_cfg.send.playing
        );
        send_cc_if_changed!(
            prev.master_deck,
            new_master_deck,
            cc_cfg.master_deck,
            cc_cfg.channel,
            cc_cfg.send.master_deck
        );

        let phrase_16_on = cc_cfg.enabled && cc_cfg.send.phrase_16;
        if phrase_16_on && master.phrase_16_beat == 0 && prev_phrase_16_beat != 0 {
            let new_phrase_16 = 0u8;
            if prev.phrase_16 != new_phrase_16 {
                prev.phrase_16 = new_phrase_16;
                let _ = midi.send_message(&cc(cc_cfg.channel, cc_cfg.phrase_16, new_phrase_16));
                // The UI counters are optional. Do not wait for the lock.
                if let Some(mut act) = activity.try_lock() {
                    act.cc_sent += 1;
                    remember(&mut act.recent_ccs, (cc_cfg.phrase_16, new_phrase_16, Instant::now()));
                }
            }
        }
        prev_phrase_16_beat = master.phrase_16_beat;

        // ── Phrase change detection ───────────────────────────────────────────
        // Check if the master deck's phrase changed; fire a MIDI note if so.
        if master.device_number > 0 {
            let cur_phrase = state
                .read()
                .devices
                .get(&master.device_number)
                .and_then(|d| d.current_phrase_idx);

            if cur_phrase != prev_phrase_idx && cur_phrase.is_some() && prev_phrase_idx.is_some() {
                let n = cfg.read().midi.notes;
                let on = n.enabled && n.send.phrase_change;
                pulse_note(
                    midi.as_ref(),
                    n.channel,
                    n.phrase_change,
                    127,
                    on,
                    &activity,
                );
                tracing::debug!(note = n.phrase_change, on, "Phrase change");
            }
            prev_phrase_idx = cur_phrase;
        }

        // ── Phrase kind output ────────────────────────────────────────────────
        // Send the kind of the master phrase as a CC, a note and an optional Program Change.
        // This also fires on a track load and on a master switch, not only at a phrase boundary.
        // When the master role goes away and comes back, the same phrase fires again.
        let (phrase, rekordbox_id) = {
            let st = state.read();
            let id = st
                .devices
                .get(&master.device_number)
                .map_or(0, |d| d.rekordbox_id);
            (st.master_phrase(), id)
        };
        if let Some(info) = phrase {
            let key = (master.device_number, rekordbox_id, info.idx);
            if prev_phrase_key != Some(key) {
                prev_phrase_key = Some(key);
                if let Some(code) = info.kind.code() {
                    let cfg_now = cfg.read().midi.clone();
                    let kind_val = code * 10;
                    let c = cfg_now.cc;
                    if c.enabled && c.send.phrase_kind {
                        let _ = midi.send_message(&cc(c.channel, c.phrase_kind, kind_val));
                        // The UI counters are optional. Do not wait for the lock.
                        if let Some(mut act) = activity.try_lock() {
                            act.cc_sent += 1;
                            remember(&mut act.recent_ccs, (c.phrase_kind, kind_val, Instant::now()));
                        }
                    }
                    let n = cfg_now.notes;
                    let note = n.phrase_base.saturating_add(code);
                    let on = n.enabled && n.send.phrase_kind;
                    pulse_note(midi.as_ref(), n.channel, note, 127, on, &activity);
                    if cfg_now.program.enabled {
                        let _ = midi.send_message(&program_change(cfg_now.program.channel, code));
                    }
                    osc.send(
                        &cfg,
                        "/phrase",
                        &[Arg::S(&info.kind.to_string()), Arg::I(code.into())],
                    );
                    tracing::debug!(kind = %info.kind, code, "Phrase kind MIDI sent");
                }
            }
            if scale01(info.progress) != prev_osc_progress {
                prev_osc_progress = scale01(info.progress);
                osc.send(&cfg, "/phrase/progress", &[Arg::F(info.progress as f32)]);
            }
            if info.beats_left != prev_osc_beats_left {
                prev_osc_beats_left = info.beats_left;
                osc.send(
                    &cfg,
                    "/phrase/beats_left",
                    &[Arg::I(info.beats_left as i32)],
                );
            }
            send_cc_if_changed!(
                prev.phrase_progress,
                scale01(info.progress),
                cc_cfg.phrase_progress,
                cc_cfg.channel,
                cc_cfg.send.phrase_progress
            );
            send_cc_if_changed!(
                prev.phrase_beats_left,
                info.beats_left.min(127) as u8,
                cc_cfg.phrase_beats_left,
                cc_cfg.channel,
                cc_cfg.send.phrase_beats_left
            );
        } else {
            prev_phrase_key = None;
        }

        // ── NO_BASS trigger ───────────────────────────────────────────────────
        // The note pulses when the bass is cut on all live mixer channels.
        // The note pulses again when the bass comes back.
        let no_bass = state.read().no_bass();
        if no_bass != prev_no_bass {
            prev_no_bass = no_bass;
            let n = cfg.read().midi.notes;
            let on = n.enabled && n.send.no_bass;
            pulse_note(midi.as_ref(), n.channel, n.no_bass, 127, on, &activity);
            osc.send(&cfg, "/no_bass", &[Arg::I(no_bass.into())]);
            tracing::debug!(no_bass, note = n.no_bass, on, "NO_BASS");
        }
    }
}

pub async fn run(
    midi: Arc<dyn MidiTransport>,
    state: SharedState,
    beat_rx: broadcast::Receiver<BeatEvent>,
    status_rx: broadcast::Receiver<StatusEvent>,
    cfg: SharedConfig,
    activity: Arc<Mutex<MidiActivity>>,
) {
    run_with_midi(midi, state, beat_rx, status_rx, cfg, activity).await;
}
