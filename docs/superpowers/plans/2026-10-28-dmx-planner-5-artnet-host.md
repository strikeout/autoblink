# DMX Planner 5: Art-Net and the Host — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The box merges Art-Net from a different desk into its own DMX output. The host page edits the box's show over the network. Without a box, the host runs the same engine and sends Art-Net to a DMX node.

**Architecture:** `autoblink-core-dmx` gets an Art-Net codec (ArtDmx only) and a merge (HTP for intensity channels, LTP for the rest). The firmware receives Art-Net on UDP 6454 in a small thread and merges the last frame in the `dmx` thread. The host finds the box among the Pro DJ Link devices, and its page edits the box's `/show` directly (the box already allows a page from `127.0.0.1`/`localhost`). The host keeps a copy of the show; with `[dmx] artnet_target` set and no box on the network, a supervised host task runs `Lights` and sends ArtDmx frames.

**Tech Stack:** Rust 2021, ESP-IDF (`std::net::UdpSocket`), the host (tokio `UdpSocket`, axum), vanilla JS.

**Spec:** `docs/superpowers/specs/2026-10-27-dmx-planner-design.md` (Where it runs; Merge rule; Host and box).

## Facts

- Art-Net 4 ArtDmx: `"Art-Net\0"`, OpCode 0x5000 little-endian, ProtVer 14 big-endian, Sequence, Physical, SubUni, Net, Length big-endian (2..512, even), then the data. UDP port 6454.
- The box and the host both use the Pro DJ Link name `autoblink` (`esp32/src/prolink.rs` `NAME`, `host/src/config.rs` default `device_name`). The host finds the box as a device with that name and a device number that is not its own.
- The box's `origin_allowed` accepts a page from `localhost` and `127.0.0.1` (`dev_origin`), `GET /show` sends `Access-Control-Allow-Origin: *`, and `POST /show` is a CORS simple request. The host page on `127.0.0.1:11111` can therefore read and write the box's show directly.

## Global Constraints

- Merge rule (spec): HTP for the channels of kind Dimmer, Red, Green, Blue, White, Amber, Uv and Strobe; LTP for every other channel, including the channels that no light patches. LTP: the source that changed a channel last owns it.
- Art-Net counts as live for 3 s after its last frame (`ARTNET_TIMEOUT_MS`). Without it, the output is the engine alone.
- The firmware listens on universe 0 (Net 0, SubUni 0) only. A frame of another universe is ignored.
- The firmware receive thread never blocks the `dmx` thread: it writes the last frame under a short lock (`try_lock` on the `dmx` side).
- The host must stay alive under every fault (`runtime::supervise`; no blocking call on a tokio worker).
- The host does not send Art-Net to the box. When it finds a box, its own engine stops sending.
- No f32 in serde types on the firmware. Commits carry no attribution lines. Comments and docs follow ASD-STE100.
- Bump the version to `1.29.0` in the last task.

## Review Focus

1. A desk that sends Art-Net and then stops: after 3 s the box output is the engine alone; no frozen channel. Test: Task 1 merge test; box session.
2. Pan/tilt from a desk while the engine moves the head: LTP gives the desk the channel while it changes, then the engine takes it back when the engine changes. Test: Task 1.
3. A malformed or short UDP packet: ignored, no panic. Test: Task 1 parse tests.
4. The host page with a box and without one: it edits the right show and says which. Test: Task 4 stub.
5. A host without `artnet_target`: no task, no socket. Test: Task 3.

---

### Task 1: The Art-Net codec and the merge

**Files:** Create `crates/autoblink-core-dmx/src/artnet.rs` and `merge.rs`; export them.

**Interfaces:**

```rust
// artnet.rs
pub const ARTNET_PORT: u16 = 6454;
/// Returns the universe (Net << 8 | SubUni) and the data of an ArtDmx packet, or None.
pub fn parse_artdmx(packet: &[u8]) -> Option<(u16, &[u8])>;
/// Writes an ArtDmx packet for `universe` with `data` (512 bytes) into `out`. Returns the length.
pub fn build_artdmx(sequence: u8, universe: u16, data: &Universe, out: &mut [u8; 530]) -> usize;
// merge.rs
pub const ARTNET_TIMEOUT_MS: u64 = 3_000;
#[derive(Default)]
pub struct Merge { /* owner per channel, the previous engine and Art-Net frames, last Art-Net time */ }
impl Merge {
    /// Merges `artnet` (the last frame and its time) into `engine` in place, by the kinds of `show`.
    pub fn apply(&mut self, show: &Show, artnet: Option<(&Universe, u64)>, now_ms: u64, engine: &mut Universe);
}
```

- [ ] **Step 1:** Write the tests: `parse_valid_artdmx` (universe 0x0102, 4 data bytes), `parse_rejects` (wrong id, wrong opcode, short, length > data), `build_round_trip`, `htp_on_dimmer` (engine 100, Art-Net 200 → 200; engine 220 → 220), `ltp_on_pan` (engine 10 steady, Art-Net changes 50 → 50; the engine then changes to 60 → 60), `unpatched_channel_is_ltp`, `artnet_times_out` (3 001 ms after the last frame → engine values; an owner reset to the engine).
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement (no allocation in `apply`).
- [ ] **Step 4:** `cargo test --locked`, fmt, clippy, firmware build.
- [ ] **Step 5:** Commit "Add the Art-Net codec and the DMX merge".

### Task 2: The box receives Art-Net

**Files:** Create `esp32/src/artnet.rs`. Modify `esp32/src/main.rs` (start the thread after the network starts), `esp32/src/dmx.rs` (call `Merge::apply` after `lights.frame`), `esp32/src/webui.rs` (status `dmx.artnet`: `true` while Art-Net is live).

- A `crate::spawn(c"artnet", 4 * 1024, ..)` thread binds `0.0.0.0:6454`, reads with a 1 s timeout, parses with `parse_artdmx`, keeps universe 0 only, and stores `(frame, now_ms)` in an `Arc<Mutex<..>>`. A bind error logs one warning and ends the thread (the box keeps working).
- The `dmx` thread reads it with `try_lock` each frame.
- Measure nothing on the box now (offline); note the stack as an estimate.
- [ ] Build both configurations; rustfmt; commit "Merge Art-Net from a desk into the DMX output".

### Task 3: The host engine and show copy

**Files:** Modify `host/src/config.rs` (`[dmx]` section: `artnet_target: Option<String>` (an IP or `ip:port`), `universe: u16` default 0), add `host/src/dmx.rs` (the task), `host/src/runtime/` (start it under `supervise` when `artnet_target` is set), `host/src/midi/mapper.rs` (forward the scene notes and fired presets to the task through a bounded tokio channel, never awaiting a full channel), `host/src/web/mod.rs` (`GET /show`, `POST /show` on the host: the show copy in `show.json` next to the config file, written on a blocking thread; cap `dmx` when `artnet_target` is set; status `box`: the IP of the box when one is on the network).

- The task runs `Lights` and `BeatClock` like the firmware `dmx` thread, at about 40 Hz, and sends ArtDmx with `build_artdmx` through a tokio `UdpSocket`. While the host sees a box, it sends nothing.
- [ ] Tests: config parse with and without `[dmx]`; the `/show` routes (round trip, bounded parse via `Show::from_json`); box detection from a device list. `cargo test --locked`, fmt, clippy.
- [ ] Commit "Run the DMX engine on the host and send Art-Net to a node".

### Task 4: The host page edits the box's show

**Files:** Modify `webui/index.html`.

- When `status.box` is set, the Lights setup and the look editor read and write `http://<box>/show` and use `http://<box>/ofl/...`; a line at the top of the setup says "Editing the show of the box at <ip>". After each successful save to the box, the page also posts the same show to the host's `/show` (the host keeps the copy).
- Without a box and with the `dmx` cap, the page edits the host's own `/show` ("Editing the show of this computer (Art-Net to <target>)").
- The live strip shows only with a box connection or the `dmx` cap; on the host page with a box, the live controls stay on the box's page (hide them, with a link "Open the box page").
- Check against a stub; screenshots `.context/host-*.png`.
- [ ] Commit "Edit the box's show from the host page".

### Task 5: Version, docs and checks

- Bump to 1.29.0 with the lock files; AGENTS.md: Art-Net in (port, universe 0, timeout, merge rule), the `artnet` thread, the host `[dmx]` config, the host engine, box detection by name, the show copy; run `sh scripts/check.sh --esp32`; commit "Release Art-Net and the host engine as 1.29.0".
- Box session (deferred while the box is offline): QLC+ sends universe 0 to the box → HTP on a par, LTP on a head; stop QLC+ → engine after 3 s; the host page edits the box's show; the host with `artnet_target` and the box off → a node gets the frames.
