//! The merge of the engine frame and the frame of the outside source (`crate::Source`).
//!
//! The first frame of a source claims every non-zero channel. The `dmx` thread calls `rearm`
//! when the source comes back after the loss time, or when a new sender takes over. A channel
//! that the desk holds at 0 stays with the engine.
//!
//! HTP (the higher value wins) applies to the intensity and colour channels.
//! LTP (the source that changed the channel last wins) applies to all other channels.

use crate::render::{fixture_channels, Universe};
use crate::show::{Channel, Show, UNIVERSE};

pub struct Merge {
    /// True when Art-Net owns the channel (LTP).
    art_owns: [bool; UNIVERSE],
    prev_engine: Universe,
    prev_artnet: Universe,
}

impl Default for Merge {
    fn default() -> Self {
        Self {
            art_owns: [false; UNIVERSE],
            prev_engine: [0; UNIVERSE],
            prev_artnet: [0; UNIVERSE],
        }
    }
}

fn is_htp(c: Channel) -> bool {
    use Channel::*;
    matches!(c, Dimmer | Red | Green | Blue | White | Amber | Uv | Strobe)
}

impl Merge {
    /// Makes the next frame of the source claim every non-zero channel.
    pub fn rearm(&mut self) {
        self.prev_artnet = [0; UNIVERSE];
    }

    /// Merges `artnet` (the output frame of the source) into `engine` in place, by the kinds of
    /// `show`. `None` is no source.
    pub fn apply(&mut self, show: &Show, artnet: Option<&Universe>, engine: &mut Universe) {
        let Some(art) = artnet else {
            self.art_owns = [false; UNIVERSE];
            self.prev_engine = *engine;
            self.prev_artnet = [0; UNIVERSE];
            return;
        };
        let mut htp = [false; UNIVERSE];
        for f in &show.fixtures {
            for (i, c) in fixture_channels(f) {
                htp[i] = is_htp(c);
            }
        }
        for i in 0..UNIVERSE {
            if engine[i] != self.prev_engine[i] {
                self.art_owns[i] = false;
            }
            if art[i] != self.prev_artnet[i] {
                self.art_owns[i] = true;
            }
        }
        self.prev_engine = *engine;
        self.prev_artnet = *art;
        for i in 0..UNIVERSE {
            if htp[i] {
                engine[i] = engine[i].max(art[i]);
            } else if self.art_owns[i] {
                engine[i] = art[i];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::Fixture;

    fn show(channels: &[Channel]) -> Show {
        Show {
            fixtures: vec![Fixture {
                name: String::new(),
                address: 1,
                channels: channels.to_vec(),
                ofl: None,
                console: false,
            }],
            ..Show::default()
        }
    }

    fn frame(v: &[u8]) -> Universe {
        let mut u = [0; UNIVERSE];
        u[..v.len()].copy_from_slice(v);
        u
    }

    #[test]
    fn htp_on_dimmer() {
        let s = show(&[Channel::Dimmer]);
        let mut m = Merge::default();
        let a = frame(&[200]);
        let mut e = frame(&[100]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 200);
        let mut e = frame(&[220]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 220);
    }

    #[test]
    fn ltp_on_pan() {
        let s = show(&[Channel::Pan]);
        let mut m = Merge::default();
        let mut e = frame(&[10]);
        m.apply(&s, None, &mut e);
        let a = frame(&[50]);
        let mut e = frame(&[10]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 50);
        let mut e = frame(&[60]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 60);
    }

    #[test]
    fn unpatched_channel_is_ltp() {
        let s = show(&[Channel::Dimmer]);
        let mut m = Merge::default();
        let mut e = frame(&[0, 0, 90]);
        m.apply(&s, None, &mut e);
        let a = frame(&[0, 0, 40]);
        let mut e = frame(&[0, 0, 90]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[2], 40);
    }

    #[test]
    fn fog_is_ltp() {
        let s = show(&[Channel::Fog]);
        let mut m = Merge::default();
        let mut e = frame(&[200]);
        m.apply(&s, None, &mut e);
        let a = frame(&[50]);
        let mut e = frame(&[200]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 50);
    }

    #[test]
    fn same_frame_change_gives_artnet() {
        let s = show(&[Channel::Pan]);
        let mut m = Merge::default();
        let mut e = frame(&[10]);
        m.apply(&s, None, &mut e);
        let a = frame(&[50]);
        let mut e = frame(&[60]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 50);
    }

    #[test]
    fn no_source_gives_the_engine_and_a_new_source_claims_its_channels() {
        let s = show(&[Channel::Pan]);
        let mut m = Merge::default();
        let a = frame(&[50]);
        let mut e = frame(&[10]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 50);
        let mut e = frame(&[10]);
        m.apply(&s, None, &mut e);
        assert_eq!(e[0], 10);
        assert!(!m.art_owns[0]);
        let mut e = frame(&[10]);
        m.apply(&s, Some(&a), &mut e);
        assert_eq!(e[0], 50);
    }
}
