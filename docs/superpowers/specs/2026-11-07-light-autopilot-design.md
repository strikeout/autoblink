# Light autopilot: design

## Context

The owner runs a club. A different DJ plays each night, and nobody operates the lights. The light must be good for the whole night without a person at the controls.

The room has two light systems:

- A DMX console runs some of the fixtures. The console holds 10 to 30 full light programs. A MIDI program change selects a program.
- AUTOBLINK drives the other fixtures directly with DMX.

The console DMX goes through the box: console → box DMX IN → box DMX OUT → all fixtures. Thus the box can also dim or black out the console fixtures.

The console has a sound-to-light mode. It reacts to noise, not to the beat. AUTOBLINK takes the beat, the phrases and the track energy from the players over Pro DJ Link. Thus crowd noise has no effect on it.

The mockups are in `.superpowers/brainstorm/78175-1794060713/content/`. They are `autopilot-layout.html` (the three setup views) and `move-editor.html` (the move editor).

## Goals

- The light follows the music all night. The owner sets the rules once, and they run every night.
- The music alone drives the light: the BPM and the track energy. Speed and intensity rise from the warm-up to the peak.
- The light does not repeat itself. The same look at each chorus for two hours is a failure.
- The console and the box change together on a drop. At other times, they run independently.
- Bar staff and DJs can use the live controls without risk.

## Non-goals

- Rules for one track, and a timeline for one track.
- Rules that use the clock or the time of night.
- Curves that the owner draws. The owner picks ready-made moves and sets a few knobs.
- A microphone or any other audio input.

## Anti-goals

The design fails, even when it works technically, in these cases:

- The light is out of time with the beat.
- The room looks the same for a long time.
- The strobe or the fog fires too often.
- A staff member breaks the setup or leaves the room in the wrong state.
- A blackout or a master change moves the heads, because it touched a pan or tilt channel.
- The room goes dark when the box restarts or loses Pro DJ Link.

## The model

### Energy and tiers

1. The box gives the music an energy spot from 0 to 100. The spot comes from the BPM of the master deck and the track energy at the playhead.
2. The spot is relative to the night. 0 is the quietest music so far, and 100 is the hardest music so far.
3. The spot has three tiers. Calm is the bottom third. Peak is the top fifth. Groove is the part between them.
4. The box keeps an "adaptive memory" of past nights. At the first track of a night, the box uses the range of past nights. Thus the warm-up starts as Calm.
5. The owner never sets a BPM number.

The track energy comes from the PWV4 profile of `autoblink_core_midi::guess::DropGuess`. The host builds the profile for each track (`host/src/state/dj_state.rs`). The box builds it only for a track without phrases (`esp32/src/tracks.rs`). The box must build it for each track. `DropGuess` must also give the energy at a position, because its profile is private.

### Console programs

- The owner adds each console program as a chip: the program number, a name and a colour.
- Each chip has a spot on the energy axis. A chip that the owner drops into a tier column gets a spot inside that tier.
- On each phrase change, the box picks a program near the current energy. It sends that program as a MIDI program change.
- The pick does not repeat. See "Decisions by the agent" for the rule.

### Box moves

A move is a ready-made shape on a target, with a few knobs. Like a program, each move has a spot on the energy axis. On each phrase change, the box picks one move for each target that has moves near the current energy. Thus the heads can sweep while the bars pulse. The box picks the moves independently of the program.

Under the moves, the box fixtures show the colour of the tier. Their level follows the energy spot. A move changes only the parameters of its target.

The shapes are Pulse, Ramp, Sweep, Circle, Figure 8, Chase, Wave, Strobe burst, Blackout and Hold value.

The target is one of these:

- The whole room: a master level or a blackout over the console fixtures and the box fixtures.
- A group, for example all heads or all bars.
- One fixture.

The owner then picks the parameter: Pan, Tilt, Pan and Tilt, Dimmer, Colour or Strobe. The editor shows only the parameters that the selected fixtures have.

The knobs are these:

- Speed: one cycle in ½, 1, 2, 4, 8, 16 or 32 beats. The option "faster when the energy rises" is on by default.
- Size: 0 to 100 %.
- Spread: together, fan out, mirror or random.
- Colour: from the tier, or a fixed colour.

### The drop

A drop is the one moment where the console and the box change together. Each `Event::Drop` of the scene engine is a drop, also a Chorus after a Chorus. The owner decided this.

1. Build: through the last 16 bars before the drop, the speed and the brightness climb. At the start of the build, the box dims the console fixtures to about 50 %. Then it brings them back to full by the drop.
2. Blackout: the whole room goes dark for a short time before the drop. The default is ½ bar.
3. Hit: on the drop, the console gets a Peak program, and the box starts a Peak move. A strobe burst of 2 beats comes with it.

The strobe of the hit has a cooldown (default 2 minutes). Inside the cooldown, the hit comes without the strobe. The build, the blackout and the hit stay.

The scene engine gives the Build and Drop buckets (`autoblink_core_midi::scenes::Event`). Build comes from the rekordbox phrases Up and Bridge, and Drop comes from Chorus. For a track without phrases, the buckets come from the drop guess.

### The whole room

The master and the blackout change only the light channels: Dimmer, Red, Green, Blue, White, Amber, Uv and Strobe. They never change Pan, Tilt or another channel. Thus the box must know the channels of the console fixtures. The owner adds each console fixture once, from the fixture library, and marks it as a console fixture. The box does not render looks or moves on a console fixture, except for a whole-room move.

The current code cannot do this yet:

- No DMX Mode can lower a DMX IN channel. Merge uses HTP, and Pass copies the input (`autoblink_core_ui::dmx`). The box needs a new stage. The stage scales the DMX IN light channels of the console fixtures before the merge.
- The current master scales only the Dimmer channel, or the colour channels of a fixture without a dimmer (`crates/autoblink-core-dmx/src/lights.rs`). The whole-room master must scale all light channels.

The whole room needs the console on DMX IN.

## The pages

### Setup views

The setup has three views of the same settings. A switch on the panel selects the view, and the page keeps the choice.

- A, three tier columns: Calm, Groove and Peak side by side. Each column holds its console programs and its box moves. The column that plays now glows.
- B, tabs: one tier at a time, with larger knobs. It suits a phone.
- C, energy curve: one axis from calm to peak, without tier borders. The owner drags programs and moves to a spot on the axis.

A change in one view shows in the other two. A chip that the owner drags in C goes to the tier column of its new spot in A and B.

Each view has a live line at the top. It shows the tier, the BPM, the energy meter, the current program and the current move. It also shows the bars to the next expected drop. Below the tiers, a strip shows the drop: build, blackout and hit. A line shows the safety limits of the strobe and the fog.

### Move editor

A tap on a move opens the editor (see `move-editor.html`). The editor has four steps from top to bottom: the shape, the target, the knobs and a preview.

- The rig map shows each fixture and tells if the box or the console drives it.
- The preview runs on the live beat as a curve and as moving beams.
- "Try now" sends the move to the rig for 8 bars.

### Live screen

The live screen is for bar staff and the DJ. It has large buttons only:

- Auto, Calm, Groove and Peak.
- Blackout, which acts only while a finger holds it.
- Fog, with its cooldown.

Auto uses the adaptive memory. A tier button pins that tier.

The box serves one page at a time, as now. The owner decided this. Thus the live screen and the setup are not open at the same time. A take-over of the page releases a held Blackout.

### Expert view

The phrase table and the look editor of the current page stay as the view "Manual rules", next to the autopilot.

## Where it runs

- The engine goes into the shared crates, so that the box and the host run the same code. The current DMX engine is in `autoblink-core-dmx`, and the scene engine is in `autoblink-core-midi`.
- The box sends the program changes on its MIDI output (`esp32/src/midi.rs`). Today the `midi` thread sends the MIDI clock, the phrase notes, the CCs and the scene notes. It sends no program change. The host sends one program change for each phrase kind (`host/src/midi/mapper.rs`, `program_change`).
- The host has no DMX IN. On the host, the whole-room target changes only the Art-Net output of the host.

## Decisions by the agent

The owner did not decide these. The owner can change each one.

- **The pick rule.** Inside the current tier, the box picks at random. It never picks one of the last two picks, and it prefers the chip that played least tonight.
- **A pinned tier ends.** A tier that staff pin goes back to Auto after 30 minutes. Thus the night cannot stay in the wrong tier.
- **The setup lock.** A PIN locks the setup views and the move editor. The live screen needs no PIN. The PIN only prevents accidents. The WiFi password of the box stays the real lock.
- **One driver at a time.** A switch selects "Autopilot" or "Manual rules". Both at the same time would send conflicting program changes and looks.
- **A drop without a known time.** The blackout needs the time of the drop. With rekordbox phrases, the box knows the beat of the next phrase start. With the drop guess only, the box skips the blackout and only hits on the drop. A blackout at the wrong moment looks like a fault.
- **The tier changes at a phrase change.** The energy spot moves all the time. The box changes the tier only at a phrase change or at a 16-beat block. A hysteresis stops the tier from flipping between two tiers.
- **The program change channel.** The autopilot has its own MIDI channel setting for the console.
- **A night.** A night starts when a deck plays after 2 hours without a playing deck. The box writes the range of the night to NVS from time to time. After a restart in the night, the box reads the range again. Thus a restart at 01:00 does not send the peak back to Calm.
- **A drop under a pin.** A pin of Calm or Groove turns off the drop sequence. Then the box gives no build, no blackout, no strobe and no Peak hit. A pin of Peak keeps the drop sequence.

## Constraints from the code

- Do not use an `f32` field in a serde type of the firmware. LLVM for Xtensa cannot compile it.
- The firmware app image uses 89 % of the slot (1 748 832 bytes in 1.30.0). The page is in the image. Keep the three views small. Above 90 %, examine `CONFIG_COMPILER_OPTIMIZATION_SIZE` first.
- The show JSON is at most 60 000 bytes (`POST /show`). The autopilot settings go into the show.
- The adaptive memory goes into NVS. Keep it to a few hundred bytes, and write it at most once each few minutes. Each flash write can make the MIDI clock stutter.
- Use the FreeRTOS queue to the `dmx` thread, not `std::sync::mpsc`.
- The status message is a `Serialize` struct, not a `serde_json::Value` tree.
- The host must stay alive under every fault (`runtime::supervise`).

## Left to the builder

- The formula of the energy spot from the BPM and the track energy.
- The format and the decay of the adaptive memory.
- The hysteresis of the tiers.
- The math of the new shapes: Pulse, Ramp, Wave, Strobe burst, Blackout and Hold value.
- The math of the option "faster when the energy rises". A speed must stay on the beat grid.
- The limits: the number of programs, moves and console fixtures.
- How "Manual rules" and the autopilot share the settings that already exist.

## Verification

1. Run `cargo test --locked`. Tests cover the energy spot, the tiers, the pick rule, the drop sequence and the whole-room rule.
2. Run `sh scripts/check.sh --esp32`. The image must stay below 90 % of the slot.
3. Play a night on the XDJ-AZ from a calm track to a hard track. The tier must climb, and the program and the move must change at phrase changes without a repeat.
4. Play a track with rekordbox phrases and a drop. The build, the blackout and the hit must come at the right beats.
5. Play a track without phrases. The hit must come without a blackout.
6. Hold Blackout on the live screen. All light channels must go dark, and no head may move.
7. Pin Calm and wait 30 minutes. The box must go back to Auto.
