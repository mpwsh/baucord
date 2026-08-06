// settings/general.rs — General panel: language selection.
//
// Same explicit size/pos-inside-scroll pattern as the other panels.

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
        .with_size(w, 220)
        .column();
    col.set_spacing(12);

    let mut title = frame::Frame::default()
        .with_label(&t!("tab-general"))
        .with_align(enums::Align::Left | enums::Align::Inside);
    title.set_label_size(16);
    title.set_label_font(enums::Font::HelveticaBold);
    title.set_label_color(theme::TEXT_PRIMARY);
    col.fixed(&title, 30);

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

    col.end();
    col
}
