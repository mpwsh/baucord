//! baucord's UI logic: the client threads, the Slint models and the
//! windows, for every host. `desktop` wraps it in a binary with the
//! OS pieces (`main.rs`: logging, the config directory, the single
//! instance, the panic hook); Android will wrap it in a cdylib
//! (ANDROID-SPEC.md). `app::run` is the entry point; `app::with_current`
//! reaches the live state from callbacks.

pub mod adminui;
pub mod app;
/// The log file Settings → Debug reads, sends and clears: each host
/// says where it writes it (`logfile::set_path`).
pub use app_core::logfile;
pub mod host;
pub mod media;
/// What the app asks a phone's notification system for (`host::notify`).
pub mod notice;
pub mod pictures;
pub mod present;
pub mod resources;
pub mod settingsui;
pub mod viewers;

// Host-only modules have a twin under `stubs/` with the same API and
// no host: that is what a build without the `desktop` feature gets,
// and what Android starts from until the `Platform` trait
// (ANDROID-SPEC.md §5) hands these to the Android host.
#[cfg(feature = "desktop")]
pub mod clipboard;
#[cfg(not(feature = "desktop"))]
#[path = "stubs/clipboard.rs"]
pub mod clipboard;

#[cfg(feature = "desktop")]
pub mod hotkeys;
#[cfg(not(feature = "desktop"))]
#[path = "stubs/hotkeys.rs"]
pub mod hotkeys;

#[cfg(feature = "desktop")]
pub mod updater;
#[cfg(not(feature = "desktop"))]
#[path = "stubs/updater.rs"]
pub mod updater;

/// One instance per config directory: a desktop binary's concern.
#[cfg(feature = "desktop")]
pub mod instance;
