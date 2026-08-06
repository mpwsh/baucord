//! baucord core — the UI-free client engine.
//!
//! Everything a frontend needs to run a baucord session: settings and
//! persistence ([`config`]), the network thread ([`network`]), the
//! voice-engine bridge ([`voice`]) and the screen-share thread
//! ([`screen`]).
//!
//! Frontends talk to the worker threads exclusively through their
//! control / event channels and inject a wake callback, so this crate
//! depends on no UI toolkit. The FLTK app lives in the sibling
//! `desktop` crate; a future mobile shell sits on the same seam.

pub mod config;
pub mod network;
pub mod screen;
pub mod voice;
