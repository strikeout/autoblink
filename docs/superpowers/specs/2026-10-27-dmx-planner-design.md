# DMX planner: design

## Context

The owner has a Disco 240 DMX desk. The desk has no MIDI input. It has only two DMX outputs and a mains inlet. Thus the MIDI mapping of AUTOBLINK cannot control the desk.

The DMX planner replaces the desk as the light source. The desk stays as a spare. The planner uses the same song events as the MIDI mapping. These events are the beat, the downbeat, the phrase change, the phrase kind, the phrase progress and the play state.

The owner chose layout B of the brainstorm: a look library with a trigger table.

## Goals

- The light follows the song structure. The phrase kind selects a look.
- The light follows the beat. An effect layer flashes, pulses, chases and moves the heads in time.
- The setup before a gig is fast. The owner starts from presets and changes single fixtures only when necessary.
- The owner can take control live from a phone or a laptop.

## Non-goals

- More than one DMX universe.
- Pixel mapping, a 3D visualiser and a timeline editor for each track.
- More than one saved show.
- A MIDI controller as a live surface. This comes in a later project.

## Anti-goals

The design fails, even when it works technically, in these cases:

- The light is out of time with the beat.
- The strobe or the fog fires too often, or at the wrong moment.
- The setup before a gig is slow and complex.
- The rig goes dark when the box restarts, or when the box loses Pro DJ Link.

## The model

### Show

There is one show. The owner edits it for each venue. The show holds the patch, the groups, the positions, the looks, the triggers and the idle look.

### Patch and fixtures

- The rig has 8 to 24 fixtures on one universe. The types are RGB and RGBW pars, LED bars, moving heads, scanners, strobes, dimmers and fog machines.
- The owner adds a fixture by its model name. The box joins the venue WiFi and gets the definition from the Open Fixture Library (OFL).
- The owner selects the DMX mode and the start address of each fixture.
- The owner puts fixtures into groups, for example "Pars", "Heads", "Strobe" and "Fog".

### Positions

The owner aims each moving head once at each venue. The owner uses pan and tilt sliders, and saves the result as a named position, for example "crowd", "DJ" or "ceiling".

### Look

A look is a static state. It has one row for each group. A row holds the colour, the intensity and the position. A look also has a fade-in time: cut, 1 beat or 1 bar.

The owner builds a look from presets: a colour palette, an intensity and a named position. The owner can open one fixture and change its values.

### Effect layer

The effect layer runs on top of any look, in time with the beat. It holds these effects:

- Flash, pulse, chase and random on the intensity of a group.
- Strobe, with a cooldown.
- A fog burst, with a cooldown.
- The engine fires the strobe and the fog only on a phrase event, for example a Chorus start. The cooldown applies to each automatic burst. The live buttons fire them at any time.
- Shapes for the moving heads: circle, figure 8 and sweep. A shape moves around the position of the look. Its period is in beats or bars.

Each look has a default effect. The owner can override the effect live.

### Triggers

1. A phrase kind selects a look: Intro, Verse 1 to 6, Bridge, Chorus, Up, Down and Outro. The look brings its default effect.
2. If the track has no phrase analysis, the planner finds the drops from the waveform energy. It puts each change on a 16-beat block.
3. If no deck plays, or the box loses Pro DJ Link, the idle look starts after 2 s. The rig never goes dark.

### Live controls

The page has a live strip with these controls:

- A look picker (previous, next, select).
- An effect override.
- A master dimmer.
- A strobe hold.
- A fog button.

## Where it runs

- The ESP32 drives the DMX cable on IO4 through an RS-485 module (`esp32/src/dmx.rs`).
- The show lives on the box. The "Lights" panel of the host page edits the show of the box over the network.
- The ESP32 accepts Art-Net from a different desk, for example a laptop with QLC+. It merges that Art-Net with its own engine (HTP).
- The host runs the same engine only when there is no box. Then it sends Art-Net or sACN to a DMX node.
- The owner wants all parts in the first release. The ESP32 and the host get the planner at the same time.

## Decisions by the agent (the owner did not decide these)

- **Shared crate.** The engine is a new crate `crates/autoblink-core-dmx`, like `autoblink-core-midi`. The crate holds the show model, the trigger logic, the effects, the merge and the energy guess. It has host tests.
- **Merge rule.** HTP applies only to the intensity channels: dimmer, strobe intensity and colour channels. The pan, the tilt and the other channels use the last source that changed them (LTP). An HTP merge of pan and tilt makes the heads point to wrong places.
- **Timing.** The effects use the predicted beat phase (BPM and the last beat), not the arrival time of a packet. On WiFi, a beat packet can arrive up to 300 ms late. The MIDI clock already corrects for this. The DMX engine uses the same phase.
- **Storage.** The show goes into a new partition `show` of 64 KB at 0x3F0000, after `coredump`. The show uses a compact binary format. A box needs one USB flash to get the partition, because OTA does not change the partition table.
- **Page.** The planner is a new "Lights" panel in `webui/index.html`. The ESP32 and the host serve this page. The show goes over the WebSocket in its own messages, not as setting indices.
- **OFL on the ESP32.** The box streams the OFL data to the browser, and the browser parses it. The browser sends back only the channel layout of the chosen mode. Thus the ESP32 does not hold a large JSON document in its heap.
- **Feature flag.** The `dmx` feature becomes a default feature of the firmware.
- **Manual look and effect.** A look or an effect that the owner picks by hand stays until the next phrase change. A look that the owner picks by hand brings its default effect.
- **Host and box.** The host does not send Art-Net to the box. The host keeps the last show that it read from the box. It uses that copy only when it drives a DMX node without a box.
- **Never dark at a restart.** The firmware starts the DMX thread before the network. The thread sends the idle look of the stored show. The OTA maintenance start also sends the idle look.
- **OFL without internet.** Each fixture definition stays in the show after the first fetch. The patch also has a small set of generic types: dimmer, RGB, RGBW and fog. The owner can use these when the venue has no internet.

## Constraints from the code

- The ESP32 has about 100 KB free heap with one track and the page open. TLS to GitHub uses about 50 KB. The OFL fetch uses the same heap checks as `ota_check`: 60 KB free and a 24 KB largest block.
- The `dmx` thread has a lower priority than `midi` (10) and `prolink-rx` (8). It starts with `crate::spawn`.
- The show messages are `Serialize` structs. Do not build a `serde_json::Value` tree.
- Do not use `std::sync::mpsc` from `prolink-rx` to a thread with a higher priority. Use the FreeRTOS queue, as `midi.rs` does.
- Do not use `std::io::copy` in a firmware thread.
- The app image must stay below the slot size (0x1E0000). Version 1.21.0 uses 74 %.
- The firmware keeps 600 preview columns of 6 bytes for each track. The drop guess uses these columns.
- Without phrases, the firmware does not request the beat grid. The drop guess therefore counts the live beats in 16-beat blocks.
- The host must stay alive under every fault (`runtime::supervise`). The Art-Net sender must not block a tokio worker.

## Left to the builder

- The exact binary format of the show.
- The effect curves, the default decay times and the cooldown defaults.
- The colour mixing for RGB and RGBW fixtures, and the colour wheel mapping.
- The algorithm of the drop guess.
- The choice between Art-Net and sACN as the first host protocol.
- The page layout inside layout B.

## Verification

1. Run `cargo test --locked`. The tests of `autoblink-core-dmx` cover the triggers, the effect timing, the cooldowns, the merge and the drop guess.
2. Build the firmware with `cd esp32 && cargo build --release`. Make sure that the image is below the slot size.
3. Flash the box over USB. Connect a fixture, or a USB-DMX interface with a DMX monitor.
4. Play a track with phrase analysis on the XDJ-AZ. Make sure that the look changes at each phrase start and that the flashes are on the beat.
5. Play a track without phrase analysis. Make sure that the drop guess changes the look on 16-beat blocks.
6. Stop the deck, and pull the Ethernet cable. Make sure that the idle look starts after 2 s.
7. Read `curl http://xdj-clock.local/mem` after a fetch from OFL and with the page open. Make sure that each task keeps its stack margin.
8. Send Art-Net from a different desk (QLC+) to the box. Make sure that the merge gives the expected channel values.
9. Run the host without a box, with Art-Net to a DMX node. Make sure that the node gets the frames of the host engine.
