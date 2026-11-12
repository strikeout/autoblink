//! Decides when the mic is the master.

use crate::tracker::MicState;

/// Keeps whether the mic is the master.
#[derive(Clone, Copy, Debug, Default)]
pub struct Arbiter {
    mic: bool,
}

impl Arbiter {
    /// A Pro DJ Link deck that plays always wins. The mic takes over only from `Locked`. The
    /// flywheel only keeps a mic master that took over already.
    pub fn update(&mut self, deck_playing: bool, mic: MicState) -> bool {
        self.mic = !deck_playing
            && match mic {
                MicState::Locked => true,
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

    #[test]
    fn a_playing_deck_wins_at_once() {
        let mut a = Arbiter::default();
        assert!(a.update(false, MicState::Locked));
        assert!(!a.update(true, MicState::Locked));
        assert!(!a.is_mic());
    }

    #[test]
    fn only_a_lock_takes_over() {
        let mut a = Arbiter::default();
        assert!(!a.update(false, MicState::Flywheel));
    }

    #[test]
    fn the_flywheel_keeps_a_mic_master() {
        let mut a = Arbiter::default();
        assert!(a.update(false, MicState::Locked));
        assert!(a.update(false, MicState::Flywheel));
        assert!(!a.update(false, MicState::Listening));
        assert!(!a.update(false, MicState::Flywheel));
    }

    #[test]
    fn no_mic_never_takes_over() {
        let mut a = Arbiter::default();
        assert!(!a.update(false, MicState::NoMic));
        assert!(!a.update(false, MicState::Listening));
    }
}
