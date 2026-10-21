//! The MIDI output logic of the ESP32 firmware: the settings, the clock and the note and CC
//! mapper. The code has no clock and no I/O. The caller gives the time in microseconds and
//! sends the `Out` messages, so the host tests can run it.
//!
//! The behaviour follows the host (`host/src/midi/`). The firmware receives only the beat
//! packets, so it has no phrase, no song structure and no mixer data.

pub mod clock;
pub mod config;
pub mod mapper;
pub mod settings;

pub use clock::Clock;
pub use config::{Cc, MidiConfig, CC_COUNT};
pub use mapper::{Mapper, Smoother, Snapshot};
pub use settings::SettingItem;

pub const CLOCK: u8 = 0xF8;
pub const START: u8 = 0xFA;
pub const CONTINUE: u8 = 0xFB;
pub const STOP: u8 = 0xFC;

/// One MIDI message for the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    /// A one-byte realtime message: `CLOCK`, `START`, `CONTINUE` or `STOP`.
    Realtime(u8),
    /// Song Position Pointer, in sixteenth notes.
    SongPosition(u16),
    /// A note on, followed at once by its note off. The channel is 0-based.
    Note { ch: u8, note: u8, vel: u8 },
    /// A control change. The channel is 0-based.
    Cc { ch: u8, num: u8, val: u8 },
}

impl Out {
    /// Writes the bytes of the message into `buf` and returns their number.
    pub fn encode(self, buf: &mut [u8; 6]) -> usize {
        match self {
            Out::Realtime(b) => {
                buf[0] = b;
                1
            }
            Out::SongPosition(pos) => {
                *buf = [0xF2, (pos & 0x7f) as u8, ((pos >> 7) & 0x7f) as u8, 0, 0, 0];
                3
            }
            Out::Note { ch, note, vel } => {
                let (ch, note) = (ch & 0x0f, note & 0x7f);
                *buf = [0x90 | ch, note, vel & 0x7f, 0x80 | ch, note, 0];
                6
            }
            Out::Cc { ch, num, val } => {
                *buf = [0xB0 | (ch & 0x0f), num & 0x7f, val & 0x7f, 0, 0, 0];
                3
            }
        }
    }
}
