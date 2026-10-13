# AGENTS.md — xdj-midi

## Workspace

- **3 crates**: `xdj-clock-host` (desktop app), `xdj-clock-esp32` (firmware), `xdj-clock-esp32-emulator` (native emulator)
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

# ESP32 firmware (requires ESP-IDF environment)
. /path/to/esp-idf/export.sh
cargo build --release -p xdj-clock-esp32

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
| `esp32` | Firmware, bare metal | `prolink.rs` (different impl), `midi.rs` (UART), `webui.rs` |
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

**ESP32 vs host**: ESP32 uses `smoltcp` instead of `std::net::UdpSocket` and has no `tokio` runtime. Its `prolink.rs` re-implements protocol concepts from the host but with a different network stack.

## Toolchain & Dependencies

- **Async runtime**: tokio (multi-threaded) on host/emulator; no-std + `embassy-sync` on ESP32
- **Error handling**: `anyhow::Result` on host/emulator; `anyhow` optional on ESP32
- **Logging**: `tracing` + `tracing-subscriber` on host; `log` on ESP32
- **State sync**: `parking_lot::RwLock` (not tokio's)
- **MIDI**: `midir` on host (no ESP32 equivalent)
- **Web UI**: `axum` with WebSockets on host and emulator; vanilla HTML/JS page, no build step
- **Network interfaces**: `network-interface` crate on host

## Testing

- **`cargo test --locked`** runs tests across all crates
- No dedicated test infrastructure found (no `tests/` directory, no fixtures)
- ESP32 CI runs a QEMU smoke test in `esp32-emulator-test` job
- Coverage gaps: packet parsing, state machine transitions, BPM smoothing, MIDI clock transport

## ESP32 Build Requirements

1. Source ESP-IDF environment: `. /path/to/esp-idf/export.sh`
2. Install Rust target: `rustup target add riscv32imc-esp-elf`
3. Build with: `cargo build --release --locked -p xdj-clock-esp32`
4. Output: `esp32/target/riscv32imc-esp-elf/release/xdj-clock-esp32`

ESP32 CI uses the `espressif/idf:latest` Docker container and builds on the `esp32` branch.

## Refactor Context (for future sessions)

- The protocol constants and packet parsing in `host/src/prolink/` are duplicated in `esp32/src/prolink.rs`
- The `esp32` crate does not compile. `main.rs`, `state.rs`, `midi.rs` and `prolink.rs` use APIs that do not exist (for example `heapless::singleton::Smp`, `smoltcp::iface::EthernetInterface`, the embedded-hal 0.2 serial traits). Nothing receives Pro DJ Link packets. The target `riscv32imc` (ESP32-C3) has no Ethernet MAC. Before the firmware can serve `webui/index.html` with protocol v1, it needs a rewrite. The ESP32 CI on the `esp32` branch fails since 2026-09-17.
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
