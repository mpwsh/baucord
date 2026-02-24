// ui/viewer.rs — Screen share viewer window.
//
// One window per watched peer. Receives RGBA pixel data from the screen
// thread (via ScreenEvent::FrameReady) and blits it into an FLTK image.
//
// The window is resizable and maximizable. The stream is rendered at its
// native resolution and scaled to fit the window by FLTK's draw pass —
// no CPU-side scaling needed. Aspect ratio is preserved with letterboxing.
//
// Closing the window sends an unsubscribe message so the server stops
// forwarding frames.

use fltk::{enums, image::RgbImage, prelude::*, *};
use std::cell::RefCell;
use std::rc::Rc;

use crate::ui::messages::UiMessage;

/// Shared state between the struct and the draw callback.
struct ViewerPixels {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

/// State for a single viewer window.
pub struct ViewerWindow {
    pub client_id: u64,
    pub width: u32,
    pub height: u32,
    wind: window::Window,
    canvas: frame::Frame,
    /// Shared pixel buffer — written by update_frame(), read by the draw callback.
    pixels: Rc<RefCell<ViewerPixels>>,
    /// Frame counter for stats.
    frame_count: u64,
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
        let win_h = height.min(max_h) as i32;

        let title = format!("{username}'s screen ({width}x{height})");

        let mut wind = window::Window::default()
            .with_size(win_w, win_h)
            .with_label(&title);
        wind.make_resizable(true);
        wind.size_range(320, 240, 0, 0); // minimum size, no maximum
        wind.set_color(enums::Color::Black);
        crate::ui::set_window_icon(&mut wind);

        // Center on screen
        let cx = ((screen_w as i32) - win_w) / 2;
        let cy = ((screen_h as i32) - win_h) / 2;
        wind.set_pos(cx, cy);

        // Canvas frame fills the entire window
        let mut canvas = frame::Frame::default()
            .with_pos(0, 0)
            .with_size(win_w, win_h);
        canvas.set_frame(enums::FrameType::FlatBox);
        canvas.set_color(enums::Color::Black);

        // Make the canvas the resizable widget so it stretches with the window
        wind.resizable(&canvas);

        // Shared pixel buffer for the draw callback
        let pixels = Rc::new(RefCell::new(ViewerPixels {
            data: vec![0u8; (width * height * 4) as usize],
            width,
            height,
        }));
        let pixels_draw = pixels.clone();

        // Draw callback: scale the native-resolution image to fit the canvas
        // while preserving aspect ratio (letterboxing with black bars).
        // FLTK's RgbImage::draw(x, y, w, h) handles the scaling in the
        // display layer — no CPU-side pixel manipulation needed.
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
                // Image is wider than canvas — fit to width, letterbox top/bottom
                let w = canvas_w as i32;
                let h = (canvas_w / img_aspect) as i32;
                (w, h)
            } else {
                // Image is taller than canvas — fit to height, pillarbox left/right
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

        // Close → unsubscribe from this peer's stream
        let cid = client_id;
        wind.set_callback(move |w| {
            if app::event() == enums::Event::Close {
                sender.send(UiMessage::UnwatchPeer { client_id: cid });
                w.hide();
            }
        });

        Self {
            client_id,
            width,
            height,
            wind,
            canvas,
            pixels,
            frame_count: 0,
        }
    }

    /// Push a new RGBA frame into the viewer. Redraws the canvas.
    ///
    /// Handles resolution changes gracefully — the draw callback reads
    /// width/height from the shared pixel buffer, so a mid-stream
    /// resolution switch just works.
    pub fn update_frame(&mut self, rgba: Vec<u8>, width: u32, height: u32) {
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
            let title = self.wind.label();
            if let Some(name_end) = title.find("'s screen") {
                let username = &title[..name_end];
                self.wind
                    .set_label(&format!("{username}'s screen ({width}x{height})"));
            }
        }

        // Swap in the new pixel data + dimensions
        {
            let mut px = self.pixels.borrow_mut();
            px.data = rgba;
            px.width = width;
            px.height = height;
        }

        self.frame_count += 1;
        self.canvas.redraw();
    }

    /// Returns true if the window has been closed by the user.
    pub fn is_closed(&self) -> bool {
        !self.wind.shown()
    }

    /// Close the window programmatically (e.g. peer stopped sharing).
    pub fn close(&mut self) {
        self.wind.hide();
    }
}