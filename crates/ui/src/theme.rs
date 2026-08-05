//! App-wide theming.
//!
//! Palette — "workbench": layered dark greys with a cobalt accent.
//! Slack-adjacent, deliberately not near-black and with no purple.
//!
//! | name      | hex       | role                     |
//! |-----------|-----------|--------------------------|
//! | graphite  | `#1A1D21` | ground                   |
//! | gunmetal  | `#222529` | raised panels            |
//! | fog       | `#D1D2D3` | primary text             |
//! | steel     | `#ABADB0` | secondary text           |
//! | cobalt    | `#1264A3` | accent                   |
//! | green     | `#2BAC76` | positive / speaking      |
//! | amber     | `#ECB22E` | caution                  |
//! | raspberry | `#E01E5A` | errors / destructive     |
//!
//! Greys and tints below are derived from these.
//!
//! # Usage
//!
//! ```no_run
//! # use fltk::{prelude::*, *};
//! let app = app::App::default();
//! ui::theme::apply();
//! ```

use fltk::{app, enums};
use fltk_theme::{ColorTheme, SchemeType, WidgetScheme, color_themes};

// ── Backgrounds ─────────────────────────────────────────────────────────

/// Primary window background — graphite `#1A1D21`.
pub const BG_PRIMARY: enums::Color = enums::Color::from_rgb(26, 29, 33);
/// Secondary panels (sidebar, scroll areas) — gunmetal `#222529`.
pub const BG_SECONDARY: enums::Color = enums::Color::from_rgb(34, 37, 41);
/// Tertiary element background (buttons, inactive controls) — `#2B2E33`.
pub const BG_TERTIARY: enums::Color = enums::Color::from_rgb(43, 46, 51);

// ── Text ────────────────────────────────────────────────────────────────

/// Primary text (headings, labels) — fog `#D1D2D3`.
pub const TEXT_PRIMARY: enums::Color = enums::Color::from_rgb(209, 210, 211);
/// Secondary text (field labels, descriptions) — steel `#ABADB0`.
pub const TEXT_SECONDARY: enums::Color = enums::Color::from_rgb(171, 173, 176);
/// Muted text (hints, inactive items) — `#808386`.
pub const TEXT_MUTED: enums::Color = enums::Color::from_rgb(128, 131, 134);
/// Monospace stat readout text — quiet green `#86AA90`.
pub const MONO_TEXT: enums::Color = enums::Color::from_rgb(134, 170, 144);

// ── Accents & status ────────────────────────────────────────────────────

/// Primary interactive accent (buttons, selection, sliders, own
/// username) — cobalt `#1264A3`.
pub const ACCENT: enums::Color = enums::Color::from_rgb(18, 100, 163);
/// Hover / pressed / engaged state for accent elements — deep cobalt
/// `#0E5082`.
pub const ACCENT_HOVER: enums::Color = enums::Color::from_rgb(14, 80, 130);
/// Positive status / speaking indicator / normal level — green
/// `#2BAC76`.
pub const SUCCESS: enums::Color = enums::Color::from_rgb(43, 172, 118);
/// Caution — level approaching limit — amber `#ECB22E`.
pub const WARNING: enums::Color = enums::Color::from_rgb(236, 178, 46);
/// Error / clipping / destructive action / live — raspberry `#E01E5A`.
pub const DANGER: enums::Color = enums::Color::from_rgb(224, 30, 90);
/// Meters and neutral secondary readouts — steel `#ABADB0`.
/// (Constant keeps its historical name; instrument-style displays
/// stay neutral so the accent keeps its meaning.)
pub const CYAN: enums::Color = enums::Color::from_rgb(171, 173, 176);

// ── Surfaces ────────────────────────────────────────────────────────────

/// Background for custom-drawn widgets (meters, inputs) — `#15171A`.
pub const SURFACE: enums::Color = enums::Color::from_rgb(21, 23, 26);
/// Hairline border around custom-drawn widgets — `#3A3D42`.
pub const BORDER: enums::Color = enums::Color::from_rgb(58, 61, 66);
/// Dim fill for inactive bars and separators — `#303338`.
pub const SUBTLE: enums::Color = enums::Color::from_rgb(48, 51, 56);

/// High-contrast text on accent-coloured buttons — white.
pub const BUTTON_TEXT: enums::Color = enums::Color::from_rgb(255, 255, 255);

// ── Sidebar rows ────────────────────────────────────────────────────────
// Previously hardcoded in the client's sidebar; kept here so the whole
// skin lives in one file.

/// Row tint for a user you've locally muted — raspberry-tinted grey.
pub const ROW_MUTED_TINT: enums::Color = enums::Color::from_rgb(46, 28, 35);
/// Row tint for a user with non-default volume — cobalt-tinted grey.
pub const ROW_VOLUME_TINT: enums::Color = enums::Color::from_rgb(28, 35, 45);
/// Row hover highlight.
pub const HOVER: enums::Color = enums::Color::from_rgb(39, 42, 47);

/// Apply the full theme.  Call once, right after `app::App::default()`.
pub fn apply() {
    app::set_visible_focus(false);

    // Widget drawing scheme — Fleet1 gives subtle frames that work well
    // on a dark palette. The Fleet color map provides sane defaults for
    // every widget type…
    ColorTheme::new(&color_themes::fleet::DARK2).apply();
    WidgetScheme::new(SchemeType::Fleet1).apply();

    // …then the base colors are re-mapped onto the baucord palette so
    // widgets we never touch directly (menus, scrollbars, tooltips)
    // match ink/ash instead of Fleet's greys. Derived from the
    // constants above — edit those and this follows.
    let (r, g, b) = BG_PRIMARY.to_rgb();
    app::background(r, g, b);
    let (r, g, b) = SURFACE.to_rgb();
    app::background2(r, g, b);
    let (r, g, b) = TEXT_PRIMARY.to_rgb();
    app::foreground(r, g, b);
    let (r, g, b) = ACCENT.to_rgb();
    app::set_color(enums::Color::Selection, r, g, b);
}
