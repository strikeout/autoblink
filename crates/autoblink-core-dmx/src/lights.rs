//! The Lights engine: scene notes and fired events become a DMX frame.
//!
//! The firmware calls `frame` about 42 times a second. `frame` allocates no memory.

use crate::clock::Beat;
use crate::effect::factor;
use crate::moves::{is_light, render_auto, room_factor, AutoLook};
use crate::pilot::{Action, BUILD_BEATS};
use crate::render::{fixture_channels, group_fixtures, render, touched, Universe};
use crate::show::{Channel, Effect, Fade, Show, MAX_FIXTURES, UNIVERSE};
use autoblink_core_midi::scenes::Event;

/// The live controls of the page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Live {
    /// 255 is full.
    pub master: u8,
    pub strobe_hold: bool,
    /// True when the page holds the whole room dark.
    pub blackout: bool,
}

/// A look shows for at least this long, also after a short note.
pub const MIN_ON_MS: u64 = 80;
/// The idle look shows this long after the stop of the playback.
pub const IDLE_AFTER_MS: u64 = 2000;
/// The fog button blows for this long.
pub const FOG_BUTTON_MS: u64 = 3000;
/// An aim from the page holds the pan and tilt of a fixture for this long.
pub const AIM_MS: u64 = 10_000;

const DEFAULT_MS_PER_BEAT: f64 = 500.0;

struct Layer {
    preset: u8,
    on_ms: u64,
    manual: bool,
    off_at: Option<u64>,
}

/// A `Strobe` or `Fog` row that fired. It forces one channel kind of a group.
struct Burst {
    group: u8,
    channel: Channel,
    value: u8,
    until: u64,
}

/// The pan, the tilt and the end time of the aim of one fixture.
#[derive(Clone, Copy)]
struct AimHold {
    pan: u16,
    tilt: u16,
    until: u64,
}

#[derive(Default)]
pub struct Lights {
    /// The aim of each fixture. A fixed array, so that `frame` allocates nothing.
    aims: [Option<AimHold>; MAX_FIXTURES],
    layers: Vec<Layer>,
    bursts: Vec<Burst>,
    /// The last fire time of a row: (preset, row index, ms).
    fires: Vec<(u8, usize, u64)>,
    stopped_at: Option<u64>,
    last_on_ms: Option<u64>,
    fog_until: u64,
    ms_per_beat: Option<f64>,
    /// What the pilot told the engine: tier, energy and moves.
    look: AutoLook,
    /// The start of a build and the time of the drop, when known.
    build: Option<(u64, Option<u64>)>,
    /// The time of a Hit that strobes the box fixtures.
    hit_at: Option<u64>,
    /// The move that runs alone, and the end time.
    trial: Option<(u8, u64)>,
}

/// The progress 0.0..=1.0 of a build. Without a drop time, the climb takes `BUILD_BEATS` beats.
fn build_fraction(from: u64, drop: Option<u64>, now_ms: u64, mpb: f64) -> f64 {
    let span = match drop {
        Some(d) => d.saturating_sub(from) as f64,
        None => BUILD_BEATS * mpb,
    };
    if span <= 0.0 {
        return 1.0;
    }
    (now_ms.saturating_sub(from) as f64 / span).clamp(0.0, 1.0)
}

impl Lights {
    /// A scene note on.
    pub fn on(&mut self, preset: u8, now_ms: u64) {
        self.last_on_ms = Some(now_ms);
        if let Some(l) = self.layers.iter_mut().find(|l| l.preset == preset) {
            l.off_at = None;
            l.manual = false;
            return;
        }
        self.layers.push(Layer {
            preset,
            on_ms: now_ms,
            manual: false,
            off_at: None,
        });
    }

    /// A scene note off.
    pub fn off(&mut self, preset: u8, now_ms: u64) {
        let Some(i) = self
            .layers
            .iter()
            .position(|l| l.preset == preset && !l.manual)
        else {
            return;
        };
        let min_end = self.layers[i].on_ms + MIN_ON_MS;
        if now_ms < min_end {
            self.layers[i].off_at = Some(min_end);
        } else {
            self.layers.remove(i);
        }
    }

    /// An event of the scenes fired a preset.
    pub fn fired(&mut self, show: &Show, event: Event, preset: u8, now_ms: u64) {
        if event.is_phrase_event() {
            self.start_bursts(show, preset, now_ms, false);
        }
        if !event.is_overlay() {
            self.layers.retain(|l| !l.manual);
        }
    }

    /// A click on the page: the preset shows as a manual look.
    pub fn test(&mut self, show: &Show, preset: u8, now_ms: u64) {
        self.layers.retain(|l| !l.manual);
        self.layers.push(Layer {
            preset,
            on_ms: now_ms,
            manual: true,
            off_at: None,
        });
        self.last_on_ms = Some(now_ms);
        self.start_bursts(show, preset, now_ms, true);
    }

    /// The fog button.
    pub fn fog(&mut self, now_ms: u64) {
        self.fog_until = now_ms + FOG_BUTTON_MS;
    }

    /// The page aims a fixture. The aim holds the pan and tilt over every layer until `AIM_MS` pass.
    pub fn aim(&mut self, fixture: u8, pan: u16, tilt: u16, now_ms: u64) {
        if let Some(slot) = self.aims.get_mut(usize::from(fixture)) {
            *slot = Some(AimHold {
                pan,
                tilt,
                until: now_ms + AIM_MS,
            });
        }
    }

    /// An action of the pilot. The state stays until the next action. The engine ignores the
    /// state while `show.auto.on` is false.
    pub fn auto(&mut self, action: Action, now_ms: u64) {
        match action {
            Action::Program(_) => {}
            Action::Moves(mask) => self.look.moves = mask,
            Action::Energy { tier, spot } => {
                self.look.tier = tier;
                self.look.spot = spot;
            }
            Action::Build {
                from_ms,
                drop_at_ms,
            } => {
                self.build = Some((from_ms, drop_at_ms));
                self.hit_at = None;
            }
            Action::Hit { strobe } => {
                self.build = None;
                self.hit_at = strobe.then_some(now_ms);
            }
            Action::EndBuild => self.build = None,
        }
    }

    /// Runs move `index` alone for 8 bars (32 beats). Then the normal set returns.
    pub fn try_move(&mut self, index: u8, now_ms: u64) {
        let mpb = self.ms_per_beat.unwrap_or(DEFAULT_MS_PER_BEAT);
        self.trial = Some((index, now_ms + (32.0 * mpb) as u64));
    }

    /// The (console, all) room levels for `apply_room`, after the merge. `console` is the dip
    /// of a build for the console fixtures. `all` is 0 in the blackout before a drop, and with
    /// the blackout of the page.
    pub fn room(&self, show: &Show, beat: Option<&Beat>, live: &Live, now_ms: u64) -> (u8, u8) {
        if !show.auto.on {
            return (255, if live.blackout { 0 } else { 255 });
        }
        let mpb = beat
            .map(|b| b.ms_per_beat)
            .or(self.ms_per_beat)
            .unwrap_or(DEFAULT_MS_PER_BEAT);
        let mut console = 255;
        let mut dark = live.blackout;
        if let Some((from, drop)) = self.build {
            let frac = build_fraction(from, drop, now_ms, mpb);
            let dip = f64::from(show.auto.drop.dip);
            console = (dip + (255.0 - dip) * frac).round() as u8;
            if let Some(d) = drop {
                let start = d as f64 - show.auto.drop.blackout.beats() * mpb;
                dark |= now_ms < d && now_ms as f64 >= start;
            }
        }
        let st = self.auto_look(now_ms, mpb);
        let all = if dark {
            0
        } else {
            room_factor(show, &st, beat)
        };
        (console, all)
    }

    /// What the autopilot shows at `now_ms`.
    fn auto_look(&self, now_ms: u64, mpb: f64) -> AutoLook {
        let mut st = self.look;
        if let Some((i, until)) = self.trial {
            if now_ms < until {
                st.moves = 1u32.checked_shl(u32::from(i)).unwrap_or(0);
            }
        }
        if let Some((from, drop)) = self.build {
            let frac = build_fraction(from, drop, now_ms, mpb);
            st.level = (128.0 + 127.0 * frac).round() as u8;
            if frac >= 0.5 {
                st.speed = 2;
            }
        }
        st
    }

    pub fn set_playing(&mut self, playing: bool, now_ms: u64) {
        self.stopped_at = if playing { None } else { Some(now_ms) };
    }

    /// The presets that show now, bottom first, for the page.
    pub fn layers(&self) -> impl Iterator<Item = u8> + '_ {
        self.layers.iter().map(|l| l.preset)
    }

    fn start_bursts(&mut self, show: &Show, preset: u8, now_ms: u64, ignore_cooldown: bool) {
        let Some(look) = show.look(preset) else {
            return;
        };
        let mpb = self.ms_per_beat.unwrap_or(DEFAULT_MS_PER_BEAT);
        for (r, row) in look.rows.iter().enumerate() {
            let (channel, value, ms) = match row.effect {
                Effect::Strobe { rate, beats } => (Channel::Strobe, rate, f64::from(beats) * mpb),
                Effect::Fog { ms } => (Channel::Fog, 255, f64::from(ms)),
                _ => continue,
            };
            let last = self.fires.iter_mut().find(|f| f.0 == preset && f.1 == r);
            if !ignore_cooldown {
                if let Some(f) = &last {
                    if now_ms.saturating_sub(f.2) < u64::from(row.cooldown_s) * 1000 {
                        continue;
                    }
                }
            }
            match last {
                Some(f) => f.2 = now_ms,
                None => self.fires.push((preset, r, now_ms)),
            }
            self.bursts.push(Burst {
                group: row.group,
                channel,
                value,
                until: now_ms + ms as u64,
            });
        }
    }

    /// The base, the moves and the strobe of a hit.
    fn frame_auto(
        &mut self,
        show: &Show,
        beat: Option<&Beat>,
        now_ms: u64,
        mpb: f64,
        out: &mut Universe,
    ) {
        if self.trial.is_some_and(|(_, until)| until <= now_ms) {
            self.trial = None;
        }
        let st = self.auto_look(now_ms, mpb);
        render_auto(show, &st, beat, now_ms, out);
        let hit_ms = f64::from(show.auto.drop.strobe_beats) * mpb;
        if self
            .hit_at
            .is_some_and(|t| (now_ms.saturating_sub(t) as f64) < hit_ms)
        {
            for f in show.fixtures.iter().filter(|f| !f.console) {
                for (i, c) in fixture_channels(f) {
                    if c == Channel::Strobe {
                        out[i] = 255;
                    }
                }
            }
        }
    }

    /// Writes the DMX frame for `now_ms` into `out`.
    pub fn frame(
        &mut self,
        show: &Show,
        beat: Option<&Beat>,
        live: &Live,
        now_ms: u64,
        out: &mut Universe,
    ) {
        if let Some(b) = beat {
            self.ms_per_beat = Some(b.ms_per_beat);
        }
        let mpb = beat.map_or(DEFAULT_MS_PER_BEAT, |b| b.ms_per_beat);
        self.layers.retain(|l| l.off_at.is_none_or(|t| t > now_ms));
        self.bursts.retain(|b| b.until > now_ms);
        out.fill(0);
        // A head that no look aims sits at the centre, not at the end stop.
        for f in &show.fixtures {
            for (i, c) in fixture_channels(f) {
                if matches!(c, Channel::Pan | Channel::Tilt) {
                    out[i] = 0x80;
                }
            }
        }

        if show.auto.on {
            self.frame_auto(show, beat, now_ms, mpb, out);
        } else {
            let stopped = self.stopped_at.is_some_and(|s| {
                now_ms >= s + IDLE_AFTER_MS && self.last_on_ms.is_none_or(|o| o < s)
            });
            let mut scratch = [0u8; UNIVERSE];
            // The idle look is the bottom layer. Thus a layer without a look never blacks out the rig.
            if let Some(look) = show.idle.and_then(|p| show.look(p)) {
                render(
                    show,
                    look,
                    beat,
                    |r, k, n| factor(&look.rows[r].effect, beat, k, n),
                    out,
                );
            }
            if !stopped {
                for layer in &self.layers {
                    let Some(look) = show.look(layer.preset) else {
                        continue;
                    };
                    render(
                        show,
                        look,
                        beat,
                        |r, k, n| factor(&look.rows[r].effect, beat, k, n),
                        &mut scratch,
                    );
                    let fade_ms = match look.fade {
                        Fade::Cut => 0.0,
                        Fade::Beat => mpb,
                        Fade::Bar => 4.0 * mpb,
                    };
                    let age = now_ms.saturating_sub(layer.on_ms) as f64;
                    let alpha = if fade_ms > 0.0 {
                        (age / fade_ms).min(1.0)
                    } else {
                        1.0
                    };
                    // ponytail: a fade between two positions blends the coarse and the fine byte
                    // alone. A pan across a byte border can jump for a moment. Blend the 16-bit pairs to fix it.
                    for ch in touched(show, look) {
                        let under = f64::from(out[ch]);
                        let top = f64::from(scratch[ch]);
                        out[ch] = (under + (top - under) * alpha).round() as u8;
                    }
                }
            }
        }

        for (i, slot) in self.aims.iter_mut().enumerate() {
            if slot.is_some_and(|a| a.until <= now_ms) {
                *slot = None;
            }
            let (Some(a), Some(f)) = (*slot, show.fixtures.get(i)) else {
                continue;
            };
            for (ch, c) in fixture_channels(f) {
                out[ch] = match c {
                    Channel::Pan => (a.pan >> 8) as u8,
                    Channel::PanFine => a.pan as u8,
                    Channel::Tilt => (a.tilt >> 8) as u8,
                    Channel::TiltFine => a.tilt as u8,
                    _ => continue,
                };
            }
        }

        for b in &self.bursts {
            for (_, _, f) in group_fixtures(show, b.group) {
                for (i, c) in fixture_channels(f) {
                    if c == b.channel {
                        out[i] = b.value;
                    }
                }
            }
        }
        let fog = now_ms < self.fog_until;
        if fog || live.strobe_hold {
            for f in &show.fixtures {
                for (i, c) in fixture_channels(f) {
                    match c {
                        Channel::Fog if fog => out[i] = 255,
                        Channel::Strobe if live.strobe_hold => out[i] = 255,
                        _ => {}
                    }
                }
            }
        }
        if live.master != 255 {
            let m = u32::from(live.master);
            for f in &show.fixtures {
                let has_dimmer = f.channels.contains(&Channel::Dimmer);
                for (i, c) in fixture_channels(f) {
                    if is_light(c, has_dimmer) {
                        out[i] = (u32::from(out[i]) * m / 255) as u8;
                    } else if c == Channel::Strobe && m == 0 {
                        out[i] = 0;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::{Fixture, Group, Look, LookRow};

    fn row(group: u8, color: [u8; 4], intensity: u8, effect: Effect, cooldown_s: u16) -> LookRow {
        LookRow {
            group,
            color,
            intensity,
            effect,
            cooldown_s,
            position: None,
            shape: crate::show::Shape::None,
        }
    }

    fn par(color: [u8; 4], intensity: u8, fade: Fade) -> Look {
        Look {
            fade,
            rows: vec![row(0, color, intensity, Effect::None, 0)],
        }
    }

    fn fixture(address: u16, channels: &[Channel]) -> Fixture {
        Fixture {
            name: "f".into(),
            address,
            channels: channels.to_vec(),
            ofl: None,
            console: false,
        }
    }

    fn group(f: u8) -> Group {
        Group {
            name: "g".into(),
            fixtures: vec![f],
        }
    }

    fn show() -> Show {
        use Channel::*;
        Show {
            fixtures: vec![
                fixture(1, &[Dimmer, Red, Green, Blue]),
                fixture(10, &[Strobe]),
                fixture(20, &[Fog]),
            ],
            groups: vec![group(0), group(1), group(2)],
            looks: vec![
                Some(par([255, 0, 0, 0], 255, Fade::Cut)),
                Some(par([0, 0, 255, 0], 255, Fade::Cut)),
                Some(Look {
                    fade: Fade::Cut,
                    rows: vec![
                        row(
                            1,
                            [0; 4],
                            255,
                            Effect::Strobe {
                                rate: 200,
                                beats: 2,
                            },
                            30,
                        ),
                        row(2, [0; 4], 255, Effect::Fog { ms: 1000 }, 30),
                    ],
                }),
                Some(par([255, 255, 255, 0], 50, Fade::Cut)),
            ],
            idle: Some(3),
            positions: Vec::new(),
            auto: crate::auto::Auto::default(),
        }
    }

    const LIVE: Live = Live {
        master: 255,
        strobe_hold: false,
        blackout: false,
    };

    fn frame(l: &mut Lights, s: &Show, now: u64) -> Universe {
        let mut out = [0; UNIVERSE];
        l.frame(s, None, &LIVE, now, &mut out);
        out
    }

    #[test]
    fn layers_stack_and_unstack() {
        let s = show();
        let mut l = Lights::default();
        l.on(0, 0);
        l.on(1, 100);
        let f = frame(&mut l, &s, 200);
        assert_eq!((f[1], f[3]), (0, 255));
        l.off(1, 300);
        let f = frame(&mut l, &s, 400);
        assert_eq!((f[1], f[3]), (255, 0));
        assert_eq!(l.layers().collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn pulse_shows_for_min_on() {
        let s = show();
        let mut l = Lights::default();
        l.on(1, 1000);
        l.off(1, 1000);
        assert_eq!(frame(&mut l, &s, 1040)[3], 255);
        let f = frame(&mut l, &s, 1081);
        assert_eq!((f[0], f[3]), (50, 255));
        assert_eq!(f[1], 255);
        assert_eq!(l.layers().count(), 0);
    }

    #[test]
    fn fade_beat() {
        let mut s = show();
        s.looks[1] = Some(par([0, 0, 255, 0], 255, Fade::Beat));
        let beat = Beat {
            phase: 0.0,
            beat_in_bar: 1,
            count: 0,
            ms_per_beat: 500.0,
        };
        let mut l = Lights::default();
        l.on(1, 0);
        let mut out = [0; UNIVERSE];
        l.frame(&s, Some(&beat), &LIVE, 250, &mut out);
        // The idle look (white, dimmer 50) is under the blue look (dimmer 255).
        for (ch, want) in [(0, 152), (1, 127), (2, 127), (3, 255)] {
            assert!(out[ch].abs_diff(want) <= 2, "{ch}: {}", out[ch]);
        }
    }

    #[test]
    fn layer_without_look_shows_idle() {
        let s = show();
        let mut l = Lights::default();
        l.on(9, 0);
        assert_eq!(frame(&mut l, &s, 10)[0..4], [50, 255, 255, 255]);
    }

    #[test]
    fn on_makes_a_manual_layer_scene_held() {
        let s = show();
        let mut l = Lights::default();
        l.test(&s, 1, 0);
        l.on(1, 10);
        l.fired(&s, Event::Drop, 1, 20);
        assert_eq!(l.layers().collect::<Vec<_>>(), [1]);
        assert_eq!(frame(&mut l, &s, 30)[3], 255);
    }

    #[test]
    fn strobe_only_on_phrase_event() {
        let s = show();
        let mut l = Lights::default();
        l.on(2, 0);
        l.fired(&s, Event::Beat, 2, 0);
        assert_eq!(frame(&mut l, &s, 10)[9], 0);
        l.fired(&s, Event::Drop, 2, 100);
        assert_eq!(frame(&mut l, &s, 200)[9], 200);
        assert_eq!(frame(&mut l, &s, 1099)[9], 200);
        assert_eq!(frame(&mut l, &s, 1101)[9], 0);
    }

    #[test]
    fn strobe_cooldown() {
        let s = show();
        let mut l = Lights::default();
        l.on(2, 0);
        l.fired(&s, Event::Drop, 2, 0);
        assert_eq!(frame(&mut l, &s, 100)[9], 200);
        l.fired(&s, Event::Drop, 2, 10_000);
        assert_eq!(frame(&mut l, &s, 10_100)[9], 0);
        l.fired(&s, Event::Drop, 2, 31_000);
        assert_eq!(frame(&mut l, &s, 31_100)[9], 200);
    }

    #[test]
    fn test_ignores_cooldown() {
        let s = show();
        let mut l = Lights::default();
        l.fired(&s, Event::Drop, 2, 0);
        l.test(&s, 2, 1000);
        assert_eq!(frame(&mut l, &s, 1100)[9], 200);
    }

    #[test]
    fn manual_layer_ends_on_next_scene() {
        let s = show();
        let mut l = Lights::default();
        l.test(&s, 0, 0);
        l.fired(&s, Event::Beat, 1, 10);
        assert_eq!(l.layers().collect::<Vec<_>>(), [0]);
        l.fired(&s, Event::Drop, 1, 20);
        assert_eq!(l.layers().count(), 0);
    }

    #[test]
    fn no_layer_shows_idle() {
        let s = show();
        let f = frame(&mut Lights::default(), &s, 0);
        assert_eq!(f[0..4], [50, 255, 255, 255]);
    }

    #[test]
    fn stop_without_look_shows_idle_after_2_s() {
        let s = show();
        let mut l = Lights::default();
        l.on(0, 0);
        l.set_playing(false, 1);
        assert_eq!(frame(&mut l, &s, 2000)[1], 255);
        assert_eq!(frame(&mut l, &s, 1999)[0], 255);
        assert_eq!(frame(&mut l, &s, 2001)[0], 50);

        let mut l = Lights::default();
        l.on(0, 0);
        l.set_playing(false, 0);
        l.on(1, 500);
        assert_eq!(frame(&mut l, &s, 2001)[3], 255);
    }

    #[test]
    fn stop_look_in_same_ms_stays() {
        let s = show();
        let mut l = Lights::default();
        l.set_playing(false, 0);
        l.on(1, 0);
        assert_eq!(frame(&mut l, &s, 2001)[3], 255);
    }

    #[test]
    fn master_scales_dimmer() {
        let s = show();
        let mut l = Lights::default();
        l.on(0, 0);
        let mut out = [0; UNIVERSE];
        let live = Live {
            master: 128,
            strobe_hold: false,
            blackout: false,
        };
        l.frame(&s, None, &live, 10, &mut out);
        assert!((127..=128).contains(&out[0]), "{}", out[0]);
    }

    #[test]
    fn strobe_hold_and_fog_button() {
        let s = show();
        let mut l = Lights::default();
        let mut out = [0; UNIVERSE];
        let live = Live {
            master: 255,
            strobe_hold: true,
            blackout: false,
        };
        l.frame(&s, None, &live, 0, &mut out);
        assert_eq!(out[9], 255);
        l.fog(0);
        assert_eq!(frame(&mut l, &s, 2999)[19], 255);
        assert_eq!(frame(&mut l, &s, 3001)[19], 0);
    }

    #[test]
    fn aim_override_and_timeout() {
        use Channel::*;
        let mut s = show();
        s.fixtures
            .push(fixture(30, &[Pan, PanFine, Tilt, TiltFine]));
        let mut l = Lights::default();
        l.aim(3, 0x1234, 0xABCD, 0);
        assert_eq!(frame(&mut l, &s, 9_999)[29..33], [0x12, 0x34, 0xAB, 0xCD]);
        assert_eq!(frame(&mut l, &s, 10_001)[29..33], [0x80, 0, 0x80, 0]);
    }

    #[test]
    fn aim_wins_over_layer_and_idle_position() {
        use Channel::*;
        let mut s = show();
        s.fixtures
            .push(fixture(30, &[Dimmer, Pan, PanFine, Tilt, TiltFine]));
        s.groups.push(group(3));
        s.positions = vec![crate::show::Position {
            name: "a".into(),
            aims: vec![crate::show::Aim {
                fixture: 3,
                pan: 0x2000,
                tilt: 0x3000,
            }],
        }];
        let mut idle = par([1, 2, 3, 0], 255, Fade::Cut);
        idle.rows = vec![row(3, [1, 2, 3, 0], 255, Effect::None, 0)];
        idle.rows[0].position = Some(0);
        s.looks[3] = Some(idle);
        let mut l = Lights::default();
        l.on(0, 0);
        assert_eq!(frame(&mut l, &s, 10)[30..34], [0x20, 0, 0x30, 0]);
        l.aim(3, 0x1234, 0xABCD, 20);
        assert_eq!(frame(&mut l, &s, 30)[30..34], [0x12, 0x34, 0xAB, 0xCD]);
    }

    #[test]
    fn layer_without_aim_keeps_idle_position() {
        use Channel::*;
        let mut s = show();
        s.fixtures
            .push(fixture(30, &[Dimmer, Pan, PanFine, Tilt, TiltFine]));
        s.groups.push(group(3));
        s.positions = vec![crate::show::Position {
            name: "a".into(),
            aims: vec![crate::show::Aim {
                fixture: 3,
                pan: 0x2000,
                tilt: 0x3000,
            }],
        }];
        let mut idle = par([1, 2, 3, 0], 255, Fade::Cut);
        idle.rows = vec![row(3, [1, 2, 3, 0], 255, Effect::None, 0)];
        idle.rows[0].position = Some(0);
        s.looks[3] = Some(idle);
        let mut flash = par([255, 0, 0, 0], 255, Fade::Cut);
        flash.rows = vec![row(3, [255, 0, 0, 0], 255, Effect::None, 0)];
        s.looks[0] = Some(flash);
        let mut l = Lights::default();
        l.on(0, 0);
        assert_eq!(frame(&mut l, &s, 10)[30..34], [0x20, 0, 0x30, 0]);
        let mut empty = show();
        empty
            .fixtures
            .push(fixture(30, &[Pan, PanFine, Tilt, TiltFine]));
        assert_eq!(
            frame(&mut Lights::default(), &empty, 10)[29..33],
            [0x80, 0, 0x80, 0]
        );
    }

    mod autopilot {
        use super::*;
        use crate::auto::{Blackout, Move, MoveShape, Param, Spread, Target, Tier};
        use crate::moves::apply_room;
        use crate::pilot::Action;
        use Channel::*;

        fn auto_show() -> Show {
            let mut s = show();
            s.fixtures = vec![
                fixture(1, &[Dimmer, Red, Green, Blue, Strobe]),
                Fixture {
                    console: true,
                    ..fixture(10, &[Dimmer, Strobe])
                },
            ];
            s.groups = vec![group(0), group(1)];
            s.auto.on = true;
            s.auto.drop.blackout = Blackout::HalfBar;
            s
        }

        const B120: Beat = Beat {
            phase: 0.0,
            beat_in_bar: 1,
            count: 0,
            ms_per_beat: 500.0,
        };

        fn live(blackout: bool) -> Live {
            Live {
                master: 255,
                strobe_hold: false,
                blackout,
            }
        }

        #[test]
        fn build_climbs_and_blacks_out() {
            let s = auto_show();
            let mut l = Lights::default();
            l.auto(
                Action::Build {
                    from_ms: 0,
                    drop_at_ms: Some(8000),
                },
                0,
            );
            let (c, a) = l.room(&s, Some(&B120), &live(false), 4000);
            assert!(c.abs_diff(128 + 64) <= 1 && a == 255, "{c} {a}");
            assert_eq!(l.room(&s, Some(&B120), &live(false), 7500).1, 0);
            // The blackout ends at the drop time, before the Hit arrives.
            assert_eq!(l.room(&s, Some(&B120), &live(false), 8100), (255, 255));
            l.auto(Action::Hit { strobe: false }, 8100);
            assert_eq!(l.room(&s, Some(&B120), &live(false), 8100), (255, 255));
        }

        #[test]
        fn build_without_drop_climbs_and_stays() {
            let s = auto_show();
            let mut l = Lights::default();
            l.auto(
                Action::Build {
                    from_ms: 0,
                    drop_at_ms: None,
                },
                0,
            );
            let (c, a) = l.room(&s, Some(&B120), &live(false), 16_000);
            assert!(c.abs_diff(192) <= 1 && a == 255, "{c} {a}");
            assert_eq!(l.room(&s, Some(&B120), &live(false), 100_000), (255, 255));
            let mut out = [0; UNIVERSE];
            l.frame(&s, Some(&B120), &live(false), 1000, &mut out);
            assert!(out[0] < 175);
            l.auto(Action::EndBuild, 1000);
            assert_eq!(l.room(&s, Some(&B120), &live(false), 1000), (255, 255));
        }

        #[test]
        fn hit_strobes_box_only() {
            let s = auto_show();
            let mut l = Lights::default();
            l.auto(Action::Hit { strobe: true }, 1000);
            let mut out = [0; UNIVERSE];
            l.frame(&s, Some(&B120), &live(false), 1500, &mut out);
            assert_eq!((out[4], out[10]), (255, 0));
            l.frame(&s, Some(&B120), &live(false), 2100, &mut out);
            assert_eq!(out[4], 0);
        }

        #[test]
        fn live_blackout() {
            let s = auto_show();
            let l = Lights::default();
            assert_eq!(l.room(&s, None, &live(true), 0).1, 0);
        }

        #[test]
        fn manual_rules_unchanged_when_auto_off() {
            let mut s = auto_show();
            s.auto.on = false;
            let mut l = Lights::default();
            l.auto(
                Action::Energy {
                    tier: Tier::Peak,
                    spot: 100,
                },
                0,
            );
            let f = frame(&mut l, &s, 0);
            assert_eq!(f[0], 50);
        }

        #[test]
        fn try_move_runs_alone_for_8_bars() {
            let mut s = auto_show();
            let m = |spot| Move {
                shape: MoveShape::Hold,
                target: Target::Group(0),
                param: Param::Dimmer,
                half_beats: 4,
                size: 50,
                spread: Spread::Together,
                color: None,
                faster: false,
                spot,
            };
            s.auto.moves = vec![m(0), m(0)];
            s.auto.moves[0].size = 100;
            let mut l = Lights::default();
            l.auto(Action::Moves(0b10), 0);
            l.try_move(0, 1000);
            let mut out = [0; UNIVERSE];
            l.frame(&s, Some(&B120), &live(false), 2000, &mut out);
            assert_eq!(out[0], 175);
            // 32 beats of 500 ms later the set of Action::Moves returns.
            l.frame(&s, Some(&B120), &live(false), 17_001, &mut out);
            assert_eq!(out[0], 88);
        }

        #[test]
        fn room_blackout_move_darkens() {
            let mut s = auto_show();
            s.auto.moves = vec![Move {
                shape: MoveShape::Blackout,
                target: Target::Room,
                param: Param::Dimmer,
                half_beats: 8,
                size: 50,
                spread: Spread::Together,
                color: None,
                faster: false,
                spot: 0,
            }];
            let mut l = Lights::default();
            l.auto(Action::Moves(1), 0);
            let late = Beat {
                phase: 0.0,
                beat_in_bar: 4,
                count: 3,
                ms_per_beat: 500.0,
            };
            assert_eq!(l.room(&s, Some(&B120), &live(false), 0).1, 255);
            assert_eq!(l.room(&s, Some(&late), &live(false), 0).1, 0);
            let mut out = [9; UNIVERSE];
            apply_room(&s, 255, 0, &mut out);
            assert_eq!(out[0], 0);
        }

        #[test]
        fn master_scales_amber_uv() {
            // The look renders amber and uv as 0, so the test checks the shared rule.
            assert!(crate::moves::is_light(Amber, false));
            assert!(crate::moves::is_light(Uv, false));
            assert!(!crate::moves::is_light(Amber, true));
            let mut s = show();
            s.fixtures = vec![fixture(1, &[Red, Amber, Uv])];
            s.groups = vec![group(0)];
            let mut o = [0; UNIVERSE];
            o[..3].copy_from_slice(&[200; 3]);
            apply_room(&s, 255, 128, &mut o);
            assert_eq!(o[..3], [100; 3]);
        }
    }

    #[test]
    fn master_zero_clears_strobe_hold() {
        let s = show();
        let mut l = Lights::default();
        let mut out = [0; UNIVERSE];
        let live = Live {
            master: 0,
            strobe_hold: true,
            blackout: false,
        };
        l.frame(&s, None, &live, 0, &mut out);
        assert_eq!(out[9], 0);
    }
}
