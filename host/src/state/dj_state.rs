use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;

use crate::config::Source;
use crate::prolink::packets::{AbsPositionPacket, BeatPacket, CdjStatus, MixerStatus};
use crate::prolink::BEAT_NONE;

use super::beat_source::BeatSource;
use super::device::{DeviceState, MasterState, MixerChannel, PhraseInfo};
use super::song_structure::SongStructure;
use super::timing::{LogThrottle, TimingMeasurement, TimingModel};
use super::track_change::TrackChange;

// ponytail: the NO_BASS thresholds are constants. If a user must tune them, move them into
// `midi.notes` next to `no_bass`.
/// The lowest fader value (0–127) that counts as "up" for NO_BASS. 13 is about 10 %.
const NO_BASS_FADER_MIN: u8 = 13;
/// The highest LOW EQ value (0–127, centre 64) that counts as "bass cut" for NO_BASS.
const NO_BASS_LOW_MAX: u8 = 16;

#[derive(Debug)]
pub struct DjState {
    pub devices: HashMap<u8, DeviceState>,
    pub master: MasterState,
    pub timing: TimingModel,
    pub bpm_smooth_window: usize,
    master_device_num: u8,
    pub prolink_seen: bool,
    pub link_peer_count: usize,
    source_mode: Source,
    last_link_master: Option<MasterState>,
    /// The mixer channels CH1 to CH4, from the MIDI input of the XDJ-AZ.
    pub mixer: [MixerChannel; 4],

    // Bounded observability: avoid per-packet TRACE spam.
    abspos_ingest_trace: LogThrottle,
    link_ingest_trace: LogThrottle,
    last_link_logged_beat_in_bar: Option<u8>,
}

impl DjState {
    pub fn new(smoothing_ms: u64) -> Self {
        let window = ((smoothing_ms as f64 / 200.0).round() as usize).max(1);
        Self {
            devices: HashMap::new(),
            master: MasterState::default(),
            timing: TimingModel::default(),
            bpm_smooth_window: window,
            master_device_num: 0,
            prolink_seen: false,
            link_peer_count: 0,
            source_mode: Source::Auto,
            last_link_master: None,
            mixer: [MixerChannel::default(); 4],

            abspos_ingest_trace: LogThrottle::default(),
            link_ingest_trace: LogThrottle::default(),
            last_link_logged_beat_in_bar: None,
        }
    }

    pub fn set_source_mode(&mut self, source_mode: Source) {
        if self.source_mode != source_mode {
            self.source_mode = source_mode;
            self.reconcile_source_mode();
        }
    }

    fn reconcile_source_mode(&mut self) {
        match self.source_mode {
            Source::Auto => {
                if self.link_peer_count > 0 {
                    if !self.apply_cached_link_master() {
                        self.refresh_master();
                    }
                } else {
                    self.refresh_master();
                }
            }
            Source::Link => {
                if !self.apply_cached_link_master() {
                    self.master = MasterState::default();
                }
            }
            Source::ProLink => self.refresh_master(),
        }
    }

    fn apply_cached_link_master(&mut self) -> bool {
        if let Some(link_master) = self.last_link_master.clone() {
            self.master = link_master;
            true
        } else {
            false
        }
    }

    pub fn set_smoothing_ms(&mut self, smoothing_ms: u64) {
        self.bpm_smooth_window = ((smoothing_ms as f64 / 200.0).round() as usize).max(1);
    }

    fn device_mut(&mut self, num: u8) -> &mut DeviceState {
        self.devices
            .entry(num)
            .or_insert_with(|| DeviceState::new(num))
    }

    pub fn apply_cdj_status(&mut self, s: &CdjStatus) -> Option<TrackChange> {
        let window = self.bpm_smooth_window;
        let dev = self.device_mut(s.device_number);
        dev.is_master = s.is_master;
        dev.is_playing = s.play_state.is_playing() || s.is_playing_flag;
        dev.is_on_air = s.is_on_air;
        dev.is_sync = s.is_sync;
        dev.play_state = s.play_state;
        // A paused deck reports its motor pitch, which is near -100 %. Keep the last real pitch.
        // ponytail: on a paused pre-3000 deck, a fader move shows only at play. Read 0x8c to fix.
        if dev.is_playing {
            dev.pitch_pct = s.pitch_pct;
        }
        if dev.beat_in_bar != s.beat_in_bar && s.beat_in_bar != 0 {
            dev.phrase_16_beat = dev.phrase_16_beat.wrapping_add(1) % 16;
        }
        if s.beat_count != u32::MAX && s.beat_count > 0 {
            dev.phrase_16_beat = ((s.beat_count - 1) % 16) as u8;
        }

        dev.beat_in_bar = s.beat_in_bar;
        dev.beat_count = s.beat_count;
        if s.effective_bpm > 0.0 {
            dev.effective_bpm = dev.smooth_bpm(s.effective_bpm, window);
        }
        if s.beat_in_bar >= 1 {
            dev.bar_phase = ((dev.phrase_16_beat as f64 + dev.beat_phase) / 16.0).clamp(0.0, 1.0);
        }

        let prev_rbid = dev.rekordbox_id;
        let prev_slot = dev.track_slot;
        let prev_type = dev.track_type;
        let prev_src = dev.track_source_player;

        dev.track_source_player = s.track_source_player;
        dev.track_slot = s.track_slot;
        dev.track_type = s.track_type;
        dev.rekordbox_id = s.rekordbox_id;

        let track_changed = prev_rbid != s.rekordbox_id
            || prev_slot != s.track_slot
            || prev_type != s.track_type
            || prev_src != s.track_source_player;

        let change = if track_changed {
            dev.track_title.clear();
            dev.track_artist.clear();
            dev.track_key.clear();
            dev.track_bpm_meta = None;
            dev.song_structure = None;
            dev.current_phrase_idx = None;
            dev.prev_phrase_idx = None;
            // An eject has rekordbox_id 0. It clears the metadata but loads no track.
            (s.rekordbox_id != 0).then(|| TrackChange {
                device_number: s.device_number,
                track_source_player: s.track_source_player,
                track_slot: s.track_slot,
                track_type: s.track_type,
                rekordbox_id: s.rekordbox_id,
            })
        } else {
            None
        };

        if s.is_master {
            if self.master_device_num != s.device_number {
                tracing::info!(
                    device = s.device_number,
                    prev_master = self.master_device_num,
                    "Master changed to device"
                );
            }
            self.master_device_num = s.device_number;
        } else if self.master_device_num == s.device_number {
            tracing::info!(device = s.device_number, "Device gave up master role");
            self.master_device_num = 0;
        }

        let phrase_changed = self.update_current_phrase(s.device_number);
        if phrase_changed {
            if let Some(dev) = self.devices.get(&s.device_number) {
                if let (Some(ss), Some(idx)) = (&dev.song_structure, dev.current_phrase_idx) {
                    if let Some(phrase) = ss.phrases.get(idx) {
                        tracing::debug!(
                            device = s.device_number,
                            phrase = %phrase.kind,
                            index = phrase.index,
                            beat = phrase.beat,
                            "Phrase changed"
                        );
                    }
                }
            }
        }

        {
            let dev = self.devices.get(&s.device_number).unwrap();
            tracing::debug!(
                device = dev.device_number,
                is_master = dev.is_master,
                is_playing = dev.is_playing,
                is_on_air = dev.is_on_air,
                is_sync = dev.is_sync,
                play_state = ?dev.play_state,
                effective_bpm = format!("{:.2}", dev.effective_bpm).as_str(),
                pitch_pct = format!("{:.2}", dev.pitch_pct).as_str(),
                beat_in_bar = dev.beat_in_bar,
                beat_count = dev.beat_count,
                bar_phase = format!("{:.3}", dev.bar_phase).as_str(),
                beat_phase = format!("{:.3}", dev.beat_phase).as_str(),
                rekordbox_id = dev.rekordbox_id,
                track_slot = dev.track_slot,
                track_type = dev.track_type,
                track_source_player = dev.track_source_player,
                track_title = dev.track_title.as_str(),
                track_artist = dev.track_artist.as_str(),
                track_key = dev.track_key.as_str(),
                has_song_structure = dev.song_structure.is_some(),
                current_phrase_idx = ?dev.current_phrase_idx,
                "CDJ status update"
            );
        }

        self.refresh_master();
        change
    }

    pub fn apply_mixer_status(&mut self, s: &MixerStatus) {
        let dev = self.device_mut(s.device_number);
        dev.is_master = s.is_master;
        dev.beat_in_bar = s.beat_in_bar;
        if let Some(b) = s.track_bpm {
            dev.effective_bpm = b;
        }
        if s.is_master {
            if self.master_device_num != s.device_number {
                tracing::info!(
                    mixer_device = s.device_number,
                    prev_master = self.master_device_num,
                    "Mixer became master"
                );
            }
            self.master_device_num = s.device_number;
        } else if self.master_device_num == s.device_number {
            tracing::info!(mixer_device = s.device_number, "Mixer gave up master role");
            self.master_device_num = 0;
        }
        self.refresh_master();
    }

    pub fn apply_beat(&mut self, bp: &BeatPacket, received_at: Instant) -> bool {
        let window = self.bpm_smooth_window;
        let dev = self.device_mut(bp.device_number);

        if dev.beat_in_bar != bp.beat_in_bar && bp.beat_in_bar != 0 {
            dev.phrase_16_beat = dev.phrase_16_beat.wrapping_add(1) % 16;
            if dev.beat_count != u32::MAX && dev.beat_count > 0 {
                dev.phrase_16_beat = ((dev.beat_count - 1) % 16) as u8;
            }
        }

        dev.beat_in_bar = bp.beat_in_bar;
        dev.last_beat_at = Some(received_at);
        if bp.effective_bpm > 0.0 {
            dev.effective_bpm = dev.smooth_bpm(bp.effective_bpm, window);
            if bp.next_beat_ms != BEAT_NONE {
                let beat_dur_ms = 60_000.0 / bp.effective_bpm;
                let time_into_beat = (beat_dur_ms - bp.next_beat_ms as f64).clamp(0.0, beat_dur_ms);
                dev.beat_phase = (time_into_beat / beat_dur_ms).clamp(0.0, 1.0);
                dev.bar_phase =
                    ((dev.phrase_16_beat as f64 + dev.beat_phase) / 16.0).clamp(0.0, 1.0);
            } else {
                // End-of-track/unknown upcoming beat sentinel must not propagate stale phase.
                dev.beat_phase = 0.0;
                dev.bar_phase = 0.0;
            }
        }
        dev.pitch_pct = bp.pitch_pct;

        let prev_master = self.master.device_number;

        let m = TimingMeasurement::from_prolink_beat(bp, received_at);
        if self.prolink_may_observe() {
            self.timing.observe(m.clone());
        }

        self.refresh_master();
        let master = self.master.device_number;
        let master_changed = prev_master != master;

        tracing::trace!(
            target: "timing.ingest",
            source = ?m.source,
            kind = ?m.kind,
            device = m.device_number,
            bpm = %format!("{:.2}", m.bpm),
            effective_bpm = %format!("{:.2}", m.effective_bpm),
            beat_in_bar = m.beat_in_bar,
            beat_phase = m.beat_phase.map(|v| format!("{v:.3}")),
            bar_phase = m.bar_phase.map(|v| format!("{v:.3}")),
            playhead_ms = m.playhead_ms,
            age_ms = 0u64,
            master,
            master_changed,
            "Timing measurement observed"
        );

        master == bp.device_number
    }

    pub fn apply_abs_position(&mut self, ap: &AbsPositionPacket, received_at: Instant) -> bool {
        let window = self.bpm_smooth_window;
        let dev = self.device_mut(ap.device_number);
        dev.playhead_ms = Some(ap.playhead_ms);
        dev.pitch_pct = ap.pitch_pct;
        if ap.effective_bpm > 0.0 {
            dev.effective_bpm = dev.smooth_bpm(ap.effective_bpm, window);
        }
        if ap.effective_bpm > 0.0 {
            let beat_dur = 60_000.0 / ap.effective_bpm;
            let within = (ap.playhead_ms as f64) % beat_dur;
            dev.beat_phase = (within / beat_dur).clamp(0.0, 1.0);
        }

        let prev_master = self.master.device_number;

        let m = TimingMeasurement::from_prolink_abs_position(ap, received_at);
        if self.prolink_may_observe() {
            self.timing.observe(m.clone());
        }

        self.refresh_master();
        let master = self.master.device_number;
        let master_changed = prev_master != master;

        let should_log = self
            .abspos_ingest_trace
            .should_log(received_at, std::time::Duration::from_secs(1))
            || master_changed;

        if should_log {
            tracing::trace!(
                target: "timing.ingest",
                source = ?m.source,
                kind = ?m.kind,
                device = m.device_number,
                bpm = %format!("{:.2}", m.bpm),
                effective_bpm = %format!("{:.2}", m.effective_bpm),
                beat_in_bar = m.beat_in_bar,
                beat_phase = m.beat_phase.map(|v| format!("{v:.3}")),
                bar_phase = m.bar_phase.map(|v| format!("{v:.3}")),
                playhead_ms = m.playhead_ms,
                age_ms = 0u64,
                master,
                master_changed,
                "Timing measurement observed"
            );
        }

        master == ap.device_number
    }

    /// ProLink must not overwrite the timing slot while Link is the active master source.
    fn prolink_may_observe(&self) -> bool {
        match self.source_mode {
            Source::Link => false,
            Source::ProLink => true,
            Source::Auto => {
                !(self.link_peer_count > 0
                    && self.master.source == Some(BeatSource::AbletonLink)
                    && self.master.bpm > 0.0)
            }
        }
    }

    fn refresh_master(&mut self) {
        if self.source_mode == Source::Link {
            return;
        }

        let link_active_with_peers = self.link_peer_count > 0
            && self.master.source == Some(BeatSource::AbletonLink)
            && self.master.bpm > 0.0;

        if self.source_mode == Source::Auto && link_active_with_peers {
            return;
        }

        let prev_master = self.master.device_number;

        let (dev, is_virtual, selection_reason) = if self.master_device_num != 0 {
            let reason = if let Some(d) = self.devices.get(&self.master_device_num) {
                format!(
                    "explicit is_master (device {} claims master, is_master={}, is_playing={}, bpm={:.2})",
                    d.device_number, d.is_master, d.is_playing, d.effective_bpm
                )
            } else {
                format!(
                    "explicit master_device_num={} but not in device table",
                    self.master_device_num
                )
            };
            (self.devices.get(&self.master_device_num), false, reason)
        } else {
            let mixer = self
                .devices
                .values()
                .filter(|d| d.device_number >= 16 && d.effective_bpm > 0.0)
                .min_by_key(|d| d.device_number);
            if let Some(d) = mixer {
                let reason = format!(
                    "fallback(mixer) → device {} (bpm={:.2})",
                    d.device_number, d.effective_bpm
                );
                (Some(d), true, reason)
            } else {
                let real_cdj = self
                    .devices
                    .values()
                    .filter(|d| d.device_number < 16 && d.effective_bpm > 0.0)
                    .min_by_key(|d| (!d.is_playing, d.device_number));
                if let Some(d) = real_cdj {
                    let reason = format!(
                        "fallback(cdj) → device {} (is_playing={}, bpm={:.2})",
                        d.device_number, d.is_playing, d.effective_bpm
                    );
                    (Some(d), true, reason)
                } else {
                    (None, false, "no devices with bpm>0".to_string())
                }
            }
        };

        if let Some(dev) = dev {
            if prev_master != dev.device_number || !self.master.source.is_some() {
                tracing::info!(
                    device = dev.device_number,
                    is_virtual_master = is_virtual,
                    reason = %selection_reason,
                    "Master selected"
                );
            }
            self.master = MasterState {
                device_number: dev.device_number,
                source: Some(BeatSource::ProLink),
                bpm: dev.effective_bpm,
                pitch_pct: dev.pitch_pct,
                beat_in_bar: dev.beat_in_bar,
                bar_phase: dev.bar_phase,
                beat_phase: dev.beat_phase,
                is_playing: if dev.device_number >= 16 {
                    mixer_is_playing(dev)
                } else {
                    dev.is_playing
                },
                last_beat_at: dev.last_beat_at,
                is_virtual_master: is_virtual,
                phrase_16_beat: dev.phrase_16_beat,
            };
        } else if self.master_device_num == 0 {
            if prev_master != 0 {
                tracing::info!("Master cleared (no devices with bpm>0)");
            }
            self.master = MasterState::default();
        }
    }

    pub fn apply_link_state(
        &mut self,
        bpm: f64,
        beat_in_bar: u8,
        bar_phase: f64,
        beat_phase: f64,
        is_playing: bool,
        beat_crossed: bool,
        received_at: Instant,
    ) {
        let prev_master = self.master.device_number;
        let link_master = MasterState {
            device_number: 0,
            source: Some(BeatSource::AbletonLink),
            bpm,
            pitch_pct: 0.0,
            beat_in_bar,
            bar_phase,
            beat_phase,
            is_playing,
            last_beat_at: if beat_crossed {
                Some(received_at)
            } else {
                self.master.last_beat_at
            },
            is_virtual_master: false,
            phrase_16_beat: 0,
        };

        self.last_link_master = Some(link_master.clone());

        // Bounded ingest TRACE (beat crossings and/or at most 1Hz), regardless of
        // whether Link becomes the active master under the current source mode.
        let has_link_peers = self.link_peer_count > 0;
        let prolink_active =
            self.master.source == Some(BeatSource::ProLink) && self.master.bpm > 0.0;

        let will_set_master = match self.source_mode {
            Source::ProLink => false,
            Source::Auto if prolink_active && !has_link_peers => false,
            _ => true,
        };

        let master_after = if will_set_master { 0 } else { prev_master };
        let master_changed = master_after != prev_master;
        let beat_edge = beat_crossed && self.last_link_logged_beat_in_bar != Some(beat_in_bar);
        let should_log = beat_edge
            || master_changed
            || self
                .link_ingest_trace
                .should_log(received_at, std::time::Duration::from_secs(1));

        if should_log {
            if beat_edge {
                self.last_link_logged_beat_in_bar = Some(beat_in_bar);
            }
            tracing::trace!(
                target: "timing.ingest",
                source = "AbletonLink",
                kind = "AbletonLink",
                device = "-",
                bpm = %format!("{bpm:.2}"),
                effective_bpm = %format!("{bpm:.2}"),
                beat_in_bar,
                beat_phase = %format!("{beat_phase:.3}"),
                bar_phase = %format!("{bar_phase:.3}"),
                playhead_ms = "-",
                age_ms = 0u64,
                playing = is_playing,
                beat_crossed,
                beat_crossed_edge = beat_edge,
                peers = self.link_peer_count,
                source_mode = ?self.source_mode,
                prolink_active,
                master_before = prev_master,
                master_after,
                master_changed,
                "Timing measurement observed"
            );
        }

        if self.source_mode == Source::ProLink {
            return;
        }

        if self.source_mode == Source::Auto && prolink_active && !has_link_peers {
            return;
        }

        // Link writes to the timing model only when it is the active master source.
        self.timing.observe(TimingMeasurement::from_link(
            bpm,
            beat_in_bar,
            bar_phase,
            beat_phase,
            is_playing,
            received_at,
        ));
        self.master = link_master;
    }

    pub fn remove_device(&mut self, num: u8) {
        self.devices.remove(&num);
        if self.master_device_num == num {
            self.master_device_num = 0;
        }
        // Reset only a ProLink master. A Link master does not depend on this device.
        if self.master.source == Some(BeatSource::ProLink) && self.master.device_number == num {
            self.master = MasterState::default();
            self.refresh_master();
        }
    }

    pub fn mark_prolink_seen(&mut self) {
        self.prolink_seen = true;
    }

    pub fn set_link_peer_count(&mut self, count: usize) {
        if self.link_peer_count != count {
            tracing::info!(
                old = self.link_peer_count,
                new = count,
                "Ableton Link peer count changed"
            );
        }
        self.link_peer_count = count;
        self.reconcile_source_mode();
    }

    pub fn set_track_metadata(
        &mut self,
        device_number: u8,
        rekordbox_id: u32,
        title: String,
        artist: String,
        key: String,
        bpm: Option<f64>,
    ) {
        if let Some(dev) = self.devices.get_mut(&device_number) {
            if dev.rekordbox_id == rekordbox_id {
                dev.track_title = title;
                dev.track_artist = artist;
                dev.track_key = key;
                dev.track_bpm_meta = bpm;
            }
        }
    }

    pub fn set_song_structure(
        &mut self,
        device_number: u8,
        rekordbox_id: u32,
        structure: SongStructure,
    ) {
        if let Some(dev) = self.devices.get_mut(&device_number) {
            if dev.rekordbox_id == rekordbox_id {
                dev.song_structure = Some(structure);
                dev.current_phrase_idx = None;
                dev.prev_phrase_idx = None;
            }
        }
    }

    /// Returns true when at least one mixer channel is live and every live channel has its
    /// bass cut. A channel is live when its deck plays and its fader is up.
    /// Mixer channel CHn takes its play state from player number n.
    pub fn no_bass(&self) -> bool {
        let mut live = self
            .mixer
            .iter()
            .enumerate()
            .filter(|&(i, ch)| {
                let Some(dev) = self.devices.get(&(i as u8 + 1)) else {
                    return false;
                };
                // Until the fader moves, the on-air flag of the player tells if the fader is up.
                let fader_up = ch.fader.map_or(dev.is_on_air, |v| v >= NO_BASS_FADER_MIN);
                dev.is_playing && fader_up
            })
            .peekable();
        live.peek().is_some() && live.all(|(_, ch)| ch.low.is_some_and(|v| v <= NO_BASS_LOW_MAX))
    }

    /// Returns the phrase that the master deck plays now.
    pub fn master_phrase(&self) -> Option<PhraseInfo> {
        self.devices.get(&self.master.device_number)?.phrase_info()
    }

    pub fn update_current_phrase(&mut self, device_number: u8) -> bool {
        let dev = match self.devices.get_mut(&device_number) {
            Some(d) => d,
            None => return false,
        };
        let beat_count = dev.beat_count;
        if beat_count == u32::MAX {
            return false;
        }
        let structure = match &dev.song_structure {
            Some(s) => s,
            None => return false,
        };
        let beat = u16::try_from(beat_count).unwrap_or(u16::MAX);

        let new_idx = structure
            .phrases
            .iter()
            .enumerate()
            .rev()
            .find(|(_, p)| p.beat <= beat)
            .map(|(i, _)| i);

        let changed = dev.current_phrase_idx != new_idx && new_idx.is_some();
        dev.prev_phrase_idx = dev.current_phrase_idx;
        dev.current_phrase_idx = new_idx;
        changed
    }
}

/// The mixer status packet has no play flag.
/// A mixer plays when its last beat is newer than two beat durations.
fn mixer_is_playing(dev: &DeviceState) -> bool {
    match (dev.last_beat_at, dev.effective_bpm > 0.0) {
        (Some(at), true) => at.elapsed().as_secs_f64() < 2.0 * 60.0 / dev.effective_bpm,
        _ => false,
    }
}

pub type SharedState = Arc<RwLock<DjState>>;

pub fn new_shared(smoothing_ms: u64) -> SharedState {
    Arc::new(RwLock::new(DjState::new(smoothing_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::beat_source::BeatSource;

    /// Sets mixer channel CHn (1-based) and the play state of player n.
    fn set_channel(st: &mut DjState, n: u8, playing: bool, fader: u8, low: u8) {
        st.device_mut(n).is_playing = playing;
        st.mixer[usize::from(n - 1)] = MixerChannel {
            fader: Some(fader),
            low: Some(low),
            ..Default::default()
        };
    }

    #[test]
    fn no_bass_needs_a_live_channel() {
        let mut st = DjState::new(30);
        assert!(!st.no_bass());
        // The LOW knob is at 0, but player 1 does not play and the CH2 fader is down.
        set_channel(&mut st, 1, false, 127, 0);
        set_channel(&mut st, 2, true, 0, 0);
        assert!(!st.no_bass());
    }

    #[test]
    fn no_bass_with_one_live_channel_follows_its_low_eq() {
        let mut st = DjState::new(30);
        set_channel(&mut st, 1, true, 127, 64);
        assert!(!st.no_bass());
        set_channel(&mut st, 1, true, 127, NO_BASS_LOW_MAX);
        assert!(st.no_bass());
        set_channel(&mut st, 1, true, 127, NO_BASS_LOW_MAX + 1);
        assert!(!st.no_bass());
    }

    #[test]
    fn no_bass_with_two_live_channels_needs_both_cut() {
        let mut st = DjState::new(30);
        set_channel(&mut st, 1, true, 127, 0);
        set_channel(&mut st, 3, true, 100, 64);
        assert!(!st.no_bass());
        set_channel(&mut st, 3, true, 100, 0);
        assert!(st.no_bass());
        // A channel that is not live does not block NO_BASS.
        set_channel(&mut st, 2, true, 0, 64);
        assert!(st.no_bass());
    }

    #[test]
    fn no_bass_uses_the_on_air_flag_until_the_fader_moves() {
        let mut st = DjState::new(30);
        set_channel(&mut st, 1, true, 127, 0);
        st.mixer[0].fader = None;
        assert!(!st.no_bass());
        st.device_mut(1).is_on_air = true;
        assert!(st.no_bass());
    }

    #[test]
    fn test_auto_mode_link_priority_with_peers() {
        let mut state = DjState::new(30);

        state.set_link_peer_count(2);
        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());

        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));
        assert_eq!(state.master.bpm, 120.0);
        assert!(state.master.is_playing);

        let beat = crate::prolink::packets::BeatPacket {
            device_number: 1,
            next_beat_ms: 500,
            second_beat_ms: 1000,
            next_bar_ms: 2000,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: 13000,
            beat_in_bar: 1,
            track_bpm: Some(130.0),
            effective_bpm: 130.0,
            pitch_pct: 0.0,
        };

        state.apply_beat(&beat, Instant::now());

        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));
        assert_eq!(state.master.bpm, 120.0);

        state.set_link_peer_count(0);
        state.apply_beat(&beat, Instant::now());

        assert_eq!(state.master.source, Some(BeatSource::ProLink));
        assert_eq!(state.master.bpm, 130.0);
    }

    #[test]
    fn test_link_without_peers_does_not_override_prolink() {
        let mut state = DjState::new(30);

        let beat = crate::prolink::packets::BeatPacket {
            device_number: 1,
            next_beat_ms: 500,
            second_beat_ms: 1000,
            next_bar_ms: 2000,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: 12500,
            beat_in_bar: 1,
            track_bpm: Some(125.0),
            effective_bpm: 125.0,
            pitch_pct: 0.0,
        };

        state.apply_beat(&beat, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::ProLink));
        assert_eq!(state.master.bpm, 125.0);

        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());

        assert_eq!(state.master.source, Some(BeatSource::ProLink));
        assert_eq!(state.master.bpm, 125.0);

        state.set_link_peer_count(1);
        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());

        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));
        assert_eq!(state.master.bpm, 120.0);
    }

    #[test]
    fn test_explicit_link_mode_suppresses_prolink() {
        let mut state = DjState::new(30);
        let beat = crate::prolink::packets::BeatPacket {
            device_number: 1,
            next_beat_ms: 500,
            second_beat_ms: 1000,
            next_bar_ms: 2000,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: 12800,
            beat_in_bar: 1,
            track_bpm: Some(128.0),
            effective_bpm: 128.0,
            pitch_pct: 0.0,
        };

        state.apply_beat(&beat, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::ProLink));

        state.set_source_mode(Source::Link);
        assert_eq!(state.master.source, None);

        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));

        state.apply_beat(&beat, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));
        assert_eq!(state.master.bpm, 120.0);
    }

    #[test]
    fn test_explicit_prolink_mode_suppresses_link() {
        let mut state = DjState::new(30);
        let beat = crate::prolink::packets::BeatPacket {
            device_number: 1,
            next_beat_ms: 500,
            second_beat_ms: 1000,
            next_bar_ms: 2000,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: 12600,
            beat_in_bar: 1,
            track_bpm: Some(126.0),
            effective_bpm: 126.0,
            pitch_pct: 0.0,
        };

        state.set_link_peer_count(2);
        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));

        state.set_source_mode(Source::ProLink);
        state.apply_beat(&beat, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::ProLink));

        state.apply_link_state(121.0, 2, 0.25, 0.5, true, true, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::ProLink));
        assert_eq!(state.master.bpm, 126.0);
    }

    #[test]
    fn beat_none_resets_master_phase_instead_of_leaking_previous_value() {
        let mut state = DjState::new(30);

        let normal = crate::prolink::packets::BeatPacket {
            device_number: 1,
            next_beat_ms: 250,
            second_beat_ms: 500,
            next_bar_ms: 1000,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: 12000,
            beat_in_bar: 2,
            track_bpm: Some(120.0),
            effective_bpm: 120.0,
            pitch_pct: 0.0,
        };
        state.apply_beat(&normal, Instant::now());
        assert!(state.master.beat_phase > 0.0);

        let none = crate::prolink::packets::BeatPacket {
            next_beat_ms: BEAT_NONE,
            ..normal
        };
        state.apply_beat(&none, Instant::now());

        assert_eq!(state.master.beat_phase, 0.0);
        assert_eq!(state.master.bar_phase, 0.0);
    }

    fn cdj(device: u8, playing: bool, bpm: f64, rekordbox_id: u32) -> CdjStatus {
        CdjStatus {
            device_number: device,
            play_state: if playing {
                crate::prolink::packets::PlayState::Playing
            } else {
                crate::prolink::packets::PlayState::Paused
            },
            is_master: false,
            is_sync: false,
            is_on_air: false,
            is_playing_flag: false,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: (bpm * 100.0) as u16,
            track_bpm: Some(bpm),
            effective_bpm: bpm,
            pitch_pct: 0.0,
            beat_count: u32::MAX,
            beat_in_bar: 1,
            track_source_player: 1,
            track_slot: 3,
            track_type: 1,
            rekordbox_id,
        }
    }

    fn beat_from(device: u8, bpm: f64) -> BeatPacket {
        BeatPacket {
            device_number: device,
            next_beat_ms: 250,
            second_beat_ms: 500,
            next_bar_ms: 1000,
            pitch_raw: crate::prolink::PITCH_NORMAL,
            bpm_raw: (bpm * 100.0) as u16,
            beat_in_bar: 1,
            track_bpm: Some(bpm),
            effective_bpm: bpm,
            pitch_pct: 0.0,
        }
    }

    #[test]
    fn fallback_master_prefers_a_playing_deck() {
        let mut state = DjState::new(30);
        state.apply_cdj_status(&cdj(1, false, 120.0, 10));
        state.apply_cdj_status(&cdj(2, true, 125.0, 20));

        assert_eq!(state.master.device_number, 2);
        assert!(state.master.is_playing);
    }

    #[test]
    fn removing_a_fallback_master_selects_a_new_master() {
        let mut state = DjState::new(30);
        state.apply_cdj_status(&cdj(1, false, 120.0, 10));
        state.apply_cdj_status(&cdj(2, true, 125.0, 20));
        assert_eq!(state.master.device_number, 2);

        state.remove_device(2);
        assert_eq!(state.master.device_number, 1);

        state.remove_device(1);
        assert_eq!(state.master.source, None);
    }

    #[test]
    fn removing_a_device_keeps_a_link_master() {
        let mut state = DjState::new(30);
        state.set_link_peer_count(1);
        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());
        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));
        state.master_device_num = 3;

        state.remove_device(3);

        assert_eq!(state.master_device_num, 0);
        assert_eq!(state.master.source, Some(BeatSource::AbletonLink));
    }

    #[test]
    fn link_does_not_overwrite_prolink_timing_without_peers() {
        let mut state = DjState::new(30);
        state.apply_beat(&beat_from(1, 125.0), Instant::now());
        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());

        let last = state.timing.last().unwrap();
        assert_eq!(last.device_number, Some(1));
        assert_eq!(last.bpm, 125.0);
    }

    #[test]
    fn prolink_does_not_overwrite_link_timing_in_link_mode() {
        let mut state = DjState::new(30);
        state.set_source_mode(Source::Link);
        state.apply_link_state(120.0, 1, 0.0, 0.5, true, true, Instant::now());
        state.apply_beat(&beat_from(1, 125.0), Instant::now());

        assert_eq!(state.timing.last().unwrap().bpm, 120.0);
    }

    #[test]
    fn mixer_master_plays_only_while_beats_are_recent() {
        let mut state = DjState::new(30);
        state.apply_mixer_status(&MixerStatus {
            device_number: 33,
            is_master: true,
            bpm_raw: 12000,
            track_bpm: Some(120.0),
            beat_in_bar: 1,
        });
        assert!(!state.master.is_playing);

        state.apply_beat(&beat_from(33, 120.0), Instant::now());
        assert!(state.master.is_playing);

        let old = Instant::now() - std::time::Duration::from_secs(5);
        state.apply_beat(&beat_from(33, 120.0), old);
        assert!(!state.master.is_playing);
    }

    #[test]
    fn paused_status_does_not_overwrite_pitch() {
        let mut state = DjState::new(30);
        let playing = CdjStatus {
            pitch_pct: -3.0,
            ..cdj(1, true, 120.0, 10)
        };
        state.apply_cdj_status(&playing);
        assert_eq!(state.master.pitch_pct, -3.0);

        let paused = CdjStatus {
            pitch_raw: 0,
            pitch_pct: -100.0,
            ..cdj(1, false, 120.0, 10)
        };
        state.apply_cdj_status(&paused);
        assert_eq!(state.master.pitch_pct, -3.0);
    }

    #[test]
    fn eject_clears_the_track_without_a_track_change() {
        let mut state = DjState::new(30);
        assert!(state.apply_cdj_status(&cdj(1, true, 120.0, 10)).is_some());

        assert!(state.apply_cdj_status(&cdj(1, false, 120.0, 0)).is_none());
        assert_eq!(state.devices[&1].rekordbox_id, 0);
    }
}
