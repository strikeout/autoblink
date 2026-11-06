# DMX Planner 2: Open Fixture Library Import — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The owner adds a light by its model name. The box gets the definition from the Open Fixture Library (OFL), and the page turns the chosen mode into the channel list of the show.

**Architecture:** The phone on the box's WiFi has no internet, so the box is the proxy. Two firmware routes fetch OFL over HTTPS on a short-lived thread with a 12 KB stack and pass the JSON through to the page. The page parses the JSON and maps each channel of the chosen mode to a `Channel` of the show. The show keeps the OFL key and the mode name of each light, so the light stays usable without internet.

**Tech Stack:** Rust 2021, esp-idf-svc 0.53 HTTP client with the certificate bundle, vanilla JS.

**Spec:** `docs/superpowers/specs/2026-10-27-dmx-planner-design.md` (Patch and fixtures; "OFL on the ESP32"; "OFL without internet").

## OFL facts (checked 2026-10-28)

- Search: `POST https://open-fixture-library.org/api/v1/get-search-results` with the JSON body `{"searchQuery":"<text>","manufacturersQuery":[],"categoriesQuery":[]}`. The reply is a JSON list of `{"key":"<manufacturer>/<model>","name":"<full name>","color":"#rrggbb"}`. The query "par" gives 108 results, about 10 KB.
- Fixture: `GET https://open-fixture-library.org/<manufacturer>/<model>.json`. The reply is about 5 KB for a simple light. `modes[].name` and `modes[].channels` (a list of channel names, or `null` for an unused channel) give the modes. `availableChannels.<name>` has `capability` (one) or `capabilities` (a list); each has a `type`, for example `Intensity`, `ColorIntensity` (with `color`: Red, Green, Blue, White, Amber, UV, Cyan, Magenta, Yellow, Lime, Indigo, Warm White, Cold White), `ShutterStrobe`, `Pan`, `Tilt`, `Fog`, `FogOutput`, `NoFunction`. A channel can have `defaultValue` and `fineChannelAliases`.

## Global Constraints

- The box has about 100 KB free heap with one track and the page open. TLS to an HTTPS server takes about 50 KB. An OFL request does not start below 80 KB free heap or a 32 KB largest block; the route then answers 503.
- An OFL reply body larger than 24 KB is cut off; the route answers 502 with a short text.
- TLS needs more stack than the HTTP task has. The fetch runs on a thread with a 12 KB stack (`crate::spawn`), and the handler waits for it.
- While a fetch runs, the status thread sends no status, as during an OTA check (the heap is short).
- Do not use `std::io::copy` in a firmware thread. Do not build a `serde_json::Value` on the firmware: the firmware passes the OFL bytes through and does not parse them.
- A key must match `^[a-z0-9-]+/[a-z0-9-]+$`. The firmware refuses any other key with 400.
- No f32 in a serde type that the firmware reads.
- Commits carry no attribution lines. Comments, log lines, error strings and docs follow ASD-STE100.
- Bump the version to `1.26.0` (workspace and `esp32/Cargo.toml`) in the last task.

## Review Focus

1. A box without internet (only its own WiFi): both routes answer 502 or 504 quickly with a clear text, and the page offers the generic types. Test: Task 3 stub with a 502 reply; box session.
2. A light with a matrix or a fine channel (16-bit pan): the mapping gives one `Channel` for each DMX channel of the mode, never fewer, so the addresses of the next lights stay right. Test: `oflChannels` unit cases in Task 3.
3. A search typed fast on a phone: the page sends at most one search at a time and drops stale results. Test: Task 3 stub.
4. Heap after ten fetches in a row: no leak. Test: box session `/mem`.
5. A reply that is not JSON (an OFL error page): the page shows the error, adds nothing. Test: Task 3 stub.

---

### Task 1: The show keeps the OFL source of a light

**Files:** Modify `crates/autoblink-core-dmx/src/show.rs`.

**Interfaces:**
- Produces: `Fixture` gets `#[serde(default, skip_serializing_if = "Option::is_none")] pub ofl: Option<String>` — `"<manufacturer>/<model>#<mode name>"`. `sanitize` cuts it to 96 characters and drops control characters. `Channel` stays as it is (pan and tilt come in plan 3; the page maps them to `Fixed` for now).

- [ ] **Step 1:** Write the tests `ofl_round_trip` (a fixture with `ofl: Some("eurolite/led-bar-6-qcl-rgbw#4-channel")` survives `to_string` + `from_json`; a fixture without it serializes without the key) and `ofl_sanitize` (200 characters → 96; a `\n` is dropped).
- [ ] **Step 2:** Run `cargo test -p autoblink-core-dmx`. Expected: FAIL.
- [ ] **Step 3:** Implement the field and the sanitize rule.
- [ ] **Step 4:** Run `cargo test --locked`, fmt, clippy. Expected: PASS.
- [ ] **Step 5:** Commit "Keep the Open Fixture Library source of a light".

### Task 2: The box fetches from OFL

**Files:** Create `esp32/src/ofl.rs`. Modify `esp32/src/ota.rs` (share the HTTPS client set-up and the DNS fix; no behaviour change for OTA), `esp32/src/webui.rs` (two routes, the status pause), `esp32/src/main.rs` (`mod ofl;`).

**Interfaces:**
- Produces in `ofl.rs`:

```rust
pub const MAX_BODY: usize = 24 * 1024;
pub const MIN_FREE: u32 = 80 * 1024;
pub const MIN_BLOCK: u32 = 32 * 1024;
/// True while a fetch runs. The status thread sends no status then.
pub fn busy() -> bool;
/// Searches OFL. Returns the reply body, or an HTTP status and a short text.
pub fn search(query: &str) -> Result<Vec<u8>, (u16, String)>;
/// Gets the JSON of one fixture. `key` is "<manufacturer>/<model>".
pub fn fixture(key: &str) -> Result<Vec<u8>, (u16, String)>;
```

- Routes: `GET /ofl/search?q=<text>` (q is URL-decoded, 1 to 64 characters; JSON-escape it into the POST body) and `GET /ofl/fixture?key=<manufacturer>/<model>`. Both: Origin check as `POST /show` (403); `Access-Control-Allow-Origin: *`; 200 with `Content-Type: application/json` and the OFL body; 400 for a bad query or key; 503 below `MIN_FREE` / `MIN_BLOCK` or while another fetch or an OTA check runs; 502 when OFL answers an error, the body is too large, or the box has no route to the internet; 504 after a 10 s timeout.
- Errors keep the box alive: every failure is a status and a text; nothing panics.

- [ ] **Step 1:** Move the HTTPS client set-up (`get` in ota.rs with the certificate bundle, and `fix_dns`) into a small shared helper that both `ota.rs` and `ofl.rs` call. Add a POST variant for the search. Keep OTA behaviour identical.
- [ ] **Step 2:** Implement `ofl.rs`: heap guard, busy flag, a `crate::spawn(c"ofl", 12 * 1024, ..)` thread that does the request and reads the body into a `Vec` with a read loop (no `io::copy`), cut at `MAX_BODY`, joined by the caller.
- [ ] **Step 3:** Add the two routes and make the status thread skip a status while `ofl::busy()` (as for `ota::busy()`).
- [ ] **Step 4:** Build both firmware configurations (`cargo build --release --locked`, `--no-default-features`), `rustfmt --check --edition 2021` on the touched files, and report the image size.
- [ ] **Step 5:** Commit "Fetch fixtures from the Open Fixture Library through the box".

### Task 3: "Add from library" in the Lights setup

**Files:** Modify `webui/index.html`.

**Interfaces:**
- Consumes: `GET BASE + '/ofl/search?q='`, `GET BASE + '/ofl/fixture?key='` (Task 2); `Fixture.ofl` (Task 1).
- Produces: `function oflChannels(fixture, modeName) -> { channels: Channel[], notes: string[] }` — one `Channel` for each entry of the mode's `channels`:
  - `Intensity` → `"dimmer"`.
  - `ColorIntensity` with Red, Green, Blue, White, Amber, UV → `"red"`, `"green"`, `"blue"`, `"white"`, `"amber"`, `"uv"`. Warm White and Cold White → `"white"`. Other colours → `{"fixed":0}`.
  - `ShutterStrobe` → `"strobe"`. `Fog`, `FogOutput` → `"fog"`.
  - A fine channel (named in `fineChannelAliases` of another channel), `Pan`, `Tilt`, anything else, and `null` → `{"fixed": defaultValue or 0}`.
  - When a channel has several capabilities, use the type of the capability that covers DMX value 255; else the first.
  - A mode entry that names a matrix or a template channel → `{"fixed":0}` and a note "pixel channels are not supported yet".
- UI: in "Lights setup", next to "+ light", a "+ from library" button opens a small search box. Typing (debounced 300 ms, at most one request in flight, stale replies dropped) lists up to 20 results (name). A pick fetches the fixture, shows the modes as pills with their channel count and the mapped channel list (as text), and an "Add" button that adds the light at the next free address with `name` (cut to 16 characters), `channels`, and `ofl: "<key>#<mode>"`. Errors (503, 502, 504, not JSON) show a toast with the text and keep the generic types available.
- A light with `ofl` shows the OFL name in its card.

- [ ] **Step 1:** Write `oflChannels` and check it in a scratch file under `.context/` (never committed) against three real OFL fixtures saved from the site: an RGBW bar with a 4-channel mode, a light with a fine channel, and a fog machine. Paste the outputs into the report.
- [ ] **Step 2:** Build the UI. Check it against a local stub with fake `/ofl/*` replies, including a 502 and a reply that is not JSON. Console clean. Screenshots to `.context/ofl-*.png`.
- [ ] **Step 3:** Build the firmware (the page is in the image).
- [ ] **Step 4:** Commit "Add lights from the Open Fixture Library".

### Task 4: Version, docs and checks

**Files:** `Cargo.toml`, `esp32/Cargo.toml`, both lock files, `AGENTS.md`.

- [ ] **Step 1:** Bump to 1.26.0; update the lock files.
- [ ] **Step 2:** AGENTS.md, section "DMX show": the two routes, the heap guards, the 12 KB `ofl` thread, the status pause, the 24 KB cut, the need for internet on a joined WiFi network or Ethernet, the `ofl` field. Run `sh ~/.claude/templates/ste-check.sh AGENTS.md` on the new lines.
- [ ] **Step 3:** Run `sh scripts/check.sh --esp32`. Expected: all checks pass.
- [ ] **Step 4:** Commit "Release the Open Fixture Library import as 1.26.0".
- [ ] **Step 5 (box session):** flash over USB; with the box on a WiFi network with internet: search "par", add one light, `/mem` after ten fetches; without internet: both routes answer 502/504 within 10 s.
