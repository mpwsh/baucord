// ui/connect.rs — Connection view (nickname + server address).
//
// Pure UI construction — all logic routes through UiMessages back to App.
//
// Since the single-window migration this builds a *view* (a Group hosted
// in the main window's Wizard) rather than its own window: a fixed-size
// form kept centered by flexible spacers, so it stays centered at any
// window size. The old borderless window, its drag-to-move handler and
// its custom ✕ button are gone — the real titlebar does that job now.

use fltk::{prelude::*, *};

use client::config::Settings;
use crate::ui::messages::UiMessage;
use ui::theme;
use i18n::t;
use ui::widgets::{controls, style};

/// Fixed size of the centered connect form.
/// FORM_H = sum of fixed child heights (305) + 9 gaps × 15 spacing.
const FORM_W: i32 = 380;
const FORM_H: i32 = 440;

/// Widget handles the App needs to keep after the view is built.
pub struct ConnectView {
    pub group: group::Group,
    pub error_frame: frame::Frame,
    /// Shows "saved device is not connected" warnings; empty when fine.
    pub device_warning: frame::Frame,
}

/// Build the connect view into the current group (the main window's
/// Wizard).
pub fn build_view(sender: app::Sender<UiMessage>, settings: &Settings) -> ConnectView {
    let view = group::Group::default_fill();

    // Flexible spacer frames on all four sides center the form.
    let mut outer = group::Flex::default_fill().column();
    frame::Frame::default(); // top spacer
    let mut mid = group::Flex::default().row();
    outer.fixed(&mid, FORM_H);
    frame::Frame::default(); // left spacer

    let mut col = group::Flex::default().column();
    mid.fixed(&col, FORM_W);
    col.set_spacing(15);

    // ── Title ────────────────────────────────────────────────────────

    let mut title = frame::Frame::default()
        .with_label("baucord")
        .with_align(enums::Align::Center);
    title.set_label_size(32);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 50);

    let mut subtitle = frame::Frame::default()
        .with_label(&t!("app-tagline"))
        .with_align(enums::Align::Center);
    subtitle.set_label_size(12);
    subtitle.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&subtitle, 20);

    // ── Error display ────────────────────────────────────────────────
    // Validation and disconnect reasons, right under the tagline.

    let mut error_frame = frame::Frame::default().with_align(enums::Align::Center);
    error_frame.set_label_color(theme::DANGER);
    error_frame.set_label_size(12);
    col.fixed(&error_frame, 25);

    // ── Nickname ─────────────────────────────────────────────────────

    let mut nick_frame = frame::Frame::default()
        .with_label(&t!("nickname"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    nick_frame.set_label_color(theme::TEXT_SECONDARY);
    nick_frame.set_label_size(11);
    col.fixed(&nick_frame, 20);

    let mut nick_input = input::Input::default();
    nick_input.set_value(&settings.user.nickname);
    style::style_input(&mut nick_input);
    col.fixed(&nick_input, 35);

    // ── Server address ───────────────────────────────────────────────

    let mut addr_frame = frame::Frame::default()
        .with_label(&t!("server-address"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    addr_frame.set_label_color(theme::TEXT_SECONDARY);
    addr_frame.set_label_size(11);
    col.fixed(&addr_frame, 20);

    let mut addr_input = input::Input::default();
    addr_input.set_value(&settings.server.address);
    style::style_input(&mut addr_input);
    col.fixed(&addr_input, 35);

    // ── Connect button ───────────────────────────────────────────────

    let mut connect_btn = controls::accent_button(&t!("connect"), theme::ACCENT);
    col.fixed(&connect_btn, 40);

    // ── Secondary actions ────────────────────────────────────────────
    // Settings works before connecting (devices, gains, keybinds) —
    // useful precisely when the saved device is gone.

    let mut actions = group::Flex::default().row();
    col.fixed(&actions, 30);
    actions.set_spacing(10);
    let mut settings_btn = controls::button(&t!("settings"));
    let mut quit_btn = controls::button(&t!("quit"));
    actions.end();

    // ── Missing-device warning ───────────────────────────────────────
    // Filled in by App when a device saved in settings isn't currently
    // connected (e.g. the USB mic was unplugged before launch).
    // Sits under the action row so the fix (Settings) is right above.

    let mut device_warning = frame::Frame::default().with_align(enums::Align::Center);
    device_warning.set_label_color(theme::WARNING);
    device_warning.set_label_size(11);
    col.fixed(&device_warning, 30);

    col.end();

    frame::Frame::default(); // right spacer
    mid.end();
    frame::Frame::default(); // bottom spacer
    outer.end();

    view.end();

    // ── Callbacks ────────────────────────────────────────────────────

    let s = sender;
    connect_btn.set_callback(move |_| {
        let nickname = nick_input.value();
        let server = addr_input.value();
        if nickname.trim().is_empty() {
            s.send(UiMessage::ShowError(t!("error-nickname-empty")));
        } else {
            s.send(UiMessage::Connect { nickname, server });
        }
    });

    let s = sender;
    settings_btn.set_callback(move |_| {
        s.send(UiMessage::OpenSettings);
    });

    let s = sender;
    quit_btn.set_callback(move |_| {
        s.send(UiMessage::Shutdown);
    });

    ConnectView {
        group: view,
        error_frame,
        device_warning,
    }
}
