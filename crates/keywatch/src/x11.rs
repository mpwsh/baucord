//! X11, over the connection `x11rb` opens (pure Rust).
//!
//! The state is one `QueryKeymap` round trip into a local 32-byte
//! reply (`device_query` made it through a pointer into a dropped
//! temporary — review P1-3). The events are `XInput2` raw key presses
//! and releases, selected on the root window: delivered to every
//! client that asks, whoever has the focus, and taken from nobody.
//!
//! No display (Wayland without Xwayland) and [`open`] fails. No
//! `XInput2` and the state is read on a clock.

use std::{
    io::{Read as _, Write as _},
    os::{
        fd::{AsRawFd as _, RawFd},
        unix::net::UnixStream,
    },
    time::{Duration, Instant},
};

use x11rb::{
    connection::Connection as _,
    protocol::{
        xinput::{ConnectionExt as _, EventMask, XIEventMask},
        xproto::ConnectionExt as _,
        Event,
    },
    rust_connection::RustConnection,
};

use crate::{Interest, Kick, Modifier, Source, Woke};

/// What an X11 keycode adds to the input event code.
const X11_OFFSET: u16 = 8;

/// `XIAllMasterDevices`: every keyboard, as the one the user types on.
const ALL_MASTER_DEVICES: u16 = 1;

/// Input event codes of the modifier keys, left and right.
const CTRL: (u16, u16) = (29, 97);
const SHIFT: (u16, u16) = (42, 54);
const ALT: (u16, u16) = (56, 100);
const META: (u16, u16) = (125, 126);

pub(crate) fn is_modifier(evdev: u16) -> bool {
    [CTRL, SHIFT, ALT, META]
        .iter()
        .any(|(left, right)| evdev == *left || evdev == *right)
}

pub(crate) fn access_granted() -> bool {
    true
}

pub(crate) fn ask_for_access() {}

pub(crate) struct Keys {
    conn: RustConnection,
    keymap: [u8; 32],
    interest: Interest,
    /// Raw key events were selected.
    events: bool,
    /// The reading end of the pipe a kick writes to.
    kicked: UnixStream,
}

pub(crate) fn open(interest: Interest) -> Option<(Keys, Kick)> {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let (kicked, kicker) = UnixStream::pair().ok()?;
    // Neither end waits: a kick repeated into a full pipe is dropped
    // (one is enough), and draining an empty one returns.
    kicked.set_nonblocking(true).ok()?;
    kicker.set_nonblocking(true).ok()?;
    let events = select_raw_keys(&conn, root);
    let kick: Kick = Box::new(move || {
        let _ = (&kicker).write(&[1]);
    });
    Some((
        Keys {
            conn,
            keymap: [0; 32],
            interest,
            events,
            kicked,
        },
        kick,
    ))
}

/// Ask for raw key presses and releases on the root window. `XInput`
/// 2.0 is enough, and has to be asked for before anything else of it.
fn select_raw_keys(conn: &RustConnection, root: u32) -> bool {
    let version = conn
        .xinput_xi_query_version(2, 0)
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    if version.is_none() {
        return false;
    }
    let mask = EventMask {
        deviceid: ALL_MASTER_DEVICES,
        mask: vec![XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_KEY_RELEASE],
    };
    conn.xinput_xi_select_events(root, &[mask])
        .ok()
        .is_some_and(|cookie| cookie.check().is_ok())
}

/// Whether X11 keycode `keycode` is down in a `QueryKeymap` reply: one
/// bit per keycode, eight to a byte, the lowest bit first.
fn keymap_bit(keys: &[u8; 32], keycode: u16) -> bool {
    keys.get(usize::from(keycode / 8))
        .is_some_and(|byte| byte & (1 << (keycode % 8)) != 0)
}

impl Keys {
    /// Take what the connection holds in memory; whether a key of
    /// interest was among it. Events also arrive behind a reply
    /// ([`Source::refresh`]'s), so this runs before every sleep.
    fn drain(&mut self) -> bool {
        let mut hit = false;
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            if let Event::XinputRawKeyPress(key) | Event::XinputRawKeyRelease(key) = event {
                let evdev = u16::try_from(key.detail)
                    .ok()
                    .and_then(|keycode| keycode.checked_sub(X11_OFFSET));
                hit |= evdev.is_some_and(|evdev| self.interest.wants(evdev));
            }
        }
        hit
    }
}

impl Source for Keys {
    /// One `QueryKeymap`; false once the connection is gone.
    fn refresh(&mut self) -> bool {
        let Ok(cookie) = self.conn.query_keymap() else {
            return false;
        };
        match cookie.reply() {
            Ok(reply) => {
                self.keymap = reply.keys;
                true
            }
            Err(_) => false,
        }
    }

    fn key_down(&self, evdev: u16) -> bool {
        keymap_bit(&self.keymap, evdev + X11_OFFSET)
    }

    fn modifier_down(&self, modifier: Modifier) -> bool {
        let (left, right) = match modifier {
            Modifier::Ctrl => CTRL,
            Modifier::Shift => SHIFT,
            Modifier::Alt => ALT,
        };
        self.key_down(left) || self.key_down(right)
    }

    fn has_events(&self) -> bool {
        self.events
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Woke {
        let deadline = timeout.map(|t| Instant::now() + t);
        let x11: RawFd = self.conn.stream().as_raw_fd();
        let kick: RawFd = self.kicked.as_raw_fd();
        loop {
            if self.drain() {
                return Woke::Key;
            }
            let ms = match deadline {
                None => -1,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Woke::TimedOut;
                    }
                    // Rounded up: a wait of 0 would spin.
                    i32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
                }
            };
            let mut fds = [
                libc::pollfd {
                    fd: x11,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: kick,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `fds` is two initialized entries and outlives the
            // call; both descriptors are open for as long as `self` is.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Woke::Other;
            }
            if ready == 0 {
                return Woke::TimedOut;
            }
            if fds[1].revents != 0 {
                let mut spent = [0u8; 64];
                while matches!((&self.kicked).read(&mut spent), Ok(n) if n > 0) {}
                return Woke::Other;
            }
            if fds[0].revents & libc::POLLIN == 0 {
                // Hung up or in error: the next `refresh` says so.
                return Woke::Other;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_modifier_keys_are_known_on_both_sides() {
        for code in [29, 97, 42, 54, 56, 100, 125, 126] {
            assert!(is_modifier(code), "{code}");
        }
        assert!(!is_modifier(65));
    }
}
