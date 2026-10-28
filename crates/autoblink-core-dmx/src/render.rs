//! Render a look into the DMX universe.

use crate::clock::Beat;
use crate::effect::shape_offset;
use crate::show::{Channel, Fixture, Look, Show, UNIVERSE};

/// One value for each channel of the universe. Index 0 is DMX channel 1.
pub type Universe = [u8; UNIVERSE];

/// The fixtures of a group, with the position of each fixture in the group.
/// A wrong index gives no fixture, because only an unsanitized show has one.
pub(crate) fn group_fixtures(
    show: &Show,
    group: u8,
) -> impl Iterator<Item = (usize, usize, &Fixture)> + '_ {
    let ids = show
        .groups
        .get(usize::from(group))
        .map_or(&[][..], |g| &g.fixtures[..]);
    ids.iter().enumerate().filter_map(move |(pos, &i)| {
        show.fixtures
            .get(usize::from(i))
            .map(|f| (pos, ids.len(), f))
    })
}

/// The channel indices of a fixture. The universe end cuts the list.
pub(crate) fn fixture_channels(f: &Fixture) -> impl Iterator<Item = (usize, Channel)> + '_ {
    let start = usize::from(f.address).saturating_sub(1);
    f.channels
        .iter()
        .enumerate()
        .map(move |(n, &c)| (start + n, c))
        .take_while(|&(i, _)| i < UNIVERSE)
}

/// Writes the static values of `look` into `out`. `level(row, fixture_pos, fixture_count)`
/// gives the effect factor 0 to 255 of a fixture in a row; 255 is no change.
/// `beat` moves the pan and tilt of a row with a shape.
pub fn render(
    show: &Show,
    look: &Look,
    beat: Option<&Beat>,
    level: impl Fn(usize, usize, usize) -> u8,
    out: &mut Universe,
) {
    for (r, row) in look.rows.iter().enumerate() {
        for (pos, count, f) in group_fixtures(show, row.group) {
            let lvl = u32::from(level(r, pos, count));
            let intensity = u32::from(row.intensity);
            let has_dimmer = f.channels.contains(&Channel::Dimmer);
            let colour = |v: u8| {
                if has_dimmer {
                    v
                } else {
                    (u32::from(v) * intensity * lvl / (255 * 255)) as u8
                }
            };
            let fixture_index = show.fixtures.iter().position(|x| std::ptr::eq(x, f));
            let base = row
                .position
                .and_then(|p| show.positions.get(usize::from(p)))
                .and_then(|p| {
                    p.aims
                        .iter()
                        .find(|a| Some(usize::from(a.fixture)) == fixture_index)
                })
                .map_or((32768, 32768), |a| (i32::from(a.pan), i32::from(a.tilt)));
            let (dx, dy) = shape_offset(&row.shape, beat, pos, count);
            let pan = (base.0 + dx).clamp(0, 65535) as u16;
            let tilt = (base.1 + dy).clamp(0, 65535) as u16;
            for (i, c) in fixture_channels(f) {
                out[i] = match c {
                    Channel::Dimmer => (intensity * lvl / 255) as u8,
                    Channel::Red => colour(row.color[0]),
                    Channel::Green => colour(row.color[1]),
                    Channel::Blue => colour(row.color[2]),
                    Channel::White => colour(row.color[3]),
                    Channel::Amber | Channel::Uv | Channel::Strobe | Channel::Fog => 0,
                    Channel::Pan => (pan >> 8) as u8,
                    Channel::PanFine => pan as u8,
                    Channel::Tilt => (tilt >> 8) as u8,
                    Channel::TiltFine => tilt as u8,
                    Channel::Fixed(v) => v,
                };
            }
        }
    }
}

/// Returns the set of channel indices that `look` touches (the channels of the fixtures of its groups).
/// A channel can appear more than once.
pub fn touched<'a>(show: &'a Show, look: &'a Look) -> impl Iterator<Item = usize> + 'a {
    look.rows
        .iter()
        .flat_map(move |row| group_fixtures(show, row.group))
        .flat_map(|(_, _, f)| fixture_channels(f).map(|(i, _)| i))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::{Effect, Fade, Group, LookRow};
    use std::collections::BTreeSet;

    fn fx(address: u16, channels: &[Channel]) -> Fixture {
        Fixture {
            name: "f".into(),
            address,
            channels: channels.to_vec(),
            ofl: None,
        }
    }

    fn one(fixtures: Vec<Fixture>, color: [u8; 4], intensity: u8) -> (Show, Look) {
        let show = Show {
            groups: vec![Group {
                name: "g".into(),
                fixtures: (0..fixtures.len() as u8).collect(),
            }],
            fixtures,
            ..Show::default()
        };
        let look = Look {
            fade: Fade::Cut,
            rows: vec![LookRow {
                group: 0,
                color,
                intensity,
                effect: Effect::None,
                cooldown_s: 0,
                position: None,
                shape: crate::show::Shape::None,
            }],
        };
        (show, look)
    }

    fn run(show: &Show, look: &Look, level: u8) -> Universe {
        let mut out = [0; UNIVERSE];
        render(show, look, None, |_, _, _| level, &mut out);
        out
    }

    use Channel::*;

    #[test]
    fn rgb_par_without_dimmer_scales_colour() {
        let (s, l) = one(vec![fx(1, &[Red, Green, Blue])], [255, 128, 0, 0], 128);
        assert_eq!(run(&s, &l, 255)[0..3], [128, 64, 0]);
    }

    #[test]
    fn dimmer_takes_intensity() {
        let (s, l) = one(
            vec![fx(1, &[Dimmer, Red, Green, Blue])],
            [10, 20, 30, 0],
            200,
        );
        assert_eq!(run(&s, &l, 255)[0..4], [200, 10, 20, 30]);
    }

    #[test]
    fn level_scales_dimmer() {
        let (s, l) = one(
            vec![fx(1, &[Dimmer, Red, Green, Blue])],
            [10, 20, 30, 0],
            200,
        );
        assert_eq!(run(&s, &l, 128)[0], 100);
    }

    #[test]
    fn fixed_channel_keeps_value() {
        let (s, l) = one(vec![fx(1, &[Fixed(7), Dimmer])], [0; 4], 255);
        assert_eq!(run(&s, &l, 255)[0], 7);
    }

    #[test]
    fn fixture_past_512_is_cut() {
        let (s, l) = one(vec![fx(511, &[Fixed(9); 4])], [0; 4], 255);
        let out = run(&s, &l, 255);
        let set: Vec<usize> = (0..UNIVERSE).filter(|&i| out[i] != 0).collect();
        assert_eq!(set, [510, 511]);
        assert_eq!(touched(&s, &l).collect::<Vec<_>>(), [510, 511]);
    }

    #[test]
    fn touched_lists_group_channels() {
        let (s, l) = one(
            vec![fx(1, &[Red, Green, Blue]), fx(10, &[Red, Green, Blue])],
            [0; 4],
            255,
        );
        let set: BTreeSet<usize> = touched(&s, &l).collect();
        assert_eq!(set, BTreeSet::from([0, 1, 2, 9, 10, 11]));
    }

    #[test]
    fn wrong_group_is_skipped() {
        let (s, mut l) = one(vec![fx(1, &[Dimmer])], [0; 4], 255);
        l.rows[0].group = 5;
        assert_eq!(run(&s, &l, 255)[0], 0);
        assert_eq!(touched(&s, &l).count(), 0);
    }

    #[test]
    fn head_without_position_is_centred() {
        let (s, l) = one(vec![fx(1, &[Pan, PanFine, Tilt, TiltFine])], [0; 4], 255);
        assert_eq!(run(&s, &l, 255)[0..4], [0x80, 0, 0x80, 0]);
    }

    #[test]
    fn position_sets_pan_tilt() {
        let (mut s, mut l) = one(vec![fx(1, &[Pan, PanFine, Tilt, TiltFine])], [0; 4], 255);
        s.positions = vec![crate::show::Position {
            name: "p".into(),
            aims: vec![crate::show::Aim {
                fixture: 0,
                pan: 0x1234,
                tilt: 0xABCD,
            }],
        }];
        l.rows[0].position = Some(0);
        assert_eq!(run(&s, &l, 255)[0..4], [0x12, 0x34, 0xAB, 0xCD]);
    }

    #[test]
    fn coarse_only_head() {
        let (mut s, mut l) = one(vec![fx(1, &[Pan])], [0; 4], 255);
        s.positions = vec![crate::show::Position {
            name: "p".into(),
            aims: vec![crate::show::Aim {
                fixture: 0,
                pan: 0x1234,
                tilt: 0,
            }],
        }];
        l.rows[0].position = Some(0);
        assert_eq!(run(&s, &l, 255)[0], 0x12);
    }

    #[test]
    fn shape_clamps() {
        let (mut s, mut l) = one(vec![fx(1, &[Pan, PanFine])], [0; 4], 255);
        s.positions = vec![crate::show::Position {
            name: "p".into(),
            aims: vec![crate::show::Aim {
                fixture: 0,
                pan: 65000,
                tilt: 0,
            }],
        }];
        l.rows[0].position = Some(0);
        l.rows[0].shape = crate::show::Shape::Sweep {
            beats: 4,
            size: 255,
        };
        let b = Beat {
            phase: 0.0,
            beat_in_bar: 2,
            count: 1,
            ms_per_beat: 500.0,
        };
        let mut out = [0; UNIVERSE];
        render(&s, &l, Some(&b), |_, _, _| 255, &mut out);
        assert_eq!(out[0..2], [255, 255]);
    }
}
