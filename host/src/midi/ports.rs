//! MIDI output port listing for the UI.

/// Returns the names of the MIDI output ports, in the order of the system.
///
/// The call blocks (CoreMIDI, ALSA). Call it from `spawn_blocking`.
pub fn list_output_ports() -> Vec<String> {
    let Ok(midi_out) = midir::MidiOutput::new("xdj-clock-ports") else {
        return Vec::new();
    };
    midi_out
        .ports()
        .iter()
        .filter_map(|p| midi_out.port_name(p).ok())
        .collect()
}
