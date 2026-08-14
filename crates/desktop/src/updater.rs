// updater.rs — auto-update: version check at startup, zip download
// with live progress, extract + self-replace + relaunch.
//
// Ported from the FLTK `ui/splash.rs` + `ui/update.rs`; URL building
// and version comparison live in app-core, the side effects stay here.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use log::{error, info, warn};
use slint::ComponentHandle;

use app_core::update::{PLATFORM_SUFFIX, VERSION_URL, download_url, is_newer};
use i18n::t;
use ui::State;

use crate::app::{self, CURRENT_VERSION};

/// The main executable filename inside the update zip.
const MAIN_EXE: &str = "baucord.exe";

/// The companion executable filename inside the update zip.
const COMPANION_EXE: &str = "dedicated-server.exe";

// ── Version check (startup) ─────────────────────────────────────────────

/// Check for updates off-thread; route the verdict back to the UI.
/// The splash phase stays up until this resolves.
pub fn spawn_version_check() {
    std::thread::spawn(|| {
        let result = check_for_update();
        let _ = slint::invoke_from_event_loop(move || {
            app::with_current(move |app| match result {
                Some(remote_version) => app.set_update_available(remote_version),
                None => app.show_connect_phase(),
            });
        });
    });
}

/// `Some(version)` if a newer release exists for this platform.
fn check_for_update() -> Option<String> {
    // No release archive exists for this target — never prompt.
    if PLATFORM_SUFFIX.is_empty() {
        info!("No update channel for this platform — skipping check");
        return None;
    }

    info!("Checking for updates at {VERSION_URL}");

    let remote = match fetch_remote_version() {
        Ok(v) => v,
        Err(e) => {
            warn!("Update check failed: {e}");
            return None;
        }
    };

    if is_newer(&remote, CURRENT_VERSION) {
        info!("Update available: {CURRENT_VERSION} → {remote}");
        Some(remote)
    } else {
        info!("Client is up to date ({CURRENT_VERSION})");
        None
    }
}

/// GET the version string from the remote endpoint.
fn fetch_remote_version() -> Result<String, String> {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(Duration::from_secs(5)))
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

// ── Download ────────────────────────────────────────────────────────────

/// Kick off the download: flips the UI to the progress phase, spawns
/// the worker, and starts a 200 ms poll of the shared progress value.
pub fn start_download(app: &mut crate::app::AppState) {
    let Some(version) = app.update_version().map(str::to_string) else {
        return;
    };

    let s = app.main.global::<State>();
    s.set_update_heading(t!("update-downloading-heading", "version" => version.as_str()).into());
    s.set_update_progress(0.0);
    s.set_update_progress_text("".into());
    s.set_phase(ui::Phase::Downloading);

    let cancelled = app.download_cancel_flag();
    cancelled.store(false, Ordering::Relaxed);
    let progress = Arc::new(AtomicU32::new(0));

    // Progress poll — mirrors the shared atomic into the progress bar.
    // Runs until the phase leaves Downloading.
    {
        let progress = progress.clone();
        let timer = slint::Timer::default();
        let timer_keeper = timer; // moved into AppState.timers below
        timer_keeper.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let frac = f32::from_bits(progress.load(Ordering::Relaxed));
                app::with_current(move |app| {
                    let s = app.main.global::<State>();
                    if s.get_phase() == ui::Phase::Downloading {
                        s.set_update_progress(frac);
                        s.set_update_progress_text(
                            t!("update-progress",
                                "percent" => format!("{:.0}", f64::from(frac) * 100.0))
                            .into(),
                        );
                    }
                });
            },
        );
        app.hold_timer(timer_keeper);
    }

    let worker_progress = progress;
    let worker_cancelled = cancelled;
    std::thread::spawn(move || {
        let result = download_zip(&version, &worker_progress, &worker_cancelled);
        let was_cancelled = worker_cancelled.load(Ordering::Relaxed);
        let _ = slint::invoke_from_event_loop(move || {
            app::with_current(move |app| match result {
                Ok(path) => app.set_download_done(path.display().to_string()),
                Err(_) if was_cancelled => {
                    // User cancelled — updater already returned them to
                    // the connect view; nothing to do.
                }
                Err(e) => app.set_download_failed(&e),
            });
        });
    });
}

fn download_zip(
    version: &str,
    progress: &AtomicU32,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let url = download_url(version);
    let filename = url.rsplit('/').next().unwrap_or("baucord-update.zip");
    let dest = exe_dir.join(filename);

    info!("Downloading {url} → {}", dest.display());

    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();

    let mut resp = agent
        .get(&url)
        .call()
        .map_err(|e| format!("Download request failed: {e}"))?;

    let content_length: Option<u64> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    let mut file = std::fs::File::create(&dest)
        .map_err(|e| format!("Failed to create {}: {e}", dest.display()))?;

    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 16384];
    let mut downloaded: u64 = 0;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&dest);
            return Err("Download cancelled".into());
        }

        let n = std::io::Read::read(&mut reader, &mut buf)
            .map_err(|e| format!("Read error: {e}"))?;

        if n == 0 {
            break;
        }

        std::io::Write::write_all(&mut file, &buf[..n])
            .map_err(|e| format!("Write error: {e}"))?;

        downloaded += n as u64;

        if let Some(total) = content_length {
            #[allow(clippy::cast_precision_loss)]
            let frac = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
            progress.store(frac.to_bits(), Ordering::Relaxed);
        }
    }

    progress.store(1.0f32.to_bits(), Ordering::Relaxed);
    info!("Download complete: {}", dest.display());

    Ok(dest)
}

// ── Install ─────────────────────────────────────────────────────────────

/// Extract the downloaded zip, replace both executables, and relaunch.
///
/// Runs synchronously on the UI thread — it's fast (local file I/O)
/// and ends in `process::exit`.
///
/// Steps:
/// 1. Extract zip contents to a staging dir next to the exe.
/// 2. `self_replace` the running binary.
/// 3. `fs::copy` the companion binary.
/// 4. Clean up the zip and staging dir.
/// 5. Spawn the new executable and exit.
pub fn extract_and_install(zip_path: &str) -> Result<(), String> {
    let zip_path = PathBuf::from(zip_path);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let extract_dir = exe_dir.join(".baucord-update-staging");

    // Clean up any previous failed staging
    if extract_dir.exists() {
        let _ = std::fs::remove_dir_all(&extract_dir);
    }

    extract_zip(&zip_path, &extract_dir)?;

    // Locate the extracted main binary
    let new_main_exe = extract_dir.join(MAIN_EXE);
    if !new_main_exe.exists() {
        cleanup_staging(&zip_path, &extract_dir);
        return Err(format!("{MAIN_EXE} not found in update zip"));
    }

    // Replace the running executable via self_replace
    self_replace::self_replace(&new_main_exe)
        .map_err(|e| format!("Failed to replace {MAIN_EXE}: {e}"))?;

    info!("Replaced {MAIN_EXE} via self_replace");

    // Copy the companion executable (not locked — plain overwrite)
    let new_companion = extract_dir.join(COMPANION_EXE);
    if new_companion.exists() {
        let dest_companion = exe_dir.join(COMPANION_EXE);
        std::fs::copy(&new_companion, &dest_companion)
            .map_err(|e| format!("Failed to copy {COMPANION_EXE}: {e}"))?;
        info!("Copied {COMPANION_EXE}");
    }

    cleanup_staging(&zip_path, &extract_dir);

    // Spawn the new binary and exit
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Failed to locate current exe: {e}"))?;

    info!("Relaunching {}", current_exe.display());

    // spawn() returning Ok means the OS has created the process — the
    // child runs the freshly-replaced binary from the same path and
    // does not need this parent alive. Exit immediately.
    std::process::Command::new(&current_exe)
        .spawn()
        .map_err(|e| format!("Failed to relaunch: {e}"))?;

    std::process::exit(0);
}

/// Extract a zip file into the given directory.
fn extract_zip(zip_path: &PathBuf, dest_dir: &PathBuf) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("Failed to open {}: {e}", zip_path.display()))?;

    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("Failed to read zip: {e}"))?;

    std::fs::create_dir_all(dest_dir).map_err(|e| format!("Failed to create staging dir: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read zip entry {i}: {e}"))?;

        // Only extract files at the root level — skip directories and nested paths
        let Some(name) = entry.enclosed_name().and_then(|p| {
            if p.components().count() == 1 {
                p.file_name().map(std::ffi::OsStr::to_os_string)
            } else {
                None
            }
        }) else {
            continue;
        };

        if entry.is_dir() {
            continue;
        }

        let out_path = dest_dir.join(&name);
        let mut out_file = std::fs::File::create(&out_path)
            .map_err(|e| format!("Failed to create {}: {e}", out_path.display()))?;

        std::io::copy(&mut entry, &mut out_file)
            .map_err(|e| format!("Failed to extract {}: {e}", out_path.display()))?;

        info!("Extracted: {}", out_path.display());
    }

    Ok(())
}

/// Best-effort cleanup of the zip and staging directory.
fn cleanup_staging(zip_path: &PathBuf, extract_dir: &PathBuf) {
    if let Err(e) = std::fs::remove_dir_all(extract_dir) {
        error!("Failed to clean staging dir: {e}");
    }
    if let Err(e) = std::fs::remove_file(zip_path) {
        error!("Failed to clean zip file: {e}");
    }
}
