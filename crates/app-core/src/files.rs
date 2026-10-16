//! What the hosts do with a file the user picked or saved: a name that
//! is safe to write under, a new file nobody has yet, whether it can
//! run code, and the one pick that may be in flight. Each host had its
//! own copy (review P3-44).

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// A file name that is one path component and that every desktop can
/// write and open by that name: separators, control characters and the
/// characters Windows refuses (`< > : " | ? *`) become `_`; invisible
/// format characters go (a right-to-left override, U+202E, made
/// `invoice` + it + `fdp.exe` read as a PDF); trailing dots and spaces go
/// (Windows drops them); a reserved device name (`CON`, `NUL`,
/// `COM1`, …, with any extension) gets a `_` before it; the name is
/// cut to [`MAX_NAME_BYTES`], keeping its extension; and a name that is
/// empty or names a directory becomes `file` (review 2026-10, P2-19).
#[must_use]
pub fn safe_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .filter(|&c| !is_invisible_format(c))
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = mapped.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return "file".to_string();
    }
    let stem = trimmed.split('.').next().unwrap_or_default().trim_end();
    let named = if is_reserved_device(stem) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    cut_to(named, MAX_NAME_BYTES)
}

/// The longest saved name, in bytes: well inside every file system's
/// 255, with room for the ` (2)` of a name taken already.
pub(crate) const MAX_NAME_BYTES: usize = 150;

/// Characters that draw nothing and can reorder or hide what is drawn
/// around them: the bidi controls and marks, zero-width characters,
/// the byte-order mark.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// A name Windows takes as a device, whatever follows its first dot.
fn is_reserved_device(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let (Some(head), Some(tail)) = (upper.get(..3), upper.get(3..)) else {
                return false;
            };
            matches!(head, "COM" | "LPT")
                && matches!(
                    tail,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
        }
    }
}

/// `name` cut to at most `max` bytes on a character boundary, its
/// extension (up to 16 bytes of it) kept.
fn cut_to(name: String, max: usize) -> String {
    if name.len() <= max {
        return name;
    }
    let ext = Path::new(&name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 16)
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut end = max - ext.len();
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", name[..end].trim_end())
}

/// Write `bytes` to a new file `dir/name`, or `dir/name (2).ext` and so
/// on if that is taken; the path written. Each try creates the file
/// only if it is not there (`create_new`), so two saves of one name at
/// once cannot overwrite each other, nor anything already there.
///
/// # Errors
/// The I/O error of creating or writing the file.
pub fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let base = Path::new(name);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = base
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for n in 1..=10_000 {
        let path = if n == 1 {
            dir.join(name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "every numbered name is taken",
    ))
}

/// Whether a file of this name can run code when it is opened: a
/// program, a script, an installer, a shortcut. Its Open shows it in
/// its folder instead (review 2026-10, P1-9).
#[must_use]
pub fn is_runnable_name(name: &str) -> bool {
    const RUNNABLE: &[&str] = &[
        // Windows
        "exe",
        "com",
        "scr",
        "pif",
        "msi",
        "msp",
        "msix",
        "appx",
        "bat",
        "cmd",
        "ps1",
        "psm1",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "hta",
        "cpl",
        "lnk",
        "url",
        "reg",
        "inf",
        "scf",
        "msc",
        "jar",
        "appref-ms",
        "application",
        "gadget",
        "chm",
        "iso",
        "img",
        "vhd",
        "vhdx",
        // macOS
        "app",
        "command",
        "tool",
        "pkg",
        "mpkg",
        "dmg",
        "terminal",
        "workflow",
        // Linux and anywhere with a shell
        "sh",
        "bash",
        "run",
        "bin",
        "appimage",
        "desktop",
        "deb",
        "rpm",
        "py",
        "pl",
    ];
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| RUNNABLE.iter().any(|r| e.eq_ignore_ascii_case(r)))
}

/// A pick older than this is taken as lost (the picker's screen went
/// away with it open) and no longer blocks a new one.
pub const PICK_STALE: Duration = Duration::from_mins(10);

/// The file pick in flight, with the callback `F` that waits for it.
///
/// One at a time: a second Attach while a large file is still being
/// copied would delete the copy under it, and could hand one pick's
/// file to the other's callback (an attachment as the avatar).
///
/// The callback is *claimed* by the first delivery of a file
/// ([`Self::claim`]) and the slot stays taken until that file is
/// handed over or has failed ([`Self::finish`]) — a picker that
/// reports one pick twice finds nothing to claim the second time.
pub struct PickSlot<F> {
    slot: Mutex<Option<Pick<F>>>,
}

struct Pick<F> {
    /// `None` once claimed.
    on_pick: Option<F>,
    since: Instant,
}

impl<F> Default for PickSlot<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> PickSlot<F> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Pick<F>>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the slot for a new pick. `Err` hands `on_pick` back: a
    /// pick is in flight (and not older than [`PICK_STALE`]).
    ///
    /// # Errors
    /// The slot is taken.
    pub fn begin(&self, on_pick: F) -> Result<(), F> {
        self.begin_at(on_pick, Instant::now())
    }

    fn begin_at(&self, on_pick: F, now: Instant) -> Result<(), F> {
        let mut slot = self.lock();
        if slot
            .as_ref()
            .is_some_and(|pick| now.saturating_duration_since(pick.since) < PICK_STALE)
        {
            return Err(on_pick);
        }
        *slot = Some(Pick {
            on_pick: Some(on_pick),
            since: now,
        });
        Ok(())
    }

    /// The waiting callback, for a file just delivered — once. The
    /// slot stays taken. `None`: no pick waits, it was cancelled, or
    /// this is a second delivery of the same pick.
    pub fn claim(&self) -> Option<F> {
        self.lock().as_mut().and_then(|pick| pick.on_pick.take())
    }

    /// The pick is over: free the slot. The callback, if nothing had
    /// claimed it.
    pub fn finish(&self) -> Option<F> {
        self.lock().take().and_then(|pick| pick.on_pick)
    }

    /// Nothing was picked: free the slot — unless a file already
    /// delivered is still being copied, which ends the pick itself.
    /// Whether the slot was freed.
    pub fn cancel(&self) -> bool {
        let mut slot = self.lock();
        if slot.as_ref().is_some_and(|pick| pick.on_pick.is_some()) {
            *slot = None;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_made_one_path_component() {
        assert_eq!(safe_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_name("a/b\\c:d.txt"), "a_b_c_d.txt");
        assert_eq!(safe_name("tab\there\n"), "tab_here_");
        assert_eq!(safe_name("  spaced.png  "), "spaced.png");
        for empty in ["", "   ", ".", "..", " . "] {
            assert_eq!(safe_name(empty), "file", "{empty:?}");
        }
        // A traversal is flattened, not followed.
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
    }

    #[test]
    fn a_name_windows_could_not_write_or_would_misread_is_mended() {
        assert_eq!(safe_name("a<b>c|d?e*\"f\".txt"), "a_b_c_d_e__f_.txt");
        // The override hid the real extension.
        assert_eq!(safe_name("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
        assert_eq!(safe_name("zero\u{200B}width\u{FEFF}.png"), "zerowidth.png");
        assert_eq!(safe_name("notes. . ."), "notes");
        assert_eq!(safe_name("CON"), "_CON");
        assert_eq!(safe_name("nul.tar.gz"), "_nul.tar.gz");
        assert_eq!(safe_name("com7.log"), "_com7.log");
        assert_eq!(safe_name("console.txt"), "console.txt");
        assert_eq!(safe_name("com10.txt"), "com10.txt");
        let long = format!("{}.pdf", "é".repeat(200));
        let cut = safe_name(&long);
        assert!(cut.len() <= MAX_NAME_BYTES, "{}", cut.len());
        assert_eq!(cut.rsplit_once('.').map(|(_, ext)| ext), Some("pdf"));
    }

    #[test]
    fn a_taken_name_gets_a_number_and_nothing_is_overwritten() {
        let dir = std::env::temp_dir().join(format!("baucord-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(write_new(&dir, "a.txt", b"1").unwrap(), dir.join("a.txt"));
        assert_eq!(
            write_new(&dir, "a.txt", b"2").unwrap(),
            dir.join("a (2).txt")
        );
        assert_eq!(
            write_new(&dir, "a.txt", b"3").unwrap(),
            dir.join("a (3).txt")
        );
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"1");
        assert_eq!(write_new(&dir, "notes", b"4").unwrap(), dir.join("notes"));
        assert_eq!(
            write_new(&dir, "notes", b"5").unwrap(),
            dir.join("notes (2)")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn programs_and_scripts_are_told_from_documents() {
        for runnable in [
            "setup.exe",
            "RUN.BAT",
            "x.ps1",
            "a.lnk",
            "b.app",
            "c.sh",
            "d.AppImage",
        ] {
            assert!(is_runnable_name(runnable), "{runnable}");
        }
        for document in ["a.pdf", "b.png", "c.txt", "d.zip", "noext", "e.docx"] {
            assert!(!is_runnable_name(document), "{document}");
        }
    }

    #[test]
    fn one_pick_at_a_time_and_a_cancel_frees_the_slot() {
        let slot: PickSlot<&str> = PickSlot::new();
        assert!(slot.begin("first").is_ok());
        assert_eq!(slot.begin("second"), Err("second"), "busy");
        assert!(slot.cancel());
        assert!(!slot.cancel(), "nothing left to cancel");
        assert!(slot.begin("third").is_ok());
        assert_eq!(slot.finish(), Some("third"));
        assert_eq!(slot.finish(), None);
    }

    /// The simulator reported one Files pick twice (IOS-SPEC.md D36):
    /// the first delivery moved the file, the second found it gone.
    #[test]
    fn a_delivery_claims_the_callback_once_and_keeps_the_slot_while_it_copies() {
        let slot: PickSlot<&str> = PickSlot::new();
        assert!(slot.begin("pick").is_ok());
        assert_eq!(slot.claim(), Some("pick"));
        assert_eq!(slot.claim(), None, "the second delivery finds nothing");
        // The picker's "dismissed" arrives while the file is copied.
        assert!(!slot.cancel(), "a copy in progress is not a cancel");
        assert_eq!(slot.begin("another"), Err("another"), "still busy");
        // Handed over (or failed): the slot is free again.
        assert_eq!(slot.finish(), None);
        assert!(slot.begin("another").is_ok());
    }

    #[test]
    fn a_pick_that_never_came_back_stops_blocking() {
        let slot: PickSlot<&str> = PickSlot::new();
        let start = Instant::now();
        assert!(slot.begin_at("lost", start).is_ok());
        let soon = start + PICK_STALE.saturating_sub(Duration::from_secs(1));
        assert_eq!(slot.begin_at("early", soon), Err("early"));
        let late = start + PICK_STALE + Duration::from_secs(1);
        assert!(slot.begin_at("new", late).is_ok());
        assert_eq!(slot.claim(), Some("new"));
    }
}
