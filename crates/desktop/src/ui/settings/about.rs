// ui/settings/about.rs — About panel: version, paths, tech links.

use fltk::{prelude::*, *};
use ui::theme;
use ui::widgets::{controls, labels};

pub fn build(x: i32, y: i32, w: i32) -> group::Flex {
    let mut col = group::Flex::default()
        .with_pos(x, y)
        .with_size(w, 700)
        .column();
    col.set_spacing(4);

    labels::section_header(&mut col, ">", "ABOUT");

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
        .unwrap_or_else(|_| "Unknown".to_string());

    let config_path = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .map(|d| d.join("settings.json").display().to_string())
        })
        .unwrap_or_else(|| "Unknown".to_string());

    path_label(&mut col, "Running from:", &exe_path);
    path_label(&mut col, "Config file:", &config_path);

    let mut open_btn = controls::button("Open install folder");
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

    labels::section_header(&mut col, ">", "BUILT WITH");

    let techs: &[(&str, &str)] = &[
        ("Rust - Programming Language", "https://rust-lang.org"),
        ("FLTK - User Interface", "https://www.fltk.org/"),
        ("Renet - Networking", "https://github.com/lucaspoffo/renet"),
        (
            "FFmpeg - Video Encoding/Decoding",
            "https://www.ffmpeg.org/",
        ),
        ("Xcap - Screen capture", "https://github.com/nashaofu/xcap"),
        ("CPAL - Audio I/O", "https://github.com/RustAudio/cpal"),
        ("Opus - Audio Codec", "https://opus-codec.org/"),
        (
            "RNNoise - Noise Suppression",
            "https://jmvalin.ca/demo/rnnoise/",
        ),
        (
            "Rubato - Audio Resampling",
            "https://github.com/HEnquist/rubato",
        ),
    ];

    for &(name, url) in techs {
        let mut name_lbl = frame::Frame::default()
            .with_label(name)
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
