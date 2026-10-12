//! The keys a keybind can name, in each platform's own code.

/// No key of its own on that platform.
pub(crate) const NONE: u16 = u16::MAX;

/// Every W3C key code a keybind can name (what the keybind strings
/// carry), with that key on each platform the watcher reads: the
/// Windows virtual key, the macOS virtual key code (`kVK_*`), and the
/// Linux input event code (`KEY_*`; an X11 keycode is that plus 8, as
/// `device_query` read it).
#[rustfmt::skip]
pub(crate) const KEYS: &[(&str, u16, u16, u16)] = &[
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

/// The key a W3C code names on this platform, in the code the watcher
/// reads ([`crate::Binding::key`]). `None` for a key the watcher
/// cannot read here: such a binding is registered as a hotkey instead.
#[must_use]
pub fn platform_key(code: &str) -> Option<u16> {
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
}
