# DMX Planner 1: Looks on the Box — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A preset of the DMX scenes can be a look. The ESP32 sends the look on its own DMX output (IO4), in time with the beat.

**Architecture:** A new crate `autoblink-core-dmx` holds the show model, the look renderer, the beat effects, a beat clock and the `Lights` engine. The scene engine stays as it is. The `midi` thread forwards the scene notes and the fired rows to the `dmx` thread through a FreeRTOS queue. The `dmx` thread renders one frame about every 24 ms. The show is JSON in a new flash partition `show`. The page edits it with `GET /show` and `POST /show`.

**Tech Stack:** Rust 2021, serde and serde_json, ESP-IDF 5.5.3 through esp-idf-svc 0.53, vanilla JS in `webui/index.html`.

**Spec:** `docs/superpowers/specs/2026-10-27-dmx-planner-design.md`

## Plan series

This plan is the first of five. Each plan gives working software. The owner wants one release, so all five plans go on one branch before the merge.

1. Looks on the box (this plan): generic fixtures, groups, looks, intensity effects, strobe and fog bursts, idle look, live strip.
2. OFL import: the box proxies the Open Fixture Library, the browser parses it.
3. Positions and shapes: pan and tilt channels, named positions, circle, figure 8 and sweep.
4. Drop guess: bucket events from the waveform energy for tracks without phrase analysis.
5. Art-Net and host: Art-Net input with the HTP/LTP merge, the host Lights panel, host Art-Net output to a node.

## Global Constraints

- Do not use `f32` in a type that serde reads on the firmware. Use `u8`, `u16` or `f64`.
- Do not use `std::sync::mpsc` toward a thread with a higher priority. Use `esp_idf_svc::hal::task::queue::Queue`.
- Do not use `std::io::copy` in a firmware thread.
- Build a message to the page as a `Serialize` struct, never as a `serde_json::Value` tree.
- The `dmx` thread has priority 6 on core 1. The `midi` thread has 10, `prolink-rx` has 8.
- The app image must stay below 0x1E0000 bytes.
- The show limits: 32 fixtures, 32 channels for each fixture, 8 groups, 32 looks (`scenes::MAX_PRESETS`), names of 16 characters.
- Comments, log lines and error strings follow ASD-STE100, as the rest of the repo.
- Bump the version to `1.24.0` in `Cargo.toml` (`[workspace.package]`) and in `esp32/Cargo.toml` in the last task.

## Review Focus

1. A show from an older or a damaged partition (erased flash, bad JSON, wrong limits) must give the default show, never a panic. Test: `show::tests::load_bad_json_gives_default` (Task 2) and the firmware load path (Task 6).
2. A fixture with an address near 512 must not write past the universe. Test: `render::tests::fixture_past_512_is_cut` (Task 3).
3. A pulse row sends note on and note off at once. The look must still show. Test: `lights::tests::pulse_shows_for_min_on` (Task 5).
4. The rig must not go dark when the master stops and the Stop row has no look. Test: `lights::tests::stop_without_look_shows_idle_after_2_s` (Task 5).
5. A late WiFi beat packet must not shift the flash. Test: `clock::tests::late_packets_keep_phase` (Task 4).

---

### Task 1: The scene engine reports each fired preset

**Files:**
- Modify: `crates/autoblink-core-midi/src/scenes.rs` (struct `Scenes`, `fire`, `step_bars`, tests)

**Interfaces:**
- Produces: `Scenes::drain_fired(&mut self, f: impl FnMut(Event, u8))`. It calls `f(event, preset)` for each preset that a row sent since the last call, in order, and clears the list. `preset` is the entry of `Row::presets` (0-based preset index, or `EXTRA_BASE` and up). Also `pub fn is_phrase_event(e: Event) -> bool` on `Event`: true for Intro, Groove, Build, Drop, Break, Outro, PhraseChange and Fill.

- [ ] **Step 1: Write the failing tests** in the `tests` module of `scenes.rs`:

```rust
#[test]
fn drain_fired_reports_event_and_preset() {
    let mut c = cfg(vec![row(Event::Drop, &[3, 4])]);
    c.enabled = true;
    let mut s = Scenes::default();
    let input = SceneInput { device: 1, track: 9, playing: true, bpm: 120.0,
        phrase: Some(ScenePhrase { idx: 0, code: Some(8), beats_left: 16, fill_beats_left: None }),
        bass_cut: None };
    s.update(&c, 1000, &input, &mut |_| {});
    let mut got = Vec::new();
    s.drain_fired(|e, p| got.push((e, p)));
    assert_eq!(got, vec![(Event::Drop, 3)]);
    let mut again = Vec::new();
    s.drain_fired(|e, p| again.push((e, p)));
    assert!(again.is_empty());
}

#[test]
fn phrase_events() {
    assert!(Event::Drop.is_phrase_event());
    assert!(Event::Fill.is_phrase_event());
    assert!(!Event::Beat.is_phrase_event());
    assert!(!Event::Stop.is_phrase_event());
}
```

Add a test `drain_fired_reports_bar_steps`: a Drop row with `advance: 1` and presets `[0, 1]`. After the trigger and one downbeat beat later than half a beat, the drain gives `(Drop, 0)` and then `(Drop, 1)`.

- [ ] **Step 2: Run the tests.** Run: `cargo test -p autoblink-core-midi scenes`. Expected: FAIL, `drain_fired` and `is_phrase_event` do not exist.
- [ ] **Step 3: Implement.** Add `fired: Vec<(Event, u8)>` to `Scenes` (capacity 8 in `Default`). Push in `send` with `row.event` and `row.presets[step]`. Do not push while `cfg.enabled` is false. Cap the list at 16 entries; drop the oldest.
- [ ] **Step 4: Run the tests.** Run: `cargo test -p autoblink-core-midi`. Expected: PASS, the old scene tests too.
- [ ] **Step 5: Commit.** `git commit -m "Report each fired preset of the DMX scenes"`

### Task 2: The show model

**Files:**
- Create: `crates/autoblink-core-dmx/Cargo.toml`, `crates/autoblink-core-dmx/src/lib.rs`, `crates/autoblink-core-dmx/src/show.rs`
- Modify: `Cargo.toml` (add the member `crates/autoblink-core-dmx`)

**Interfaces:**
- Consumes: nothing.
- Produces, in `show.rs` (re-exported from `lib.rs`), all `Debug, Clone, PartialEq, Serialize, Deserialize`, enums `#[serde(rename_all = "snake_case")]`:

```rust
pub const UNIVERSE: usize = 512;
pub const MAX_FIXTURES: usize = 32;
pub const MAX_CHANNELS: usize = 32;
pub const MAX_GROUPS: usize = 8;
pub const MAX_LOOKS: usize = 32;
pub const MAX_NAME: usize = 16;

pub enum Channel { Dimmer, Red, Green, Blue, White, Amber, Uv, Strobe, Fog, Fixed(u8) }
pub struct Fixture { pub name: String, pub address: u16, pub channels: Vec<Channel> }
pub struct Group { pub name: String, pub fixtures: Vec<u8> }
pub enum Fade { Cut, Beat, Bar }                       // Default: Cut
pub enum Every { Beat, Half, Bar }                     // Default: Beat
pub enum Effect { None, Flash { every: Every }, Pulse { every: Every }, Chase { every: Every },
                  Random { every: Every }, Strobe { rate: u8, beats: u8 }, Fog { ms: u16 } } // Default: None
pub struct LookRow { pub group: u8, pub color: [u8; 4], pub intensity: u8,
                     #[serde(default)] pub effect: Effect, #[serde(default)] pub cooldown_s: u16 }
pub struct Look { #[serde(default)] pub fade: Fade, pub rows: Vec<LookRow> }
#[serde(default)]
pub struct Show { pub fixtures: Vec<Fixture>, pub groups: Vec<Group>,
                  pub looks: Vec<Option<Look>>, pub idle: Option<u8> }  // Default: all empty

impl Show {
    pub fn sanitize(&mut self);
    pub fn look(&self, preset: u8) -> Option<&Look>;
    pub fn from_json(bytes: &[u8]) -> Show;   // bad JSON gives Show::default(); always sanitized
}
```

`color` is red, green, blue, white. `address` is the DMX start channel, 1 to 512. `looks[i]` is the look of preset `i`.

- [ ] **Step 1: Write the failing tests** in `show.rs`:
  - `sanitize_cuts_limits`: 40 fixtures, a name of 30 characters, 40 channels, 10 groups, 40 looks → 32, 16, 32, 8, 32.
  - `sanitize_drops_bad_refs`: a group entry 99, a row with group 9, `idle: Some(5)` with `looks[5] == None` → the entry and the row are gone, `idle` is `None`.
  - `sanitize_clamps_values`: address 0 → 1, address 600 → 512, `Strobe { beats: 0 }` → 1, `Strobe { beats: 99 }` → 16, `Fog { ms: 5 }` → 100, `Fog { ms: 60000 }` → 10000, `cooldown_s: 5000` → 600.
  - `json_round_trip`: a show with each `Channel` variant and each `Effect` variant survives `to_string` then `from_json`. The JSON of `Channel::Fixed(5)` is `{"fixed":5}`, of `Channel::Dimmer` is `"dimmer"`.
  - `load_bad_json_gives_default`: `from_json(b"\xff\xff")` and `from_json(b"{\"fixtures\":7}")` give `Show::default()`.
- [ ] **Step 2: Run the tests.** Run: `cargo test -p autoblink-core-dmx`. Expected: FAIL to compile.
- [ ] **Step 3: Implement** the types and the three methods. The crate depends on `serde` and on `autoblink-core-midi` (path), with `serde_json` as a normal dependency (`from_json` needs it).
- [ ] **Step 4: Run the tests.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -m "Add the show model of the DMX planner"`

### Task 3: Render a look into the universe

**Files:**
- Create: `crates/autoblink-core-dmx/src/render.rs`

**Interfaces:**
- Consumes: `Show`, `Look`, `Channel` (Task 2).
- Produces:

```rust
/// One value for each channel of the universe. Index 0 is DMX channel 1.
pub type Universe = [u8; UNIVERSE];
/// Writes the static values of `look` into `out`. `level(row, fixture_pos, fixture_count)`
/// gives the effect factor 0 to 255 of a fixture in a row; 255 is no change.
pub fn render(show: &Show, look: &Look, level: impl Fn(usize, usize, usize) -> u8, out: &mut Universe);
/// Returns the set of channel indices that `look` touches (the channels of the fixtures of its groups).
pub fn touched(show: &Show, look: &Look) -> impl Iterator<Item = usize> + '_;
```

The channel rules, for each fixture of the group of a row:
- `Dimmer` = intensity × level / 255.
- `Red`, `Green`, `Blue`, `White` = the colour value. If the fixture has no `Dimmer`, also × intensity × level / 255².
- `Amber`, `Uv`, `Strobe`, `Fog` = 0. `Fixed(v)` = v.

- [ ] **Step 1: Write the failing tests:**
  - `rgb_par_without_dimmer_scales_colour`: fixture at address 1 with `[Red, Green, Blue]`, row colour `[255, 128, 0, 0]`, intensity 128, level 255 → `out[0..3] == [128, 64, 0]`.
  - `dimmer_takes_intensity`: `[Dimmer, Red, Green, Blue]`, intensity 200, colour `[10, 20, 30, 0]` → `[200, 10, 20, 30]`.
  - `level_scales_dimmer`: same fixture, level 128 → dimmer 100.
  - `fixed_channel_keeps_value`: `[Fixed(7), Dimmer]` → `out[0] == 7`.
  - `fixture_past_512_is_cut`: address 511 with 4 channels → only `out[510]` and `out[511]` change; no panic.
  - `touched_lists_group_channels`: two fixtures in the group, 3 channels each at 1 and 10 → `{0,1,2,9,10,11}`.
- [ ] **Step 2: Run** `cargo test -p autoblink-core-dmx render`. Expected: FAIL.
- [ ] **Step 3: Implement** `render` and `touched`.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -m "Render a DMX look into the universe"`

### Task 4: The beat clock and the intensity effects

**Files:**
- Create: `crates/autoblink-core-dmx/src/clock.rs`, `crates/autoblink-core-dmx/src/effect.rs`

**Interfaces:**
- Produces:

```rust
// clock.rs
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat { pub phase: f64, pub beat_in_bar: u8, pub count: u32, pub ms_per_beat: f64 }
#[derive(Default)]
pub struct BeatClock { /* private */ }
impl BeatClock {
    pub fn beat(&mut self, now_ms: u64, beat_in_bar: u8, bpm: f64);
    /// `None` before the first beat, and 2000 ms after the last beat packet.
    pub fn now(&self, now_ms: u64) -> Option<Beat>;
}
// effect.rs
/// The factor 0 to 255 of fixture `k` of `n` in a row with `effect`. 255 for `None`, `Strobe` and `Fog`.
pub fn factor(effect: &Effect, beat: Option<&Beat>, k: usize, n: usize) -> u8;
```

The clock rule: the clock keeps an anchor, the predicted time of the last beat. A packet earlier than the prediction moves the anchor to the packet. A late packet does not move it. If the earliest of the last 8 packets is still late by more than 20 ms, the anchor moves by that amount. The clock expects the next packet to have the next `beat_in_bar`. A packet with another `beat_in_bar`, a packet more than 1 beat late, or a packet more than half a beat early sets the anchor to the packet. WiFi packets can be 300 ms late, which is more than half a beat at 120 BPM. `now` predicts the beats after the anchor: `count` and `beat_in_bar` go on, and `phase` stays in 0 to 1.

The effect formulas, with `p` the phase of the period (`Every::Beat`: the beat phase; `Half`: `frac(2 × phase)`; `Bar`: `((beat_in_bar − 1) + phase) / 4`) and `c` the period count (`count`, `2 × count + (phase ≥ 0.5)`, `count / 4`):
- Flash: `255 × (1 − p)³`.
- Pulse: `255 × (1 + cos 2πp) / 2`.
- Chase: 255 when `c mod n == k`, else 0.
- Random: 255 when `xorshift32(c + 1) mod n == k`, else 0.
- Without a beat (`None`), each effect gives 255.

- [ ] **Step 1: Write the failing tests:**
  - `clock::late_packets_keep_phase`: beats at 120 BPM, true times 0, 500, 1000, 1500, 2000 ms, packets late by 0, 300, 100, 250, 180 ms. At 2000 ms, `now(2000).phase < 0.02`, `count == 4`.
  - `clock::early_packet_moves_anchor`: steady 500 ms beats, then a packet 40 ms early → the phase at that packet time is 0.
  - `clock::stops_after_2_s`: `now(last + 2001)` is `None`.
  - `clock::predicts_between_packets`: one beat at 0 with `beat_in_bar` 4, 120 BPM. `now(750)` gives `count == 1`, `beat_in_bar == 1`, `phase == 0.5`.
  - `effect::flash_shape`: phase 0 → 255, phase 0.25 → 107 or 108, phase 0.99 → 0.
  - `effect::pulse_shape`: phase 0 → 255, phase 0.5 → 0.
  - `effect::chase_steps`: n 3, count 0..6 → lit k = 0,1,2,0,1,2.
  - `effect::half_and_bar`: Flash Half at phase 0.5 → 255; Flash Bar at beat_in_bar 3, phase 0 → `255 × 0.5³` = 31 or 32.
  - `effect::no_beat_is_full`: `factor(&Flash, None, 0, 1) == 255`.
- [ ] **Step 2: Run** `cargo test -p autoblink-core-dmx`. Expected: FAIL.
- [ ] **Step 3: Implement** `BeatClock` and `factor`.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -m "Add the DMX beat clock and the intensity effects"`

### Task 5: The Lights engine

**Files:**
- Create: `crates/autoblink-core-dmx/src/lights.rs`

**Interfaces:**
- Consumes: Tasks 2 to 4; `autoblink_core_midi::scenes::Event`.
- Produces:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Live { pub master: u8, pub strobe_hold: bool }   // master 255 is full
pub const MIN_ON_MS: u64 = 80;
pub const IDLE_AFTER_MS: u64 = 2000;
pub const FOG_BUTTON_MS: u64 = 3000;
#[derive(Default)]
pub struct Lights { /* private */ }
impl Lights {
    pub fn on(&mut self, preset: u8, now_ms: u64);          // a scene note on
    pub fn off(&mut self, preset: u8, now_ms: u64);         // a scene note off
    pub fn fired(&mut self, show: &Show, event: Event, preset: u8, now_ms: u64);
    pub fn test(&mut self, show: &Show, preset: u8, now_ms: u64); // page click: a manual look
    pub fn fog(&mut self, now_ms: u64);                     // the fog button
    pub fn set_playing(&mut self, playing: bool, now_ms: u64);
    pub fn frame(&mut self, show: &Show, beat: Option<&Beat>, live: &Live, now_ms: u64, out: &mut Universe);
    /// The presets that show now, bottom first, for the page.
    pub fn layers(&self) -> impl Iterator<Item = u8> + '_;
}
```

The rules:
- The engine keeps a list of layers in the order of their `on`. An `on` of a layer that shows already does nothing.
- `frame` starts at 0 for each channel. It renders each layer with `render`, in order. A layer writes only its `touched` channels. A layer fades in from the values under it: `Cut` 0 ms, `Beat` one beat, `Bar` four beats (500 ms for each beat without a clock).
- An `off` less than `MIN_ON_MS` after the `on` takes effect at `on + MIN_ON_MS`.
- `fired` with a phrase event (`Event::is_phrase_event`) starts the `Strobe` and `Fog` rows of the look, if the cooldown of the row has passed. A `Strobe` burst sets the `Strobe` channels of the group to `rate` for `beats` beats. A `Fog` burst sets the `Fog` channels to 255 for `ms`. `fired` with any event that is not an overlay (`!is_overlay()`) removes the manual layers.
- `test` removes the manual layers, adds `preset` as a manual layer and starts its bursts without the cooldown.
- With no layer, the idle look shows. After `set_playing(false)`, the idle look shows when `IDLE_AFTER_MS` passed and no `on` came after the stop.
- `master` scales each `Dimmer` channel, and the colour channels of a fixture without `Dimmer`. `strobe_hold` sets each `Strobe` channel to 255. `fog` sets each `Fog` channel to 255 for `FOG_BUTTON_MS`.
- A look that the show does not have (`show.look(preset) == None`) is still a layer, but it renders nothing.

- [ ] **Step 1: Write the failing tests** (a helper show: fixture 1 `[Dimmer, Red, Green, Blue]` at 1, fixture 2 `[Strobe]` at 10, fixture 3 `[Fog]` at 20; groups "par" `[0]`, "strobe" `[1]`, "fog" `[2]`; look 0 red par, look 1 blue par, look 2 a strobe row `Strobe { rate: 200, beats: 2 }` with `cooldown_s: 30` plus a fog row `Fog { ms: 1000 }`, idle look 3 white par at intensity 50):
  - `layers_stack_and_unstack`: on 0, on 1 → blue; off 1 → red.
  - `pulse_shows_for_min_on`: on 1, off 1 at the same ms → blue at +40 ms, the idle look at +81 ms.
  - `fade_beat`: look 1 with `Fade::Beat`, a clock at 120 BPM → at +250 ms the blue channel is about half (127 ± 2).
  - `strobe_only_on_phrase_event`: on 2 then `fired(Beat, 2)` → strobe channel 0; `fired(Drop, 2)` → 200, for 2 beats.
  - `strobe_cooldown`: `fired(Drop, 2)` at 0 and at 10 000 ms → second burst gives 0; at 31 000 ms → 200.
  - `test_ignores_cooldown`: after a Drop burst at 0, `test(2)` at 1000 ms → strobe 200.
  - `manual_layer_ends_on_next_scene`: `test(0)`; `fired(Beat, 1)` keeps it; `fired(Drop, 1)` removes it.
  - `no_layer_shows_idle`: no layers → par dimmer 50, colour white.
  - `stop_without_look_shows_idle_after_2_s`: on 0, `set_playing(false)` at 0 → red at 1999 ms, idle at 2001 ms; an `on 1` at 500 ms → blue at 2001 ms.
  - `master_scales_dimmer`: master 128 → red look dimmer 127 or 128.
  - `strobe_hold_and_fog_button`: `strobe_hold` → channel 10 is 255; `fog(0)` → channel 20 is 255 at 2999 ms and 0 at 3001 ms.
- [ ] **Step 2: Run** `cargo test -p autoblink-core-dmx lights`. Expected: FAIL.
- [ ] **Step 3: Implement** `Lights`.
- [ ] **Step 4: Run** `cargo test --locked`. Expected: PASS for the whole workspace.
- [ ] **Step 5: Commit.** `git commit -m "Add the Lights engine of the DMX planner"`

### Task 6: The show partition on the firmware

**Files:**
- Modify: `esp32/partitions.csv` (add `show, data, 0x40, 0x3F0000, 0x10000`), `esp32/Cargo.toml` (add `autoblink-core-dmx`, make `dmx` a default feature: `default = ["dmx"]`)
- Create: `esp32/src/show.rs`

**Interfaces:**
- Produces:

```rust
pub struct ShowStore { /* EspPartition "show" */ }
impl ShowStore {
    /// `None` when the partition table has no "show" partition (a box flashed before 1.24.0).
    pub fn open() -> Option<ShowStore>;
    /// Reads the show. Erased flash or bad data gives `Show::default()`.
    pub fn load(&mut self) -> Show;
    /// Writes the show as JSON. Erases only the sectors that the new data needs.
    pub fn save(&mut self, show: &Show) -> anyhow::Result<()>;
    /// Streams the stored JSON to `w` in chunks of 1 KB.
    pub fn write_json(&mut self, w: &mut impl FnMut(&[u8]) -> anyhow::Result<()>) -> anyhow::Result<()>;
}
```

The format: bytes 0 to 3 are the JSON length, little-endian `u32`. The JSON follows. A length of `0xFFFF_FFFF` or more than `0x10000 − 4` means "no show". `save` writes the JSON through a writer with a 256-byte buffer (`serde_json::to_writer`), and writes the length last. A power loss during a save thus leaves "no show", not half a show. `load` parses with `serde_json::from_reader` over a reader with a 256-byte buffer, then calls `sanitize`.

- [ ] **Step 1: Implement** `show.rs`. Use `esp_idf_svc::partition::EspPartition::new("show")`.
- [ ] **Step 2: Build.** Run: `. ~/export-esp.sh && cd esp32 && cargo build --release`. Expected: the build passes. Check the size: `espflash save-image --chip esp32 target/xtensa-esp32-espidf/release/autoblink-esp32 /tmp/app.bin && stat -f%z /tmp/app.bin`. Expected: below 1966080 (0x1E0000).
- [ ] **Step 3: Commit.** `git commit -m "Add the show partition to the firmware"`

### Task 7: The DMX thread renders the looks

**Files:**
- Modify: `esp32/src/dmx.rs` (replace the beat flash), `esp32/src/main.rs` (start order, `State` fields), `esp32/src/midi.rs` (forward the scene notes)

**Interfaces:**
- Consumes: `Lights`, `BeatClock`, `Show` (Tasks 2 to 5); `Scenes::drain_fired` (Task 1); `ShowStore` (Task 6).
- Produces:

```rust
// dmx.rs
#[derive(Clone, Copy)]
pub enum DmxMsg { On(u8), Off(u8), Fired(Event, u8), Test(u8), Fog, Beat { beat_in_bar: u8, bpm: f64, at_ms: u64 } }
pub struct DmxLive { pub master: AtomicU8, pub strobe_hold: AtomicBool }   // master starts at 255
#[derive(Default, Clone, Serialize)]
pub struct DmxStatus { pub layers: Vec<u8>, pub master: u8, pub strobe_hold: bool }
pub fn start(uart: UART1<'static>, tx: Gpio4<'static>, rx: Gpio36<'static>, state: Arc<State>,
             show: Arc<Mutex<Show>>) -> anyhow::Result<Arc<Queue<DmxMsg>>>;
// main.rs State: pub dmx_live: DmxLive, pub dmx_status: Mutex<DmxStatus>, pub show: Arc<Mutex<Show>>
```

- [ ] **Step 1: Start the DMX output before the network.** In `run`, open the `ShowStore` and load the show right after `Settings::new`. Move the creation of `State` up to the same place; it needs nothing from the network. Start `dmx::start` before `net::start_wifi`, so that the maintenance start of an OTA update also sends the idle look.
- [ ] **Step 2: Write the frame loop.** Thread `crate::spawn_rt(c"dmx", 6 * 1024, 6, Some(Core::Core1), ..)`. Each pass: take the messages from the queue (wait until the next frame time), give them to `Lights` and `BeatClock`, call `lights.set_playing` from `state.tracker` master `is_playing`, call `lights.frame`, write the frame with `write_with_break` as today, update `dmx_status` without waiting for its lock (`try_lock`).
- [ ] **Step 3: Send the beats.** In the `prolink::spawn` beat closure, replace `dmx.beat()` with `dmx_q.send_back(DmxMsg::Beat { .. }, 0)`. A full queue drops the beat.
- [ ] **Step 4: Forward the scene notes.** In `midi.rs`, wrap the `send` closure that `scenes.beat` and `scenes.update` get. For `Out::NoteOn` and `Out::NoteOff` on `scenes_cfg.channel` with a note in `first_note .. first_note + count`, also queue `On(note − first_note)` or `Off(..)`. After each `scenes.beat` and `scenes.update`, call `drain_fired` and queue `Fired`. For a `scene_test`, also queue `Test(preset)`. Use `send_back(msg, 0)` everywhere; the `midi` thread never waits.
- [ ] **Step 5: Build and flash.** Run: `cd esp32 && cargo run --release`. The partition table changes, so this needs USB. Expected: the log shows "DMX output is on IO4" before the WiFi lines.
- [ ] **Step 6: Check the stacks.** Run: `curl http://autoblink.local/mem`. Expected: `dmx` and `midi` keep at least 1.5 KB free. Raise the stack size if not, and note the measured value in the `spawn_rt` comment.
- [ ] **Step 7: Commit.** `git commit -m "Send the DMX looks of the scenes on IO4"`

### Task 8: The show and the live controls over the web

**Files:**
- Modify: `esp32/src/webui.rs`

**Interfaces:**
- Consumes: `ShowStore`, `DmxLive`, `DmxStatus` (Tasks 6, 7).
- Produces:
  - `GET /show`: the stored JSON, `Content-Type: application/json`, `Access-Control-Allow-Origin: *`. An empty store gives the JSON of `Show::default()`.
  - `POST /show`: refuses a failed `origin_allowed` check with 403, and a body over 60 000 bytes with 413. Parses the body with `serde_json::from_reader` into `Show`, never into a `Value`. Calls `sanitize`, `ShowStore::save`, then replaces `State::show`. Replies 204. A parse error replies 400 with the serde message.
  - WebSocket commands: `{"type":"dmx_live","master":0..255,"strobe":bool}` sets `DmxLive`; `{"type":"dmx_fog"}` queues `DmxMsg::Fog`. Both reply with no message.
  - The hello caps get `"dmx"`. The status gets `"dmx": DmxStatus`.

- [ ] **Step 1: Implement** the two routes, the two commands, the cap and the status field.
- [ ] **Step 2: Build and flash** as in Task 7.
- [ ] **Step 3: Check the routes.** Run: `curl -s http://autoblink.local/show` → `{"fixtures":[],...}`. Run: `curl -s -X POST -H 'Origin: http://evil.example' --data '{}' -o /dev/null -w '%{http_code}' http://autoblink.local/show` → `403`. Run the same POST without the `Origin` header and with a one-fixture show → `204`, and a new `GET` returns it. Restart the box; the `GET` still returns it.
- [ ] **Step 4: Commit.** `git commit -m "Serve the DMX show and the live controls"`

### Task 9: The Lights panel

**Files:**
- Modify: `webui/index.html`

**Interfaces:**
- Consumes: `GET /show`, `POST /show`, `dmx_live`, `dmx_fog`, `scene_test`, status `dmx` (Task 8); the scene palette functions `sceneChip` and `renderScenes`.
- Produces: a panel `<details id="p-lights" class="panel wide" data-cap="dmx">` after `p-scenes`, with these parts, top to bottom:
  1. Live strip: master slider (sends `dmx_live`), "Strobe" button that holds while pressed (`pointerdown` → `strobe: true`, `pointerup` and `pointercancel` → `false`), "Fog" button (`dmx_fog`), and the names of the layers from `status.dmx.layers`.
  2. Patch: one line for each fixture: name, type, address. Add and remove buttons. The types come from `const FIXTURE_TYPES = { dimmer: ['dimmer'], rgb: ['red','green','blue'], rgb_dimmer: ['dimmer','red','green','blue'], rgbw: ['red','green','blue','white'], rgbw_dimmer: ['dimmer','red','green','blue','white'], strobe: ['dimmer','strobe'], fog: ['fog'] }`. The page shows the next free address as the default, and marks an overlap in red.
  3. Groups: name, and one toggle for each fixture.
  4. Look editor: a select of the presets 1 to `scenes.count`, with their labels. For the selected preset: fade (cut, 1 beat, 1 bar); one row for each group with a colour (8 swatches and an `<input type="color">`), a white slider for groups with a white channel, intensity (0 to 255), effect (none, flash, pulse, chase, random, strobe, fog), "every" (beat, ½ beat, bar) for the first four, rate and beats for strobe, ms for fog, cooldown in seconds for strobe and fog. A "Clear look" button sets `looks[i]` to `null`.
  5. Idle look: a select of the presets that have a look, or "none".
- The palette of the DMX scenes marks a preset that has a look with the class `look` (a small coloured corner in the colour of its first row).
- The page loads the show with `fetch(BASE + '/show')` after a hello with the `dmx` cap. Each edit saves the full show with `POST` 1 s after the last change. A failed save shows a toast with the status text.
- Each label and `title` of the panel is plain STE text, like the scene panel.

- [ ] **Step 1: Implement** the panel. Work on it without a flash: `python3 -m http.server 8000 -d webui`, then open `http://localhost:8000/index.html?box=autoblink.local` (see AGENTS.md "Web page without a flash").
- [ ] **Step 2: Check** in the browser: add an RGB par at 1 and a fog machine at 20, make the groups, give preset 1 a red look and preset 2 a blue flash look, drop them on the Drop and Beat rows of the scenes. Play a track with phrases. Expected: a DMX monitor shows red at the Chorus and blue flashes on the beat; the layers in the live strip follow.
- [ ] **Step 3: Screenshot** the panel to `.context/lights-panel.png`.
- [ ] **Step 4: Commit.** `git commit -m "Add the Lights panel for the DMX looks"`

### Task 10: Version, docs and the box test

**Files:**
- Modify: `Cargo.toml` and `esp32/Cargo.toml` (`1.24.0`), `AGENTS.md`

- [ ] **Step 1: Bump** both versions to `1.24.0`. Run `cargo build` so that `Cargo.lock` and `esp32/Cargo.lock` follow.
- [ ] **Step 2: Update AGENTS.md:** the crate table (`crates/autoblink-core-dmx`), the `dmx` thread (priority 6, core 1, measured stack), the `show` partition and its format, the need for one USB flash, `GET /show` and `POST /show`, the removal of the beat flash on channel 1, and the `dmx` default feature. Run `sh ~/.claude/templates/ste-check.sh AGENTS.md`.
- [ ] **Step 3: Run** `cargo test --locked`. Expected: PASS. Build the firmware. Expected: the image is below 0x1E0000.
- [ ] **Step 4: Box test** (spec Verification 3, 4, 7 and 8): flash over USB; play a track with phrases → looks change at the phrase starts and the flash looks are on the beat; stop the deck and pull the Ethernet cable → the idle look after 2 s; restart the box → the idle look shows before the network starts; `curl http://autoblink.local/mem` with the page open → each task keeps its margin.
- [ ] **Step 5: Commit.** `git commit -m "Release the DMX looks as 1.24.0"`
