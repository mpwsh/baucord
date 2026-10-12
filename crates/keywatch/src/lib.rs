//! Keys watched, not taken.
//!
//! A registered global hotkey is consumed by the OS (`RegisterHotKey`,
//! `RegisterEventHotKey`, `XGrabKey` are grabs — there is no
//! listen-only mode), so a push-to-talk key of `N` stopped typing `n`
//! in every other program. A [`Watcher`] reads the bound keys instead
//! and reports each binding's edges; nothing is swallowed.
//!
//! Two things per platform, behind [`Source`]:
//!
//! - the *state* of a key, read on demand (`GetAsyncKeyState`,
//!   `CGEventSourceKeyState`, X11 `QueryKeymap`) — what decides
//!   whether a binding is held;
//! - the *events* that say when to look (Windows Raw Input, a
//!   listen-only event tap on macOS, `XInput2` raw key events on X11).
//!
//! The watcher sleeps until a key a binding names changes, reads the
//! state then and a few more times ([`SETTLE_PASSES`]), and reads
//! every [`POLL`] while a binding is held, so a release is seen even
//! if its event is not. With the keyboard at rest it does not wake.
//! Until 1.2.1 it read the state every 12 ms for as long as a session
//! lasted, about 83 wakes a second (review P1-3).
//!
//! The events are trusted only once they have shown they arrive. A
//! fresh watcher also reads on a slow clock ([`HEARTBEAT`]): a press
//! that the clock finds and no event announced means the events do
//! not reach this process (another registration took them, a tap that
//! delivers nothing), and the watcher reads every [`POLL`] from then
//! on, as it always did. After [`PROVEN_PRESSES`] presses that events
//! did announce, the clock stops.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use log::{info, warn};

mod keys;

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod os;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;

#[cfg(target_os = "linux")]
#[path = "x11.rs"]
mod os;

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[path = "none.rs"]
mod os;

pub use keys::platform_key;

/// How often the bound keys are read while one is held, right after a
/// key event, and always where there are no events to wait for.
pub const POLL: Duration = Duration::from_millis(12);

/// Reads after a key event (and at the start), [`POLL`] apart: the
/// event can reach this thread before the state it announces does.
const SETTLE_PASSES: u8 = 3;

/// The slow clock a fresh watcher also reads on, until its events have
/// proven themselves.
const HEARTBEAT: Duration = Duration::from_millis(250);

/// Presses announced by an event before the heartbeat stops.
const PROVEN_PRESSES: u8 = 3;

/// One binding as the watcher reads it.
// The three parts of a chord and the kind of binding: a yes or no each.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding<A> {
    pub action: A,
    /// The key in the platform's own code ([`platform_key`]).
    pub key: u16,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    /// A hold (push-to-talk): the release is reported too. A toggle
    /// reports the press alone.
    pub hold: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modifier {
    Ctrl,
    Shift,
    Alt,
}

/// The keys whose events matter to a set of bindings: the bound keys,
/// and every modifier key when a binding names a modifier (or is one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Interest {
    keys: Vec<u16>,
    modifiers: bool,
}

impl Interest {
    fn of<A>(bindings: &[Binding<A>]) -> Self {
        Self {
            keys: bindings.iter().map(|b| b.key).collect(),
            modifiers: bindings
                .iter()
                .any(|b| b.ctrl || b.shift || b.alt || os::is_modifier(b.key)),
        }
    }

    /// Whether a change of `key` (a platform code) can change what a
    /// binding reads.
    // Asked by the platforms that have key events.
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos", target_os = "linux")),
        allow(dead_code)
    )]
    pub(crate) fn wants(&self, key: u16) -> bool {
        self.keys.contains(&key) || (self.modifiers && os::is_modifier(key))
    }

    /// Whether a change of some modifier, not known which, can.
    // Only the platforms whose events name no key for a modifier ask.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn wants_modifiers(&self) -> bool {
        self.modifiers
    }
}

/// The key state and the key events of one platform.
pub(crate) trait Source {
    /// Read the state for this pass; `false` once it cannot be read.
    fn refresh(&mut self) -> bool;
    fn key_down(&self, key: u16) -> bool;
    fn modifier_down(&self, modifier: Modifier) -> bool;
    /// Whether [`Self::wait`] can be woken by a key event.
    fn has_events(&self) -> bool;
    /// Sleep until a key of interest changes (`true`), or `timeout`
    /// passes or the watcher is kicked (`false`). `None` waits for as
    /// long as it takes; only asked of a source that has events.
    fn wait(&mut self, timeout: Option<Duration>) -> bool;
}

/// Wakes a [`Source::wait`] from another thread.
pub(crate) type Kick = Box<dyn Fn() + Send + Sync>;

/// Whether the bound keys can be read without the user's say-so. Only
/// macOS asks for one (the Accessibility permission); everywhere else
/// this is `true`. Never prompts.
#[must_use]
pub fn access_granted() -> bool {
    os::access_granted()
}

/// Ask the user for that say-so: on macOS, the system's Accessibility
/// prompt, which also lists the app in System Settings. Nothing
/// elsewhere. For a button, never for a launch (review P2-30).
pub fn ask_for_access() {
    os::ask_for_access();
}

/// A thread that watches a set of bindings until dropped.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    kick: Kick,
    thread: Option<JoinHandle<()>>,
    by_events: bool,
}

impl Watcher {
    /// Watch `bindings`, calling `report(action, pressed)` on the
    /// watcher thread for each edge; `report` returns `false` when
    /// nobody listens any more, which ends the watch.
    ///
    /// `None` where the key state cannot be read (Wayland without
    /// Xwayland, a macOS without the Accessibility permission): the
    /// caller registers hotkeys instead. The state is opened on the
    /// watcher thread itself, which reports back before this returns.
    pub fn start<A, R>(bindings: Vec<Binding<A>>, report: R) -> Option<Self>
    where
        A: Copy + Send + 'static,
        R: Fn(A, bool) -> bool + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Option<(Kick, bool)>>(1);
        let interest = Interest::of(&bindings);
        let spawned = std::thread::Builder::new().name("key-watch".into()).spawn({
            let stop = Arc::clone(&stop);
            move || {
                let Some((mut source, kick)) = os::open(interest) else {
                    let _ = ready_tx.send(None);
                    return;
                };
                // Nobody listening: `start` gave up waiting.
                if ready_tx.send(Some((kick, source.has_events()))).is_err() {
                    return;
                }
                watch(&mut source, &bindings, &stop, &report);
            }
        });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                warn!("key watcher thread not started: {e}");
                return None;
            }
        };
        // Opening is quick; a platform that cannot says so at once.
        let Ok(Some((kick, by_events))) = ready_rx.recv_timeout(Duration::from_secs(2)) else {
            // A thread that opens late finds nobody listening and ends.
            return None;
        };
        Some(Self {
            stop,
            kick,
            thread: Some(thread),
            by_events,
        })
    }

    /// Whether the watcher sleeps on key events (`true`) or reads the
    /// keys on a clock, as it must where there are none.
    #[must_use]
    pub fn by_events(&self) -> bool {
        self.by_events
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Kicked until it is out: a kick that lands between the
        // thread's look at `stop` and its sleep would otherwise be the
        // only one. Joined, so its release of a held push-to-talk lands
        // before a new watcher's first edge.
        while !thread.is_finished() {
            (self.kick)();
            thread.thread().unpark();
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = thread.join();
    }
}

/// How far the key events are trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// No events here, or they failed: read every [`POLL`].
    Clock,
    /// Events, and a heartbeat watching over them; so many presses
    /// were announced so far.
    Probation(u8),
    /// Events alone.
    Events,
}

/// The watcher thread's loop: read the bound keys, report the edges,
/// sleep until there is a reason to read again.
fn watch<A: Copy>(
    source: &mut impl Source,
    bindings: &[Binding<A>],
    stop: &AtomicBool,
    report: &impl Fn(A, bool) -> bool,
) {
    let mut down = vec![false; bindings.len()];
    let mut trust = if source.has_events() {
        Trust::Probation(0)
    } else {
        Trust::Clock
    };
    info!(
        "Key watcher: {} binding(s), {}",
        bindings.len(),
        if trust == Trust::Clock {
            "read on a clock"
        } else {
            "woken by key events"
        }
    );
    // Passes left of the reads that follow a key event, and whether
    // this pass was reached on the heartbeat alone. A key held since
    // before the watcher began has no event coming: the first passes
    // are read like an event's.
    let mut settle = SETTLE_PASSES;
    let mut on_heartbeat = false;
    'watching: while !stop.load(Ordering::Acquire) {
        if !source.refresh() {
            warn!("Key watcher: the key state stopped answering; keybinds are off until reconnect");
            break;
        }
        let mut pressed_now = false;
        for (b, was) in bindings.iter().zip(down.iter_mut()) {
            let now = source.key_down(b.key)
                && (!b.ctrl || source.modifier_down(Modifier::Ctrl))
                && (!b.shift || source.modifier_down(Modifier::Shift))
                && (!b.alt || source.modifier_down(Modifier::Alt));
            pressed_now |= now && !*was;
            if let Some(pressed) = edge(b.hold, was, now)
                && !report(b.action, pressed)
            {
                break 'watching;
            }
        }
        if pressed_now && let Trust::Probation(announced) = trust {
            trust = if on_heartbeat {
                warn!(
                    "Key watcher: a key was pressed and no event said so; reading on a clock \
                     from here on"
                );
                Trust::Clock
            } else if settle > 0 && announced + 1 >= PROVEN_PRESSES {
                Trust::Events
            } else if settle > 0 {
                Trust::Probation(announced + 1)
            } else {
                trust
            };
        }
        let held = down.contains(&true);
        let timeout = match trust {
            Trust::Clock => Some(POLL),
            _ if held || settle > 0 => Some(POLL),
            Trust::Probation(_) => Some(HEARTBEAT),
            Trust::Events => None,
        };
        on_heartbeat = !held && settle == 0 && matches!(trust, Trust::Probation(_));
        settle = settle.saturating_sub(1);
        if source.wait(timeout) {
            settle = SETTLE_PASSES;
            on_heartbeat = false;
        }
    }
    // Stopped (a disconnect, a rebind) with a hold held: let it go, or
    // the microphone would stay open until the next press.
    for (b, was) in bindings.iter().zip(&down) {
        if *was && b.hold {
            let _ = report(b.action, false);
        }
    }
}

/// The edge to report when a binding's chord goes from `was` to `now`
/// (and `was` follows): both for a hold, the press alone for a toggle.
fn edge(hold: bool, was: &mut bool, now: bool) -> Option<bool> {
    if now == *was {
        return None;
    }
    *was = now;
    (now || hold).then_some(now)
}

/// A source whose events could not be had waits like this: asleep for
/// `timeout`, or until the watcher is dropped (which unparks it).
// X11 sleeps in `poll` either way.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(crate) fn wait_on_the_clock(timeout: Option<Duration>) {
    std::thread::park_timeout(timeout.unwrap_or(POLL));
}

#[cfg(test)]
mod tests;
