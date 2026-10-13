# xdj-midi

[![Version](https://img.shields.io/badge/version-1.4.0-blue.svg)](Cargo.toml)



![Terminal UI in action](./docs/tui_v1.2.9.png?auto=compress&cs=tinysrgb&dpr=3 "TUI on MacOS")


---

### Bridge between Pioneer CDJ/XDJ hardware (Pro DJ Link), Ableton Link and MIDI clock, MTC, CC, and note output.



## ✨ Features

### Pro DJ Link (Ethernet)
- Joins the ProLink network as a Virtual CDJ (device #7 by default).
- Supports CDJ-3000 and XDJ-AZ absolute-position packets and metadata.
- Tracks tempo master with explicit master flags and playing-state fallback.

### Ableton Link
- Auto mode prefers Link peers when present and falls back to Pro DJ Link.
- Default polling is 500µs for tight MIDI clock synchronization.
- Peer count and tempo are visible in the TUI and logs.

### MIDI Output
- Stable MIDI Clock (0xF8) pulses with Start/Stop/Continue support.
- Remappable CCs for BPM, pitch, bar phase, beat phase, playing state, master deck, and phrase-16.
- MIDI notes on every beat and on each downbeat.
- Phrase kind of the master deck as CC 20 (kind code times 10), as notes 40–51 and as Program Change (off by default). CC 21 sends the phrase progress. CC 22 sends the beats left in the phrase.

---

## 🚀 Quick Start

### Download the release
1. download the latest release on the right
2. run it.

### Raspberry Pi and unattended use
The `xdj-clock-linux-aarch64` release runs on a Raspberry Pi 3, 4 or 5 with a 64-bit OS.

The app recovers from these faults without a restart:
- When the MIDI device goes away, the app tries to open the same port again every 2 s. When the port comes back, the clock sends Stop, then Start on the next phrase. MTC sends a full frame.
- When the network is not up at start, the app waits for it.
- When the IP address goes away, the app finds the network again within a few seconds. Then it joins the Pro DJ Link network again.
- When you change the interface in the TUI, the app uses the new interface within 2 s.

To run the app as a service:
1. Copy `xdj-clock` and `config.toml` to `/opt/xdj-clock/`.
2. Copy `xdj-clock.service` to `/etc/systemd/system/`.
3. Run `sudo systemctl daemon-reload`.
4. Run `sudo systemctl enable --now xdj-clock`.
5. Read the log with `journalctl -u xdj-clock -f`.

The service runs the app with `--no-tui` and with real-time priority. `systemctl stop` sends MIDI Stop before the app exits.

Before a long night, test the MIDI path on the target machine:
```sh
./xdj-clock --soak midi-clock --duration-secs 36000 --report soak.json
```
The test passes when the p99 jitter is 5 ms or less and no messages drop.

**Note:** Keep the log level at `info`. The `trace` level writes every packet, which is approximately 1 GB in 10 hours.

---

## ⚙️ Configuration

Settings are managed via `config.toml` for the host and `sdkconfig` for the ESP32. You should be able to just run the app without any changes to these just fine.

```toml
# config.toml defaults
source = "auto"       # "auto" | "link" | "prolink"
device_number = 7      # Virtual CDJ ID
interface = "auto"    # Network adapter selection
device_name = "xdj-clock"

[midi]
output = "auto"       # MIDI port substring match
clock_enabled = true
clock_loop_enabled = true
smoothing_ms = 0       # BPM jitter reduction
latency_compensation_ms = 0
# Bar interval for phrase-lock full resync (1 bar = 4 beats).
phrase_lock_stable_beats = 4

[midi.notes]
channel = 9
beat = 36
downbeat = 37
phrase_change = 38
phrase_base = 40       # phrase kind notes: phrase_base + kind code (40–51)
no_bass = 39           # on while the bass is cut on all live XDJ-AZ channels

[midi.cc]
channel = 0
bpm_coarse = 1
bpm_fine = 33
pitch = 2
bar_phase = 3
beat_phase = 4
playing = 5
master_deck = 6
phrase_16 = 7
phrase_kind = 20       # kind code x 10
phrase_progress = 21
phrase_beats_left = 22

[midi.program]
enabled = false        # Program Change = kind code at each new phrase
channel = 0

[midi.mtc]
enabled = false
frame_rate = "25"

[link]
enabled = false
quantum = 4.0
poll_interval_us = 500
```

### Phrase output

The mapper reads the rekordbox phrase analysis of the master deck. At each new phrase it sends the phrase kind. It also sends the kind when a track loads and when the master deck changes.

| Kind | Code |
|---|---|
| Intro | 0 |
| Verse 1–6 | 1–6 |
| Bridge | 7 |
| Chorus | 8 |
| Up | 9 |
| Down | 10 |
| Outro | 11 |

- CC `phrase_kind` (default 20) sends the code times 10.
- Note `phrase_base` plus the code (default 40–51) fires on `midi.notes.channel`.
- Program Change sends the code on `midi.program.channel`. It is off by default, because a Program Change switches the patch of a synth.
- CC `phrase_progress` (default 21) sends the position in the phrase.
- CC `phrase_beats_left` (default 22) sends the beats until the next phrase, up to 127.

The player must send the phrase data over its dbserver. This needs at least one real player (1–4) on the network, or a CDJ-3000 class player. The log shows a warning when a player refuses the connection.

---

### XDJ-AZ mixer and NO_BASS

The XDJ-AZ sends its EQ knobs and channel faders as MIDI over its USB-C cable. The app reads them from the "XDJ-AZ" MIDI port, and the TUI shows them in the Deck Information panel.

Note `no_bass` (default 39) is on while the bass is cut on all live channels. Use it to start a strobe or another effect.

- A channel is live when its deck plays and its fader is at 10 % or higher.
- The bass is cut when the LOW knob is at 16 or lower (0–127, centre 64).
- One live channel sends the note when its bass is cut. Two or more live channels send the note only when the bass is cut on all of them.
- The note goes off when one live channel gets its bass back, or when no channel is live.
- Mixer channel CHn takes its play state from player n.
- Until a fader moves, the app uses the on-air flag of the player.

## Tested hardware

Please help test the software on your decks.

2024–2023
- ⚪ OMNIS-DUO (2024)
- ✅ XDJ-AZ (2024)
- ⚪ OPUS-QUAD (2023)
- ⚪ DJM-A9 (2023)
2021–2020
- ❌ XDJ-RX3 (2021, no PRO LINK)
- ⚪ CDJ-3000 (2020)
- ⚪ XDJ-XZ (2020)
2019–2016
- ❌ XDJ-RR (2019, no PRO LINK) 
- ⚪ CDJ-TOUR1 (2016)
- ⚪ CDJ-2000NXS2 (2016)
- ⚪ DJM-900NXS2 (2016)
- ❌ XDJ-RX2 (2017, no PRO LINK)
- ⚪ XDJ-1000MK2 (2018)
2015–2013
- ⚪ XDJ-1000 (2015)
- ⚪ XDJ-700 (2015)
- ⚪ XDJ-RX (2015)
- ⚪ CDJ-2000NXS (2013)
- ⚪ CDJ-900NXS (2013)
- ⚪ DJM-2000NXS (2013)
- ⚪ DJM-900SRT (2013)
2012–2009
- ⚪ CDJ-2000 (2009)
- ⚪ CDJ-900 (2009)
- ⚪ DJM-2000 (2010)
- ⚪ DJM-900NXS (2011)
---

## 🏗 Architecture

The project is a Rust workspace with shared core logic across desktop and embedded targets.

- **`host/`**: Desktop app and shared core library.
  - Pro DJ Link packet parsing and shared state management.
  - Real-time TUI with device discovery, BPM/phase, and MIDI port selection.
  - Ableton Link engine with hybrid auto mode.
- **`esp32/`**: ESP32 firmware.
  - WiFi AP setup.
  - UART MIDI in/out.
- **`esp32-emulator/`**: Native emulator for the ESP32 firmware.
  - Web dashboard with WebSocket updates.
  - Simulators for master handoff and stopped-deck cases.

Core outputs:
- **24-PPQ MIDI clock** — Start / Stop / Continue + 0xF8 pulses from DJ tempo
- **MIDI CC** — BPM coarse/fine, pitch, bar phase, beat phase, playing state, master deck, phrase-16
- **MIDI notes** — beat trigger and downbeat trigger with velocity accents
- **Phrase output** — kind of the master phrase as CC, note and optional Program Change, plus phrase progress and beats left (see below)
- **Source modes** — Pro DJ Link, Ableton Link, or auto
- **Network auto-discovery** — picks a suitable interface automatically
- **TOML config** — remappable MIDI channel, note, and CC numbers
- **CLI overrides** — interface, MIDI port, source, device number, log level
- **TUI** — live status, port selection, and logging via [ratatui](https://ratatui.rs)

---

## Slow  Start 
### Compile the Host (Desktop)
1.  **Vendor Setup**: Ableton Link support requires vendoring `rusty_link`.
    ```sh
    mkdir -p vendor
    curl -sL https://crates.io/api/v1/crates/rusty_link/0.2.3/download | tar xzf - -C vendor
    mv vendor/rusty_link-0.2.3 vendor/rusty_link
    cp .github/vendor-patches/build.rs vendor/rusty_link/build.rs
    cp .github/vendor-patches/link_bindings.rs vendor/rusty_link/link_bindings.rs
    ```
2.  **Run**:
    ```sh
    cargo run                   # The desktop app with the TUI.
    cargo run -- --no-tui       # The desktop app without the TUI (headless).
    ```

### Building the ESP32 Firmware
```sh
. /path/to/esp-idf/export.sh
cargo build --release -p xdj-clock-esp32
```

### Running the Emulator
```sh
cargo run -p xdj-clock-esp32-emulator
```
Open `http://localhost:8080` to view the responsive WebSocket dashboard.

---

## 🛠 Project State (v1.2.9)
- v1.1.0: Migrated emulator to WebSockets.
- v1.1.2: Added proactive Pro DJ Link participation.
- v1.1.4: Fixed master handoff and play-state latching.
- v1.1.7: Restored Ableton Link after `rusty_link` fixes.
- v1.2.9: Tightened MIDI clock/MTC scheduling and pitch-correct beat timing.

---

## 📜 License
All rights reserved. Uses components licensed under MIT and GPL-2.0-or-later (Ableton Link).
