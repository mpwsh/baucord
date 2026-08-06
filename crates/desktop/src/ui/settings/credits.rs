// ui/settings/credits.rs — Credits panel: testers and contributors.

use fltk::{prelude::*, *};
use i18n::t;
use ui::theme;
use ui::widgets::labels;

const GOLD: enums::Color = enums::Color::from_hex(0xDAA520);

pub fn build(x: i32, y: i32, w: i32) -> group::Flex {
    let mut col = group::Flex::default()
        .with_pos(x, y)
        .with_size(w, 500)
        .column();
    col.set_spacing(4);

    labels::section_header(&mut col, "\u{2139}", &t!("credits-header"));

    // ── Super beta testers (gold) ───────────────────────────────

    let mut super_header = frame::Frame::default()
        .with_label(&format!(">  {}", t!("credits-super-testers")))
        .with_align(enums::Align::Left | enums::Align::Inside);
    col.fixed(&super_header, 24);
    super_header.set_label_size(13);

    let super_testers = vec!["empe"];

    for name in &super_testers {
        let mut f = frame::Frame::default()
            .with_label(name)
            .with_align(enums::Align::Left | enums::Align::Inside);
        col.fixed(&f, 20);
        f.set_label_color(GOLD);
        f.set_label_size(12);
    }

    spacer(&mut col, 4);

    // ── Beta testers (2 columns) ────────────────────────────────

    labels::section_header(&mut col, ">", &t!("credits-beta-testers"));

    let mut testers = vec![
        "Bartrix",
        "BDK",
        "ChichoV3",
        "Cholo",
        "willybal",
        "Nachovsky",
        "naqel",
        "Nicky21",
        "oveer",
        "Patitutitu",
        "piksel",
        "Pin0",
        "Scryer",
    ];
    testers.sort_unstable_by_key(|t| t.to_lowercase());

    let mut i = 0;
    while i < testers.len() {
        let mut row = group::Flex::default().row();
        col.fixed(&row, 20);
        row.set_spacing(10);

        let mut left = frame::Frame::default()
            .with_label(testers[i])
            .with_align(enums::Align::Left | enums::Align::Inside);
        left.set_label_color(theme::TEXT_PRIMARY);
        left.set_label_size(12);

        if i + 1 < testers.len() {
            let mut right = frame::Frame::default()
                .with_label(testers[i + 1])
                .with_align(enums::Align::Left | enums::Align::Inside);
            right.set_label_color(theme::TEXT_PRIMARY);
            right.set_label_size(12);
        } else {
            frame::Frame::default();
        }

        row.end();
        i += 2;
    }

    // Bottom spacer
    frame::Frame::default();

    col.end();
    col
}

fn spacer(col: &mut group::Flex, h: i32) {
    let s = frame::Frame::default();
    col.fixed(&s, h);
}
