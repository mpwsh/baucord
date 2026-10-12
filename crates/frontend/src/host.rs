//! What the host OS does for the UI and no engine crate can: native
//! file dialogs and opening a URL or path in whatever handles it.
//! Desktop uses `rfd`/`pollster`/`open` (feature `desktop`); the
//! other build of this module (`stubs/host.rs`) logs and does
//! nothing, until the `Platform` trait of ANDROID-SPEC.md §5 hands
//! these to the Android host.
//!
//! Dialogs are async on the portal backend, so each runs on a scratch
//! thread and calls `on_pick` there; callers hop back to the UI with
//! `slint::invoke_from_event_loop` themselves.

use std::path::{Path, PathBuf};

/// A filter for a dialog: a label and the extensions it admits.
pub type Filter = (String, Vec<String>);

/// Saving asks where (`save_file`); the mobile twin has no dialog
/// and saves into Downloads instead.
pub const HAS_SAVE_DIALOG: bool = true;

/// The clipboard takes pictures (`clipboard::set_clipboard_image`); a
/// phone's has no way in from here, and its menus leave "Copy image"
/// out.
pub const HAS_IMAGE_CLIPBOARD: bool = true;

/// A file written into the user's Downloads.
pub struct Saved {
    /// The path written.
    pub target: PathBuf,
    /// Where it went, for the toast.
    pub shown: String,
}

/// The desktop writes Downloads itself (`media::save_to_downloads`).
#[must_use]
pub fn save_download(_name: &str, _mime: &str, _bytes: &[u8]) -> Option<Result<Saved, String>> {
    None
}

/// A desktop process keeps running in the background by itself.
pub fn keep_alive(_server: Option<&str>, _in_room: bool) {}

pub use crate::notice::{ChimeRoute, Notice, NoticeKind};

/// No notifications on the desktop yet: the window, the tray and the
/// chimes carry everything (`stubs/host.rs` has the phones').
#[must_use]
pub fn notifies() -> bool {
    false
}

pub fn notify(_notice: &Notice) {}

#[must_use]
pub fn notices_allowed(_kind: NoticeKind) -> bool {
    false
}

pub fn cancel_notice(_key: &str) {}

/// The desktop's chimes always go through the voice engine.
#[must_use]
pub fn chime_route() -> ChimeRoute {
    ChimeRoute::App
}

/// The desktop's engine always has playback open.
#[must_use]
pub fn play_chime() -> bool {
    false
}

/// The desktop's viewer is a window the user arranges; the display's
/// sleep is left to the OS (a phone keeps its screen on while a stream
/// plays, `stubs/host.rs`).
pub fn keep_screen_on(_on: bool) {}

/// Whether a saved file is still where it was saved.
#[must_use]
pub fn target_exists(target: &Path) -> bool {
    target.exists()
}

/// Ask the user for one existing file.
pub fn pick_file(filter: Option<Filter>, on_pick: impl FnOnce(PathBuf) + Send + 'static) {
    std::thread::spawn(move || {
        let mut dialog = rfd::AsyncFileDialog::new();
        if let Some((name, exts)) = &filter {
            let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
            dialog = dialog.add_filter(name, &exts);
        }
        if let Some(handle) = pollster::block_on(dialog.pick_file()) {
            on_pick(handle.path().to_path_buf());
        }
    });
}

/// Ask the user where to write a file, suggesting `default_name`.
pub fn save_file(
    default_name: &str,
    filter: Option<Filter>,
    on_pick: impl FnOnce(PathBuf) + Send + 'static,
) {
    let default_name = default_name.to_string();
    std::thread::spawn(move || {
        let mut dialog = rfd::AsyncFileDialog::new().set_file_name(&default_name);
        if let Some((name, exts)) = &filter {
            let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
            dialog = dialog.add_filter(name, &exts);
        }
        if let Some(handle) = pollster::block_on(dialog.save_file()) {
            on_pick(handle.path().to_path_buf());
        }
    });
}

/// Open a URL, file or folder with the system handler, detached.
///
/// # Errors
/// The launcher's own message when nothing could be started.
pub fn open_target(target: &Path) -> Result<(), String> {
    open::that_detached(target).map_err(|e| e.to_string())
}

/// [`open_target`] for a URL string.
///
/// # Errors
/// As [`open_target`].
pub fn open_url(url: &str) -> Result<(), String> {
    open::that_detached(url).map_err(|e| e.to_string())
}

/// Open the system's sound settings where what happens to other apps
/// during a call is chosen: Windows' Sound dialog on its Communications
/// tab (`AEC-SPEC.md` §4.6).
///
/// # Errors
/// No such setting on this system, or the launcher's own message.
pub fn open_sound_settings() -> Result<(), String> {
    #[cfg(windows)]
    {
        // By its full path: a bare name is looked up next to our own
        // executable first.
        let control = std::env::var_os("SystemRoot").map_or_else(
            || PathBuf::from("control.exe"),
            |root| PathBuf::from(root).join("System32").join("control.exe"),
        );
        std::process::Command::new(control)
            .arg("mmsys.cpl,,3")
            .spawn()
            .map(drop)
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err("no such setting on this system".to_string())
    }
}
