//! The DMX planner logic of the AUTOBLINK box.
//!
//! This crate holds the show model: fixtures, groups and looks. It has no clock
//! and no I/O, like `autoblink-core-midi`. Thus the host can test it, and the
//! firmware can use it.

pub mod auto;
pub use auto::*;
pub mod night;
pub use night::*;
pub mod show;
pub use show::*;
pub mod render;
pub use render::*;
pub mod clock;
pub use clock::*;
pub mod effect;
pub use effect::*;
pub mod moves;
pub use moves::*;
pub mod lights;
pub use lights::*;
pub mod artnet;
pub use artnet::*;
pub mod merge;
pub use merge::*;
pub mod source;
pub use source::*;
pub mod pilot;
pub use pilot::*;
pub mod watch;
pub use watch::*;
pub mod sacn;
pub use sacn::*;
