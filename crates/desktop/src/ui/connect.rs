// ui/connect.rs — Connection view (nickname + optional server password).
//
// Pure UI construction — all logic routes through UiMessages back to App.
//
// Since the single-window migration this builds a *view* (a Group hosted
// in the main window's Wizard) rather than its own window: a fixed-size
// form kept centered by flexible spacers, so it stays centered at any
// window size.
//
// The server address (and token URL for secure servers) moved to
// Settings → General: for this app they're deployment constants, not
// per-session choices — a friend's first run should be nickname +
// password + Connect. A read-only line under the form shows which
// server we'll dial so a failed connect is never a mystery, and the
// Settings button right below it is the way to change it. App rebuilds
// this view when the settings window closes, so the line stays current.

use fltk::{prelude::*, *};

use crate::ui::messages::UiMessage;
use client::config::Settings;
use i18n::t;
use ui::theme;
use ui::widgets::{controls, style};

/// Fixed size of the centered connect form.
/// FORM_H = sum of fixed child heights (325) + 10 gaps × 15 spacing.
const FORM_W: i32 = 380;
const FORM_H: i32 = 475;

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

    // ── Server password (optional) ───────────────────────────────────
    // The issuer's shared secret on secure servers; ignored by open
    // servers. Prefilled from settings so returning users don't retype.

    let mut pass_frame = frame::Frame::default()
        .with_label(&t!("server-password"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    pass_frame.set_label_color(theme::TEXT_SECONDARY);
    pass_frame.set_label_size(11);
    col.fixed(&pass_frame, 20);

    let mut pass_input = input::SecretInput::default();
    pass_input.set_value(settings.server.token_password.as_deref().unwrap_or(""));
    style::style_input(&mut pass_input);
    col.fixed(&pass_input, 35);

    // ── Target server note ───────────────────────────────────────────
    // Read-only: which server the Connect button will dial. The way to
    // change it is the Settings button right below.

    let mut server_note = frame::Frame::default()
        .with_label(&t!("connect-server-note", "server" => settings.server.address.as_str()))
        .with_align(enums::Align::Center);
    server_note.set_label_size(11);
    server_note.set_label_color(theme::TEXT_MUTED);
    col.fixed(&server_note, 20);

    // ── Connect button ───────────────────────────────────────────────

    let mut connect_btn = controls::accent_button(&t!("connect"), theme::ACCENT);
    col.fixed(&connect_btn, 40);

    // ── Secondary actions ────────────────────────────────────────────
    // Settings works before connecting (devices, gains, keybinds, and
    // now the server address) — useful precisely when something saved
    // no longer matches reality.

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
        let password = pass_input.value();
        if nickname.trim().is_empty() {
            s.send(UiMessage::ShowError(t!("error-nickname-empty")));
        } else {
            s.send(UiMessage::Connect { nickname, password });
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
