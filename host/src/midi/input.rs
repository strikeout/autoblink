//! Reads the EQ knobs and the channel faders of an XDJ-AZ from its USB MIDI port.

// The XDJ-AZ does not send these positions over Pro DJ Link. It sends them as 7-bit MIDI
// control change messages on MIDI channel 5, also in standalone mode. It sends a value only
// when a control moves.

use std::sync::Arc;
use std::time::Duration;

use midir::{MidiInput, MidiInputConnection, MidiInputPort};

use crate::state::{MixerChannel, SharedState};

// ponytail: the thread opens only a port with "XDJ-AZ" in its name. To support another
// mixer, add a `midi.input` config key and a CC table for each model.
const PORT_MATCH: &str = "XDJ-AZ";

/// The status byte of a control change message on MIDI channel 5.
const STATUS_CC_CH5: u8 = 0xB4;

const POLL_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Hi,
    Mid,
    Low,
    Fader,
}

/// The CC number, the mixer channel index (CH1 = 0) and the control.
/// The numbers come from captures of an XDJ-AZ. They do not follow a fixed stride.
const XDJ_AZ_CC: [(u8, usize, Control); 16] = [
    (0x02, 0, Control::Hi),
    (0x03, 0, Control::Mid),
    (0x04, 0, Control::Low),
    (0x11, 0, Control::Fader),
    (0x07, 1, Control::Hi),
    (0x08, 1, Control::Mid),
    (0x09, 1, Control::Low),
    (0x12, 1, Control::Fader),
    (0x0E, 2, Control::Hi),
    (0x0F, 2, Control::Mid),
    (0x15, 2, Control::Low),
    (0x13, 2, Control::Fader),
    (0x51, 3, Control::Hi),
    (0x5C, 3, Control::Mid),
    (0x52, 3, Control::Low),
    (0x14, 3, Control::Fader),
];

/// Stores one MIDI message in `mixer` if the message is a known XDJ-AZ mixer control.
/// Returns true when a value changes.
pub fn apply_cc(mixer: &mut [MixerChannel; 4], msg: &[u8]) -> bool {
    let &[STATUS_CC_CH5, cc, value] = msg else {
        return false;
    };
    let Some(&(_, ch, control)) = XDJ_AZ_CC.iter().find(|(n, _, _)| *n == cc) else {
        return false;
    };
    let slot = match control {
        Control::Hi => &mut mixer[ch].hi,
        Control::Mid => &mut mixer[ch].mid,
        Control::Low => &mut mixer[ch].low,
        Control::Fader => &mut mixer[ch].fader,
    };
    let changed = *slot != Some(value);
    *slot = Some(value);
    changed
}

/// Starts a thread that connects to the XDJ-AZ MIDI port and writes its mixer values into
/// `dj_state`. The thread polls for the port, so a disconnected unit connects again.
pub fn spawn(dj_state: SharedState) {
    let result = std::thread::Builder::new()
        .name("midi-input".into())
        .spawn(move || run(dj_state));
    if let Err(e) = result {
        tracing::warn!(error = %e, "The MIDI input thread did not start.");
    }
}

fn run(dj_state: SharedState) {
    let mut conn: Option<MidiInputConnection<()>> = None;
    let mut warned = false;
    loop {
        match (find_port(), conn.is_some()) {
            (None, true) => {
                conn = None;
                tracing::info!("The XDJ-AZ MIDI input is disconnected.");
            }
            (Some((midi_in, port)), false) => match connect(midi_in, &port, &dj_state) {
                Ok(c) => {
                    conn = Some(c);
                    warned = false;
                    tracing::info!("The XDJ-AZ MIDI input is connected.");
                }
                Err(e) if !warned => {
                    warned = true;
                    tracing::warn!(error = %e, "The XDJ-AZ MIDI input did not connect.");
                }
                Err(_) => {}
            },
            _ => {}
        }
        std::thread::sleep(POLL_PERIOD);
    }
}

fn find_port() -> Option<(MidiInput, MidiInputPort)> {
    let midi_in = MidiInput::new("xdj-clock-in").ok()?;
    let port = midi_in
        .ports()
        .into_iter()
        .find(|p| midi_in.port_name(p).is_ok_and(|n| n.contains(PORT_MATCH)))?;
    Some((midi_in, port))
}

fn connect(
    midi_in: MidiInput,
    port: &MidiInputPort,
    dj_state: &SharedState,
) -> anyhow::Result<MidiInputConnection<()>> {
    let dj_state = Arc::clone(dj_state);
    midi_in
        .connect(
            port,
            "xdj-clock-mixer",
            move |_, msg, _| {
                // A panic on the CoreMIDI thread aborts the process.
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    apply_cc(&mut dj_state.write().mixer, msg);
                }));
                if caught.is_err() {
                    tracing::error!("The MIDI input callback panicked. The message is ignored.");
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(mixer: &[MixerChannel; 4], ch: usize, control: Control) -> Option<u8> {
        match control {
            Control::Hi => mixer[ch].hi,
            Control::Mid => mixer[ch].mid,
            Control::Low => mixer[ch].low,
            Control::Fader => mixer[ch].fader,
        }
    }

    #[test]
    fn each_table_entry_sets_only_its_own_control() {
        for (cc, ch, control) in XDJ_AZ_CC {
            let mut mixer = [MixerChannel::default(); 4];
            assert!(apply_cc(&mut mixer, &[STATUS_CC_CH5, cc, 0x2A]));
            assert_eq!(read(&mixer, ch, control), Some(0x2A), "cc {cc:#04x}");
            let set = mixer
                .iter()
                .flat_map(|c| [c.hi, c.mid, c.low, c.fader])
                .filter(Option::is_some)
                .count();
            assert_eq!(set, 1, "cc {cc:#04x} set more than one control");
        }
    }

    #[test]
    fn table_has_no_duplicate_cc_or_control() {
        for (i, a) in XDJ_AZ_CC.iter().enumerate() {
            for b in &XDJ_AZ_CC[i + 1..] {
                assert_ne!(a.0, b.0, "duplicate cc {:#04x}", a.0);
                assert_ne!((a.1, a.2), (b.1, b.2), "duplicate control");
            }
        }
    }

    #[test]
    fn other_messages_change_nothing() {
        let mut mixer = [MixerChannel::default(); 4];
        for msg in [
            &[0xB0, 0x02, 0x10][..],      // CC on MIDI channel 1
            &[STATUS_CC_CH5, 0x7F, 0x10], // CC number not in the table
            &[0x94, 0x02, 0x10],          // note on, MIDI channel 5
            &[STATUS_CC_CH5, 0x02],       // short message
            &[0xF8],                      // timing clock
        ] {
            assert!(!apply_cc(&mut mixer, msg));
        }
        assert_eq!(mixer, [MixerChannel::default(); 4]);
    }

    #[test]
    fn a_repeated_value_is_not_a_change() {
        let mut mixer = [MixerChannel::default(); 4];
        assert!(apply_cc(&mut mixer, &[STATUS_CC_CH5, 0x02, 0x40]));
        assert!(!apply_cc(&mut mixer, &[STATUS_CC_CH5, 0x02, 0x40]));
        assert!(apply_cc(&mut mixer, &[STATUS_CC_CH5, 0x02, 0x41]));
    }
}
