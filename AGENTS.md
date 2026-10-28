# AGENTS.md — AUTOBLINK

## Workspace

- **3 crates**: `autoblink-host` (desktop app), `autoblink-esp32` (firmware), `autoblink-esp32-emulator` (native emulator)
- The root workspace excludes `esp32`. The firmware has its own `Cargo.lock`, toolchain (`esp`) and target directory.
- **Root `Cargo.toml`** defines workspace members and patches `rusty_link` from `vendor/rusty_link`
- Package version is set once at workspace level (`[workspace.package]`)

## Developer Commands

```sh
# Host (desktop app). A bare `cargo run` starts it (`default-run` in host/Cargo.toml).
# The web UI is at http://127.0.0.1:11111 (next free port when in use).
cargo run
cargo run -p autoblink-host
cargo run -p autoblink-host -- --web-bind 127.0.0.1:11111
cargo run -p autoblink-host -- --list-midi
cargo run -p autoblink-host -- --list-interfaces
cargo build -p autoblink-host --release

# ESP32 firmware for the WT32-ETH01 (see "ESP32 Build Requirements")
. ~/export-esp.sh
cd esp32 && cargo build --release
cd esp32 && cargo run --release   # flashes the board and opens the serial monitor

# Emulator (Web dashboard at http://localhost:8080)
cargo run -p autoblink-esp32-emulator

# Tests (all crates)
cargo test --locked

# The checks of CI before a push: format, clippy, tests, and the firmware build when the branch changes it
sh scripts/check.sh            # --esp32 always builds the firmware, --no-esp32 never builds it
git config core.hooksPath .githooks   # once for each clone: the pre-push hook runs scripts/check.sh

# Single workspace member
cargo test -p autoblink-host
cargo build -p autoblink-esp32-emulator
```

## Vendor Setup (required before local build)

The Ableton Link crate (`rusty_link` v0.2.3) must be vendored locally. CI handles this automatically. For local development:

```sh
mkdir -p vendor
curl -sL https://crates.io/api/v1/crates/rusty_link/0.2.3/download | tar xzf - -C vendor
mv vendor/rusty_link-0.2.3 vendor/rusty_link
cp .github/vendor-patches/build.rs vendor/rusty_link/build.rs
cp .github/vendor-patches/link_bindings.rs vendor/rusty_link/link_bindings.rs
```

**Do not skip the patch step.** The vendored `build.rs` patches CMake build flags for static linking of `lib_abl_link_c`. Without it, the build will link dynamically and fail at runtime.

## Release Build Targets

- Windows x86-64 → `.zip` archive
- Linux x86-64 → `.tar.gz` archive
- Linux ARM64 (Raspberry Pi) → `.tar.gz` archive, built on the native `ubuntu-24.04-arm` runner. The Linux archives include `contrib/autoblink.service`.
- macOS ARM64 → `.tar.gz` archive
- ESP32 firmware → `esp32-firmware.tar.gz`. It holds the ELF file, the merged image for address 0x0 and the OTA image. The workflow also puts the OTA image and `esp32-version.txt` into the prerelease `esp32-latest`.

## Architecture Map

| Crate | Role | Key modules |
|---|---|---|
| `host` | Desktop app + shared library | `config`, `midi/*`, `prolink/*`, `state`, `web/*`, `settings`, `link` |
| `esp32` | Firmware for the WT32-ETH01, ESP-IDF with `std` | `net.rs` (Ethernet, WiFi AP, WiFi station), `ota.rs` (update from GitHub), `prolink.rs` (virtual CDJ, UDP receive), `tracks.rs` (dbserver fetch), `midi.rs` (UART MIDI out), `led.rs` (beat LED, status LED), `dmx.rs` (feature `dmx`), `show.rs` (the `show` partition), `ofl.rs` (Open Fixture Library fetch), `webui.rs` (page, protocol v1, waveform route), `settings.rs` (NVS), `logring.rs` (log that stays through a restart, core dump report) |
| `crates/autoblink-core-dbserver` | Blocking dbserver client of the firmware, host-testable | `client.rs` (fetch), `protocol.rs` (wire format), `parse.rs` (PSSI, PWV4, PQTZ, cues), `types.rs` (song structure, phrase position) |
| `crates/autoblink-core-midi` | MIDI logic of the firmware, host-testable. The host uses `scenes.rs` too. | `clock.rs`, `mapper.rs`, `scenes.rs` (DMX scenes), `settings.rs` (host setting indices) |
| `crates/autoblink-core-dmx` | DMX looks of the firmware, host-testable | `show.rs` (show model and limits), `render.rs` (look into universe), `clock.rs` (beat clock), `effect.rs` (intensity and bar effects), `lights.rs` (engine) |
| `esp32-emulator` | Native emulator | `main.rs`, `verify_stopped.rs` |

### Host library exports (`host/src/lib.rs`)

`pub mod config link logbuf midi prolink runtime settings state web`

Plus `pub fn interface_priority()` for network interface ranking.

### State management

`SharedState = Arc<RwLock<DjState>>` — all tasks read/write through this type alias. Defined in `host/src/state.rs` along with `SharedConfig = Arc<RwLock<Config>>`.

### Module ownership (host/src/)

- `prolink/mod.rs` — protocol constants (MAGIC, PORT_*, PKT_*, PITCH_NORMAL, helper fns)
- `prolink/packets.rs` — zero-copy packet parsing (KeepAlive, BeatPacket, AbsPositionPacket, CdjStatus)
- `prolink/discovery.rs`, `prolink/beat_listener.rs`, `prolink/status_listener.rs` — UDP listeners
- `prolink/virtual_cdj.rs` — virtual CDJ participant
- `midi/clock.rs` — MIDI clock generation (24 PPQ, Start/Stop/Continue)
- `midi/mapper.rs` — beat → MIDI CC/Note mapping
- `midi/input.rs` — XDJ-AZ USB MIDI input. It stores the EQ and channel fader values in `DjState.mixer`.
- `web/mod.rs` — axum server, WebSocket protocol v1 (documented in the module header), Origin check, port fallback
- `prolink/metadata.rs` — the dbserver client. One TCP session gets the metadata, the PSSI phrases, the PWV4 waveform preview, the extended cue list (`0x2b04`) and the PQTZ beat grid. The beat grid puts the phrases on the time axis. The web route `GET /waveform/{deck}` serves the waveform and the cues.
- `web/status.rs` — status snapshot; takes each lock alone, never two at once
- `settings.rs` — the settings table and the validated changes
- DMX scenes: the engine is `autoblink_core_midi::scenes`. `midi/mapper.rs` gives it the beats and the master state. The page sends the full settings as `scenes_set`. The host writes them to `scenes.json` in the folder of the config file (`config::save_scenes`, on a blocking thread). The TOML config has no scenes.
- `webui/index.html` (repo root) — the one page that the host, the emulator and the ESP32 serve
- `runtime/mod.rs` — `supervise` restarts each long-running task after a panic. The MIDI clock, the MTC, the mapper and the MIDI output worker run on the `midi-rt` thread, a separate single-thread runtime.

## Platform-Specific Code

**Windows vs Unix socket differences**: `set_reuse_port(true)` is Unix-only. Files affected:

- `host/src/prolink/discovery.rs`
- `host/src/prolink/beat_listener.rs`
- `host/src/prolink/status_listener.rs`
- `host/src/prolink/virtual_cdj.rs`

All have `#[cfg(not(windows))]` guards around reuse_port calls.

**ESP32 vs host**: The ESP32 uses `std::net::UdpSocket` and `TcpStream` on the ESP-IDF network stack. It has no `tokio` runtime; each task is a `std::thread`. It parses packets with `autoblink-core-prolink`, and `autoblink_core_prolink::Tracker` keeps the deck and master state. It fetches the track data with `autoblink-core-dbserver`, one track after the other. The host does not use these crates yet. It has its own async copy of the dbserver client in `host/src/prolink/metadata.rs`.

## Toolchain & Dependencies

- **Async runtime**: tokio (multi-threaded) on host/emulator; `std::thread` on ESP32
- **Error handling**: `anyhow::Result` on host, emulator and ESP32
- **Logging**: `tracing` + `tracing-subscriber` on host; `log` on ESP32
- **State sync**: `parking_lot::RwLock` (not tokio's)
- **MIDI**: `midir` on host (no ESP32 equivalent)
- **Web UI**: `axum` with WebSockets on host and emulator; vanilla HTML/JS page, no build step
- **Network interfaces**: `network-interface` crate on host

## Testing

- **`cargo test --locked`** runs tests across all crates
- No dedicated test infrastructure found (no `tests/` directory, no fixtures)
- Do the builds on your computer. GitHub runs only quick checks.
- Before a push, run `sh scripts/check.sh`. It runs the checks of CI. When the branch changes `esp32/`, `crates/` or `webui/`, it also builds the firmware with and without `dmx`. The feature `dmx` is a default feature, so `--no-default-features` builds without it.
- `ci.yml` runs for each pull request and each merge to `main`. One Linux job checks the format, runs clippy with `-D warnings` and runs the tests. A change to Markdown or `docs/` only does not start it.
- The Windows and macOS jobs of `ci.yml` run `cargo check` only after a merge to `main` or on a manual start.
- `esp32-ci.yml` builds the default firmware (with `dmx`) only after a merge to `main` or on a manual start. No test runs on the device.
- To start a workflow on a branch, run `gh workflow run ci.yml --ref <branch>` or `gh workflow run esp32-ci.yml --ref <branch>`.
- The release workflows (`release.yml`, `esp32-release.yml`) start from a tag. They build all targets.
- Coverage gaps: packet parsing, state machine transitions, BPM smoothing, MIDI clock transport

## ESP32 Build Requirements

The firmware is for the WT32-ETH01 board: a classic ESP32 (Xtensa) with a LAN8720 Ethernet PHY.

1. Install the toolchain once: `espup install`, then `cargo install espflash ldproxy`.
2. In each new shell, run `. ~/export-esp.sh`.
3. Build in `esp32/`: `cargo build --release`. The first build downloads ESP-IDF v5.5.3 to `~/.espressif`.
4. Connect the USB-C programming dock. Run `cargo run --release` in `esp32/` to flash the board. The runner writes the bootloader of the build, `partitions.csv` and the app into `ota_0`. It erases `otadata`.
5. If espflash cannot connect, hold BOOT, push RST, then release BOOT.

The output is `esp32/target/xtensa-esp32-espidf/release/autoblink-esp32`.

### OTA update

- The flash has two app slots, `ota_0` and `ota_1` (`esp32/partitions.csv`). The NVS partition keeps its old offset and size, so the settings stay.
- Firmware 1.17 and older use a single-app table. Flash the first firmware with OTA over USB. After that, the box can update itself.
- The System panel of the page sends `ota_check` and `ota_install`. Its Restart button sends `reboot`, and the firmware restarts after 1 s (`restart_soon`). The panel also has the WiFi rows of the settings (section `WiFi`, indices 100 to 103). The header shows the uptime from `system.uptime_s`. The host sends no uptime, so the header hides it. TLS to GitHub takes about 50 KB of heap. Version 1.19.0 has about 100 KB free with 1 track loaded and the page open.
- `ota_check` starts an `ota` thread, which stops after the check. It reads `esp32-version.txt` from the prerelease `esp32-latest` on GitHub. The check refuses to start below 60 KB free heap or a 24 KB largest block. While it runs, the page gets no status.
- `ota_install` stores a request in NVS (namespace `ota`) and restarts. In this maintenance start, `main` starts only WiFi and Ethernet, and `ota::install_and_restart` downloads the image into the free slot. Then the box restarts. An error goes into NVS, and the page shows it after the next normal start.
- The maintenance start removes the request first. Thus a crash during the download does not repeat the download at each start.
- The box needs internet for the update: a joined WiFi network (indices 102 and 103), or an Ethernet network with a router.
- A router at 192.168.1.1 is often the DNS server too. The box has that address on its own WiFi network, so the DNS query goes to the box itself. In that case, the `ota` thread sets the DNS server to 1.1.1.1.
- `EspOta::get_running_slot` of esp-idf-svc 0.53 fails with `ESP_ERR_INVALID_SIZE` on this firmware. Read the label from `esp_ota_get_running_partition` instead.
- The flash writes can make the MIDI clock stutter. The page warns when a deck plays. It does not refuse the update.
- Each show save erases flash sectors. This pauses the DMX frames and can make the MIDI clock stutter, like an OTA write.
- The bootloader has rollback (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`). A new app calls `ota::mark_valid` after the web server and the Pro DJ Link threads start. If it restarts before that call, the bootloader starts the previous app.
- Do not use the bootloader of espflash. It has no rollback. The runner and the release workflow give `--bootloader` with the bootloader of the build.
- The tag `esp32-vX.Y.Z` must match the version in `esp32/Cargo.toml`. The release workflow stops when they differ. Otherwise the box installs the same firmware after each check.
- An app image must be smaller than the slot (0x1E0000 bytes). Version 1.21.0 uses 74 % of the slot (1 450 080 bytes). Version 1.28.0 uses 85 % (1 672 752 bytes). Full LTO, the gzip page and the common certificate bundle make it smaller.
- The OTA check uses the common certificate bundle (`CONFIG_MBEDTLS_CERTIFICATE_BUNDLE_DEFAULT_CMN`). The GitHub certificate chains end at DigiCert and USERTrust. If `ota_check` fails with a certificate error, set the full bundle again.

### Web page without a flash

- The firmware serves the page from the app image. A change to `webui/index.html` therefore needs a flash, or an OTA update and a restart.
- To work on the page, serve it from the computer: `python3 -m http.server 8000 -d webui`.
- Open http://localhost:8000/index.html?box=autoblink.local. The page then uses the box for the WebSocket and the waveforms. A reload shows each change.
- The firmware accepts a page from `localhost` or `127.0.0.1` (`dev_origin` in `webui.rs`). The waveform route sends `Access-Control-Allow-Origin: *`.
- Flash the box only when the page is complete.

### DMX scenes

- The firmware keeps the scenes as JSON in NVS (key `scenes`). Like the MIDI settings, it writes them 2 s after the last change (`Settings::save`).
- The webui parses `scenes_set` straight into `ScenesConfig`, not into a `serde_json::Value`. `MAX_COMMAND_LEN` is 3072 bytes for this command.
- The page sends `scene_test`. The webui puts the preset into `State::scene_test`, and the `midi` thread sends the note.
- The `midi` thread has 6 KB of stack since 1.23.0 (5 KB before). Measure it with `/mem`.
- Do not use an `f32` field in a struct that serde reads on the firmware. LLVM for Xtensa stops with "Cannot select: XtensaISD::PCREL_WRAPPER" (`scenes::Decay` uses `f64`).
- The firmware has no mixer data. The page hides the Mixer rows of the scenes without the `mixer` capability.
- The setting "Note Off Velocity 0" (index 33, default on) sends each note off as a note on with velocity 0. It applies to the MIDI notes and the DMX scenes. A DMX controller of the user kept the light on after a 0x80 note off. The test of a preset (`scene_test`) always uses 0x80.
- The scene setting `one_at_a_time` (default on) is for a controller that shows one preset at a time. A note off of such a controller can end a preset with a different note. With the setting on, a flash ends before the next note on. The held scene comes back after a flash or a pulse. The engine sends no note off before a note on of the same note. Such a note off turns the light off for a moment.
- Index 33 is after the OSC rows, so that the other indices stay. The page puts a row into the group of its section.
- A row entry of 32 and up is an extra note (`scenes::EXTRA_BASE`): 32 is `extra[0]`. The page moves the entries when it removes an extra note.
- `Row::holds` gives each entry of a row a hold in beats (0 is no hold). The page sends it only when a row has a hold, so `scenes_set` stays below `MAX_COMMAND_LEN`. The page keeps `holds` aligned with `presets` when it adds, removes or moves an entry.
- An entry with a hold sends its note off after the hold, whatever the gate of the row. During the hold, the triggers of the row wait in one pending trigger. When the hold ends, the next entry fires at once. A scene row drops its pending trigger when another row has become the current scene.
- After a flash or a pulse, `restore_held` sends the note of the newest held entry again (with `one_at_a_time`). The light can then show the entry of an older scene row while the page shows the newer scene.
- The status field `scene.events` counts each event in the order of `Event::ALL`, also without presets and while the scenes are off. The page keeps the same order in `SCENE_EVENTS`.
- The drop guess (`autoblink_core_midi::guess::DropGuess`) gives a bucket (Intro, Groove, Build, Drop, Break, Outro) to a track without rekordbox phrases. The engine uses it only when `SceneInput::phrase` is `None`. Phrases always win.
- The profile is the sum of the 6 PWV4 bytes of a column divided by 6. It is folded to at most 600 columns (`u8`) and smoothed with a moving average of 4 columns on each side. The page does not receive it.
- The thresholds come from the profile of the track: `hi` is the 70th percentile, `lo` is the 30th percentile, and `mid` is the median. Before the first column and after the last column at or above `mid`, the bucket is Intro or Outro. Drop is at or above `hi`. Break is at or below `lo`. Build is a column below `hi` where the next 2 % of the track reaches `hi`. The rest is Groove. A Drop or a Break stays while the energy is within 8 of its threshold (hysteresis). When `hi - lo` is below 24, the track gives only Intro, Groove and Outro. A loudly mastered track therefore gives no Drop guess.
- The engine fires the guessed bucket only at a block start: `phrase16 == 1` after a value other than 1. It fires only when the bucket differs from the last bucket that the guess fired for this track. A new track or a new master deck resets it. Play without a Play row fires the guess again at once.
- The page shows "(guess)" at the row of the current bucket when the status field `scene.guessed` is true.
- The guess is built once for each track. The box builds it in the `dbserver` thread before it takes the lock (about 4 KB of heap for a short time, `tracks.rs`). The host builds it in `set_overview` (`host/src/state/dj_state.rs`). Without a preview of 32 columns, there is no guess.

### DMX show

- The `show` partition holds the DMX show (`show, data, undefined, 0x3F0000, 0x10000` in `esp32/partitions.csv`). Its content is JSON. A u32 length comes first. The firmware writes the length last, so a write that stops early leaves no show. The box then starts with an empty show until the page saves again.
- OTA does not change the partition table. Flash a box once over USB to add the partition.
- Do not use the subtype `0x40` for the partition. It panics espflash 4.6.0. The table uses the named subtype `undefined`.
- `GET /show` serializes the running show. `POST /show` is a CORS simple request with an Origin check. It answers 413 when the body is larger than 60 000 bytes. It answers 503 when the body is larger than half of the largest free heap block. The reader bounds the lists of the show while it reads them.
- The firmware sends the capability `dmx` and the status field `dmx`. The page sends the WebSocket commands `dmx_live`, `dmx_fog` and `dmx_aim`.
- The `dmx` thread has priority 6 on core 1. Its stack is 6 KB. This size is an estimate. Measure it with `/mem` on a box.
- Three senders use one queue of depth 32 to the `dmx` thread: `midi`, `prolink-rx` and the HTTP task (`dmx_fog`). A full queue drops a message. A dropped Off keeps its layer on until the next Off of that preset. An On does not remove the layer. A stop only hides it.
- The DMX output starts before WiFi. Thus the OTA maintenance start sends the idle look.
- The strobe hold ends when the active page closes or a send to it fails. A take-over of another page also ends it.
- The HTTP task has 8 KB of stack. Nobody has measured it under `POST /show`.
- A fixture has the channel kinds `pan`, `pan_fine`, `tilt` and `tilt_fine`. The generic types are `moving_head` (pan, tilt, dimmer, red, green, blue) and `moving_head_16` (with the fine channels). A head with only coarse channels gets the high byte of the 16-bit value.
- The show holds up to 8 positions (`MAX_POSITIONS`, names of 16 characters). A position holds aims, and an aim is a 16-bit pan and tilt for one fixture. A head without an aim in the position sits at 32768 on each axis.
- A look row can have `position` (an index) and `shape`. The shapes are `circle`, `figure8` and `sweep`. A shape has `beats` (1 to 32) and `size` (0 to 255). The amplitude is `size` x 64 units of the 16-bit range, so 255 is a quarter of the range.
- The offset of a shape comes from `t = (i + phase) / beats + k / n`, where `i` is the beat index. `i` counts the beats from a bar line: `i = (beat in bar - 1 + 4 * bar number) mod beats`. A period of 4, 8 or 16 beats therefore starts on beat 1. Without bar information, `i` is the beat count mod beats. The effects with `Every::Bar` use the same fallback: the beat in the bar is the count mod 4. Fixture `k` of `n` in the group is `k / n` of a cycle ahead of the first one. Circle is (cos, sin) of 2 pi t. Figure8 is (sin, sin(2a) / 2). Sweep moves pan only, as sin. The sum of position and offset is clamped to 0 to 65535. It does not wrap. Without a beat, the offset is zero.
- The WebSocket command `dmx_aim` (fixture below 32, pan, tilt) holds the pan and tilt of one head for `AIM_MS` (10 s, `crates/autoblink-core-dmx/src/lights.rs`). The aim overrides every layer and the idle look. The queue drops it when full. The page sends about 20 aims each second and always sends the last value.
- A fade between two looks blends the coarse byte and the fine byte alone. A pan across a byte border can jump for a moment. The `ponytail:` comment in `lights.rs` names the fix: blend the 16-bit pairs.

### Light library (Open Fixture Library)

- The box is the proxy of the Open Fixture Library (OFL). The phone on the WiFi network of the box often has no internet. The firmware passes the JSON of OFL through and does not parse it (`esp32/src/ofl.rs`).
- `GET /ofl/search?q=<text>` searches. The text has 1 to 64 characters, and the page asks only for 3 or more. A 1-letter search gives a reply of 48 KB, which the firmware refuses (502).
- `GET /ofl/fixture?key=<manufacturer>/<model>` gets one light. The key must match `^[a-z0-9-]+/[a-z0-9-]+$`.
- Both routes check the Origin and send `Access-Control-Allow-Origin: *`. The statuses are:
  - 400: the value is bad.
  - 403: the Origin is wrong.
  - 502: OFL fails, the reply is too large, or the box has no internet.
  - 503: the heap is low, or another fetch or an OTA check runs.
  - 504: OFL does not answer in 10 s.
- A fetch does not start below 80 KB free heap or a 32 KB largest block (`MIN_FREE`, `MIN_BLOCK`). TLS takes about 50 KB. The firmware cuts off a reply larger than 24 KB (`MAX_BODY`).
- The fetch runs on the `ofl` thread (12 KB stack, priority 5, from `crate::spawn`). The HTTP handler joins the thread. Thus a fetch blocks the HTTP task. The page gets no status and no beats for up to tens of seconds. The MIDI clock and the DMX output keep running.
- While `ofl::busy()` is true, the status thread sends no status, as during an OTA check. The page disables the library search while the strobe hold is on.
- The certificate chain of OFL ends at ISRG Root X1. The common certificate bundle has it through the X2 cross-certificate.
- The box needs internet: a joined WiFi network (indices 102 and 103), or an Ethernet network with a router.
- `Fixture.ofl` holds `<key>#<mode>`, for example `eurolite/led-bar-6-qcl-rgbw#4-channel`. The core limits it to 96 characters.
- The page maps the OFL channels to the kinds of the show. A pixel channel gets the kind of its template channel, so each pixel shows the same colour. A channel that no template describes becomes a fixed placeholder channel, so that the footprint stays right. A Pan or Tilt channel maps to `pan` or `tilt`, and its fine alias maps to `pan_fine` or `tilt_fine`. A mode with an unknown pixel count, or with more than 32 channels, the page does not add.
- Nobody has measured the heap after many fetches on a box. Run `/mem` after ten fetches.

### Debug logs

- Run `curl http://autoblink.local/log` to read the log over WiFi. The page shows no log.
- Run `curl http://autoblink.local/mem` to read the heap and the free stack of each task. The free stack is the low point since the start. Set a stack size from it, and keep a margin.
- Start a thread with `crate::spawn`. It gives the FreeRTOS task the thread name. Without it, `/mem` shows each Rust thread as "pthread".
- `crate::spawn` gives priority 5 on the two cores, the same as the HTTP server task. `crate::spawn_rt` sets a priority and a core. The WiFi task runs on core 0. The `midi` thread has priority 10 on core 1. The `prolink-rx` thread has priority 8 on core 1.
- `prolink-rx` receives on the three Pro DJ Link sockets with one `poll` loop and one buffer. The task watchdog watches it and the `midi` thread. After a receive error, the thread sleeps for 10 ms, so that the idle task can run.
- Each stack comes from the heap. The sizes in 1.19.0 keep about 1.5 to 2 KB free in each task. Measure with `/mem` again after a change that adds work to a thread. Version 1.21.0 changes `main` to 7 KB and adds `prolink-rx` with 6 KB. Measure these two.
- The status message and the settings message are `Serialize` structs. Do not build them as a `serde_json::Value` tree. A tree has a heap block for each field, and the blocks fragment the heap.
- The firmware keeps 600 columns of the waveform preview (`autoblink_core_dbserver::PREVIEW_COLUMNS`). The players send 1200.
- The dbserver client reads the PWV4 preview and the PQTZ beat grid as a stream (`read_message_with`). It keeps only 600 preview columns and the beats of the phrase starts, the fill-ins and the end. A buffered grid and its parsed copy needed up to 72 KB for one fetch. Without phrases, the client does not request the beat grid.
- The fetch does not start below 24 KB free heap or an 8 KB largest block. The page then shows "not enough memory for the track data", and the fetch starts again after 3 s.
- Do not use `std::io::copy` in a firmware thread. It puts an 8 KB buffer on the stack.
- Do not use `std::sync::mpsc` when the receiver has a higher priority than the sender on the same core. The receiver can spin on a half-written slot, and the sender cannot run. Use `esp_idf_svc::hal::task::queue::Queue` (see `midi.rs`). Firmware 1.21.0 restarted from the task watchdog on the `midi` thread because of this.
- The log ring is in RTC slow memory (4 KB). A panic, a watchdog or a software restart keeps it. A power-on clears it.
- The text has three parts: the last crash, the lines of the previous start and the lines of this start.
- The firmware keeps the crash report outside the ring. Thus a flood of log lines cannot push it out.
- The ESP-IDF C components (WiFi driver, HTTP server) log only to UART0. The ring has only the Rust log lines. The C components log only warnings and errors (`CONFIG_LOG_DEFAULT_LEVEL_WARN`), because each UART write blocks the task.
- After a crash, the panic handler writes a core dump to the `coredump` partition (0x3E0000). At the next start, the firmware logs the task, the exception and the backtrace. Then it erases the dump.
- A box with an older partition table has no `coredump` partition. Flash it over USB to add the partition. OTA does not change the partition table.
- Decode the backtrace with the ELF of the same build:

```sh
~/.rustup/toolchains/esp/xtensa-esp-elf/*/xtensa-esp-elf/bin/xtensa-esp-elf-addr2line -pfiaC \
  -e esp32/target/xtensa-esp32-espidf/release/autoblink-esp32 0x400d1234 0x400d5678
```

- The release workflow puts the ELF into `esp32-firmware.tar.gz`. Use that ELF for a box with a release firmware.
- A task watchdog timeout also gives the reset reason "panic", because `CONFIG_ESP_TASK_WDT_PANIC` is on.

### WT32-ETH01 pins

| Pin | Use |
|---|---|
| GPIO16 | Switches on the 50 MHz oscillator of the PHY. Set it high before the Ethernet driver starts. |
| GPIO0 | RMII clock input from the oscillator |
| GPIO18, 19, 21, 22, 23, 25, 26, 27 | RMII and MDIO to the LAN8720 (PHY address 1) |
| IO32 | Beat LED. It pulses for 30 ms on each beat of the master deck. |
| IO33 | Status LED, with the label 485_EN on the board. While the master deck plays, it pulses for 120 ms on beat 1 and for 25 ms on the other beats. When no beat arrives for 2 s, it shows a blink code (see below). |
| IO17 | MIDI out (UART2 TX, 31250 baud). TRS-A: IO17 → 10 Ω → tip, 3V3 → 33 Ω → ring, GND → sleeve. |
| IO35 | UART2 RX. It is not used, but the UART driver needs a receive pin. |
| IO4 | DMX out (UART1 TX, 250 kbaud) to an RS-485 module. The `dmx` feature is on by default. |
| IO36 | UART1 RX. It is not used, but the UART driver needs a receive pin. |

The blink code of IO33 repeats every 2 s. The LED shows the first line that applies:

| IO33 | State |
|---|---|
| On | The firmware starts. |
| 1 blink | The Ethernet port and the joined WiFi network have no address. Examine the cable. |
| 2 blinks | The virtual CDJ is not on the Pro DJ Link network. It joins, or another device has its device number. |
| 3 blinks | The firmware finds no player. |
| 4 blinks | The firmware cannot join the WiFi network of the settings (index 102). |
| 1 s on, 1 s off | The box is ready. No deck plays. |

After a panic, a watchdog or a brownout restart, an 800 ms flash comes before the code for 10 minutes. The code is in `esp32/src/led.rs`.

Do not connect a load to IO12 at power-on. If IO12 is high, the ESP32 sets the flash to 1.8 V and does not start.

### Network

- The firmware starts its own WiFi network `autoblink-XXXX` (the last 2 bytes of the MAC address). The default password is `autoblink123`. The web page is at http://192.168.1.1.
- The page settings "Join WiFi name" and "Join WiFi password" (indices 102 and 103) set an existing WiFi network. After a restart, the firmware joins it, and the own network stays on. The page is then also at the DHCP address of that network and at http://autoblink.local (mDNS). The iface field of the status shows the two addresses.
- When the firmware loses the joined network, it tries to join again after 30 s. After each failed try, the time doubles, up to 10 minutes, because each try scans all channels. The log shows each try.
- Do not join a 192.168.1.x WiFi network. The page can fail on one of the two networks. The firmware logs a warning for this case.
- The virtual CDJ uses the Ethernet port while it has an address. Without an Ethernet address, it uses the joined WiFi network. The Pro DJ Link sockets receive and send only on that interface (lwIP `SO_BINDTODEVICE`). Thus a joined WiFi network in the subnet of the Ethernet port gives no second copy of each beat. The firmware logs a warning when the two networks use one subnet.
- The XDJ-AZ sends Pro DJ Link on one interface only. It can also be on the joined WiFi network, for example with rekordbox. Then it can stop the packets on the cable after about 1 minute. When the two interfaces of the box have an address, the box examines the players. A deck leaves the tracker 15 s after its last packet. After 5 s more without a player, the virtual CDJ changes to the other interface.
- Without a player on the two interfaces, the time between two changes doubles up to 60 s. The log shows each change.
- A joined 192.168.1.x WiFi network has the subnet of the own WiFi network. Without the binding, lwIP can send the packets of the virtual CDJ on the own network. The players then send no CDJ status, and the page shows no track and no waveform. The dbserver connection also uses the interface of the virtual CDJ (`net::connect_on`). Without it, the connect times out.
- On WiFi, the access point sends a broadcast only after a DTIM beacon. Thus a beat packet arrives late by up to about 300 ms. The firmware switches off the WiFi power save. The MIDI clock corrects its phase with the earliest beat of the last "Phrase Lock Stable" bars, because a late packet is never early.
- Do not connect the Ethernet port to a 192.168.1.x network. The WiFi network uses that subnet, and the firmware can then send a reply out on the wrong port. The firmware logs a warning for this case.
- Ethernet gets its address from DHCP. Without a DHCP server (a direct cable to the XDJ-AZ), it takes a 169.254.x.x address.
- The firmware joins the network as a virtual CDJ, like the host. The page setting "Device Number" (index 2, default 7) sets the number. A change makes the virtual CDJ join again.
- The XDJ-AZ sends the track ID only to a virtual CDJ with number 7 or higher. Do not run the host and the firmware on one network with the same number.
- The players send their CDJ status (port 50002) to the virtual CDJ. The master flag in the status decides the master. Without a status, the master is the deck that sent the last beat, until that deck stops.
- The firmware reads the absolute position packets (port 50001) for the playhead and the track length.
- When a deck loads a track, the firmware connects to the dbserver of the source player. It gets the title, the artist, the phrases, the waveform preview, the cues and the beat grid. It fetches one track at a time.
- The `espressif/mdns` component comes from `[package.metadata.esp-idf-sys.extra_components]` in `esp32/Cargo.toml`. After a change to that table, run `cargo clean --release -p esp-idf-sys`. If you do not, the build uses the old ESP-IDF build and `esp_idf_svc::mdns` does not exist.
- The WebSocket handshake completes before the firmware can read the `Origin` header. When the Origin check fails, the firmware closes the connection and sends no data. It cannot reply with 403.
- The firmware sends the status and the beats to one page at a time. Another page gets `busy` and shows the button "Use this screen". The button sends `take_over`, and the firmware closes the connection of the old page with `httpd_sess_trigger_close`. The old page then connects again and gets `busy`.
- TCP keepalive (5 s idle, 3 probes at 5 s) closes the connection of a page that left the network in about 20 s. Without it, lwIP keeps the connection for minutes, and each status send to it waits up to 5 s.
- The web server has 6 sockets. When all are open, a new connection closes the socket that received data least recently. That socket is the live WebSocket. The page loads the waveforms of all decks at the same time. Do not set fewer sockets.
- The firmware serves the page with gzip, also to a client that does not ask for it. `esp32/build.rs` compresses `webui/index.html` at build time. Use `curl --compressed` to read the page.
- The firmware uses only IPv4 (`CONFIG_LWIP_IPV6=n`). The WiFi driver does not write its configuration to NVS (`CONFIG_ESP_WIFI_NVS_ENABLED=n`), because the firmware sets it at each start.
- A deck that sends no packet for 15 s leaves the tracker (`DECK_TIMEOUT_MS`), as on the host. A shorter time drops all track data after a short network fault.
- An error from the `ws_handler` closure does not close the socket. esp-idf-svc 0.53 logs it and returns `ESP_OK`. The handler wrapper in `webui.rs` closes the session with `httpd_sess_trigger_close`.

## Refactor Context (for future sessions)

- `crates/autoblink-core-prolink` holds a copy of the protocol constants and the packet parsing from `host/src/prolink/`. The host does not use the shared crate yet.
- The ESP32 MIDI output follows the host rules (`autoblink-core-midi`), with the phrase notes and CCs. It has no No Bass note, no MTC and no OSC.
- The ESP32 page has no mixer section. The XDJ-AZ sends the EQ and fader positions only as USB MIDI, and the classic ESP32 has no USB host.
- The ESP32 sends the Phrase 16 CC as the page help says (0 at each block start). The host never sends it, because its dedupe starts at 0.
- The DMX output no longer flashes channel 1 on each beat. The `dmx` thread renders the looks of the show (see "DMX show").
- The emulator has no `scenes` capability. The page hides the DMX Scenes panel there.
- The emulator uses old setting indices (for example 8 for Note Channel, which the host has at 9). It does not use `autoblink-core-midi` yet.
- A refactor plan exists at `.sisyphus/plans/multi-architecture-refactor-plan.md`
- `host/src/state.rs` and `host/src/main.rs` are the highest-coupling files in the host crate
- Adding a new architecture target should mirror the `esp32`/`esp32-emulator` split:
  - firmware crate + native emulator crate
  - both share protocol via a new shared crate (planned as `autoblink-core-prolink`)

## Constraints

- **until 2027-01-11** — Keep the host app alive under every fault. A task must restart after a panic (`runtime::supervise`). A blocking call must not run on a tokio worker. An FFI callback must not capture Rust data, because the vendored `rusty_link` callback setters keep a dangling pointer.
- **until 2027-01-25** — After each merge to `main`, build `main` on the local computer. Run `git checkout main && git pull`, then `sh scripts/check.sh --esp32 && cargo build --release --locked`. GitHub Actions runs only quick checks, because the account has a limit of free Actions minutes.

## graphify

This project has a graphify knowledge graph at graphify-out/.

Rules:
- Before answering architecture or codebase questions, read graphify-out/GRAPH_REPORT.md for god nodes and community structure
- If graphify-out/wiki/index.md exists, navigate it instead of reading raw files
- After modifying code files in this session, run `graphify update .` to keep the graph current (AST-only, no API cost)
