//! The intensity effects of a row of fixtures.

use crate::clock::Beat;
use crate::show::{Effect, Every, Shape};

/// The factor 0 to 255 of fixture `k` of `n` in a row with `effect`.
/// It is 255 for `None`, `Strobe` and `Fog`, and for every effect without a beat.
pub fn factor(effect: &Effect, beat: Option<&Beat>, k: usize, n: usize) -> u8 {
    let (
        Some(b),
        Effect::Flash { every }
        | Effect::Pulse { every }
        | Effect::Chase { every }
        | Effect::Random { every },
    ) = (beat, effect)
    else {
        return 255;
    };
    let second_half = b.phase >= 0.5;
    let (p, c) = match every {
        Every::Beat => (b.phase, b.count),
        Every::Half => (
            (2.0 * b.phase).fract(),
            b.count.wrapping_mul(2).wrapping_add(u32::from(second_half)),
        ),
        Every::Bar => (
            (f64::from(b.beat_in_bar.saturating_sub(1)) + b.phase) / 4.0,
            // Count the bar starts, so that a bar steps with beat 1.
            b.count
                .wrapping_sub(u32::from(b.beat_in_bar.saturating_sub(1)))
                / 4,
        ),
    };
    let lit = |on: bool| if on { 255 } else { 0 };
    match effect {
        Effect::Flash { .. } => (255.0 * (1.0 - p).powi(3)).clamp(0.0, 255.0) as u8,
        Effect::Pulse { .. } => {
            (255.0 * (1.0 + (std::f64::consts::TAU * p).cos()) / 2.0).clamp(0.0, 255.0) as u8
        }
        Effect::Chase { .. } if n > 0 => lit(c as usize % n == k),
        Effect::Random { .. } if n > 0 => lit(xorshift32(c.wrapping_add(1)) as usize % n == k),
        _ => 255,
    }
}

/// The pan and tilt offset of a shape for fixture `k` of `n`, in 16-bit units.
/// A fixture is `k / n` of a cycle behind the first one. Without a beat, the offset is zero.
pub fn shape_offset(shape: &Shape, beat: Option<&Beat>, k: usize, n: usize) -> (i32, i32) {
    let (
        Some(b),
        Shape::Circle { beats, size }
        | Shape::Figure8 { beats, size }
        | Shape::Sweep { beats, size },
    ) = (beat, shape)
    else {
        return (0, 0);
    };
    let beats = u32::from((*beats).max(1));
    let amp = f64::from(*size) * 64.0;
    let shift = if n > 0 { k as f64 / n as f64 } else { 0.0 };
    let t = ((f64::from(b.count % beats) + b.phase) / f64::from(beats) + shift).fract();
    let a = std::f64::consts::TAU * t;
    let (x, y) = match shape {
        Shape::Circle { .. } => (a.cos(), a.sin()),
        Shape::Figure8 { .. } => (a.sin(), (2.0 * a).sin() / 2.0),
        _ => (a.sin(), 0.0),
    };
    ((amp * x).round() as i32, (amp * y).round() as i32)
}

fn xorshift32(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(phase: f64, beat_in_bar: u8, count: u32) -> Beat {
        Beat {
            phase,
            beat_in_bar,
            count,
            ms_per_beat: 500.0,
        }
    }

    const FLASH: Effect = Effect::Flash { every: Every::Beat };

    #[test]
    fn flash_shape() {
        assert_eq!(factor(&FLASH, Some(&beat(0.0, 1, 0)), 0, 1), 255);
        let v = factor(&FLASH, Some(&beat(0.25, 1, 0)), 0, 1);
        assert!(v == 107 || v == 108, "{v}");
        assert_eq!(factor(&FLASH, Some(&beat(0.99, 1, 0)), 0, 1), 0);
    }

    #[test]
    fn pulse_shape() {
        let e = Effect::Pulse { every: Every::Beat };
        assert_eq!(factor(&e, Some(&beat(0.0, 1, 0)), 0, 1), 255);
        assert_eq!(factor(&e, Some(&beat(0.5, 1, 0)), 0, 1), 0);
    }

    #[test]
    fn chase_steps() {
        let e = Effect::Chase { every: Every::Beat };
        for (count, lit) in [0, 1, 2, 0, 1, 2].into_iter().enumerate() {
            for k in 0..3 {
                let v = factor(&e, Some(&beat(0.0, 1, count as u32)), k, 3);
                assert_eq!(v, if k == lit { 255 } else { 0 }, "count {count} k {k}");
            }
        }
        assert_eq!(factor(&e, Some(&beat(0.0, 1, 0)), 0, 0), 255);
    }

    #[test]
    fn random_lights_one() {
        let e = Effect::Random { every: Every::Beat };
        for count in 0..20 {
            let lit = (0..4)
                .filter(|&k| factor(&e, Some(&beat(0.0, 1, count)), k, 4) == 255)
                .count();
            assert_eq!(lit, 1);
        }
    }

    #[test]
    fn half_and_bar() {
        let half = Effect::Flash { every: Every::Half };
        assert_eq!(factor(&half, Some(&beat(0.5, 1, 0)), 0, 1), 255);
        let bar = Effect::Flash { every: Every::Bar };
        let v = factor(&bar, Some(&beat(0.0, 3, 2)), 0, 1);
        assert!(v == 31 || v == 32, "{v}");
    }

    #[test]
    fn bar_count_follows_bar_start() {
        let e = Effect::Chase { every: Every::Bar };
        let lit =
            |count, bib| (0..3).find(|&k| factor(&e, Some(&beat(0.0, bib, count)), k, 3) == 255);
        assert_eq!(lit(2, 1), lit(5, 4));
        assert_ne!(lit(2, 1), lit(6, 1));
    }

    #[test]
    fn no_beat_is_full() {
        assert_eq!(factor(&FLASH, None, 0, 1), 255);
        assert_eq!(
            factor(&Effect::Fog { ms: 500 }, Some(&beat(0.5, 1, 0)), 0, 1),
            255
        );
    }

    #[test]
    fn circle_at_quarter_phase() {
        let sh = Shape::Circle {
            beats: 4,
            size: 255,
        };
        let (x, y) = shape_offset(&sh, Some(&beat(0.0, 2, 1)), 0, 1);
        assert!(x.abs() <= 1 && (y - 16320).abs() <= 1, "{x} {y}");
        assert_eq!(shape_offset(&sh, None, 0, 1), (0, 0));
    }

    #[test]
    fn heads_are_phase_shifted() {
        let sh = Shape::Sweep {
            beats: 4,
            size: 255,
        };
        let b = beat(0.5, 1, 0);
        assert_ne!(
            shape_offset(&sh, Some(&b), 0, 2),
            shape_offset(&sh, Some(&b), 1, 2)
        );
    }
}
