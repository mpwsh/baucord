// hotkeys.rs — Global hotkey registration and dispatch.
//
// Only Pressed events are forwarded — Released events are ignored so
// the hotkeys act as toggles, not push-to-talk.
//
// Hotkeys are strictly optional: if the OS refuses to create a global
// hotkey manager (common on Wayland, some remote sessions, etc.) the
// app runs without them instead of crashing at startup.

use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use log::{info, warn};
use std::collections::HashMap;
use std::str::FromStr;

use crate::config::{BindableAction, Settings};

pub struct HotkeyManager {
    /// `None` when the OS-level manager could not be created — hotkeys
    /// are simply disabled in that case.
    manager: Option<GlobalHotKeyManager>,
    /// Hotkeys we actually registered, kept so `update()` can unregister
    /// them properly instead of leaking OS-level grabs.
    registered: Vec<HotKey>,
    bindings: HashMap<u32, BindableAction>,
}

impl HotkeyManager {
    pub fn new(settings: &Settings) -> Self {
        let manager = match GlobalHotKeyManager::new() {
            Ok(m) => Some(m),
            Err(e) => {
                warn!("Global hotkeys unavailable: {e}");
                None
            }
        };

        let mut hm = Self {
            manager,
            registered: Vec::new(),
            bindings: HashMap::new(),
        };
        hm.register_all(settings);
        hm
    }

    pub fn update(&mut self, settings: &Settings) {
        self.unregister_all();
        self.register_all(settings);
    }

    /// Poll for hotkey press events. Ignores releases.
    pub fn poll(&self) -> Option<BindableAction> {
        if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv()
            && event.state() == HotKeyState::Pressed
            && let Some(&action) = self.bindings.get(&event.id())
        {
            return Some(action);
        }
        None
    }

    fn register_all(&mut self, settings: &Settings) {
        let Some(manager) = &self.manager else {
            return;
        };

        for entry in &settings.keybinds {
            if entry.bind.is_empty() {
                continue;
            }

            let parsed = match entry.parse() {
                Some(p) => p,
                None => {
                    warn!(
                        "Invalid keybind '{}' for {}",
                        entry.bind,
                        entry.action.label()
                    );
                    continue;
                }
            };

            let code = match Code::from_str(&parsed.code) {
                Ok(c) => c,
                Err(_) => {
                    warn!(
                        "Unknown key code '{}' in '{}' for {}",
                        parsed.code,
                        entry.bind,
                        entry.action.label()
                    );
                    continue;
                }
            };

            let mut mods = Modifiers::empty();
            if parsed.ctrl {
                mods |= Modifiers::CONTROL;
            }
            if parsed.shift {
                mods |= Modifiers::SHIFT;
            }
            if parsed.alt {
                mods |= Modifiers::ALT;
            }

            let modifiers = if mods.is_empty() { None } else { Some(mods) };
            let hotkey = HotKey::new(modifiers, code);

            match manager.register(hotkey) {
                Ok(()) => {
                    self.registered.push(hotkey);
                    self.bindings.insert(hotkey.id(), entry.action);
                    info!(
                        "🔑 Registered global hotkey: {} → {}",
                        entry.bind,
                        entry.action.label()
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to register hotkey '{}' for {}: {e}",
                        entry.bind,
                        entry.action.label()
                    );
                }
            }
        }
    }

    fn unregister_all(&mut self) {
        if let Some(manager) = &self.manager {
            for hotkey in self.registered.drain(..) {
                if let Err(e) = manager.unregister(hotkey) {
                    warn!("Failed to unregister hotkey: {e}");
                }
            }
        }
        self.registered.clear();
        self.bindings.clear();
    }
}
