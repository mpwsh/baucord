// ui/settings/mod.rs — Settings window with tabbed navigation.
//
// Left panel: category list (Audio, Keyboard, etc.)
// Right panel: scrollable content area that swaps based on selected tab.
//
// The content panels are Flex columns with explicit size and position
// inside a Scroll — matching the same pattern the original single-panel
// settings window used (which FLTK is happy with).

pub mod about;
pub mod audio;
pub mod credits;
pub mod general;
pub mod keybinds;

use fltk::{prelude::*, *};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use client::config::Settings;
use crate::ui::messages::UiMessage;
use i18n::t;
use ui::theme;
use ui::widgets::controls;

/// Tab identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Audio,
    Keyboard,
    Credits,
    About,
}

impl Tab {
    const ALL: &'static [Tab] =
        &[Tab::General, Tab::Audio, Tab::Keyboard, Tab::Credits, Tab::About];

    fn label(&self) -> String {
        match self {
            Tab::General => t!("tab-general"),
            Tab::Audio => t!("tab-audio"),
            Tab::Keyboard => t!("tab-keyboard"),
            Tab::Credits => t!("tab-credits"),
            Tab::About => t!("tab-about"),
        }
    }
}

pub struct SettingsWindow;

impl SettingsWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        sender: app::Sender<UiMessage>,
        settings: Settings,
        input_devices: Vec<(String, bool)>,
        output_devices: Vec<(String, bool)>,
        output_level: Arc<AtomicU32>,
        denoised_level: Arc<AtomicU32>,
        parent_x: i32,
        parent_y: i32,
        parent_w: i32,
        parent_h: i32,
    ) {
        let win_w = 700;
        let win_h = 630;
        let sx = parent_x + (parent_w - win_w) / 2;
        let sy = parent_y + (parent_h - win_h) / 2;

        let nav_w = 140;
        let btn_h = 50;
        // Content area dimensions (inside the scroll)
        let content_x = nav_w + 10;
        let content_w = win_w - nav_w - 30;
        let scroll_h = win_h - btn_h;

        let mut wind = window::Window::default()
            .with_size(win_w, win_h)
            .with_label(&t!("settings-title"));
        wind.set_pos(sx, sy);
        wind.make_resizable(false);
        wind.set_color(theme::BG_PRIMARY);

        // ── Left nav panel (absolute positioned) ────────────────────

        let mut nav = group::Flex::default()
            .with_pos(0, 0)
            .with_size(nav_w, scroll_h)
            .column();
        nav.set_color(theme::SURFACE);
        nav.set_frame(enums::FrameType::FlatBox);
        nav.set_margins(8, 12, 8, 12);
        nav.set_spacing(4);

        let mut nav_buttons: Vec<button::Button> = Vec::new();
        for &tab in Tab::ALL {
            let mut btn = button::Button::default().with_label(&tab.label());
            nav.fixed(&btn, 32);
            btn.set_frame(enums::FrameType::FlatBox);
            btn.set_label_size(13);
            btn.set_align(enums::Align::Left | enums::Align::Inside);
            nav_buttons.push(btn);
        }
        frame::Frame::default(); // spacer
        nav.end();

        // ── Scrollable content area ─────────────────────────────────

        let mut scroll = group::Scroll::default()
            .with_pos(nav_w, 0)
            .with_size(win_w - nav_w, scroll_h);
        scroll.set_scrollbar_size(8);
        scroll.set_color(theme::BG_PRIMARY);

        // General panel — first tab, visible initially.
        let general_flex = general::build(sender, &settings, content_x, 10, content_w);

        // Audio panel — Flex with explicit size/pos inside the scroll.
        // This is the pattern the original settings.rs used.
        let mut audio_flex = audio::build(
            sender,
            &settings,
            input_devices,
            output_devices,
            output_level,
            denoised_level,
            content_x,
            10,
            content_w,
        );
        audio_flex.hide();

        // Keyboard panel — same pattern, hidden initially
        let mut keyboard_flex = keybinds::build(sender, &settings, content_x, 10, content_w);
        keyboard_flex.hide();

        // Credits panel
        let mut credits_flex = credits::build(content_x, 10, content_w);
        credits_flex.hide();

        // About panel
        let mut about_flex = about::build(content_x, 10, content_w);
        about_flex.hide();

        scroll.end();

        // ── Bottom buttons ──────────────────────────────────────────

        let mut btn_row = group::Flex::default()
            .with_pos(0, scroll_h)
            .with_size(win_w, btn_h)
            .row();
        btn_row.set_margins(20, 8, 20, 8);
        btn_row.set_spacing(10);
        btn_row.set_color(theme::BG_PRIMARY);
        btn_row.set_frame(enums::FrameType::FlatBox);

        frame::Frame::default(); // left spacer
        let mut reset_btn = controls::button(&t!("reset-defaults"));
        btn_row.fixed(&reset_btn, 180);
        let mut save_btn = controls::accent_button(&t!("save-and-close"), theme::ACCENT);
        btn_row.fixed(&save_btn, 180);
        frame::Frame::default(); // right spacer
        btn_row.end();

        wind.end();
        wind.show();

        // ── Tab switching ───────────────────────────────────────────

        // Collect the switchable panels.  We use group::Flex which
        // implements WidgetExt, so show/hide work fine.
        let panels: Vec<group::Flex> = vec![
            general_flex.clone(),
            audio_flex.clone(),
            keyboard_flex.clone(),
            credits_flex.clone(),
            about_flex.clone(),
        ];
        let buttons_rc = std::rc::Rc::new(std::cell::RefCell::new(nav_buttons.clone()));

        style_nav_buttons(&nav_buttons, 0);

        for (idx, btn) in nav_buttons.iter_mut().enumerate() {
            let panels = panels.clone();
            let buttons = buttons_rc.clone();
            let mut scroll = scroll.clone();
            btn.set_callback(move |_| {
                for (i, p) in panels.iter().enumerate() {
                    let mut flex = p.clone();
                    if i == idx {
                        flex.show();
                    } else {
                        flex.hide();
                    }
                }
                // Reset scroll position when switching tabs
                scroll.scroll_to(0, 0);
                style_nav_buttons(&buttons.borrow(), idx);
                app::redraw();
            });
        }

        // ── Callbacks ───────────────────────────────────────────────

        let s = sender;
        wind.set_callback(move |w| {
            if app::event() == enums::Event::Close {
                s.send(UiMessage::SettingsWindowClosed);
                w.hide();
            }
        });

        let s = sender;
        let mut wind_clone = wind.clone();
        save_btn.set_callback(move |_| {
            s.send(UiMessage::SaveSettings);
            s.send(UiMessage::SettingsWindowClosed);
            wind_clone.hide();
        });

        let s = sender;
        reset_btn.set_callback(move |_| {
            s.send(UiMessage::ResetSettings);
        });
    }
}

fn style_nav_buttons(buttons: &[button::Button], active: usize) {
    for (i, btn) in buttons.iter().enumerate() {
        let mut b = btn.clone();
        if i == active {
            b.set_color(theme::ACCENT);
            b.set_label_color(theme::BUTTON_TEXT);
        } else {
            b.set_color(theme::SURFACE);
            b.set_label_color(theme::TEXT_SECONDARY);
        }
    }
}
