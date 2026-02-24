// ui/connect.rs — Connection screen (nickname + server address).
//
// Pure UI construction — all logic routes through UiMessages back to App.

use fltk::{prelude::*, *};

use crate::config::Settings;
use crate::ui::messages::UiMessage;
use ui::theme;
use ui::widgets::{controls, style};

/// Build and show the connection screen.
///
/// Returns the window handle and error frame so App can track them
/// (hide on successful connect, show validation errors).
pub fn show(sender: app::Sender<UiMessage>, settings: &Settings) -> (window::Window, frame::Frame) {
    let (cx, cy) = settings.center_child(600, 480);
    let mut wind = window::Window::default()
        .with_size(600, 480)
        .with_pos(cx, cy)
        .with_label("Connect");
    wind.make_resizable(false);
    wind.set_color(theme::BG_PRIMARY);
    super::set_window_icon(&mut wind);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(20, 20, 20, 20);
    col.set_spacing(15);

    // ── Title ────────────────────────────────────────────────────────

    let mut title = frame::Frame::default()
        .with_label("baucord")
        .with_align(enums::Align::Center);
    title.set_label_size(32);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 50);

    let mut subtitle = frame::Frame::default()
        .with_label("We have discord at home")
        .with_align(enums::Align::Center);
    subtitle.set_label_size(12);
    subtitle.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&subtitle, 20);

    // ── Nickname ─────────────────────────────────────────────────────

    let mut nick_frame = frame::Frame::default()
        .with_label("Nickname")
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
        .with_label("Server Address:Port")
        .with_align(enums::Align::Left | enums::Align::Inside);
    addr_frame.set_label_color(theme::TEXT_SECONDARY);
    addr_frame.set_label_size(11);
    col.fixed(&addr_frame, 20);

    let mut addr_input = input::Input::default();
    addr_input.set_value(&settings.server.address);
    style::style_input(&mut addr_input);
    col.fixed(&addr_input, 35);

    // ── Error display ────────────────────────────────────────────────

    let mut error_frame = frame::Frame::default().with_align(enums::Align::Center);
    error_frame.set_label_color(theme::DANGER);
    error_frame.set_label_size(12);
    col.fixed(&error_frame, 25);

    // ── Connect button ───────────────────────────────────────────────

    let mut connect_btn = controls::accent_button("Connect", theme::ACCENT);
    col.fixed(&connect_btn, 40);

    col.end();
    wind.end();
    wind.show();

    // ── Callbacks ────────────────────────────────────────────────────

    let s = sender;
    connect_btn.set_callback(move |_| {
        let nickname = nick_input.value();
        let server = addr_input.value();
        if nickname.trim().is_empty() {
            s.send(UiMessage::ShowError("Nickname cannot be empty".into()));
        } else {
            s.send(UiMessage::Connect { nickname, server });
        }
    });

    // Window close → clean shutdown (don't let FLTK silently hide
    // the window while worker threads are still running).
    let s = sender;
    wind.set_callback(move |w| {
        if app::event() == enums::Event::Close {
            s.send(UiMessage::Shutdown);
            w.hide();
        }
    });

    (wind, error_frame)
}

