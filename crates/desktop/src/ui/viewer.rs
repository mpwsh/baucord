// ui/viewer.rs — Screen share viewer window.
//
// One window per watched peer. Receives RGBA pixel data from the screen
// thread (via ScreenEvent::FrameReady) and blits it into an FLTK image.
//
// The window is resizable and maximizable. The stream is rendered at its
// native resolution and scaled to fit the canvas by FLTK's draw pass —
// no CPU-side scaling needed. Aspect ratio is preserved with letterboxing.
//
// A single-row toolbar at the bottom provides:
//   [ ----o--- volume | Mute | Stop Watching |       720p 24fps 12KB ]
//
// Closing the window sends an unsubscribe message so the server stops
// forwarding frames.

use fltk::{enums, image::RgbImage, prelude::*, *};
use std::cell::RefCell;
use std::rc::Rc;

use crate::ui::messages::UiMessage;
use i18n::t;
use ui::fps::FpsCounter;
use ui::theme;
use ui::widgets::controls;

/// Height of the bottom toolbar in pixels.
const TOOLBAR_HEIGHT: i32 = 32;

/// Shared state between the struct and the draw callback.
struct ViewerPixels {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

/// State for a single viewer window.
pub struct ViewerWindow {
    pub width: u32,
    pub height: u32,
    /// Peer's display name — kept here so title updates don't have to
    /// parse it back out of the window label.
    username: String,
    wind: window::Window,
    canvas: frame::Frame,
    /// Shared pixel buffer — written by update_frame(), read by the draw callback.
    pixels: Rc<RefCell<ViewerPixels>>,
    /// Frame counter for stats.
    frame_count: u64,
    /// FPS counter — updated on each frame, displayed in toolbar.
    fps: FpsCounter,
    /// Stats label in the toolbar (right-aligned).
    stats_label: frame::Frame,
    /// Total bytes received (encoded) across all frames.
    total_bytes: u64,
    /// Ring buffer of recent frame sizes for smoothed throughput display.
    recent_frame_bytes: [u64; 10],
    /// Write index into the ring buffer.
    ring_idx: usize,
}

impl ViewerWindow {
    /// Create and show a viewer window for a remote peer's stream.
    ///
    /// `username` is displayed in the title bar.
    /// The window starts at the stream's native resolution (capped to
    /// 90% of screen size so it always fits). Resizable and maximizable.
    pub fn new(
        client_id: u64,
        username: &str,
        width: u32,
        height: u32,
        sender: app::Sender<UiMessage>,
    ) -> Self {
        // Cap initial window size to 90% of screen
        let (screen_w, screen_h) = app::screen_size();
        let max_w = (screen_w * 0.9) as u32;
        let max_h = (screen_h * 0.9) as u32;

        let win_w = width.min(max_w) as i32;
        // Extra height for toolbar
        let win_h = height.min(max_h) as i32 + TOOLBAR_HEIGHT;

        let title = t!("viewer-title", "name" => username, "width" => width, "height" => height);

        let mut wind = window::Window::default()
            .with_size(win_w, win_h)
            .with_label(&title);
        wind.make_resizable(true);
        wind.size_range(320, 240 + TOOLBAR_HEIGHT, 0, 0);
        wind.set_color(enums::Color::Black);
        crate::ui::set_window_icon(&mut wind);

        // Center on screen
        let cx = ((screen_w as i32) - win_w) / 2;
        let cy = ((screen_h as i32) - win_h) / 2;
        wind.set_pos(cx, cy);

        // ── Layout: column with canvas (resizable) + toolbar (fixed) ──
        let mut col = group::Flex::default()
            .with_pos(0, 0)
            .with_size(win_w, win_h)
            .column();
        col.set_spacing(0);

        // Canvas — fills all available space above the toolbar
        let mut canvas = frame::Frame::default();
        canvas.set_frame(enums::FrameType::FlatBox);
        canvas.set_color(enums::Color::Black);
        // (Flex gives remaining space to non-fixed children)

        // ── Toolbar row ──────────────────────────────────────────────
        let mut toolbar = group::Flex::default().row();
        toolbar.set_color(theme::BG_SECONDARY);
        toolbar.set_frame(enums::FrameType::FlatBox);
        toolbar.set_margins(6, 2, 6, 2);
        toolbar.set_spacing(4);
        col.fixed(&toolbar, TOOLBAR_HEIGHT);

        // Volume slider
        let mut volume_slider = valuator::HorNiceSlider::default();
        volume_slider.set_minimum(0.0);
        volume_slider.set_maximum(1.0);
        volume_slider.set_value(1.0);
        volume_slider.set_step(0.05, 1);
        volume_slider.set_color(theme::BG_TERTIARY);
        volume_slider.set_selection_color(theme::ACCENT);
        toolbar.fixed(&volume_slider, 120);

        // Separator
        let mut sep1 = frame::Frame::default();
        sep1.set_frame(enums::FrameType::FlatBox);
        sep1.set_color(theme::SUBTLE);
        toolbar.fixed(&sep1, 1);

        // Mute button — 90px (was 50) so "Desmutear" & co. fit.
        let mute_btn = controls::button(&t!("mute"));
        toolbar.fixed(&mute_btn, 90);

        // Separator
        let mut sep2 = frame::Frame::default();
        sep2.set_frame(enums::FrameType::FlatBox);
        sep2.set_color(theme::SUBTLE);
        toolbar.fixed(&sep2, 1);

        // Stop Watching button — 120px (was 90) for longer translations.
        let mut stop_btn = controls::button(&t!("stop-watching"));
        toolbar.fixed(&stop_btn, 120);

        // Separator
        let mut sep3 = frame::Frame::default();
        sep3.set_frame(enums::FrameType::FlatBox);
        sep3.set_color(theme::SUBTLE);
        toolbar.fixed(&sep3, 1);

        // Stats label — takes remaining space, right-aligned
        let mut stats_label = frame::Frame::default().with_label("--");
        stats_label.set_label_color(theme::TEXT_MUTED);
        stats_label.set_label_size(10);
        stats_label.set_align(enums::Align::Right | enums::Align::Inside);

        toolbar.end();
        col.end();

        // Make the column fill the window and the canvas the resizable part
        wind.resizable(&col);
        col.resizable(&canvas);

        // Shared pixel buffer for the draw callback
        let pixels = Rc::new(RefCell::new(ViewerPixels {
            data: vec![0u8; (width * height * 4) as usize],
            width,
            height,
        }));
        let pixels_draw = pixels.clone();

        // Draw callback: scale the native-resolution image to fit the canvas
        // while preserving aspect ratio (letterboxing with black bars).
        canvas.draw(move |frm| {
            let px = pixels_draw.borrow();
            if px.data.is_empty() || px.width == 0 || px.height == 0 {
                return;
            }

            let Ok(mut img) = RgbImage::new(
                &px.data,
                px.width as i32,
                px.height as i32,
                enums::ColorDepth::Rgba8,
            ) else {
                return;
            };

            let canvas_w = frm.w() as f64;
            let canvas_h = frm.h() as f64;
            let img_aspect = px.width as f64 / px.height as f64;
            let canvas_aspect = canvas_w / canvas_h;

            // Fit image within canvas, preserving aspect ratio
            let (draw_w, draw_h) = if img_aspect > canvas_aspect {
                let w = canvas_w as i32;
                let h = (canvas_w / img_aspect) as i32;
                (w, h)
            } else {
                let h = canvas_h as i32;
                let w = (canvas_h * img_aspect) as i32;
                (w, h)
            };

            // Center the scaled image within the canvas
            let draw_x = frm.x() + (frm.w() - draw_w) / 2;
            let draw_y = frm.y() + (frm.h() - draw_h) / 2;

            img.scale(draw_w, draw_h, false, true);
            img.draw(draw_x, draw_y, draw_w, draw_h);
        });

        wind.end();
        wind.show();

        // ── Callbacks ────────────────────────────────────────────────

        // Stop Watching → unsubscribe + close window
        let s = sender;
        stop_btn.set_callback(move |_| {
            s.send(UiMessage::UnwatchPeer { client_id });
        });

        // Window close → also unsubscribe
        wind.set_callback(move |w| {
            if app::event() == enums::Event::Close {
                sender.send(UiMessage::UnwatchPeer { client_id });
                w.hide();
            }
        });

        // Mute — toggle stream audio mute
        let mut mute_clone = mute_btn.clone();
        let s_mute = sender;
        let mut is_stream_muted = false;
        mute_btn.clone().set_callback(move |_| {
            is_stream_muted = !is_stream_muted;
            s_mute.send(UiMessage::MuteStream {
                client_id,
                muted: is_stream_muted,
            });
            if is_stream_muted {
                mute_clone.set_label(&t!("unmute"));
                mute_clone.set_color(theme::DANGER);
                mute_clone.set_label_color(theme::BUTTON_TEXT);
            } else {
                mute_clone.set_label(&t!("mute"));
                mute_clone.set_color(theme::BG_TERTIARY);
                mute_clone.set_label_color(theme::TEXT_PRIMARY);
            }
        });

        // Volume — stream audio volume control
        let s_vol = sender;
        volume_slider.set_callback(move |s| {
            s_vol.send(UiMessage::SetStreamVolume {
                client_id,
                volume: s.value() as f32,
            });
        });

        Self {
            width,
            height,
            username: username.to_string(),
            wind,
            canvas,
            pixels,
            frame_count: 0,
            fps: FpsCounter::new(),
            total_bytes: 0,
            recent_frame_bytes: [0; 10],
            ring_idx: 0,
            stats_label,
        }
    }

    /// Push a new RGBA frame into the viewer. Redraws the canvas and
    /// updates the toolbar stats.
    ///
    /// `frame_bytes` is the encoded H.264 frame size (before decode)
    /// for display in the stats label. Pass 0 if unknown.
    ///
    /// Handles resolution changes gracefully — the draw callback reads
    /// width/height from the shared pixel buffer, so a mid-stream
    /// resolution switch just works.
    pub fn update_frame(
        &mut self,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        frame_bytes: usize,
        buffer_depth_ms: f32,
        buffer_target_ms: f32,
        buffer_jitter_ms: f32,
    ) {
        // Update stream resolution tracking
        if width != self.width || height != self.height {
            log::info!(
                "Viewer stream resolution changed: {}x{} → {width}x{height}",
                self.width,
                self.height,
            );
            self.width = width;
            self.height = height;

            // Update title bar to reflect new resolution
            self.wind.set_label(&t!(
                "viewer-title",
                "name" => self.username.as_str(),
                "width" => width,
                "height" => height
            ));
        }

        // Swap in the new pixel data + dimensions
        {
            let mut px = self.pixels.borrow_mut();
            px.data = rgba;
            px.width = width;
            px.height = height;
        }

        self.frame_count += 1;
        self.total_bytes += frame_bytes as u64;
        self.recent_frame_bytes[self.ring_idx] = frame_bytes as u64;
        self.ring_idx = (self.ring_idx + 1) % self.recent_frame_bytes.len();

        let fps = self.fps.tick();
        if self.fps.second_started() {
            let avg_frame_kb = self.recent_frame_bytes.iter().sum::<u64>() as f32
                / self.recent_frame_bytes.len() as f32
                / 1024.0;
            let throughput_kbps = avg_frame_kb * fps as f32;
            let total_mb = self.total_bytes as f32 / (1024.0 * 1024.0);
            let res = format!("{}p", height);
            self.stats_label.set_label(&t!(
                "viewer-stats",
                "res" => res,
                "fps" => fps,
                "kbps" => format!("{throughput_kbps:.0}"),
                "mb" => format!("{total_mb:.1}"),
                "depth" => format!("{buffer_depth_ms:.0}"),
                "target" => format!("{buffer_target_ms:.0}"),
                "jitter" => format!("{buffer_jitter_ms:.0}"),
            ));
        }

        self.canvas.redraw();
    }

    /// Close the window programmatically (e.g. peer stopped sharing).
    pub fn close(&mut self) {
        self.wind.hide();
    }
}
