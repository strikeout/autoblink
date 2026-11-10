//! The outside source of the DMX output: DMX IN, Art-Net or sACN.
//!
//! The `dmx` thread gives each received frame to `Source::accept`. The source follows one sender
//! at a time. A frame of another sender does not change the output, so two senders cannot fight.
//! `Source::frame` gives the frame for the output.
//!
//! When the sender stops, the source holds the last frame for the loss time. Then it holds the
//! frame for ever, or it fades the light out to all 0. The output thus never jumps back to the
//! box engine after a network fault.

use serde::Serialize;

use crate::moves::apply_room;
use crate::render::{fixture_channels, Universe};
use crate::show::{Channel, Show, UNIVERSE};

/// A sender with the same priority takes over after the current sender is silent for this time.
/// It is the loss timeout of E1.31. An sACN source sends a frame at least each second, and a
/// static Art-Net desk can send one each 4 s.
pub const SWITCH_MS: u64 = 2_500;

/// The source counts as live for this time after its last frame. The status shows "held" after
/// it.
pub const LIVE_MS: u64 = 2_500;

/// The fade from the last frame to all 0, after the loss time.
// ponytail: the fade time is fixed. Add a setting when a rig needs a slower fade.
pub const FADE_MS: u64 = 2_000;

/// DMX IN has a priority above each sACN priority (0 to 200). Thus a console on DMX IN always
/// wins over the network.
pub const DMX_IN_PRIORITY: u8 = 201;

/// The priority of an Art-Net sender. It is the default priority of sACN.
pub const ARTNET_PRIORITY: u8 = 100;

/// The protocol of a received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    DmxIn,
    Artnet,
    Sacn,
}

/// One sender of frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sender {
    pub proto: Proto,
    /// The IPv4 address of the sender. DMX IN uses 0.0.0.0.
    pub addr: [u8; 4],
    pub priority: u8,
    /// The CID of an sACN source. Art-Net and DMX IN use 0.
    pub cid: [u8; 16],
}

impl Sender {
    fn same(&self, other: &Sender) -> bool {
        self.proto == other.proto && self.addr == other.addr && self.cid == other.cid
    }
}

/// The state of the output frame of the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    /// A frame came less than `LIVE_MS` ago.
    Live,
    /// The sender is silent. The output holds its last frame.
    Held,
    /// The loss time is over. The light fades out.
    Fading,
    /// The fade is over. The light is off.
    Black,
}

/// What the source does after the sender stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loss {
    /// True: fade the light out after the loss time. False: hold the last frame for ever.
    pub black: bool,
    /// The hold time after the last frame, in ms.
    pub after_ms: u64,
}

/// The counters of the source since the last `take_stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// The frames that changed the output.
    pub frames: u32,
    /// The frames that came out of order (sequence rule of E1.31).
    pub late: u32,
    /// The frames of a sender that the source does not follow.
    pub ignored: u32,
    /// The changes of the sender.
    pub switches: u32,
    /// The longest time between two frames of the followed sender, in ms.
    pub max_gap_ms: u64,
}

/// The outside source. See the module documentation.
pub struct Source {
    frame: Universe,
    len: usize,
    sender: Option<Sender>,
    /// The sequence number of the last frame. `None` when the sender sends no sequence.
    seq: Option<u8>,
    /// The time of the last frame.
    at: Option<u64>,
    /// The time when the sender ended its stream (sACN Stream_Terminated).
    ended: Option<u64>,
    stats: Stats,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            frame: [0; UNIVERSE],
            len: 0,
            sender: None,
            seq: None,
            at: None,
            ended: None,
            stats: Stats::default(),
        }
    }
}

impl Source {
    /// Takes a received frame. `seq` is the sequence number, or `None` when the sender sends no
    /// sequence (DMX IN, Art-Net with sequence 0). Returns true when the frame changes the output.
    pub fn accept(&mut self, from: Sender, seq: Option<u8>, data: &[u8], now_ms: u64) -> bool {
        if !self.takes(&from, now_ms) {
            self.stats.ignored = self.stats.ignored.saturating_add(1);
            return false;
        }
        let same = self.sender.is_some_and(|s| s.same(&from));
        // After a silence or the end of a stream, the sender can start a new sequence.
        let streaming = self.ended.is_none()
            && self
                .at
                .is_some_and(|t| now_ms.saturating_sub(t) < SWITCH_MS);
        if same {
            // E1.31 6.7.2: a frame that is up to 19 steps behind the last frame is late.
            if let (true, Some(new), Some(last)) = (streaming, seq, self.seq) {
                let d = new.wrapping_sub(last) as i8;
                if d <= 0 && d > -20 {
                    self.stats.late = self.stats.late.saturating_add(1);
                    return false;
                }
            }
            if let Some(at) = self.at {
                self.stats.max_gap_ms = self.stats.max_gap_ms.max(now_ms.saturating_sub(at));
            }
        } else if self.sender.is_some() {
            self.stats.switches = self.stats.switches.saturating_add(1);
        }
        let n = data.len().min(UNIVERSE);
        self.frame[..n].copy_from_slice(&data[..n]);
        // A shorter frame does not keep the old values of the other channels.
        self.frame[n..].fill(0);
        self.len = n;
        self.sender = Some(from);
        self.seq = seq;
        self.at = Some(now_ms);
        self.ended = None;
        self.stats.frames = self.stats.frames.saturating_add(1);
        true
    }

    /// The sender ended its stream. The loss time is over at once, so the loss action starts now.
    /// Another sender can take over at once.
    pub fn end(&mut self, from: Sender, now_ms: u64) {
        if self.sender.is_some_and(|s| s.same(&from)) && self.ended.is_none() {
            self.ended = Some(now_ms);
            self.seq = None;
        }
    }

    /// Forgets the frame and the sender. The output then has no outside source, as before the
    /// first frame.
    pub fn clear(&mut self) {
        let stats = self.stats;
        *self = Self {
            stats,
            ..Self::default()
        };
    }

    /// True when a frame of `from` can change the output.
    fn takes(&self, from: &Sender, now_ms: u64) -> bool {
        let Some(cur) = self.sender else {
            return true;
        };
        if cur.same(from) {
            return true;
        }
        let silent = self.ended.is_some()
            || self
                .at
                .is_none_or(|t| now_ms.saturating_sub(t) >= SWITCH_MS);
        silent || from.priority > cur.priority
    }

    /// The sender that the source follows, or `None` before the first frame.
    pub fn sender(&self) -> Option<Sender> {
        self.sender
    }

    /// The slots of the last frame, or `None` before the first frame.
    pub fn slots(&self) -> Option<usize> {
        self.sender.map(|_| self.len)
    }

    /// The state of the output frame at `now_ms`, or `None` before the first frame.
    pub fn state(&self, loss: Loss, now_ms: u64) -> Option<SourceState> {
        let at = self.at?;
        if self.ended.is_none() && now_ms.saturating_sub(at) < LIVE_MS {
            return Some(SourceState::Live);
        }
        let Some(start) = self.fade_start(loss) else {
            return Some(SourceState::Held);
        };
        Some(if now_ms < start {
            SourceState::Held
        } else if now_ms.saturating_sub(start) < FADE_MS {
            SourceState::Fading
        } else {
            SourceState::Black
        })
    }

    /// The start of the fade, or `None` when the source holds for ever.
    fn fade_start(&self, loss: Loss) -> Option<u64> {
        if !loss.black {
            return None;
        }
        self.ended
            .or(self.at.map(|t| t.saturating_add(loss.after_ms)))
    }

    /// Writes the output frame at `now_ms` into `out`. Returns its state, or `None` before the
    /// first frame (then `out` does not change).
    ///
    /// In the fade, the light channels of the lights of `show` go down, and their pan and tilt
    /// stay. The Fog channels and each channel that no light of `show` patches go down too. At
    /// the end, only the pan, tilt and fixed channels of the lights keep a value.
    pub fn frame(
        &self,
        show: &Show,
        loss: Loss,
        now_ms: u64,
        out: &mut Universe,
    ) -> Option<SourceState> {
        let state = self.state(loss, now_ms)?;
        *out = self.frame;
        let level = match (state, self.fade_start(loss)) {
            (SourceState::Fading, Some(start)) => {
                let left = FADE_MS.saturating_sub(now_ms.saturating_sub(start));
                (left * 255 / FADE_MS) as u8
            }
            (SourceState::Black, _) => 0,
            _ => return Some(state),
        };
        dim(show, level, out);
        Some(state)
    }

    /// Returns the counters and sets them to 0.
    pub fn take_stats(&mut self) -> Stats {
        std::mem::take(&mut self.stats)
    }
}

/// Scales the light of `out` to `level` (255 is full). The light channels of the lights of
/// `show`, their Fog channels and each channel that no light patches go down. The pan, tilt and
/// fixed channels of the lights stay.
pub fn dim(show: &Show, level: u8, out: &mut Universe) {
    // `apply_room` scales the light channels of the lights and sets their Strobe channel to 0 at
    // level 0. The loop scales the Fog channels and the channels that no light patches.
    let mut keep = [false; UNIVERSE];
    for f in &show.fixtures {
        for (i, c) in fixture_channels(f) {
            keep[i] = c != Channel::Fog;
        }
    }
    apply_room(show, 255, level, out);
    for (o, k) in out.iter_mut().zip(keep) {
        if !k {
            *o = (u32::from(*o) * u32::from(level) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::Fixture;

    const ART: Sender = Sender {
        proto: Proto::Artnet,
        addr: [192, 168, 1, 2],
        priority: ARTNET_PRIORITY,
        cid: [0; 16],
    };
    const OTHER: Sender = Sender {
        proto: Proto::Artnet,
        addr: [192, 168, 1, 3],
        priority: ARTNET_PRIORITY,
        cid: [0; 16],
    };
    const HOLD: Loss = Loss {
        black: false,
        after_ms: 10_000,
    };
    const BLACK: Loss = Loss {
        black: true,
        after_ms: 10_000,
    };

    fn out(s: &Source, show: &Show, loss: Loss, now: u64) -> (Option<SourceState>, Universe) {
        let mut u = [7; UNIVERSE];
        let st = s.frame(show, loss, now, &mut u);
        (st, u)
    }

    #[test]
    fn nothing_before_the_first_frame() {
        let s = Source::default();
        let (st, u) = out(&s, &Show::default(), HOLD, 0);
        assert_eq!(st, None);
        assert_eq!(u[0], 7);
        assert_eq!(s.slots(), None);
    }

    #[test]
    fn a_short_frame_clears_the_other_channels() {
        let mut s = Source::default();
        assert!(s.accept(ART, None, &[1, 2, 3], 0));
        assert!(s.accept(ART, None, &[9], 10));
        let (st, u) = out(&s, &Show::default(), HOLD, 10);
        assert_eq!(st, Some(SourceState::Live));
        assert_eq!(&u[..3], &[9, 0, 0]);
        assert_eq!(s.slots(), Some(1));
    }

    #[test]
    fn hold_keeps_the_frame_for_ever() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        assert_eq!(
            out(&s, &Show::default(), HOLD, 3_000).0,
            Some(SourceState::Held)
        );
        let (st, u) = out(&s, &Show::default(), HOLD, 3_600_000);
        assert_eq!(st, Some(SourceState::Held));
        assert_eq!(u[0], 200);
    }

    #[test]
    fn black_holds_then_fades_to_zero() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        let show = Show::default();
        assert_eq!(
            out(&s, &show, BLACK, 9_999),
            (Some(SourceState::Held), {
                let mut u = [0; UNIVERSE];
                u[0] = 200;
                u
            })
        );
        let (st, u) = out(&s, &show, BLACK, 11_000);
        assert_eq!(st, Some(SourceState::Fading));
        assert!(u[0] > 90 && u[0] < 110, "half way: {}", u[0]);
        let (st, u) = out(&s, &show, BLACK, 12_000);
        assert_eq!(st, Some(SourceState::Black));
        assert_eq!(u[0], 0);
    }

    #[test]
    fn the_fade_keeps_pan_and_tilt_and_drops_fog() {
        let show = Show {
            fixtures: vec![Fixture {
                name: "head".into(),
                address: 1,
                channels: vec![Channel::Pan, Channel::Tilt, Channel::Dimmer, Channel::Fog],
                ofl: None,
                console: false,
            }],
            ..Show::default()
        };
        let mut s = Source::default();
        s.accept(ART, None, &[100, 50, 255, 200, 80], 0);
        let (st, u) = out(&s, &show, BLACK, 20_000);
        assert_eq!(st, Some(SourceState::Black));
        assert_eq!(&u[..5], &[100, 50, 0, 0, 0]);
    }

    #[test]
    fn a_new_frame_ends_the_loss() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        assert_eq!(s.state(BLACK, 20_000), Some(SourceState::Black));
        s.accept(ART, None, &[150], 20_000);
        assert_eq!(out(&s, &Show::default(), BLACK, 20_000).1[0], 150);
    }

    #[test]
    fn the_end_of_a_stream_starts_the_fade_at_once() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        s.end(ART, 100);
        assert_eq!(s.state(BLACK, 100), Some(SourceState::Fading));
        assert_eq!(s.state(HOLD, 100), Some(SourceState::Held));
        assert_eq!(s.state(BLACK, 2_100), Some(SourceState::Black));
        // Another sender can take over at once.
        assert!(s.accept(OTHER, None, &[1], 150));
    }

    #[test]
    fn a_second_sender_waits_until_the_first_is_silent() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        assert!(!s.accept(OTHER, None, &[1], 500));
        assert!(s.accept(ART, None, &[201], 600));
        assert!(!s.accept(OTHER, None, &[1], 3_000));
        assert!(s.accept(OTHER, None, &[1], 3_100));
        assert_eq!(s.sender(), Some(OTHER));
        let st = s.take_stats();
        assert_eq!((st.frames, st.ignored, st.switches), (3, 2, 1));
        assert_eq!(s.take_stats(), Stats::default());
    }

    #[test]
    fn a_higher_priority_takes_over_at_once() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        let dmx_in = Sender {
            proto: Proto::DmxIn,
            addr: [0; 4],
            priority: DMX_IN_PRIORITY,
            cid: [0; 16],
        };
        assert!(s.accept(dmx_in, None, &[5], 10));
        assert!(!s.accept(ART, None, &[200], 20));
        let low = Sender {
            proto: Proto::Sacn,
            addr: [192, 168, 1, 9],
            priority: 50,
            cid: [1; 16],
        };
        assert!(!s.accept(low, Some(1), &[1], 30));
    }

    #[test]
    fn a_late_frame_is_dropped() {
        let mut s = Source::default();
        assert!(s.accept(ART, Some(10), &[10], 0));
        assert!(!s.accept(ART, Some(9), &[9], 1));
        assert!(!s.accept(ART, Some(10), &[10], 2));
        assert!(s.accept(ART, Some(11), &[11], 3));
        // A jump of 20 or more back is a new stream, for example after a restart of the desk.
        assert!(s.accept(ART, Some(200), &[1], 4));
        // The sequence wraps.
        assert!(s.accept(ART, Some(255), &[1], 5));
        assert!(s.accept(ART, Some(0), &[1], 6));
        // Without a sequence, the rule does not apply.
        assert!(s.accept(ART, None, &[1], 7));
        assert!(s.accept(ART, Some(0), &[1], 8));
        assert_eq!(s.take_stats().late, 2);
    }

    #[test]
    fn a_new_stream_after_a_silence_or_an_end_starts_its_own_sequence() {
        let mut s = Source::default();
        assert!(s.accept(ART, Some(15), &[1], 0));
        assert!(s.accept(ART, Some(1), &[2], SWITCH_MS));
        s.end(ART, SWITCH_MS + 10);
        assert!(s.accept(ART, Some(0), &[3], SWITCH_MS + 20));
    }

    #[test]
    fn two_sacn_sources_on_one_computer_do_not_mix() {
        let a = Sender {
            proto: Proto::Sacn,
            addr: [10, 0, 0, 1],
            priority: 100,
            cid: [1; 16],
        };
        let b = Sender { cid: [2; 16], ..a };
        let mut s = Source::default();
        assert!(s.accept(a, Some(1), &[1], 0));
        assert!(!s.accept(b, Some(1), &[2], 10));
    }

    #[test]
    fn clear_forgets_the_frame() {
        let mut s = Source::default();
        s.accept(ART, None, &[200], 0);
        s.clear();
        assert_eq!(s.state(HOLD, 10), None);
        assert_eq!(s.sender(), None);
        assert_eq!(s.take_stats().frames, 1);
    }

    #[test]
    fn the_fade_scales_each_channel_once() {
        let show = Show {
            fixtures: vec![Fixture {
                name: "par".into(),
                address: 1,
                channels: vec![Channel::Dimmer],
                ofl: None,
                console: false,
            }],
            ..Show::default()
        };
        let mut s = Source::default();
        s.accept(ART, None, &[200, 200], 0);
        let (st, u) = out(&s, &show, BLACK, 11_000);
        assert_eq!(st, Some(SourceState::Fading));
        // Half way, the patched dimmer and the unpatched channel are both at about 100.
        assert!((90..=110).contains(&u[0]), "dimmer {}", u[0]);
        assert_eq!(u[0], u[1]);
    }

    #[test]
    fn the_gap_counter_keeps_the_longest_gap() {
        let mut s = Source::default();
        s.accept(ART, None, &[1], 0);
        s.accept(ART, None, &[1], 25);
        s.accept(ART, None, &[1], 400);
        s.accept(ART, None, &[1], 420);
        assert_eq!(s.take_stats().max_gap_ms, 375);
    }
}
