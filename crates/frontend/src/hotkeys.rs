// hotkeys.rs — Global hotkey registration and dispatch.
//
// Both edges are forwarded with a `pressed` flag: the toggles act on
// the press only, push-to-talk on both.
//
// Bindings are *watched*, not registered: a registered global hotkey
// is consumed by the OS (`RegisterHotKey`, `RegisterEventHotKey`,
// `XGrabKey` are grabs — there is no listen-only mode), so a PTT key
// of `N` stopped typing `n` in every other program. The `keywatch`
// crate's thread reads the state of the bound keys instead and
// reports the edges; nothing is swallowed, for any binding. It sleeps
// until a key event says there is something to read (review P1-3;
// the how and the per-platform halves are that crate's). Where the
// state cannot be read (Wayland, a macOS without the accessibility
// grant) the bindings fall back to registered hotkeys, with a log
// line saying so.
//
// Keybinds act only in a session, so all of this runs only while
// connected, and the push-to-talk key is watched only while
// push-to-talk is on: the watcher used to poll the whole keyboard
// every 12 ms from launch to exit, in the tray too.
//
// Hotkeys are strictly optional: if the OS refuses to create a global
// hotkey manager (common on Wayland, some remote sessions, etc.) the
// app runs without them instead of crashing at startup.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Once, OnceLock},
};

use client::config::{BindableAction, KeybindEntry, Settings};
use crossbeam_channel::{unbounded, Receiver, Sender};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use keywatch::{platform_key, Binding, Watcher};
use log::{info, warn};

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

/// Whether keybinds can be watched without the user's say-so. Only
/// macOS wants one (the Accessibility permission); without it a
/// binding is a registered hotkey there, which works and takes its key
/// from every other program. Never prompts (review P2-30).
#[must_use]
pub fn access_granted() -> bool {
    keywatch::access_granted()
}

/// Put up the system's prompt for that permission. For the button in
/// Settings → Keyboard, never for a launch or a rebind.
pub fn ask_for_access() {
    keywatch::ask_for_access();
}

pub struct HotkeyManager {
    /// The OS-level manager, created only when a binding falls back to
    /// registration, and dropped at disconnect: its X11 backend runs a
    /// 50 ms poll thread from creation, registered hotkeys or not.
    /// `None` also when the OS refused one — hotkeys are then simply
    /// disabled.
    manager: Option<GlobalHotKeyManager>,
    /// The key watcher, when the key state can be read: every binding
    /// it can name goes through it, and none of those is registered.
    watcher: Option<Watcher>,
    watch_rx: Receiver<(BindableAction, bool)>,
    watch_tx: Sender<(BindableAction, bool)>,
    /// Hotkeys we actually registered, kept so `update()` can unregister
    /// them properly instead of leaking OS-level grabs.
    registered: Vec<HotKey>,
    bindings: HashMap<u32, BindableAction>,
    /// The bindings in force: the non-empty ones of the settings, and
    /// push-to-talk's only while push-to-talk is on.
    binds: Vec<KeybindEntry>,
    /// Connected: the bindings are watched (or registered).
    active: bool,
}

impl HotkeyManager {
    /// Nothing is watched or registered until [`Self::set_connected`].
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

        let (watch_tx, watch_rx) = unbounded::<(BindableAction, bool)>();
        Self {
            manager: None,
            watcher: None,
            watch_rx,
            watch_tx,
            registered: Vec::new(),
            bindings: HashMap::new(),
            binds: wanted(&settings.keybinds, settings.audio.push_to_talk),
            active: false,
        }
    }

    /// The keybinds or push-to-talk changed: re-arm with the new set
    /// (while connected).
    pub fn update(&mut self, settings: &Settings) {
        let binds = wanted(&settings.keybinds, settings.audio.push_to_talk);
        if binds == self.binds {
            return;
        }
        self.binds = binds;
        if self.active {
            self.unregister_all();
            self.register_all();
        }
    }

    /// Arm the same bindings again (while connected): what
    /// [`access_granted`] says has changed, so a binding that was
    /// registered can be watched now.
    pub fn rearm(&mut self) {
        if self.active {
            self.unregister_all();
            self.register_all();
        }
    }

    /// A session began (`true`) or ended: the keys are watched only in
    /// between, where their edges mean something.
    pub fn set_connected(&mut self, connected: bool) {
        if connected == self.active {
            return;
        }
        self.active = connected;
        if connected {
            self.register_all();
        } else {
            self.unregister_all();
            self.manager = None;
        }
    }

    /// Poll for a hotkey edge: `(action, pressed)`.
    ///
    /// Reads our own channel (see [`event_channel`]) — with an event
    /// handler installed, the crate's built-in receiver never fires.
    #[must_use]
    pub fn poll(&self) -> Option<(BindableAction, bool)> {
        if let Ok(edge) = self.watch_rx.try_recv() {
            return Some(edge);
        }
        if let Ok(event) = event_channel().1.try_recv()
            && let Some(&action) = self.bindings.get(&event.id())
        {
            return Some((action, event.state() == HotKeyState::Pressed));
        }
        None
    }

    /// Hand everything the watcher can name to one watcher thread;
    /// the actions it took. Empty where the key state cannot be read.
    fn start_watcher(&mut self) -> Vec<BindableAction> {
        let watched: Vec<Binding<BindableAction>> = self
            .binds
            .iter()
            .filter_map(|e| {
                let parsed = e.parse()?;
                Some(Binding {
                    action: e.action,
                    key: platform_key(&parsed.code)?,
                    ctrl: parsed.ctrl,
                    shift: parsed.shift,
                    alt: parsed.alt,
                    // Push-to-talk is a hold: its release is an edge
                    // too. The toggles act on the press alone.
                    hold: e.action == BindableAction::PushToTalk,
                })
            })
            .collect();
        let mut taken: Vec<BindableAction> = Vec::new();
        if !watched.is_empty() {
            taken = watched.iter().map(|w| w.action).collect();
            let tx = self.watch_tx.clone();
            let report = move |action: BindableAction, pressed: bool| {
                let sent = tx.send((action, pressed)).is_ok();
                // Same as a registered hotkey's delivery: wake the UI
                // thread now, not at its next timer.
                if let Some(wake) = WAKE.get() {
                    wake();
                }
                sent
            };
            if let Some(w) = Watcher::start(watched, report) {
                info!(
                    "Watching {} keybind(s), {}",
                    taken.len(),
                    if w.by_events() {
                        "woken by key events"
                    } else {
                        "on a clock"
                    }
                );
                self.watcher = Some(w);
            } else {
                info!("Key state not readable here; registering hotkeys instead");
                taken.clear();
            }
        }
        taken
    }

    fn register_all(&mut self) {
        let taken = self.start_watcher();
        let fallback: Vec<KeybindEntry> = self
            .binds
            .iter()
            .filter(|e| !taken.contains(&e.action))
            .cloned()
            .collect();
        if fallback.is_empty() {
            return;
        }
        if self.manager.is_none() {
            match GlobalHotKeyManager::new() {
                Ok(m) => self.manager = Some(m),
                Err(e) => {
                    warn!("Global hotkeys unavailable: {e}");
                    return;
                }
            }
        }
        let Some(manager) = &self.manager else {
            return;
        };

        for entry in &fallback {
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
        self.watcher = None;
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

/// The bindings to arm: the bound ones, push-to-talk's only while
/// push-to-talk is on (its key is not watched, nor grabbed, otherwise).
fn wanted(keybinds: &[KeybindEntry], push_to_talk: bool) -> Vec<KeybindEntry> {
    keybinds
        .iter()
        .filter(|e| !e.bind.is_empty())
        .filter(|e| push_to_talk || e.action != BindableAction::PushToTalk)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_captured_key_is_watchable() {
        // What Settings' capture can produce (settingsui::key_code_for).
        let letters = (b'A'..=b'Z').map(|c| format!("Key{}", c as char));
        let digits = (0..10).map(|d| format!("Digit{d}"));
        let fkeys = (1..=12).map(|n| format!("F{n}"));
        let named = [
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Insert",
            "Delete",
            "Tab",
            "Enter",
            "Space",
            "Minus",
            "Equal",
            "Comma",
            "Period",
            "Slash",
            "Semicolon",
            "Quote",
            "BracketLeft",
            "BracketRight",
            "Backslash",
            "Backquote",
        ]
        .map(String::from);
        for code in letters.chain(digits).chain(fkeys).chain(named) {
            assert!(platform_key(&code).is_some(), "{code}");
        }
    }

    #[test]
    fn push_to_talk_is_armed_only_while_on() {
        let binds = [
            KeybindEntry {
                action: BindableAction::ToggleMute,
                bind: "F7".into(),
            },
            KeybindEntry {
                action: BindableAction::ToggleDeafen,
                bind: String::new(),
            },
            KeybindEntry {
                action: BindableAction::PushToTalk,
                bind: "KeyV".into(),
            },
        ];
        let actions =
            |ptt| -> Vec<BindableAction> { wanted(&binds, ptt).iter().map(|e| e.action).collect() };
        assert_eq!(actions(false), [BindableAction::ToggleMute]);
        assert_eq!(
            actions(true),
            [BindableAction::ToggleMute, BindableAction::PushToTalk]
        );
    }
}
