//! The DMX modes and the merge of the DMX input with the effects of the box.

use serde::{Deserialize, Serialize};

/// What the box sends on DMX OUT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmxMode {
    /// The box sends no frames.
    Off,
    /// The box sends its effects and ignores DMX IN.
    BoxOnly,
    /// For each channel, the box sends the higher value of DMX IN and its effects (HTP).
    #[default]
    Merge,
    /// The box sends DMX IN without its effects.
    Pass,
}

impl DmxMode {
    pub const ALL: [DmxMode; 4] = [Self::Off, Self::BoxOnly, Self::Merge, Self::Pass];

    /// The text for the page and the display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::BoxOnly => "Box only",
            Self::Merge => "Merge",
            Self::Pass => "Pass",
        }
    }

    /// The name in the status message.
    pub fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::BoxOnly => "box_only",
            Self::Merge => "merge",
            Self::Pass => "pass",
        }
    }

    /// The next mode for `dir` 1, the previous mode for `dir` -1. The list wraps.
    pub fn step(self, dir: i8) -> Self {
        let i = Self::ALL.iter().position(|&m| m == self).unwrap_or(0);
        let n = Self::ALL.len();
        Self::ALL[if dir < 0 {
            (i + n - 1) % n
        } else {
            (i + 1) % n
        }]
    }
}

/// The fewest channels of a good frame. DMX512 lets a transmitter send fewer than 24 slots when it
/// makes the time from break to break at least 1204 µs.
pub const MIN_CHANNELS: usize = 1;

/// Returns the channels of a received frame, or `None` when the frame is not good.
///
/// `rx` has the bytes from one frame end to the next. The ESP32 UART stores a 0x00 byte for a
/// break, and a long break can give one byte for each break event. Thus the function removes up
/// to `breaks` 0x00 bytes from the end. A removed 0x00 that was a channel does no harm, because
/// the caller sets the missing channels to 0.
pub fn frame_channels(rx: &[u8], breaks: usize) -> Option<&[u8]> {
    let mut frame = rx;
    for _ in 0..breaks {
        match frame.split_last() {
            Some((0, rest)) => frame = rest,
            _ => break,
        }
    }
    // Byte 0 is the start code. Only start code 0 has dimmer data.
    match frame.split_first() {
        Some((0, channels)) if (MIN_CHANNELS..=512).contains(&channels.len()) => Some(channels),
        _ => None,
    }
}

/// Writes the output channels for `mode` into `out`. Returns `false` when the box must send no
/// frame.
///
/// Without a DMX IN signal, Merge and Pass send the effects. Then a box without a console works
/// as before.
pub fn merge(
    mode: DmxMode,
    signal: bool,
    input: &[u8; 512],
    effect: &[u8; 512],
    out: &mut [u8; 512],
) -> bool {
    match mode {
        DmxMode::Off => return false,
        DmxMode::Merge if signal => {
            for ((o, i), e) in out.iter_mut().zip(input).zip(effect) {
                *o = (*i).max(*e);
            }
        }
        DmxMode::Pass if signal => out.copy_from_slice(input),
        _ => out.copy_from_slice(effect),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> ([u8; 512], [u8; 512]) {
        let mut input = [0u8; 512];
        let mut effect = [0u8; 512];
        input[0] = 100;
        input[4] = 200;
        effect[0] = 255;
        effect[4] = 50;
        (input, effect)
    }

    #[test]
    fn merge_takes_the_higher_value_of_each_channel() {
        let (input, effect) = frames();
        let mut out = [9u8; 512];
        assert!(merge(DmxMode::Merge, true, &input, &effect, &mut out));
        assert_eq!((out[0], out[4], out[1]), (255, 200, 0));
    }

    #[test]
    fn pass_sends_the_input() {
        let (input, effect) = frames();
        let mut out = [0u8; 512];
        assert!(merge(DmxMode::Pass, true, &input, &effect, &mut out));
        assert_eq!(out, input);
    }

    #[test]
    fn box_only_ignores_the_input() {
        let (input, effect) = frames();
        let mut out = [0u8; 512];
        assert!(merge(DmxMode::BoxOnly, true, &input, &effect, &mut out));
        assert_eq!(out, effect);
    }

    #[test]
    fn without_a_signal_merge_and_pass_send_the_effects() {
        let (input, effect) = frames();
        for mode in [DmxMode::Merge, DmxMode::Pass] {
            let mut out = [0u8; 512];
            assert!(merge(mode, false, &input, &effect, &mut out));
            assert_eq!(out, effect);
        }
    }

    #[test]
    fn off_sends_nothing() {
        let (input, effect) = frames();
        let mut out = [0u8; 512];
        assert!(!merge(DmxMode::Off, true, &input, &effect, &mut out));
    }

    #[test]
    fn a_frame_loses_its_break_bytes() {
        let mut rx = vec![0u8; 514];
        rx[512] = 7;
        assert_eq!(
            frame_channels(&rx, 1).map(|c| (c.len(), c[511])),
            Some((512, 7))
        );
        // Two break events and two 0x00 bytes.
        let mut rx = vec![0u8; 515];
        rx[1] = 9;
        assert_eq!(
            frame_channels(&rx, 2).map(|c| (c.len(), c[0])),
            Some((512, 9))
        );
        // Without a break byte, a 0 channel goes. The caller sets it to 0 again.
        assert_eq!(frame_channels(&[0u8; 513], 1).map(<[u8]>::len), Some(511));
    }

    #[test]
    fn a_frame_with_few_channels_is_good() {
        // The start code, 3 channels and the break byte.
        assert_eq!(
            frame_channels(&[0, 10, 20, 30, 0], 1),
            Some(&[10, 20, 30][..])
        );
    }

    #[test]
    fn an_empty_long_or_other_frame_is_rejected() {
        // The start code and the break byte.
        assert_eq!(frame_channels(&[0u8; 2], 1), None);
        assert_eq!(frame_channels(&[0u8; 515], 1), None);
        let mut rx = [0u8; 514];
        rx[0] = 0xCC;
        assert_eq!(frame_channels(&rx, 1), None);
    }

    #[test]
    fn step_wraps() {
        assert_eq!(DmxMode::Off.step(-1), DmxMode::Pass);
        assert_eq!(DmxMode::Pass.step(1), DmxMode::Off);
        assert_eq!(DmxMode::BoxOnly.step(1), DmxMode::Merge);
    }
}
