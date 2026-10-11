// hotkeys.rs — Global hotkey registration and dispatch.
//
// Both edges are forwarded with a `pressed` flag: the toggles act on
// the press only, push-to-talk on both.
//
// Bindings are *watched*, not registered: a registered global hotkey
// is consumed by the OS (`RegisterHotKey`, `RegisterEventHotKey`,
// `XGrabKey` are grabs — there is no listen-only mode), so a PTT key
// of `N` stopped typing `n` in every other program. A small thread
// reads the state of the bound keys instead (`GetAsyncKeyState`,
// `CGEventSourceKeyState`, `XQueryKeymap`) and reports the edges;
// nothing is swallowed, for any binding. Where the state cannot be
// read (Wayland, a macOS without the accessibility grant) the
// bindings fall back to registered hotkeys, with a log line saying so.
//
// Keybinds act only in a session, so all of this runs only while
// connected, and the push-to-talk key is watched only while
// push-to-talk is on (review P1-3): the watcher used to poll the whole
// keyboard every 12 ms from launch to exit, in the tray too.
//
// Hotkeys are strictly optional: if the OS refuses to create a global
// hotkey manager (common on Wayland, some remote sessions, etc.) the
// app runs without them instead of crashing at startup.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Once, OnceLock},
    thread::JoinHandle,
    time::Duration,
};

use client::config::{BindableAction, KeybindEntry, Settings};
use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
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

/// How often the watcher reads the bound keys while connected.
const POLL: Duration = Duration::from_millis(12);

pub struct HotkeyManager {
    /// The OS-level manager, created only when a binding falls back to
    /// registration, and dropped at disconnect: its X11 backend runs a
    /// 50 ms poll thread from creation, registered hotkeys or not.
    /// `None` also when the OS refused one — hotkeys are then simply
    /// disabled.
    manager: Option<GlobalHotKeyManager>,
    /// The key watcher, when the key state can be read: every binding
    /// it can name goes through it, and none of those is registered.
    watcher: Option<KeyWatcher>,
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

    fn register_all(&mut self) {
        // Everything the watcher can name goes to one watcher thread.
        let watched: Vec<Watched> = self
            .binds
            .iter()
            .filter_map(|e| {
                let parsed = e.parse()?;
                Some(Watched {
                    action: e.action,
                    key: platform_key(&parsed.code)?,
                    ctrl: parsed.ctrl,
                    shift: parsed.shift,
                    alt: parsed.alt,
                })
            })
            .collect();
        let mut taken: Vec<BindableAction> = Vec::new();
        if !watched.is_empty() {
            taken = watched.iter().map(|w| w.action).collect();
            if let Some(w) = KeyWatcher::start(watched, self.watch_tx.clone()) {
                info!("Watching {} keybind(s)", taken.len());
                self.watcher = Some(w);
            } else {
                info!("Key state not readable here; registering hotkeys instead");
                taken.clear();
            }
        }

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

// ── Key watcher ─────────────────────────────────────────────────────────

/// One binding as the watcher reads it: the key in the platform's own
/// code (see [`KEYS`]) and the modifiers it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Watched {
    action: BindableAction,
    key: u16,
    ctrl: bool,
    shift: bool,
    alt: bool,
}

#[derive(Debug, Clone, Copy)]
enum Modifier {
    Ctrl,
    Shift,
    Alt,
}

/// Reads the bound keys every [`POLL`] and reports each binding's
/// edges: both for push-to-talk, the press alone for the toggles.
struct KeyWatcher {
    /// Dropped to stop the thread: it waits on this between passes, so
    /// it wakes at once.
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl KeyWatcher {
    /// `None` where the key state cannot be read (the caller then
    /// registers the hotkeys instead). The state is opened on the
    /// watcher thread itself, which reports back whether it could
    /// before this returns.
    fn start(bindings: Vec<Watched>, tx: Sender<(BindableAction, bool)>) -> Option<Self> {
        let (stop, stop_rx) = bounded::<()>(0);
        let (ready_tx, ready_rx) = bounded::<bool>(1);
        let spawned = std::thread::Builder::new()
            .name("key-watch".into())
            .spawn(move || {
                let Some(mut state) = keystate::KeyState::open() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                // Nobody listening: `start` gave up waiting.
                if ready_tx.send(true).is_err() {
                    return;
                }
                watch(&mut state, &bindings, &stop_rx, &tx);
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                warn!("key watcher thread not started: {e}");
                return None;
            }
        };
        // Opening the state is quick; a platform that cannot (Wayland,
        // a macOS without the accessibility grant) says so at once.
        if let Ok(true) = ready_rx.recv_timeout(Duration::from_secs(2)) {
            Some(Self {
                stop: Some(stop),
                thread: Some(thread),
            })
        } else {
            // Dropping `stop` ends a thread that opened late.
            None
        }
    }
}

impl Drop for KeyWatcher {
    fn drop(&mut self) {
        drop(self.stop.take());
        // At most one pass: the thread is waiting on `stop` or is in
        // the middle of reading a few keys. Joined, so its release of a
        // held push-to-talk lands before a new watcher's first edge.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The watcher thread's loop: read the bound keys, report the edges,
/// wait [`POLL`] or until stopped.
fn watch(
    state: &mut keystate::KeyState,
    bindings: &[Watched],
    stop: &Receiver<()>,
    tx: &Sender<(BindableAction, bool)>,
) {
    let mut down = vec![false; bindings.len()];
    let report = |edge: (BindableAction, bool)| {
        let sent = tx.send(edge).is_ok();
        // Same as a registered hotkey's delivery: wake the UI thread
        // now, not at its next timer.
        if let Some(wake) = WAKE.get() {
            wake();
        }
        sent
    };
    'watching: loop {
        if !state.refresh() {
            warn!("Key watcher: the key state stopped answering; keybinds are off until reconnect");
            break;
        }
        for (b, was) in bindings.iter().zip(down.iter_mut()) {
            let now = state.key_down(b.key)
                && (!b.ctrl || state.modifier_down(Modifier::Ctrl))
                && (!b.shift || state.modifier_down(Modifier::Shift))
                && (!b.alt || state.modifier_down(Modifier::Alt));
            if let Some(pressed) = edge(b.action, was, now)
                && !report((b.action, pressed))
            {
                break 'watching;
            }
        }
        match stop.recv_timeout(POLL) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Stopped (a disconnect, a rebind) with push-to-talk held: let it
    // go, or the microphone would stay open until the next press.
    for (b, was) in bindings.iter().zip(&down) {
        if *was && b.action == BindableAction::PushToTalk {
            let _ = report((b.action, false));
        }
    }
}

/// The edge to report when a binding's chord goes from `was` to `now`
/// (and `was` follows): both for push-to-talk, a hold; the press alone
/// for the toggles.
fn edge(action: BindableAction, was: &mut bool, now: bool) -> Option<bool> {
    if now == *was {
        return None;
    }
    *was = now;
    (now || action == BindableAction::PushToTalk).then_some(now)
}

// ── The keys, per platform ──────────────────────────────────────────────

/// No key of its own on that platform.
const NONE: u16 = u16::MAX;

/// Every W3C key code a keybind can name (what the keybind strings
/// carry), with that key on each platform the watcher reads: the
/// Windows virtual key, the macOS virtual key code (`kVK_*`), and the
/// Linux input event code (`KEY_*`; an X11 keycode is that plus 8, as
/// `device_query` read it).
#[rustfmt::skip]
const KEYS: &[(&str, u16, u16, u16)] = &[
    // code             Windows mac   evdev
    ("KeyA",            0x41, 0x00, 30),
    ("KeyB",            0x42, 0x0B, 48),
    ("KeyC",            0x43, 0x08, 46),
    ("KeyD",            0x44, 0x02, 32),
    ("KeyE",            0x45, 0x0E, 18),
    ("KeyF",            0x46, 0x03, 33),
    ("KeyG",            0x47, 0x05, 34),
    ("KeyH",            0x48, 0x04, 35),
    ("KeyI",            0x49, 0x22, 23),
    ("KeyJ",            0x4A, 0x26, 36),
    ("KeyK",            0x4B, 0x28, 37),
    ("KeyL",            0x4C, 0x25, 38),
    ("KeyM",            0x4D, 0x2E, 50),
    ("KeyN",            0x4E, 0x2D, 49),
    ("KeyO",            0x4F, 0x1F, 24),
    ("KeyP",            0x50, 0x23, 25),
    ("KeyQ",            0x51, 0x0C, 16),
    ("KeyR",            0x52, 0x0F, 19),
    ("KeyS",            0x53, 0x01, 31),
    ("KeyT",            0x54, 0x11, 20),
    ("KeyU",            0x55, 0x20, 22),
    ("KeyV",            0x56, 0x09, 47),
    ("KeyW",            0x57, 0x0D, 17),
    ("KeyX",            0x58, 0x07, 45),
    ("KeyY",            0x59, 0x10, 21),
    ("KeyZ",            0x5A, 0x06, 44),
    ("Digit0",          0x30, 0x1D, 11),
    ("Digit1",          0x31, 0x12, 2),
    ("Digit2",          0x32, 0x13, 3),
    ("Digit3",          0x33, 0x14, 4),
    ("Digit4",          0x34, 0x15, 5),
    ("Digit5",          0x35, 0x17, 6),
    ("Digit6",          0x36, 0x16, 7),
    ("Digit7",          0x37, 0x1A, 8),
    ("Digit8",          0x38, 0x1C, 9),
    ("Digit9",          0x39, 0x19, 10),
    ("F1",              0x70, 0x7A, 59),
    ("F2",              0x71, 0x78, 60),
    ("F3",              0x72, 0x63, 61),
    ("F4",              0x73, 0x76, 62),
    ("F5",              0x74, 0x60, 63),
    ("F6",              0x75, 0x61, 64),
    ("F7",              0x76, 0x62, 65),
    ("F8",              0x77, 0x64, 66),
    ("F9",              0x78, 0x65, 67),
    ("F10",             0x79, 0x6D, 68),
    ("F11",             0x7A, 0x67, 87),
    ("F12",             0x7B, 0x6F, 88),
    ("F13",             0x7C, 0x69, 183),
    ("F14",             0x7D, 0x6B, 184),
    ("F15",             0x7E, 0x71, 185),
    ("F16",             0x7F, 0x6A, 186),
    ("F17",             0x80, 0x40, 187),
    ("F18",             0x81, 0x4F, 188),
    ("F19",             0x82, 0x50, 189),
    ("F20",             0x83, 0x5A, 190),
    ("Space",           0x20, 0x31, 57),
    ("Escape",          0x1B, 0x35, 1),
    ("Enter",           0x0D, 0x24, 28),
    ("Tab",             0x09, 0x30, 15),
    ("Backspace",       0x08, 0x33, 14),
    ("CapsLock",        0x14, 0x39, 58),
    ("Home",            0x24, 0x73, 102),
    ("End",             0x23, 0x77, 107),
    ("PageUp",          0x21, 0x74, 104),
    ("PageDown",        0x22, 0x79, 109),
    // A Mac keyboard's Help sits where Insert is.
    ("Insert",          0x2D, 0x72, 110),
    ("Delete",          0x2E, 0x75, 111),
    ("ArrowUp",         0x26, 0x7E, 103),
    ("ArrowDown",       0x28, 0x7D, 108),
    ("ArrowLeft",       0x25, 0x7B, 105),
    ("ArrowRight",      0x27, 0x7C, 106),
    ("ControlLeft",     0xA2, 0x3B, 29),
    ("ControlRight",    0xA3, 0x3E, 97),
    ("ShiftLeft",       0xA0, 0x38, 42),
    ("ShiftRight",      0xA1, 0x3C, 54),
    ("AltLeft",         0xA4, 0x3A, 56),
    ("AltRight",        0xA5, 0x3D, 100),
    ("MetaLeft",        0x5B, 0x37, 125),
    ("MetaRight",       0x5C, 0x36, 126),
    ("Backquote",       0xC0, 0x32, 41),
    ("Minus",           0xBD, 0x1B, 12),
    ("Equal",           0xBB, 0x18, 13),
    ("BracketLeft",     0xDB, 0x21, 26),
    ("BracketRight",    0xDD, 0x1E, 27),
    ("Backslash",       0xDC, 0x2A, 43),
    ("Semicolon",       0xBA, 0x29, 39),
    ("Quote",           0xDE, 0x27, 40),
    ("Comma",           0xBC, 0x2B, 51),
    ("Period",          0xBE, 0x2F, 52),
    ("Slash",           0xBF, 0x2C, 53),
    ("Numpad0",         0x60, 0x52, 82),
    ("Numpad1",         0x61, 0x53, 79),
    ("Numpad2",         0x62, 0x54, 80),
    ("Numpad3",         0x63, 0x55, 81),
    ("Numpad4",         0x64, 0x56, 75),
    ("Numpad5",         0x65, 0x57, 76),
    ("Numpad6",         0x66, 0x58, 77),
    ("Numpad7",         0x67, 0x59, 71),
    ("Numpad8",         0x68, 0x5B, 72),
    ("Numpad9",         0x69, 0x5C, 73),
    ("NumpadAdd",       0x6B, 0x45, 78),
    ("NumpadSubtract",  0x6D, 0x4E, 74),
    ("NumpadMultiply",  0x6A, 0x43, 55),
    ("NumpadDivide",    0x6F, 0x4B, 98),
    ("NumpadDecimal",   0x6E, 0x41, 83),
    // Windows has one virtual key for both Enters, and no keypad `=`.
    ("NumpadEnter",     NONE, 0x4C, 96),
    ("NumpadEqual",     NONE, 0x51, 117),
];

/// The bound key's code on this platform, if the watcher can read it.
/// `None` sends the binding to a registered hotkey instead.
fn platform_key(code: &str) -> Option<u16> {
    let &(_, windows, mac, evdev) = KEYS.iter().find(|(name, ..)| *name == code)?;
    let key = if cfg!(target_os = "windows") {
        windows
    } else if cfg!(target_os = "macos") {
        mac
    } else {
        evdev
    };
    (key != NONE).then_some(key)
}

/// Whether X11 keycode `keycode` is down in a `QueryKeymap` reply: one
/// bit per keycode, eight to a byte, the lowest bit first.
#[cfg(any(target_os = "linux", test))]
fn keymap_bit(keys: &[u8; 32], keycode: u16) -> bool {
    keys.get(usize::from(keycode / 8))
        .is_some_and(|byte| byte & (1 << (keycode % 8)) != 0)
}

#[cfg(target_os = "windows")]
mod keystate {
    //! `GetAsyncKeyState`, one call per bound key and modifier per
    //! pass (the whole keyboard was 256 calls a pass).

    use super::Modifier;

    #[link(name = "user32")]
    unsafe extern "system" {
        // No preconditions: any value is a key code, or none.
        safe fn GetAsyncKeyState(vkey: i32) -> i16;
    }

    // Either side of each modifier.
    const VK_SHIFT: u16 = 0x10;
    const VK_CONTROL: u16 = 0x11;
    const VK_MENU: u16 = 0x12;

    pub(super) struct KeyState;

    // The same methods as the other platforms', state or not.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    impl KeyState {
        pub(super) fn open() -> Option<Self> {
            Some(Self)
        }

        pub(super) fn refresh(&mut self) -> bool {
            true
        }

        pub(super) fn key_down(&self, vk: u16) -> bool {
            // The high bit says "down now". The low bit ("pressed since
            // the last call") is shared with every other caller.
            GetAsyncKeyState(i32::from(vk)) < 0
        }

        pub(super) fn modifier_down(&self, modifier: Modifier) -> bool {
            self.key_down(match modifier {
                Modifier::Ctrl => VK_CONTROL,
                Modifier::Shift => VK_SHIFT,
                Modifier::Alt => VK_MENU,
            })
        }
    }
}

#[cfg(target_os = "macos")]
mod keystate {
    //! `CGEventSourceKeyState` per bound key; the modifiers, as keys
    //! or as chord parts, from one `CGEventSourceFlagsState` a pass.

    use std::cell::Cell;

    use super::Modifier;

    /// `kCGEventSourceStateCombinedSessionState`: every source in the
    /// login session, keyboards included.
    const COMBINED_SESSION: i32 = 0;

    // `CGEventFlags`: either side of a modifier…
    const FLAG_SHIFT: u64 = 1 << 17;
    const FLAG_CONTROL: u64 = 1 << 18;
    const FLAG_ALTERNATE: u64 = 1 << 19;
    const FLAG_COMMAND: u64 = 1 << 20;
    // …and one side, from the device bits (IOKit's `NX_DEVICE*KEYMASK`).
    const LEFT_CONTROL: u64 = FLAG_CONTROL | 0x0001;
    const LEFT_SHIFT: u64 = FLAG_SHIFT | 0x0002;
    const RIGHT_SHIFT: u64 = FLAG_SHIFT | 0x0004;
    const LEFT_COMMAND: u64 = FLAG_COMMAND | 0x0008;
    const RIGHT_COMMAND: u64 = FLAG_COMMAND | 0x0010;
    const LEFT_OPTION: u64 = FLAG_ALTERNATE | 0x0020;
    const RIGHT_OPTION: u64 = FLAG_ALTERNATE | 0x0040;
    const RIGHT_CONTROL: u64 = FLAG_CONTROL | 0x2000;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        // No preconditions: plain values in, a plain value out.
        safe fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        safe fn CGEventSourceFlagsState(state: i32) -> u64;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        // The check that never asks (review P2-30): asking, as
        // `device_query` did, put up the Accessibility prompt at every
        // launch and every keybind change.
        safe fn AXIsProcessTrusted() -> u8;
    }

    pub(super) struct KeyState {
        /// This pass's flags, read on first need.
        flags: Cell<Option<u64>>,
    }

    impl KeyState {
        pub(super) fn open() -> Option<Self> {
            if AXIsProcessTrusted() == 0 {
                return None;
            }
            Some(Self {
                flags: Cell::new(None),
            })
        }

        pub(super) fn refresh(&mut self) -> bool {
            self.flags.set(None);
            true
        }

        fn flags(&self) -> u64 {
            if let Some(flags) = self.flags.get() {
                return flags;
            }
            let flags = CGEventSourceFlagsState(COMBINED_SESSION);
            self.flags.set(Some(flags));
            flags
        }

        pub(super) fn key_down(&self, key: u16) -> bool {
            let side = match key {
                0x3B => LEFT_CONTROL,
                0x3E => RIGHT_CONTROL,
                0x38 => LEFT_SHIFT,
                0x3C => RIGHT_SHIFT,
                0x3A => LEFT_OPTION,
                0x3D => RIGHT_OPTION,
                0x37 => LEFT_COMMAND,
                0x36 => RIGHT_COMMAND,
                _ => return CGEventSourceKeyState(COMBINED_SESSION, key),
            };
            self.flags() & side == side
        }

        pub(super) fn modifier_down(&self, modifier: Modifier) -> bool {
            let mask = match modifier {
                Modifier::Ctrl => FLAG_CONTROL,
                Modifier::Shift => FLAG_SHIFT,
                Modifier::Alt => FLAG_ALTERNATE,
            };
            self.flags() & mask != 0
        }
    }
}

#[cfg(target_os = "linux")]
mod keystate {
    //! `QueryKeymap` over the X11 connection `x11rb` opens (pure Rust):
    //! one round trip a pass into a local 32-byte reply. `device_query`
    //! made it through a pointer into a dropped temporary (review P1-3).
    //! No display (Wayland without Xwayland) and `open` fails.

    use x11rb::{protocol::xproto::ConnectionExt as _, rust_connection::RustConnection};

    use super::{keymap_bit, Modifier};

    /// What an X11 keycode adds to the input event code.
    const X11_OFFSET: u16 = 8;

    pub(super) struct KeyState {
        conn: RustConnection,
        keys: [u8; 32],
    }

    impl KeyState {
        pub(super) fn open() -> Option<Self> {
            let (conn, _screen) = x11rb::connect(None).ok()?;
            Some(Self {
                conn,
                keys: [0; 32],
            })
        }

        /// One `QueryKeymap`; false once the connection is gone.
        pub(super) fn refresh(&mut self) -> bool {
            let Ok(cookie) = self.conn.query_keymap() else {
                return false;
            };
            match cookie.reply() {
                Ok(reply) => {
                    self.keys = reply.keys;
                    true
                }
                Err(_) => false,
            }
        }

        pub(super) fn key_down(&self, evdev: u16) -> bool {
            keymap_bit(&self.keys, evdev + X11_OFFSET)
        }

        pub(super) fn modifier_down(&self, modifier: Modifier) -> bool {
            let (left, right) = match modifier {
                Modifier::Ctrl => (29, 97),
                Modifier::Shift => (42, 54),
                Modifier::Alt => (56, 100),
            };
            self.key_down(left) || self.key_down(right)
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod keystate {
    //! No key state to read: every binding is registered instead.

    use super::Modifier;

    pub(super) struct KeyState;

    #[allow(clippy::unused_self)]
    impl KeyState {
        pub(super) fn open() -> Option<Self> {
            None
        }

        pub(super) fn refresh(&mut self) -> bool {
            false
        }

        pub(super) fn key_down(&self, _key: u16) -> bool {
            false
        }

        pub(super) fn modifier_down(&self, _modifier: Modifier) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(code: &str) -> (u16, u16, u16) {
        let &(_, w, m, e) = KEYS.iter().find(|(name, ..)| *name == code).unwrap();
        (w, m, e)
    }

    #[test]
    fn w3c_codes_map_to_each_platforms_key() {
        assert_eq!(column("KeyN"), (0x4E, 0x2D, 49));
        assert_eq!(column("Digit5"), (0x35, 0x17, 6));
        assert_eq!(column("F7"), (0x76, 0x62, 65));
        assert_eq!(column("F12"), (0x7B, 0x6F, 88));
        assert_eq!(column("Numpad3"), (0x63, 0x55, 81));
        assert_eq!(column("Space"), (0x20, 0x31, 57));
        assert_eq!(column("Backquote"), (0xC0, 0x32, 41));
        assert_eq!(column("ControlRight"), (0xA3, 0x3E, 97));
        assert_eq!(platform_key("PrintScreen"), None);
        assert_eq!(platform_key(""), None);
    }

    #[test]
    fn key_table_has_no_duplicates_and_fits_x11() {
        for (i, (name, w, m, e)) in KEYS.iter().enumerate() {
            for (other, w2, m2, e2) in &KEYS[i + 1..] {
                assert_ne!(name, other);
                for (a, b) in [(w, w2), (m, m2), (e, e2)] {
                    assert!(*a == NONE || a != b, "{name} and {other} share a key");
                }
            }
            // An X11 keycode is at most 255.
            assert!(*e != NONE && *e + 8 <= 255, "{name}");
        }
    }

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
    fn edges_are_presses_for_toggles_and_both_for_push_to_talk() {
        let mut was = false;
        assert_eq!(edge(BindableAction::ToggleMute, &mut was, false), None);
        assert_eq!(edge(BindableAction::ToggleMute, &mut was, true), Some(true));
        assert_eq!(edge(BindableAction::ToggleMute, &mut was, true), None);
        assert_eq!(edge(BindableAction::ToggleMute, &mut was, false), None);
        assert!(!was, "the release is tracked even when not reported");

        let mut was = false;
        assert_eq!(edge(BindableAction::PushToTalk, &mut was, true), Some(true));
        assert_eq!(edge(BindableAction::PushToTalk, &mut was, true), None);
        assert_eq!(
            edge(BindableAction::PushToTalk, &mut was, false),
            Some(false)
        );
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

    #[test]
    fn keymap_bits_are_lowest_first() {
        let mut keys = [0u8; 32];
        // F7 (input code 65) is X11 keycode 73: byte 9, bit 1.
        keys[9] = 0b0000_0010;
        assert!(keymap_bit(&keys, 65 + 8));
        assert!(!keymap_bit(&keys, 64 + 8));
        assert!(!keymap_bit(&keys, 66 + 8));
        keys[31] = 0x80;
        assert!(keymap_bit(&keys, 255));
    }
}
