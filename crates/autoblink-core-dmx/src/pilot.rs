//! The pilot of the light autopilot.
//!
//! The pilot reads the energy of the night and the phrase events. At a phrase change it picks one
//! console program and one move for each target. Before a drop it starts the build, and on the
//! drop it hits. The pilot has no clock: the caller gives `now_ms`. The path of `tick` allocates
//! nothing.

use autoblink_core_midi::scenes::Event;

use crate::auto::{Auto, Target, Tier, CALM_BELOW, MAX_MOVES, MAX_PROGRAMS, PEAK_FROM};
use crate::night::{raw_energy, Night};

/// A pin of a tier ends after this time.
pub const PIN_MS: u64 = 30 * 60 * 1000;
/// The build starts at most this many beats before a known drop.
pub const BUILD_BEATS: f64 = 64.0;
/// Without a phrase event for this many beats, the pilot picks again.
pub const REPICK_BEATS: f64 = 64.0;
/// The spot must cross a tier border by more than this value to change the tier.
const HYSTERESIS: u8 = 4;
/// Two drop times that differ by less than this value are the same drop.
const SAME_DROP_MS: u64 = 1000;
/// A build that is this long past its drop time ends without a drop.
const LATE_DROP_MS: u64 = 2000;
/// The BPM for the timers when the BPM is not valid.
const FALLBACK_BPM: f64 = 120.0;
/// The mark of an empty place in a history.
const NONE: u8 = u8::MAX;

/// A set of the scene events, one bit for each entry of `Event::ALL`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventSet(pub u32);

impl EventSet {
    fn bit(e: Event) -> u32 {
        let i = Event::ALL.iter().position(|&x| x == e).unwrap_or(0);
        1 << i
    }

    pub fn insert(&mut self, e: Event) {
        self.0 |= Self::bit(e);
    }

    pub fn contains(self, e: Event) -> bool {
        self.0 & Self::bit(e) != 0
    }

    /// An event is in the set when its counter changed. The counters wrap.
    pub fn from_counts(prev: &[u8; 16], now: &[u8; 16]) -> EventSet {
        let mut s = EventSet(0);
        for (i, e) in Event::ALL.iter().enumerate() {
            if prev[i] != now[i] {
                s.insert(*e);
            }
        }
        s
    }

    fn has_phrase_event(self) -> bool {
        Event::ALL
            .iter()
            .any(|&e| e.is_phrase_event() && self.contains(e))
    }
}

pub struct PilotInput {
    pub now_ms: u64,
    pub playing: bool,
    pub bpm: f64,
    /// The track energy at the playhead (`DropGuess::energy_at`).
    pub track_energy: Option<u8>,
    /// The phrase events that the scene engine gave in this tick, also without presets.
    pub events: EventSet,
    /// The start time of the next Drop phrase, when the rekordbox phrases give it.
    pub drop_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// The MIDI program number, 0 to 127.
    Program(u8),
    /// Bit i is set when `auto.moves[i]` runs.
    Moves(u32),
    Energy {
        tier: Tier,
        spot: u8,
    },
    Build {
        from_ms: u64,
        drop_at_ms: Option<u64>,
    },
    Hit {
        strobe: bool,
    },
    /// The build ends without a drop (stop, track change).
    EndBuild,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PilotStatus {
    pub tier: Tier,
    pub spot: u8,
    pub pinned: Option<Tier>,
    /// The index into `auto.programs` of the last pick.
    pub program: Option<u8>,
    pub moves: u32,
    pub drop_in_beats: Option<u16>,
}

pub struct Pilot {
    night: Night,
    tier: Option<Tier>,
    spot: u8,
    pin: Option<(Tier, u64)>,
    prog_hist: [u8; 2],
    prog_plays: [u16; MAX_PROGRAMS],
    /// The history of a target sits at the index of its first move.
    move_hist: [[u8; 2]; MAX_MOVES],
    move_plays: [u16; MAX_MOVES],
    rng: u64,
    last_phrase_ms: Option<u64>,
    build_open: bool,
    build_drop_ms: Option<u64>,
    last_strobe_ms: Option<u64>,
    last_program: Option<u8>,
    last_moves: u32,
    last_now: u64,
    last_bpm: f64,
    last_drop_at: Option<u64>,
}

impl Default for Pilot {
    fn default() -> Self {
        Pilot {
            night: Night::default(),
            tier: None,
            spot: 50,
            pin: None,
            prog_hist: [NONE; 2],
            prog_plays: [0; MAX_PROGRAMS],
            move_hist: [[NONE; 2]; MAX_MOVES],
            move_plays: [0; MAX_MOVES],
            rng: 0,
            last_phrase_ms: None,
            build_open: false,
            build_drop_ms: None,
            last_strobe_ms: None,
            last_program: None,
            last_moves: 0,
            last_now: 0,
            last_bpm: 0.0,
            last_drop_at: None,
        }
    }
}

/// The length of a beat in milliseconds. A BPM of 0 or less counts as 120 BPM.
fn beat_ms(bpm: f64) -> f64 {
    60_000.0 / if bpm > 0.0 { bpm } else { FALLBACK_BPM }
}

/// The tier after a phrase change. The spot must cross a border by more than `HYSTERESIS`.
fn follow(cur: Tier, spot: u8) -> Tier {
    let stay = match cur {
        Tier::Calm => spot <= CALM_BELOW + HYSTERESIS,
        Tier::Groove => spot + HYSTERESIS >= CALM_BELOW && spot <= PEAK_FROM + HYSTERESIS,
        Tier::Peak => spot + HYSTERESIS >= PEAK_FROM,
    };
    if stay {
        cur
    } else {
        Tier::of(spot)
    }
}

/// The nearest spot to `spot` inside the tier.
fn clamp_to_tier(tier: Tier, spot: u8) -> u8 {
    match tier {
        Tier::Calm => spot.min(CALM_BELOW - 1),
        Tier::Groove => spot.clamp(CALM_BELOW, PEAK_FROM - 1),
        Tier::Peak => spot.max(PEAK_FROM),
    }
}

/// Picks one of the candidates. It skips the last two picks (the last pick when only two
/// candidates exist), then takes the lowest play count and breaks a tie at random.
fn pick(cands: &[u8], hist: &mut [u8; 2], plays: &mut [u16], rng: &mut u64) -> Option<u8> {
    let n = cands.len();
    if n == 0 {
        return None;
    }
    let mut pool = [0u8; MAX_PROGRAMS];
    let mut m = 0;
    for &c in cands {
        let skip = match n {
            1 => false,
            2 => c == hist[0],
            _ => c == hist[0] || c == hist[1],
        };
        if !skip {
            pool[m] = c;
            m += 1;
        }
    }
    let least = pool[..m].iter().map(|&c| plays[usize::from(c)]).min()?;
    let mut ties = [0u8; MAX_PROGRAMS];
    let mut t = 0;
    for &c in &pool[..m] {
        if plays[usize::from(c)] == least {
            ties[t] = c;
            t += 1;
        }
    }
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    let chosen = ties[(*rng % t as u64) as usize];
    plays[usize::from(chosen)] = plays[usize::from(chosen)].saturating_add(1);
    hist[1] = hist[0];
    hist[0] = chosen;
    Some(chosen)
}

impl Pilot {
    pub fn night(&self) -> &Night {
        &self.night
    }

    pub fn set_night(&mut self, night: Night) {
        self.night = night;
    }

    /// Pins a tier for `PIN_MS`. `None` clears the pin at once. The tier changes at the next
    /// phrase change.
    pub fn pin(&mut self, tier: Option<Tier>, now_ms: u64) {
        self.pin = tier.map(|t| (t, now_ms));
    }

    pub fn status(&self) -> PilotStatus {
        let drop_in_beats = self.last_drop_at.filter(|&d| d > self.last_now).map(|d| {
            let b = (d - self.last_now) as f64 / beat_ms(self.last_bpm);
            b.min(f64::from(u16::MAX)) as u16
        });
        PilotStatus {
            tier: self.tier.unwrap_or_default(),
            spot: self.spot,
            pinned: self.pin.map(|p| p.0),
            program: self.last_program,
            moves: self.last_moves,
            drop_in_beats,
        }
    }

    pub fn tick(&mut self, auto: &Auto, input: &PilotInput, out: &mut impl FnMut(Action)) {
        let now = input.now_ms;
        if self.rng == 0 {
            self.rng = (now ^ 0x9E37_79B9_7F4A_7C15) | 1;
        }
        self.last_now = now;
        self.last_bpm = input.bpm;
        self.last_drop_at = input.drop_at_ms;
        if self
            .pin
            .is_some_and(|(_, set)| now.saturating_sub(set) >= PIN_MS)
        {
            self.pin = None;
        }

        let raw = raw_energy(input.bpm, input.track_energy);
        if self.night.sample(raw, input.playing, now) {
            self.prog_hist = [NONE; 2];
            self.prog_plays = [0; MAX_PROGRAMS];
            self.move_hist = [[NONE; 2]; MAX_MOVES];
            self.move_plays = [0; MAX_MOVES];
        }
        let spot = self.night.spot(raw);
        let ev = input.events;
        let pin_tier = self.pin.map(|p| p.0);
        let drop_on = matches!(pin_tier, None | Some(Tier::Peak));

        // The build ends at a stop, a track change, a pin that turns the drop off, or a drop that
        // does not come.
        if self.build_open {
            let gone = self.build_drop_ms.is_some_and(|d| {
                (input.drop_at_ms.is_none() && now + SAME_DROP_MS < d)
                    || now > d.saturating_add(LATE_DROP_MS)
            });
            if !input.playing
                || !drop_on
                || gone
                || ev.contains(Event::Stop)
                || ev.contains(Event::TrackLoad)
            {
                self.end_build(out);
            }
        }
        if !input.playing {
            self.last_phrase_ms = None;
            return;
        }

        let first = self.tier.is_none();
        if first {
            let t = pin_tier.unwrap_or_else(|| Tier::of(spot));
            self.tier = Some(t);
            self.spot = spot;
            out(Action::Energy { tier: t, spot });
        }
        let last_phrase = *self.last_phrase_ms.get_or_insert(now);

        if drop_on {
            self.build(input, out);
        }

        let is_drop = drop_on && ev.contains(Event::Drop);
        let repick = (now.saturating_sub(last_phrase)) as f64 >= REPICK_BEATS * beat_ms(input.bpm);
        if !(ev.has_phrase_event() || repick) {
            return;
        }
        self.last_phrase_ms = Some(now);
        let cur = self.tier.unwrap_or_default();
        let tier = pin_tier.unwrap_or_else(|| follow(cur, spot));
        self.tier = Some(tier);
        self.spot = spot;
        out(Action::Energy { tier, spot });
        let pick_tier = if is_drop { Tier::Peak } else { tier };
        self.pick_all(auto, pick_tier, spot, out);
        if is_drop {
            let cooldown = u64::from(auto.drop.strobe_cooldown_s) * 1000;
            let strobe = self
                .last_strobe_ms
                .is_none_or(|l| now.saturating_sub(l) >= cooldown);
            if strobe {
                self.last_strobe_ms = Some(now);
            }
            self.build_open = false;
            self.build_drop_ms = None;
            out(Action::Hit { strobe });
        }
    }

    fn end_build(&mut self, out: &mut impl FnMut(Action)) {
        self.build_open = false;
        self.build_drop_ms = None;
        out(Action::EndBuild);
    }

    /// Starts the build for a known drop in the window, or for a Build event without a drop time.
    fn build(&mut self, input: &PilotInput, out: &mut impl FnMut(Action)) {
        let now = input.now_ms;
        let future = input.drop_at_ms.filter(|&d| d > now);
        if let Some(d) = future {
            let in_window = (d - now) as f64 <= BUILD_BEATS * beat_ms(input.bpm);
            let same = self
                .build_drop_ms
                .is_some_and(|b| b.abs_diff(d) < SAME_DROP_MS);
            if in_window && !same {
                self.build_open = true;
                self.build_drop_ms = Some(d);
                out(Action::Build {
                    from_ms: now,
                    drop_at_ms: Some(d),
                });
            }
        } else if input.events.contains(Event::Build) && !self.build_open {
            self.build_open = true;
            self.build_drop_ms = None;
            out(Action::Build {
                from_ms: now,
                drop_at_ms: None,
            });
        }
    }

    /// Picks the program and the moves of a tier and sends them.
    fn pick_all(&mut self, auto: &Auto, tier: Tier, spot: u8, out: &mut impl FnMut(Action)) {
        let progs = &auto.programs[..auto.programs.len().min(MAX_PROGRAMS)];
        let mut cands = [0u8; MAX_PROGRAMS];
        let mut n = 0;
        for (i, p) in progs.iter().enumerate() {
            if tier.contains(p.spot) {
                cands[n] = i as u8;
                n += 1;
            }
        }
        if n == 0 && !progs.is_empty() {
            let target = clamp_to_tier(tier, spot);
            let best = progs.iter().map(|p| p.spot.abs_diff(target)).min();
            for (i, p) in progs.iter().enumerate() {
                if Some(p.spot.abs_diff(target)) == best {
                    cands[n] = i as u8;
                    n += 1;
                }
            }
        }
        let mut hist = self.prog_hist;
        if let Some(i) = pick(&cands[..n], &mut hist, &mut self.prog_plays, &mut self.rng) {
            self.prog_hist = hist;
            self.last_program = Some(i);
            out(Action::Program(progs[usize::from(i)].pc));
        }

        let moves = &auto.moves[..auto.moves.len().min(MAX_MOVES)];
        let mut mask = 0u32;
        for i in 0..moves.len() {
            let target: Target = moves[i].target;
            // Handle each target once, at its first move in the tier.
            if !tier.contains(moves[i].spot)
                || moves[..i]
                    .iter()
                    .any(|m| m.target == target && tier.contains(m.spot))
            {
                continue;
            }
            let key = moves.iter().position(|m| m.target == target).unwrap_or(i);
            let mut c = [0u8; MAX_PROGRAMS];
            let mut k = 0;
            for (j, m) in moves.iter().enumerate().skip(i) {
                if m.target == target && tier.contains(m.spot) {
                    c[k] = j as u8;
                    k += 1;
                }
            }
            if let Some(j) = pick(
                &c[..k],
                &mut self.move_hist[key],
                &mut self.move_plays,
                &mut self.rng,
            ) {
                mask |= 1 << j;
            }
        }
        self.last_moves = mask;
        out(Action::Moves(mask));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto::{Move, MoveShape, Param, Program, Spread};

    fn prog(pc: u8, spot: u8) -> Program {
        Program {
            pc,
            name: format!("p{pc}"),
            color: [1, 2, 3],
            spot,
        }
    }

    fn mv(target: Target, spot: u8) -> Move {
        Move {
            shape: MoveShape::Pulse,
            target,
            param: Param::Dimmer,
            half_beats: 4,
            size: 50,
            spread: Spread::Together,
            color: None,
            faster: true,
            spot,
        }
    }

    fn input(now_ms: u64, events: &[Event]) -> PilotInput {
        let mut e = EventSet::default();
        for &x in events {
            e.insert(x);
        }
        PilotInput {
            now_ms,
            playing: true,
            bpm: 90.0,
            track_energy: None,
            events: e,
            drop_at_ms: None,
        }
    }

    fn run(p: &mut Pilot, a: &Auto, i: &PilotInput) -> Vec<Action> {
        let mut v = Vec::new();
        p.tick(a, i, &mut |x| v.push(x));
        v
    }

    fn programs_of(v: &[Action]) -> Vec<u8> {
        v.iter()
            .filter_map(|a| match a {
                Action::Program(p) => Some(*p),
                _ => None,
            })
            .collect()
    }

    fn tiered() -> Auto {
        Auto {
            programs: vec![
                prog(1, 10),
                prog(2, 15),
                prog(3, 20),
                prog(11, 50),
                prog(12, 55),
                prog(13, 60),
                prog(21, 85),
                prog(22, 90),
                prog(23, 95),
            ],
            ..Auto::default()
        }
    }

    fn only(spots: &[u8]) -> Auto {
        Auto {
            programs: spots
                .iter()
                .enumerate()
                .map(|(i, &s)| prog(i as u8, s))
                .collect(),
            ..Auto::default()
        }
    }

    #[test]
    fn picks_program_of_tier_on_phrase_change() {
        let a = tiered();
        let mut p = Pilot::default();
        let v = run(&mut p, &a, &input(1000, &[Event::Intro]));
        let pc = programs_of(&v);
        assert_eq!(pc.len(), 1);
        assert!([1, 2, 3].contains(&pc[0]), "{pc:?}");
        assert!(v.contains(&Action::Moves(0)));
        assert_eq!(p.status().tier, Tier::Calm);
    }

    #[test]
    fn never_repeats_last_two() {
        let a = only(&[10, 11, 12, 13]);
        let mut p = Pilot::default();
        let mut picks = Vec::new();
        for k in 0..40 {
            let v = run(&mut p, &a, &input(1000 + k * 10, &[Event::Groove]));
            picks.extend(programs_of(&v));
        }
        assert_eq!(picks.len(), 40);
        for i in 2..picks.len() {
            assert_ne!(picks[i], picks[i - 1]);
            assert_ne!(picks[i], picks[i - 2]);
        }
    }

    #[test]
    fn prefers_least_played() {
        let a = only(&[10, 11, 12]);
        let mut p = Pilot::default();
        for k in 0..30 {
            run(&mut p, &a, &input(1000 + k * 10, &[Event::Groove]));
        }
        assert_eq!(&p.prog_plays[..3], &[10, 10, 10]);
    }

    #[test]
    fn pick_small_lists() {
        let mut p = Pilot::default();
        let v = run(&mut p, &Auto::default(), &input(1000, &[Event::Groove]));
        assert!(programs_of(&v).is_empty());
        assert!(v.contains(&Action::Moves(0)));

        let a = only(&[10]);
        let mut p = Pilot::default();
        for k in 0..4 {
            let v = run(&mut p, &a, &input(1000 + k * 10, &[Event::Groove]));
            assert_eq!(programs_of(&v), vec![0]);
        }

        let a = only(&[10, 12]);
        let mut p = Pilot::default();
        let mut picks = Vec::new();
        for k in 0..6 {
            let v = run(&mut p, &a, &input(1000 + k * 10, &[Event::Groove]));
            picks.extend(programs_of(&v));
        }
        for w in picks.windows(2) {
            assert_ne!(w[0], w[1]);
        }
    }

    #[test]
    fn nearest_program_when_tier_empty() {
        let a = only(&[10, 20, 30]);
        let mut p = Pilot::default();
        p.pin(Some(Tier::Peak), 0);
        let v = run(&mut p, &a, &input(1000, &[Event::Groove]));
        assert_eq!(programs_of(&v), vec![2]);
    }

    #[test]
    fn one_move_per_target() {
        let a = Auto {
            moves: vec![
                mv(Target::Group(0), 50),
                mv(Target::Group(0), 55),
                mv(Target::Group(1), 50),
                mv(Target::Room, 50),
            ],
            ..Auto::default()
        };
        let mut p = Pilot::default();
        p.pin(Some(Tier::Groove), 0);
        let v = run(&mut p, &a, &input(1000, &[Event::Groove]));
        let mask = v
            .iter()
            .find_map(|x| match x {
                Action::Moves(m) => Some(*m),
                _ => None,
            })
            .unwrap();
        assert_eq!(mask.count_ones(), 3);
        assert_eq!((mask & 0b11).count_ones(), 1);
        assert_eq!(mask & 0b1100, 0b1100);
        assert_eq!(p.status().moves, mask);
    }

    #[test]
    fn repicks_without_phrase_events() {
        let a = only(&[10, 12]);
        let mut p = Pilot::default();
        let mut i = input(1000, &[]);
        i.bpm = 120.0;
        assert!(programs_of(&run(&mut p, &a, &i)).is_empty());
        i.now_ms = 1000 + 31_990;
        assert!(programs_of(&run(&mut p, &a, &i)).is_empty());
        i.now_ms = 1000 + 32_000;
        assert_eq!(programs_of(&run(&mut p, &a, &i)).len(), 1);
    }

    #[test]
    fn tier_hysteresis() {
        assert_eq!(follow(Tier::Calm, 36), Tier::Calm);
        assert_eq!(follow(Tier::Calm, 38), Tier::Calm);
        assert_eq!(follow(Tier::Calm, 39), Tier::Groove);
        assert_eq!(follow(Tier::Groove, 31), Tier::Groove);
        assert_eq!(follow(Tier::Groove, 29), Tier::Calm);
        assert_eq!(follow(Tier::Groove, 84), Tier::Groove);
        assert_eq!(follow(Tier::Groove, 85), Tier::Peak);
        assert_eq!(follow(Tier::Peak, 76), Tier::Peak);
        assert_eq!(follow(Tier::Peak, 75), Tier::Groove);
        assert_eq!(follow(Tier::Calm, 99), Tier::Peak);
    }

    #[test]
    fn pin_holds_and_expires() {
        let a = tiered();
        let mut p = Pilot::default();
        run(&mut p, &a, &input(0, &[]));
        p.pin(Some(Tier::Peak), 0);
        assert_eq!(p.status().pinned, Some(Tier::Peak));
        let v = run(&mut p, &a, &input(10, &[Event::Groove]));
        assert!([21, 22, 23].contains(&programs_of(&v)[0]));
        assert_eq!(p.status().tier, Tier::Peak);
        let v = run(&mut p, &a, &input(PIN_MS + 1, &[Event::Groove]));
        assert!([1, 2, 3].contains(&programs_of(&v)[0]));
        assert_eq!(p.status().pinned, None);
        p.pin(Some(Tier::Peak), PIN_MS + 1);
        p.pin(None, PIN_MS + 2);
        assert_eq!(p.status().pinned, None);
    }

    fn builds(v: &[Action]) -> Vec<Action> {
        v.iter()
            .copied()
            .filter(|a| matches!(a, Action::Build { .. }))
            .collect()
    }

    #[test]
    fn build_before_known_drop() {
        let a = tiered();
        let mut p = Pilot::default();
        let mut i = input(1000, &[]);
        i.bpm = 128.0;
        i.drop_at_ms = Some(31_000);
        let v = run(&mut p, &a, &i);
        assert_eq!(
            builds(&v),
            vec![Action::Build {
                from_ms: 1000,
                drop_at_ms: Some(31_000)
            }]
        );
        i.now_ms = 1010;
        assert!(builds(&run(&mut p, &a, &i)).is_empty());
        assert_eq!(p.status().drop_in_beats, Some(63));
        // A drop farther away than the window starts no build.
        let mut q = Pilot::default();
        let mut j = input(1000, &[]);
        j.bpm = 128.0;
        j.drop_at_ms = Some(32_000);
        assert!(builds(&run(&mut q, &a, &j)).is_empty());
    }

    #[test]
    fn build_from_guess() {
        let a = tiered();
        let mut p = Pilot::default();
        let v = run(&mut p, &a, &input(1000, &[Event::Build]));
        assert_eq!(
            builds(&v),
            vec![Action::Build {
                from_ms: 1000,
                drop_at_ms: None
            }]
        );
        assert!(builds(&run(&mut p, &a, &input(1010, &[Event::Build]))).is_empty());
    }

    fn hit(v: &[Action]) -> Option<bool> {
        v.iter().find_map(|a| match a {
            Action::Hit { strobe } => Some(*strobe),
            _ => None,
        })
    }

    #[test]
    fn drop_hits_with_strobe_cooldown() {
        let a = tiered();
        let mut p = Pilot::default();
        let t = 1000;
        assert_eq!(hit(&run(&mut p, &a, &input(t, &[Event::Drop]))), Some(true));
        let v = run(&mut p, &a, &input(t + 60_000, &[Event::Drop]));
        assert_eq!(hit(&v), Some(false));
        let v = run(&mut p, &a, &input(t + 121_000, &[Event::Drop]));
        assert_eq!(hit(&v), Some(true));
    }

    #[test]
    fn drop_picks_peak() {
        let a = tiered();
        let mut p = Pilot::default();
        let v = run(&mut p, &a, &input(1000, &[Event::Drop]));
        let pc = programs_of(&v);
        assert_eq!(pc.len(), 1, "one pick for the tick");
        assert!([21, 22, 23].contains(&pc[0]), "{pc:?}");
        assert_eq!(p.status().tier, Tier::Calm);
    }

    #[test]
    fn calm_pin_turns_off_drop() {
        let a = tiered();
        let mut p = Pilot::default();
        p.pin(Some(Tier::Calm), 0);
        let v = run(&mut p, &a, &input(1000, &[Event::Build]));
        assert!(builds(&v).is_empty());
        let v = run(&mut p, &a, &input(1010, &[Event::Drop]));
        assert_eq!(hit(&v), None);
        assert!([1, 2, 3].contains(&programs_of(&v)[0]));
    }

    #[test]
    fn peak_pin_keeps_drop() {
        let a = tiered();
        let mut p = Pilot::default();
        p.pin(Some(Tier::Peak), 0);
        let v = run(&mut p, &a, &input(1000, &[Event::Build]));
        assert_eq!(builds(&v).len(), 1);
        let v = run(&mut p, &a, &input(1010, &[Event::Drop]));
        assert_eq!(hit(&v), Some(true));
    }

    #[test]
    fn stop_ends_build() {
        let a = tiered();
        let mut p = Pilot::default();
        run(&mut p, &a, &input(1000, &[Event::Build]));
        let mut i = input(1010, &[]);
        i.playing = false;
        assert_eq!(run(&mut p, &a, &i), vec![Action::EndBuild]);
        assert!(run(&mut p, &a, &i).is_empty());
    }

    #[test]
    fn drop_ends_build_without_end_action() {
        let a = tiered();
        let mut p = Pilot::default();
        run(&mut p, &a, &input(1000, &[Event::Build]));
        let v = run(&mut p, &a, &input(1010, &[Event::Drop]));
        assert!(!v.contains(&Action::EndBuild));
        assert!(!p.build_open);
    }

    #[test]
    fn energy_goes_out_at_first_tick() {
        let a = tiered();
        let mut p = Pilot::default();
        let v = run(&mut p, &a, &input(1000, &[]));
        assert!(matches!(
            v[0],
            Action::Energy {
                tier: Tier::Calm,
                ..
            }
        ));
        assert!(run(&mut p, &a, &input(1010, &[])).is_empty());
    }

    #[test]
    fn event_set_from_counts() {
        let mut now = [0u8; 16];
        now[3] = 1;
        let s = EventSet::from_counts(&[0; 16], &now);
        assert!(s.contains(Event::Drop));
        assert!(!s.contains(Event::Build));
        let mut prev = [0u8; 16];
        prev[2] = 255;
        let s = EventSet::from_counts(&prev, &[0; 16]);
        assert!(s.contains(Event::Build));
    }
}
