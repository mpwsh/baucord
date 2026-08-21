// hotkeys.rs — Global hotkey registration and dispatch.
//
// Only Pressed events are forwarded — Released events are ignored so
// the hotkeys act as toggles, not push-to-talk.
//
// Hotkeys are strictly optional: if the OS refuses to create a global
// hotkey manager (common on Wayland, some remote sessions, etc.) the
// app runs without them instead of crashing at startup.

use crossbeam_channel::{Receiver, Sender, unbounded};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use log::{info, warn};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Once, OnceLock};

use client::config::{BindableAction, Settings};

/// Our own event channel. `GlobalHotKeyEvent::set_event_handler`
/// *replaces* the crate's internal channel delivery, so the handler
/// forwards into this one — and, crucially, calls the injected wake
/// callback the instant a key fires (Slint edition of the old
/// `fltk::app::awake()` nudge). Without it a press would sit in the
/// channel until the next unrelated UI event woke the loop (worst
/// case: the 2.5 s device-poll timer, i.e. a mute key that lands
/// seconds late while the app idles in the tray).
fn event_channel() -> &'static (Sender<GlobalHotKeyEvent>, Receiver<GlobalHotKeyEvent>) {
    static CHANNEL: OnceLock<(Sender<GlobalHotKeyEvent>, Receiver<GlobalHotKeyEvent>)> =
        OnceLock::new();
    CHANNEL.get_or_init(unbounded)
}

/// Thread-safe wake callback the OS hotkey thread fires into.
pub type WakeFn = std::sync::Arc<dyn Fn() + Send + Sync>;

static WAKE: OnceLock<WakeFn> = OnceLock::new();

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
    pub fn new(settings: &Settings, wake: WakeFn) -> Self {
        // The handler is process-global state in the global-hotkey
        // crate; guard against reinstall if a second manager is ever
        // created (harmless today — new() runs once per app).
        static INSTALL_HANDLER: Once = Once::new();
        let _ = WAKE.set(wake);
        INSTALL_HANDLER.call_once(|| {
            GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
                let _ = event_channel().0.send(event);
                // Thread-safe by design — this runs on whatever thread
                // the OS delivers hotkey events on.
                if let Some(wake) = WAKE.get() {
                    wake();
                }
            }));
        });

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
    ///
    /// Reads our own channel (see [`event_channel`]) — with an event
    /// handler installed, the crate's built-in receiver never fires.
    pub fn poll(&self) -> Option<BindableAction> {
        if let Ok(event) = event_channel().1.try_recv()
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

            let Some(parsed) = entry.parse() else {
                warn!(
                    "Invalid keybind '{}' for {}",
                    entry.bind,
                    entry.action.label()
                );
                continue;
            };

            let Ok(code) = Code::from_str(&parsed.code) else {
                warn!(
                    "Unknown key code '{}' in '{}' for {}",
                    parsed.code,
                    entry.bind,
                    entry.action.label()
                );
                continue;
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
                        "Registered global hotkey: {} → {}",
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
