# DMX Planner 3: Positions and Shapes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Moving heads and scanners point to named positions, and a look moves them in beat-synced shapes: circle, figure 8 and sweep.

**Architecture:** The show gets pan and tilt channel kinds (coarse and fine) and a list of named positions. A position holds the 16-bit pan and tilt of each head. A look row names a position and a shape. The renderer writes pan and tilt from the position plus the shape offset at the beat phase. The page aims a head live: the firmware holds an aim override for a short time while the owner moves the sliders.

**Tech Stack:** Rust 2021 (`autoblink-core-dmx`), ESP-IDF firmware, vanilla JS.

**Spec:** `docs/superpowers/specs/2026-10-27-dmx-planner-design.md` (Positions; Effect layer: shapes "move around the position of the row. Its period is in beats or bars.").

## Global Constraints

- No f32 in a serde type that the firmware reads. Shape math uses f64 locals only.
- `frame` allocates nothing on the heap.
- New limits: 8 positions (`MAX_POSITIONS`), position names 16 characters, shape period 1 to 32 beats, shape size 0 to 255 (255 is a quarter of the full pan and tilt range).
- Old shows (without positions, pan/tilt kinds or shapes) load unchanged; new fields use `#[serde(default)]` and skip serialization when empty or default.
- The firmware keeps its rules (FreeRTOS queue to the `dmx` thread, `send_back(.., 0)`, no blocking).
- Commits carry no attribution lines. Comments, log lines and docs follow ASD-STE100.
- Bump the version to `1.27.0` in the last task.

## Review Focus

1. A head that is in no position: pan and tilt sit at the centre (32768), not at 0. Test: Task 2.
2. A shape near the edge of the range: the value clamps to 0..65535 and never wraps. Test: Task 2.
3. A light with only a coarse pan channel: the renderer writes the high byte only. Test: Task 2.
4. An aim override that the page never ends: it ends by itself after `AIM_MS`. Test: Task 2.
5. Old OFL lights that were added with `{"fixed":…}` for pan and tilt keep working; a new add maps pan and tilt to the new kinds. Test: Task 4 Node check.

---

### Task 1: The show model for pan, tilt, positions and shapes

**Files:** Modify `crates/autoblink-core-dmx/src/show.rs` (and the struct literals in `render.rs` and `lights.rs` tests).

**Interfaces:**

```rust
pub const MAX_POSITIONS: usize = 8;
// Channel gets: Pan, PanFine, Tilt, TiltFine   (serde: "pan", "pan_fine", "tilt", "tilt_fine")
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aim { pub fixture: u8, pub pan: u16, pub tilt: u16 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position { pub name: String, #[serde(deserialize_with = bounded MAX_FIXTURES)] pub aims: Vec<Aim> }
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape { #[default] None, Circle { beats: u8, size: u8 }, Figure8 { beats: u8, size: u8 }, Sweep { beats: u8, size: u8 } }
// LookRow gets: #[serde(default, skip_serializing_if = "Option::is_none")] pub position: Option<u8>,
//               #[serde(default, skip_serializing_if = "is_none_shape")] pub shape: Shape
// Show gets:    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = bounded MAX_POSITIONS)] pub positions: Vec<Position>
```

`sanitize`: cut positions to 8 and names to 16 characters; drop an aim whose fixture does not exist; keep one aim for each fixture (the first); set `position` to `None` when it points past the list; clamp `beats` to 1..=32.

- [ ] **Step 1:** Write the tests `old_show_loads_without_positions` (the plan-1 JSON shape round-trips byte-identical), `positions_sanitize` (10 positions → 8; an aim for fixture 40 is dropped; a duplicate aim is dropped; `position: Some(9)` → `None`; `beats: 0` → 1 and `99` → 32), `pan_tilt_kinds_json` (`"pan_fine"` round-trips).
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --locked`, fmt, clippy, firmware build. Expected: PASS.
- [ ] **Step 5:** Commit "Add pan, tilt, positions and shapes to the DMX show".

### Task 2: Render pan and tilt, shapes and the aim override

**Files:** Modify `crates/autoblink-core-dmx/src/effect.rs` (shape math), `render.rs` (pan and tilt), `lights.rs` (aim override).

**Interfaces:**

```rust
// effect.rs
/// The pan and tilt offset of a shape for fixture k of n, in 16-bit units.
pub fn shape_offset(shape: &Shape, beat: Option<&Beat>, k: usize, n: usize) -> (i32, i32);
// render.rs — render gets the beat for the shapes:
pub fn render(show: &Show, look: &Look, beat: Option<&Beat>, level: impl Fn(usize, usize, usize) -> u8, out: &mut Universe);
// lights.rs
pub const AIM_MS: u64 = 10_000;
impl Lights { pub fn aim(&mut self, fixture: u8, pan: u16, tilt: u16, now_ms: u64); }
```

The rules:
- Base pan and tilt of a fixture in a row: the aim of that fixture in `show.positions[row.position]`, else 32768 / 32768.
- Shape offset with `t` = the phase over `beats` beats (use `count % beats` plus `phase`), `A = size × 64` (255 → 16320, a quarter of 65535), and a per-fixture phase shift of `k / n` of a cycle so that heads in a group do not move in unison: circle (A·cos 2πt, A·sin 2πt); figure 8 (A·sin 2πt, A·sin 4πt / 2); sweep (A·sin 2πt, 0). Without a beat, the offset is (0, 0).
- Value = clamp(base + offset, 0, 65535). `Pan`/`Tilt` get the high byte, `PanFine`/`TiltFine` the low byte.
- Layer blending in `Lights::frame` stays per channel. A fade between two positions blends the coarse and fine bytes separately — accept it, with a `ponytail:` comment that names the glitch and the upgrade (blend the 16-bit pairs).
- `aim`: until `now + AIM_MS`, the pan and tilt channels of that fixture show the aim value over every layer (and over idle). A new `aim` of the same fixture replaces it. The override ends by itself.

- [ ] **Step 1:** Write the tests: `head_without_position_is_centred` (pan 0x80, fine 0x00), `position_sets_pan_tilt` (aim 0x1234 / 0xABCD → coarse/fine bytes), `coarse_only_head` (a fixture with only `Pan` gets 0x12), `circle_at_quarter_phase` (size 255, beats 4, phase at count 1 → offset (0, +16320) ±1 for k = 0), `shape_clamps` (aim 65000 + offset → 65535, no wrap), `heads_are_phase_shifted` (k = 0 and k = 1 of 2 differ), `aim_override_and_timeout` (aim shows at +9 999 ms and ends at +10 001 ms).
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement and update the callers of `render`.
- [ ] **Step 4:** `cargo test --locked`, fmt, clippy, firmware build. Expected: PASS.
- [ ] **Step 5:** Commit "Move heads to positions and in beat shapes".

### Task 3: The firmware takes aim commands

**Files:** Modify `esp32/src/dmx.rs` (`DmxMsg::Aim { fixture: u8, pan: u16, tilt: u16 }` → `lights.aim`), `esp32/src/webui.rs` (WebSocket command `{"type":"dmx_aim","fixture":n,"pan":0..65535,"tilt":0..65535}` → `send_back(.., 0)`, no reply; clamp and validate).

- [ ] **Step 1:** Implement.
- [ ] **Step 2:** Build both firmware configurations; rustfmt.
- [ ] **Step 3:** Commit "Aim a head live from the page".

### Task 4: Positions and shapes on the page

**Files:** Modify `webui/index.html`.

- OFL mapping (`oflChannels`): `Pan` → `"pan"`, `Tilt` → `"tilt"`; a fine alias of a Pan channel → `"pan_fine"`, of a Tilt channel → `"tilt_fine"`. Other fine aliases stay `{"fixed":…}`. Node check with the moving-head sample in `.context/ofl-samples/`.
- Generic type list gets `moving_head: ['pan', 'tilt', 'dimmer', 'red', 'green', 'blue']` and `moving_head_16: ['pan', 'pan_fine', 'tilt', 'tilt_fine', 'dimmer', 'red', 'green', 'blue']`.
- Lights setup gets a "Positions" section: position cards (name, + position, remove). In a card, one row for each head (a light with a pan or tilt channel) with two sliders (pan, tilt, 0..65535) and a "Center" button. Moving a slider sends `dmx_aim` (throttled to 20 per second) and stores the aim; the aim saves through the debounced POST.
- Look editor card of a group with heads: a position pill list (none + the names) and shape pills (none, circle, figure 8, sweep) with beats (1, 2, 4, 8, 16) and size (slider).
- Check against a stub (console clean), screenshots to `.context/positions-*.png`, firmware build.
- Commit "Set positions and shapes for moving heads on the page".

### Task 5: Version, docs and checks

- Bump to 1.27.0 with the lock files; AGENTS.md "DMX show": pan/tilt kinds, positions, shapes (formulas in one line each), the aim override and `AIM_MS`, the coarse/fine fade glitch; run `sh scripts/check.sh --esp32`; commit "Release positions and shapes as 1.27.0".
- Box session (deferred while the box is offline): one moving head, aim two positions, a look with a circle every 4 beats; check the movement on the beat and the 10 s aim timeout.
