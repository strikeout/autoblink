//! Folds the keepalive and beat packets into the deck and master state that a status page shows.
//!
//! The tracker has no clock of its own. The caller gives the time in milliseconds, so the same
//! code runs on the firmware and in the host tests.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{BeatPacket, KeepAlive, DEV_CDJ};

/// The tracker removes a deck when it sends no packet for this time.
pub const DECK_TIMEOUT_MS: u64 = 5_000;

/// The highest player number. Mixers and rekordbox use higher numbers.
const MAX_PLAYER: u8 = 6;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deck {
    pub num: u8,
    pub name: String,
    pub bpm: f64,
    pub pitch: f64,
    /// The beat in the bar, 1 to 4. 0 means that no beat came yet.
    pub beat: u8,
    /// The beat in the 16-beat phrase, 1 to 16. 0 means that no beat came yet.
    pub phrase16: u8,
    last_seen_ms: u64,
    last_beat_ms: Option<u64>,
}

impl Deck {
    /// Returns true when the next beat is not late by more than one beat period.
    pub fn is_playing(&self, now_ms: u64) -> bool {
        self.last_beat_ms
            .is_some_and(|t| now_ms.saturating_sub(t) as f64 <= 2.0 * self.period_ms())
    }

    /// The position in the current beat, 0.0 to 1.0. It stays at 1.0 when the next beat is late.
    pub fn beat_phase(&self, now_ms: u64) -> f64 {
        self.last_beat_ms.map_or(0.0, |t| {
            (now_ms.saturating_sub(t) as f64 / self.period_ms()).min(1.0)
        })
    }

    fn period_ms(&self) -> f64 {
        if self.bpm > 0.0 { 60_000.0 / self.bpm } else { 1_000.0 }
    }
}

#[derive(Debug, Default)]
pub struct Tracker {
    decks: Vec<Deck>,
    master: Option<u8>,
}

impl Tracker {
    pub const fn new() -> Self {
        Self { decks: Vec::new(), master: None }
    }

    pub fn on_keepalive(&mut self, ka: &KeepAlive, now_ms: u64) {
        if ka.device_type != DEV_CDJ || !(1..=MAX_PLAYER).contains(&ka.device_number) {
            return;
        }
        let deck = self.deck_mut(ka.device_number);
        deck.name = ka.name.clone();
        deck.last_seen_ms = now_ms;
    }

    /// Records a beat. Returns true when the beat comes from the master deck.
    ///
    /// The master stays the same while it plays. Another deck becomes the master only when the
    /// master stops. Two playing decks therefore do not make the master change on each beat.
    pub fn on_beat(&mut self, b: &BeatPacket, now_ms: u64) -> bool {
        if !(1..=MAX_PLAYER).contains(&b.device_number) {
            return false;
        }
        let master_plays = self
            .master
            .and_then(|n| self.decks.iter().find(|d| d.num == n))
            .is_some_and(|d| d.is_playing(now_ms));
        if !master_plays {
            self.master = Some(b.device_number);
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

        self.master == Some(b.device_number)
    }

    /// Removes the decks that sent no packet for `DECK_TIMEOUT_MS`.
    pub fn expire(&mut self, now_ms: u64) {
        self.decks
            .retain(|d| now_ms.saturating_sub(d.last_seen_ms) <= DECK_TIMEOUT_MS);
        if self.master.is_some_and(|n| !self.decks.iter().any(|d| d.num == n)) {
            self.master = None;
        }
    }

    pub fn decks(&self) -> &[Deck] {
        &self.decks
    }

    pub fn master(&self) -> Option<&Deck> {
        self.master.and_then(|n| self.decks.iter().find(|d| d.num == n))
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
        assert_eq!(t.decks().len(), 1);
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
}
