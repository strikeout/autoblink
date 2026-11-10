//! The last channel changes of a DMX frame, for the page.
//!
//! The page shows them in the output status, like the last MIDI notes and CCs.

use crate::render::Universe;
use crate::show::UNIVERSE;

/// The number of changes that [`Changes`] keeps.
pub const RECENT: usize = 5;
/// The page shows a change for this time, like the MIDI history.
pub const RECENT_MS: u64 = 3_000;

/// The last changed channels of a frame sequence. Each channel has one entry at most.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    /// (channel 1 to 512, value, time in ms), newest first.
    items: [(u16, u8, u64); RECENT],
    len: usize,
}

impl Changes {
    /// Records the channels of `now` that differ from `prev`, then copies `now` into `prev`.
    /// The lowest changed channel of the frame becomes the newest entry.
    pub fn frame(&mut self, prev: &mut Universe, now: &Universe, at_ms: u64) {
        for i in (0..UNIVERSE).rev() {
            if prev[i] != now[i] {
                self.push((i as u16 + 1, now[i], at_ms));
            }
        }
        prev.copy_from_slice(now);
    }

    fn push(&mut self, entry: (u16, u8, u64)) {
        // A newer change of a channel replaces its older entry.
        let end = match self.items[..self.len].iter().position(|e| e.0 == entry.0) {
            Some(k) => k,
            None => {
                self.len = (self.len + 1).min(RECENT);
                self.len - 1
            }
        };
        self.items.copy_within(0..end, 1);
        self.items[0] = entry;
    }

    /// The channel and the value of the newest entry. `None` before the first change.
    pub fn newest(&self) -> Option<(u16, u8)> {
        self.items[..self.len].first().map(|e| (e.0, e.1))
    }

    /// Writes the entries of the last [`RECENT_MS`] into `out`, newest first, as
    /// (channel, value, age in ms).
    pub fn recent(&self, now_ms: u64, out: &mut Vec<(u16, u8, u32)>) {
        out.clear();
        out.extend(self.items[..self.len].iter().filter_map(|e| {
            let age = now_ms.saturating_sub(e.2);
            (age < RECENT_MS).then_some((e.0, e.1, age as u32))
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn universe(set: &[(usize, u8)]) -> Universe {
        let mut u = [0; UNIVERSE];
        for &(ch, v) in set {
            u[ch - 1] = v;
        }
        u
    }

    fn recent(c: &Changes, now_ms: u64) -> Vec<(u16, u8, u32)> {
        let mut out = Vec::new();
        c.recent(now_ms, &mut out);
        out
    }

    #[test]
    fn records_a_change_and_copies_the_frame() {
        let mut c = Changes::default();
        let mut prev = [0; UNIVERSE];
        let now = universe(&[(12, 255)]);
        c.frame(&mut prev, &now, 1_000);
        assert_eq!(prev, now);
        assert_eq!(c.newest(), Some((12, 255)));
        assert_eq!(recent(&c, 1_100), vec![(12, 255, 100)]);
        // The same frame again is no change.
        c.frame(&mut prev, &now, 1_200);
        assert_eq!(recent(&c, 1_200), vec![(12, 255, 200)]);
    }

    #[test]
    fn the_lowest_channel_of_a_frame_comes_first() {
        let mut c = Changes::default();
        let mut prev = [0; UNIVERSE];
        c.frame(&mut prev, &universe(&[(3, 1), (1, 2), (9, 3)]), 0);
        assert_eq!(c.newest(), Some((1, 2)));
        assert_eq!(recent(&c, 0), vec![(1, 2, 0), (3, 1, 0), (9, 3, 0)]);
    }

    #[test]
    fn keeps_one_entry_for_each_channel() {
        let mut c = Changes::default();
        let mut prev = [0; UNIVERSE];
        c.frame(&mut prev, &universe(&[(1, 10), (2, 10)]), 0);
        c.frame(&mut prev, &universe(&[(1, 10), (2, 20)]), 10);
        assert_eq!(recent(&c, 10), vec![(2, 20, 0), (1, 10, 10)]);
    }

    #[test]
    fn keeps_at_most_five_entries() {
        let mut c = Changes::default();
        let mut prev = [0; UNIVERSE];
        let all: Vec<(usize, u8)> = (1..=UNIVERSE).map(|ch| (ch, 1)).collect();
        c.frame(&mut prev, &universe(&all), 0);
        let got: Vec<u16> = recent(&c, 0).iter().map(|e| e.0).collect();
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn an_entry_goes_after_three_seconds() {
        let mut c = Changes::default();
        let mut prev = [0; UNIVERSE];
        c.frame(&mut prev, &universe(&[(5, 40)]), 1_000);
        assert_eq!(recent(&c, 1_000 + RECENT_MS - 1).len(), 1);
        assert!(recent(&c, 1_000 + RECENT_MS).is_empty());
        // The newest entry stays for the display.
        assert_eq!(c.newest(), Some((5, 40)));
    }
}
