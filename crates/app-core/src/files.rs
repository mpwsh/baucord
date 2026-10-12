//! What the phone hosts do with a file the user picked or saved: a
//! name that is safe to write under, a path nobody has yet, and the
//! one pick that may be in flight. Each host had its own copy (review
//! P3-44).

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// A file name that is one path component: separators and control
/// characters become `_`, and a name that is empty or names a
/// directory becomes `file`.
#[must_use]
pub fn safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = safe.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `dir/name`, or `dir/name (2).ext` and so on if taken.
#[must_use]
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let base = Path::new(name);
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = base
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut path = dir.join(name);
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    path
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
        for empty in ["", "   ", ".", ".."] {
            assert_eq!(safe_name(empty), "file", "{empty:?}");
        }
        // A traversal is flattened, not followed.
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
    }

    #[test]
    fn a_taken_name_gets_a_number_before_its_extension() {
        let dir = std::env::temp_dir().join(format!("baucord-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"1").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("a (2).txt"), b"2").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (3).txt"));
        std::fs::write(dir.join("notes"), b"3").unwrap();
        assert_eq!(unique_path(&dir, "notes"), dir.join("notes (2)"));
        let _ = std::fs::remove_dir_all(&dir);
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
