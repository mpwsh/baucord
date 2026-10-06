//! The app's folders, as `NSFileManager` names them (IOS-SPEC.md §8).
//!
//! - `Library/Application Support/baucord`: `settings.json` and the
//!   device identity key. Excluded from iCloud and computer backups:
//!   the key is a credential, and a backup restored onto a second
//!   iPhone would give the server two devices with one key (Android's
//!   `allowBackup="false"`, ANDROID-SPEC.md §8).
//! - `Documents`: `baucord.log`, and later the files users save. With
//!   `UIFileSharingEnabled` + `LSSupportsOpeningDocumentsInPlace` in
//!   the Info.plist this folder is the app's own in the Files app
//!   ("On My iPhone › baucord"), so a user can send the log without a
//!   Mac — the Android file log's job.
//!
//! Inside the app's sandbox only `Documents`, `Library` and `tmp` are
//! writable; `dirs::config_dir()` happens to land in Application
//! Support too, but the host names the folder explicitly so it can
//! exclude it from backup.

use std::path::{Path, PathBuf};

use objc2_foundation::{
    NSFileManager, NSNumber, NSSearchPathDirectory, NSSearchPathDomainMask, NSString,
    NSURLIsExcludedFromBackupKey, NSURL,
};

/// Where the app keeps things.
#[derive(Debug)]
pub struct AppDirs {
    /// `Library/Application Support/baucord`: settings, identity.
    pub config: PathBuf,
    /// `Documents`: the log, saved files.
    pub documents: PathBuf,
}

impl AppDirs {
    /// Look both folders up and create them. `None` only if the system
    /// cannot name them, which an app's sandbox always can — the host
    /// then runs without persistence rather than not at all. Runs
    /// before the logger exists, so failures go to stderr.
    pub fn resolve() -> Option<Self> {
        let support = user_directory(NSSearchPathDirectory::ApplicationSupportDirectory)?;
        let documents = user_directory(NSSearchPathDirectory::DocumentDirectory)?;
        let config = support.join("baucord");
        for dir in [&config, &documents] {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("baucord: could not create {}: {e}", dir.display());
                return None;
            }
        }
        Some(Self { config, documents })
    }
}

/// The first URL `NSFileManager` gives for `which` in the user domain,
/// as a path.
fn user_directory(which: NSSearchPathDirectory) -> Option<PathBuf> {
    let urls = NSFileManager::defaultManager()
        .URLsForDirectory_inDomains(which, NSSearchPathDomainMask::UserDomainMask);
    let Some(path) = urls.firstObject().and_then(|url| url.path()) else {
        eprintln!("baucord: the system names no folder for {which:?}");
        return None;
    };
    Some(PathBuf::from(path.to_string()))
}

/// Keep `dir` out of iCloud and computer backups. Logged, never fatal.
pub fn exclude_from_backup(dir: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
    let yes = NSNumber::new_bool(true);
    // SAFETY: `NSURLIsExcludedFromBackupKey` takes an `NSNumber`
    // holding a BOOL, which is what `yes` is.
    let result =
        unsafe { url.setResourceValue_forKey_error(Some(&yes), NSURLIsExcludedFromBackupKey) };
    match result {
        Ok(()) => log::debug!("{} excluded from backups", dir.display()),
        Err(e) => log::warn!(
            "could not exclude {} from backups: {}",
            dir.display(),
            e.localizedDescription()
        ),
    }
}
