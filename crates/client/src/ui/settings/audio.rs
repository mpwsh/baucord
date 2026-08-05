// ui/settings/audio.rs — Audio settings panel: devices, input, output.
//
// Returns a Flex column with explicit size and position, placed directly
// inside the parent Scroll.  This matches the layout pattern FLTK needs.

use crate::config::Settings;
use crate::ui::messages::UiMessage;
use fltk::{prelude::*, *};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use ui::theme;
use ui::widgets::{controls, labels, layout, meter};

fn format_sensitivity(value: f64) -> String {
    let name = match value {
        v if v < 0.010 => "Very High",
        v if v < 0.020 => "High",
        v if v < 0.040 => "Medium",
        v if v < 0.065 => "Low",
        _ => "Very Low",
    };
    format!("Sensitivity: {name}  (threshold: {value:.3} RMS)")
}

/// Build the audio settings panel.
///
/// `x`, `y` — position inside the scroll.
/// `w` — available width (scroll width minus scrollbar).
///
/// Returns a Flex column; the height is determined by content.
#[allow(clippy::too_many_arguments)]
pub fn build(
    sender: app::Sender<UiMessage>,
    settings: &Settings,
    input_devices: Vec<(String, bool)>,
    output_devices: Vec<(String, bool)>,
    output_level: Arc<AtomicU32>,
    denoised_level: Arc<AtomicU32>,
    x: i32,
    y: i32,
    w: i32,
) -> group::Flex {
    let mut col = group::Flex::default()
        .with_pos(x, y)
        .with_size(w, 570)
        .column();
    col.set_spacing(4);

    // ── Devices ─────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f3a7}", "DEVICES");

    labels::field_label(&mut col, "Input Device (Microphone)");
    let mut input_choice = controls::dropdown(&mut col);
    let input_names: Vec<&str> = input_devices.iter().map(|(n, _)| n.as_str()).collect();
    controls::populate_dropdown(
        &mut input_choice,
        "System Default",
        &input_names,
        settings.audio.input.device.as_deref(),
    );

    labels::field_label(&mut col, "Output Device (Speakers/Headphones)");
    let mut output_choice = controls::dropdown(&mut col);
    let output_names: Vec<&str> = output_devices.iter().map(|(n, _)| n.as_str()).collect();
    controls::populate_dropdown(
        &mut output_choice,
        "System Default",
        &output_names,
        settings.audio.output.device.as_deref(),
    );

    labels::hint_label(
        &mut col,
        &format!(
            "{} input, {} output devices",
            input_devices.len(),
            output_devices.len()
        ),
    );

    layout::separator(&mut col);

    // ── Input ───────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f3a4}", "INPUT");

    labels::field_label(&mut col, "Capture Level");
    let threshold_rc = std::rc::Rc::new(std::cell::RefCell::new(settings.audio.gate_threshold));
    let vu_input = meter::gate(&mut col, denoised_level.clone(), threshold_rc.clone(), 22);

    labels::field_label(&mut col, "Input Gain");
    let mut input_gain_slider = controls::slider(
        &mut col,
        0.0,
        2.0,
        settings.audio.input.gain as f64,
        0.05,
        theme::ACCENT,
    );
    let input_gain_lbl = labels::hint_label(
        &mut col,
        &format!("Gain: {}%", (settings.audio.input.gain * 100.0) as i32),
    );

    labels::field_label(&mut col, "Input Sensitivity");
    let mut vad_slider = controls::slider(
        &mut col,
        0.005,
        0.10,
        settings.audio.gate_threshold as f64,
        0.001,
        theme::ACCENT,
    );
    let vad_sens_lbl = labels::hint_label(
        &mut col,
        &format_sensitivity(settings.audio.gate_threshold as f64),
    );
    labels::hint_label(&mut col, "Audio below the red line won't be transmitted");

    let mut test_mic_btn = controls::button("\u{1f3a4}  Test Microphone");
    col.fixed(&test_mic_btn, 30);

    let test_mic_info =
        labels::hint_label(&mut col, "Listen to your own mic (others won't hear you)");

    layout::separator(&mut col);

    // ── Output ──────────────────────────────────────────────────

    labels::section_header(&mut col, "\u{1f50a}", "OUTPUT");

    let vu_output = meter::vu(&mut col, output_level.clone(), 22, theme::CYAN);

    labels::field_label(&mut col, "Master Volume");
    let mut output_gain_slider = controls::slider(
        &mut col,
        0.0,
        2.0,
        settings.audio.output.gain as f64,
        0.05,
        theme::ACCENT,
    );
    let output_gain_lbl = labels::hint_label(
        &mut col,
        &format!("Volume: {}%", (settings.audio.output.gain * 100.0) as i32),
    );

    col.end();

    // ── Callbacks ───────────────────────────────────────────────

    let s = sender;
    let devs = input_devices.clone();
    input_choice.set_callback(move |c| {
        let idx = c.value() as usize;
        let device = if idx == 0 {
            None
        } else {
            devs.get(idx - 1).map(|(n, _)| n.clone())
        };
        s.send(UiMessage::SetInputDevice(device));
    });

    let s = sender;
    let devs = output_devices.clone();
    output_choice.set_callback(move |c| {
        let idx = c.value() as usize;
        let device = if idx == 0 {
            None
        } else {
            devs.get(idx - 1).map(|(n, _)| n.clone())
        };
        s.send(UiMessage::SetOutputDevice(device));
    });

    let s = sender;
    let mut lbl = input_gain_lbl.clone();
    input_gain_slider.set_callback(move |sl| {
        let val = sl.value() as f32;
        lbl.set_label(&format!("Gain: {}%", (val * 100.0) as i32));
        s.send(UiMessage::SetInputGain(val));
    });

    let s = sender;
    let mut lbl = output_gain_lbl.clone();
    output_gain_slider.set_callback(move |sl| {
        let val = sl.value() as f32;
        lbl.set_label(&format!("Volume: {}%", (val * 100.0) as i32));
        s.send(UiMessage::SetOutputGain(val));
    });

    let s = sender;
    let testing = std::rc::Rc::new(std::cell::RefCell::new(false));
    let testing_cb = testing.clone();
    let mut info_lbl = test_mic_info.clone();
    test_mic_btn.set_callback(move |btn| {
        let is_testing = !*testing_cb.borrow();
        *testing_cb.borrow_mut() = is_testing;

        if is_testing {
            btn.set_label("\u{1f534}  Stop Testing");
            btn.set_color(theme::ACCENT_HOVER);
            btn.set_label_color(theme::BUTTON_TEXT);
            info_lbl.set_label("Listening to your mic... others can't hear you");
            info_lbl.set_label_color(theme::WARNING);
        } else {
            btn.set_label("\u{1f3a4}  Test Microphone");
            btn.set_color(theme::BG_TERTIARY);
            btn.set_label_color(theme::TEXT_PRIMARY);
            info_lbl.set_label("Listen to your own mic (others won't hear you)");
            info_lbl.set_label_color(theme::TEXT_MUTED);
        }

        s.send(UiMessage::TestMicrophone(is_testing));
    });

    let s = sender;
    let threshold_update = threshold_rc.clone();
    let mut sens_lbl = vad_sens_lbl.clone();
    let mut vu_input_cb = vu_input.clone();
    vad_slider.set_callback(move |sl| {
        let val = sl.value() as f32;
        *threshold_update.borrow_mut() = val;
        sens_lbl.set_label(&format_sensitivity(sl.value()));
        vu_input_cb.redraw();
        s.send(UiMessage::SetGateThreshold(val));
    });

    let mut vu_in_t = vu_input.clone();
    let mut vu_out_t = vu_output.clone();
    let alive_probe = vu_input.clone();

    app::add_timeout3(0.125, move |handle| {
        // Stop when the settings window closes — repeating forever would
        // leak one 8 Hz timer per settings-open.
        if !alive_probe.window().map(|w| w.visible()).unwrap_or(false) {
            return;
        }
        vu_in_t.redraw();
        vu_out_t.redraw();
        app::repeat_timeout3(0.125, handle);
    });

    col
}
