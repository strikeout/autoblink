# DMX Planner 4: Drop Guess — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A track without phrase analysis still changes the scenes. The engine guesses Intro, Groove, Build, Drop, Break and Outro from the waveform energy and fires the bucket rows on 16-beat block starts.

**Architecture:** A new module `guess.rs` in `autoblink-core-midi` turns the PWV4 colour preview of a track into a smoothed energy profile and classifies a playhead position into a phrase bucket. `SceneInput` gets the guessed bucket and the beat in the 16-beat block. The scene engine fires a bucket row at a block start when the guess changes and the track has no phrases. The firmware and the host both feed the guess.

**Tech Stack:** Rust 2021 (`autoblink-core-midi`), ESP-IDF firmware, the host (tokio, axum).

**Spec:** `docs/superpowers/specs/2026-10-27-dmx-planner-design.md` (Triggers and gates, rule 2: "If the track has no phrase analysis, the planner finds the drops from the waveform energy. It sends the bucket events Groove, Build, Drop and Break on 16-beat blocks.").

## Facts

- The PWV4 preview has 6 bytes for each column. The firmware keeps 600 columns (`autoblink_core_dbserver::PREVIEW_COLUMNS`); the host keeps the player's columns (usually 1200). The guess uses the sum of the 6 bytes of a column as its energy. The builder may refine this after looking at real data (left to the builder).
- The firmware has the playhead (`Deck::playhead_ms`) and the track length (`Deck::track_len_ms`) from the absolute position packets, and `phrase16` (1 to 16, 0 unknown) from the beat counting. The host has `playhead_ms` on the device and the track length from the metadata.
- Without phrases, the firmware does not request the beat grid. The guess therefore uses `phrase16` for the block starts.

## Global Constraints

- The guess never overrides real phrases: with `SceneInput::phrase` set, the engine ignores the guess.
- The guess fires a bucket row only at a block start (`phrase16 == 1` after a value other than 1) and only when the guessed bucket differs from the last bucket that the guess fired for this track. A track change or a master change resets it.
- The profile has at most 600 values (`u8`), so the firmware keeps about 600 bytes for each deck. No allocation at playback time.
- No f32 in serde types; the profile is not serialized.
- The page shows a guessed bucket with "(guess)" so that the owner can tell it from rekordbox phrases.
- Commits carry no attribution lines. Comments and docs follow ASD-STE100.
- Bump the version to `1.28.0` in the last task.

## Review Focus

1. A track with a flat energy (ambient, a talk): no bucket flicker — the guess holds Groove, and the hysteresis keeps it. Test: Task 1.
2. A preview of fewer than 32 columns, or none: no guess (`None`), no panic. Test: Task 1.
3. Loops and cue jumps: the guess follows the playhead, and it fires only at block starts. Test: Task 2.
4. A track with phrases on a deck that becomes master: the real phrases win at once. Test: Task 2.
5. The firmware heap after 20 track loads: no growth. Box session.

---

### Task 1: The energy profile and the bucket classifier

**Files:** Create `crates/autoblink-core-midi/src/guess.rs`; export it from `lib.rs`.

**Interfaces:**

```rust
pub const PROFILE_LEN: usize = 600;
#[derive(Debug, Clone, PartialEq)]
pub struct DropGuess { /* profile: Vec<u8> of at most PROFILE_LEN, plus thresholds */ }
impl DropGuess {
    /// Builds the profile from PWV4 preview bytes (6 bytes for each column). None below 32 columns.
    pub fn from_preview(preview: &[u8]) -> Option<DropGuess>;
    /// The bucket at the position `frac` (0.0 to 1.0) of the track. `prev` is the bucket that the
    /// guess gave last; the classifier keeps it while the energy stays inside a hysteresis band.
    pub fn bucket_at(&self, frac: f64, prev: Option<Event>) -> Event;
}
```

The classifier (the builder may tune the numbers; keep the structure):
- Energy of a column = the sum of its 6 bytes, scaled to 0..255; fold the columns down to at most 600; smooth with a moving average over ±4 columns.
- Thresholds from the track's own distribution: `hi` = the 70th percentile, `lo` = the 30th percentile, `mid` = the median.
- Intro: before the first column with energy ≥ `mid`. Outro: after the last column with energy ≥ `mid`.
- Drop: energy ≥ `hi`. Break: energy ≤ `lo`. Build: energy < `hi`, and the maximum over the next 2 % of the track is ≥ `hi`. Groove: the rest.
- Hysteresis: keep `prev` (Drop or Break) while the energy stays within 8 of the threshold that started it.

- [ ] **Step 1:** Write the tests with synthetic previews (6 equal bytes for each column): `flat_track_is_groove` (all 120 → Groove everywhere after the intro rule), `drop_and_break` (a low intro, a high middle, a low dip, a high end, a low tail → Intro, Drop, Break, Drop, Outro at the expected fractions), `build_before_drop` (a rising ramp into a high part → Build in the last 2 % before it), `hysteresis_holds_drop` (a small dip below `hi` keeps Drop), `short_preview_is_none` (31 columns → None), `folds_1200_to_600`.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --locked -p autoblink-core-midi`, fmt, clippy. Expected: PASS.
- [ ] **Step 5:** Commit "Guess the phrase buckets from the waveform energy".

### Task 2: The scene engine fires guessed buckets on block starts

**Files:** Modify `crates/autoblink-core-midi/src/scenes.rs`.

**Interfaces:**
- `SceneInput` gets `pub guess: Option<Event>` (the guessed bucket now) and `pub phrase16: u8` (1 to 16, 0 unknown). Both default to `None` / 0.
- `SceneStatus` gets `pub guessed: bool` — true when `phrase` (the bucket that the status shows) comes from the guess.
- The rules in Global Constraints. The guess uses the same `event()` path as a real bucket (counts, fires, `drain_fired` reports it, `current` changes).

- [ ] **Step 1:** Write the tests: `guess_fires_on_block_start` (no phrase, guess Drop, phrase16 goes 16 → 1 → fires Drop once), `guess_waits_for_block_start` (guess changes at phrase16 7 → nothing until the next 1), `same_guess_does_not_refire` (Drop at two block starts → one fire), `phrases_win` (a phrase present → the guess is ignored), `track_change_resets_guess` (the same guess fires again after a new track).
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement, and fill the new fields at the existing callers with the defaults (host `scene_input`, firmware `midi.rs`) so that everything builds.
- [ ] **Step 4:** `cargo test --locked`, fmt, clippy, firmware build. Expected: PASS.
- [ ] **Step 5:** Commit "Fire guessed buckets on 16-beat block starts".

### Task 3: The firmware and the host feed the guess

**Files:** Modify `esp32/src/tracks.rs` (build a `DropGuess` when the overview arrives; keep it in `TrackData`), `esp32/src/midi.rs` (fill `guess` from the master's playhead / track length and `phrase16` from the tracker), the host: the device or song-structure state that holds the preview (build and keep a `DropGuess` next to it), `host/src/midi/mapper.rs` `scene_input` (fill `guess` and `phrase16`), and `webui/index.html` (show "(guess)" next to the phrase bucket of the scenes panel when `scene.guessed`; also in the master panel bucket label if it shows the scene bucket).

- [ ] **Step 1:** Implement the firmware side. Build both configurations; rustfmt.
- [ ] **Step 2:** Implement the host side. `cargo test --locked`, fmt, clippy.
- [ ] **Step 3:** Page label; check with a stub; screenshot `.context/guess-*.png`.
- [ ] **Step 4:** Commit "Feed the drop guess on the box and on the host".

### Task 4: Version, docs and checks

- Bump to 1.28.0 with the lock files; AGENTS.md (DMX scenes): the guess, its block-start rule, "(guess)" on the page, the thresholds in one line; run `sh scripts/check.sh --esp32`; commit "Release the drop guess as 1.28.0".
- Box session (deferred while the box is offline): play a track without phrase analysis; the bucket rows change at block starts near the audible drops; `/mem` after 20 track loads.
