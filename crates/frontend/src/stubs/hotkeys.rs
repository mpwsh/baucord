//! `hotkeys` without global hotkeys or key polling: a manager that
//! never fires. Push-to-talk on a phone is a button, not a key.

use client::config::{BindableAction, Settings};

pub type WakeFn = std::sync::Arc<dyn Fn() + Send + Sync>;

pub struct HotkeyManager;

impl HotkeyManager {
    #[must_use]
    pub fn new(_settings: &Settings, _wake: WakeFn) -> Self {
        Self
    }

    pub fn update(&mut self, _settings: &Settings) {}

    #[must_use]
    pub fn poll(&self) -> Option<(BindableAction, bool)> {
        None
    }
}
