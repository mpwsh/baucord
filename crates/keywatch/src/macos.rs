//! macOS.
//!
//! The state is `CGEventSourceKeyState` per bound key; the modifiers,
//! as keys or as chord parts, come from one `CGEventSourceFlagsState`
//! a read. The events are a listen-only event tap on the login
//! session, served by the watcher thread's run loop: every key,
//! whoever has the focus, and taken from nobody.
//!
//! All of it needs the Accessibility permission, which is checked
//! without asking ([`access_granted`]); the prompt is
//! [`ask_for_access`]'s, behind a button (review P2-30). Without the
//! permission [`open`] fails and the bindings are registered as
//! hotkeys instead. With it but no tap (the system refused one), the
//! state is read on a clock.

use std::{
    cell::Cell,
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    time::{Duration, Instant},
};

use log::warn;

use crate::{Interest, Kick, Modifier, Source, Woke, POLL};

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

// `CGEventType`.
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
/// The system switched the tap off: it was slow, or the user's input
/// did. It is switched back on from its own callback.
const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

/// `kCGSessionEventTap`: events as they enter the login session.
const SESSION_TAP: u32 = 1;
/// `kCGHeadInsertEventTap`.
const HEAD_INSERT: u32 = 0;
/// `kCGEventTapOptionListenOnly`: the tap sees events and cannot
/// change or hold them.
const LISTEN_ONLY: u32 = 1;
/// `kCGKeyboardEventKeycode`.
const KEYCODE_FIELD: u32 = 9;

// `CFRunLoopRunResult`.
const RUN_STOPPED: i32 = 2;
const RUN_TIMED_OUT: i32 = 3;
const RUN_HANDLED_SOURCE: i32 = 4;

/// "For ever", as a run loop takes it.
const FOR_EVER_SECS: f64 = 1.0e10;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CGEventRef = *mut c_void;

/// A Core Foundation constant taken by address only.
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

/// `CGEventTapCallBack`.
type TapCallback = unsafe extern "C" fn(
    proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;

    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFRunLoopStop(run_loop: CFRunLoopRef);
    fn CFRunLoopWakeUp(run_loop: CFRunLoopRef);
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFMachPortInvalidate(port: CFMachPortRef);
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> CFTypeRef;
    fn CFRetain(object: CFTypeRef) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    // No preconditions: plain values in, a plain value out.
    safe fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    safe fn CGEventSourceFlagsState(state: i32) -> u64;
    /// Whether this process may listen to key events; never asks.
    safe fn CGPreflightListenEventAccess() -> bool;

    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events: u64,
        callback: TapCallback,
        user: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: CFStringRef;

    // The check that never asks: asking, as `device_query` did, put up
    // the Accessibility prompt at every launch and every keybind
    // change.
    safe fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> u8;
}

/// The virtual key codes of the modifier keys: Command, Shift, Caps
/// Lock, Option and Control, left then right (`kVK_RightCommand` 0x36
/// to `kVK_RightControl` 0x3E).
pub(crate) fn is_modifier(key: u16) -> bool {
    (0x36..=0x3E).contains(&key)
}

pub(crate) fn access_granted() -> bool {
    AXIsProcessTrusted() != 0
}

pub(crate) fn ask_for_access() {
    // SAFETY: the dictionary is one key and one value, both constants
    // of the system's that outlive the call, with the callbacks that
    // go with Core Foundation objects; it is released once the check
    // has read it.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        );
        if options.is_null() {
            return;
        }
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
    }
}

/// What the tap's callback shares with the watcher thread.
struct TapState {
    interest: Interest,
    /// A key of interest changed since the watcher last looked.
    hit: AtomicBool,
    /// The tap itself, for the callback to switch back on.
    port: AtomicPtr<c_void>,
}

/// Runs on the watcher thread, from its run loop.
unsafe extern "C" fn on_event(
    _proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    user: *mut c_void,
) -> CGEventRef {
    // SAFETY: `user` is the `TapState` that `Tap::open` leaked; it is
    // freed only after the port is invalidated, which ends the calls.
    let state = unsafe { &*user.cast::<TapState>() };
    match kind {
        KEY_DOWN | KEY_UP => {
            // SAFETY: `event` is the live event this call was made for.
            let code = unsafe { CGEventGetIntegerValueField(event, KEYCODE_FIELD) };
            if u16::try_from(code).is_ok_and(|code| state.interest.wants(code)) {
                state.hit.store(true, Ordering::Release);
            }
        }
        FLAGS_CHANGED if state.interest.wants_modifiers() => {
            state.hit.store(true, Ordering::Release);
        }
        TAP_DISABLED_BY_TIMEOUT | TAP_DISABLED_BY_USER_INPUT => {
            let port = state.port.load(Ordering::Acquire);
            if !port.is_null() {
                // SAFETY: the port is alive while its callback runs.
                unsafe { CGEventTapEnable(port, true) };
            }
            // Keys may have changed meanwhile: read them.
            state.hit.store(true, Ordering::Release);
        }
        _ => {}
    }
    // Listen-only: what is returned changes nothing.
    event
}

/// A listen-only tap on key events, added to the current thread's run
/// loop.
struct Tap {
    port: CFMachPortRef,
    source: CFRunLoopSourceRef,
    state: *mut TapState,
}

/// A retained run loop, to stop from another thread.
struct RunLoop(CFRunLoopRef);

// SAFETY: a `CFRunLoop` may be stopped and woken from any thread, and
// the reference is retained for as long as this value lives.
unsafe impl Send for RunLoop {}
// SAFETY: as above; the two calls made through it are thread-safe.
unsafe impl Sync for RunLoop {}

impl RunLoop {
    /// End the `CFRunLoopRunInMode` the watcher thread sleeps in.
    fn stop(&self) {
        // SAFETY: the run loop is retained by `self`, and both calls
        // may come from any thread.
        unsafe {
            CFRunLoopStop(self.0);
            CFRunLoopWakeUp(self.0);
        }
    }
}

impl Drop for RunLoop {
    fn drop(&mut self) {
        // SAFETY: balances the `CFRetain` in `Tap::open`.
        unsafe { CFRelease(self.0.cast_const()) };
    }
}

impl Tap {
    fn open(interest: Interest) -> Option<(Self, RunLoop)> {
        // Accessibility covers listening too; where the system says
        // it does not, creating a tap would put up its own prompt.
        if !CGPreflightListenEventAccess() {
            return None;
        }
        let state = Box::into_raw(Box::new(TapState {
            interest,
            hit: AtomicBool::new(false),
            port: AtomicPtr::new(ptr::null_mut()),
        }));
        let mask = (1u64 << KEY_DOWN) | (1u64 << KEY_UP) | (1u64 << FLAGS_CHANGED);
        // SAFETY: plain values, a callback of the right type, and the
        // state it is handed back — alive until `Drop` invalidates the
        // port. Each object made here is released there, or below on
        // the way out.
        unsafe {
            let port = CGEventTapCreate(
                SESSION_TAP,
                HEAD_INSERT,
                LISTEN_ONLY,
                mask,
                on_event,
                state.cast(),
            );
            if port.is_null() {
                drop(Box::from_raw(state));
                return None;
            }
            (*state).port.store(port, Ordering::Release);
            let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
            if source.is_null() {
                CFMachPortInvalidate(port);
                CFRelease(port.cast_const());
                drop(Box::from_raw(state));
                return None;
            }
            let run_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
            CFRetain(run_loop.cast_const());
            Some((
                Self {
                    port,
                    source,
                    state,
                },
                RunLoop(run_loop),
            ))
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: on the watcher thread, outside its run loop: no
        // callback is running. Invalidating the port ends them and
        // takes the source out of the run loop; only then is the
        // state they read freed.
        unsafe {
            CGEventTapEnable(self.port, false);
            CFMachPortInvalidate(self.port);
            CFRelease(self.source.cast_const());
            CFRelease(self.port.cast_const());
            drop(Box::from_raw(self.state));
        }
    }
}

pub(crate) struct Keys {
    /// This read's flags, fetched on first need.
    flags: Cell<Option<u64>>,
    /// `None`: no tap; the state is read on a clock.
    tap: Option<Tap>,
}

pub(crate) fn open(interest: Interest) -> Option<(Keys, Kick)> {
    if !access_granted() {
        return None;
    }
    let (tap, kick): (Option<Tap>, Kick) = if let Some((tap, run_loop)) = Tap::open(interest) {
        (Some(tap), Box::new(move || run_loop.stop()))
    } else {
        warn!("Key watcher: no event tap; reading the keys on a clock");
        (None, Box::new(|| {}))
    };
    Some((
        Keys {
            flags: Cell::new(None),
            tap,
        },
        kick,
    ))
}

impl Keys {
    fn flags(&self) -> u64 {
        if let Some(flags) = self.flags.get() {
            return flags;
        }
        let flags = CGEventSourceFlagsState(COMBINED_SESSION);
        self.flags.set(Some(flags));
        flags
    }
}

impl Source for Keys {
    fn refresh(&mut self) -> bool {
        self.flags.set(None);
        true
    }

    fn key_down(&self, key: u16) -> bool {
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

    fn modifier_down(&self, modifier: Modifier) -> bool {
        let mask = match modifier {
            Modifier::Ctrl => FLAG_CONTROL,
            Modifier::Shift => FLAG_SHIFT,
            Modifier::Alt => FLAG_ALTERNATE,
        };
        self.flags() & mask != 0
    }

    fn has_events(&self) -> bool {
        self.tap.is_some()
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Woke {
        let Some(tap) = &self.tap else {
            crate::wait_on_the_clock(timeout);
            return Woke::TimedOut;
        };
        // SAFETY: the state lives as long as the tap, which `self`
        // owns.
        let state = unsafe { &*tap.state };
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if state.hit.swap(false, Ordering::AcqRel) {
                return Woke::Key;
            }
            let seconds = match deadline {
                None => FOR_EVER_SECS,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Woke::TimedOut;
                    }
                    left.as_secs_f64()
                }
            };
            // SAFETY: the current thread's run loop, in the mode the
            // tap's source was added to; it returns after one event.
            let result = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 1) };
            match result {
                // An event was served: of interest or not, look.
                RUN_HANDLED_SOURCE => {}
                // Time, or a kick.
                RUN_TIMED_OUT | RUN_STOPPED if state.hit.swap(false, Ordering::AcqRel) => {
                    return Woke::Key;
                }
                RUN_TIMED_OUT => return Woke::TimedOut,
                RUN_STOPPED => return Woke::Other,
                // Nothing left to run (the tap's source is gone): do
                // not spin on it.
                _ => {
                    crate::wait_on_the_clock(Some(POLL));
                    return Woke::Other;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_modifier_keys_are_known_on_both_sides() {
        for key in [0x36, 0x37, 0x38, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E] {
            assert!(is_modifier(key), "{key:#x}");
        }
        assert!(!is_modifier(0x62));
    }
}
