//! The settings of the light autopilot.
//!
//! The autopilot picks console programs and DMX moves from the music energy. This module holds
//! only the data model. The show keeps it as JSON. No type here has an `f32` field, because LLVM
//! for Xtensa cannot compile one.

use serde::{Deserialize, Serialize};

use crate::show::{bounded, clean_to, MAX_NAME};

/// The most console programs of the autopilot.
pub const MAX_PROGRAMS: usize = 32;
/// The most moves of the autopilot. A `u32` bit mask holds the active moves.
pub const MAX_MOVES: usize = 24;
/// A spot below this value is in the tier Calm.
pub const CALM_BELOW: u8 = 34;
/// A spot at or above this value is in the tier Peak.
pub const PEAK_FROM: u8 = 80;
/// The allowed move periods in half beats: 1/2, 1, 2, 4, 8, 16 and 32 beats.
pub const HALF_BEATS: [u8; 7] = [1, 2, 4, 8, 16, 32, 64];
/// The longest PIN, in digits.
const MAX_PIN: usize = 8;

fn yes() -> bool {
    true
}

/// The energy band of a spot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Calm,
    #[default]
    Groove,
    Peak,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Calm, Tier::Groove, Tier::Peak];

    /// The tier of a spot from 0 to 100.
    pub fn of(spot: u8) -> Tier {
        if spot < CALM_BELOW {
            Tier::Calm
        } else if spot >= PEAK_FROM {
            Tier::Peak
        } else {
            Tier::Groove
        }
    }

    /// True when the spot is in this tier.
    pub fn contains(self, spot: u8) -> bool {
        Tier::of(spot) == self
    }
}

/// A console program that the autopilot can select with a program change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    /// The MIDI program number, 0 to 127.
    pub pc: u8,
    pub name: String,
    pub color: [u8; 3],
    /// The energy of the program, 0 to 100.
    pub spot: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveShape {
    Pulse,
    Ramp,
    Sweep,
    Circle,
    Figure8,
    Chase,
    Wave,
    StrobeBurst,
    Blackout,
    Hold,
}

/// The lights that a move drives. `Group` and `Fixture` hold an index into the show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Room,
    Group(u8),
    Fixture(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Param {
    PanTilt,
    Pan,
    Tilt,
    Dimmer,
    Colour,
    Strobe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Spread {
    Together,
    Fan,
    Mirror,
    Random,
}

/// One DMX move of the autopilot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    pub shape: MoveShape,
    pub target: Target,
    pub param: Param,
    /// The period in half beats. `sanitize` snaps it to a value of `HALF_BEATS`.
    pub half_beats: u8,
    /// The size of the move, 0 to 100.
    pub size: u8,
    pub spread: Spread,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
    /// True when the move runs faster at a higher energy.
    #[serde(default = "yes")]
    pub faster: bool,
    /// The energy of the move, 0 to 100.
    pub spot: u8,
}

/// How long the lights go dark before a drop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blackout {
    Off,
    Beat,
    #[default]
    HalfBar,
    Bar,
}

impl Blackout {
    /// The length of the blackout in beats.
    pub fn beats(self) -> f64 {
        match self {
            Blackout::Off => 0.0,
            Blackout::Beat => 1.0,
            Blackout::HalfBar => 2.0,
            Blackout::Bar => 4.0,
        }
    }
}

/// The settings of the drop effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DropCfg {
    pub blackout: Blackout,
    /// The length of the strobe after the drop, in beats.
    pub strobe_beats: u8,
    /// The least time between two drop strobes, in seconds.
    pub strobe_cooldown_s: u16,
    /// The energy dip that marks a drop.
    pub dip: u8,
}

impl Default for DropCfg {
    fn default() -> Self {
        DropCfg {
            blackout: Blackout::default(),
            strobe_beats: 2,
            strobe_cooldown_s: 120,
            dip: 128,
        }
    }
}

/// The autopilot settings of a show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Auto {
    pub on: bool,
    /// The MIDI channel of the program changes, 0 to 15.
    pub channel: u8,
    #[serde(deserialize_with = "bounded::<_, _, MAX_PROGRAMS>")]
    pub programs: Vec<Program>,
    #[serde(deserialize_with = "bounded::<_, _, MAX_MOVES>")]
    pub moves: Vec<Move>,
    pub drop: DropCfg,
    /// The colours of the tiers Calm, Groove and Peak.
    pub colors: [[u8; 3]; 3],
    /// Zero to eight digits. An empty PIN means no PIN.
    pub pin: String,
}

impl Default for Auto {
    fn default() -> Self {
        Auto {
            on: false,
            channel: 0,
            programs: Vec::new(),
            moves: Vec::new(),
            drop: DropCfg::default(),
            colors: [[255, 140, 40], [0, 224, 255], [255, 40, 140]],
            pin: String::new(),
        }
    }
}

/// The value of `HALF_BEATS` that is nearest to `v`. A tie goes to the higher value.
fn snap(v: u8) -> u8 {
    HALF_BEATS
        .iter()
        .rev()
        .copied()
        .min_by_key(|h| h.abs_diff(v))
        .unwrap_or(1)
}

impl Auto {
    /// True when the settings equal the defaults. The show then omits them from its JSON.
    pub fn is_default(&self) -> bool {
        *self == Auto::default()
    }

    /// Cut the settings to the limits. A move that names a missing group or fixture is dropped.
    pub fn sanitize(&mut self, show_fixtures: usize, show_groups: usize) {
        self.channel = self.channel.min(15);
        self.programs.truncate(MAX_PROGRAMS);
        for p in &mut self.programs {
            p.pc = p.pc.min(127);
            p.spot = p.spot.min(100);
            clean_to(&mut p.name, MAX_NAME);
        }
        self.moves.retain(|m| match m.target {
            Target::Room => true,
            Target::Group(g) => usize::from(g) < show_groups,
            Target::Fixture(f) => usize::from(f) < show_fixtures,
        });
        self.moves.truncate(MAX_MOVES);
        for m in &mut self.moves {
            m.half_beats = snap(m.half_beats);
            m.size = m.size.min(100);
            m.spot = m.spot.min(100);
        }
        if self.pin.len() > MAX_PIN || !self.pin.bytes().all(|b| b.is_ascii_digit()) {
            self.pin.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::Show;

    fn mv(target: Target) -> Move {
        Move {
            shape: MoveShape::Pulse,
            target,
            param: Param::Dimmer,
            half_beats: 4,
            size: 50,
            spread: Spread::Together,
            color: None,
            faster: true,
            spot: 50,
        }
    }

    fn prog(i: u8) -> Program {
        Program {
            pc: i,
            name: format!("p{i}"),
            color: [1, 2, 3],
            spot: 10,
        }
    }

    #[test]
    fn tier_bands() {
        assert_eq!(Tier::of(0), Tier::Calm);
        assert_eq!(Tier::of(33), Tier::Calm);
        assert_eq!(Tier::of(34), Tier::Groove);
        assert_eq!(Tier::of(79), Tier::Groove);
        assert_eq!(Tier::of(80), Tier::Peak);
        assert_eq!(Tier::of(100), Tier::Peak);
        assert!(Tier::Peak.contains(90));
        assert!(!Tier::Calm.contains(90));
    }

    #[test]
    fn old_show_has_default_auto() {
        let s = Show::from_json(
            br#"{"fixtures":[{"name":"P","address":1,"channels":["dimmer"]}],"groups":[],"looks":[],"idle":null}"#,
        );
        assert!(s.auto.is_default());
        assert!(!s.fixtures[0].console);
        let j = serde_json::to_string(&s).unwrap();
        assert!(!j.contains("auto"));
        assert!(!j.contains("console"));
    }

    #[test]
    fn auto_round_trip() {
        let mut s = Show::from_json(
            br#"{"fixtures":[{"name":"P","address":1,"channels":["dimmer"],"console":true}],
                 "groups":[{"name":"g","fixtures":[0]}],"looks":[],"idle":null}"#,
        );
        assert!(s.fixtures[0].console);
        s.auto.on = true;
        s.auto.programs = vec![prog(1), prog(2)];
        let mut m = mv(Target::Group(0));
        m.color = Some([9, 8, 7]);
        s.auto.moves = vec![m, mv(Target::Room)];
        s.auto.pin = "1234".into();
        let j = serde_json::to_vec(&s).unwrap();
        assert_eq!(Show::from_json(&j), s);
    }

    #[test]
    fn auto_sanitize() {
        let mut a = Auto {
            channel: 20,
            programs: (0..40).map(prog).collect(),
            moves: vec![
                mv(Target::Group(9)),
                mv(Target::Fixture(40)),
                mv(Target::Group(1)),
                mv(Target::Fixture(0)),
            ],
            pin: "12a4567890".into(),
            ..Auto::default()
        };
        a.programs[0].pc = 200;
        a.programs[0].spot = 150;
        a.programs[0].name = "x".repeat(30);
        a.moves[2].half_beats = 3;
        a.moves[2].size = 180;
        a.sanitize(2, 2);
        assert_eq!(a.programs.len(), 32);
        assert_eq!(a.programs[0].pc, 127);
        assert_eq!(a.programs[0].spot, 100);
        assert_eq!(a.channel, 15);
        assert_eq!(a.programs[0].name.chars().count(), 16);
        assert_eq!(a.moves.len(), 2);
        assert_eq!(a.moves[0].half_beats, 4);
        assert_eq!(a.moves[0].size, 100);
        assert_eq!(a.pin, "");
    }

    #[test]
    fn half_beats_snap() {
        assert_eq!(snap(0), 1);
        assert_eq!(snap(3), 4);
        assert_eq!(snap(5), 4);
        assert_eq!(snap(6), 8);
        assert_eq!(snap(200), 64);
    }

    #[test]
    fn pin_rules() {
        let mut a = Auto {
            pin: "12345678".into(),
            ..Auto::default()
        };
        a.sanitize(0, 0);
        assert_eq!(a.pin, "12345678");
        a.pin = "123456789".into();
        a.sanitize(0, 0);
        assert_eq!(a.pin, "");
    }

    #[test]
    fn target_json() {
        assert_eq!(serde_json::to_string(&Target::Room).unwrap(), "\"room\"");
        assert_eq!(
            serde_json::to_string(&Target::Group(1)).unwrap(),
            "{\"group\":1}"
        );
        assert_eq!(
            serde_json::to_string(&Target::Fixture(3)).unwrap(),
            "{\"fixture\":3}"
        );
    }

    #[test]
    fn faster_defaults_true() {
        let m: Move = serde_json::from_str(
            r#"{"shape":"pulse","target":"room","param":"dimmer","half_beats":2,"size":5,"spread":"fan","spot":1}"#,
        )
        .unwrap();
        assert!(m.faster);
    }
}
