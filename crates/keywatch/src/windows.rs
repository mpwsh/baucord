//! Windows.
//!
//! The state is `GetAsyncKeyState`, one call per bound key and
//! modifier per read (the whole keyboard was 256 calls a pass). The
//! events are Raw Input, sent to a message-only window of the watcher
//! thread with `RIDEV_INPUTSINK`: every key, whoever has the focus,
//! and taken from nobody.
//!
//! A process has one Raw Input target per kind of device, and the
//! window toolkit registers its own for the keyboard at startup. Ours
//! replaces it while the watcher runs and puts it back when it ends
//! ([`RawInput`]); the toolkit's key events for its own windows are
//! the ordinary window messages, which neither registration touches.

use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

use log::warn;
use windows::{
    core::w,
    Win32::{
        Devices::HumanInterfaceDevice::{HID_USAGE_GENERIC_KEYBOARD, HID_USAGE_PAGE_GENERIC},
        Foundation::{HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM},
        System::Threading::{GetCurrentThreadId, INFINITE},
        UI::{
            Input::{
                GetRawInputData, GetRegisteredRawInputDevices, KeyboardAndMouse::GetAsyncKeyState,
                RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
                RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT, RIM_TYPEKEYBOARD,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjectsEx,
                PeekMessageW, PostThreadMessageW, HWND_MESSAGE, MSG, MWMO_INPUTAVAILABLE,
                PM_REMOVE, QS_ALLINPUT, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_NULL,
            },
        },
    },
};

use crate::{Interest, Kick, Modifier, Source, Woke, POLL};

// Either side of each modifier…
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
// …and one side, as the key table names them.
const VK_LSHIFT: u16 = 0xA0;
const VK_RSHIFT: u16 = 0xA1;
const VK_LCONTROL: u16 = 0xA2;
const VK_RCONTROL: u16 = 0xA3;
const VK_LMENU: u16 = 0xA4;
const VK_RMENU: u16 = 0xA5;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;

/// The scan code of the right Shift key (the left one is 0x2A).
const SCAN_RIGHT_SHIFT: u16 = 0x36;
/// `RI_KEY_E0`: the scan code has the E0 prefix (the right-hand Ctrl
/// and Alt, among others).
const RAW_KEY_E0: u16 = 2;
/// A raw key that is part of an escaped sequence and names no key.
const VK_NONE: u16 = 0xFF;

pub(crate) fn is_modifier(vk: u16) -> bool {
    matches!(
        vk,
        VK_SHIFT..=VK_MENU | VK_LSHIFT..=VK_RMENU | VK_LWIN | VK_RWIN
    )
}

pub(crate) fn access_granted() -> bool {
    true
}

pub(crate) fn ask_for_access() {}

pub(crate) struct Keys {
    interest: Interest,
    /// `None`: Raw Input could not be had; the state is read on a
    /// clock.
    raw: Option<RawInput>,
}

// The same signature as the other platforms', which can fail.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn open(interest: Interest) -> Option<(Keys, Kick)> {
    // SAFETY: no preconditions.
    let thread = unsafe { GetCurrentThreadId() };
    let raw = RawInput::open();
    if raw.is_none() {
        warn!("Key watcher: Raw Input not available; reading the keys on a clock");
    }
    let kick: Kick = Box::new(move || {
        // SAFETY: plain values. A thread that has ended (or has no
        // message queue: no Raw Input) makes this fail, nothing more.
        let _ = unsafe { PostThreadMessageW(thread, WM_NULL, WPARAM(0), LPARAM(0)) };
    });
    Some((Keys { interest, raw }, kick))
}

/// The watcher thread's message-only window, registered as the
/// process's Raw Input target for keyboards, and the registration it
/// replaced.
struct RawInput {
    window: HWND,
    replaced: Option<RAWINPUTDEVICE>,
}

/// `size_of::<T>()` as the `u32` the Raw Input calls take.
const fn size_u32<T>() -> u32 {
    // A handful of bytes, each of them.
    #[allow(clippy::cast_possible_truncation)]
    {
        size_of::<T>() as u32
    }
}

fn is_keyboard(device: &RAWINPUTDEVICE) -> bool {
    device.usUsagePage == HID_USAGE_PAGE_GENERIC && device.usUsage == HID_USAGE_GENERIC_KEYBOARD
}

/// The process's Raw Input registration for keyboards, if it has one.
fn registered_keyboard() -> Option<RAWINPUTDEVICE> {
    let mut count = 0u32;
    // SAFETY: with no buffer, the call writes the count and nothing
    // else.
    unsafe { GetRegisteredRawInputDevices(None, &raw mut count, size_u32::<RAWINPUTDEVICE>()) };
    if count == 0 {
        return None;
    }
    let mut devices = vec![RAWINPUTDEVICE::default(); usize::try_from(count).ok()?];
    // SAFETY: `devices` holds `count` entries, as `count` says.
    let got = unsafe {
        GetRegisteredRawInputDevices(
            Some(devices.as_mut_ptr()),
            &raw mut count,
            size_u32::<RAWINPUTDEVICE>(),
        )
    };
    // `(UINT)-1`: the list changed between the two calls, or failed.
    if got == u32::MAX {
        return None;
    }
    devices.truncate(usize::try_from(got).ok()?);
    devices.into_iter().find(is_keyboard)
}

impl RawInput {
    fn open() -> Option<Self> {
        // SAFETY: a message-only window of the system's own `STATIC`
        // class: no class of ours, no window procedure of ours, and no
        // pointer but the two constant strings.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("baucord key watch"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .ok()?;
        let replaced = registered_keyboard();
        let ours = RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_KEYBOARD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        };
        // SAFETY: one initialized entry, and its size.
        let registered = unsafe { RegisterRawInputDevices(&[ours], size_u32::<RAWINPUTDEVICE>()) };
        if registered.is_err() {
            // SAFETY: the window is ours, made above on this thread.
            let _ = unsafe { DestroyWindow(window) };
            return None;
        }
        Some(Self { window, replaced })
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        // What was there before comes back; with nothing before, ours
        // is removed (which takes a null target).
        let back = self.replaced.unwrap_or(RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_KEYBOARD,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        });
        // SAFETY: one initialized entry and its size; the window is
        // ours and this is the thread that made it (the watcher's).
        unsafe {
            let _ = RegisterRawInputDevices(&[back], size_u32::<RAWINPUTDEVICE>());
            let _ = DestroyWindow(self.window);
        }
    }
}

/// The key a `WM_INPUT` message is about, as the key table names it;
/// `None` for what is not a key.
fn raw_key(lparam: LPARAM) -> Option<u16> {
    let mut input = RAWINPUT::default();
    let mut size = size_u32::<RAWINPUT>();
    // SAFETY: `input` is `size` bytes of a `RAWINPUT`, which holds a
    // keyboard's or a mouse's event whole; a longer one (a HID
    // report) fails the call, which writes nothing then.
    let got = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut c_void),
            RID_INPUT,
            Some((&raw mut input).cast()),
            &raw mut size,
            size_u32::<RAWINPUTHEADER>(),
        )
    };
    if got == u32::MAX || input.header.dwType != RIM_TYPEKEYBOARD.0 {
        return None;
    }
    // SAFETY: the header says the union holds a keyboard event.
    let key = unsafe { input.data.keyboard };
    let extended = key.Flags & RAW_KEY_E0 != 0;
    // Raw Input names a modifier without its side; the scan code has it.
    Some(match key.VKey {
        VK_NONE => return None,
        VK_SHIFT if key.MakeCode == SCAN_RIGHT_SHIFT => VK_RSHIFT,
        VK_SHIFT => VK_LSHIFT,
        VK_CONTROL if extended => VK_RCONTROL,
        VK_CONTROL => VK_LCONTROL,
        VK_MENU if extended => VK_RMENU,
        VK_MENU => VK_LMENU,
        vk => vk,
    })
}

impl Keys {
    /// Take the thread's queued messages; whether a key of interest
    /// was among them.
    fn drain(&self) -> bool {
        let mut hit = false;
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid place for a message; the queue is
        // this thread's.
        while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_INPUT {
                hit |= raw_key(msg.lParam).is_some_and(|key| self.interest.wants(key));
            }
            // A window's message goes to its procedure (the system's,
            // which is what frees a raw input); a thread's — the kick
            // — has none.
            if !msg.hwnd.is_invalid() {
                // SAFETY: a message just taken from this thread's queue.
                unsafe { DispatchMessageW(&raw const msg) };
            }
        }
        hit
    }
}

impl Source for Keys {
    fn refresh(&mut self) -> bool {
        true
    }

    fn key_down(&self, vk: u16) -> bool {
        // The high bit says "down now". The low bit ("pressed since
        // the last call") is shared with every other caller.
        // SAFETY: no preconditions: any value is a key code, or none.
        unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
    }

    fn modifier_down(&self, modifier: Modifier) -> bool {
        self.key_down(match modifier {
            Modifier::Ctrl => VK_CONTROL,
            Modifier::Shift => VK_SHIFT,
            Modifier::Alt => VK_MENU,
        })
    }

    fn has_events(&self) -> bool {
        self.raw.is_some()
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Woke {
        if self.raw.is_none() {
            crate::wait_on_the_clock(timeout);
            return Woke::TimedOut;
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        // One sleep, then what it brought: a key of interest, or
        // anything else (a kick, another key, the time) — the caller
        // reads again either way, and comes back to sleep.
        let ms = match deadline {
            None => INFINITE,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                // Rounded up: a wait of 0 would spin.
                u32::try_from(left.as_micros().div_ceil(1000))
                    .unwrap_or(INFINITE - 1)
                    .max(1)
            }
        };
        if self.drain() {
            return Woke::Key;
        }
        // SAFETY: no handles, plain values. `MWMO_INPUTAVAILABLE`:
        // return for what is queued, not only for what arrives after
        // the call.
        let woke =
            unsafe { MsgWaitForMultipleObjectsEx(None, ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        if woke == WAIT_TIMEOUT {
            return Woke::TimedOut;
        }
        if woke != WAIT_OBJECT_0 {
            // The wait itself failed: do not spin on it.
            crate::wait_on_the_clock(Some(POLL));
            return Woke::Other;
        }
        // Any message wakes this wait — a kick, another key, one for a
        // window — and none of them is the time running out (P2-34).
        if self.drain() {
            Woke::Key
        } else {
            Woke::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_modifier_keys_are_known_on_both_sides() {
        for vk in [
            0x10, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C,
        ] {
            assert!(is_modifier(vk), "{vk:#x}");
        }
        assert!(!is_modifier(0x76));
    }
}
