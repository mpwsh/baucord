//! `host` without a desktop: no native dialogs, no system launcher.
//! Every call logs once and does nothing; the Android host will
//! supply these through the `Platform` trait (ANDROID-SPEC.md §5).

use std::path::{Path, PathBuf};

pub type Filter = (String, Vec<String>);

pub fn pick_file(_filter: Option<Filter>, _on_pick: impl FnOnce(PathBuf) + Send + 'static) {
    log::info!("no file dialog on this host");
}

pub fn save_file(
    _default_name: &str,
    _filter: Option<Filter>,
    _on_pick: impl FnOnce(PathBuf) + Send + 'static,
) {
    log::info!("no save dialog on this host");
}

/// # Errors
/// Always: there is no launcher here.
pub fn open_target(_target: &Path) -> Result<(), String> {
    Err("no system launcher on this host".to_string())
}

/// # Errors
/// Always: there is no launcher here.
pub fn open_url(_url: &str) -> Result<(), String> {
    Err("no system launcher on this host".to_string())
}
