# AGENTS.md — xdj-midi

## Workspace

- **3 crates**: `xdj-clock-host` (desktop app), `xdj-clock-esp32` (firmware), `xdj-clock-esp32-emulator` (native emulator)
- The root workspace excludes `esp32`. The firmware has its own `Cargo.lock`, toolchain (`esp`) and target directory.
- **Root `Cargo.toml`** defines workspace members and patches `rusty_link` from `vendor/rusty_link`
- Package version is set once at workspace level (`[workspace.package]`)

## Developer Commands

```sh
# Host (desktop app). A bare `cargo run` starts it (`default-run` in host/Cargo.toml).
# The web UI is at http://127.0.0.1:11111 (next free port when in use).
cargo run
cargo run -p xdj-clock-host
cargo run -p xdj-clock-host -- --web-bind 127.0.0.1:11111
cargo run -p xdj-clock-host -- --list-midi
cargo run -p xdj-clock-host -- --list-interfaces
cargo build -p xdj-clock-host --release

# ESP32 firmware for the WT32-ETH01 (see "ESP32 Build Requirements")
. ~/export-esp.sh
cd esp32 && cargo build --release
cd esp32 && cargo run --release   # flashes the board and opens the serial monitor

# Emulator (Web dashboard at http://localhost:8080)
cargo run -p xdj-clock-esp32-emulator

# Tests (all crates)
cargo test --locked

# Single workspace member
cargo test -p xdj-clock-host
cargo build -p xdj-clock-esp32-emulator
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
- Linux ARM64 (Raspberry Pi) → `.tar.gz` archive, built on the native `ubuntu-24.04-arm` runner. The Linux archives include `contrib/xdj-clock.service`.
- macOS ARM64 → `.tar.gz` archive
- ESP32 firmware → `esp32-firmware.tar.gz`

## Architecture Map

| Crate | Role | Key modules |
|---|---|---|
| `host` | Desktop app + shared library | `config`, `midi/*`, `prolink/*`, `state`, `web/*`, `settings`, `link` |
| `esp32` | Firmware for the WT32-ETH01, ESP-IDF with `std` | `net.rs` (Ethernet, WiFi AP), `prolink.rs` (virtual CDJ, UDP receive), `tracks.rs` (dbserver fetch), `midi.rs` (UART MIDI out), `dmx.rs` (feature `dmx`), `webui.rs` (page, protocol v1, waveform route), `settings.rs` (NVS) |
| `crates/xdj-core-dbserver` | Blocking dbserver client of the firmware, host-testable | `client.rs` (fetch), `protocol.rs` (wire format), `parse.rs` (PSSI, PWV4, PQTZ, cues), `types.rs` (song structure, phrase position) |
| `crates/xdj-core-midi` | MIDI logic of the firmware, host-testable | `clock.rs`, `mapper.rs`, `settings.rs` (host setting indices) |
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
- `webui/index.html` (repo root) — the one page that the host, the emulator and the ESP32 serve
- `runtime/mod.rs` — `supervise` restarts each long-running task after a panic. The MIDI clock, the MTC, the mapper and the MIDI output worker run on the `midi-rt` thread, a separate single-thread runtime.

## Platform-Specific Code

**Windows vs Unix socket differences**: `set_reuse_port(true)` is Unix-only. Files affected:

- `host/src/prolink/discovery.rs`
- `host/src/prolink/beat_listener.rs`
- `host/src/prolink/status_listener.rs`
- `host/src/prolink/virtual_cdj.rs`

All have `#[cfg(not(windows))]` guards around reuse_port calls.

**ESP32 vs host**: The ESP32 uses `std::net::UdpSocket` and `TcpStream` on the ESP-IDF network stack. It has no `tokio` runtime; each task is a `std::thread`. It parses packets with `xdj-core-prolink`, and `xdj_core_prolink::Tracker` keeps the deck and master state. It fetches the track data with `xdj-core-dbserver`, one track after the other. The host does not use these crates yet. It has its own async copy of the dbserver client in `host/src/prolink/metadata.rs`.

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
- ESP32 CI (`esp32-ci.yml`) builds the firmware when a change touches `esp32/`, `crates/` or `webui/`. No test runs on the device.
- Coverage gaps: packet parsing, state machine transitions, BPM smoothing, MIDI clock transport

## ESP32 Build Requirements

The firmware is for the WT32-ETH01 board: a classic ESP32 (Xtensa) with a LAN8720 Ethernet PHY.

1. Install the toolchain once: `espup install`, then `cargo install espflash ldproxy`.
2. In each new shell, run `. ~/export-esp.sh`.
3. Build in `esp32/`: `cargo build --release`. The first build downloads ESP-IDF v5.5.3 to `~/.espressif`.
4. Connect the USB-C programming dock. Run `cargo run --release` in `esp32/` to flash the board.
5. If espflash cannot connect, hold BOOT, push RST, then release BOOT.

The output is `esp32/target/xtensa-esp32-espidf/release/xdj-clock-esp32`.

### WT32-ETH01 pins

| Pin | Use |
|---|---|
| GPIO16 | Switches on the 50 MHz oscillator of the PHY. Set it high before the Ethernet driver starts. |
| GPIO0 | RMII clock input from the oscillator |
| GPIO18, 19, 21, 22, 23, 25, 26, 27 | RMII and MDIO to the LAN8720 (PHY address 1) |
| IO32 | Beat LED. It pulses for 30 ms on each beat of the master deck. |
| IO33 | Bar LED, with the label 485_EN on the board. It pulses for 120 ms on beat 1. When no beat arrives for 2 s, it blinks two times every 2 s. |
| IO17 | MIDI out (UART2 TX, 31250 baud). TRS-A: IO17 → 10 Ω → tip, 3V3 → 33 Ω → ring, GND → sleeve. |
| IO35 | UART2 RX. It is not used, but the UART driver needs a receive pin. |
| IO4 | DMX out (UART1 TX, 250 kbaud) to an RS-485 module. Only with `--features dmx`. |
| IO36 | UART1 RX. It is not used, but the UART driver needs a receive pin. |

Do not connect a load to IO12 at power-on. If IO12 is high, the ESP32 sets the flash to 1.8 V and does not start.

### Network

- The firmware starts its own WiFi network `xdj-clock-XXXX` (the last 2 bytes of the MAC address). The default password is `xdjclock123`. The web page is at http://192.168.1.1.
- Do not connect the Ethernet port to a 192.168.1.x network. The WiFi network uses that subnet, and the firmware can then send a reply out on the wrong port. The firmware logs a warning for this case.
- Ethernet gets its address from DHCP. Without a DHCP server (a direct cable to the XDJ-AZ), it takes a 169.254.x.x address.
- The firmware joins the network as a virtual CDJ, like the host. The page setting "Device Number" (index 2, default 7) sets the number. A change makes the virtual CDJ join again.
- The XDJ-AZ sends the track ID only to a virtual CDJ with number 7 or higher. Do not run the host and the firmware on one network with the same number.
- The players send their CDJ status (port 50002) to the virtual CDJ. The master flag in the status decides the master. Without a status, the master is the deck that sent the last beat, until that deck stops.
- The firmware reads the absolute position packets (port 50001) for the playhead and the track length.
- When a deck loads a track, the firmware connects to the dbserver of the source player. It gets the title, the artist, the phrases, the waveform preview, the cues and the beat grid. It fetches one track at a time.
- The WebSocket handshake completes before the firmware can read the `Origin` header. When the Origin check fails, the firmware closes the connection and sends no data. It cannot reply with 403.

## Refactor Context (for future sessions)

- `crates/xdj-core-prolink` holds a copy of the protocol constants and the packet parsing from `host/src/prolink/`. The host does not use the shared crate yet.
- The ESP32 MIDI output follows the host rules (`xdj-core-midi`), with the phrase notes and CCs. It has no No Bass note, no MTC and no OSC.
- The ESP32 page has no mixer section. The XDJ-AZ sends the EQ and fader positions only as USB MIDI, and the classic ESP32 has no USB host.
- The ESP32 sends the Phrase 16 CC as the page help says (0 at each block start). The host never sends it, because its dedupe starts at 0.
- The DMX output (`--features dmx`) flashes channel 1 on each beat. It has no settings yet.
- The emulator uses old setting indices (for example 8 for Note Channel, which the host has at 9). It does not use `xdj-core-midi` yet.
- A refactor plan exists at `.sisyphus/plans/multi-architecture-refactor-plan.md`
- `host/src/state.rs` and `host/src/main.rs` are the highest-coupling files in the host crate
- Adding a new architecture target should mirror the `esp32`/`esp32-emulator` split:
  - firmware crate + native emulator crate
  - both share protocol via a new shared crate (planned as `xdj-core-prolink`)

## Constraints

- **until 2027-01-11** — Keep the host app alive under every fault. A task must restart after a panic (`runtime::supervise`). A blocking call must not run on a tokio worker. An FFI callback must not capture Rust data, because the vendored `rusty_link` callback setters keep a dangling pointer.

## graphify

This project has a graphify knowledge graph at graphify-out/.

Rules:
- Before answering architecture or codebase questions, read graphify-out/GRAPH_REPORT.md for god nodes and community structure
- If graphify-out/wiki/index.md exists, navigate it instead of reading raw files
- After modifying code files in this session, run `graphify update .` to keep the graph current (AST-only, no API cost)
