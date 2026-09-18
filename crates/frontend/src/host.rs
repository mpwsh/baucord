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
