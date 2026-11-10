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
| `host` | Desktop app + shared library | `config`, `dmx`, `midi/*`, `prolink/*`, `state`, `web/*`, `settings`, `link` |
| `esp32` | Firmware for the WT32-ETH01, ESP-IDF with `std` | `net.rs` (Ethernet, WiFi AP, WiFi station), `ota.rs` (update from GitHub), `prolink.rs` (virtual CDJ, UDP receive), `tracks.rs` (dbserver fetch), `midi.rs` (UART MIDI out), `led.rs` (beat LED, status LED), `dmx.rs` (DMX OUT and DMX IN, feature `dmx`), `show.rs` (the `show` partition), `ofl.rs` (Open Fixture Library fetch), `artnet.rs` (Art-Net in, ArtPollReply), `sacn.rs` (sACN in, multicast joins), `ui.rs` (OLED pages, joystick), `webui.rs` (page, protocol v1, waveform route), `settings.rs` (NVS, including the night memory), `logring.rs` (log that stays through a restart, core dump report) |
| `crates/autoblink-core-dbserver` | Blocking dbserver client of the firmware, host-testable | `client.rs` (fetch), `protocol.rs` (wire format), `parse.rs` (PSSI, PWV4, PQTZ, cues), `types.rs` (song structure, phrase position) |
| `crates/autoblink-core-midi` | MIDI logic of the firmware, host-testable. The host uses `scenes.rs` too. | `clock.rs`, `mapper.rs`, `scenes.rs` (DMX scenes), `settings.rs` (host setting indices) |
| `crates/autoblink-core-dmx` | DMX looks of the firmware, host-testable | `show.rs` (show model and limits), `render.rs` (look into universe), `clock.rs` (beat clock), `effect.rs` (intensity and bar effects), `lights.rs` (engine), `artnet.rs` (ArtDmx, ArtPoll, ArtPollReply), `sacn.rs` (E1.31 parse), `source.rs` (outside source, sender lock, loss action), `merge.rs` (HTP and LTP merge), `auto.rs` (autopilot settings), `night.rs` (energy spot, night memory), `pilot.rs` (picks, pins, drop sequence), `moves.rs` (move shapes, room level) |
| `crates/autoblink-core-ui` | Logic of the box hardware of the firmware, host-testable | `dmx.rs` (DMX modes, HTP merge), `config.rs` (settings 110 to 121), `screen.rs` (OLED text), `joystick.rs` (input state machine), `menu.rs` (OLED pages, settings menu) |
| `esp32-emulator` | Native emulator | `main.rs`, `verify_stopped.rs` |

### Host library exports (`host/src/lib.rs`)

`pub mod app config dmx link logbuf midi osc prolink runtime settings state web`

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
- `midi/autopilot.rs` — the light autopilot. It runs the pilot under the mapper and writes `night.bin`.
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
- The System panel of the page (`#system`) sends `ota_check` and `ota_install`. Its Restart button sends `reboot`, and the firmware restarts after 1 s (`restart_soon`). The panel also has the WiFi rows of the settings (section `WiFi`, indices 100 to 103). The header shows the uptime from `system.uptime_s`. The host sends no uptime, so the header hides it. TLS to GitHub takes about 50 KB of heap. Version 1.19.0 has about 100 KB free with 1 track loaded and the page open.
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
- An app image must be smaller than the slot (0x1E0000 bytes). Version 1.21.0 uses 74 % of the slot (1 450 080 bytes). Version 1.30.0 uses 89 % (1 748 832 bytes). Version 1.31.0 uses 93 % (1 836 144 bytes). The release builds of esp-idf-sys already use `CONFIG_COMPILER_OPTIMIZATION_SIZE`. The memory trim after 1.32.3 uses 91 % (1 782 624 bytes). Silent assertions (`CONFIG_COMPILER_OPTIMIZATION_ASSERTIONS_SILENT`) remove 50 KB. An access point without WPA3 (`CONFIG_ESP_WIFI_SOFTAP_SAE_SUPPORT=n`) removes 5 KB. The next levers are Rust `opt-level = "z"` and a smaller page. A panic hook without the default hook of std removes only 352 bytes. The std backtrace code stays, because std and `anyhow` still use it. Full LTO, the gzip page and the common certificate bundle make it smaller.
- The OTA check uses the common certificate bundle (`CONFIG_MBEDTLS_CERTIFICATE_BUNDLE_DEFAULT_CMN`). The GitHub certificate chains end at DigiCert and USERTrust. If `ota_check` fails with a certificate error, set the full bundle again.

### Update the box over WiFi

- Update the box of the user over WiFi, not over USB. The USB port is not available.
- Increase the version in `Cargo.toml` and `esp32/Cargo.toml` (a hotfix step is enough). The box installs only a newer version.
- Run `sh scripts/ota-push.sh` (or `sh scripts/ota-push.sh 192.168.1.104`). The script puts the build into the prerelease `esp32-latest` and sends `ota_install` with `scripts/ws-command.py`. Then it waits for the new version.
- Each box that does a check gets that build. The next tagged release replaces it.
- OTA writes only the app. A change to the partition table, the bootloader or the bootloader settings of `sdkconfig.defaults` needs USB. Tell the user before such a change.
- If the new app restarts before `ota::mark_valid`, the bootloader starts the previous app. Then read `/log` for the crash.

### Web page without a flash

- The firmware serves the page from the app image. A change to `webui/index.html` therefore needs a flash, or an OTA update and a restart.
- To work on the page, serve it from the computer: `python3 -m http.server 8000 -d webui`.
- Open http://localhost:8000/index.html?box=autoblink.local. The page then uses the box for the WebSocket and the waveforms. A reload shows each change.
- The firmware accepts a page from `localhost` or `127.0.0.1` (`dev_origin` in `webui.rs`). The waveform route sends `Access-Control-Allow-Origin: *`.
- Flash the box only when the page is complete.

### DMX IN, OLED and joystick

- The `dmx` thread sends and receives on UART1. It has priority 6 on core 1 and an 8 KB stack. The thread writes a frame 24 ms after the last write (about 41 frames each second). The UART needs 22.7 ms for a frame and its break.
- Do not schedule the writes on a fixed grid. A late write and an on-time write can then come less than 22.7 ms apart. The driver starts a queued frame a few µs after the break (TX idle 0). This mark after break is too short, and a light can flicker. The log reports late writes once a minute.
- A frame ends at the data event with a receive timeout after a break event (`receive_timeout` of 1 character time). The thread reads exactly the bytes of each data event.
- The thread accepts only start code 0. Without a good frame for 1 s, the input has no signal and is all 0.
- The ESP32 UART stores a 0x00 byte for a break. `autoblink_core_ui::frame_channels` removes up to one 0x00 byte for each break event from the end of the frame. A frame must have 24 to 512 channels. After 8 rejected frames in a row, the thread clears the receive buffer.
- To examine DMX IN, turn on the setting "DMX IN", then connect DMX OUT to DMX IN. `curl http://autoblink.local/log` must show "DMX input: signal" with about 41 frames each second. The OLED and the Sync tab of the page show the same rate.
- IO36 has no internal pull resistor. Without the RS-485 module, noise on IO36 can look like a frame. Connect the module or a pull-up resistor.
- The setting "DMX IN" (112, default off, since 1.36.0) turns on the input. While it is off, the `dmx` thread ignores each frame on IO36. Thus an open IO36 cannot block Art-Net. The OLED menu has no row for it.
- The setting "DMX Channels" (113, since 1.37.0) sets the channels in each output frame, 1 to 512. The default 0 is Auto: the last channel of the show. When the outside frame has more slots, Auto uses its slots (since 1.39.0). A show without lights sends 512. DMX512-A allows a short frame, because the time from break to break stays above 1204 µs.
- A cheap light can flicker and get dim when each frame has 512 channels. A light of the owner did that. Its chip reads each byte of the frame, and its dimming loses time. With 24 channels at 40 frames each second, the light flickered much less, but it still flickered now and then.
- The setting "DMX Refresh" (114, since 1.38.0) is On change (the default) or 40 Hz. With On change, the thread sends a frame when a channel changes. It checks every 24 ms, and it sends a frame without a change again after 250 ms (`REPEAT`). The light of the owner was stable only with 5 channels at 4 frames each second.
- With On change, the 3 frames after a change also go out at once, 24 ms apart (`BURST`, since 1.39.0). In 1.38.0, a change went out once and then 4 times each second. Some lights read a value only from a few frames in a row. Such a light took about 2 s for a change from Lightkey.
- The break is 176 µs (`BREAK_BITS`, since 1.38.0). The UART TX idle time is 100 µs (`IDLE_BITS`). Thus the mark after break is at least 100 µs, also for a frame in the queue.
- The `dmx` thread installs the UART driver (since 1.38.0). Thus the UART interrupt runs on core 1. On core 0, the WiFi interrupts delayed it. A frame of 512 channels then had gaps of up to 30 µs between two slots.
- The slots of the outside frame count only in Merge and Pass, and not after a fade out. In Auto without an outside source, the output has no channel after the last light of the show. A desk that sends 512 slots gives 512 channels. Set "DMX Channels" to a number when a light flickers with 512 channels.
- The settings 110 to 121 are firmware settings in `autoblink_core_ui::config`. NVS keeps them as JSON under the key `box`. The firmware writes them 2 s after the last change.
- "DMX Mode" (110) is Off, Box only, Merge (the default) or Pass. The outside source is DMX IN, Art-Net or sACN (see "Network DMX in on the box"). Without an outside source, Merge and Pass send the output of the box.
- In Merge, a show with lights uses the HTP and LTP rules of `autoblink_core_dmx::Merge` (see "Network DMX in on the box"). A show without lights uses HTP for each channel.
- The OLED DMX page shows the DMX IN frames each second and the network state in the row "Net DMX" (Art-Net or sACN). The state is "live", "no signal", or "ignored" while DMX IN has a signal. The Main page shows "Net DMX live" when DMX IN has no signal and a network sender is live. The line "Out" shows the channel and the value of the last change in the DMX output. When one frame changes more than one channel, it shows the lowest channel.
- The status field `dmx` has `mode`, `in_fps` and `signal`, and the fields of the show engine (`layers`, `master`, `strobe_hold`, `artnet`). The firmware without the feature `dmx` sends no field `dmx`.
- The status field `dmx` also has `in_recent` and `out_recent` (since 1.34.0). Each holds up to 5 changed channels as (channel, value, age in ms), newest first, for 3 s (`autoblink_core_dmx::Changes`). One channel has one entry. In one frame, the lowest changed channel comes first. `in_recent` shows the frame of the outside source.
- The Sync tab shows `in_recent` and `out_recent` under DMX in and DMX out. The host sends no such fields, so its page hides the columns.
- The `ui` thread always starts. It has priority 3 on core 0. It looks for the OLED at address 0x3C and 0x3D every 5 s. The first failure logs the I2C addresses that answer. When the OLED answers, the thread draws 5 frames each second. After a write error, it initializes the OLED again before each frame, so that a display that lost its power comes back.
- The joystick is optional. Without it, the OLED shows the Main page.
- The OLED has seven pages: Main, Decks, Track, DMX, Scenes, Network and Settings (`menu::Page`). Left and right flip the pages. Each tilt flips one page.
- On the Settings page, up and down move the cursor, and a push starts an edit. In an edit, up and down change the shown value. A push applies it, and left or right cancel it.
- On the Scenes page, a push turns the DMX scene player on or off. The page shows the phrase, the row that is on, its preset, the next preset and the preset of a flash. A flash can be shorter than one frame, so the `ui` thread reads `SceneStatus::flash` at each tick and shows it for 300 ms. The firmware sets the scenes like `scenes_set` and sends the scenes message to the page (`webui::scenes_changed`).
- After 15 s without input, an edit stops and the Settings page goes back to Main. The other pages stay.
- The OLED fonts are ASCII only. The labels and values must not use other characters. `screen::ascii` replaces the other characters of a track title with "?".
- When no deck plays for 10 minutes, the OLED goes to the minimum brightness. An input of the joystick sets it back.
- ESP32 errata 3.11: a power-on of ADC1 pulls IO36 and IO39 low for about 80 ns. IO36 is DMX IN. The oneshot driver of ESP-IDF 5.5 keeps ADC1 on while the unit exists (`adc_oneshot.c`). Thus the `ui` thread reads the axes all the time. The switch (IO5) is a digital input.
- A change in the menu goes through `Settings::set`, like a page command. The menu sets DMX Mode (110) by its index in `DmxMode::ALL` and Rotate (120) with 0 or 1. Then `webui::settings_changed` sets a flag, and the `web-status` thread sends the settings message to the active page. A flag and not a channel: the `ui` thread has a lower priority than `web-status`.

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
- The drop guess (`autoblink_core_midi::guess::DropGuess`) gives a bucket (Intro, Groove, Build, Drop, Break, Outro) to a track. The scene engine uses it only for a track without rekordbox phrases. The engine reads it only when `SceneInput::phrase` is `None`. Phrases always win.
- The profile is the sum of the 6 PWV4 bytes of a column divided by 6. It is folded to at most 600 columns (`u8`) and smoothed with a moving average of 4 columns on each side. The page does not receive it.
- The thresholds come from the profile of the track: `hi` is the 70th percentile, `lo` is the 30th percentile, and `mid` is the median. Before the first column and after the last column at or above `mid`, the bucket is Intro or Outro. Drop is at or above `hi`. Break is at or below `lo`. Build is a column below `hi` where the next 2 % of the track reaches `hi`. The rest is Groove. A Drop or a Break stays while the energy is within 8 of its threshold (hysteresis). When `hi - lo` is below 24, the track gives only Intro, Groove and Outro. A loudly mastered track therefore gives no Drop guess.
- The engine fires the guessed bucket only at a block start: `phrase16 == 1` after a value other than 1. It fires only when the bucket differs from the last bucket that the guess fired for this track. A new track or a new master deck resets it, and the first tick of a new track or master deck also fires when `phrase16` is already 1. Play without a Play row fires the guess again at once. A master change to a deck without phrases keeps the old scene until the next block start, whereas a real phrase fires at once (the Master change row). A fire at a block start decides the coming block. Thus both feeders classify half a block ahead: they add 8 beats (`8 * 60 000 / bpm` ms) to the playhead (`DropGuess::bucket_ahead`). The history of the hysteresis (`prev_guess`) belongs to the pair of master deck and track. Each fetch logs the thresholds (`hi`, `lo`, `mid` and the first and last `mid` column), so that the owner can tune `MIN_SPREAD` and `HYSTERESIS`.
- The look-ahead of 8 beats covers 5 smoothed columns only while `len_ms * bpm` is at most 57.6e6 (about 7.5 minutes at 128 BPM). A longer track fires one block late. The upgrade is an offset of the larger of 8 beats and 5 columns, below 16 beats, or a Drop test on the unsmoothed profile.
- The page shows "(guess)" at the row of the current bucket when the status field `scene.guessed` is true.
- The guess is built once for each track, also for a track with phrases (see "Light autopilot"). The box builds it in the `dbserver` thread before it takes the lock (about 4 KB of heap for a short time, `tracks.rs`). The host builds it in `set_overview` (`host/src/state/dj_state.rs`). Without a preview of 32 columns, there is no guess.

### DMX show

- The `show` partition holds the DMX show (`show, data, undefined, 0x3F0000, 0x10000` in `esp32/partitions.csv`). Its content is JSON. A u32 length comes first. The firmware writes the length last, so a write that stops early leaves no show. The box then starts with an empty show until the page saves again.
- OTA does not change the partition table. A table from 1.18 to 1.23 has no `show` partition, but the flash at 0x3F0000 is free in it. The firmware then adds the partition to the list in RAM at each start (`esp_partition_register_external` in `show.rs`).
- ESP-IDF refuses the region when it overlaps a partition of the table. The box then has no show storage, and `POST /show` answers 500. Flash such a box over USB.
- Do not use the subtype `0x40` for the partition. It panics espflash 4.6.0. The table uses the named subtype `undefined`.
- `GET /show` serializes the running show. `POST /show` is a CORS simple request with an Origin check. It answers 413 when the body is larger than 60 000 bytes. It answers 503 when the body is larger than half of the largest free heap block. The reader bounds the lists of the show while it reads them.
- The firmware sends the capability `dmx` and the status field `dmx`. The page sends the WebSocket commands `dmx_live`, `dmx_fog` and `dmx_aim`.
- The `dmx` thread has priority 6 on core 1. Its stack is 8 KB (since 1.39.1). On 1.32.3, it used 4096 bytes with DMX IN. On 1.39.0, it used 6208 bytes with Art-Net and ArtPoll from Lightkey. Measure it with `/mem` again while a fade out runs.
- Three senders use one queue of depth 32 to the `dmx` thread: `midi`, `prolink-rx` and the HTTP task (`dmx_fog`). A full queue drops a message. A dropped Off keeps its layer on until the next Off of that preset. An On does not remove the layer. A stop only hides it.
- The DMX output starts before WiFi. Thus the OTA maintenance start sends the idle look.
- The strobe hold ends when the active page closes or a send to it fails. A take-over of another page also ends it.
- The HTTP task has 8 KB of stack. Nobody has measured it under `POST /show`.
- A fixture has the channel kinds `pan`, `pan_fine`, `tilt` and `tilt_fine`. The generic types are `moving_head` (pan, tilt, dimmer, red, green, blue) and `moving_head_16` (with the fine channels). A head with only coarse channels gets the high byte of the 16-bit value.
- The patch card has a channel list, for example `dimmer 120 strobe 0 0`. A number from 0 to 255 is a `fixed` channel. Use it for a light that no type fits, such as a light with a colour macro channel. The page does not save a list with an unknown word.
- The show holds up to 8 positions (`MAX_POSITIONS`, names of 16 characters). A position holds aims, and an aim is a 16-bit pan and tilt for one fixture. A head without an aim in the position sits at 32768 on each axis.
- A look row can have `position` (an index) and `shape`. The shapes are `circle`, `figure8` and `sweep`. A shape has `beats` (1 to 32) and `size` (0 to 255). The amplitude is `size` x 64 units of the 16-bit range, so 255 is a quarter of the range.
- The offset of a shape comes from `t = (i + phase) / beats + k / n`, where `i` is the beat index. `i` counts the beats from a bar line: `i = (beat in bar - 1 + 4 * bar number) mod beats`. A period of 4, 8 or 16 beats therefore starts on beat 1. Without bar information, `i` is the beat count mod beats. The effects with `Every::Bar` use the same fallback: the beat in the bar is the count mod 4. Fixture `k` of `n` in the group is `k / n` of a cycle ahead of the first one. Circle is (cos, sin) of 2 pi t. Figure8 is (sin, sin(2a) / 2). Sweep moves pan only, as sin. The sum of position and offset is clamped to 0 to 65535. It does not wrap. Without a beat, the offset is zero.
- The WebSocket command `dmx_aim` (fixture below 32, pan, tilt) holds the pan and tilt of one head for `AIM_MS` (10 s, `crates/autoblink-core-dmx/src/lights.rs`). The aim overrides every layer and the idle look. The queue drops it when full. The page sends about 20 aims each second and always sends the last value.
- A fade between two looks blends the coarse byte and the fine byte alone. A pan across a byte border can jump for a moment. The `ponytail:` comment in `lights.rs` names the fix: blend the 16-bit pairs.

### Light autopilot

- The autopilot (`autoblink_core_dmx::Pilot`) picks a console program and the moves of the room at each phrase change. The settings are `show.auto` (`auto.rs`). The page edits them in the Autopilot panel, with the views A, B and C and the move editor.
- The energy spot is a number from 0 to 100. `raw_energy` makes a value from 0 to 255 from the BPM and the track energy of the drop guess. 90 BPM gives 0, and 150 BPM gives 255. The two parts have the same weight. Without a track energy, the BPM alone counts.
- The spot is the percentile of the raw energy in the histogram of this night. The histogram has 32 bins and takes one sample each second while a deck plays. The prior of past nights counts as at most 600 samples (`PRIOR_WEIGHT`). Thus the warm-up of a new night reads as calm.
- The tiers are Calm (spot below 34, `CALM_BELOW`), Groove, and Peak (spot 80 and up, `PEAK_FROM`). The tier changes only when the spot crosses a border by more than 4.
- The night memory is `Night::to_bytes`. It has one version byte, then the two histograms (`NIGHT_BYTES`). The box keeps it in the NVS blob with the key `night`. The host keeps it in `night.bin` in the folder of the config file.
- The `midi` thread (box) and the mapper (host) write the night memory every 5 minutes, and only when it changed. The box writes it in the main loop, because the `midi` thread must not wait for a flash write. A write can make the MIDI clock stutter, like a show save. Nobody has measured it on a box.
- A deck that plays after `NIGHT_GAP_MS` (2 hours) without play starts a new night. The pilot folds the old night into the prior and clears the pick history. A bad blob or a bad file gives a new night.
- After a software restart, a panic or a watchdog, the box continues the stored night. After a power-on or a brownout, the box starts a new night (`Night::start_new_night`). A club cuts the power at the close.
- The host uses the age of `night.bin`. When the file is older than `NIGHT_GAP_MS`, the host starts a new night at the start (`config::load_night`).
- A phrase change is only a bucket event (Intro, Groove, Build, Drop, Break, Outro). The events Beat, Downbeat, Phrase change and Fill do not count. Without a bucket event for 64 beats (`REPICK_BEATS`), the pilot picks again.
- At a phrase change the pilot picks one program of the tier. It skips the last two picks, then takes the program with the lowest play count. With two candidates, it skips only the last pick. It breaks a tie at random. When the tier has no program, it takes the program with the nearest spot.
- The pilot picks one move for each target (the room, a group or a light). A move belongs to a tier by its spot. The pilot handles a target at its first move in the tier, with the same rule as the programs.
- The moves need a beat. `BeatClock::now` gives no beat 2 s after the last beat. Then the moves stop. The fixtures show the base, and the strobe is 0. The Room moves do not change the room level.
- The home of a moving head is its aim in the first position of the show (`show.positions[0]`). A head without an aim there has its home at 32768 on each axis. The pan and tilt moves of the autopilot run around the home.
- The drop sequence starts with the build. It starts at most 64 beats (`BUILD_BEATS`) before a known drop. At the build start, the console fixtures go to the dip. Their level climbs back to full at the drop. In its second half, the moves run at double speed.
- Before the drop, the blackout of the room runs for the set time. At the drop, the pilot picks a Peak program and hits, with a strobe once for each cooldown (`strobe_cooldown_s`).
- The drop time comes from the song structure (`pilot_facts` on the box). It is the start of the next Drop phrase. Without a known drop time, the build starts at a Build event and has no blackout.
- A build ends at a stop or at a track change. It also ends at a pin to Calm or Groove, or 2 s after the drop time. It also ends when the known drop time goes away. A build without a drop time ends at the next bucket event that is not a Build or a Drop.
- A pin (`PIN_MS`, 30 minutes) holds a tier. A pin to Calm or Groove turns the drop sequence off. A pin to Peak keeps it on. The tier changes at the next phrase change. The pin `auto` clears it.
- One driver at a time drives the lights. While `show.auto.on` is true, the scene engine sends no note to the console and no message to the `dmx` thread. When the autopilot turns on, the `midi` thread and the host mapper call `Scenes::release_all` once (the take-over edge). Without it, the held presets stay on.
- The pilot ticks all the time, also while `auto.on` is false. Thus the night memory learns while the autopilot is off. The actions go out only while `auto.on` is true. At the turn-on, the thread sends the last picks (program, moves, tier) again.
- The `dmx` thread applies the room level after the merge (`apply_room`). Thus the dip and the blackout also reach the light of DMX IN, Art-Net and sACN. Pass with an outside source is the exception: only the blackout of the page dims it. A fixture with `console: true` gets the console level multiplied by the level of the room. Other fixtures get the room level.
- A light channel is the Dimmer. A fixture without a Dimmer uses its colour channels (Red, Green, Blue, White, Amber and Uv). The Strobe channel goes to 0 only at level 0, because its value is a rate. Pan, Tilt, Fog and Fixed channels never change.
- A console fixture needs its real channels in the show. The box scales its light channels in the merged frame, so a wrong map dims the wrong channel. The pilot sends the program with `Out::Program` (MIDI program change on the channel `auto.channel`).
- The WebSocket commands are `auto_pin` (`tier`: `auto`, `calm`, `groove` or `peak`), `dmx_blackout` (`on`) and `auto_try`. The field `move` of `auto_try` is an index below `MAX_MOVES` (24). `auto_try` shows one move for 8 bars, also while the autopilot is off. The engine then shows the base and the move, and after that the manual frame again. The `dmx` queue drops it when full.
- The status field `auto` has `on`, `tier`, `spot`, `pinned`, `program`, `moves` (a bit mask) and `drop_in_beats`. The field `dmx.blackout` is true while the blackout runs. The firmware sends the capability `auto`. The firmware without the feature `dmx` sends none of them.
- `dmx_blackout` is a flag in `DmxLive`, like the strobe hold. The release of the strobe hold also releases the blackout (`release_strobe` in `webui.rs`, `StrobeGuard` on the host). A take-over of another page and a failed send do the same.
- The page has four tabs: `#live`, `#lights`, `#sync` and `#system` in the URL. The default is `#live`. The old `#setup` opens Lights. A panel names its tab in `data-mode`, and the page hides the panels of the other tabs. `navSync` hides a tab without a visible panel.
- Live has the Master panel and the decks. The sticky strip with MASTER, STROBE and FOG shows in each tab.
- Lights has the driver bar "Who drives the lights", then Autopilot, Scenes and Box DMX, in this order. Box DMX (`p-dmx`) starts closed. Sync has the input, the output port, the MIDI clock, notes, CC and timecode, OSC, and the output status. System has the firmware, the WiFi, Diagnostics and the Display rows.
- `onSettings` puts a settings group into a tab by its section (`SECTION_HOME`). The group `DMX` goes to Box DMX › Desk in. The group `Display` goes to System. The other groups go to Sync.
- "Who drives the lights" has one switch: Off, Manual rules or Autopilot. Off sets `scenes.enabled` and `show.auto.on` to false. Manual rules sets `scenes.enabled` to true and `show.auto.on` to false. Autopilot sets `show.auto.on` to true and keeps `scenes.enabled`.
- The driver bar is not a panel and does not close. The scene note channel and the first note are in Scenes. The program change channel is in the settings row of Autopilot.
- `#phone` shows the phone screen alone: the tier buttons, BLACKOUT, STROBE and FOG. The link "Phone" in the header and the button in the Autopilot header open it. Escape or Exit closes it.
- Each panel is a `<details>`. The page keeps its open state in `localStorage` (`autoblink.open.<id>`). A button in a panel header does not open or close the panel. Escape closes the top open item. The order is the hold popover, the look editor and the autopilot form. Then come the library box, the Diagnostics output and the phone screen.
- The autopilot has no PIN since 1.33.0. A stored show with the old field `pin` still loads, because serde ignores an unknown field.
- On a computer, hold B for the blackout and hold S for the strobe. F starts the fog, and 1 to 4 pin a tier.
- The blackout runs while the user holds the button. The page releases it on pointer up, pointer cancel and pointer leave. It also releases it on `pagehide` and when the page hides. It releases only a blackout that it holds.
- After a reconnect, the page sends a held blackout again. The server released the old blackout with the old socket.
- The box builds the drop guess for every track with a preview, not only for tracks without phrases. The autopilot needs its energy. With 4 decks, the guess takes about 2.5 KB more heap. The scene engine still uses the guess only without phrases.
- The `midi` thread has 7 KB of stack with the feature `dmx` since 1.31.0 (6 KB before and without `dmx`). This size is an estimate. Measure it with `/mem` on a box.
- The host runs the same pilot in `host/src/midi/autopilot.rs`, under the mapper. The pilot runs only when the DMX engine runs. The `dmx` engine of the host applies the room level before it builds the ArtDmx frame.
- The mapper and the DMX engine of the host use one clock (`runtime::epoch`). Thus the build times of the pilot are correct in the engine, also after a restart of a task.

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
- A fetch does not start below 68 KB free heap or a 20 KB largest block (`MIN_FREE`, `MIN_BLOCK`). The firmware cuts off a reply larger than 24 KB (`MAX_BODY`).
- On 1.32.1, a fetch with a small reply needed 50 KB of heap, and a reply of 10 KB needed 57 KB. Each fetch logs the heap that it needed.
- The reply buffer grows in steps of 4 KB. It does not grow when the free heap is below 16 KB (`MIN_FREE_GROW` and one step). The route then answers 503. On 1.32.2, the low point of a fetch was 15 to 27 KB, and the box did not restart.
- After two fetches, the largest free block went from 72 KB to 31 KB and stayed there. Nobody knows the cause yet.
- The fetch runs on the `ofl` thread (12 KB stack, priority 5, from `crate::spawn`). The HTTP handler joins the thread. Thus a fetch blocks the HTTP task. The page gets no status and no beats for up to tens of seconds. The MIDI clock and the DMX output keep running.
- While `ofl::busy()` is true, the status thread sends no status, as during an OTA check. The page disables the library search while the strobe hold is on.
- The certificate chain of OFL ends at ISRG Root X1. The common certificate bundle has it through the X2 cross-certificate.
- The box needs internet: a joined WiFi network (indices 102 and 103), or an Ethernet network with a router.
- `Fixture.ofl` holds `<key>#<mode>`, for example `eurolite/led-bar-6-qcl-rgbw#4-channel`. The core limits it to 96 characters.
- The page maps the OFL channels to the kinds of the show. A pixel channel gets the kind of its template channel, so each pixel shows the same colour. A channel that no template describes becomes a fixed placeholder channel, so that the footprint stays right. A Pan or Tilt channel maps to `pan` or `tilt`, and its fine alias maps to `pan_fine` or `tilt_fine`. A mode with an unknown pixel count, or with more than 32 channels, the page does not add.
- Nobody has measured the heap after many fetches on a box. Run `/mem` after ten fetches.

### Network DMX in on the box (Art-Net and sACN)

- The box receives Art-Net on UDP port 6454 and sACN (E1.31) on UDP port 5568, on all interfaces (`esp32/src/artnet.rs`, `esp32/src/sacn.rs`). Neither has a thread. The `dmx` thread reads the two sockets without a wait on each loop pass (at most 16 packets each).
- The setting "Art-Net Universe" (117, default 0) is the port-address that the box receives. The setting "sACN Universe" (118, default 1) is the sACN universe. The box ignores a frame of another universe. The log counts them.
- sACN comes as multicast to 239.255.hi.lo of the universe, or as unicast. On WiFi, set the desk to unicast. A WiFi access point sends multicast only after a DTIM beacon and without a retry. Once a second, the `dmx` thread joins the group on each interface with an address (`sacn::Groups`). A change of an address or of the universe changes the joins.
- The sACN reader accepts only start code 0 (`autoblink_core_dmx::parse_sacn`). It ignores preview data, the sync packets and the 0xDD priority packets. A packet with Stream_Terminated starts the loss action at once.
- `main` opens the sockets after the network and the Pro DJ Link threads start. It puts them into `State::artnet` and `State::sacn`. The OTA maintenance start does not reach that line, so it has no network DMX.
- The permanent heap for network DMX is about 3 KB: the receive buffers, the source frame and the two sockets.

#### The outside source

- `autoblink_core_dmx::Source` holds the outside frame of DMX IN, Art-Net and sACN. It follows one sender at a time, so two desks cannot fight.
- A sender is the protocol, the IPv4 address and the sACN CID. Another sender takes over when the current sender is silent for 2.5 s (`SWITCH_MS`, the loss timeout of E1.31). A sender with a higher priority takes over at once.
- The priorities are: DMX IN 201, the sACN priority of the packet (0 to 200), and Art-Net 100. Thus DMX IN wins over the network while it has a signal.
- The source drops a frame that is up to 19 steps behind the last sequence number (E1.31 6.7.2). An Art-Net sequence of 0 is off. After 2.5 s without a frame or after the end of a stream, the next frame starts a new sequence.
- When the sender stops, the source holds the last frame for "Signal Loss Time" (116, 5 to 60 s, default 10). A desk with a static look can send a frame only each 4 s. A shorter time then makes the light pulse. Then "Signal Loss" (115) decides. Hold (the default) keeps the frame for ever. Fade out fades the light to 0 in 2 s (`FADE_MS`).
- The fade dims the light channels and the Fog channels of the lights of the show. It also dims each channel that no light patches. The pan, tilt and fixed channels of the lights keep their value (`autoblink_core_dmx::dim`).
- Before the first frame, the box has no outside source. Then Merge and Pass send the output of the box. After the first frame, the source stays until a restart. A restart loses the held frame.
- A change of "DMX Mode" clears the source. "DMX IN" off clears it while DMX IN is the source. Thus a noise frame on IO36 does not stay.
- In Merge, the source claims its non-zero LTP channels again after the loss time (`Merge::rearm`). A new sender also claims them.
- The status field `dmx.source` is `dmx_in`, `artnet`, `sacn` or null. The status field `dmx.hold` is `live`, `held`, `fading` or `black`. The field `dmx.artnet` is true while a network sender is live. The source is live for 2.5 s after its last frame (`LIVE_MS`).
- The log shows each change to fading or black, and each new live sender. Once a minute while a source sends, the log shows its counters. The counters are the frames, the late frames and the frames of other senders and universes. They also give the switches, the holds and the longest gap.

#### Discovery

- The box answers an ArtPoll with an Art-Net 4 ArtPollReply (`autoblink_core_dmx::build_artpoll_reply`). The reply gives the address of the interface in the subnet of the desk. The Ethernet port comes first, then the joined WiFi network, then the own WiFi network.
- A desk can be in no subnet of the box, for example at 2.x.x.x. Then each interface with an address sends a broadcast reply.
- The short name is `AUTOBLINK-XXXX`, the same suffix as the WiFi network. The reply gives the universe in Net, Sub and SwOut. GoodOutputA shows that the output sends (bit 7), merges (bit 3) or takes sACN (bit 0). Status1 says that the web page sets the port-address. GoodOutputB shows On change or 40 Hz. Status3 shows the failsafe: hold, or zero for Fade out.
- A targeted ArtPoll (flags bit 5) gets a reply only when the universe of the box is in its range. When a desk sets flags bit 1, the box sends it a reply after each change of the reply fields.
- The node report counts the replies (`#0001 [nnnn] AUTOBLINK OK`).
- The box does not take ArtAddress. A desk cannot rename the box or set its universe.
- The reply goes from port 6454 to port 6454 of the desk. For the send, the receive socket is bound to the interface. A desk or a firewall can drop a reply from another source port.
- When the joined WiFi network uses 192.168.1.x, a desk on the own WiFi network gets the address of the joined network. Such a desk must send to 192.168.1.1 by hand.
- The log shows the first reply and its address.

#### Merge

- "DMX Mode" sets the merge (see "DMX IN, OLED and joystick").
- The merge rule is in `autoblink_core_dmx::merge`. The channels of kind Dimmer, Red, Green, Blue, White, Amber, Uv and Strobe use HTP (the higher value wins). All other channels use LTP (the source that changed the channel last wins). The channels that no light patches and the Fog channel are LTP too. When the first frame of a source comes, it claims every non-zero channel.
- Pass sends the outside source without the effects and the room level of the box. Only the blackout of the page dims it.

### DMX engine on the host

- The `[dmx]` section of the config file has `artnet_target` (an IPv4 address or IP:port, default port 6454) and `universe` (0 to 32767, default 0). Without `artnet_target`, or with a bad value, the engine does not start.
- The engine task (`host/src/dmx.rs`) runs under `runtime::supervise`. It renders about every 25 ms and sends ArtDmx to the target.
- The task is silent while it sees a box. A box is a Pro DJ Link device with the name `autoblink`. Its device number differs from the number of the host. Two hosts with the default device name therefore silence each other. Change `device_name` on one of them. With the same device number as the host, the host does not find the box and keeps sending. The host logs a warning at start when `artnet_target` is set and its device number is the default 7.
- The host keeps a copy of the show in `show.json` in the folder of the config file. `GET /show` and `POST /show` work as on the box, with the same 60 000-byte limit.
- The host takes `dmx_live`, `dmx_fog` and `dmx_aim` from the page. When the page that holds the strobe closes, the host releases the hold (`StrobeGuard` in `web/mod.rs`).
- The status field `box` gives the IP address of the box, or null. The status field `dmx` comes from the engine.
- When a box is on the network, the host page loads and saves the show of the box (`GET` and `POST` to the box). Open the host page at `127.0.0.1` or `localhost`, so that the box accepts the POST (`dev_origin`). The preset names on the host page are the names of the host. The page hides the Preview button.

### Debug logs

- Run `curl http://autoblink.local/log` to read the log over WiFi. The buttons Log and Memory in the System tab of the page show `/log` and `/mem`.
- Run `curl http://autoblink.local/mem` to read the heap and the free stack of each task. The free stack is the low point since the start. Set a stack size from it, and keep a margin.
- `/mem` also shows the configuration of GPIO4 (DMX OUT) (since 1.35.1). `GPIO Matrix SigOut ID: 17` is UART1 TX.
- `GET /scope` records each edge of GPIO4 (DMX OUT) for 60 ms (since 1.36.2). A task with priority 24 on core 0 reads the pad. It blocks the WiFi task for the capture time. Interrupts can stop the loop for some µs, and the capture marks each stop. `scripts/dmx-decode.py` decodes the break, the mark after break and the slots.
- `GET /dmxtune` changes the timing of the DMX output at runtime (since 1.36.3), for example `/dmxtune?break=44&idle=25&period=0&slots=64`. `break` and `idle` are in bit times of 4 µs. `period` is the time between two writes in µs, and 0 sends the frames back to back. `slots` is the number of channels in a frame (1 to 512), and 0 uses the setting "DMX Channels". `gap` is the idle time between two slots in µs. `baud` is the baud rate, 240 000 to 260 000. A restart sets the defaults again.
- Start a thread with `crate::spawn`. It gives the FreeRTOS task the thread name. Without it, `/mem` shows each Rust thread as "pthread".
- `crate::spawn` gives priority 5 on the two cores, the same as the HTTP server task. `crate::spawn_rt` sets a priority and a core. The WiFi task runs on core 0. The `midi` thread has priority 10 on core 1. The `prolink-rx` thread has priority 8 on core 1.
- `prolink-rx` receives on the three Pro DJ Link sockets with one `poll` loop and one buffer. The task watchdog watches it, the `midi` thread and the `dmx` thread (since 1.39.0). The loop of the `dmx` thread waits at most 1 s, also with a long `/dmxtune` period. After a receive error, the thread sleeps for 10 ms, so that the idle task can run.
- `prolink-rx` also runs the virtual CDJ (`VirtualCdj` in `prolink.rs`). Between two receives, it does the due step. A join sends a packet every 300 ms. After the join, the virtual CDJ sends the keepalive and the status packets every 1.5 s. The poll waits until the next step, and at most 1 s. The virtual CDJ has no thread.
- Each stack comes from the heap. Each size keeps 1.5 KB or more above the low point of `/mem`. Measure with `/mem` again after a change that adds work to a thread. The memory trim after 1.32.3 sets the sizes in `sdkconfig.defaults` and in the `spawn` calls. The sizes in bytes are `ui` 6656, `dbserver` 5120, `dmx` 8192, `prolink-rx` 5632, `main` 6144, `mdns` 3072 and IDLE 2176.
- The LEDs have no thread. An esp_timer of 5 ms in the `esp_timer` task sets IO32 and IO33 (`led.rs`). The `prolink-rx` thread writes the beat into atomics, and the main loop writes the blink code each second. The `esp_timer` task has 2560 bytes of stack. Measure it with `/mem` after an hour of beats.
- Put a large struct on the heap (`Box`), not on the stack of a thread. The OLED display struct holds its 1 KB frame buffer. On the 5 KB stack of `ui`, it overflowed the stack, and the box restarted in `malloc` with the address 0xffffffff.
- The status message and the settings message are `Serialize` structs. Do not build them as a `serde_json::Value` tree. A tree has a heap block for each field, and the blocks fragment the heap.
- The firmware keeps 600 columns of the waveform preview (`autoblink_core_dbserver::PREVIEW_COLUMNS`). The players send 1200.
- The dbserver client reads the PWV4 preview and the PQTZ beat grid as a stream (`read_message_with`). It keeps only 600 preview columns and the beats of the phrase starts, the fill-ins and the end. A buffered grid and its parsed copy needed up to 72 KB for one fetch. Without phrases, the client does not request the beat grid.
- The fetch does not start below 24 KB free heap or an 8 KB largest block. The page then shows "not enough memory for the track data", and the fetch starts again after 3 s.
- Do not use `std::io::copy` in a firmware thread. It puts an 8 KB buffer on the stack.
- Do not use `std::sync::mpsc` when the receiver has a higher priority than the sender on the same core. The receiver can spin on a half-written slot, and the sender cannot run. Use `esp_idf_svc::hal::task::queue::Queue` (see `midi.rs`). Firmware 1.21.0 restarted from the task watchdog on the `midi` thread because of this.
- The log is in RTC slow memory: two rings of 4000 bytes. A panic, a watchdog or a software restart keeps them. A power-on clears them.
- Each start writes into the other ring. Thus the heap keeps no copy of the lines of the previous start. The rings use 8 KB of RTC slow memory with the other components. The linker stops when they do not fit.
- The text has three parts: the last crash, the lines of the previous start and the lines of this start. `GET /log` sends the parts one after the other (`logring::write_text`). It copies one ring at a time, so it needs about 4 KB of heap.
- The RTC slow memory has about 120 bytes free. A larger ring or another RTC variable stops the linker.
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
- The brownout detector restarts the box below 2.43 V (`CONFIG_ESP_BROWNOUT_DET`, level 0). The setting is app-only, so OTA can change it.

### WT32-ETH01 pins

The wiring diagram is `docs/wiring.svg`. It gives the board label first and the GPIO number in parentheses, for example "RXD (IO5)".

| Pin | Use |
|---|---|
| GPIO16 | Switches on the 50 MHz oscillator of the PHY. Set it high before the Ethernet driver starts. |
| GPIO0 | RMII clock input from the oscillator |
| GPIO18, 19, 21, 22, 23, 25, 26, 27 | RMII and MDIO to the LAN8720 (PHY address 1) |
| IO32 | Beat LED. It pulses for 30 ms on each beat of the master deck. |
| IO33 | Status LED, with the label 485_EN on the board. While the master deck plays, it pulses for 120 ms on beat 1 and for 25 ms on the other beats. When no beat arrives for 2 s, it shows a blink code (see below). |
| IO17 | MIDI out (UART2 TX, 31250 baud). TRS-A: IO17 → 10 Ω → tip, 3V3 → 33 Ω → ring, GND → sleeve. |
| IO2 | UART2 RX. It has no wire, but the UART driver needs a receive pin. |
| IO4 | DMX out (UART1 TX, 250 kbaud) to TXD of an RS485 module (V2.03). EN of that module goes to 3V3, so that it always sends. The feature `dmx` is on by default. |
| IO36 | DMX IN (UART1 RX, 250 kbaud) from RXD of a second RS485 module. EN of that module goes to GND, so that it always receives. |
| IO14, IO15 | I2C0 SCL and SDA for an SSD1306 or SSD1315 OLED (address 0x3C, 400 kHz). The OLED module has the pull-up resistors. |
| IO39 | Joystick VRx (ADC1 channel 3). |
| IO35 | Joystick VRy (ADC1 channel 7). |
| IO5 | Joystick switch, internal pull-up, low when pushed. The label on the board is RXD. It is a strapping pin, and the pull-up keeps its default high level. |

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

Connect the VCC of the OLED, the +5V pin of the joystick and the VCC of the RS-485 modules to 3V3. At 5 V, the joystick puts up to 5 V on an ADC pin, and a module puts 5 V on IO36.

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
- Only the active page controls the box. The firmware answers `busy` to each command from another page, except `take_over`. The firmware releases a blackout or a strobe hold only when the active page closes. Thus a hold from another page could stay on. The page hides the phone screen while it is busy.
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
- The DMX output (feature `dmx`, on by default) renders the looks of the show (see "DMX show"). A show without lights keeps the beat flash of "DMX Beat Channel" (index 111). "DMX Mode" (index 110) merges the output with DMX IN, Art-Net or sACN. The two rows are firmware settings (`autoblink_core_ui::config`), and NVS keeps them under the key `box`.
- The emulator has no `scenes` capability. The page hides the Scenes panel there.
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
