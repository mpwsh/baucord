// settings/general.rs — General panel: language + server configuration.
//
// Same explicit size/pos-inside-scroll pattern as the other panels.
//
// The server fields live here (not on the connect page) because for
// this app they're deployment constants: set once, shared by the whole
// friend group, changed only when the server moves. The connect page
// shows a read-only note of the current address and is rebuilt when
// this window closes, so edits here surface there immediately.
//
// Edits apply to Settings live (per keystroke, like every other panel)
// and persist on window close; they take effect on the next connect —
// there's deliberately no live re-dial of an active session.

use fltk::{prelude::*, *};

use crate::ui::messages::UiMessage;
use client::config::Settings;
use i18n::t;
use ui::theme;

pub fn build(
    sender: app::Sender<UiMessage>,
    settings: &Settings,
    x: i32,
    y: i32,
    w: i32,
) -> group::Flex {
    let mut col = group::Flex::default()
        .with_pos(x, y)
        .with_size(w, 500)
        .column();
    col.set_spacing(12);

    let mut title = frame::Frame::default()
        .with_label(&t!("tab-general"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    title.set_label_size(16);
    title.set_label_font(enums::Font::HelveticaBold);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 30);

    // ── Language ─────────────────────────────────────────────────────

    let mut lang_label = frame::Frame::default()
        .with_label(&t!("language"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    lang_label.set_label_size(11);
    lang_label.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&lang_label, 20);

    let mut choice = menu::Choice::default();
    choice.set_color(theme::SURFACE);
    choice.set_text_color(theme::TEXT_PRIMARY);
    choice.set_text_size(13);
    col.fixed(&choice, 32);

    choice.add_choice(&t!("language-system"));
    for locale in i18n::LOCALES {
        choice.add_choice(locale.native_name);
    }
    let current = settings
        .language
        .as_deref()
        .and_then(|code| i18n::LOCALES.iter().position(|l| l.code == code))
        .map_or(0, |idx| idx as i32 + 1);
    choice.set_value(current);

    let s = sender;
    choice.set_callback(move |c| {
        let idx = c.value();
        let lang = if idx <= 0 {
            None
        } else {
            i18n::LOCALES
                .get((idx - 1) as usize)
                .map(|l| l.code.to_string())
        };
        s.send(UiMessage::SetLanguage(lang));
    });

    let mut note = frame::Frame::default()
        .with_label(&t!("language-note"))
        .with_align(enums::Align::Left | enums::Align::Inside | enums::Align::Wrap);
    note.set_label_size(11);
    note.set_label_color(theme::TEXT_MUTED);
    col.fixed(&note, 50);

    // ── Server ───────────────────────────────────────────────────────

    let mut server_title = frame::Frame::default()
        .with_label(&t!("settings-server"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    server_title.set_label_size(16);
    server_title.set_label_font(enums::Font::HelveticaBold);
    server_title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&server_title, 30);

    let mut addr_label = frame::Frame::default()
        .with_label(&t!("server-address"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    addr_label.set_label_size(11);
    addr_label.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&addr_label, 20);

    let mut addr_input = input::Input::default();
    addr_input.set_value(&settings.server.address);
    addr_input.set_color(theme::SURFACE);
    addr_input.set_text_color(theme::TEXT_PRIMARY);
    addr_input.set_text_size(13);
    col.fixed(&addr_input, 32);

    // Live per-keystroke updates into Settings, matching how the other
    // panels apply their values; persistence happens on window close.
    addr_input.set_trigger(enums::CallbackTrigger::Changed);
    let s = sender;
    addr_input.set_callback(move |i| {
        s.send(UiMessage::SetServerAddress(i.value()));
    });

    let mut url_label = frame::Frame::default()
        .with_label(&t!("server-token-url"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    url_label.set_label_size(11);
    url_label.set_label_color(theme::TEXT_SECONDARY);
    col.fixed(&url_label, 20);

    let mut url_input = input::Input::default();
    url_input.set_value(settings.server.token_url.as_deref().unwrap_or(""));
    url_input.set_color(theme::SURFACE);
    url_input.set_text_color(theme::TEXT_PRIMARY);
    url_input.set_text_size(13);
    col.fixed(&url_input, 32);

    url_input.set_trigger(enums::CallbackTrigger::Changed);
    let s = sender;
    url_input.set_callback(move |i| {
        let value = i.value();
        let trimmed = value.trim();
        let url = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        s.send(UiMessage::SetServerTokenUrl(url));
    });

    let mut server_note = frame::Frame::default()
        .with_label(&t!("server-config-note"))
        .with_align(enums::Align::Left | enums::Align::Inside | enums::Align::Wrap);
    server_note.set_label_size(11);
    server_note.set_label_color(theme::TEXT_MUTED);
    col.fixed(&server_note, 65);

    col.end();
    col
}
