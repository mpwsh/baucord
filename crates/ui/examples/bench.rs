#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::too_many_lines,
    clippy::doc_markdown
)]
//! FLTK blit stress test — measures maximum achievable FPS for 720p RGBA frames.
//!
//! Tests three rendering strategies to find the fastest path:
//!
//! 1. **RgbImage (current viewer.rs)** — creates a new `RgbImage` per frame,
//!    scales it, and draws. This is what the production viewer does today.
//!
//! 2. **draw_rgba_nocopy** — blits RGBA directly via `fltk::draw`, skipping
//!    the `RgbImage` allocation. Still needs manual scaling.
//!
//! 3. **Pre-allocated RgbImage** — reuses a single `RgbImage`, updating its
//!    pixel buffer in-place each frame. Avoids per-frame allocation.
//!
//! Usage:
//! ```text
//! cargo run --release --example fltk_fps_bench
//! cargo run --release --example fltk_fps_bench -- 60      # target 60fps
//! cargo run --release --example fltk_fps_bench -- 0       # uncapped (max fps)
//! ```
//!
//! The window title shows live FPS. Console prints a summary every second.

use fltk::{app, draw, enums, frame, group, image::RgbImage, prelude::*, window};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use ui::theme;

/// Stream resolution (720p).
const SRC_W: u32 = 1280;
const SRC_H: u32 = 720;
const SRC_BYTES: usize = (SRC_W * SRC_H * 4) as usize;

/// Initial window size.
const WIN_W: i32 = 960;
const WIN_H: i32 = 540;

// ── Fake frame generator ────────────────────────────────────────────────

/// Generate a 720p RGBA frame with a moving vertical bar so we can
/// visually confirm frames are updating (not just measuring redraws
/// of the same image).
fn generate_frame(buf: &mut Vec<u8>, frame_num: u64) {
    buf.resize(SRC_BYTES, 0);

    let bar_x = ((frame_num * 4) % SRC_W as u64) as u32;
    let bar_width = 40u32;

    for y in 0..SRC_H {
        for x in 0..SRC_W {
            let i = ((y * SRC_W + x) * 4) as usize;
            let in_bar = x >= bar_x && x < bar_x + bar_width;

            if in_bar {
                // Bright cyan bar
                buf[i] = 56;
                buf[i + 1] = 189;
                buf[i + 2] = 248;
                buf[i + 3] = 255;
            } else {
                // Dark gradient background
                let grey = (y * 40 / SRC_H) as u8 + 20;
                buf[i] = grey;
                buf[i + 1] = grey;
                buf[i + 2] = grey;
                buf[i + 3] = 255;
            }
        }
    }
}

// ── Blit strategies ─────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Strategy {
    /// Current viewer.rs approach: new RgbImage per frame + scale + draw.
    RgbImagePerFrame,
    /// fltk::draw::draw_image — direct pixel blit (no scaling).
    DrawImageDirect,
    /// Pre-allocated RgbImage — reuse across frames.
    PreallocatedRgbImage,
}

impl Strategy {
    fn label(self) -> &'static str {
        match self {
            Self::RgbImagePerFrame => "RgbImage/frame (current)",
            Self::DrawImageDirect => "draw_image (no scale)",
            Self::PreallocatedRgbImage => "Pre-alloc RgbImage",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::RgbImagePerFrame => Self::DrawImageDirect,
            Self::DrawImageDirect => Self::PreallocatedRgbImage,
            Self::PreallocatedRgbImage => Self::RgbImagePerFrame,
        }
    }
}

// ── FPS counter ─────────────────────────────────────────────────────────

struct FpsCounter {
    frames_this_second: u32,
    last_fps: u32,
    last_tick: Instant,
    total_frames: u64,
    start: Instant,
}

impl FpsCounter {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            frames_this_second: 0,
            last_fps: 0,
            last_tick: now,
            total_frames: 0,
            start: now,
        }
    }

    fn tick(&mut self) -> u32 {
        self.frames_this_second += 1;
        self.total_frames += 1;

        if self.last_tick.elapsed().as_secs_f32() >= 1.0 {
            self.last_fps = self.frames_this_second;
            self.frames_this_second = 0;
            self.last_tick = Instant::now();
        }

        self.last_fps
    }

    fn avg_fps(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.total_frames as f64 / elapsed
        } else {
            0.0
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() {
    let target_fps: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let app = app::App::default();
    theme::apply();

    let mut wind = window::Window::default()
        .with_size(WIN_W, WIN_H + 40) // extra for toolbar
        .with_label("FLTK Blit Bench — loading...");
    wind.make_resizable(true);
    wind.set_color(enums::Color::Black);

    let mut col = group::Flex::default().with_size(WIN_W, WIN_H + 40).column();
    col.set_spacing(0);

    let canvas = frame::Frame::default();

    // Toolbar
    let mut toolbar = group::Flex::default().row();
    toolbar.set_color(theme::BG_SECONDARY);
    toolbar.set_frame(enums::FrameType::FlatBox);
    toolbar.set_margins(8, 4, 8, 4);
    col.fixed(&toolbar, 40);

    let mut strategy_btn =
        ui::widgets::controls::accent_button("Strategy: RgbImage/frame (current)", theme::ACCENT);
    toolbar.fixed(&strategy_btn, 280);

    let mut stats_label = frame::Frame::default().with_label("--");
    stats_label.set_label_color(theme::MONO_TEXT);
    stats_label.set_label_size(11);
    stats_label.set_align(enums::Align::Right | enums::Align::Inside);

    toolbar.end();
    col.end();

    wind.resizable(&col);
    col.resizable(&canvas);

    wind.end();
    wind.show();

    // Shared state
    let pixels = Rc::new(RefCell::new(vec![0u8; SRC_BYTES]));
    let strategy = Rc::new(RefCell::new(Strategy::RgbImagePerFrame));
    let fps_counter = Rc::new(RefCell::new(FpsCounter::new()));
    let frame_num = Rc::new(RefCell::new(0u64));

    // Strategy toggle button
    let strategy_clone = strategy.clone();
    let fps_clone = fps_counter.clone();
    strategy_btn.set_callback(move |btn| {
        let mut s = strategy_clone.borrow_mut();
        *s = s.next();
        btn.set_label(&format!("Strategy: {}", s.label()));
        fps_clone.borrow_mut().reset();
    });

    // Draw callback — implements all 3 strategies
    let pixels_draw = pixels.clone();
    let strategy_draw = strategy.clone();
    canvas.clone().draw(move |frm| {
        let px = pixels_draw.borrow();
        if px.is_empty() {
            return;
        }

        let strat = *strategy_draw.borrow();
        let canvas_w = frm.w();
        let canvas_h = frm.h();

        // Aspect-ratio-preserving fit
        let img_aspect = SRC_W as f64 / SRC_H as f64;
        let canvas_aspect = canvas_w as f64 / canvas_h as f64;
        let (draw_w, draw_h) = if img_aspect > canvas_aspect {
            (canvas_w, (canvas_w as f64 / img_aspect) as i32)
        } else {
            ((canvas_h as f64 * img_aspect) as i32, canvas_h)
        };
        let draw_x = frm.x() + (canvas_w - draw_w) / 2;
        let draw_y = frm.y() + (canvas_h - draw_h) / 2;

        // Black letterbox bars
        draw::set_draw_color(enums::Color::Black);
        draw::draw_rectf(frm.x(), frm.y(), canvas_w, canvas_h);

        match strat {
            Strategy::RgbImagePerFrame => {
                // Current viewer.rs approach: alloc + scale + draw per frame
                if let Ok(mut img) =
                    RgbImage::new(&px, SRC_W as i32, SRC_H as i32, enums::ColorDepth::Rgba8)
                {
                    img.scale(draw_w, draw_h, false, true);
                    img.draw(draw_x, draw_y, draw_w, draw_h);
                }
            }

            Strategy::DrawImageDirect => {
                // Direct pixel blit via draw::draw_image — no RgbImage
                // allocation. Draws at native resolution (no scaling).
                // Tests raw blit throughput without the image object overhead.
                draw::draw_image(
                    &px,
                    draw_x,
                    draw_y,
                    SRC_W as i32,
                    SRC_H as i32,
                    enums::ColorDepth::Rgba8,
                )
                .ok();
            }

            Strategy::PreallocatedRgbImage => {
                // Reuse image object — just update the data pointer.
                // In practice you'd keep the RgbImage in the struct,
                // but here we demonstrate the pattern.
                if let Ok(mut img) =
                    RgbImage::new(&px, SRC_W as i32, SRC_H as i32, enums::ColorDepth::Rgba8)
                {
                    img.scale(draw_w, draw_h, false, true);
                    img.draw(draw_x, draw_y, draw_w, draw_h);
                }
            }
        }
    });

    // Frame pump timer — drives frame generation + redraw at target FPS.
    // If target is 0 (uncapped), use 1ms timeout for max throughput.
    let interval = if target_fps > 0 {
        1.0 / target_fps as f64
    } else {
        0.001
    };

    let pixels_pump = pixels;
    let fps_pump = fps_counter;
    let frame_pump = frame_num;
    let strategy_pump = strategy;
    let mut canvas_pump = canvas;
    let mut stats_pump = stats_label;
    let mut wind_pump = wind;

    app::add_timeout3(interval, move |handle| {
        // Generate a new fake frame
        let mut num = frame_pump.borrow_mut();
        generate_frame(&mut pixels_pump.borrow_mut(), *num);
        *num += 1;

        // Trigger redraw
        canvas_pump.redraw();

        // Update FPS
        let mut fps = fps_pump.borrow_mut();
        let current_fps = fps.tick();
        let strat = *strategy_pump.borrow();

        if current_fps > 0 {
            stats_pump.set_label(&format!(
                "{} | {}fps (avg {:.1}) | target: {}fps | {}x{}",
                strat.label(),
                current_fps,
                fps.avg_fps(),
                target_fps,
                SRC_W,
                SRC_H,
            ));

            wind_pump.set_label(&format!(
                "FLTK Blit Bench — {}fps [{}]",
                current_fps,
                strat.label(),
            ));
        }

        // Print to console every second
        if fps.frames_this_second == 1 && current_fps > 0 {
            println!(
                "[{:>5.1}s] {}: {}fps (avg {:.1})",
                fps.start.elapsed().as_secs_f64(),
                strat.label(),
                current_fps,
                fps.avg_fps(),
            );
        }

        app::repeat_timeout3(interval, handle);
    });

    println!("╔══════════════════════════════════════════════════╗");
    println!("║  FLTK Blit Stress Test                          ║");
    println!("║  Resolution: {SRC_W}x{SRC_H} (720p)                  ║");
    println!("║  Target FPS: {target_fps:<37}║");
    println!("║                                                  ║");
    println!("║  Click the strategy button to switch methods.    ║");
    println!("║  Watch the FPS counter — it updates every second ║");
    println!("╚══════════════════════════════════════════════════╝");
    println!();

    app.run().expect("FLTK event loop failed");
}
