//! App-wide theming.
//!
//! Palette — shared with the website, five named colors:
//!
//! | name        | hex       | role                     |
//! |-------------|-----------|--------------------------|
//! | ink black   | `#050517` | ground                   |
//! | ash grey    | `#BFC3BA` | primary text             |
//! | silver      | `#A9ACA9` | secondary text           |
//! | cherry rose | `#A4303F` | accent                   |
//! | burnt rose  | `#8E3B46` | accent, hover / pressed  |
//!
//! Everything else here is derived from those five.
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

/// Primary window background — ink black `#050517`.
pub const BG_PRIMARY: enums::Color = enums::Color::from_rgb(5, 5, 23);
/// Secondary panels (sidebar, scroll areas) — raised ink `#0C0C24`.
pub const BG_SECONDARY: enums::Color = enums::Color::from_rgb(12, 12, 36);
/// Tertiary element background (buttons, inactive controls) — `#131330`.
pub const BG_TERTIARY: enums::Color = enums::Color::from_rgb(19, 19, 48);

// ── Text ────────────────────────────────────────────────────────────────

/// Primary text (headings, labels) — ash grey `#BFC3BA`.
pub const TEXT_PRIMARY: enums::Color = enums::Color::from_rgb(191, 195, 186);
/// Secondary text (field labels, descriptions) — silver `#A9ACA9`.
pub const TEXT_SECONDARY: enums::Color = enums::Color::from_rgb(169, 172, 169);
/// Muted text (hints, inactive items) — dimmed ash `#7C807A`.
pub const TEXT_MUTED: enums::Color = enums::Color::from_rgb(124, 128, 122);
/// Monospace stat readout text — sage `#96B28E`.
pub const MONO_TEXT: enums::Color = enums::Color::from_rgb(150, 178, 142);

// ── Accents & status ────────────────────────────────────────────────────

/// Primary interactive accent (buttons, selection, sliders, "Live",
/// own username) — cherry rose `#A4303F`.
pub const ACCENT: enums::Color = enums::Color::from_rgb(164, 48, 63);
/// Hover / pressed state for accent elements — burnt rose `#8E3B46`.
pub const ACCENT_HOVER: enums::Color = enums::Color::from_rgb(142, 59, 70);
/// Positive status / speaking indicator / normal level — sage green
/// `#84B082` (the palette has no green; this one leans toward ash).
pub const SUCCESS: enums::Color = enums::Color::from_rgb(132, 176, 130);
/// Caution — level approaching limit — muted amber `#D69E60`.
pub const WARNING: enums::Color = enums::Color::from_rgb(214, 158, 96);
/// Error / clipping / destructive action — brightened cherry `#C75C66`.
/// Deliberately lighter than [`ACCENT`] so error *text* stays readable
/// on ink and errors don't read as brand moments.
pub const DANGER: enums::Color = enums::Color::from_rgb(199, 92, 102);
/// Meters and neutral secondary readouts — silver `#A9ACA9`.
/// (Constant keeps its historical name. The palette has no cyan;
/// instrument-style displays stay neutral so cherry keeps meaning.)
pub const CYAN: enums::Color = enums::Color::from_rgb(169, 172, 169);

// ── Surfaces ────────────────────────────────────────────────────────────

/// Background for custom-drawn widgets (meters, inputs) — `#0A0A1E`.
pub const SURFACE: enums::Color = enums::Color::from_rgb(10, 10, 30);
/// Hairline border around custom-drawn widgets — `#262640`.
pub const BORDER: enums::Color = enums::Color::from_rgb(38, 38, 64);
/// Dim fill for inactive bars and separators — `#1E1E36`.
pub const SUBTLE: enums::Color = enums::Color::from_rgb(30, 30, 54);

/// High-contrast text on accent-coloured buttons — warm paper `#F2F0EC`.
pub const BUTTON_TEXT: enums::Color = enums::Color::from_rgb(242, 240, 236);

// ── Sidebar rows ────────────────────────────────────────────────────────
// Previously hardcoded in the client's sidebar; kept here so the whole
// skin lives in one file.

/// Row tint for a user you've locally muted — burnt-tinted ink.
pub const ROW_MUTED_TINT: enums::Color = enums::Color::from_rgb(40, 18, 30);
/// Row tint for a user with non-default volume.
pub const ROW_VOLUME_TINT: enums::Color = enums::Color::from_rgb(18, 18, 44);
/// Row hover highlight.
pub const HOVER: enums::Color = enums::Color::from_rgb(26, 26, 52);

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
