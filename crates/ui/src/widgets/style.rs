//! Dark-mode styling helpers for FLTK text widgets.

use crate::theme;
use fltk::{enums, input, prelude::*, text};

/// Apply dark styling to an [`input::Input`] widget.
pub fn style_input(input: &mut input::Input) {
    input.set_color(theme::SURFACE);
    input.set_text_color(enums::Color::Foreground);
    input.set_cursor_color(enums::Color::Selection);
    input.set_selection_color(enums::Color::Selection);
    input.set_frame(enums::FrameType::BorderBox);
}

/// Apply dark styling to a [`text::TextDisplay`] widget.
pub fn style_text_display(display: &mut text::TextDisplay) {
    display.set_color(theme::SURFACE);
    display.set_text_color(enums::Color::Foreground);
    display.set_cursor_color(enums::Color::Selection);
    display.set_selection_color(enums::Color::Selection);
    display.set_frame(enums::FrameType::BorderBox);
}
