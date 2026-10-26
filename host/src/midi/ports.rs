//! MIDI output port listing for the UI.

/// Returns the names of the MIDI output ports, in the order of the system.
///
/// The call blocks (CoreMIDI, ALSA). Call it from `spawn_blocking`.
pub fn list_output_ports() -> Vec<String> {
    let Ok(midi_out) = midir::MidiOutput::new("autoblink-ports") else {
        return Vec::new();
    };
    midi_out
        .ports()
        .iter()
        .filter_map(|p| midi_out.port_name(p).ok())
        .collect()
}

/// Opens the output port at `index` when it still has the name `name`; otherwise opens the
/// port by name. Two ports can have the same name (Windows names two identical USB
/// interfaces the same), so the index selects among them.
///
/// The call blocks. Call it from `spawn_blocking`.
pub fn open_output(
    name: &str,
    index: Option<usize>,
) -> anyhow::Result<(midir::MidiOutputConnection, String)> {
    if let Some(i) = index {
        let midi_out = midir::MidiOutput::new("autoblink")?;
        let ports = midi_out.ports();
        if let Some(port) = ports.get(i) {
            if midi_out.port_name(port).ok().as_deref() == Some(name) {
                return midi_out
                    .connect(port, "autoblink")
                    .map(|conn| (conn, name.to_string()))
                    .map_err(|e| anyhow::anyhow!("{e}"));
            }
        }
    }
    super::open_midi_output(name)
}
