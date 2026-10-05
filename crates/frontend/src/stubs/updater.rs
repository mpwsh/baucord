//! `updater` without one: no version check, no download, no install.
//! A sideloaded APK updates by hand (ANDROID-SPEC.md §12, later).

/// No check to run — but the splash waits for the check's verdict, so
/// deliver the "nothing new" verdict at once (the same call the real
/// updater makes on `None`), or the UI never reaches the connect
/// screen when `startup.check_updates` is on, which is the default.
pub fn spawn_version_check() {
    let _ = slint::invoke_from_event_loop(|| {
        crate::app::with_current(|app| app.show_connect_phase());
    });
}

pub fn start_download(_app: &mut crate::app::AppState) {
    log::info!("no updater on this host");
}

/// # Errors
/// Always: there is no updater here.
pub fn extract_and_install(_zip_path: &str) -> Result<(), String> {
    Err("no updater on this host".to_string())
}
