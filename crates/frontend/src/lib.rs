//! baucord's UI logic: the client threads, the Slint models and the
//! windows, for every host. `desktop` wraps it in a binary with the
//! OS pieces (`main.rs`: logging, the config directory, the single
//! instance, the panic hook); Android will wrap it in a cdylib
//! (ANDROID-SPEC.md). `app::run` is the entry point; `app::with_current`
//! reaches the live state from callbacks.

pub mod adminui;
pub mod app;
pub mod clipboard;
pub mod hotkeys;
pub mod instance;
pub mod media;
pub mod present;
pub mod resources;
pub mod settingsui;
pub mod updater;
pub mod viewers;
