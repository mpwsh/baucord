//! `hotkeys` without global hotkeys or key polling: a manager that
//! never fires. Push-to-talk on a phone is a button, not a key.

use client::config::{BindableAction, Settings};

pub type WakeFn = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Nothing to grant where no key is watched.
#[must_use]
pub fn access_granted() -> bool {
    true
}

pub fn ask_for_access() {}

pub struct HotkeyManager;

impl HotkeyManager {
    #[must_use]
    pub fn new(_settings: &Settings, _wake: WakeFn) -> Self {
        Self
    }

    pub fn update(&mut self, _settings: &Settings) {}

    pub fn rearm(&mut self) {}

    pub fn set_connected(&mut self, _connected: bool) {}

    #[must_use]
    pub fn poll(&self) -> Option<(BindableAction, bool)> {
        None
    }
}
