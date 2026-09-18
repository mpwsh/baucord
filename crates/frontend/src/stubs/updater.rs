//! `updater` without one: no version check, no download, no install.
//! A sideloaded APK updates by hand (ANDROID-SPEC.md §12, later).

pub fn spawn_version_check() {}

pub fn start_download(_app: &mut crate::app::AppState) {
    log::info!("no updater on this host");
}

/// # Errors
/// Always: there is no updater here.
pub fn extract_and_install(_zip_path: &str) -> Result<(), String> {
    Err("no updater on this host".to_string())
}
