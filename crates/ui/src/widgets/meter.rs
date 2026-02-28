//! Audio level meter widgets.
//!
//! Two meter variants built on a shared drawing core:
//!
//! - **[`vu`]** — colour-coded level bar (green → amber → red).
//! - **[`gate`]** — level bar with a threshold marker and open/closed colouring.
//!
//! Both read an [`AtomicU32`] whose bits are an `f32` in 0.0–1.0.

use crate::theme;
use fltk::{draw, enums, frame, group, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

// ── Shared constants ────────────────────────────────────────────────────

/// Horizontal padding inside the meter frame (left and right).
const BAR_INSET_X: i32 = 2;

/// Vertical padding for the VU meter bar.
const VU_INSET_Y: i32 = 3;

/// Vertical padding for the gate bar.
const GATE_INSET_Y: i32 = 4;

/// Level above which the VU meter turns red.
const LEVEL_DANGER: f32 = 0.9;

/// Level above which the VU meter turns amber.
const LEVEL_WARNING: f32 = 0.7;

// ── Shared draw helpers ─────────────────────────────────────────────────

/// Read the atomic level as a clamped `f32`.
fn read_level(level: &AtomicU32) -> f32 {
    f32::from_bits(level.load(Ordering::Relaxed)).clamp(0.0, 1.0)
}

/// Usable bar width after horizontal insets.
fn bar_width(frame_w: i32) -> i32 {
    frame_w - BAR_INSET_X * 2
}

/// Fill the frame background with [`theme::SURFACE`].
fn draw_background(frm: &frame::Frame) {
    draw::set_draw_color(theme::SURFACE);
    draw::draw_rectf(frm.x(), frm.y(), frm.w(), frm.h());
}

/// Draw the hairline border around the frame.
fn draw_border(frm: &frame::Frame) {
    draw::set_draw_color(theme::BORDER);
    draw::draw_rect(frm.x(), frm.y(), frm.w(), frm.h());
}

/// Allocate a frame, style it as a meter surface, and register it in `col`.
fn make_frame(col: &mut group::Flex, height: i32) -> frame::Frame {
    let mut f = frame::Frame::default();
    f.set_frame(enums::FrameType::FlatBox);
    f.set_color(theme::SURFACE);
    col.fixed(&f, height);
    f
}

// ── VU meter ────────────────────────────────────────────────────────────

/// Create a VU (Volume Unit) meter and add it to `col`.
///
/// The bar colour shifts from `accent` → [`WARNING`](theme::WARNING) →
/// [`DANGER`](theme::DANGER) as the level rises.
///
/// # Arguments
///
/// - `level` — shared `AtomicU32` whose bits are an `f32` in 0.0–1.0.
/// - `accent` — "normal" bar colour (e.g. [`theme::SUCCESS`] for input,
///   [`theme::CYAN`] for output).
pub fn vu(
    col: &mut group::Flex,
    level: Arc<AtomicU32>,
    height: i32,
    accent: enums::Color,
) -> frame::Frame {
    let mut f = make_frame(col, height);

    f.draw(move |frm| {
        draw_background(frm);

        let val = read_level(&level);
        let filled = (bar_width(frm.w()) as f32 * val) as i32;

        let color = if val > LEVEL_DANGER {
            theme::DANGER
        } else if val > LEVEL_WARNING {
            theme::WARNING
        } else {
            accent
        };

        if filled > 0 {
            draw::set_draw_color(color);
            draw::draw_rectf(
                frm.x() + BAR_INSET_X,
                frm.y() + VU_INSET_Y,
                filled,
                frm.h() - VU_INSET_Y * 2,
            );
        }

        draw_border(frm);
    });

    f
}

// ── Gate bar ────────────────────────────────────────────────────────────

/// Display scaling factor applied to the raw threshold before drawing,
/// so the marker aligns with the already-scaled level value.
///
/// Must match the `rms * 3.0` scaling applied in the input pipeline.
pub const GATE_DISPLAY_SCALE: f32 = 3.0;

/// Create a gate bar and add it to `col`.
///
/// The bar turns [`SUCCESS`](theme::SUCCESS) when the level exceeds the
/// threshold (gate open) and [`SUBTLE`](theme::SUBTLE) when below
/// (gate closed).  A red vertical line marks the threshold position.
///
/// # Arguments
///
/// - `level` — shared `AtomicU32` (bit-cast `f32` in 0.0–1.0),
///   already multiplied by [`GATE_DISPLAY_SCALE`].
/// - `threshold` — shared *raw* threshold (e.g. 0.02 RMS).
///   Scaled by [`GATE_DISPLAY_SCALE`] before drawing so the marker
///   sits at the correct visual position.
pub fn gate(
    col: &mut group::Flex,
    level: Arc<AtomicU32>,
    threshold: Rc<RefCell<f32>>,
    height: i32,
) -> frame::Frame {
    let mut f = make_frame(col, height);

    f.draw(move |frm| {
        draw_background(frm);

        let val = read_level(&level);
        let raw_thr = *threshold.borrow();
        let thr_scaled = (raw_thr * GATE_DISPLAY_SCALE).clamp(0.0, 1.0);

        let usable_w = bar_width(frm.w());
        let filled = (usable_w as f32 * val) as i32;

        // Bar: green when above threshold, dim when below.
        let color = if val > thr_scaled {
            theme::SUCCESS
        } else {
            theme::SUBTLE
        };

        if filled > 0 {
            draw::set_draw_color(color);
            draw::draw_rectf(
                frm.x() + BAR_INSET_X,
                frm.y() + GATE_INSET_Y,
                filled,
                frm.h() - GATE_INSET_Y * 2,
            );
        }

        // Threshold marker (red vertical line).
        let thr_x = frm.x() + BAR_INSET_X + (usable_w as f32 * thr_scaled) as i32;
        draw::set_draw_color(theme::DANGER);
        draw::set_line_style(draw::LineStyle::Solid, 2);
        draw::draw_line(
            thr_x,
            frm.y() + BAR_INSET_X,
            thr_x,
            frm.y() + frm.h() - BAR_INSET_X,
        );
        draw::set_line_style(draw::LineStyle::Solid, 1);

        draw_border(frm);
    });

    f
}
