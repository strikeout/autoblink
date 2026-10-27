//! The DMX planner logic of the AUTOBLINK box.
//!
//! This crate holds the show model: fixtures, groups and looks. It has no clock
//! and no I/O, like `autoblink-core-midi`. Thus the host can test it, and the
//! firmware can use it.

pub mod show;
pub use show::*;
pub mod render;
pub use render::*;
