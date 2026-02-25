//! Interactive controls: buttons, sliders, checkboxes, and dropdowns.
//!
//! Every constructor that takes a `col` parameter registers the widget
//! with the [`group::Flex`] at the correct height.  Buttons are the
//! exception — they are returned unsized so the caller can place them
//! in any layout context.

use crate::theme;
use fltk::{button, enums, group, menu, prelude::*, valuator};

// ── Buttons ─────────────────────────────────────────────────────────────

/// Standard button (subdued background).
///
/// Returned without a fixed height — the caller is responsible for
/// placing it in the layout (e.g. `col.fixed(&btn, 28)`).
#[must_use]
pub fn button(label: &str) -> button::Button {
    let mut btn = button::Button::default().with_label(label);
    btn.set_frame(enums::FrameType::FlatBox);
    btn.set_color(theme::BG_TERTIARY);
    btn.set_label_color(theme::TEXT_PRIMARY);
    btn.set_label_size(11);
    btn
}

/// Accent-coloured button for primary actions ("Save", "Send", etc.).
///
/// Like [`button`], returned without a fixed height.
#[must_use]
pub fn accent_button(label: &str, color: enums::Color) -> button::Button {
    let mut btn = button::Button::default().with_label(label);
    btn.set_color(color);
    btn.set_label_color(theme::BUTTON_TEXT);
    btn.set_selection_color(color);
    btn.set_frame(enums::FrameType::FlatBox);
    btn.set_label_size(11);
    btn
}

// ── Checkbox ────────────────────────────────────────────────────────────

/// Checkbox toggle, registered at 22 px height.
pub fn checkbox(col: &mut group::Flex, label: &str, checked: bool) -> button::CheckButton {
    let mut cb = button::CheckButton::default().with_label(label);
    cb.set_checked(checked);
    cb.set_label_size(11);
    col.fixed(&cb, 22);
    cb
}

// ── Slider ──────────────────────────────────────────────────────────────

/// Horizontal slider, registered at 22 px height.
pub fn slider(
    col: &mut group::Flex,
    min: f64,
    max: f64,
    value: f64,
    step: f64,
    accent: enums::Color,
) -> valuator::HorSlider {
    let mut s = valuator::HorSlider::default();
    s.set_minimum(min);
    s.set_maximum(max);
    s.set_value(value);
    s.set_step(step, 1);
    s.set_selection_color(accent);
    col.fixed(&s, 22);
    s
}

// ── Dropdown ────────────────────────────────────────────────────────────

/// Empty dropdown, registered at 26 px height.
///
/// Use [`populate_dropdown`] to fill it with options after creation.
pub fn dropdown(col: &mut group::Flex) -> menu::Choice {
    let c = menu::Choice::default();
    col.fixed(&c, 26);
    c
}

/// Populate a [`menu::Choice`] dropdown with a list of options.
///
/// `default_label` is always inserted as the first entry (e.g. `"System Default"`,
/// `"None"`, `"Auto"`).  Each item in `options` is added after it.
///
/// If `current` matches one of the option labels, that option is
/// pre-selected; otherwise the default entry (index 0) is selected.
pub fn populate_dropdown(
    choice: &mut menu::Choice,
    default_label: &str,
    options: &[impl AsRef<str>],
    current: Option<&str>,
) {
    choice.clear();
    choice.add_choice(default_label);

    let mut selected: i32 = 0;
    for (i, opt) in options.iter().enumerate() {
        choice.add_choice(opt.as_ref());
        if current == Some(opt.as_ref()) {
            selected = (i + 1) as i32;
        }
    }

    choice.set_value(selected);
}
