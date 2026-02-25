// ui/update.rs — Download window with progress meter.
//
// Downloads the platform zip to the directory next to the executable.
// Progress is driven by an AtomicU32 (bit-cast f32, 0.0–1.0) — the
// same pattern used by the audio VU meters. A timer redraws the meter
// at 8 Hz.
//
// Cancellation uses an AtomicBool shared with the download thread.
// On completion, "Open File" opens the zip and shuts down the app,
// "Not Now" dismisses and continues to the connect screen.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use fltk::{prelude::*, *};
use log::info;

use crate::ui::messages::UiMessage;
use crate::ui::splash;
use ui::theme;
use ui::widgets::controls;

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
        .with_label("Updating");
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(8);

    let mut title = frame::Frame::default()
        .with_label(&format!("Downloading update v{remote_version}..."))
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
        .with_label("Starting download...")
        .with_align(enums::Align::Center);
    status_lbl.set_label_size(11);
    status_lbl.set_label_color(theme::TEXT_MUTED);
    col.fixed(&status_lbl, 18);

    // Spacer
    frame::Frame::default();

    // Button row — starts with Cancel, changes to Open File / Not Now
    let mut button_row = group::Flex::default().row();
    col.fixed(&button_row, 40);
    button_row.set_spacing(15);

    frame::Frame::default(); // left spacer

    let mut cancel_btn = controls::button("Cancel");
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
    let s = sender;
    cancel_btn.set_callback(move |_| {
        cancelled_cb.store(true, Ordering::Relaxed);
        s.send(UiMessage::DownloadCancelled);
    });

    // Window close = cancel
    let cancelled_close = cancelled.clone();
    let s = sender;
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            cancelled_close.store(true, Ordering::Relaxed);
            s.send(UiMessage::DownloadCancelled);
            w.hide();
        }
    });

    // ── Timer: 8 Hz meter refresh ────────────────────────────────────

    let mut meter_timer = meter.clone();
    let progress_timer = progress.clone();
    let mut status_timer = status_lbl.clone();

    app::add_timeout3(0.125, move |handle| {
        let val = f32::from_bits(progress_timer.load(Ordering::Relaxed)).clamp(0.0, 1.0);
        let pct = (val * 100.0) as u32;
        status_timer.set_label(&format!("Downloading... {pct}%"));
        meter_timer.redraw();
        app::repeat_timeout3(0.125, handle);
    });

    // ── Spawn download thread ────────────────────────────────────────

    let progress_dl = progress.clone();
    let cancelled_dl = cancelled.clone();
    let s = sender;
    let version = remote_version.to_string();

    std::thread::Builder::new()
        .name("download-update".into())
        .spawn(move || {
            let result = download_zip(&version, &progress_dl, &cancelled_dl);
            match result {
                Ok(path) => {
                    // Set to 100% before sending completion
                    progress_dl.store(1.0f32.to_bits(), Ordering::Relaxed);
                    s.send(UiMessage::DownloadComplete(
                        path.to_string_lossy().to_string(),
                    ));
                }
                Err(e) => {
                    if !cancelled_dl.load(Ordering::Relaxed) {
                        s.send(UiMessage::DownloadFailed(e));
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
/// Open File / Not Now buttons. Returns the new window handle.
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
        .with_label("Update Ready");
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(10);

    let mut title = frame::Frame::default()
        .with_label("Update downloaded!")
        .with_align(enums::Align::Center);
    title.set_label_size(18);
    title.set_label_color(theme::SUCCESS);
    col.fixed(&title, 30);

    let mut info = frame::Frame::default()
        .with_label("Close and extract the zip to update.")
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

    let mut not_now_btn = controls::button("Not Now");
    button_row.fixed(&not_now_btn, 120);

    let mut open_btn = controls::accent_button("Open File", theme::SUCCESS);
    button_row.fixed(&open_btn, 120);

    frame::Frame::default();

    button_row.end();
    col.end();
    wind.end();
    wind.show();

    // ── Callbacks ────────────────────────────────────────────────────

    let s = sender;
    not_now_btn.set_callback(move |_| {
        s.send(UiMessage::SkipUpdate);
    });

    let s = sender;
    let path_owned = path.to_string();
    open_btn.set_callback(move |_| {
        s.send(UiMessage::OpenUpdateAndQuit(path_owned.clone()));
    });

    let s = sender;
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            s.send(UiMessage::SkipUpdate);
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
        .with_label("Update Failed");
    wind.set_pos(((screen_w as i32) - w) / 2, ((screen_h as i32) - h) / 2);
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    crate::ui::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(25, 20, 25, 20);
    col.set_spacing(10);

    let mut title = frame::Frame::default()
        .with_label("Download failed")
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

    let mut continue_btn = controls::accent_button("Continue", theme::ACCENT);
    button_row.fixed(&continue_btn, 140);

    frame::Frame::default();

    button_row.end();
    col.end();
    wind.end();
    wind.show();

    let s = sender;
    continue_btn.set_callback(move |_| {
        s.send(UiMessage::SkipUpdate);
    });

    let s = sender;
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            s.send(UiMessage::SkipUpdate);
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
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let filename = format!("baucord-v{version}.zip");
    let dest = exe_dir.join(&filename);

    info!("Downloading {} → {}", splash::DOWNLOAD_URL, dest.display());

    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();

    let mut resp = agent
        .get(splash::DOWNLOAD_URL)
        .call()
        .map_err(|e| format!("Download request failed: {e}"))?;

    // Try to get content length for progress calculation
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
            // Clean up partial file
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
