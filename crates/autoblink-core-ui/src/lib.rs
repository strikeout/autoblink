//! The logic of the box hardware of the ESP32 firmware: the DMX merge and the box settings. The
//! code has no I/O, so the host tests can run it.

pub mod config;
pub mod dmx;

pub use config::BoxConfig;
pub use dmx::{merge, DmxMode};
