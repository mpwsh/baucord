// ui/splash.rs — Startup splash that checks for updates.
//
// Shown before the connect screen. Spawns a background thread to
// check the remote version. If an update is available, presents
// "Update" / "Skip" buttons. Otherwise transitions straight to connect.

use fltk::{prelude::*, *};
use log::{info, warn};

use crate::ui::messages::{UiMessage, VersionCheckResult};
use i18n::t;
use ui::theme;
use ui::widgets::controls;

/// Remote endpoint that returns just the latest version string.
const VERSION_URL: &str = "https://baucord.mpw.sh/version";

/// Compiled-in version of this binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Base URL for downloads — append the platform-specific filename.
const DOWNLOAD_BASE: &str = "https://baucord.mpw.sh";

/// Platform-specific zip filename suffix.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const PLATFORM_SUFFIX: &str = "win-x64";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const PLATFORM_SUFFIX: &str = "macos-arm64";

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const PLATFORM_SUFFIX: &str = "macos-intel";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PLATFORM_SUFFIX: &str = "linux-x64";

/// Build the versioned download URL for a given version string.
pub fn download_url(version: &str) -> String {
    format!("{DOWNLOAD_BASE}/baucord-v{version}-{PLATFORM_SUFFIX}.zip")
}

/// Handles returned to App.
pub struct SplashHandles {
    pub window: window::Window,
}

/// Show the splash and kick off the version check.
///
/// The splash starts as a "Checking for updates..." label. The
/// background thread sends `UiMessage::VersionCheckComplete` when done,
/// and App decides what to do next.
pub fn show(sender: app::Sender<UiMessage>) -> SplashHandles {
    let (screen_w, screen_h) = app::screen_size();
    let w = 400;
    let h = 200;

    let mut wind = window::Window::default()
        .with_size(w, h)
        .with_label("baucord");
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.set_border(false);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(10);

    let mut title = frame::Frame::default()
        .with_label("baucord")
        .with_align(enums::Align::Center);
    title.set_label_size(24);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 35);

    let mut status = frame::Frame::default()
        .with_label(&t!("splash-checking"))
        .with_align(enums::Align::Center);
    status.set_label_size(12);
    status.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&status, 20);

    // Spacer — fills remaining space
    frame::Frame::default();

    col.end();
    wind.end();
    wind.show();

    // Spawn the version check thread
    check_version_in_background(sender);

    SplashHandles { window: wind }
}

/// Show the "update available" prompt as a new window.
///
/// Hides the existing splash and creates a fresh window with version
/// info and Update / Skip buttons. Returns the new window handle.
pub fn show_update_prompt(
    sender: app::Sender<UiMessage>,
    remote_version: &str,
    splash: &mut window::Window,
) -> window::Window {
    splash.hide();

    let (screen_w, screen_h) = app::screen_size();
    let w = 400;
    let h = 200;

    let mut wind = window::Window::default()
        .with_size(w, h)
        .with_label(&t!("update-available-title"));
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(10);

    let mut title = frame::Frame::default()
        .with_label(&t!("update-available-heading"))
        .with_align(enums::Align::Center);
    title.set_label_size(18);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 30);

    let msg = t!(
        "update-version-info",
        "current" => CURRENT_VERSION,
        "latest" => remote_version,
    );
    let mut info = frame::Frame::default()
        .with_label(&msg)
        .with_align(enums::Align::Center | enums::Align::Inside);
    info.set_label_size(13);
    info.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&info, 40);

    // Spacer
    frame::Frame::default();

    // Buttons
    let mut button_row = group::Flex::default().row();
    col.fixed(&button_row, 40);
    button_row.set_spacing(15);

    // Left spacer
    frame::Frame::default();

    let mut skip_btn = controls::button(&t!("update-skip"));
    button_row.fixed(&skip_btn, 120);

    let remote = remote_version.to_string();
    let mut update_btn = controls::accent_button(&t!("update-now"), theme::ACCENT);
    button_row.fixed(&update_btn, 120);

    // Right spacer
    frame::Frame::default();

    button_row.end();
    col.end();
    wind.end();
    wind.show();

    // ── Callbacks ────────────────────────────────────────────────────

    let s = sender;
    skip_btn.set_callback(move |_| {
        s.send(UiMessage::SkipUpdate);
    });

    let s = sender;
    update_btn.set_callback(move |_| {
        s.send(UiMessage::StartDownload(remote.clone()));
    });

    // Window close = skip
    let s = sender;
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            s.send(UiMessage::SkipUpdate);
            w.hide();
        }
    });

    wind
}

// ── Version check logic ─────────────────────────────────────────────────

/// Spawn a short-lived thread that checks the remote version.
fn check_version_in_background(sender: app::Sender<UiMessage>) {
    std::thread::Builder::new()
        .name("version-check".into())
        .spawn(move || {
            let result = check_version();
            sender.send(UiMessage::VersionCheckComplete(result));
            fltk::app::awake();
        })
        .ok();
}

/// Blocking version check — runs on the spawned thread.
fn check_version() -> VersionCheckResult {
    info!("Checking for updates at {VERSION_URL}");

    let remote = match fetch_remote_version() {
        Ok(v) => v,
        Err(e) => {
            warn!("Update check failed: {e}");
            return VersionCheckResult::UpToDate;
        }
    };

    if is_newer(&remote, CURRENT_VERSION) {
        info!("Update available: {CURRENT_VERSION} → {remote}");
        VersionCheckResult::UpdateAvailable {
            remote_version: remote,
        }
    } else {
        info!("Client is up to date ({CURRENT_VERSION})");
        VersionCheckResult::UpToDate
    }
}

/// GET the version string from the remote endpoint.
fn fetch_remote_version() -> Result<String, String> {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .build()
        .into();

    let mut resp = agent
        .get(VERSION_URL)
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let version = body.trim().to_string();
    if version.is_empty() {
        return Err("Empty version response".into());
    }

    Ok(version)
}

/// Simple semver comparison: true if `remote` > `local`.
///
/// Expects "major.minor.patch" format. Strips leading 'v' if present.
/// Falls back to string inequality if parsing fails.
fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let v = v.strip_prefix('v').unwrap_or(v);
        let mut parts = v.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some((major, minor, patch))
    };

    match (parse(remote), parse(local)) {
        (Some(r), Some(l)) => r > l,
        // Fail closed: if either side doesn't parse as semver, don't
        // prompt. The old `remote != local` fallback meant any garbage
        // /version response triggered an "update available" dialog.
        _ => false,
    }
}
