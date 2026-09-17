# AGENTS.md — xdj-midi

## Constraints

Standing instructions for working in this repo. **Every rule carries an expiry date, and an
expired rule is not in force.** On the first session after a rule's date, either renew it — push
the date out and append the reason it still holds — or delete it. Never silently keep it, and
never renew a rule you cannot justify in one line: not being able to say why it still applies
*is* the answer. This is the only thing that stops a constraint list growing into hundreds of
lines of dead instruction that an agent quietly ignores.

The line format is fixed so the list stays greppable — `- **until YYYY-MM-DD** — <rule>`. A new
rule gets **90 days** unless there is a reason for a shorter or longer life; a rule tied to a
known event (a migration, a launch, a vendor deadline) expires on that date instead.

```bash
sh ~/.claude/templates/check-constraints.sh
```

- **until 2026-11-26** — No repo-specific constraints have been written yet. Delete this line
  when the first one is added.

Only *instructions* belong here — "always X", "never Y", "ask before Z". Durable **facts** about
how this code works do not expire and live in the other sections, not in this list.

## Workspace

- **3 crates**: `xdj-clock-host` (desktop app), `xdj-clock-esp32` (firmware), `xdj-clock-esp32-emulator` (native emulator)
- **Root `Cargo.toml`** defines workspace members and patches `rusty_link` from `vendor/rusty_link`
- Package version is set once at workspace level (`[workspace.package]`)

## Developer Commands

```sh
# Host (desktop app)
cargo run -p xdj-clock-host
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
- macOS ARM64 → `.tar.gz` archive
- ESP32 firmware → `esp32-firmware.tar.gz`

## Architecture Map

| Crate | Role | Key modules |
|---|---|---|
| `host` | Desktop app + shared library | `config`, `midi/*`, `prolink/*`, `state`, `tui/*`, `link` |
| `esp32` | Firmware, bare metal | `prolink.rs` (different impl), `midi.rs` (UART), `webui.rs` |
| `esp32-emulator` | Native emulator | `main.rs`, `verify_stopped.rs` |

### Host library exports (`host/src/lib.rs`)

`pub mod config midi prolink state tui`

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
- `tui/render.rs` — ratatui rendering

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
- **TUI**: `ratatui` + `crossterm` on host (no TUI on ESP32)
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
- A refactor plan exists at `.sisyphus/plans/multi-architecture-refactor-plan.md`
- `host/src/state.rs` and `host/src/main.rs` are the highest-coupling files in the host crate
- Adding a new architecture target should mirror the `esp32`/`esp32-emulator` split:
  - firmware crate + native emulator crate
  - both share protocol via a new shared crate (planned as `xdj-core-prolink`)

## graphify

This project has a graphify knowledge graph at graphify-out/.

Rules:
- Before answering architecture or codebase questions, read graphify-out/GRAPH_REPORT.md for god nodes and community structure
- If graphify-out/wiki/index.md exists, navigate it instead of reading raw files
- After modifying code files in this session, run `graphify update .` to keep the graph current (AST-only, no API cost)
---

## Wiki Protocol

There is a personal wiki at `~/Sites/AI-projects/Wiki` — an Obsidian vault that an agent
maintains and the human reads. This project is a subject in its **`digital`** domain.

### The trigger — when a learning occurs, file it

Capture when, and only when, one of these is true:

1. A bug's root cause was **non-obvious** and cost real time.
2. An external system behaved **contrary to its documentation** — API quirk, rate limit, silent
   failure, undocumented error.
3. A decision was made **and alternatives were rejected** — record the why and the rejected list.
4. A build/deploy/tooling incantation was found that is **in no README**.
5. Something learned here **applies to another project**. Highest value; nothing else catches it.
6. The human states a **standing rule or preference** that applies to more than one project.

Do **not** capture routine feature work, anything already in this file, restatements of public
docs, or session status updates.

### The split

Facts durable to *this project only* stay in this file. The **cross-project and
durable-knowledge** slice goes to the wiki. The test: *"would this help me in a different
repo?"* Yes → wiki. No → here.

### The procedure — read it, never remember it

**`~/Sites/AI-projects/Wiki/CLAUDE.md`, section "Capture", is the single source of truth for
where the page goes, what shape it takes and how the log line reads.** Read it before writing
into the wiki, and follow it over anything you remember.

It is deliberately not restated here. This block sat a full schema version behind for months
because eleven repos each carried their own copy of the procedure; the fix was to stop copying
the half that changes. Keep it that way — if you find yourself pasting filing rules into this
file, you are recreating the bug.
