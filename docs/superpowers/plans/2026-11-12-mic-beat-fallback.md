# Mic Beat Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A PDM mic on the box gives the beat to the MIDI clock and the DMX when no Pro DJ Link deck plays, and Pro DJ Link calibrates the mic while it plays.

**Architecture:** A new host-testable crate `autoblink-core-beat` holds the tracker (onsets, tempo, phase, bar 1, flywheel, Break and Drop), the sample clock, the beat emitter, the calibrator and the arbiter. The firmware module `esp32/src/mic.rs` reads I2S0 in PDM mode on IO2 and IO12 in a `mic` thread. The thread runs the crate, writes a `MicView` into `State`, and sends the mic beats into the same fan-out as the Pro DJ Link beats while the mic is the master. The MIDI clock in `autoblink-core-midi` gets a handover without Stop and Start.

**Tech Stack:** Rust 2021, no new external crate for the DSP (`f32` math, IIR biquads). ESP-IDF 5.5.3 with esp-idf-hal 0.47 (`I2sDriver::new_pdm_rx`). Vanilla JS in `webui/index.html`.

**Spec:** `docs/superpowers/specs/2026-11-12-mic-beat-fallback-design.md`

## Global Constraints

- Pro DJ Link always wins. The mic takes over only from `Locked`. The flywheel only continues a mic master that already took over.
- The mic master uses the setting "Device Number" (`Settings::device_number`, default 7). It never borrows the number of a player. It is never 0, also while the box is not on a network.
- A handover between a deck and the mic does not send MIDI Stop or Start. A change from one deck to another deck keeps the Stop and the Start of today.
- PCM: 16 kHz, 16 bit, mono. Tempo range 80 to 160 BPM, with octave folding. A 4/4 prior favours 120 to 130 BPM.
- `NoMic` after 5 s of constant PCM. Silence: below -50 dBFS for 10 s. Break: no kick for 8 beats. Log a flywheel longer than 5 minutes.
- Default offset 30 ms. NVS key `mic` keeps the offset. Write at most every 5 minutes, and only after a change of more than 2 ms.
- Setting 119 "Mic": a toggle, default on (Auto). Off stops the I2S channel.
- `GET /mic?s=<1..=120>`: 403 unless the active page sent `mic_arm` in the last 60 s. One capture for each arm.
- `mic` thread: core 0, priority 4, `crate::spawn_rt`. DMA buffer of at least 96 ms. Heap at most 12 KB without a capture. CPU at most 10 % of core 0. Flash at most 40 KB more.
- The `mic` thread must not send on a `std::sync::mpsc` channel. The receiver `web-status` has a higher priority (AGENTS.md, "Do not use `std::sync::mpsc`").
- No `f32` field in a struct that serde reads on the firmware. The new crate has no serde.
- Status messages are `Serialize` structs, not `serde_json::Value` trees.
- Comments, log lines, error strings and docs follow ASD-STE100. Commits carry no attribution lines.
- The version goes to `1.40.0` in the last task (both `Cargo.toml` files and both lock files).

## Review Focus

1. A box without the module: IO12 reads low, the PCM is constant, and noise never gives a beat or a takeover. Test: Task 2 `dc_and_zero_give_no_mic`; Task 4 `no_mic_never_takes_over`.
2. A mic next to a loud speaker clips hard. The tracker must still lock. Test: Task 2 `a_clipped_input_still_locks`.
3. A hard cut to a track with another tempo (124 to 130 BPM) during a mic set. The tracker must lock again within 8 bars, and the emitter must not send a burst. Test: Task 2 `a_hard_cut_to_a_new_tempo_relocks`; Task 4 `a_gap_emits_no_burst`.
4. Pro DJ Link comes back while the mic is the master. No beat may go out twice in that window. Test: Task 4 `a_playing_deck_wins_at_once`; Task 10 step "emit only while no deck plays".
5. A `/mic` request while the heap is low. The route answers 503 and the box does not restart. Test: Task 11 box check with `/mem`.

---

### Task 1: Crate scaffold, test signals and onsets

**Files:**
- Create: `crates/autoblink-core-beat/Cargo.toml`, `src/lib.rs`, `src/onset.rs`, `src/synth.rs`
- Modify: `Cargo.toml` (root, add the member)

**Interfaces:**
- Produces in `lib.rs`: `pub const SAMPLE_RATE: u32 = 16_000;`, `pub const HOP: usize = 128;` (8 ms), `pub const BPM_MIN: f64 = 80.0;`, `pub const BPM_MAX: f64 = 160.0;`, `pub const NO_MIC_MS: u64 = 5_000;`, `pub const SILENCE_MS: u64 = 10_000;`, `pub const SILENCE_DBFS: f32 = -50.0;`, `pub const BREAK_BEATS: u32 = 8;`, `pub const LONG_FLYWHEEL_MS: u64 = 300_000;`, `pub const DEFAULT_OFFSET_US: i64 = 30_000;`
- Produces in `onset.rs`: `pub struct Onsets`, `Onsets::new() -> Self`, `Onsets::push(&mut self, samples: &[i16], out: &mut impl FnMut(Frame))`. It calls `out` once for each full hop of `HOP` samples and keeps a partial hop for the next call.
- Produces: `#[derive(Clone, Copy, Debug, Default)] pub struct Frame { pub kick: f32, pub clap: f32, pub full: f32, pub dbfs: f32, pub span: u16 }`. `kick`, `clap` and `full` are the half-wave-rectified rise of the log energy of the band. `span` is max minus min of the hop.
- Produces in `synth.rs` (`#[cfg(test)] pub(crate) mod synth`): `pub struct Track { pub bpm: f64, pub clap_on_2_4: bool, pub bar_change: bool, pub noise_db: Option<f64>, pub breakdown: Option<(u32, u32)> /* first beat, beats */, pub flam_ms: Option<f64>, pub clip: bool }` with `Default` (124 BPM, clap and bar change on, no noise). `pub fn render(t: &Track, seconds: f64) -> Vec<i16>`. `pub fn beat_times(t: &Track, seconds: f64) -> Vec<(f64, u8)>` gives (seconds, beat_in_bar) of the true beats. The first sample is a downbeat.

- [ ] **Step 1: Add the crate.** `Cargo.toml` copies the workspace fields of `crates/autoblink-core-midi/Cargo.toml`. It has no dependencies. Add `"crates/autoblink-core-beat"` to the root `members`.

- [ ] **Step 2: Write `synth.rs`.** A kick is a 55 Hz sine that falls to 45 Hz, with an exponential decay of 120 ms. A clap is band-passed noise around 2 kHz for 60 ms. A bar change is a 70 Hz bass note that changes its pitch on beat 1 of each bar, plus a 6 kHz noise burst of 300 ms. The noise comes from a fixed LCG seed, so that each run is the same. `flam_ms` adds a second kick at -6 dB that much after each kick. `clip` multiplies by 8 and saturates at `i16`. A breakdown removes the kick and the clap for its beats.

- [ ] **Step 3: Write the failing tests in `onset.rs`.**

```rust
#[test]
fn the_kick_band_peaks_on_each_kick() {
    // 124 BPM, no clap. Each true beat has a kick peak within 2 hops (16 ms).
}
#[test]
fn the_clap_band_peaks_on_2_and_4() {
    // The clap band at beats 2 and 4 is at least 4 times its value at beats 1 and 3.
}
#[test]
fn silence_gives_a_low_dbfs_and_zero_span() {
    // 1 s of zeros: each frame has dbfs < -90.0 and span == 0.
}
```

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-beat onset`. Expected: FAIL, `Onsets` is missing.

- [ ] **Step 5: Implement `Onsets`.** Use second-order biquads at 16 kHz: kick low-pass 150 Hz (two stages), clap band-pass 1 to 4 kHz, full high-pass 30 Hz. For each hop, take the mean square of each band, then `ln(1 + e / 1e3)`, then the rise from the last hop, clamped at 0.

- [ ] **Step 6: Run them.** Run: `cargo test -p autoblink-core-beat onset`. Expected: PASS.

- [ ] **Step 7: Commit.** `git add Cargo.toml Cargo.lock crates/autoblink-core-beat && git commit -m "Add the beat crate with test signals and band onsets"`

### Task 2: Tracker tempo, phase and lock

**Files:**
- Create: `crates/autoblink-core-beat/src/tracker.rs`

**Interfaces:**
- Consumes: `Onsets`, `Frame`, the constants of Task 1.
- Produces:
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub enum MicState { #[default] Listening, NoMic, Locked, Flywheel }` with `pub fn label(self) -> &'static str` ("listening", "no_mic", "locked", "flywheel").
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Section { Break, Drop }`
  - `#[derive(Clone, Copy, Debug, PartialEq)] pub struct SampleGrid { pub beat: f64, pub period: f64, pub bar_pos: u8, pub phrase_pos: u8 }`. `beat` is the sample index of one beat of the grid, `period` is samples for each beat. `bar_pos` is 0 to 3 for that beat, 0 is bar 1. `phrase_pos` is 0 to 15.
  - `pub struct Tracker` with `new() -> Self`, `push(&mut self, samples: &[i16])`, `samples(&self) -> u64`, `state(&self) -> MicState`, `grid(&self) -> Option<SampleGrid>` (`Some` in `Locked` and `Flywheel`), `bpm(&self) -> Option<f64>`, `section(&self) -> Option<Section>`, `confidence(&self) -> u8` (0 to 100), `level(&self) -> u8` (0 to 100 from -90 to 0 dBFS), `bar_confident(&self) -> bool`, `flywheel_ms(&self) -> u64` (0 outside `Flywheel`), `cold_lock_ms(&self) -> Option<u32>` (the time from `Listening` to `Locked`, last value).
- Test helper in `tracker.rs`: `fn run(t: &Track, s: f64) -> (Tracker, Vec<(f64, MicState)>)` pushes the PCM in blocks of 256 samples and logs each state change with its time.
- Test helper: `fn phase_err_ms(tr: &Tracker, t: &Track, s: f64) -> f64`, the largest distance of the last 8 true beats from the nearest grid beat.

- [ ] **Step 1: Write the failing tests.**

```rust
#[test] fn locks_within_4_bars() // Track::default(), 20 s: Locked before 7.75 s (16 beats).
#[test] fn the_tempo_is_within_a_tenth() // after 30 s: |bpm - 124.0| < 0.1.
#[test] fn the_phase_is_within_5_ms() // after 30 s, no noise: phase_err_ms < 5.0.
#[test] fn noise_keeps_the_phase_within_10_ms() // noise_db: Some(-10.0): phase_err_ms < 10.0.
#[test] fn follows_a_tempo_ramp() // 124 to 126 over 32 bars: phase_err_ms < 15.0 at each bar.
#[test] fn a_flam_does_not_move_the_phase() // flam_ms: Some(30.0) for 8 beats: the grid moves < 5 ms.
#[test] fn drum_and_bass_folds_to_half() // 174 BPM: bpm in 86.0..=88.0.
#[test] fn dc_and_zero_give_no_mic() // 6 s of 0, then 6 s of 1000: NoMic at 5 s and at 11 s.
#[test] fn silence_after_music_goes_to_listening() // 20 s music, 11 s zeros at -100 dBFS: Listening, not Flywheel.
#[test] fn a_clipped_input_still_locks() // clip: true: Locked before 8 s, phase_err_ms < 10.0.
#[test] fn a_hard_cut_to_a_new_tempo_relocks() // 30 s at 124, then 30 s at 130 with a random phase: Locked at 130 within 8 bars of the cut.
```

- [ ] **Step 2: Run them.** Run: `cargo test -p autoblink-core-beat tracker`. Expected: FAIL, `Tracker` is missing.

- [ ] **Step 3: Implement the tempo, phase and lock in `Tracker`.**
  - Keep the last 4 s of `kick + 0.5 * full` (500 frames at 125 frames each second) in a ring.
  - Every 0.5 s, compute the autocorrelation for the lags of 80 to 160 BPM. Add the values at 2 and 4 times the lag. Multiply by a log-normal prior around 125 BPM, with a width of one octave. Interpolate the peak with a parabola. The ratio of the peak to the mean gives `confidence`.
  - Run a phase lock on the predicted beat. In a window of plus or minus 1/8 of a period, take the strongest kick onset. Move the phase by 0.1 of the error and the period by 0.01 of the error. Clamp one correction to 10 ms. Thus a flam moves the grid only when it stays.
  - When the autocorrelation tempo and the period of the lock differ by more than 2 % on 2 estimates, seed the lock again from the autocorrelation.
  - `Locked`: 6 of the last 8 predicted beats had a kick in the window, and `confidence` is at least the lock threshold. Choose the threshold from the tests.
  - Without a kick near the grid for 4 beats in `Locked` while kicks come, go to `Listening`. This covers the hard cut.
  - `NoMic`: `span` < 4 on each frame for `NO_MIC_MS`. Silence: `dbfs` < `SILENCE_DBFS` for `SILENCE_MS` gives `Listening`.

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-beat tracker`. Expected: PASS.

- [ ] **Step 5: Commit.** `git add crates/autoblink-core-beat && git commit -m "Track the tempo and the phase of the kick"`

### Task 3: Bar 1, flywheel, Break and Drop

**Files:**
- Modify: `crates/autoblink-core-beat/src/tracker.rs`

**Interfaces:**
- Consumes: Task 2.
- Produces: the same API. `SampleGrid::bar_pos`, `phrase_pos`, `section()`, `bar_confident()` and `flywheel_ms()` get their values.

- [ ] **Step 1: Write the failing tests.**

```rust
#[test] fn finds_bar_1_within_8_bars() // Track::default(): after 16 bars, the grid beat at a true downbeat has bar_pos == 0.
#[test] fn without_a_clap_the_bar_change_decides() // clap_on_2_4: false: bar_pos == 0 at a downbeat after 16 bars.
#[test] fn a_breakdown_runs_the_flywheel() // breakdown: Some((64, 64)): Flywheel by beat 72, section == Some(Break), phase_err_ms < 10.0 at beat 127.
#[test] fn the_kick_return_is_a_drop() // same track: Locked and section == Some(Drop) by beat 130; bar_pos == 0 and phrase_pos == 0 at beat 128.
#[test] fn the_flywheel_counts_its_time() // a breakdown of 30 s: flywheel_ms() >= 29_000 at its end.
#[test] fn the_bar_stays_without_confidence() // a track without clap and without bar change: bar_pos does not change after the lock.
```

- [ ] **Step 2: Run them.** Run: `cargo test -p autoblink-core-beat tracker`. Expected: the new tests FAIL.

- [ ] **Step 3: Implement.**
  - At each grid beat, add the clap onset and the full onset of a window of plus or minus 1/8 period into 4 accumulators for the 4 bar positions. Let them decay with a time constant of 16 bars.
  - Bar 1 is the position with the most full change among the two positions with less clap. Without a clap difference, use the full change alone.
  - Change `bar_pos` only when the new position wins over 8 bars in a row by a margin. Choose the margin from the tests. `bar_confident()` is true while it wins.
  - `phrase_pos` counts the beats mod 16 from bar 1. When `bar_pos` changes, align `phrase_pos` so that `phrase_pos % 4 == bar_pos`.
  - `Locked` to `Flywheel` after `BREAK_BEATS` predicted beats without a kick, and `section` becomes `Break`. The grid does not change in `Flywheel`.
  - `Flywheel` to `Locked` after a kick near 2 predicted beats in a row. `section` becomes `Drop`. The first of the 2 beats gets `bar_pos = 0` and `phrase_pos = 0`.
  - Kicks that are not near the grid for 4 beats in `Flywheel` give `Listening`.

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-beat`. Expected: PASS.

- [ ] **Step 5: Commit.** `git commit -am "Find bar 1 and run the flywheel through a breakdown"`

### Task 4: Grid on the µs axis, emitter and arbiter

**Files:**
- Create: `crates/autoblink-core-beat/src/grid.rs`, `src/arbiter.rs`

**Interfaces:**
- Consumes: `SampleGrid`, `MicState`.
- Produces in `grid.rs`:
  - `#[derive(Clone, Copy, Debug, PartialEq)] pub struct Grid { pub beat_us: f64, pub period_us: f64, pub bar_pos: u8, pub phrase_pos: u8 }` with `bpm(&self) -> f64`, `beat_phase(&self, now_us: f64) -> f64` (0 to 1), `position(&self, at_us: f64) -> (u8, u8)` (beat in bar 1 to 4, beat of the 16-beat block 1 to 16, for the grid beat nearest `at_us`).
  - `pub struct SampleClock` with `new() -> Self`, `observe(&mut self, samples_end: u64, now_us: u64)`, `to_us(&self, sample: f64) -> f64`, `to_sample(&self, us: f64) -> f64`, `grid(&self, g: &SampleGrid) -> Grid`. The offset is `now_us - samples_end * 62.5`. Keep the smallest offset of each 2 s window, and use the smaller of the last full window and the current window. A late wake only makes the offset larger.
  - `pub struct Emitter` with `new() -> Self`, `next(&mut self, g: &Grid, now_us: f64) -> Option<OutBeat>`, `reset(&mut self)`.
  - `#[derive(Clone, Copy, Debug, PartialEq)] pub struct OutBeat { pub at_us: u64, pub beat_in_bar: u8, pub phrase16: u8, pub bpm: f64 }`
- Produces in `arbiter.rs`: `#[derive(Default)] pub struct Arbiter`, `update(&mut self, deck_playing: bool, mic: MicState) -> bool` (true while the mic is the master).

- [ ] **Step 1: Write the failing tests.**

```rust
// grid.rs
#[test] fn late_wakes_do_not_move_the_time() // observe with random late wakes of 0 to 20 ms: to_us moves < 1 ms.
#[test] fn emits_each_beat_once() // a fixed 120 BPM grid, next() every 16 ms for 10 s: 20 beats, in order, at_us on the grid.
#[test] fn a_phase_slew_never_doubles_a_beat() // move beat_us by +200 ms and by -200 ms: no two beats less than period/2 apart.
#[test] fn a_gap_emits_no_burst() // no call for 3 s, then next(): one beat only.
#[test] fn the_position_counts_bars_and_blocks() // bar_pos 2, phrase_pos 6 at beat_us: position(beat_us + 2 periods) == (1, 9).
// arbiter.rs
#[test] fn a_playing_deck_wins_at_once() // mic master, then update(true, Locked) == false.
#[test] fn only_a_lock_takes_over() // update(false, Flywheel) == false while the mic is not the master.
#[test] fn the_flywheel_keeps_a_mic_master() // update(false, Locked), then update(false, Flywheel) == true.
#[test] fn no_mic_never_takes_over() // update(false, NoMic) == false; update(false, Listening) == false.
```

- [ ] **Step 2: Run them.** Run: `cargo test -p autoblink-core-beat`. Expected: the new tests FAIL.

- [ ] **Step 3: Implement.** The emitter returns the earliest grid beat in (last + period/2, now_us]. When more than 2 periods passed since the last beat, it returns only the latest due beat. The arbiter: a playing deck gives false and clears the mic master. `Locked` gives true. `Flywheel` gives true only when the mic is the master already. Each other state gives false.

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-beat`. Expected: PASS.

- [ ] **Step 5: Commit.** `git add crates/autoblink-core-beat && git commit -m "Put the mic grid on the time axis and pick the master"`

### Task 5: Calibrator and accuracy numbers

**Files:**
- Create: `crates/autoblink-core-beat/src/calib.rs`

**Interfaces:**
- Consumes: `Grid`.
- Produces:
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub enum Ratio { #[default] One, Double, Half }`
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub struct Stats { pub beats: u32, pub tempo_err_cbpm: u16, pub phase_med_ms: u16, pub phase_p95_ms: u16, pub bar1_pct: u8, pub lock_ms: Option<u32> }` (`cbpm` is 0.01 BPM).
  - `pub struct Calibrator` with `new(offset_us: i64) -> Self`, `pdl_beat(&mut self, at_us: u64, beat_in_bar: u8, bpm: f64, raw: Option<&Grid>)`, `set_learning(&mut self, on: bool)`, `on_drop(&mut self)`, `map(&self, raw: &Grid) -> Grid`, `offset_us(&self) -> i64`, `rotation(&self) -> u8`, `ratio(&self) -> Ratio`, `note_lock(&mut self, ms: u32)`, `take_minute(&mut self, now_us: u64) -> Option<Stats>` (once each 60 s while beats came), `stats(&self) -> Stats` (the last minute).
- `map` applies the ratio to `period_us`, subtracts the offset from `beat_us`, and rotates `bar_pos` and `phrase_pos` by the rotation. The rotation counts in the beats of the Pro DJ Link grid.

- [ ] **Step 1: Write the failing tests.**

```rust
#[test] fn learns_the_offset_from_the_earliest_packet() // mic grid late by 40 ms; PDL beats late by 0 to 60 ms at random: offset_us within 2 ms of 40_000 after 8 windows.
#[test] fn learns_the_bar_rotation() // mic bar_pos 2 at PDL beat 1, for 8 beats: rotation() == 2, map gives bar_pos 0 there.
#[test] fn learns_a_double_tempo() // PDL 174 BPM, mic 87 BPM: ratio() == Double, map(raw).bpm() within 0.1 of 174.
#[test] fn a_frozen_calibrator_keeps_its_values() // set_learning(false), then PDL beats with another offset: offset_us() does not change.
#[test] fn a_drop_after_the_takeover_resets_the_rotation() // rotation 2, set_learning(false), on_drop(): rotation() == 0.
#[test] fn a_confident_bar_change_after_the_takeover_wins() // set_learning(false), raw bar_pos changes: rotation() == 0.
#[test] fn the_minute_numbers() // 60 s of PDL and mic beats with a 3 ms error: phase_med_ms == 3, bar1_pct == 100, beats == 120.
#[test] fn without_beats_no_minute() // take_minute after 60 s without beats: None.
```

- [ ] **Step 2: Run them.** Run: `cargo test -p autoblink-core-beat calib`. Expected: FAIL.

- [ ] **Step 3: Implement.**
  - Ratio: `pdl_bpm / raw.bpm()` in 0.9 to 1.1 is One, 1.8 to 2.2 is Double, 0.45 to 0.55 is Half. Change the ratio after 8 beats in a row that agree. Skip a beat that fits no ratio.
  - Offset: for each PDL beat, `d` = the nearest mapped mic beat without the offset, minus `at_us`. A window is 32 PDL beats, and it keeps the largest `d` (the earliest packet). The offset is the median of the last 8 windows. The first window sets it at once.
  - Rotation: `(beat_in_bar - 1 - mic_pos) mod 4` at each PDL beat. Change it after 8 beats in a row that agree.
  - While the calibrator does not learn, a change of the raw `bar_pos` that the beat count does not explain sets the rotation to 0.
  - Stats: keep at most 512 phase errors (`u16` ms) for each minute. Compute the median and the 95th percentile at `take_minute`. A bar-1 hit is a PDL beat 1 where the raw `bar_pos` without rotation is 0.

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-beat`. Expected: PASS.

- [ ] **Step 5: Commit.** `git add crates/autoblink-core-beat && git commit -m "Calibrate the mic against Pro DJ Link"`

### Task 6: WAV and the eval example

**Files:**
- Create: `crates/autoblink-core-beat/src/wav.rs`, `crates/autoblink-core-beat/examples/eval.rs`

**Interfaces:**
- Produces: `pub fn header(frames: u32, channels: u16, rate: u32) -> [u8; 44]` (PCM 16 bit). `pub struct Wav { pub rate: u32, pub channels: u16, pub samples: Vec<i16> }` and `pub fn read(bytes: &[u8]) -> Result<Wav, &'static str>`. `read` skips unknown chunks.
- Consumes in `eval.rs`: `Tracker`, `SampleClock`, `Calibrator`. The time axis is `sample * 62.5` µs. A right-channel sample of 1000, 2000, 3000 or 4000 is a PDL beat with that beat in the bar.

- [ ] **Step 1: Write the failing test.**

```rust
#[test] fn a_header_round_trips() // header(16_000, 2, 16_000) + 64 000 bytes: read gives rate 16_000, channels 2, 32_000 samples.
```

- [ ] **Step 2: Run it.** Run: `cargo test -p autoblink-core-beat wav`. Expected: FAIL.

- [ ] **Step 3: Implement `header` and `read`, then `eval.rs`.** `eval` prints one line for each minute: the `Stats` fields, the offset, the rotation, the ratio and the state changes of the tracker.

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-beat && cargo build -p autoblink-core-beat --examples`. Expected: PASS and a build.

- [ ] **Step 5: Commit.** `git add crates/autoblink-core-beat && git commit -m "Add the WAV format and the eval example of the beat crate"`

### Task 7: MIDI clock handover

**Files:**
- Modify: `crates/autoblink-core-midi/src/clock.rs` (struct `Master` at :26, `Clock`, `poll` at :115, `beat` at :172, tests at :297)
- Modify: `esp32/src/midi.rs:309` (the `Master` literal)

**Interfaces:**
- Produces: `Master { pub device: u8, pub playing: bool, pub bpm: f64, pub mic: bool }`. `Clock` gets the private fields `master_mic: bool` and `handover: bool`.

- [ ] **Step 1: Write the failing tests in `clock.rs`.**

```rust
#[test] fn a_handover_to_the_mic_keeps_the_clock_running() // running on device 1, poll with Master { device: 7, mic: true }: no STOP, no START, pulses continue.
#[test] fn a_handover_back_to_a_deck_keeps_the_clock_running() // running on the mic (device 7), poll with device 1, mic false: no STOP, no START.
#[test] fn the_first_beat_after_a_handover_slews_at_once() // phrase_lock_bars 4; the first mic beat is 30 ms late: a slew starts on that beat.
#[test] fn a_deck_change_still_stops() // running on device 1, poll with device 2, mic false: STOP.
```

- [ ] **Step 2: Run them.** Run: `cargo test -p autoblink-core-midi clock`. Expected: FAIL, the field `mic` is missing.

- [ ] **Step 3: Implement.** In `poll`, when the device changes, `m.mic != self.master_mic` and the state is `Running`: set the new master and `handover = true`, and keep running. Otherwise keep the Stop path of today. In `beat`, when `handover` is set, start a slew of the phase error at once (same duration rule), and clear `handover`. Set `mic: false` in each existing `Master` literal.

- [ ] **Step 4: Run them.** Run: `cargo test -p autoblink-core-midi`. Expected: PASS.

- [ ] **Step 5: Commit.** `git commit -am "Keep the MIDI clock running across a handover to the mic"`

### Task 8: Setting 119 and the OLED line

**Files:**
- Modify: `crates/autoblink-core-ui/src/config.rs` (constants at :9, `INDICES` at :22, `BoxConfig`, `items`, `set`)
- Modify: `crates/autoblink-core-ui/src/screen.rs` (`Snapshot` at :12, `lines` at :56)
- Modify: `crates/autoblink-core-ui/src/menu.rs` when it lists the indices

**Interfaces:**
- Produces: `pub const IDX_MIC: usize = 119;`, `BoxConfig::mic: bool` (default `true`), `INDICES: [usize; 12]`. The row: label "Mic", section `Some("Mic")`, kind "toggle". `Snapshot::mic: bool` (true while the mic is the master).

- [ ] **Step 1: Write the failing tests.**

```rust
// config.rs
#[test] fn the_mic_is_on_by_default_and_toggles() // BoxConfig::default().mic; set(119, "0") gives false; set(119, "2") is Err.
#[test] fn an_old_blob_without_mic_keeps_the_mic_on() // serde_json::from_str::<BoxConfig>("{}").unwrap().mic == true.
// screen.rs
#[test] fn the_mic_master_shows_mic() // Snapshot { mic: true, playing: true, bpm: 128.0, ..Default::default() } (derive Default when it is missing): lines().deck == "MIC  PLAY", bpm == "128.0".
```

- [ ] **Step 2: Run them.** Run: `cargo test -p autoblink-core-ui`. Expected: FAIL.

- [ ] **Step 3: Implement.** In `lines`, `mic: true` gives "MIC  PLAY" or "MIC  STOP" in place of "Deck n". Fix each `Snapshot` literal in the firmware (`esp32/src/ui.rs:447`) with `mic: false` for now.

- [ ] **Step 4: Run them.** Run: `cargo test --locked`. Expected: PASS.

- [ ] **Step 5: Commit.** `git commit -am "Add the Mic setting and the MIC line of the OLED"`

### Task 9: Firmware mic input

**Files:**
- Create: `esp32/src/mic.rs`
- Modify: `esp32/Cargo.toml` (dependency `autoblink-core-beat = { path = "../crates/autoblink-core-beat" }`), `esp32/src/main.rs` (`State`, `mod mic`, the start, the NVS write in the main loop), `esp32/src/midi.rs:91-99` (the UART2 receive input), `esp32/src/settings.rs` (NVS key `mic`)

**Interfaces:**
- Consumes: Tasks 1 to 5, `BoxConfig::mic`, `Settings::box_cfg`, `Settings::device_number`.
- Produces in `mic.rs`:
  - `#[derive(Clone, Copy, Debug, Default)] pub struct MicView { pub enabled: bool, pub state: MicState, pub master: bool, pub device: u8, pub grid: Option<Grid>, pub section: Option<Section>, pub level: u8, pub confidence: u8, pub offset_ms: i16, pub stats: Stats }`
  - `pub fn start(state: Arc<State>, i2s: I2S0<'static>, clk: Gpio2<'static>, din: Gpio12<'static>, box_cfg: Arc<Mutex<BoxConfig>>, device: Arc<AtomicU8>, pdl: Arc<Queue<midi::Beat>>, offset_us: i64) -> anyhow::Result<()>`. `main` makes `pdl` with `Arc::new(Queue::new(16))`. In this task, the thread sends no beats. Task 10 adds the parameter `out: BeatOut`.
- Produces in `State`: `pub mic: Mutex<MicView>`, `pub mic_master: AtomicBool`, `pub mic_offset: Mutex<Option<i32>>` (µs, for the main loop to write).
- Produces in `settings.rs`: `pub fn mic_offset(&self) -> Option<i64>` and `pub fn save_mic_offset(&self, us: i32)` on NVS key `mic` (`get_i32` and `set_i32`).

- [ ] **Step 1: Free IO2.** After `UartDriver::new` in `midi.rs`, route the UART2 receive input to the constant high level: `esp_rom_gpio_connect_in_signal(GPIO_MATRIX_CONST_ONE_INPUT, U2RXD_IN_IDX, false)`, as `route_rx` does (`dmx.rs:899`). In `main.rs`, take a second token with `unsafe { Gpio2::steal() }` for the mic. The comment names the reroute as the reason that the UART does not use the pin.

- [ ] **Step 2: Open the PDM channel.** `I2sDriver::new_pdm_rx(i2s, &cfg, clk, din)` with `PdmRxClkConfig::from_sample_rate_hz(16_000)`, 16-bit mono slots, and a channel `Config` with `dma_buffer_count(6)` and `frames_per_buffer(256)` (96 ms). An error logs a warning, and the thread publishes `NoMic`. The box continues without the mic.

- [ ] **Step 3: Write the thread loop.** `crate::spawn_rt(c"mic", 6144, 4, Some(Core::Core0), ...)`. In each pass:
  1. Read 256 frames with a timeout of 100 ms.
  2. Push them into the `Tracker`.
  3. Call `SampleClock::observe` with `tracker.samples()` and `state.now_us()`.
  4. Drain the `pdl` queue without a wait into `Calibrator::pdl_beat`, with the raw grid from `SampleClock::grid`.
  5. Copy the result into `state.mic`. `MicView::grid` is the calibrated grid, `Calibrator::map` of the raw grid. Hold the lock only for the copy.

  While `BoxConfig::mic` is false, disable the channel, publish `enabled: false`, and check the setting once a second.

- [ ] **Step 4: Log.** Log each state change of the tracker. When the tracker goes to `Locked`, call `Calibrator::note_lock` with `cold_lock_ms`. Log the `Calibrator::take_minute` numbers once a minute. Log a flywheel that passes `LONG_FLYWHEEL_MS`, once for each flywheel.

- [ ] **Step 5: Keep the offset.** At the start, read `Settings::mic_offset`. Without a value, use `DEFAULT_OFFSET_US`. When the offset differs from the last saved value by more than 2000 µs, the thread puts it into `state.mic_offset`. The main loop writes it beside the night memory (`main.rs:401`), at most every 5 minutes.

- [ ] **Step 6: Build.** Run: `. ~/export-esp.sh && cd esp32 && cargo build --release && cargo build --release --no-default-features`. Expected: two builds without a warning.

- [ ] **Step 7: Commit.** `git add esp32 && git commit -m "Read the PDM mic on IO2 and IO12 and run the tracker in a mic thread"`

### Task 10: Firmware takeover

**Files:**
- Modify: `esp32/src/main.rs:318-342` (fan-out), `esp32/src/webui.rs:426` (page beat queue), `esp32/src/midi.rs:225-315` (master view), `esp32/src/dmx.rs:554` (`is_playing`), `esp32/src/webui.rs:1112-1225` (status), `esp32/src/ui.rs:434` (snapshot), `esp32/src/mic.rs`

**Interfaces:**
- Produces in `main.rs`: the parameter `out: BeatOut` of `mic::start`, and `#[derive(Clone)] pub struct BeatOut { .. }` with `pub fn send(&self, beat: midi::Beat)`. It holds the `dmx` queue (with `dmx`), the `midi` queue and the page beat queue, and calls `led::beat`.
- Changes `webui::start` to return `Arc<Queue<(u8, u8)>>` in place of the `SyncSender<(u8, u8)>`. The `web-status` thread receives with `recv_front` and the same timeout.
- Produces in `webui.rs`: `MicJson { state: &'static str, master: bool, bpm: f64 /* grid.bpm(), 0 without a grid */, level: u8, confidence: u8, offset_ms: i16, beats: u32, tempo_err_cbpm: u16, phase_med_ms: u16, phase_p95_ms: u16, bar1_pct: u8, lock_ms: Option<u32> }` as `Status::mic: Option<MicJson>` (`None` while the setting is off). Add "mic" to `CAPS`.

- [ ] **Step 1: Make the fan-out one function.** The Pro DJ Link callback calls `out.send(beat)` and then `pdl.send_back(beat, 0)` for the calibrator.

- [ ] **Step 2: Replace the page beat channel.** Use `esp_idf_svc::hal::task::queue::Queue<(u8, u8)>` of depth 8. The reason is the priority rule in the Global Constraints.

- [ ] **Step 3: Emit the mic beats.** In each pass of the `mic` thread:
  1. Read the playing state of the tracker master: `tracker.master().is_some_and(|m| m.is_playing(now_ms))`.
  2. Call `Arbiter::update` with it.
  3. Set `state.mic_master` and `Calibrator::set_learning(!master)`.
  4. On a new `Drop`, call `Calibrator::on_drop`.
  5. While the mic is the master, send each `Emitter::next` beat as `midi::Beat { device, beat_in_bar, bpm, at_us }` through `out.send`. Read `device` from `Settings::device_number`.
  6. Before each send, read the deck state again. Emit only while no deck plays.
  7. Reset the emitter when the mic stops being the master.

- [ ] **Step 4: Use the master view in `midi.rs`.** When `state.mic_master` is true, build the `Snapshot`, the `SceneInput` and the clock `Master` from `MicView`:
  - `device` is the mic device. `track` is 0. `pitch` is 0.
  - `phrase16` and the beat come from `Grid::position`. `beat_phase` comes from `Grid::beat_phase`. `playing` is true.
  - `guess` is `section` mapped to `Event::Break` or `Event::Drop`.
  - The clock `Master` has `mic: true`.

  When it is false, keep the tracker path and use `mic: false`.

- [ ] **Step 5: Use the master view in `dmx.rs`, `webui.rs` and `ui.rs`.**
  - `dmx.rs`: `is_playing` is the tracker value `|| state.mic_master.load(Relaxed)`.
  - The status: `source` is `Some("Mic")` and `MasterJson` comes from `MicView` while the mic is the master. Fill `Status::mic` from `MicView`.
  - The OLED snapshot: `mic`, `bpm` and `beat` come from `MicView` while the mic is the master.

- [ ] **Step 6: Build and test.** Run: `cargo test --locked && . ~/export-esp.sh && cd esp32 && cargo build --release && cargo build --release --no-default-features`. Expected: PASS and two builds.

- [ ] **Step 7: Commit.** `git commit -am "Give the master to the mic when no deck plays"`

### Task 11: `/mic` capture

**Files:**
- Modify: `esp32/src/mic.rs`, `esp32/src/webui.rs` (route beside `/scope` at :236, command `mic_arm` in `command` at :778)

**Interfaces:**
- Produces in `mic.rs`: `pub fn arm(now_ms: u64)`, `pub fn take_arm(now_ms: u64) -> bool` (true once within 60 s of `arm`), `pub fn capture_start(frames: u32) -> bool`, `pub fn capture_read(buf: &mut [i16], timeout_ms: u32) -> usize`, `pub fn capture_stop()`.
- The capture ring holds 2048 stereo frames (8 KB). It exists only during a capture.

- [ ] **Step 1: Add `mic_arm`.** `Some("mic_arm") => { crate::mic::arm(state.now_ms()); r#"{"type":"mic_arm"}"#.into() }`. Only the active page reaches `command`.

- [ ] **Step 2: Add the route.**
  1. `take_arm` false gives 403.
  2. `s` outside 1 to 120 gives 400. The default is 30.
  3. A free heap below 40 KB or a largest block below 12 KB gives 503.
  4. Write `wav::header(s * 16_000, 2, 16_000)`.
  5. Write the frames from `capture_read`.
  6. A gap of more than 1 s, or the end of the duration, ends the stream. Pad with zeros to the size of the header.
  7. Log the number of dropped frames.

- [ ] **Step 3: Fill the ring in the `mic` thread.** The left channel is the PCM. The right channel holds a PDL master beat at its frame (`SampleClock::to_sample(at_us)`), with the value `beat_in_bar * 1000`. Delay the frames by 2 blocks, so that a PDL beat that arrives late still lands on its frame. A full ring drops the frames and counts them.

- [ ] **Step 4: Build.** Run: `. ~/export-esp.sh && cd esp32 && cargo build --release`. Expected: a build.

- [ ] **Step 5: Commit.** `git commit -am "Record the mic and the Pro DJ Link beats with GET /mic"`

### Task 12: Page

**Files:**
- Modify: `webui/index.html`

**Interfaces:**
- Consumes: `status.mic` (Task 10), `status.source`, the commands `mic_arm` and `set` with index 119.

- [ ] **Step 1: Live master panel.** When `status.source == "Mic"`, show the source "Mic" and the mic state, for example "Mic · locked · 124.0". When `status.mic` exists and the mic is not the master, show a small line "Mic: listening" (or the other state).

- [ ] **Step 2: System › Diagnostics.** Without `status.mic`, hide these rows:
  - the mic level, the confidence and the offset;
  - the minute numbers (beats, tempo error, phase median and 95th percentile, bar-1 hits, lock time);
  - a button "Record mic 30 s". It sends `mic_arm`. On the reply, it opens `/mic?s=30` on the box host. That is the `?box=` host when the page has one.

- [ ] **Step 3: Settings row.** The "Mic" toggle goes into the Sync tab through the section `Mic` (`SECTION_HOME` default). Confirm that it shows.

- [ ] **Step 4: Check the page.** Run `cargo run -p autoblink-esp32-emulator` and open http://localhost:8080. The mic rows must stay hidden there. The owner checks the rows on the box (Task 13, step 7) with `python3 -m http.server 8000 -d webui` and `http://localhost:8000/index.html?box=autoblink.local`.

- [ ] **Step 5: Commit.** `git commit -am "Show the mic source, its state and its accuracy on the page"`

### Task 13: Docs, version and the full check

**Files:**
- Modify: `AGENTS.md`, `docs/wiring.svg`, `Cargo.toml`, `esp32/Cargo.toml`, `Cargo.lock`, `esp32/Cargo.lock`

- [ ] **Step 1: Update `AGENTS.md`.**
  - The architecture map: a row for `crates/autoblink-core-beat`, and `mic.rs` in the `esp32` row.
  - The pin table: IO2 is the PDM clock, and IO12 is the PDM data with the strapping warning.
  - A section "Mic beat fallback". It covers the takeover rule, the handover without Stop, the calibration, `/mic` and `mic_arm`, setting 119, the NVS key `mic` and the thread with its stack. It also covers the known flywheel limit.
  - The app size of 1.40.0 in "OTA update".

- [ ] **Step 2: Update `docs/wiring.svg`.** Add the mic module: 3V3, GND, CLK to IO2, DATA to IO12, SEL to GND.

- [ ] **Step 3: Set the version.** Set `1.40.0` in `Cargo.toml` (`[workspace.package]`) and `esp32/Cargo.toml`. Run `cargo update -w` in the root and in `esp32/`.

- [ ] **Step 4: Run the full check.** Run: `sh scripts/check.sh --esp32`. Expected: the format, clippy, the tests and the two firmware builds pass. Record the app size and the percentage of the slot. The flash must grow by at most 40 KB.

- [ ] **Step 5: Update the graph.** Run: `graphify update .`

- [ ] **Step 6: Commit.** `git commit -am "Document the mic beat fallback and set the version to 1.40.0"`

- [ ] **Step 7: Hand over to the owner.** The box steps are verification steps 3 to 6 of the spec. The owner does them after `sh scripts/ota-push.sh`, because the box takes only OTA updates.
