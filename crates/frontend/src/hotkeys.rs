// hotkeys.rs — Global hotkey registration and dispatch.
//
// Both edges are forwarded with a `pressed` flag: the toggles act on
// the press only, push-to-talk on both.
//
// Bindings are *watched*, not registered: a registered global hotkey
// is consumed by the OS (`RegisterHotKey`, `RegisterEventHotKey`,
// `XGrabKey` are grabs — there is no listen-only mode), so a PTT key
// of `N` stopped typing `n` in every other program. A small thread
// polls the keyboard state instead (`device_query`:
// `GetAsyncKeyState`, `CGEventSourceKeyState`, `XQueryKeymap`) and
// reports the edges; nothing is swallowed, for any binding. Where the
// state cannot be read (Wayland, a macOS without the accessibility
// grant) the bindings fall back to registered hotkeys, as before,
// with a log line saying so.
//
// Hotkeys are strictly optional: if the OS refuses to create a global
// hotkey manager (common on Wayland, some remote sessions, etc.) the
// app runs without them instead of crashing at startup.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Once, OnceLock,
    },
    time::Duration,
};

use client::config::{BindableAction, Settings};
use crossbeam_channel::{unbounded, Receiver, Sender};
use device_query::{DeviceQuery, DeviceState, Keycode};
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
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

pub struct HotkeyManager {
    /// `None` when the OS-level manager could not be created — hotkeys
    /// are simply disabled in that case.
    manager: Option<GlobalHotKeyManager>,
    /// The key watcher, when the key state can be read: every binding
    /// goes through it, and none is registered.
    watcher: Option<KeyWatcher>,
    watch_rx: Receiver<(BindableAction, bool)>,
    watch_tx: Sender<(BindableAction, bool)>,
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

        let (watch_tx, watch_rx) = unbounded::<(BindableAction, bool)>();
        let manager = match GlobalHotKeyManager::new() {
            Ok(m) => Some(m),
            Err(e) => {
                warn!("Global hotkeys unavailable: {e}");
                None
            }
        };

        let mut hm = Self {
            manager,
            watcher: None,
            watch_rx,
            watch_tx,
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

    /// Poll for a hotkey edge: `(action, pressed)`.
    ///
    /// Reads our own channel (see [`event_channel`]) — with an event
    /// handler installed, the crate's built-in receiver never fires.
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

    fn register_all(&mut self, settings: &Settings) {
        // Everything the poller can read goes to one watcher thread.
        let watched: Vec<(BindableAction, Keycode, (bool, bool, bool))> = settings
            .keybinds
            .iter()
            .filter(|e| !e.bind.is_empty())
            .filter_map(|e| {
                let parsed = e.parse()?;
                let key = keycode_for(&parsed.code)?;
                Some((e.action, key, (parsed.ctrl, parsed.shift, parsed.alt)))
            })
            .collect();
        let mut taken: Vec<BindableAction> = Vec::new();
        if !watched.is_empty() {
            taken = watched.iter().map(|(a, _, _)| *a).collect();
            match KeyWatcher::start(watched, self.watch_tx.clone()) {
                Some(w) => {
                    info!("Watching {} keybind(s); none registered", taken.len());
                    self.watcher = Some(w);
                }
                None => {
                    info!("Key state not readable here; registering hotkeys instead");
                    taken.clear();
                }
            }
        }

        let Some(manager) = &self.manager else {
            return;
        };

        for entry in &settings.keybinds {
            if taken.contains(&entry.action) {
                continue;
            }
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

// ── Key watcher ─────────────────────────────────────────────────────────

/// Polls the keyboard state and reports each binding's edges: both
/// for push-to-talk, the press alone for the toggles.
struct KeyWatcher {
    stop: Arc<AtomicBool>,
}

impl KeyWatcher {
    /// `None` where the key state cannot be read (the caller then
    /// registers the hotkeys instead). The state handle is created on
    /// the watcher thread itself — on Linux it holds an `Rc` to the
    /// X11 connection and cannot cross threads — so the thread reports
    /// back whether it could open one before this returns.
    fn start(
        bindings: Vec<(BindableAction, Keycode, (bool, bool, bool))>,
        tx: Sender<(BindableAction, bool)>,
    ) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (ready_tx, ready_rx) = unbounded::<bool>();
        let spawned = std::thread::Builder::new()
            .name("key-watch".into())
            .spawn(move || {
                let Some(state) = DeviceState::checked_new() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                let mut down = vec![false; bindings.len()];
                while !flag.load(Ordering::Relaxed) {
                    let keys = state.get_keys();
                    let has = |a: Keycode, b: Keycode| keys.contains(&a) || keys.contains(&b);
                    for (i, (action, key, (ctrl, shift, alt))) in bindings.iter().enumerate() {
                        let now = keys.contains(key)
                            && (!ctrl || has(Keycode::LControl, Keycode::RControl))
                            && (!shift || has(Keycode::LShift, Keycode::RShift))
                            && (!alt || has(Keycode::LAlt, Keycode::RAlt));
                        if now == down[i] {
                            continue;
                        }
                        down[i] = now;
                        let report = now || *action == BindableAction::PushToTalk;
                        if report {
                            if tx.send((*action, now)).is_err() {
                                return;
                            }
                            // Same as a registered hotkey's delivery: wake
                            // the UI thread now, not at its next timer.
                            if let Some(wake) = WAKE.get() {
                                wake();
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(12));
                }
            });
        if let Err(e) = spawned {
            warn!("key watcher thread not started: {e}");
            return None;
        }
        // Opening the state is quick; a platform that cannot (Wayland,
        // a macOS without the accessibility grant) says so at once.
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(true) => Some(Self { stop }),
            _ => {
                stop.store(true, Ordering::Relaxed);
                None
            }
        }
    }
}

impl Drop for KeyWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A W3C `Code` name (what the keybind strings carry) to the key the
/// state poller knows. `None` for keys it does not cover.
fn keycode_for(code: &str) -> Option<Keycode> {
    use Keycode as K;
    let simple = match code {
        "Space" => K::Space,
        "Escape" => K::Escape,
        "Enter" => K::Enter,
        "Tab" => K::Tab,
        "Backspace" => K::Backspace,
        "CapsLock" => K::CapsLock,
        "Home" => K::Home,
        "End" => K::End,
        "PageUp" => K::PageUp,
        "PageDown" => K::PageDown,
        "Insert" => K::Insert,
        "Delete" => K::Delete,
        "ArrowUp" => K::Up,
        "ArrowDown" => K::Down,
        "ArrowLeft" => K::Left,
        "ArrowRight" => K::Right,
        "ControlLeft" => K::LControl,
        "ControlRight" => K::RControl,
        "ShiftLeft" => K::LShift,
        "ShiftRight" => K::RShift,
        "AltLeft" => K::LAlt,
        "AltRight" => K::RAlt,
        "MetaLeft" => K::LMeta,
        "MetaRight" => K::RMeta,
        "Backquote" => K::Grave,
        "Minus" => K::Minus,
        "Equal" => K::Equal,
        "BracketLeft" => K::LeftBracket,
        "BracketRight" => K::RightBracket,
        "Backslash" => K::BackSlash,
        "Semicolon" => K::Semicolon,
        "Quote" => K::Apostrophe,
        "Comma" => K::Comma,
        "Period" => K::Dot,
        "Slash" => K::Slash,
        "NumpadAdd" => K::NumpadAdd,
        "NumpadSubtract" => K::NumpadSubtract,
        "NumpadMultiply" => K::NumpadMultiply,
        "NumpadDivide" => K::NumpadDivide,
        "NumpadDecimal" => K::NumpadDecimal,
        "NumpadEnter" => K::NumpadEnter,
        _ => {
            // Letters (`KeyN` → `N`), digits (`Digit5` → `Key5`),
            // F-keys and numpad digits (same spelling).
            let name = if let Some(letter) = code.strip_prefix("Key") {
                letter.to_string()
            } else if let Some(digit) = code.strip_prefix("Digit") {
                format!("Key{digit}")
            } else {
                code.to_string()
            };
            return Keycode::from_str(&name).ok();
        }
    };
    Some(simple)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w3c_codes_map_to_polled_keys() {
        assert_eq!(keycode_for("KeyN"), Some(Keycode::N));
        assert_eq!(keycode_for("Digit5"), Some(Keycode::Key5));
        assert_eq!(keycode_for("F7"), Some(Keycode::F7));
        assert_eq!(keycode_for("Numpad3"), Some(Keycode::Numpad3));
        assert_eq!(keycode_for("Space"), Some(Keycode::Space));
        assert_eq!(keycode_for("Backquote"), Some(Keycode::Grave));
        assert_eq!(keycode_for("PrintScreen"), None);
    }
}
