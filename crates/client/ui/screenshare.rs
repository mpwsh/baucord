use fltk::{prelude::*, *};
use ui::theme;
use ui::widgets::controls;
use video::{Quality, capture::list_monitors};

use crate::ui::messages::UiMessage;

pub fn show(sender: app::Sender<UiMessage>, parent: &window::Window) {
    let monitors = list_monitors().unwrap_or_default();
    if monitors.is_empty() {
        sender.send(UiMessage::ShowError("No monitors found".into()));
        return;
    }

    let w = 320;
    let h = 200;
    let px = parent.x() + (parent.w() - w) / 2;
    let py = parent.y() + (parent.h() - h) / 2;

    let mut wind = window::Window::default()
        .with_size(w, h)
        .with_label("Share Screen");
    wind.set_pos(px, py);
    wind.set_color(theme::BG_PRIMARY);
    wind.make_resizable(false);

    let mut col = group::Flex::default_fill().column();
    col.set_margins(20, 15, 20, 15);
    col.set_spacing(10);

    // Monitor picker
    let mut monitor_label = frame::Frame::default().with_label("Monitor");
    monitor_label.set_label_color(theme::TEXT_SECONDARY);
    monitor_label.set_label_size(12);
    monitor_label.set_align(enums::Align::Left | enums::Align::Inside);
    col.fixed(&monitor_label, 18);

    let mut monitor_choice = menu::Choice::default();
    for m in &monitors {
        let primary = if m.is_primary { " (primary)" } else { "" };
        monitor_choice.add_choice(&format!("{} — {}x{}{}", m.name, m.width, m.height, primary));
    }
    let primary_idx = monitors.iter().position(|m| m.is_primary).unwrap_or(0);
    monitor_choice.set_value(primary_idx as i32);
    col.fixed(&monitor_choice, 30);

    // Quality picker
    let mut quality_label = frame::Frame::default().with_label("Quality");
    quality_label.set_label_color(theme::TEXT_SECONDARY);
    quality_label.set_label_size(12);
    quality_label.set_align(enums::Align::Left | enums::Align::Inside);
    col.fixed(&quality_label, 18);

    // Built from Quality::ALL so new presets in the video crate appear
    // here automatically. Display order == ALL order == picker indices.
    let mut quality_choice = menu::Choice::default();
    for q in Quality::ALL {
        quality_choice.add_choice(&q.to_string());
    }
    let default_idx = Quality::ALL
        .iter()
        .position(|q| *q == Quality::FHD30)
        .unwrap_or(0);
    quality_choice.set_value(default_idx as i32);
    col.fixed(&quality_choice, 30);

    // Spacer
    frame::Frame::default();

    // Start button
    let mut start_btn = controls::accent_button("Start Sharing", theme::ACCENT);
    col.fixed(&start_btn, 35);

    col.end();
    wind.end();
    wind.show();

    let mut wind_clone = wind.clone();
    start_btn.set_callback(move |_| {
        let monitor_index = monitor_choice.value().max(0) as usize;
        let quality = Quality::ALL
            .get(quality_choice.value().max(0) as usize)
            .copied()
            .unwrap_or(Quality::FHD30);
        sender.send(UiMessage::StartScreenShare {
            quality,
            monitor_index,
        });
        wind_clone.hide();
    });

    // Close = cancel
    wind.set_callback(|w| {
        if app::event() == enums::Event::Close {
            w.hide();
        }
    });
}
