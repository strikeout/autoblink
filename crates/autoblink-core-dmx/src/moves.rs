//! The renderer of the autopilot moves, the build and the room level.
//!
//! `render_auto` replaces the idle look and the layers when the autopilot is on. `apply_room`
//! runs after the merge, so it also reaches the console fixtures that come from DMX IN. Nothing
//! here allocates memory.

use crate::auto::{Move, MoveShape, Param, Spread, Target, Tier, MAX_MOVES};
use crate::clock::Beat;
use crate::render::{fixture_channels, Universe};
use crate::show::{Channel, Show};

/// What the autopilot shows now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoLook {
    pub tier: Tier,
    /// The energy, 0 to 100.
    pub spot: u8,
    /// Bit i is set when `auto.moves[i]` runs.
    pub moves: u32,
    /// 1 or 2. The second half of a build gives 2.
    pub speed: u8,
    /// The level of the box fixtures, 0 to 255. The build lowers it.
    pub level: u8,
}

impl Default for AutoLook {
    fn default() -> Self {
        AutoLook {
            tier: Tier::Groove,
            spot: 50,
            moves: 0,
            speed: 1,
            level: 255,
        }
    }
}

/// One quarter of the 16-bit pan or tilt range.
const QUARTER: f64 = 16384.0;

/// True when `c` is a light channel of a fixture. A fixture with a dimmer takes its colour
/// from the dimmer, so its colour channels are not light channels.
pub(crate) fn is_light(c: Channel, has_dimmer: bool) -> bool {
    match c {
        Channel::Dimmer => true,
        Channel::Red
        | Channel::Green
        | Channel::Blue
        | Channel::White
        | Channel::Amber
        | Channel::Uv => !has_dimmer,
        _ => false,
    }
}

/// The share 0.0..1.0 of a cycle by which fixture `k` of `n` runs ahead.
fn spread_shift(spread: Spread, k: usize, n: usize) -> f64 {
    let nf = n.max(1) as f64;
    match spread {
        Spread::Together => 0.0,
        Spread::Fan => k as f64 / nf,
        Spread::Mirror => (k as f64 - (nf - 1.0) / 2.0).abs() / nf,
        Spread::Random => {
            let h = (k as u32).wrapping_add(1).wrapping_mul(2_654_435_761);
            f64::from(h >> 8) / f64::from(1u32 << 24)
        }
    }
}

/// The value 0.0..=1.0 of a move shape at the cycle phase `p` (0.0..1.0) for fixture k of n.
/// `size` (0 to 100) is the size of the move.
pub fn shape_value(shape: MoveShape, spread: Spread, size: u8, p: f64, k: usize, n: usize) -> f64 {
    let s = f64::from(size.min(100)) / 100.0;
    if shape == MoveShape::Chase {
        return f64::from(u8::from((p * n.max(1) as f64).floor() as usize == k));
    }
    let q = (p + spread_shift(spread, k, n)).fract();
    let tau = std::f64::consts::TAU;
    match shape {
        MoveShape::Pulse => (1.0 + (tau * q).cos()) / 2.0,
        MoveShape::Ramp => q,
        MoveShape::Wave | MoveShape::Sweep | MoveShape::Circle | MoveShape::Figure8 => {
            (1.0 + (tau * q).sin()) / 2.0
        }
        MoveShape::StrobeBurst => f64::from(u8::from(q < 0.25)),
        MoveShape::Blackout => f64::from(u8::from(q < 1.0 - s)),
        MoveShape::Hold => s,
        MoveShape::Chase => 0.0,
    }
}

/// The speed of a move: 2 in the second half of a build, or for a fast move in the tier Peak.
fn move_speed(st: &AutoLook, m: &Move) -> u8 {
    if st.speed >= 2 || (m.faster && st.tier == Tier::Peak) {
        2
    } else {
        1
    }
}

/// The cycle phase 0.0..1.0 of a move. A cycle starts on a bar line, as in `shape_offset`.
/// Without a beat, the phase is 0.
fn cycle_phase(beat: Option<&Beat>, m: &Move, speed: u8) -> f64 {
    let Some(b) = beat else {
        return 0.0;
    };
    let period = f64::from(m.half_beats.max(1)) / 2.0 / f64::from(speed.max(1));
    let i = if b.beat_in_bar == 0 {
        b.count
    } else {
        let bib = u32::from(b.beat_in_bar - 1);
        bib.wrapping_add((b.count.wrapping_sub(bib) / 4).wrapping_mul(4))
    };
    ((f64::from(i) + b.phase) / period).fract()
}

/// The moves that run now, with the index of each.
fn active<'a>(show: &'a Show, st: &AutoLook) -> impl Iterator<Item = &'a Move> + 'a {
    let mask = st.moves;
    show.auto
        .moves
        .iter()
        .take(MAX_MOVES)
        .enumerate()
        .filter(move |(i, _)| mask >> i & 1 == 1)
        .map(|(_, m)| m)
}

/// The position of fixture `i` in the target of a move: (k, n). None when the move skips it.
fn slot(show: &Show, target: Target, i: usize) -> Option<(usize, usize)> {
    match target {
        Target::Room => None,
        Target::Fixture(f) => (usize::from(f) == i).then_some((0, 1)),
        Target::Group(g) => {
            let ids = &show.groups.get(usize::from(g))?.fixtures;
            let k = ids.iter().position(|&x| usize::from(x) == i)?;
            Some((k, ids.len()))
        }
    }
}

/// The pan and tilt offsets of a move in 16-bit units.
fn aim_offset(m: &Move, p: f64, k: usize, n: usize) -> (f64, f64) {
    let amp = f64::from(m.size.min(100)) / 100.0 * QUARTER;
    let (x, y) = match m.shape {
        MoveShape::Sweep | MoveShape::Circle | MoveShape::Figure8 => {
            let a = std::f64::consts::TAU * (p + spread_shift(m.spread, k, n)).fract();
            match m.shape {
                MoveShape::Circle => (a.cos(), a.sin()),
                MoveShape::Figure8 => (a.sin(), (2.0 * a).sin() / 2.0),
                _ => (a.sin(), 0.0),
            }
        }
        _ => {
            let v = shape_value(m.shape, m.spread, m.size, p, k, n);
            (v, v)
        }
    };
    match m.param {
        Param::Pan => (amp * x, 0.0),
        // A sweep moves along one axis only, so the tilt axis takes the same value.
        Param::Tilt if m.shape == MoveShape::Sweep => (0.0, amp * x),
        Param::Tilt => (0.0, amp * y),
        _ => (amp * x, amp * y),
    }
}

/// Writes the base (tier colour at the energy level) and the active moves into `out`.
/// It writes only fixtures with `console == false`. Room moves act through `room_factor`.
pub fn render_auto(
    show: &Show,
    st: &AutoLook,
    beat: Option<&Beat>,
    _now_ms: u64,
    out: &mut Universe,
) {
    let tier_color = show.auto.colors[match st.tier {
        Tier::Calm => 0,
        Tier::Groove => 1,
        Tier::Peak => 2,
    }];
    let base = (96 + 159 * u32::from(st.spot.min(100)) / 100) * u32::from(st.level) / 255;
    for (i, f) in show.fixtures.iter().enumerate().filter(|(_, f)| !f.console) {
        let mut dim = 1.0;
        let mut mix = (0.0, [0u8; 3]);
        let mut strobe = 0u8;
        let (mut pan, mut tilt) = (0.0, 0.0);
        for m in active(show, st) {
            let Some((k, n)) = slot(show, m.target, i) else {
                continue;
            };
            let speed = move_speed(st, m);
            let p = cycle_phase(beat, m, speed);
            let v = shape_value(m.shape, m.spread, m.size, p, k, n);
            match m.param {
                Param::Dimmer => dim *= v,
                Param::Colour => mix = (v, m.color.unwrap_or([255; 3])),
                Param::Strobe => {
                    if v > 0.5 {
                        strobe = strobe.max((u32::from(m.size.min(100)) * 255 / 100) as u8);
                    }
                }
                Param::Pan | Param::Tilt | Param::PanTilt => {
                    let (dx, dy) = aim_offset(m, p, k, n);
                    pan += dx;
                    tilt += dy;
                }
            }
        }
        let lvl = (f64::from(base) * dim).round() as u32;
        let has_dimmer = f.channels.contains(&Channel::Dimmer);
        let colour = |j: usize| {
            let a = f64::from(tier_color[j]);
            let v = (a + (f64::from(mix.1[j]) - a) * mix.0).round() as u32;
            if has_dimmer {
                v as u8
            } else {
                (v * lvl / 255) as u8
            }
        };
        let pan16 = (32768.0 + pan).clamp(0.0, 65535.0) as u16;
        let tilt16 = (32768.0 + tilt).clamp(0.0, 65535.0) as u16;
        for (idx, c) in fixture_channels(f) {
            out[idx] = match c {
                Channel::Dimmer => lvl as u8,
                Channel::Red => colour(0),
                Channel::Green => colour(1),
                Channel::Blue => colour(2),
                Channel::White | Channel::Amber | Channel::Uv | Channel::Fog => 0,
                Channel::Strobe => strobe,
                Channel::Pan => (pan16 >> 8) as u8,
                Channel::PanFine => pan16 as u8,
                Channel::Tilt => (tilt16 >> 8) as u8,
                Channel::TiltFine => tilt16 as u8,
                Channel::Fixed(v) => v,
            };
        }
    }
}

/// The room level 0..=255 that Room moves give now (255 without a Room move).
pub fn room_factor(show: &Show, st: &AutoLook, beat: Option<&Beat>) -> u8 {
    let mut f = 1.0;
    for m in active(show, st) {
        if m.target == Target::Room && m.param == Param::Dimmer {
            let p = cycle_phase(beat, m, move_speed(st, m));
            f *= shape_value(m.shape, m.spread, m.size, p, 0, 1);
        }
    }
    (255.0 * f).round() as u8
}

/// Scales the light channels of the fixtures in place. `console` scales the console fixtures,
/// `all` scales every fixture. The strobe channel scales too, so that a blackout stops it.
pub fn apply_room(show: &Show, console: u8, all: u8, out: &mut Universe) {
    for f in &show.fixtures {
        let s = if f.console {
            u32::from(console) * u32::from(all) / 255
        } else {
            u32::from(all)
        };
        if s >= 255 {
            continue;
        }
        let has_dimmer = f.channels.contains(&Channel::Dimmer);
        for (i, c) in fixture_channels(f) {
            if is_light(c, has_dimmer) || c == Channel::Strobe {
                out[i] = (u32::from(out[i]) * s / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto::Auto;
    use crate::show::{Fixture, Group, Show, UNIVERSE};
    use Channel::*;

    fn fx(address: u16, channels: &[Channel], console: bool) -> Fixture {
        Fixture {
            name: "f".into(),
            address,
            channels: channels.to_vec(),
            ofl: None,
            console,
        }
    }

    pub fn show_with(fixtures: Vec<Fixture>) -> Show {
        let ids = (0..fixtures.len() as u8).collect();
        Show {
            fixtures,
            groups: vec![Group {
                name: "g".into(),
                fixtures: ids,
            }],
            looks: Vec::new(),
            idle: None,
            positions: Vec::new(),
            auto: Auto::default(),
        }
    }

    #[test]
    fn shape_values() {
        let v = |s, size, p| shape_value(s, Spread::Together, size, p, 0, 1);
        assert!((v(MoveShape::Pulse, 0, 0.0) - 1.0).abs() < 1e-9);
        assert!(v(MoveShape::Pulse, 0, 0.5).abs() < 1e-9);
        assert!((v(MoveShape::Ramp, 0, 0.25) - 0.25).abs() < 1e-9);
        assert!((v(MoveShape::Hold, 40, 0.7) - 0.4).abs() < 1e-9);
        assert_eq!(v(MoveShape::Blackout, 25, 0.8), 0.0);
        assert_eq!(v(MoveShape::Blackout, 25, 0.5), 1.0);
        assert_eq!(v(MoveShape::StrobeBurst, 0, 0.1), 1.0);
        assert_eq!(v(MoveShape::StrobeBurst, 0, 0.5), 0.0);
        assert_eq!(
            shape_value(MoveShape::Chase, Spread::Fan, 0, 0.3, 1, 4),
            1.0
        );
        assert_eq!(
            shape_value(MoveShape::Chase, Spread::Fan, 0, 0.3, 0, 4),
            0.0
        );
    }

    #[test]
    fn fan_offsets_fixtures() {
        let a = shape_value(MoveShape::Ramp, Spread::Fan, 0, 0.0, 0, 4);
        let b = shape_value(MoveShape::Ramp, Spread::Fan, 0, 0.0, 1, 4);
        assert!((b - a - 0.25).abs() < 1e-9);
    }

    #[test]
    fn base_follows_tier_and_spot() {
        let mut s = show_with(vec![fx(1, &[Dimmer, Red, Green, Blue], false)]);
        s.auto.colors = [[1, 2, 3], [10, 20, 30], [4, 5, 6]];
        let st = AutoLook {
            tier: Tier::Groove,
            spot: 50,
            ..AutoLook::default()
        };
        let mut out = [0; UNIVERSE];
        render_auto(&s, &st, None, 0, &mut out);
        assert_eq!(out[..4], [175, 10, 20, 30]);
    }

    #[test]
    fn console_fixture_not_rendered() {
        let s = show_with(vec![fx(1, &[Dimmer, Red], true)]);
        let mut out = [0; UNIVERSE];
        render_auto(&s, &AutoLook::default(), None, 0, &mut out);
        assert_eq!(out[..2], [0, 0]);
    }

    #[test]
    fn room_never_touches_pan_tilt() {
        let s = show_with(vec![fx(
            1,
            &[Pan, PanFine, Tilt, TiltFine, Dimmer, Strobe],
            false,
        )]);
        let mut out = [0; UNIVERSE];
        out[..6].copy_from_slice(&[1, 2, 3, 4, 200, 255]);
        apply_room(&s, 0, 0, &mut out);
        assert_eq!(out[..6], [1, 2, 3, 4, 0, 0]);
    }

    #[test]
    fn room_scales_colour_without_dimmer() {
        let s = show_with(vec![fx(1, &[Red, Green, Blue, White, Amber, Uv], false)]);
        let mut out = [0; UNIVERSE];
        out[..6].copy_from_slice(&[200; 6]);
        apply_room(&s, 255, 128, &mut out);
        assert_eq!(out[..6], [100; 6]);
    }
}
