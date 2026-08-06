// ui/settings/about.rs — About panel: version, paths, tech links.

use fltk::{prelude::*, *};
use i18n::t;
use ui::theme;
use ui::widgets::{controls, labels};

pub fn build(x: i32, y: i32, w: i32) -> group::Flex {
    let mut col = group::Flex::default()
        .with_pos(x, y)
        .with_size(w, 700)
        .column();
    col.set_spacing(4);

    labels::section_header(&mut col, ">", &t!("about-header"));

    let version = env!("CARGO_PKG_VERSION");
    let mut ver_label = frame::Frame::default()
        .with_label(&format!("baucord v{}", version))
        .with_align(enums::Align::Left | enums::Align::Inside);
    col.fixed(&ver_label, 24);
    ver_label.set_label_color(theme::ACCENT);
    ver_label.set_label_size(16);

    link_button(&mut col, "https://baucord.mpw.sh", "https://baucord.mpw.sh");

    spacer(&mut col, 4);

    let exe_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| t!("unknown"));

    let config_path = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .map(|d| d.join("settings.json").display().to_string())
        })
        .unwrap_or_else(|| t!("unknown"));

    path_label(&mut col, &t!("about-running-from"), &exe_path);
    path_label(&mut col, &t!("about-config-file"), &config_path);

    let mut open_btn = controls::button(&t!("about-open-folder"));
    col.fixed(&open_btn, 26);
    open_btn.set_frame(enums::FrameType::FlatBox);
    open_btn.set_color(theme::BG_TERTIARY);
    open_btn.set_label_color(theme::BUTTON_TEXT);
    open_btn.set_label_size(11);

    let exe_for_btn = exe_path.clone();
    open_btn.set_callback(move |_| {
        if let Some(parent) = std::path::Path::new(&exe_for_btn).parent() {
            let _ = open::that(parent);
        }
    });

    separator(&mut col);

    labels::section_header(&mut col, ">", &t!("about-built-with"));

    // (message id, url) — labels come from the locale files.
    let techs: &[(&str, &str)] = &[
        ("tech-rust", "https://rust-lang.org"),
        ("tech-fltk", "https://www.fltk.org/"),
        ("tech-renet", "https://github.com/lucaspoffo/renet"),
        ("tech-ffmpeg", "https://www.ffmpeg.org/"),
        ("tech-xcap", "https://github.com/nashaofu/xcap"),
        ("tech-cpal", "https://github.com/RustAudio/cpal"),
        ("tech-opus", "https://opus-codec.org/"),
        ("tech-rnnoise", "https://jmvalin.ca/demo/rnnoise/"),
        ("tech-rubato", "https://github.com/HEnquist/rubato"),
    ];

    for &(key, url) in techs {
        let mut name_lbl = frame::Frame::default()
            .with_label(&t!(key))
            .with_align(enums::Align::Left | enums::Align::Inside);
        col.fixed(&name_lbl, 18);
        name_lbl.set_label_color(theme::TEXT_PRIMARY);
        name_lbl.set_label_size(12);

        link_button(&mut col, url, url);
    }

    frame::Frame::default();
    col.end();
    col
}

fn path_label(col: &mut group::Flex, label: &str, path: &str) {
    let mut lbl = frame::Frame::default()
        .with_label(label)
        .with_align(enums::Align::Left | enums::Align::Inside);
    col.fixed(&lbl, 16);
    lbl.set_label_color(theme::TEXT_SECONDARY);
    lbl.set_label_size(11);

    let mut path_frame = frame::Frame::default()
        .with_label(path)
        .with_align(enums::Align::Left | enums::Align::Inside);
    col.fixed(&path_frame, 18);
    path_frame.set_label_color(theme::TEXT_PRIMARY);
    path_frame.set_label_size(11);
}

fn link_button(col: &mut group::Flex, label: &str, url: &str) {
    let mut btn = button::Button::default().with_label(label);
    col.fixed(&btn, 18);
    btn.set_frame(enums::FrameType::NoBox);
    btn.set_label_color(theme::ACCENT);
    btn.set_label_size(11);
    btn.set_align(enums::Align::Left | enums::Align::Inside);
    let url = url.to_string();
    btn.set_callback(move |_| {
        let _ = open::that(&url);
    });
}

fn separator(col: &mut group::Flex) {
    let mut sep = frame::Frame::default();
    col.fixed(&sep, 1);
    sep.set_frame(enums::FrameType::FlatBox);
    sep.set_color(theme::BG_TERTIARY);
    spacer(col, 4);
}

fn spacer(col: &mut group::Flex, h: i32) {
    let s = frame::Frame::default();
    col.fixed(&s, h);
}
