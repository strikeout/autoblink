//! The beat tracker of the box microphone. It has no ESP-IDF dependency, so the tests run on the
//! computer.
//!
//! The firmware reads 16-bit mono PCM at 16 kHz from a PDM mic. The tracker finds the tempo, the
//! beat phase and bar 1 from the kick drum. The calibrator compares the mic grid with the Pro DJ
//! Link beats while a deck plays.

pub mod onset;
#[cfg(test)]
pub(crate) mod synth;

pub use onset::{Frame, Onsets};

/// The sample rate of the PCM, in Hz.
pub const SAMPLE_RATE: u32 = 16_000;
/// The samples in one onset frame (8 ms).
pub const HOP: usize = 128;
/// The lowest tempo of the tracker. A slower tempo folds to its double.
pub const BPM_MIN: f64 = 80.0;
/// The highest tempo of the tracker. A faster tempo folds to its half.
pub const BPM_MAX: f64 = 160.0;
/// The time of constant PCM that gives `NoMic`, in ms.
pub const NO_MIC_MS: u64 = 5_000;
/// The time below `SILENCE_DBFS` that stops the flywheel, in ms.
pub const SILENCE_MS: u64 = 10_000;
/// The level of silence, in dBFS.
pub const SILENCE_DBFS: f32 = -50.0;
/// The predicted beats without a kick that start a Break.
pub const BREAK_BEATS: u32 = 8;
/// The firmware logs a flywheel that lasts longer than this, in ms.
pub const LONG_FLYWHEEL_MS: u64 = 300_000;
/// The offset from the mic grid to the Pro DJ Link grid before the first calibration, in µs.
pub const DEFAULT_OFFSET_US: i64 = 30_000;
