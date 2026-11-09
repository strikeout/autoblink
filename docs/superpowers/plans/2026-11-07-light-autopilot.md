# Light Autopilot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The box and the host drive the console programs (MIDI program changes) and the box fixtures (DMX moves) from the BPM and the track energy all night, without an operator.

**Architecture:** `autoblink-core-dmx` gets four modules. `auto.rs` holds the settings in the show. `night.rs` turns the BPM and the track energy into a spot from 0 to 100, relative to the night. `pilot.rs` picks the programs and the moves and runs the drop sequence. `moves.rs` renders the moves and the room level into the universe. The firmware runs `Pilot` in the `midi` thread, which owns the scene events and the MIDI output. It sends the picks to the `dmx` thread through the existing queue. The host does the same in the mapper and its DMX task. The page gets three setup views, a move editor and a live screen.

**Tech Stack:** Rust 2021 (`autoblink-core-dmx`, `autoblink-core-midi`), ESP-IDF firmware (`std` threads, FreeRTOS queue), the host (tokio, axum), vanilla JS in `webui/index.html`.

**Spec:** `docs/superpowers/specs/2026-11-07-light-autopilot-design.md`. Mockups: `.superpowers/brainstorm/78175-1794060713/content/autopilot-layout.html` and `move-editor.html`.

## Global Constraints

- No `f32` in a serde type that the firmware reads. Use `f64` locals or integers.
- `Lights::frame`, `Pilot::tick` and the room stage allocate nothing on the heap.
- Old shows load unchanged. Each new field uses `#[serde(default)]` and skips serialization when it is empty or default.
- Tiers: Calm is a spot below 34. Peak is a spot of 80 or more. Groove is the rest.
- A pinned tier goes back to Auto after 30 minutes (`PIN_MS`).
- A night starts when a deck plays after 2 hours without a playing deck (`NIGHT_GAP_MS`).
- The drop: build over the last 16 bars (64 beats), console dip to `dip` (default 128) at the build start, blackout (default ½ bar) before the drop, hit with a strobe burst of 2 beats. The strobe has a cooldown of 120 s. Inside the cooldown, the hit comes without the strobe.
- Each `Event::Drop` is a drop. A pin of Calm or Groove turns off the drop sequence. A pin of Peak keeps it.
- Without a known drop time (drop guess only), the box skips the blackout and the console dip climb ends at the hit.
- The master, the dip and the blackout change only light channels. A fixture with a Dimmer channel: scale the Dimmer. A fixture without one: scale Red, Green, Blue, White, Amber and Uv. At level 0, also set Strobe to 0. Never touch Pan, Tilt, Fog or Fixed channels.
- Pick rule: random inside the tier, never one of the last two picks of that list (when the list has more than two), prefer the least played tonight.
- One driver at a time: `Auto::on` true means the autopilot drives, and the scene engine sends no notes and no DMX messages.
- The firmware uses the FreeRTOS queue to the `dmx` thread. The `midi` thread never waits for a lock that the `dmx` thread holds during a frame: it uses `try_lock` and keeps its last copy.
- Status and settings messages are `Serialize` structs, not `serde_json::Value` trees.
- The host stays alive under every fault (`runtime::supervise`, no blocking call on a tokio worker).
- Commits carry no attribution lines. Comments, log lines, error strings and docs follow ASD-STE100.
- The version goes to `1.31.0` in the last task (both `Cargo.toml` files and the lock files).

## Review Focus

1. A track without phrases and without a waveform preview (old USB stick): the energy comes from the BPM alone, and the programs still change every 64 beats. Test: Task 3 `repicks_without_phrase_events`.
2. A box restart in the middle of the night: the night memory comes back from NVS, so the peak does not fall to Calm. Test: Task 2 `bytes_round_trip`; Task 5 box check.
3. A tier list with zero, one or two programs: no panic, no endless repeat block. Test: Task 3 `pick_small_lists`.
4. Blackout and dip on a moving head: Pan and Tilt keep their values. Test: Task 4 `room_never_touches_pan_tilt`.
5. A staff page that closes while Blackout is held: the blackout ends. Test: Task 6 host `blackout_released_on_close`; Task 5 uses the strobe-hold release path.

---

### Task 1: The autopilot settings in the show

**Files:**
- Create: `crates/autoblink-core-dmx/src/auto.rs`
- Modify: `crates/autoblink-core-dmx/src/show.rs` (`Fixture::console`, `Show::auto`, `Show::sanitize`), `crates/autoblink-core-dmx/src/lib.rs` (export), the `Fixture { .. }` and `Show { .. }` literals in the tests of `render.rs`, `lights.rs`, `merge.rs`.
- Test: `crates/autoblink-core-dmx/src/auto.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:

```rust
pub const MAX_PROGRAMS: usize = 32;
pub const MAX_MOVES: usize = 24; // a u32 bit mask holds the active moves
pub const CALM_BELOW: u8 = 34;
pub const PEAK_FROM: u8 = 80;
/// The allowed move periods in half beats: ½, 1, 2, 4, 8, 16 and 32 beats.
pub const HALF_BEATS: [u8; 7] = [1, 2, 4, 8, 16, 32, 64];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier { Calm, #[default] Groove, Peak }
impl Tier {
    pub const ALL: [Tier; 3];
    pub fn of(spot: u8) -> Tier;              // < 34 Calm, >= 80 Peak
    pub fn contains(self, spot: u8) -> bool;  // Tier::of(spot) == self
}

pub struct Program { pub pc: u8, pub name: String, pub color: [u8; 3], pub spot: u8 }
pub enum MoveShape { Pulse, Ramp, Sweep, Circle, Figure8, Chase, Wave, StrobeBurst, Blackout, Hold }
pub enum Target { Room, Group(u8), Fixture(u8) }   // JSON: "room", {"group":1}, {"fixture":3}
pub enum Param { PanTilt, Pan, Tilt, Dimmer, Colour, Strobe }
pub enum Spread { Together, Fan, Mirror, Random }
pub struct Move {
    pub shape: MoveShape, pub target: Target, pub param: Param,
    pub half_beats: u8, pub size: u8 /* 0..=100 */, pub spread: Spread,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub color: Option<[u8; 3]>,
    #[serde(default = "yes")] pub faster: bool, pub spot: u8,
}
pub enum Blackout { Off, Beat, #[default] HalfBar, Bar }
impl Blackout { pub fn beats(self) -> f64; } // 0, 1, 2, 4
pub struct DropCfg { pub blackout: Blackout, pub strobe_beats: u8 /*2*/, pub strobe_cooldown_s: u16 /*120*/, pub dip: u8 /*128*/ }
pub struct Auto {
    pub on: bool,
    pub channel: u8,                       // MIDI channel 0..=15 for the program changes
    pub programs: Vec<Program>,            // bounded MAX_PROGRAMS
    pub moves: Vec<Move>,                  // bounded MAX_MOVES
    pub drop: DropCfg,
    pub colors: [[u8; 3]; 3],              // tier colours Calm, Groove, Peak. Default amber, cyan, magenta
    pub pin: String,                       // 0 to 8 digits; empty is no PIN
}
impl Auto { pub fn is_default(&self) -> bool; pub fn sanitize(&mut self, show_fixtures: usize, show_groups: usize); }
// show.rs
// Fixture: #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub console: bool
// Show:    #[serde(default, skip_serializing_if = "Auto::is_default")] pub auto: Auto
```

All structs and enums derive `Debug, Clone, PartialEq, Serialize, Deserialize` (and `Copy` where all fields are `Copy`), with `#[serde(rename_all = "snake_case")]` on enums. Every field of `Auto` and `DropCfg` has `#[serde(default)]`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn tier_bands() {
    assert_eq!(Tier::of(0), Tier::Calm); assert_eq!(Tier::of(33), Tier::Calm);
    assert_eq!(Tier::of(34), Tier::Groove); assert_eq!(Tier::of(79), Tier::Groove);
    assert_eq!(Tier::of(80), Tier::Peak); assert_eq!(Tier::of(100), Tier::Peak);
}
#[test] fn old_show_has_default_auto() {
    // A show JSON without "auto" and without "console" loads, and serializes without both keys.
    let s = Show::from_json(br#"{"fixtures":[{"name":"P","address":1,"channels":["dimmer"]}],"groups":[],"looks":[],"idle":null}"#);
    assert!(s.auto.is_default()); assert!(!s.fixtures[0].console);
    let j = serde_json::to_string(&s).unwrap();
    assert!(!j.contains("auto")); assert!(!j.contains("console"));
}
#[test] fn auto_round_trip() { /* a show with 2 programs, 2 moves (Target::Group(0), Target::Room), console fixture: to_json -> from_json is equal */ }
#[test] fn auto_sanitize() {
    // 40 programs -> 32; pc 200 -> 127; spot 150 -> 100; channel 20 -> 15; name of 30 chars -> 16;
    // half_beats 3 -> 4 (the nearest allowed value, ties go up); size 180 -> 100;
    // a move with Target::Group(9) when the show has 2 groups is dropped; Target::Fixture(40) is dropped;
    // pin "12a4567890" -> "" (not only digits or longer than 8 gives no PIN).
}
#[test] fn target_json() {
    assert_eq!(serde_json::to_string(&Target::Room).unwrap(), "\"room\"");
    assert_eq!(serde_json::to_string(&Target::Group(1)).unwrap(), "{\"group\":1}");
}
```

- [ ] **Step 2: Run** `cargo test --locked -p autoblink-core-dmx auto::` Expected: FAIL (module missing).
- [ ] **Step 3: Implement** `auto.rs` with the interfaces above. Call `self.auto.sanitize(..)` from `Show::sanitize`, after the fixtures and groups are cut. Reuse `bounded` from `show.rs` (make it `pub(crate)`). Default tier colours: Calm `[255, 140, 40]`, Groove `[0, 224, 255]`, Peak `[255, 40, 140]`.
- [ ] **Step 4: Run** `cargo test --locked -p autoblink-core-dmx`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`. Expected: PASS.
- [ ] **Step 5: Commit** "Add the autopilot settings to the DMX show"

### Task 2: The energy spot and the night memory

**Files:**
- Create: `crates/autoblink-core-dmx/src/night.rs`
- Modify: `crates/autoblink-core-midi/src/guess.rs` (add `energy_at`), `crates/autoblink-core-dmx/src/lib.rs`
- Test: in both files

**Interfaces:**
- Produces:

```rust
// guess.rs
impl DropGuess { /// The smoothed energy 0..=255 at the position `frac` (0.0 to 1.0).
    pub fn energy_at(&self, frac: f64) -> u8; }
// night.rs
pub const BINS: usize = 32;
pub const NIGHT_GAP_MS: u64 = 2 * 60 * 60 * 1000;
pub const NIGHT_BYTES: usize = 4 * BINS + 1;   // to_bytes length
/// 0..=255 from the BPM and the track energy. Without a track energy, the BPM alone.
pub fn raw_energy(bpm: f64, track: Option<u8>) -> u8;
#[derive(Clone)]
pub struct Night { /* now: [u16; BINS], prior: [u16; BINS], last_play_ms, last_sample_ms, version byte */ }
impl Default for Night  // prior = a broad default hump, so the first night works
impl Night {
    /// Call it for each tick. It counts one sample each second while `playing`.
    /// When a deck plays after `NIGHT_GAP_MS` without play, it folds this night into the prior
    /// and returns true (a new night starts).
    pub fn sample(&mut self, raw: u8, playing: bool, now_ms: u64) -> bool;
    /// The percentile 0..=100 of `raw` in this night plus the prior.
    pub fn spot(&self, raw: u8) -> u8;
    pub fn to_bytes(&self) -> [u8; NIGHT_BYTES];
    /// A wrong length or version gives `Night::default()`.
    pub fn from_bytes(b: &[u8]) -> Night;
}
```

`raw_energy`: the BPM part maps 90 BPM to 0 and 150 BPM to 255 (clamped). With a track energy, the result is the mean of the two parts. The builder may tune these numbers; the tests below use only the order of values.

The prior weight: the prior counts as at most 600 samples (10 minutes) in `spot`. Thus the night itself rules after about 10 minutes. A fold keeps half of the old prior and adds this night, normalized. When a bin of `now` reaches 30 000, halve all bins of `now`.

- [ ] **Step 1: Write the failing tests**

```rust
// guess.rs
#[test] fn energy_at_follows_profile() { /* preview: 100 columns at 40, then 100 at 200; energy_at(0.1) < energy_at(0.9) */ }
// night.rs
#[test] fn raw_energy_orders() {
    assert!(raw_energy(100.0, None) < raw_energy(130.0, None));
    assert!(raw_energy(125.0, Some(50)) < raw_energy(125.0, Some(220)));
    assert_eq!(raw_energy(0.0, None), 0);
}
#[test] fn warm_up_starts_calm() {
    // A past night of raw values 60..=220 (one per second, folded with a 2 h gap).
    // In the new night, the first raw value 70 gives a spot below 34.
}
#[test] fn night_adapts() {
    // Default Night; 30 minutes of samples at raw 120, then 10 minutes at raw 200.
    // spot(200) >= 80 and spot(120) < 80.
}
#[test] fn gap_starts_new_night() {
    // sample(.., playing=true, t); sample(.., playing=false, t+1h) returns false;
    // sample(.., playing=true, t + 2h + 1s) returns true; a second call at +2s returns false.
}
#[test] fn bytes_round_trip() { /* after samples: Night::from_bytes(&n.to_bytes()).spot(x) == n.spot(x) for x in 0..=255 step 15 */ }
#[test] fn bad_bytes_give_default() { assert_eq!(Night::from_bytes(&[1, 2, 3]).spot(128), Night::default().spot(128)); }
```

- [ ] **Step 2: Run** `cargo test --locked -p autoblink-core-dmx night::` and `-p autoblink-core-midi energy_at`. Expected: FAIL.
- [ ] **Step 3: Implement** both. `Night` has no heap field.
- [ ] **Step 4: Run** the tests, clippy and fmt. Expected: PASS.
- [ ] **Step 5: Commit** "Give the music an energy spot relative to the night"

### Task 3: The pilot: picks, pins and the drop sequence

**Files:**
- Create: `crates/autoblink-core-dmx/src/pilot.rs`
- Modify: `crates/autoblink-core-dmx/src/lib.rs`
- Test: `crates/autoblink-core-dmx/src/pilot.rs`

**Interfaces:**
- Consumes: `Auto`, `Tier`, `Move`, `Target` (Task 1); `Night`, `raw_energy` (Task 2); `autoblink_core_midi::scenes::Event`.
- Produces:

```rust
pub const PIN_MS: u64 = 30 * 60 * 1000;
pub const BUILD_BEATS: f64 = 64.0;
/// Without a phrase event for this many beats, the pilot picks again.
pub const REPICK_BEATS: f64 = 64.0;

pub struct PilotInput {
    pub now_ms: u64,
    pub playing: bool,
    pub bpm: f64,
    pub track_energy: Option<u8>,   // DropGuess::energy_at at the playhead
    /// The phrase events that the scene engine gave in this tick (also without presets).
    pub events: EventSet,           // a u32 bit set over Event::ALL; EventSet::contains(Event)
    /// The start time of the next Drop phrase, when the rekordbox phrases give it.
    pub drop_at_ms: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Program(u8),                                   // MIDI program number 0..=127
    Moves(u32),                                    // bit i: auto.moves[i] runs
    Energy { tier: Tier, spot: u8 },               // after each change
    Build { from_ms: u64, drop_at_ms: Option<u64> },
    Hit { strobe: bool },
    EndBuild,                                      // the build ends without a drop (stop, track change)
}
pub struct PilotStatus { pub tier: Tier, pub spot: u8, pub pinned: Option<Tier>,
    pub program: Option<u8> /* index into auto.programs */, pub moves: u32, pub drop_in_beats: Option<u16> }
#[derive(Default)] pub struct Pilot { /* night, tier, pin, history, plays, rng, build state, strobe time */ }
impl Pilot {
    pub fn tick(&mut self, auto: &Auto, input: &PilotInput, out: &mut impl FnMut(Action));
    pub fn pin(&mut self, tier: Option<Tier>, now_ms: u64);
    pub fn status(&self) -> PilotStatus;
    pub fn night(&self) -> &Night;
    pub fn set_night(&mut self, night: Night);
}
```

`EventSet` lives in `pilot.rs`: `pub struct EventSet(pub u32)` with `insert(Event)`, `contains(Event)`, built from the index of the event in `Event::ALL`. Add `pub fn EventSet::from_counts(prev: &[u8; 16], now: &[u8; 16]) -> EventSet`: an event is in the set when its count in `SceneStatus::events` changed (the counts wrap). The callers diff `SceneStatus::events` from tick to tick, because `drain_fired` reports only events with a preset row.

The rules:
1. Each tick feeds `Night::sample`. A new night clears the play counts and the history.
2. A phrase change is any `Event::is_phrase_event` in `input.events`, or `REPICK_BEATS` beats without one while playing.
3. At a phrase change, the tier follows `Tier::of(night.spot(raw))`, but only across a border by more than 4 points (hysteresis). A pin replaces the tier until `PIN_MS` ends.
4. At a phrase change, the pilot picks one program of the tier and one move for each target that has moves in the tier. It sends `Program`, `Moves` and, on a change, `Energy`.
5. The program list: the candidates are the programs of the tier. With none, the nearest program by spot. Exclude the last two picks when more than two candidates exist; with exactly two, exclude the last pick. Among the rest, take the lowest play count, and break ties with a xorshift random number. The moves use the same rule for each target, without the nearest fallback.
6. With `drop_at_ms` known and inside `BUILD_BEATS` beats (from the BPM), the pilot sends one `Build { from_ms: now, drop_at_ms }` for that drop. On `Event::Build` without a known drop time, it sends `Build { from_ms: now, drop_at_ms: None }`.
7. On `Event::Drop`, the pilot picks from Peak (programs and moves), sends `Program`, `Moves` and `Hit { strobe }`. `strobe` is false inside `strobe_cooldown_s` of the last strobe hit. A drop counts as the phrase change of that tick.
8. A pin of Calm or Groove: no `Build` and no `Hit`. `Event::Drop` is then a normal phrase change.
9. A stop of the playback or a missing next drop ends an open build with `EndBuild`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn picks_program_of_tier_on_phrase_change() // 3 Calm, 3 Groove, 3 Peak programs; a calm night; Event::Intro -> Program of a Calm program
#[test] fn never_repeats_last_two() // 4 programs in the tier; 40 phrase changes; no pick equals one of the two picks before it
#[test] fn prefers_least_played() // 3 programs; after 30 picks, each play count is 10
#[test] fn pick_small_lists() // 0 programs: no Program action, no panic; 1 program: picked each time; 2 programs: they alternate
#[test] fn nearest_program_when_tier_empty() // only Calm programs, Peak tier -> the program with the highest spot
#[test] fn one_move_per_target() // moves: 2 on Group(0), 1 on Group(1), 1 on Room, all Groove -> Moves mask has 3 bits, one from Group(0)
#[test] fn repicks_without_phrase_events() // playing, 120 BPM, no events, no track energy: a Program action after 64 beats (32 s), not before
#[test] fn tier_hysteresis() // spot moves 33 -> 36 -> 33 at phrase changes: tier stays Calm until > 38 (34 + 4)
#[test] fn pin_holds_and_expires() // pin(Some(Peak)) -> Peak picks; after PIN_MS + 1 the tier follows the energy again
#[test] fn build_before_known_drop() // drop_at_ms = now + 30 s at 128 BPM (64 beats = 30 s): one Build{drop_at_ms: Some(..)}, not two
#[test] fn build_from_guess() // Event::Build without drop_at_ms -> Build{drop_at_ms: None}
#[test] fn drop_hits_with_strobe_cooldown() // Drop at t: Hit{strobe:true}; Drop at t+60 s: Hit{strobe:false}; Drop at t+121 s: Hit{strobe:true}
#[test] fn drop_picks_peak() // calm night, Event::Drop -> Program of a Peak program
#[test] fn calm_pin_turns_off_drop() // pin Calm; Event::Build, Event::Drop -> no Build, no Hit; a Calm Program
#[test] fn peak_pin_keeps_drop()
#[test] fn stop_ends_build() // Build open, playing=false -> EndBuild
#[test] fn event_set_from_counts() // counts [0;16] -> Drop index 1: contains(Drop), not Build; 255 -> 0 wraps and counts
```

- [ ] **Step 2: Run** `cargo test --locked -p autoblink-core-dmx pilot::` Expected: FAIL.
- [ ] **Step 3: Implement** `pilot.rs`. Seed the xorshift from the first `now_ms` (never 0). `tick` allocates nothing: the history and the play counts are fixed arrays (`[u16; MAX_PROGRAMS]`, `[u16; MAX_MOVES]`).
- [ ] **Step 4: Run** the tests, clippy and fmt. Expected: PASS.
- [ ] **Step 5: Commit** "Pick the programs and the moves from the energy of the night"

### Task 4: Render the moves, the build and the room level

**Files:**
- Create: `crates/autoblink-core-dmx/src/moves.rs`
- Modify: `crates/autoblink-core-dmx/src/lights.rs`, `crates/autoblink-core-dmx/src/lib.rs`
- Test: `crates/autoblink-core-dmx/src/moves.rs`, `lights.rs`

**Interfaces:**
- Consumes: `Action` (Task 3), `Auto`, `Move`, `Tier` (Task 1), `render::{fixture_channels, group_fixtures}`, `clock::Beat`.
- Produces:

```rust
// moves.rs
/// The value 0.0..=1.0 of a move shape at the cycle phase `p` (0.0..1.0) for fixture k of n.
pub fn shape_value(shape: MoveShape, spread: Spread, p: f64, k: usize, n: usize) -> f64;
/// Writes the base (tier colour at the energy level) and the active moves into `out`.
/// It writes only fixtures with `console == false`, except Room moves (see room_factor).
pub fn render_auto(show: &Show, st: &AutoLook, beat: Option<&Beat>, now_ms: u64, out: &mut Universe);
/// The room level 0..=255 that Room moves give now (255 without a Room move).
pub fn room_factor(show: &Show, st: &AutoLook, beat: Option<&Beat>) -> u8;
/// Scales the light channels of the fixtures in place (see Global Constraints). `console`
/// scales the console fixtures, `all` scales every fixture.
pub fn apply_room(show: &Show, console: u8, all: u8, out: &mut Universe);
pub struct AutoLook { pub tier: Tier, pub spot: u8, pub moves: u32, pub speed: u8 /* 1 or 2 */, pub level: u8 }
// lights.rs
impl Lights {
    pub fn auto(&mut self, action: Action, now_ms: u64);   // keeps the AutoLook, build and hit state
    pub fn try_move(&mut self, index: u8, now_ms: u64);    // the move runs alone for 8 bars
    /// The (console, all) room levels for `apply_room`, after the merge.
    pub fn room(&self, show: &Show, beat: Option<&Beat>, live: &Live, now_ms: u64) -> (u8, u8);
}
// Live gets: pub blackout: bool
```

The rules:
- With `show.auto.on`, `Lights::frame` renders `render_auto` instead of the idle look and the layers. The aim override, the bursts, the fog and the strobe hold stay on top as now.
- The base: each box fixture shows the tier colour at the level `96 + 159 × spot / 100`.
- A move runs with its period `half_beats / 2` beats, divided by `speed`. `speed` is 2 when `faster` is on and the tier is Peak, and 2 in the second half of a build. A move phase starts on a bar line, as in `effect::shape_offset`.
- Param Pan, Tilt and PanTilt: Sweep, Circle and Figure8 reuse the math of `effect::shape_offset` with the amplitude `size` % of a quarter range. The other shapes on Pan or Tilt move the axis by `shape_value × size %` of a quarter range.
- Param Dimmer: the fixture level × `shape_value`. Param Colour: the colour goes from the tier colour to the move colour by `shape_value`. Param Strobe: the strobe channel is `size × 255 / 100` while `shape_value > 0.5`, else 0.
- `shape_value`: Pulse `(1 + cos 2πp) / 2`; Ramp `p`; Wave `(1 + sin 2πp) / 2`; Chase 1 when `floor(p × n) == k`, else 0 (no spread); Sweep, Circle and Figure8 `(1 + sin 2πp) / 2` for a non-pan param; StrobeBurst 1 in the first quarter of the cycle; Blackout 0 in the last `size` % of the cycle; Hold `size / 100`. Spread: Together adds 0, Fan adds `k / n`, Mirror adds `|k − (n − 1) / 2| / n`, Random adds a fixed hash of k in 0..1.
- Build: from `from_ms` to the drop, the box level climbs from 50 % to 100 %. The console level climbs from `dip` to 255. Without a drop time, the climb takes 64 beats. Blackout: from `drop_at_ms − blackout.beats() × ms_per_beat` to `drop_at_ms`, `all` is 0. Hit: the strobe channels of the box fixtures go to 255 for `strobe_beats` beats when `strobe` is true. `Live::blackout` makes `all` 0.
- `apply_room` runs after the merge in the firmware and the host, so it also reaches the console fixtures that come from DMX IN.
- A Room move (param Dimmer only) multiplies `all` by `room_factor`. A Room Blackout move gives `all` 0 in its dark part.
- Fix the existing live master in `frame` to the same light-channel rule (add Amber and Uv).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn shape_values() // Pulse(0)=1, Pulse(0.5)=0; Ramp(0.25)=0.25; Hold size 40 -> 0.4; Blackout size 25 at p=0.8 -> 0, at p=0.5 -> 1
#[test] fn fan_offsets_fixtures() // Fan with n=4: k=1 is 0.25 ahead of k=0
#[test] fn base_follows_tier_and_spot() // Groove, spot 50: an RGB par shows the Groove colour, dimmer 96+79=175
#[test] fn console_fixture_not_rendered() // console fixture channels stay 0 in render_auto
#[test] fn room_never_touches_pan_tilt() // apply_room(.., 0, 0, ..) on a head: pan/tilt/fine unchanged; dimmer 0; strobe 0
#[test] fn room_scales_colour_without_dimmer() // RGBWAU par without dimmer, all=128: each colour halves, amber and uv too
#[test] fn build_climbs_and_blacks_out() // Build{from 0, drop 8000 ms} at 120 BPM, blackout HalfBar (1000 ms): room(4000).0 ~ dip + half; room(7500).1 == 0; room(8100) after Hit == (255,255)
#[test] fn hit_strobes_box_only() // Hit{strobe:true}: box strobe channel 255 for 2 beats, console strobe channel untouched
#[test] fn live_blackout() // Live{blackout:true}: room().1 == 0
#[test] fn manual_rules_unchanged_when_auto_off() // existing lights tests keep passing; auto.on == false ignores Action
#[test] fn master_scales_amber_uv() // live master 128 on a fixture without dimmer halves amber and uv
```

- [ ] **Step 2: Run** `cargo test --locked -p autoblink-core-dmx` Expected: the new tests FAIL.
- [ ] **Step 3: Implement** `moves.rs` and the `Lights` changes. No heap allocation in `frame`, `room` or `apply_room`.
- [ ] **Step 4: Run** the tests, clippy and fmt. Expected: PASS.
- [ ] **Step 5: Commit** "Render the autopilot moves and the room level"

### Task 5: The box runs the autopilot

**Files:**
- Modify: `crates/autoblink-core-midi/src/lib.rs` (`Out::Program { ch: u8, prog: u8 }` and its `encode`: `[0xC0 | ch, prog & 0x7F]`, 2 bytes), `esp32/src/tracks.rs` (build `DropGuess` for every track), `esp32/src/midi.rs`, `esp32/src/dmx.rs`, `esp32/src/webui.rs`, `esp32/src/main.rs`, `esp32/src/settings.rs`.
- Test: `crates/autoblink-core-midi/src/lib.rs` (`program_encodes`); the firmware has no unit tests, so the check is the build of both configurations.

**Interfaces:**
- Consumes: `Pilot`, `PilotInput`, `Action`, `EventSet`, `PilotStatus` (Task 3); `Lights::auto`, `Lights::try_move`, `Lights::room`, `apply_room`, `Live::blackout` (Task 4); `Night::{to_bytes, from_bytes}`, `DropGuess::energy_at` (Task 2).
- Produces (firmware only):
  - `DmxMsg::Auto(Action)` and `DmxMsg::TryMove(u8)`.
  - `State` fields (under `#[cfg(feature = "dmx")]`): `auto_pin: AtomicU8` (255 no request, 0 Auto, 1 Calm, 2 Groove, 3 Peak), `auto_status: Mutex<Option<AutoStatus>>`, `show_rev: AtomicU32` (the show store increments it after each saved show), `night: Mutex<Option<[u8; NIGHT_BYTES]>>` (the `midi` thread puts the bytes, the main loop takes them). `DmxLive` gets `blackout: AtomicBool`.
  - `AutoStatus` (`Serialize`): `on: bool, tier: Tier, spot: u8, pinned: Option<Tier>, program: Option<u8>, moves: u32, drop_in_beats: Option<u16>`.
  - WebSocket commands: `{"type":"auto_pin","tier":"auto"|"calm"|"groove"|"peak"}`, `{"type":"dmx_blackout","on":bool}`, `{"type":"auto_try","move":n}` (n below `MAX_MOVES`). No reply.
  - Status field `auto` (skipped when `None`). Capability `auto` with the feature `dmx`.

The rules:
1. `tracks.rs` builds the guess for every track that has a preview, not only for a track without phrases. The scene engine still ignores the guess while `SceneInput::phrase` is set.
2. The `midi` thread keeps a `Pilot`, a copy of `show.auto` and the last `show_rev`. When `show_rev` changes, it takes the show with `try_lock` and clones `auto`. It never waits for the show lock.
3. Each pass, after `scenes.update` and `scenes.status`, the thread builds `PilotInput`:
   - `events` from `EventSet::from_counts` over the previous and the new `SceneStatus::events`;
   - `track_energy` from `guess.energy_at(playhead / length)`;
   - `drop_at_ms` from the `SongStructure` of the track: the first phrase after `beat_count` whose `Event::bucket(kind.code())` is `Drop`. `drop_at_ms = now_ms + beats × 60 000 / bpm`.
4. With `auto.on`, the scene engine output (`send` and `to_dmx`) and the `drain_fired` forward are skipped. The engine still runs for its events and status. The scene test keeps working.
5. `Action::Program(p)` sends `Out::Program { ch: auto.channel, prog: p }` with the existing `send`. Every action goes to the `dmx` thread as `DmxMsg::Auto(action)` with `send_back(.., 0)`.
6. The thread swaps `auto_pin` each pass and calls `pilot.pin`. It writes `auto_status` with `try_lock`.
7. Every 5 minutes, and when `Night::sample` starts a new night, the thread puts `pilot.night().to_bytes()` into `State::night`. The main loop takes the bytes and writes NVS key `night` (blob) through `Settings`. At boot, `Settings` loads `night`, and `midi::start` gets it for `pilot.set_night`.
8. The `dmx` thread handles `DmxMsg::Auto` (`lights.auto`) and `DmxMsg::TryMove` (`lights.try_move`). It reads `blackout` into `Live`. After the merge or `lts.apply`, it calls `lights.room(..)` and `apply_room` on `channels` when the show has `auto.on` or `live.blackout` is set.
9. `dmx_blackout` sets `dmx_live.blackout`. The paths that release the strobe hold (`release_strobe`: page close, failed send, take-over) also release the blackout.
10. `auto_try` sends `DmxMsg::TryMove(n)`.

- [ ] **Step 1: Write the failing test** `program_encodes`: `Out::Program { ch: 2, prog: 11 }` encodes to `[0xC2, 11]`; `prog: 200` gives `[0xC2, 72]`.
- [ ] **Step 2: Run** `cargo test --locked -p autoblink-core-midi program_encodes` Expected: FAIL.
- [ ] **Step 3: Implement** `Out::Program`, then the firmware rules 1 to 10.
- [ ] **Step 4: Run** `sh scripts/check.sh --esp32`. Expected: PASS, no firmware warnings, both configurations build. The firmware without `dmx` has no autopilot.
- [ ] **Step 5: Commit** "Run the light autopilot on the box"

### Task 6: The host runs the autopilot

**Files:**
- Modify: `host/src/midi/mapper.rs`, `host/src/dmx.rs`, `host/src/web/mod.rs` (commands, status, `StrobeGuard`), `host/src/state/dj_state.rs` only if the phrase beats need an accessor, `host/src/config.rs` (the path of `night.bin` next to `show.json`).
- Test: `host/src/dmx.rs`, `host/src/web/mod.rs`, `host/src/midi/mapper.rs` test modules.

**Interfaces:**
- Consumes: as Task 5, and the host `program_change(ch, prog)` helper.
- Produces: host `DmxMsg::Auto(Action)`, `DmxMsg::TryMove(u8)`; `DmxLive::blackout: AtomicBool`; the same three WebSocket commands, the status field `auto` and the capability `auto` (when the host has the `dmx` capability); `night.bin` (the `Night` bytes) in the folder of the config file.

The rules:
1. The mapper runs `Pilot` like the firmware (Task 5, rules 2 to 6). It reads `show.auto` from the host show copy without blocking the mapper task (`try_read` or a cloned snapshot after each `/show` POST). The program change uses `program_change(auto.channel, p)`.
2. With `auto.on`, the mapper does not send the scene notes and does not forward the scene messages to the DMX task.
3. `drop_at_ms` comes from the song structure of the master in `DjState`: the next phrase whose bucket is `Drop`.
4. The DMX task handles the new messages and calls `apply_room` before `build_artdmx`.
5. Every 5 minutes and at a new night, the mapper writes `night.bin` on a blocking thread (`spawn_blocking`). At start, it reads the file; a missing or bad file gives `Night::default()`.
6. `dmx_blackout` sets `DmxLive::blackout`. `StrobeGuard` also releases the blackout when the page closes.

- [ ] **Step 1: Write the failing tests**: `blackout_released_on_close` (a page that set `dmx_blackout` true closes: `blackout` is false); `auto_commands_parse` (`auto_pin` "peak" sets the pin request; a bad tier is ignored; `auto_try` 99 is ignored); `night_file_round_trip` (write then read gives the same bytes; a 3-byte file gives the default).
- [ ] **Step 2: Run** `cargo test --locked -p autoblink-host` Expected: FAIL.
- [ ] **Step 3: Implement** the rules.
- [ ] **Step 4: Run** `cargo test --locked`, clippy and fmt. Expected: PASS.
- [ ] **Step 5: Commit** "Run the light autopilot on the host"

### Task 7: The autopilot setup on the page

**Files:**
- Modify: `webui/index.html`

**Interfaces:**
- Consumes: `show.auto` (Task 1 JSON), `show.fixtures[i].console`, status field `auto` and capability `auto` (Tasks 5 and 6), `showEdit`, `showSave`, `send`.
- Produces: `renderAuto()`, `autoView` (`'cols' | 'tabs' | 'curve'`, kept in `localStorage['autoView']`), `openMove(i)` (Task 8 fills the editor), the panel `details#p-auto data-cap="auto"`.

The rules (follow `autopilot-layout.html`):
1. A new panel "Autopilot" above "DMX Scenes & Looks". Its header has the driver switch (Autopilot / Manual rules, `auto.on`) and the view switch (A columns, B tabs, C curve).
2. The live line from `status.auto`: tier, BPM, energy meter (`spot`), the current program name, the current move names, "drop in ~N bars" when `drop_in_beats` is set, and "pinned" when `pinned` is set.
3. View A: three columns. Each column lists its programs and moves; the column of the current tier glows. Drag a chip to another column: its spot goes to the middle of that tier (Calm 17, Groove 57, Peak 90). View B: tabs Calm, Groove, Peak with the same lists and larger cards. View C: one axis; drag a chip along it to set its spot 0 to 100.
4. "+ program" adds `{pc, name, color, spot}` with a small form (program number 1 to 128 shown, 0 to 127 stored). A tap on a program chip opens the same form, with "Delete". "+ move" adds a default move (Pulse on the first group, Dimmer, 4 beats, size 60, Fan, faster on, spot of the column) and calls `openMove(i)`.
5. The drop strip: blackout length (Off, 1 beat, ½ bar, 1 bar), the console dip (slider 0 to 100 %), the strobe beats (0 to 8) and the strobe cooldown (30 s to 10 min). The safety line shows the strobe cooldown and the fog cooldown of the looks.
6. A setting row: the MIDI channel of the program changes (1 to 16), the tier colours, and the PIN (0 to 8 digits).
7. The Lights setup patch list gets a "console" toggle for each fixture.
8. With a PIN set, the panel and the Lights setup show a PIN field first. A correct PIN unlocks them until the page closes (`sessionStorage`). The PIN is an accident guard, not security.
9. With `auto.on`, "DMX Scenes & Looks" shows a line "The autopilot drives the lights. Manual rules are paused." and stays editable.
10. Every edit uses `showEdit` and `showSave`. Keep the added markup, CSS and JS small: the image is at 89 %.

Note: a program test needs a MIDI send from the page. It is out of scope; the chip form has no Test button.

- [ ] **Step 1:** Implement rules 1 to 10.
- [ ] **Step 2:** Check against a stub: serve `webui/` with `python3 -m http.server 8000 -d webui` and a stub WebSocket and `/show` (reuse the stub of the earlier DMX plans in `.context/` if present). The console is clean. Switch the three views; a drag in C moves the chip in A. Screenshots to `.context/auto-*.png`.
- [ ] **Step 3: Run** `sh scripts/check.sh --esp32`. Expected: PASS; the image is below 90 % of the slot.
- [ ] **Step 4: Commit** "Set up the autopilot on the page"

### Task 8: The move editor and the live screen

**Files:**
- Modify: `webui/index.html`

**Interfaces:**
- Consumes: `openMove(i)`, `renderAuto()` (Task 7); `auto_try`, `auto_pin`, `dmx_blackout`, `dmx_fog` commands.

The rules:
1. The move editor (follow `move-editor.html`) is a sheet with four steps: the shape grid (ten shapes with a small SVG curve), the target (Whole room, Group, One fixture) with the rig map and the parameter chips, the knobs (speed segments ½ to 32 beats, "faster when the energy rises", size, spread, colour from the tier or fixed), and the preview.
2. The parameter chips show only the parameters that the selected fixtures have (Pan and Tilt need pan or tilt channels; Strobe needs a strobe channel; Dimmer is always there). The Whole room target offers only Dimmer.
3. The preview draws the shape curve over 2 bars from the same `shape_value` formulas (a JS copy of Task 4) with a moving playhead at the live BPM. A short comment names the Rust source of the formulas.
4. "Try now" sends `{"type":"auto_try","move":i}`. "Done" closes the sheet. "Delete" removes the move.
5. The live screen opens with `#live` in the URL, or with a "Live screen" button in the Autopilot panel. It hides everything else and shows large buttons: Auto, Calm, Groove, Peak (`auto_pin`), Blackout (sends `dmx_blackout` true on press and false on release, also on `pointercancel` and `pagehide`), Fog (`dmx_fog`). It shows the tier and the BPM. It needs no PIN.
6. On the live screen, the pressed tier button stays lit while `status.auto.pinned` matches it.

- [ ] **Step 1:** Implement rules 1 to 6.
- [ ] **Step 2:** Check against the stub. Blackout release on `pointercancel`; the editor hides Pan for a par. Screenshots to `.context/auto-editor.png` and `.context/auto-live.png`.
- [ ] **Step 3: Run** `sh scripts/check.sh --esp32`. Expected: PASS; below 90 %.
- [ ] **Step 4: Commit** "Edit the autopilot moves and add the live screen"

### Task 9: Version, docs, checks and flash

**Files:**
- Modify: `Cargo.toml`, `esp32/Cargo.toml`, `Cargo.lock`, `esp32/Cargo.lock` (1.31.0), `AGENTS.md`.

- [ ] **Step 1:** Bump to `1.31.0` with the lock files.
- [ ] **Step 2:** AGENTS.md: a section "Light autopilot" with the tiers and their borders, the night memory (NVS key `night`, `night.bin` on the host, `NIGHT_GAP_MS`), the pick rule, the drop sequence and the pin rules, the room stage after the merge and its light-channel rule, the new commands and the status field `auto`, `Out::Program`, the guess for every track, and the new image size. Update the architecture map row of `autoblink-core-dmx` (`auto.rs`, `night.rs`, `pilot.rs`, `moves.rs`). Check it with the STE checker.
- [ ] **Step 3: Run** `sh scripts/check.sh --esp32`. Expected: PASS. Note the image size.
- [ ] **Step 4: Commit** "Release the light autopilot as 1.31.0"
- [ ] **Step 5: Flash** over WiFi: `sh scripts/ota-push.sh`. Expected: the box reports 1.31.0. Then `curl http://autoblink.local/mem` and record the free stack of `midi` and `dmx`.
