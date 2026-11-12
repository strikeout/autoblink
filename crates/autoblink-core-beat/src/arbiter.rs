//! Decides when the mic is the master.

use crate::tracker::MicState;

/// After a deck stops, the mic takes over only with a kick that comes this long after the stop,
/// in µs. The lock of the mic holds the kicks of the deck for 8 beats after the deck stops.
pub const TAKEOVER_AFTER_US: u64 = 500_000;

/// Keeps whether the mic is the master.
#[derive(Clone, Copy, Debug, Default)]
pub struct Arbiter {
    mic: bool,
    deck_was_playing: bool,
    /// The time at which the last Pro DJ Link deck stopped to play, in µs.
    deck_stop_us: Option<u64>,
}

impl Arbiter {
    /// A Pro DJ Link deck that plays always wins. The mic takes over only from `Locked`, and
    /// only with a kick after the deck stopped (`last_kick_us`). Thus a pause of the deck in a
    /// silent room does not give the beat to the mic. The flywheel only keeps a mic master that
    /// took over already.
    pub fn update(
        &mut self,
        deck_playing: bool,
        mic: MicState,
        now_us: u64,
        last_kick_us: Option<u64>,
    ) -> bool {
        if deck_playing {
            self.deck_was_playing = true;
            self.mic = false;
            return false;
        }
        if self.deck_was_playing {
            self.deck_was_playing = false;
            self.deck_stop_us = Some(now_us);
        }
        let new_kick = self
            .deck_stop_us
            .is_none_or(|stop| last_kick_us.is_some_and(|k| k > stop + TAKEOVER_AFTER_US));
        self.mic = match mic {
            MicState::Locked => self.mic || new_kick,
            MicState::Flywheel => self.mic,
            MicState::Listening | MicState::NoMic => false,
        };
        self.mic
    }

    pub fn is_mic(&self) -> bool {
        self.mic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::MicState;

    #[test]
    fn a_playing_deck_wins_at_once() {
        let mut a = Arbiter::default();
        assert!(a.update(false, MicState::Locked, 0, None));
        assert!(!a.update(true, MicState::Locked, 10, Some(5)));
        assert!(!a.is_mic());
    }

    #[test]
    fn only_a_lock_takes_over() {
        let mut a = Arbiter::default();
        assert!(!a.update(false, MicState::Flywheel, 0, None));
    }

    #[test]
    fn the_flywheel_keeps_a_mic_master() {
        let mut a = Arbiter::default();
        assert!(a.update(false, MicState::Locked, 0, None));
        assert!(a.update(false, MicState::Flywheel, 1, None));
        assert!(!a.update(false, MicState::Listening, 2, None));
        assert!(!a.update(false, MicState::Flywheel, 3, None));
    }

    #[test]
    fn no_mic_never_takes_over() {
        let mut a = Arbiter::default();
        assert!(!a.update(false, MicState::NoMic, 0, None));
        assert!(!a.update(false, MicState::Listening, 1, None));
    }

    #[test]
    fn a_paused_deck_without_a_new_kick_keeps_the_mic_off() {
        let mut a = Arbiter::default();
        assert!(!a.update(true, MicState::Locked, 0, Some(0)));
        // The deck stops at 1 s. The lock of the mic still holds the kicks of the deck.
        assert!(!a.update(false, MicState::Locked, 1_000_000, Some(900_000)));
        assert!(!a.update(false, MicState::Locked, 2_000_000, Some(1_200_000)));
    }

    #[test]
    fn a_kick_after_the_deck_stops_takes_over() {
        let mut a = Arbiter::default();
        assert!(!a.update(true, MicState::Locked, 0, Some(0)));
        assert!(!a.update(false, MicState::Locked, 1_000_000, Some(900_000)));
        assert!(a.update(false, MicState::Locked, 2_500_000, Some(1_600_000)));
    }
}
