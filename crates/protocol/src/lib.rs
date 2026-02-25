//! Shared wire protocol types for baucord client ↔ server communication.
//!
//! This crate is **types + constants only** — no networking logic, no I/O,
//! no behavior.  Both `baucord-client` and `baucord-server` depend on it
//! to ensure protocol definitions stay in sync at compile time.

pub mod audio;
pub mod channels;
pub mod messages;
pub mod video;
pub mod voice;

pub use audio::*;
pub use channels::*;
pub use messages::*;
pub use video::*;
pub use voice::*;
