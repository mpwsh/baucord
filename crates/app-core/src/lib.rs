//! # app-core — the platform-agnostic application layer
//!
//! Everything in this crate answers "would a phone need this?" with
//! *yes*: view-model types, the chat history model, link detection,
//! emoji shortcodes and catalog, the bounded image/GIF pipelines, the
//! GIF frame decoder that drives inline animation, the audio-device
//! diff reducer, roster sorting, stats formatting, and the update-URL
//! helpers.
//!
//! What deliberately is **not** here: anything touching a UI toolkit,
//! the OS clipboard, global hotkeys, trays, file dialogs, process
//! introspection, or the self-updater's install step — those live in
//! the shells (`desktop` now; `mobile-*` later).
//!
//! Localization stays out too: fallible operations return typed
//! [`errors`], and the shell maps them to fluent strings — so `i18n`
//! remains a shell concern and this crate has no message catalog.

pub mod chat;
pub mod devices;
pub mod emoji;
mod emoji_table;
pub mod fps;
pub mod gif;
pub mod images;
pub mod roster;
pub mod stats;
pub mod update;

pub use emoji_table::EMOJI;
