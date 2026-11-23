# Mic beat fallback on the box — design

The implementation plan is `docs/superpowers/plans/2026-11-12-mic-beat-fallback.md`.

## Context

The box (WT32-ETH01, classic ESP32) gets its beat only from Pro DJ Link today. Without a Pro DJ
Link beat, the MIDI clock, the DMX beat effects, the moves and the autopilot stop. The owner
names three cases:

- A guest DJ plays a whole set on gear without Pro DJ Link (vinyl, Serato, Traktor).
- The XDJ-AZ stops Pro DJ Link during a set.
- The box cannot reach the players at the venue (no cable, wrong network).

The box sits away from the mixer, and the mixers of other DJs often have no free output. Thus the
input is a microphone on the box. A line input is not in scope.

## Goals

- A mic beat tracker drives the MIDI clock and the DMX of the box when no Pro DJ Link deck plays.
  The two outputs have the same priority.
- The tracker finds the tempo, the beat phase and bar 1 from the audio alone.
- In a breakdown without a kick, the clock and the lights continue at the last tempo and phase
  (flywheel) until the kick comes back.
- The mic gives the phrase events Break (the kick stops) and Drop (the kick comes back). A Drop
  sets bar 1 and the phrase start again.
- The tracker runs all the time, also while Pro DJ Link plays (shadow run). Pro DJ Link is the
  ground truth:
  - When Pro DJ Link stops, the mic continues with the tempo, phase and bar 1 of the players.
    It has no lock time.
  - The box measures the delay from the speakers to the mic and compensates it.
  - The box measures the accuracy of the mic at each gig and reports it.
  - `GET /mic` records the audio with the true beats, for offline tuning.

## Non-goals

- The desktop host app and the emulator get no mic.
- No line input. No manual Bar 1 button (the owner chose auto-detect only).
- No trained bar-1 classifier in this round (tier 3). The `/mic` recordings prepare it.
- No drop prediction from the mic. The drop sequence of the autopilot (dip, blackout, hits) does
  not run on mic beats. A mic Drop is a normal bucket event.
- No Intro, Outro, Build or Groove events from the mic.
- No track energy from the mic. The pilot energy uses the BPM alone (`track_energy: None`).

## Anti-goals

- A mic beat must never override a Pro DJ Link deck that plays.
- The mic must not make the MIDI clock or the DMX output stutter. A busy core 0 must not move the
  beat times.
- A box without a mic must work as today.
- Firmware updates must stay OTA-only. No change to the partition table, the bootloader or the
  eFuses.

## Hardware (decided)

- The mic is a PDM MEMS breakout: 3.3 V, an acoustic overload point of 120 dB SPL or more, and
  **no pull-up resistor on DATA**. Candidate chips are ST MP34DT05 and Knowles SPH0641. Before
  the owner buys a module, read its schematic.
- The wiring is 3V3, GND, CLK to IO2, DATA to IO12. SEL (L/R) goes to GND. Add the mic to
  `docs/wiring.svg` and to the pin table in `AGENTS.md`.
- IO12 is a strapping pin. If IO12 is high at reset, the ESP32 sets the flash to 1.8 V and does
  not start. A PDM mic without a clock keeps DATA at high impedance, and the internal pull-down
  keeps IO12 low. Do not use a module with a pull-up on DATA.
- IO2 is the dummy receive pin of the MIDI UART (`esp32/src/midi.rs:91`, `:99`). Free it: route
  the UART2 receive input to the constant high level of the GPIO matrix
  (`esp_rom_gpio_connect_in_signal(GPIO_MATRIX_CONST_ONE_INPUT, U2RXD_IN_IDX, false)`), as
  `route_rx` does for UART1 (`esp32/src/dmx.rs:899`). `UartDriver::new` takes the RX pin by value
  (esp-idf-hal 0.47 `uart.rs:825`). The builder chooses how to give the I2S driver a `Gpio2`
  (for example `unsafe { Gpio2::steal() }` with a comment that names the reroute).
- The PDM receive uses I2S0. The ESP32 converts PDM to PCM in hardware. esp-idf-hal 0.47 has PDM
  RX (`i2s/pdm.rs:194`). The firmware has no I2S code yet.

## Behaviour

### Who drives the beat

- One master view feeds every reader: the MIDI clock, the mapper, the scenes, the pilot, the
  `dmx` thread, the page, the OLED and the LEDs. The readers today call `tracker.master()`
  (`midi.rs:231`, `dmx.rs:554`, `webui.rs:1114`, `ui.rs:436`).
- A Pro DJ Link deck that plays (`Deck::is_playing`, `tracker.rs:67`) is the master, as today.
- When no Pro DJ Link deck plays, and the mic is `Locked` (it hears a live kick), the mic
  becomes the master. Thus the mic takes over about 2 s after the last Pro DJ Link beat. This
  rule includes a deck that the DJ paused and that still sends status (owner decision).
- The mic cannot take over in `Flywheel`. The flywheel only continues a mic master that already
  took over. Thus a pause of the deck in a silent room stops the clock and the lights as today
  (MIDI Stop with Clock Loop Sync, the scene Stop row, `lights.set_playing(false)`).
- When a Pro DJ Link deck plays again, it is the master at once.
- The mic master uses the device number of the box: the setting "Device Number" (index 2,
  default 7). The mic does not borrow the number of a player (owner decision).
- A handover between a Pro DJ Link deck and the mic does not stop the MIDI clock (owner
  decision). Today `Clock::poll` sends Stop on each device change (`clock.rs:134`). Change the
  clock: when one side of the change is the mic, the clock keeps running at its grid and
  takes the new device as its master. The first beats of the new source give the phase error,
  and the clock slews it out at once. It does not wait for `phrase_lock_bars`. A change from
  one deck to another deck keeps the Stop and the Start of today.
- The `Master` that the clock gets must tell the mic from a deck. The builder chooses the field.
  A test in `crates/autoblink-core-midi/src/clock.rs` covers each direction of the handover.
- The mic master is "playing" while it is locked or in its flywheel. It stops playing when the
  input level stays below the noise floor for 10 s (the music stopped). Speech without a kick
  keeps the flywheel.
- Known limit: the owner chose a flywheel without a time limit. Loud crowd noise after the music
  stops can keep the input above the noise floor, and the flywheel then continues. Log each
  flywheel that lasts longer than 5 minutes, so that the owner can decide on a limit later.

### Beat output of the mic

- The mic thread sends a beat for each beat of its predicted grid, not for each raw kick. The
  `Beat.at_us` is the predicted time of the beat. The time base is the sample counter of the I2S
  stream, anchored to `state.now_us()`. A late wake of the thread does not move a beat.
- The output grid is the grid of the tracker plus the calibrated offset (see "Calibration"). The
  bar position is the bar of the tracker plus the calibrated bar rotation.
- While the mic is the master, its beats go into the fan-out of the Pro DJ Link master beat
  (`esp32/src/main.rs:320`). The fan-out sends to `led::beat`, `DmxMsg::Beat`, the `midi` queue
  and the page. Make the fan-out one function that the two sources call.
- While a Pro DJ Link deck plays, the mic beats go only to the calibrator.
- `Clock::beat` keeps the earliest phase error of each lock period (`clock.rs:225`), because a
  late WiFi packet is never early. The mic beats come from a grid, so this filter does not bias
  them. Keep this filter. The clock changes only for the handover (see "Who drives the beat").

### Tracker (tier 1)

The tracker is a new host-testable crate, `crates/autoblink-core-beat`. It has no ESP-IDF
dependency. It gets PCM blocks with their sample index and gives the state below.

- Onset detection in bands: the kick (below about 150 Hz), the clap and snare (about 1 to 4 kHz)
  and a full-band change. The builder chooses an IIR filter bank or an FFT. Tests decide.
- Tempo: 80 to 160 BPM. A tempo outside the range folds into it (drum & bass reads at half
  tempo). A 4/4 prior favours 120 to 130 BPM. The tempo follows a pitch fader.
- Phase: a slow phase lock on the beat grid, causal, with a prediction of the next beat. Club
  tracks sit on a machine grid, so the lock averages over many beats.
- Bar 1: score the 4 beat positions over 8 to 16 bars. A clap on 2 and 4 splits the pairs. Bar
  starts carry more spectral change (crash, bass note, new layer). A kick return after a
  breakdown sets bar 1.
- States: `NoMic`, `Listening`, `Locked`, `Flywheel`.
  - `NoMic`: the PCM is constant for 5 s (no module, or IO12 open).
  - `Flywheel`: the kick stopped. The grid continues at the last tempo and phase.
- Events: `Break` when the kick is gone for 2 bars. `Drop` when the kick comes back after a
  Break. A Drop sets the beat of the 16-beat block (`phrase16`) to 1 on the first beat of the
  kick return.
- Mixes: two kicks slightly apart (a flam during a mix) do not move the phase, unless the offset
  stays for several beats.

### Scenes and pilot

- The mic events go into `SceneInput.guess` (`crates/autoblink-core-midi/src/scenes.rs:336`). The
  engine uses the guess only without a rekordbox phrase, and fires it at a block start
  (`phrase16 == 1`). A Drop starts a block, so it fires at once. A Break fires at the next block
  start.
- The page shows "(guess)" at the row, as it does for the drop guess (`scene.guessed`).
- The pilot gets these events through the scene engine, as today. Without a known drop time, a
  build does not start. The 64-beat re-pick (`REPICK_BEATS`) covers the rest.

### Calibration (tier 2)

- The tracker never sees the Pro DJ Link beats. A calibrator compares the two grids while a Pro
  DJ Link deck plays.
- The calibrator learns:
  - the offset in ms from the mic grid to the Pro DJ Link grid. This includes the sound path
    (about 3 ms for each metre), the DSP delay and the network delay of the beat packets. Use
    the earliest-filtered Pro DJ Link phase, as the MIDI clock does. On WiFi, a beat packet is
    late by up to about 300 ms.
  - the bar rotation, 0 to 3, from the mic bar to the Pro DJ Link bar.
  - the tempo ratio, 1, 2 or 1/2 (my call). A deck outside 80 to 160 BPM folds into the range in
    the tracker. With the ratio, drum & bass at 174 BPM stays at 174 BPM after the handover. The
    bar rotation counts in the beats of the Pro DJ Link grid. Without Pro DJ Link, the folded
    tempo applies.
- When the mic takes over, the calibrator stops learning. The mic continues with the last offset
  and rotation. After the takeover, a Drop sets bar 1 again. The bar detector of the tracker
  changes bar 1 only when it disagrees with high confidence over at least 8 bars.
- NVS keeps the last offset under a new key `mic`. A night that never sees Pro DJ Link uses it.
  Without a stored value, the offset is 30 ms. The `midi` thread must not wait for the flash
  write. Write in the main loop, at most every 5 minutes, and only after a change of more than
  2 ms (like the night memory).
- Accuracy, measured against Pro DJ Link before the calibration applies:
  - the tempo error,
  - the phase error (median and 95th percentile, in ms),
  - the bar-1 hit rate,
  - the lock time after a track change.
  The `/log` shows the numbers once a minute while the two sources run. The page shows them in
  System › Diagnostics.

### Recording for tuning

- `GET /mic?s=<seconds>` (1 to 120) streams a 16 kHz, 16-bit stereo WAV. The left channel is the
  mic. The right channel is the beat track: an impulse at each Pro DJ Link master beat, with the
  value `beat_in_bar * 1000`. The duration gives the WAV header sizes before the stream starts.
- The route streams a live room microphone. It answers 403 unless the active page armed it. The
  active page sends the WebSocket command `mic_arm`. The route then accepts one capture that
  starts within 60 s (owner decision). Another page gets `busy`, as for each command. The page
  sends `mic_arm` and opens the route from the Diagnostics button.
- The capture must not stop the MIDI clock or the DMX output. The page may get no status during
  the capture, like an OFL fetch.
- An example binary in the core crate runs the tracker on such a WAV and prints the accuracy
  numbers: `cargo run -p autoblink-core-beat --example eval -- <file.wav>`. Keep the recordings
  out of git.

### Settings, page, OLED

- Setting 119 "Mic": Auto (default) or Off. It is a firmware setting in
  `crates/autoblink-core-ui/src/config.rs` (NVS key `box`). Off stops the I2S driver and the mic
  thread work.
- The status field `master.source` (`webui.rs:1214`) is "Mic" while the mic is the master.
- A new status field `mic` has `state` (`off`, `no_mic`, `listening`, `locked`, `flywheel`),
  `bpm`, `level`, `confidence`, `offset_ms` and the accuracy numbers. A new capability `mic`. The
  page hides the mic rows without it (host, emulator, firmware with the setting Off).
- The Live master panel shows the source and the mic state. System › Diagnostics shows the
  accuracy numbers and a button that opens `/mic?s=30`.
- The OLED Main page shows `MIC 128.0` while the mic is the master. The labels are ASCII only.

## Budgets and placement (decided as targets)

- The `mic` thread runs on core 0 with priority 4 (below WiFi, above `ui`). Start it with
  `crate::spawn_rt`. Measure the stack with `/mem` and keep 1.5 KB above the low point.
- The I2S DMA buffer holds at least 96 ms of audio, so that a WiFi burst on core 0 drops no
  sample.
- Heap: at most 12 KB for the DMA buffers and the tracker. Measure with `/mem` while the page is
  open and a deck has a track.
- CPU: at most 10 % of core 0.
- Flash: at most 40 KB more. The app uses 91 % of the slot (0x1E0000 bytes) now. Record the new
  size in `AGENTS.md`.
- Do not use an `f32` field in a struct that serde reads on the firmware (LLVM for Xtensa fails).
  f32 math is fine.

## Left to the builder

- The DSP method (filter bank or FFT), the window lengths, the onset rate and the thresholds.
  Tests and the `/mic` recordings decide.
- The code shape of the master view (a function, a struct or an enum over the two sources).
- The confidence measure and its thresholds.
- The test signals: synthetic click and kick tracks with a clap on 2 and 4, a breakdown, a tempo
  ramp and a flam. They run in `cargo test --locked`.

## Critical files

- New: `crates/autoblink-core-beat/` (tracker, calibrator, tests, `examples/eval.rs`),
  `esp32/src/mic.rs` (I2S PDM, thread, `/mic` route).
- Change: `crates/autoblink-core-midi/src/clock.rs` (handover without Stop),
  `esp32/src/main.rs` (fan-out function, mic start), `esp32/src/midi.rs` (UART2 RX
  reroute, master view, `SceneInput.guess` from the mic), `esp32/src/dmx.rs` (master view),
  `esp32/src/webui.rs` (status, capability), `esp32/src/ui.rs` (OLED Main),
  `esp32/src/settings.rs` (NVS key `mic`), `crates/autoblink-core-ui/src/config.rs` (setting
  119), `webui/index.html`, root `Cargo.toml` (workspace member), `docs/wiring.svg`, `AGENTS.md`.
- Version: new functionality, so a minor step to 1.40.0 in `Cargo.toml` and `esp32/Cargo.toml`.

## Rollout

- 1.40.0 ships the full feature with the takeover on. "Mic" is Auto by default (owner decision).
  The thresholds come from the synthetic tests. The `/mic` recordings tune them in later hotfix
  versions.
- The implementation plan builds in this order. Each step builds and passes the tests:
  1. The core crate with the tracker and the synthetic tests.
  2. The calibrator and its tests.
  3. The firmware input: the IO2 reroute, I2S PDM, the `mic` thread, `NoMic`, `/mic`.
  4. The master view, the fan-out function, the takeover and the clock handover.
  5. The scenes events, the page, the OLED, the settings and the docs.
- The owner buys the module, reads its schematic for the DATA pull-up, and solders it. The owner
  runs verification steps 3 to 6 on the box, because the box takes only OTA updates.

## Verification

1. `cargo test --locked`: the core crate tests pass on the synthetic signals (lock time, tempo
   error, phase error, bar 1, Break and Drop, flywheel, flam, `NoMic`).
2. `sh scripts/check.sh --esp32`: the format, clippy, the tests and the firmware builds with and
   without `dmx`. Record the app size.
3. A box without a mic, OTA with `sh scripts/ota-push.sh`: everything works as before.
   `/mem` shows the heap. The page shows `mic.state = no_mic`.
4. A box with a mic, music from the XDJ-AZ over speakers at home:
   - `/log` shows the accuracy numbers once a minute, and the page shows them in Diagnostics.
   - Pull the Ethernet cable (or stop Pro DJ Link). The MIDI clock continues without Stop and
     Start. The DMX beat effects continue, and the page shows the source "Mic".
   - Play a track with a breakdown. The clock continues through it, and the page shows the
     Break and the Drop.
   - Connect the cable again. Pro DJ Link is the master again.
5. `GET /mic?s=30`, then `cargo run -p autoblink-core-beat --example eval -- rec.wav` on the
   computer. The numbers match the numbers of the box.
6. `/mem`: the stack of `mic` and the heap are within the budgets.
