//! Folds the Pro DJ Link packets into the deck and master state that a status page shows.
//!
//! The tracker reads the keepalive, beat, absolute position and CDJ status packets. The CDJ
//! status packets come only to a device that joins the network as a virtual CDJ. Without them,
//! the tracker uses the beats: a deck plays while its beats arrive.
//!
//! The tracker has no clock of its own. The caller gives the time in milliseconds, so the same
//! code runs on the firmware and in the host tests.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{AbsPositionPacket, BeatPacket, CdjStatus, KeepAlive, PlayState, DEV_CDJ};

/// The tracker removes a deck when it sends no packet for this time.
pub const DECK_TIMEOUT_MS: u64 = 5_000;

/// The status of a deck is current for this time. After it, the tracker uses the beats.
const STATUS_TIMEOUT_MS: u64 = 2_000;

/// The highest player number. Mixers and rekordbox use higher numbers.
const MAX_PLAYER: u8 = 6;

/// The track that a deck loaded. A `rekordbox_id` of 0 means no track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackRef {
    pub device: u8,
    pub source_player: u8,
    pub slot: u8,
    pub track_type: u8,
    pub rekordbox_id: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deck {
    pub num: u8,
    pub name: String,
    pub ip: [u8; 4],
    pub bpm: f64,
    pub pitch: f64,
    /// The beat in the bar, 1 to 4. 0 means that no beat came yet.
    pub beat: u8,
    /// The beat in the 16-beat phrase, 1 to 16. 0 means that no beat came yet.
    pub phrase16: u8,
    /// The fields below come from the CDJ status packets.
    pub play_state: Option<PlayState>,
    pub on_air: bool,
    pub sync: bool,
    /// The beat number in the track. `None` when the player does not know it.
    pub beat_count: Option<u32>,
    pub track: TrackRef,
    pub loop_ms: Option<(u32, u32)>,
    /// The fields below come from the absolute position packets.
    pub playhead_ms: Option<u32>,
    pub track_len_ms: Option<u32>,
    status_playing: bool,
    last_status_ms: Option<u64>,
    last_seen_ms: u64,
    last_beat_ms: Option<u64>,
}

impl Deck {
    /// Returns true when the deck plays. A current CDJ status decides. Without one, the deck
    /// plays when the next beat is not late by more than one beat period.
    pub fn is_playing(&self, now_ms: u64) -> bool {
        if self.has_status(now_ms) {
            return self.status_playing;
        }
        self.last_beat_ms
            .is_some_and(|t| now_ms.saturating_sub(t) as f64 <= 2.0 * self.period_ms())
    }

    /// The position in the current beat, 0.0 to 1.0. It stays at 1.0 when the next beat is late.
    pub fn beat_phase(&self, now_ms: u64) -> f64 {
        self.last_beat_ms.map_or(0.0, |t| {
            (now_ms.saturating_sub(t) as f64 / self.period_ms()).min(1.0)
        })
    }

    fn has_status(&self, now_ms: u64) -> bool {
        self.last_status_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= STATUS_TIMEOUT_MS)
    }

    fn period_ms(&self) -> f64 {
        if self.bpm > 0.0 { 60_000.0 / self.bpm } else { 1_000.0 }
    }
}

/// A device on the network that is not a deck, for example rekordbox or a mixer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Peer {
    pub num: u8,
    pub name: String,
    pub ip: [u8; 4],
    last_seen_ms: u64,
}

#[derive(Debug, Default)]
pub struct Tracker {
    decks: Vec<Deck>,
    /// The other devices. A track from rekordbox has a source player above 15.
    peers: Vec<Peer>,
    /// The master from the beats: the first deck that beats, until it stops.
    beat_master: Option<u8>,
    /// The deck whose CDJ status has the master flag.
    flag_master: Option<u8>,
    /// The device number of this device. The tracker ignores its own packets.
    own: u8,
}

impl Tracker {
    pub const fn new() -> Self {
        Self { decks: Vec::new(), peers: Vec::new(), beat_master: None, flag_master: None, own: 0 }
    }

    /// Sets the device number of this device, so that the tracker ignores its own packets.
    pub fn set_own_device(&mut self, num: u8) {
        self.own = num;
        self.decks.retain(|d| d.num != num);
    }

    fn is_player(&self, num: u8) -> bool {
        (1..=MAX_PLAYER).contains(&num) && num != self.own
    }

    pub fn on_keepalive(&mut self, ka: &KeepAlive, now_ms: u64) {
        if ka.device_number == self.own {
            return;
        }
        if ka.device_type != DEV_CDJ || !self.is_player(ka.device_number) {
            let peer = Peer { num: ka.device_number, name: ka.name.clone(), ip: ka.ip, last_seen_ms: now_ms };
            match self.peers.iter_mut().find(|p| p.num == ka.device_number) {
                Some(p) => *p = peer,
                None => self.peers.push(peer),
            }
            return;
        }
        let deck = self.deck_mut(ka.device_number);
        deck.name = ka.name.clone();
        deck.ip = ka.ip;
        deck.last_seen_ms = now_ms;
    }

    /// Records a beat. Returns true when the beat comes from the master deck.
    ///
    /// The master flag of the CDJ status decides the master. Without it, the master stays the
    /// same while it plays. Another deck becomes the master only when the master stops. Two
    /// playing decks therefore do not make the master change on each beat.
    pub fn on_beat(&mut self, b: &BeatPacket, now_ms: u64) -> bool {
        if !self.is_player(b.device_number) {
            return false;
        }
        let master_plays = self
            .beat_master
            .and_then(|n| self.decks.iter().find(|d| d.num == n))
            .is_some_and(|d| d.is_playing(now_ms));
        if !master_plays {
            self.beat_master = Some(b.device_number);
        }

        let deck = self.deck_mut(b.device_number);
        deck.bpm = b.effective_bpm;
        deck.pitch = b.pitch_pct;
        deck.last_seen_ms = now_ms;
        deck.last_beat_ms = Some(now_ms);
        let mut next = deck.phrase16 % 16 + 1;
        if (1..=4).contains(&b.beat_in_bar) {
            deck.beat = b.beat_in_bar;
            // Keep the phrase position in line with the beat in the bar.
            next = (next - 1) / 4 * 4 + b.beat_in_bar;
        } else {
            deck.beat = (next - 1) % 4 + 1;
        }
        deck.phrase16 = next;

        self.master_num() == Some(b.device_number)
    }

    /// Records a CDJ status. Returns the new track when the loaded track changed. An eject gives
    /// a track with `rekordbox_id` 0.
    pub fn on_cdj_status(&mut self, st: &CdjStatus, now_ms: u64) -> Option<TrackRef> {
        if !self.is_player(st.device_number) {
            return None;
        }
        if st.is_master {
            self.flag_master = Some(st.device_number);
        } else if self.flag_master == Some(st.device_number) {
            self.flag_master = None;
        }

        let deck = self.deck_mut(st.device_number);
        deck.last_seen_ms = now_ms;
        deck.last_status_ms = Some(now_ms);
        deck.play_state = Some(st.play_state);
        deck.status_playing = st.play_state.is_playing() || st.is_playing_flag;
        deck.on_air = st.is_on_air;
        deck.sync = st.is_sync;
        deck.loop_ms = st.loop_ms;
        if st.effective_bpm > 0.0 {
            deck.bpm = st.effective_bpm;
        }
        if deck.status_playing {
            deck.pitch = st.pitch_pct;
        }
        deck.beat_count = (st.beat_count != u32::MAX && st.beat_count != 0).then_some(st.beat_count);
        if let Some(count) = deck.beat_count {
            // Like the host: the phrase position follows the beat number of the track.
            deck.phrase16 = ((count - 1) % 16 + 1) as u8;
        }
        if (1..=4).contains(&st.beat_in_bar) {
            deck.beat = st.beat_in_bar;
        }

        let track = TrackRef {
            device: st.device_number,
            source_player: st.track_source_player,
            slot: st.track_slot,
            track_type: st.track_type,
            rekordbox_id: st.rekordbox_id,
        };
        if track == deck.track {
            return None;
        }
        deck.track = track;
        deck.track_len_ms = None;
        deck.playhead_ms = None;
        Some(track)
    }

    /// Records an absolute position packet: the playhead and the track length.
    pub fn on_abs_position(&mut self, p: &AbsPositionPacket, now_ms: u64) {
        if !self.is_player(p.device_number) {
            return;
        }
        let deck = self.deck_mut(p.device_number);
        deck.last_seen_ms = now_ms;
        deck.playhead_ms = Some(p.playhead_ms);
        if deck.track_len_ms.is_none() && p.track_length_s > 0 {
            deck.track_len_ms = Some(p.track_length_s.saturating_mul(1000));
        }
        if p.effective_bpm > 0.0 {
            deck.bpm = p.effective_bpm;
        }
        deck.pitch = p.pitch_pct;
    }

    /// Sets the track length from the metadata. It replaces the length from the position packets.
    pub fn set_track_length(&mut self, device: u8, rekordbox_id: u32, secs: u32) {
        if secs == 0 {
            return;
        }
        if let Some(d) = self.decks.iter_mut().find(|d| d.num == device && d.track.rekordbox_id == rekordbox_id) {
            d.track_len_ms = Some(secs.saturating_mul(1000));
        }
    }

    /// Removes the decks that sent no packet for `DECK_TIMEOUT_MS`.
    pub fn expire(&mut self, now_ms: u64) {
        self.decks
            .retain(|d| now_ms.saturating_sub(d.last_seen_ms) <= DECK_TIMEOUT_MS);
        self.peers
            .retain(|p| now_ms.saturating_sub(p.last_seen_ms) <= DECK_TIMEOUT_MS);
        let present = |m: Option<u8>, decks: &[Deck]| m.filter(|n| decks.iter().any(|d| d.num == *n));
        self.beat_master = present(self.beat_master, &self.decks);
        self.flag_master = present(self.flag_master, &self.decks);
    }

    pub fn decks(&self) -> &[Deck] {
        &self.decks
    }

    pub fn deck(&self, num: u8) -> Option<&Deck> {
        self.decks.iter().find(|d| d.num == num)
    }

    /// The name and the IP address of any device on the network: a deck or another device.
    pub fn device_address(&self, num: u8) -> Option<(&str, [u8; 4])> {
        self.deck(num)
            .map(|d| (d.name.as_str(), d.ip))
            .or_else(|| self.peers.iter().find(|p| p.num == num).map(|p| (p.name.as_str(), p.ip)))
    }

    pub fn master(&self) -> Option<&Deck> {
        self.master_num().and_then(|n| self.deck(n))
    }

    /// True when no deck has the master flag, so the tracker chose the master from the beats.
    pub fn master_is_virtual(&self) -> bool {
        self.flag_master.is_none()
    }

    fn master_num(&self) -> Option<u8> {
        self.flag_master.or(self.beat_master)
    }

    fn deck_mut(&mut self, num: u8) -> &mut Deck {
        let i = match self.decks.iter().position(|d| d.num == num) {
            Some(i) => i,
            None => {
                self.decks.push(Deck { num, ..Deck::default() });
                self.decks.sort_by_key(|d| d.num);
                self.decks.iter().position(|d| d.num == num).unwrap()
            }
        };
        &mut self.decks[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BEAT_NONE, PITCH_NORMAL};
    use alloc::string::ToString;

    fn beat(num: u8, beat_in_bar: u8, bpm: f64) -> BeatPacket {
        BeatPacket {
            device_number: num,
            next_beat_ms: 500,
            second_beat_ms: 1000,
            next_bar_ms: BEAT_NONE,
            pitch_raw: PITCH_NORMAL,
            bpm_raw: (bpm * 100.0) as u16,
            beat_in_bar,
            track_bpm: Some(bpm),
            effective_bpm: bpm,
            pitch_pct: 0.0,
        }
    }

    fn status(num: u8, master: bool, play: PlayState, rbid: u32) -> CdjStatus {
        CdjStatus {
            device_number: num,
            play_state: play,
            is_master: master,
            is_sync: false,
            is_on_air: true,
            is_playing_flag: false,
            pitch_raw: PITCH_NORMAL,
            bpm_raw: 12_800,
            track_bpm: Some(128.0),
            effective_bpm: 128.0,
            pitch_pct: 0.0,
            beat_count: 37,
            beat_in_bar: 1,
            track_source_player: num,
            track_slot: 3,
            track_type: 1,
            rekordbox_id: rbid,
            loop_ms: None,
        }
    }

    fn keepalive(num: u8, name: &str) -> KeepAlive {
        KeepAlive {
            device_number: num,
            device_type: DEV_CDJ,
            name: name.into(),
            mac: [0; 6],
            ip: [0; 4],
            peer_count: 1,
        }
    }

    #[test]
    fn first_beat_makes_the_master() {
        let mut t = Tracker::new();
        assert!(t.on_beat(&beat(2, 1, 120.0), 0));
        let m = t.master().unwrap();
        assert_eq!((m.num, m.beat, m.phrase16), (2, 1, 1));
        assert!(m.is_playing(0));
        assert_eq!(m.beat_phase(250), 0.5);
        assert_eq!(m.beat_phase(900), 1.0);
    }

    #[test]
    fn master_stays_while_it_plays() {
        let mut t = Tracker::new();
        assert!(t.on_beat(&beat(1, 1, 120.0), 0));
        assert!(!t.on_beat(&beat(2, 1, 128.0), 100));
        assert!(t.on_beat(&beat(1, 2, 120.0), 500));
        assert_eq!(t.master().unwrap().num, 1);
    }

    #[test]
    fn master_moves_when_it_stops() {
        let mut t = Tracker::new();
        t.on_beat(&beat(1, 1, 120.0), 0);
        // Deck 1 plays at 120 BPM, so it counts as stopped after 1000 ms.
        assert!(t.on_beat(&beat(2, 3, 128.0), 1_500));
        assert_eq!(t.master().unwrap().num, 2);
        assert!(!t.decks()[0].is_playing(1_500));
    }

    #[test]
    fn phrase_counts_to_16_and_follows_the_bar() {
        let mut t = Tracker::new();
        let mut now = 0;
        for i in 0..16u8 {
            t.on_beat(&beat(1, i % 4 + 1, 120.0), now);
            assert_eq!(t.master().unwrap().phrase16, i + 1);
            now += 500;
        }
        t.on_beat(&beat(1, 1, 120.0), now);
        assert_eq!(t.master().unwrap().phrase16, 1);
        // A jump in the track moves the phrase to the new beat in the bar.
        t.on_beat(&beat(1, 4, 120.0), now + 500);
        assert_eq!(t.master().unwrap().phrase16, 4);
    }

    #[test]
    fn keepalive_names_the_deck_and_skips_other_devices() {
        let mut t = Tracker::new();
        t.on_keepalive(&keepalive(3, "XDJ-AZ"), 0);
        t.on_keepalive(&KeepAlive { device_type: 1, ..keepalive(0x21, "DJM") }, 0);
        t.on_keepalive(&KeepAlive { device_type: 3, ip: [10, 0, 0, 9], ..keepalive(17, "rekordbox") }, 0);
        assert_eq!(t.decks().len(), 1);
        assert_eq!(t.device_address(17), Some(("rekordbox", [10, 0, 0, 9])));
        assert_eq!(t.decks()[0].name, "XDJ-AZ");
        assert!(!t.decks()[0].is_playing(0));
    }

    #[test]
    fn silent_decks_expire_and_clear_the_master() {
        let mut t = Tracker::new();
        t.on_beat(&beat(1, 1, 120.0), 0);
        t.expire(DECK_TIMEOUT_MS);
        assert!(t.master().is_some());
        t.expire(DECK_TIMEOUT_MS + 1);
        assert!(t.master().is_none());
        assert!(t.decks().is_empty());
    }

    #[test]
    fn the_master_flag_beats_the_beat_master() {
        let mut t = Tracker::new();
        t.on_beat(&beat(1, 1, 120.0), 0);
        t.on_cdj_status(&status(2, true, PlayState::Playing, 5), 10);
        assert_eq!(t.master().unwrap().num, 2);
        assert!(!t.master_is_virtual());
        assert!(!t.on_beat(&beat(1, 2, 120.0), 500));
        t.on_cdj_status(&status(2, false, PlayState::Paused, 5), 600);
        assert_eq!(t.master().unwrap().num, 1);
        assert!(t.master_is_virtual());
    }

    #[test]
    fn status_reports_a_track_change_once() {
        let mut t = Tracker::new();
        let tr = t.on_cdj_status(&status(2, false, PlayState::Playing, 207), 0).unwrap();
        assert_eq!((tr.device, tr.slot, tr.track_type, tr.rekordbox_id), (2, 3, 1, 207));
        assert!(t.on_cdj_status(&status(2, false, PlayState::Playing, 207), 200).is_none());
        let eject = t.on_cdj_status(&status(2, false, PlayState::NoTrack, 0), 400).unwrap();
        assert_eq!(eject.rekordbox_id, 0);
    }

    #[test]
    fn status_decides_playing_and_the_phrase_position() {
        let mut t = Tracker::new();
        t.on_cdj_status(&status(2, false, PlayState::Paused, 1), 0);
        let d = t.deck(2).unwrap();
        assert!(!d.is_playing(0));
        // Beat 37 is the 5th beat of the third 16-beat block.
        assert_eq!((d.phrase16, d.beat_count), (5, Some(37)));
        assert_eq!(d.play_state.unwrap().to_string(), "Paused");
        // When the status stops, the beats decide again.
        t.on_beat(&beat(2, 1, 120.0), 3_000);
        assert!(t.deck(2).unwrap().is_playing(3_000));
    }

    #[test]
    fn position_sets_the_playhead_and_the_length() {
        let mut t = Tracker::new();
        let p = AbsPositionPacket {
            device_number: 3,
            track_length_s: 300,
            playhead_ms: 12_345,
            pitch_raw_signed: 0,
            bpm_x10: 1280,
            effective_bpm: 128.0,
            pitch_pct: 0.0,
        };
        t.on_abs_position(&p, 0);
        let d = t.deck(3).unwrap();
        assert_eq!((d.playhead_ms, d.track_len_ms), (Some(12_345), Some(300_000)));
        t.set_track_length(3, 0, 299);
        assert_eq!(t.deck(3).unwrap().track_len_ms, Some(299_000));
    }

    #[test]
    fn own_packets_are_ignored() {
        let mut t = Tracker::new();
        t.set_own_device(5);
        t.on_keepalive(&keepalive(5, "xdj-clock"), 0);
        assert!(!t.on_beat(&beat(5, 1, 120.0), 0));
        assert!(t.decks().is_empty());
    }
}
