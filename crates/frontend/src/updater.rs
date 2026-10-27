// updater.rs — auto-update: version check at startup, zip download
// with live progress, extract + self-replace + relaunch.
//
// Ported from the FLTK `ui/splash.rs` + `ui/update.rs`; URL building
// and version comparison live in app-core, the side effects stay here.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use app_core::update::{
    app_bundle_download_url, download_url, is_newer, PLATFORM_SUFFIX, VERSION_URL,
};
use i18n::t;
use log::{error, info, warn};
use slint::ComponentHandle;
use ui::State;

use crate::{
    app::{self, CURRENT_VERSION},
    bundle,
};

/// Executable file stems inside the update zip. The release archives
/// use the target's native naming, so the real filename is the stem
/// plus [`std::env::consts::EXE_SUFFIX`] — `baucord.exe` on Windows,
/// `baucord` on Linux and macOS (see [`exe_name`]).
const MAIN_EXE_STEM: &str = "baucord";
const COMPANION_EXE_STEM: &str = "dedicated-server";

/// Platform-native executable filename for a stem.
fn exe_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

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

    // Progress poll — mirrors the shared atomic into the progress bar,
    // and stops itself when the phase leaves Downloading: done, failed
    // or cancelled. (It ran five times a second for good after any
    // download — review P2-38.)
    {
        let progress = progress.clone();
        app.update_progress_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let frac = f32::from_bits(progress.load(Ordering::Relaxed));
                app::with_current(move |app| {
                    let s = app.main.global::<State>();
                    if s.get_phase() != ui::Phase::Downloading {
                        app.update_progress_timer.stop();
                        return;
                    }
                    s.set_update_progress(frac);
                    s.set_update_progress_text(
                        t!("update-progress",
                            "percent" => format!("{:.0}", f64::from(frac) * 100.0))
                        .into(),
                    );
                });
            },
        );
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
    // A copy installed as baucord.app downloads the whole bundle into a
    // work folder beside it; a bare binary, its platform's zip beside
    // itself.
    let (url, dir) = if let Some(app) = installed_bundle() {
        let work = update_dir(&app);
        std::fs::create_dir_all(&work)
            .map_err(|e| format!("Failed to create {}: {e}", work.display()))?;
        (app_bundle_download_url(version), work)
    } else {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        (download_url(version), exe_dir)
    };

    let filename = url.rsplit('/').next().unwrap_or("baucord-update.zip");
    let dest = dir.join(filename);

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

    // Written as `.part` and renamed when whole: a download that fails
    // or is cancelled leaves nothing behind, and a zip under the real
    // name is always a whole one (review 2026-10, P3-11).
    let part = dest.with_extension("zip.part");
    let mut file = std::fs::File::create(&part)
        .map_err(|e| format!("Failed to create {}: {e}", part.display()))?;

    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 16384];
    let mut downloaded: u64 = 0;

    let copied = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Err("Download cancelled".to_string());
        }
        let n = match std::io::Read::read(&mut reader, &mut buf) {
            Ok(n) => n,
            Err(e) => break Err(format!("Read error: {e}")),
        };
        if n == 0 {
            break Ok(());
        }
        if let Err(e) = std::io::Write::write_all(&mut file, &buf[..n]) {
            break Err(format!("Write error: {e}"));
        }
        downloaded += n as u64;
        if let Some(total) = content_length {
            #[allow(clippy::cast_precision_loss)]
            let frac = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
            progress.store(frac.to_bits(), Ordering::Relaxed);
        }
    };
    drop(file);
    if let Err(e) = copied.and_then(|()| {
        std::fs::rename(&part, &dest).map_err(|e| format!("Failed to keep the download: {e}"))
    }) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    progress.store(1.0f32.to_bits(), Ordering::Relaxed);
    info!("Download complete: {}", dest.display());

    Ok(dest)
}

// ── Install ─────────────────────────────────────────────────────────────

/// The `baucord.app` this copy runs from, on macOS; `None` for a bare
/// binary and on every other platform.
fn installed_bundle() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    bundle::bundle_root(&exe)
}

/// Where a bundle update is downloaded, unpacked and swapped: a hidden
/// folder beside the bundle, on its volume, so the swap is two renames.
fn update_dir(app: &Path) -> PathBuf {
    app.parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join(".baucord-update")
}

/// Replace the running `baucord.app` with the one in the zip, whole
/// (the bundle is signed as one piece — `bundle`), and relaunch it.
/// On any failure the installed app stays as it was.
fn install_bundle(zip_path: &Path, app: &Path) -> Result<(), String> {
    let work = update_dir(app);
    let unpacked = work.join("new");
    let aside = work.join("old.app");
    let _ = std::fs::remove_dir_all(&unpacked);

    let swapped = (|| {
        bundle::extract_tree(zip_path, &unpacked)?;
        let fresh = unpacked.join(bundle::BUNDLE_NAME);
        let new_exe = fresh
            .join("Contents")
            .join("MacOS")
            .join(exe_name(MAIN_EXE_STEM));
        if !new_exe.is_file() {
            return Err(format!(
                "{}/Contents/MacOS/{} not found in the update zip",
                bundle::BUNDLE_NAME,
                exe_name(MAIN_EXE_STEM)
            ));
        }
        ensure_executable(&new_exe)?;
        bundle::swap_bundle(app, &fresh, &aside)
    })();
    // The zip, the unpacked copy and the old bundle all live in `work`.
    let _ = std::fs::remove_dir_all(&work);
    swapped?;

    info!("Replaced {} with the new version", app.display());
    relaunch(
        &app.join("Contents")
            .join("MacOS")
            .join(exe_name(MAIN_EXE_STEM)),
    )
}

/// Extract the downloaded zip, replace both executables, and relaunch.
///
/// Runs on a thread of its own (`AppState::finish_leaving`): extracting
/// the zip took the window with it for seconds (review 2026-10, P3-11).
/// Ends in `process::exit`; returns only on an error.
///
/// A copy installed as `baucord.app` (macOS) replaces its bundle whole
/// instead ([`install_bundle`]). A bare binary's steps:
/// 1. Extract root-level zip entries to a staging dir next to the exe.
/// 2. Make sure both staged binaries carry the exec bit (Unix).
/// 3. `self_replace` the running binary.
/// 4. `fs::copy` the companion binary.
/// 5. Clean up the zip and staging dir.
/// 6. Spawn the new executable and exit.
pub fn extract_and_install(zip_path: &str) -> Result<(), String> {
    let zip_path = PathBuf::from(zip_path);

    // macOS, installed as baucord.app: the bundle is replaced whole.
    if let Some(app) = installed_bundle() {
        return install_bundle(&zip_path, &app);
    }

    // Resolve our own path ONCE, before the swap. On Linux
    // `current_exe()` reads `/proc/self/exe`, which turns into
    // `…/baucord (deleted)` the moment self_replace renames the new
    // file over the running inode — relaunching from a second lookup
    // would spawn a path that no longer exists.
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Failed to locate current exe: {e}"))?;
    let exe_dir = current_exe
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    let extract_dir = exe_dir.join(".baucord-update-staging");

    // Clean up any previous failed staging
    if extract_dir.exists() {
        let _ = std::fs::remove_dir_all(&extract_dir);
    }

    let extracted = extract_zip(&zip_path, &extract_dir)?;

    // Locate the extracted main binary
    let main_exe = exe_name(MAIN_EXE_STEM);
    let new_main_exe = extract_dir.join(&main_exe);
    if !new_main_exe.exists() {
        cleanup_staging(&zip_path, &extract_dir);
        let listing = if extracted.is_empty() {
            "none".to_string()
        } else {
            extracted.join(", ")
        };
        return Err(format!(
            "{main_exe} not found in update zip (root-level entries: {listing})"
        ));
    }

    // Zips built on Windows carry no Unix mode bits, so the staged
    // files may be 0644. self_replace re-applies the running binary's
    // permissions itself, but the companion is a plain fs::copy — and
    // fs::copy carries the *source* mode — so fix both up front.
    ensure_executable(&new_main_exe)?;

    // Replace the running executable via self_replace
    self_replace::self_replace(&new_main_exe)
        .map_err(|e| format!("Failed to replace {main_exe}: {e}"))?;

    info!("Replaced {main_exe} via self_replace");

    // Copy the companion executable (not locked — plain overwrite)
    let companion_exe = exe_name(COMPANION_EXE_STEM);
    let new_companion = extract_dir.join(&companion_exe);
    if new_companion.exists() {
        ensure_executable(&new_companion)?;
        let dest_companion = exe_dir.join(&companion_exe);
        std::fs::copy(&new_companion, &dest_companion)
            .map_err(|e| format!("Failed to copy {companion_exe}: {e}"))?;
        info!("Copied {companion_exe}");
    }

    cleanup_staging(&zip_path, &extract_dir);

    relaunch(&current_exe)
}

/// Start `exe` and exit this process; returns only if the start failed.
fn relaunch(exe: &Path) -> Result<(), String> {
    info!("Relaunching {}", exe.display());

    // spawn() returning Ok means the OS has created the process — the
    // child runs the freshly-replaced binary from the same path and
    // does not need this parent alive. Exit immediately.
    //
    // The child must not mistake this exiting process for a running
    // instance: until our exit completes, the single-instance socket
    // still answers, and a child that knocked on it was told "already
    // running" and quit — the app closed and never came back (1.2.0).
    // The variable tells it to wait for us to go first (`instance`).
    // The same arguments carry over, so a `--profile` run comes back
    // on its own profile.
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(crate::instance::RELAUNCH_ENV, "1")
        .spawn()
        .map_err(|e| format!("Failed to relaunch: {e}"))?;

    std::process::exit(0);
}

/// Extract a zip file's root-level files into the given directory,
/// preserving Unix mode bits where the archive recorded them. Returns
/// the extracted filenames (for the "not found" diagnostic).
fn extract_zip(zip_path: &Path, dest_dir: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("Failed to open {}: {e}", zip_path.display()))?;

    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Failed to read zip: {e}"))?;

    std::fs::create_dir_all(dest_dir).map_err(|e| format!("Failed to create staging dir: {e}"))?;

    let mut extracted = Vec::new();

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

        apply_unix_mode(&out_path, entry.unix_mode())?;

        info!("Extracted: {}", out_path.display());
        extracted.push(name.to_string_lossy().into_owned());
    }

    Ok(extracted)
}

/// Apply the mode bits a zip entry recorded, if any (archives built on
/// Windows have none).
#[cfg(unix)]
fn apply_unix_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else {
        return Ok(());
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|e| format!("Failed to set mode on {}: {e}", path.display()))
}

// The Unix version can fail; the callers' `?` needs one signature.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps)]
fn apply_unix_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

/// Make a staged binary runnable regardless of what the archive said:
/// owner rwx, group/other rx, on top of whatever is already set.
#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| format!("Failed to stat {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
        .map_err(|e| format!("Failed to make {} executable: {e}", path.display()))
}

// Same signature as the Unix version, for the callers' `?`.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps)]
fn ensure_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Best-effort cleanup of the zip and staging directory.
fn cleanup_staging(zip_path: &Path, extract_dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(extract_dir) {
        error!("Failed to clean staging dir: {e}");
    }
    if let Err(e) = std::fs::remove_file(zip_path) {
        error!("Failed to clean zip file: {e}");
    }
}
