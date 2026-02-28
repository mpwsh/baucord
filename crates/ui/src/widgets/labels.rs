//! Label constructors for panel layouts.
//!
//! Each function creates a [`frame::Frame`], styles it, registers it with
//! a [`group::Flex`] column at the correct height, and returns the frame
//! (where the caller needs to update it later).

use crate::theme;
use fltk::{enums, frame, group, prelude::*};

/// Bold section heading with an emoji icon, e.g. `("🎧", "DEVICES")`.
pub fn section_header(col: &mut group::Flex, icon: &str, text: &str) {
    let mut lbl = frame::Frame::default()
        .with_label(&format!("{icon}  {text}"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    lbl.set_label_size(13);
    col.fixed(&lbl, 24);
}

/// Small label above a control, e.g. "Input Device".
pub fn field_label(col: &mut group::Flex, text: &str) {
    let mut lbl = frame::Frame::default()
        .with_label(text)
        .with_align(enums::Align::Left | enums::Align::Inside);
    lbl.set_label_size(11);
    col.fixed(&lbl, 18);
}

/// Monospace-coloured label for live stat readouts.
pub fn stat_label(col: &mut group::Flex, text: &str) -> frame::Frame {
    let mut lbl = frame::Frame::default()
        .with_label(text)
        .with_align(enums::Align::Left | enums::Align::Inside);
    lbl.set_label_color(theme::MONO_TEXT);
    lbl.set_label_size(11);
    col.fixed(&lbl, 16);
    lbl
}

/// Tiny muted label for values / hints below a slider.
pub fn hint_label(col: &mut group::Flex, text: &str) -> frame::Frame {
    let mut lbl = frame::Frame::default()
        .with_label(text)
        .with_align(enums::Align::Left | enums::Align::Inside);
    lbl.set_label_color(theme::TEXT_MUTED);
    lbl.set_label_size(10);
    col.fixed(&lbl, 14);
    lbl
}
