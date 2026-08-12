// ui/update.rs — Download window with progress meter + in-place binary update.
//
// Downloads the platform zip to a temp location next to the executable,
// extracts it, replaces the running binary via `self_replace`, copies
// the companion executable, then relaunches.
//
// Progress is driven by an AtomicU32 (bit-cast f32, 0.0–1.0) — the
// same pattern used by the audio VU meters. A timer redraws the meter
// at 8 Hz.
//
// Cancellation uses an AtomicBool shared with the download thread.
// On completion, "Install & Restart" performs the replacement and
// relaunches; "Not Now" dismisses and continues to the connect screen.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use fltk::{prelude::*, *};
use log::{error, info};

use crate::ui::messages::UiMessage;
use crate::ui::splash;
use i18n::t;
use ui::theme;
use ui::widgets::controls;

/// The main executable filename inside the update zip.
const MAIN_EXE: &str = "baucord.exe";

/// The companion executable filename inside the update zip.
const COMPANION_EXE: &str = "dedicated-server.exe";

/// Show the download window and start downloading.
///
/// Returns the window handle so App can track it for later
/// state transitions (complete, error).
pub fn show(sender: app::Sender<UiMessage>, remote_version: &str) -> window::Window {
    let (screen_w, screen_h) = app::screen_size();
    let w = 450;
    let h = 250;

    let mut wind = window::Window::default()
        .with_size(w, h)
        .with_label(&t!("update-downloading-title"));
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(8);

    let mut title = frame::Frame::default()
        .with_label(&t!("update-downloading-heading", "version" => remote_version))
        .with_align(enums::Align::Center);
    title.set_label_size(16);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 28);

    // Progress meter — same AtomicU32 pattern as audio meters.
    let progress = Arc::new(AtomicU32::new(0f32.to_bits()));
    let progress_draw = progress.clone();

    let mut meter = frame::Frame::default();
    meter.set_frame(enums::FrameType::FlatBox);
    meter.set_color(theme::SURFACE);
    col.fixed(&meter, 24);

    meter.draw(move |frm| {
        // Background
        draw::set_draw_color(theme::SURFACE);
        draw::draw_rectf(frm.x(), frm.y(), frm.w(), frm.h());

        // Filled bar
        let val = f32::from_bits(progress_draw.load(Ordering::Relaxed)).clamp(0.0, 1.0);
        let bar_w = frm.w() - 4;
        let filled = (bar_w as f32 * val) as i32;

        if filled > 0 {
            let color = if val >= 1.0 {
                theme::SUCCESS
            } else {
                theme::ACCENT
            };
            draw::set_draw_color(color);
            draw::draw_rectf(frm.x() + 2, frm.y() + 3, filled, frm.h() - 6);
        }

        // Border
        draw::set_draw_color(theme::BORDER);
        draw::draw_rect(frm.x(), frm.y(), frm.w(), frm.h());
    });

    // Status label — shows percentage or result
    let mut status_lbl = frame::Frame::default()
        .with_label(&t!("update-starting-download"))
        .with_align(enums::Align::Center);
    status_lbl.set_label_size(11);
    status_lbl.set_label_color(theme::TEXT_MUTED);
    col.fixed(&status_lbl, 18);

    // Spacer
    frame::Frame::default();

    // Button row — starts with Cancel, changes on completion
    let mut button_row = group::Flex::default().row();
    col.fixed(&button_row, 40);
    button_row.set_spacing(15);

    frame::Frame::default(); // left spacer

    let mut cancel_btn = controls::button(&t!("cancel"));
    button_row.fixed(&cancel_btn, 140);

    frame::Frame::default(); // right spacer

    button_row.end();
    col.end();
    wind.end();
    wind.show();

    // ── Shared state ─────────────────────────────────────────────────

    let cancelled = Arc::new(AtomicBool::new(false));

    // ── Cancel callback ──────────────────────────────────────────────

    let cancelled_cb = cancelled.clone();
    cancel_btn.set_callback(move |_| {
        cancelled_cb.store(true, Ordering::Relaxed);
        sender.send(UiMessage::DownloadCancelled);
    });

    // Window close = cancel
    let cancelled_close = cancelled.clone();
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            cancelled_close.store(true, Ordering::Relaxed);
            sender.send(UiMessage::DownloadCancelled);
            w.hide();
        }
    });

    // ── Timer: 8 Hz meter refresh ────────────────────────────────────

    let mut meter_timer = meter.clone();
    let progress_timer = progress.clone();
    let mut status_timer = status_lbl.clone();
    let alive_probe = meter.clone();

    app::add_timeout3(0.125, move |handle| {
        // Stop once the download window is hidden (complete / error /
        // cancel) — otherwise this timer would repeat forever.
        if !alive_probe.window().map(|w| w.visible()).unwrap_or(false) {
            return;
        }
        let val = f32::from_bits(progress_timer.load(Ordering::Relaxed)).clamp(0.0, 1.0);
        let pct = (val * 100.0) as u32;
        status_timer.set_label(&t!("update-progress", "percent" => pct));
        meter_timer.redraw();
        app::repeat_timeout3(0.125, handle);
    });

    // ── Spawn download thread ────────────────────────────────────────

    let progress_dl = progress.clone();
    let cancelled_dl = cancelled.clone();
    let version = remote_version.to_string();

    std::thread::Builder::new()
        .name("download-update".into())
        .spawn(move || {
            let result = download_zip(&version, &progress_dl, &cancelled_dl);
            match result {
                Ok(path) => {
                    progress_dl.store(1.0f32.to_bits(), Ordering::Relaxed);
                    sender.send(UiMessage::DownloadComplete(
                        path.to_string_lossy().to_string(),
                    ));
                }
                Err(e) => {
                    if !cancelled_dl.load(Ordering::Relaxed) {
                        sender.send(UiMessage::DownloadFailed(e));
                    }
                }
            }
            fltk::app::awake();
        })
        .ok();

    wind
}

/// Show the "download complete" state as a new window.
///
/// Hides the download window and creates a fresh one with
/// Install & Restart / Not Now buttons. Returns the new window handle.
pub fn show_complete(
    sender: app::Sender<UiMessage>,
    path: &str,
    download_wind: &mut window::Window,
) -> window::Window {
    download_wind.hide();

    let (screen_w, screen_h) = app::screen_size();
    let w = 450;
    let h = 250;

    let mut wind = window::Window::default()
        .with_size(w, h)
        .with_label(&t!("update-ready-title"));
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(10);

    let mut title = frame::Frame::default()
        .with_label(&t!("update-downloaded-heading"))
        .with_align(enums::Align::Center);
    title.set_label_size(18);
    title.set_label_color(theme::SUCCESS);
    col.fixed(&title, 30);

    let mut info = frame::Frame::default()
        .with_label(&t!("update-ready-info"))
        .with_align(enums::Align::Center | enums::Align::Inside);
    info.set_label_size(12);
    info.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&info, 20);

    let mut path_lbl = frame::Frame::default()
        .with_label(path)
        .with_align(enums::Align::Center | enums::Align::Inside);
    path_lbl.set_label_size(10);
    path_lbl.set_label_color(theme::TEXT_MUTED);
    col.fixed(&path_lbl, 16);

    // Spacer
    frame::Frame::default();

    // Buttons
    let mut button_row = group::Flex::default().row();
    col.fixed(&button_row, 40);
    button_row.set_spacing(15);

    frame::Frame::default();

    let mut not_now_btn = controls::button(&t!("update-not-now"));
    button_row.fixed(&not_now_btn, 120);

    // FLTK treats '&' as a shortcut marker — escape it so translated
    // labels containing '&' render literally.
    let install_label = t!("update-install-restart").replace('&', "&&");
    let mut install_btn = controls::accent_button(&install_label, theme::SUCCESS);
    button_row.fixed(&install_btn, 160);

    frame::Frame::default();

    button_row.end();
    col.end();
    wind.end();
    wind.show();

    // ── Callbacks ────────────────────────────────────────────────────

    // Skipping the install would otherwise leave the downloaded zip
    // next to the exe forever — one per skipped version — and the next
    // prompt re-downloads from scratch anyway. Best-effort cleanup on
    // both skip paths (button and window close).
    let zip_for_skip = path.to_string();
    not_now_btn.set_callback(move |_| {
        let _ = std::fs::remove_file(&zip_for_skip);
        sender.send(UiMessage::SkipUpdate);
    });

    let path_owned = path.to_string();
    install_btn.set_callback(move |_| {
        sender.send(UiMessage::InstallUpdate(path_owned.clone()));
    });

    let zip_for_close = path.to_string();
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            let _ = std::fs::remove_file(&zip_for_close);
            sender.send(UiMessage::SkipUpdate);
            w.hide();
        }
    });

    wind
}

/// Show a download error as a new window with a "Continue" button.
pub fn show_error(
    sender: app::Sender<UiMessage>,
    error: &str,
    download_wind: &mut window::Window,
) -> window::Window {
    download_wind.hide();

    let (screen_w, screen_h) = app::screen_size();
    let w = 450;
    let h = 200;

    let mut wind = window::Window::default()
        .with_size(w, h)
        .with_label(&t!("update-failed-title"));
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(10);

    let mut title = frame::Frame::default()
        .with_label(&t!("update-failed-heading"))
        .with_align(enums::Align::Center);
    title.set_label_size(16);
    title.set_label_color(theme::DANGER);
    col.fixed(&title, 28);

    let mut err_lbl = frame::Frame::default()
        .with_label(error)
        .with_align(enums::Align::Center | enums::Align::Inside);
    err_lbl.set_label_size(11);
    err_lbl.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&err_lbl, 30);

    // Spacer
    frame::Frame::default();

    let mut button_row = group::Flex::default().row();
    col.fixed(&button_row, 40);
    button_row.set_spacing(15);

    frame::Frame::default();

    let mut continue_btn = controls::accent_button(&t!("update-continue"), theme::ACCENT);
    button_row.fixed(&continue_btn, 140);

    frame::Frame::default();

    button_row.end();
    col.end();
    wind.end();
    wind.show();

    continue_btn.set_callback(move |_| {
        sender.send(UiMessage::SkipUpdate);
    });

    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            sender.send(UiMessage::SkipUpdate);
            w.hide();
        }
    });

    wind
}

// ── Download logic ──────────────────────────────────────────────────────

/// Download the platform zip next to the running executable.
///
/// Updates `progress` (0.0–1.0 as bit-cast f32 in AtomicU32) as
/// data arrives. Checks `cancelled` between chunks and bails early
/// if set.
fn download_zip(
    version: &str,
    progress: &AtomicU32,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let url = splash::download_url(version);
    let filename = url.rsplit('/').next().unwrap_or("baucord-update.zip");
    let dest = exe_dir.join(filename);

    info!("Downloading {url} → {}", dest.display());

    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
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

        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("Read error: {e}"))?;

        if n == 0 {
            break;
        }

        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|e| format!("Write error: {e}"))?;

        downloaded += n as u64;

        if let Some(total) = content_length {
            let frac = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
            progress.store(frac.to_bits(), Ordering::Relaxed);
        }
    }

    progress.store(1.0f32.to_bits(), Ordering::Relaxed);
    info!("Download complete: {}", dest.display());

    Ok(dest)
}

// ── Install logic ───────────────────────────────────────────────────────

/// Extract the downloaded zip, replace both executables, and relaunch.
///
/// This is called from the main thread when the user clicks
/// "Install & Restart". It runs synchronously — the UI will briefly
/// freeze, but the operation is fast (local file I/O only).
///
/// Steps:
/// 1. Extract zip contents to a temp directory next to the exe.
/// 2. `self_replace` the running binary (`baucord.exe`).
/// 3. `fs::copy` the companion binary (`dedicated-server.exe`).
/// 4. Clean up the zip and temp directory.
/// 5. Spawn the new executable and exit.
pub fn extract_and_install(zip_path: &str) -> Result<(), String> {
    let zip_path = PathBuf::from(zip_path);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
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

    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Failed to read zip: {e}"))?;

    std::fs::create_dir_all(dest_dir).map_err(|e| format!("Failed to create staging dir: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read zip entry {i}: {e}"))?;

        // Only extract files at the root level — skip directories and nested paths
        let Some(name) = entry.enclosed_name().and_then(|p| {
            if p.components().count() == 1 {
                p.file_name().map(|n| n.to_os_string())
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
