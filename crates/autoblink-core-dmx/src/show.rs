//! The show model: the fixtures, the groups and the looks of one box.
//!
//! The firmware keeps a show as JSON. `Show::from_json` always returns a valid show.
//! No type here has an `f32` field, because LLVM for Xtensa cannot compile one.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// The number of channels in one DMX universe.
pub const UNIVERSE: usize = 512;
pub const MAX_FIXTURES: usize = 32;
pub const MAX_CHANNELS: usize = 32;
pub const MAX_GROUPS: usize = 8;
pub const MAX_LOOKS: usize = 32;
pub const MAX_NAME: usize = 16;
/// The longest OFL source of a fixture, in characters.
pub const MAX_OFL: usize = 96;
/// The most rows of one look. A row sets one group, so more rows than groups give nothing.
pub const MAX_ROWS: usize = MAX_GROUPS;
/// The longest cooldown of a look row, in seconds.
const MAX_COOLDOWN_S: u16 = 600;

/// The function of one DMX channel of a fixture.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Dimmer,
    Red,
    Green,
    Blue,
    White,
    Amber,
    Uv,
    Strobe,
    Fog,
    /// A channel that always sends this value.
    Fixed(u8),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fixture {
    pub name: String,
    /// The DMX start channel, 1 to 512.
    pub address: u16,
    #[serde(deserialize_with = "bounded::<_, _, MAX_CHANNELS>")]
    pub channels: Vec<Channel>,
    /// The Open Fixture Library source: `<manufacturer>/<model>#<mode name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ofl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// Indices into `Show::fixtures`.
    #[serde(deserialize_with = "bounded::<_, _, MAX_FIXTURES>")]
    pub fixtures: Vec<u8>,
}

/// How a look changes the light when it starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fade {
    #[default]
    Cut,
    Beat,
    Bar,
}

/// How often an effect repeats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Every {
    #[default]
    Beat,
    Half,
    Bar,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    #[default]
    None,
    Flash {
        every: Every,
    },
    Pulse {
        every: Every,
    },
    Chase {
        every: Every,
    },
    Random {
        every: Every,
    },
    Strobe {
        rate: u8,
        beats: u8,
    },
    Fog {
        ms: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookRow {
    /// Index into `Show::groups`.
    pub group: u8,
    /// Red, green, blue and white.
    pub color: [u8; 4],
    pub intensity: u8,
    #[serde(default)]
    pub effect: Effect,
    #[serde(default)]
    pub cooldown_s: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Look {
    #[serde(default)]
    pub fade: Fade,
    #[serde(deserialize_with = "bounded::<_, _, MAX_ROWS>")]
    pub rows: Vec<LookRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Show {
    #[serde(deserialize_with = "bounded::<_, _, MAX_FIXTURES>")]
    pub fixtures: Vec<Fixture>,
    #[serde(deserialize_with = "bounded::<_, _, MAX_GROUPS>")]
    pub groups: Vec<Group>,
    /// `looks[i]` is the look of preset `i`.
    #[serde(deserialize_with = "bounded::<_, _, MAX_LOOKS>")]
    pub looks: Vec<Option<Look>>,
    /// The preset that plays when no other look plays.
    pub idle: Option<u8>,
}

/// Reads a JSON list and keeps only its first `N` entries. It reads the other entries as
/// `IgnoredAny`, so they use no memory. Without this limit, a long list can use all of the heap
/// of the firmware before `Show::sanitize` runs.
fn bounded<'de, D, T, const N: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const N: usize>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Bounded<T, N> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut v = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
            while v.len() < N {
                match seq.next_element()? {
                    Some(x) => v.push(x),
                    None => return Ok(v),
                }
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(v)
        }
    }

    d.deserialize_seq(Bounded::<T, N>(PhantomData))
}

fn clean_to(name: &mut String, max: usize) {
    *name = name.chars().filter(|c| !c.is_control()).take(max).collect();
}

fn clean(name: &mut String) {
    clean_to(name, MAX_NAME);
}

impl Show {
    /// Cut the show to the limits and remove each wrong reference.
    pub fn sanitize(&mut self) {
        self.fixtures.truncate(MAX_FIXTURES);
        for f in &mut self.fixtures {
            clean(&mut f.name);
            if let Some(o) = &mut f.ofl {
                clean_to(o, MAX_OFL);
            }
            f.address = f.address.clamp(1, UNIVERSE as u16);
            f.channels.truncate(MAX_CHANNELS);
        }
        self.groups.truncate(MAX_GROUPS);
        let fixtures = self.fixtures.len();
        for g in &mut self.groups {
            clean(&mut g.name);
            g.fixtures.retain(|&i| usize::from(i) < fixtures);
        }
        self.looks.truncate(MAX_LOOKS);
        let groups = self.groups.len();
        for look in self.looks.iter_mut().flatten() {
            look.rows.retain(|r| usize::from(r.group) < groups);
            look.rows.truncate(MAX_ROWS);
            for r in &mut look.rows {
                r.cooldown_s = r.cooldown_s.min(MAX_COOLDOWN_S);
                match &mut r.effect {
                    Effect::Strobe { beats, .. } => *beats = (*beats).clamp(1, 16),
                    Effect::Fog { ms } => *ms = (*ms).clamp(100, 10000),
                    _ => {}
                }
            }
        }
        if self.idle.is_some_and(|i| self.look(i).is_none()) {
            self.idle = None;
        }
    }

    /// The look of a preset, or `None` when the preset has no look.
    pub fn look(&self, preset: u8) -> Option<&Look> {
        self.looks.get(usize::from(preset))?.as_ref()
    }

    /// Read a show from JSON. A bad document gives the empty show.
    pub fn from_json(bytes: &[u8]) -> Show {
        let mut show: Show = serde_json::from_slice(bytes).unwrap_or_default();
        show.sanitize();
        show
    }
}

#[cfg(test)]
mod tests {
    // The tests build a show field by field, as the page does.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    fn fixture(n: usize) -> Fixture {
        Fixture {
            name: format!("f{n}"),
            address: 1,
            channels: vec![Channel::Dimmer],
            ofl: None,
        }
    }

    fn row(group: u8, effect: Effect) -> LookRow {
        LookRow {
            group,
            color: [1, 2, 3, 4],
            intensity: 255,
            effect,
            cooldown_s: 0,
        }
    }

    #[test]
    fn sanitize_cuts_limits() {
        let mut s = Show::default();
        s.fixtures = (0..40).map(fixture).collect();
        s.fixtures[0].name = "x".repeat(30);
        s.fixtures[0].channels = vec![Channel::Red; 40];
        s.groups = (0..10)
            .map(|_| Group {
                name: "g".into(),
                fixtures: vec![],
            })
            .collect();
        s.looks = (0..40).map(|_| None).collect();
        s.sanitize();
        assert_eq!(s.fixtures.len(), 32);
        assert_eq!(s.fixtures[0].name.chars().count(), 16);
        assert_eq!(s.fixtures[0].channels.len(), 32);
        assert_eq!(s.groups.len(), 8);
        assert_eq!(s.looks.len(), 32);
    }

    #[test]
    fn sanitize_drops_bad_refs() {
        let mut s = Show::default();
        s.fixtures = vec![fixture(0)];
        s.groups = vec![Group {
            name: "a".into(),
            fixtures: vec![0, 99],
        }];
        s.looks = vec![
            Some(Look {
                fade: Fade::Cut,
                rows: vec![row(0, Effect::None), row(9, Effect::None)],
            }),
            None,
            None,
            None,
            None,
            None,
        ];
        s.idle = Some(5);
        s.sanitize();
        assert_eq!(s.groups[0].fixtures, [0]);
        assert_eq!(s.looks[0].as_ref().unwrap().rows.len(), 1);
        assert_eq!(s.idle, None);
        s.idle = Some(0);
        s.sanitize();
        assert_eq!(s.idle, Some(0));
    }

    #[test]
    fn sanitize_clamps_values() {
        let mut s = Show::default();
        s.fixtures = vec![fixture(0), fixture(1)];
        s.fixtures[0].address = 0;
        s.fixtures[1].address = 600;
        s.groups = vec![Group {
            name: "a".into(),
            fixtures: vec![0],
        }];
        let mut rows = vec![
            row(0, Effect::Strobe { rate: 5, beats: 0 }),
            row(0, Effect::Strobe { rate: 5, beats: 99 }),
            row(0, Effect::Fog { ms: 5 }),
            row(0, Effect::Fog { ms: 60000 }),
            row(0, Effect::None),
        ];
        rows[4].cooldown_s = 5000;
        s.looks = vec![Some(Look {
            fade: Fade::Cut,
            rows,
        })];
        s.sanitize();
        assert_eq!((s.fixtures[0].address, s.fixtures[1].address), (1, 512));
        let r = &s.looks[0].as_ref().unwrap().rows;
        assert_eq!(r[0].effect, Effect::Strobe { rate: 5, beats: 1 });
        assert_eq!(r[1].effect, Effect::Strobe { rate: 5, beats: 16 });
        assert_eq!(r[2].effect, Effect::Fog { ms: 100 });
        assert_eq!(r[3].effect, Effect::Fog { ms: 10000 });
        assert_eq!(r[4].cooldown_s, 600);
    }

    #[test]
    fn ofl_round_trip() {
        let mut s = Show::default();
        let mut f = fixture(0);
        f.ofl = Some("eurolite/led-bar-6-qcl-rgbw#4-channel".into());
        s.fixtures = vec![f, fixture(1)];
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json.matches("\"ofl\"").count(), 1);
        assert_eq!(Show::from_json(json.as_bytes()), s);
    }

    #[test]
    fn ofl_sanitize() {
        let mut s = Show::default();
        let mut f = fixture(0);
        f.ofl = Some("a".repeat(200));
        let mut g = fixture(1);
        g.ofl = Some("ab\ncd".into());
        s.fixtures = vec![f, g];
        s.sanitize();
        assert_eq!(s.fixtures[0].ofl.as_ref().unwrap().chars().count(), 96);
        assert_eq!(s.fixtures[1].ofl.as_deref(), Some("abcd"));
    }

    #[test]
    fn json_round_trip() {
        let mut s = Show::default();
        s.fixtures = vec![Fixture {
            name: "Par".into(),
            address: 10,
            channels: vec![
                Channel::Dimmer,
                Channel::Red,
                Channel::Green,
                Channel::Blue,
                Channel::White,
                Channel::Amber,
                Channel::Uv,
                Channel::Strobe,
                Channel::Fog,
                Channel::Fixed(5),
            ],
            ofl: None,
        }];
        s.groups = vec![Group {
            name: "all".into(),
            fixtures: vec![0],
        }];
        let effects = [
            Effect::None,
            Effect::Flash { every: Every::Beat },
            Effect::Pulse { every: Every::Half },
            Effect::Chase { every: Every::Bar },
            Effect::Random { every: Every::Beat },
            Effect::Strobe { rate: 8, beats: 2 },
            Effect::Fog { ms: 500 },
        ];
        s.looks = vec![Some(Look {
            fade: Fade::Bar,
            rows: effects.into_iter().map(|e| row(0, e)).collect(),
        })];
        s.idle = Some(0);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#"{"fixed":5}"#));
        assert!(json.contains(r#""dimmer""#));
        assert_eq!(Show::from_json(json.as_bytes()), s);
    }

    /// A JSON list with `n` copies of `item`.
    fn list(item: &str, n: usize) -> String {
        format!("[{}]", vec![item; n].join(","))
    }

    #[test]
    fn read_cuts_looks_without_sanitize() {
        let json = format!(r#"{{"looks":{}}}"#, list("null", 40));
        let s: Show = serde_json::from_str(&json).unwrap();
        assert_eq!(s.looks.len(), MAX_LOOKS);
    }

    #[test]
    fn read_cuts_rows_without_sanitize() {
        let row = r#"{"group":0,"color":[0,0,0,0],"intensity":0}"#;
        let json = format!(r#"{{"looks":[{{"rows":{}}}]}}"#, list(row, 20));
        let s: Show = serde_json::from_str(&json).unwrap();
        assert_eq!(s.looks[0].as_ref().unwrap().rows.len(), MAX_ROWS);
    }

    #[test]
    fn read_continues_after_a_cut_list() {
        let fixture = format!(
            r#"{{"name":"a","channels":{},"address":7}}"#,
            list(r#"{"fixed":1}"#, 40)
        );
        let group = format!(r#"{{"fixtures":{},"name":"g"}}"#, list("0", 40));
        let json = format!(
            r#"{{"fixtures":{},"groups":{},"looks":{},"idle":3}}"#,
            list(&fixture, 40),
            list(&group, 10),
            list("null", 40)
        );
        let s: Show = serde_json::from_str(&json).unwrap();
        assert_eq!(s.fixtures.len(), MAX_FIXTURES);
        assert_eq!(s.fixtures[31].channels.len(), MAX_CHANNELS);
        assert_eq!(s.fixtures[31].address, 7);
        assert_eq!(s.groups.len(), MAX_GROUPS);
        assert_eq!(s.groups[7].fixtures.len(), MAX_FIXTURES);
        assert_eq!(s.groups[7].name, "g");
        assert_eq!(s.idle, Some(3));
    }

    #[test]
    fn sanitize_cuts_rows() {
        let mut s = Show::default();
        s.fixtures = vec![fixture(0)];
        s.groups = vec![Group {
            name: "a".into(),
            fixtures: vec![0],
        }];
        s.looks = vec![Some(Look {
            fade: Fade::Cut,
            rows: vec![row(0, Effect::None); 20],
        })];
        s.sanitize();
        assert_eq!(s.looks[0].as_ref().unwrap().rows.len(), MAX_ROWS);
    }

    #[test]
    fn load_bad_json_gives_default() {
        assert_eq!(Show::from_json(b"\xff\xff"), Show::default());
        assert_eq!(Show::from_json(b"{\"fixtures\":7}"), Show::default());
    }
}
