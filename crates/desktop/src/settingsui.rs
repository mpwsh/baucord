// settingsui.rs — the settings window: populate, wire, and the two
// live loops (8 Hz level meters, keybind capture).

use std::sync::atomic::Ordering;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use client::config::{BindableAction, build_bind_string};
use client::voice::VoiceControl;
use i18n::t;
use ui::{KeybindRow, SettingsLogic, SettingsState, SettingsWindow};

use crate::app::{self, AppState};

/// Localized label for a bindable action.
fn action_label(action: BindableAction) -> String {
    match action {
        BindableAction::ToggleMute => t!("action-toggle-mute"),
        BindableAction::ToggleDeafen => t!("action-toggle-deafen"),
        BindableAction::ToggleScreenShare => t!("action-toggle-share"),
        BindableAction::PushToTalk => t!("action-push-to-talk"),
    }
}

/// The sensitivity slider's descriptive line (ported thresholds).
fn format_sensitivity(threshold: f32) -> String {
    let level = if threshold <= 0.010 {
        t!("sensitivity-very-high")
    } else if threshold <= 0.020 {
        t!("sensitivity-high")
    } else if threshold <= 0.040 {
        t!("sensitivity-medium")
    } else if threshold <= 0.065 {
        t!("sensitivity-low")
    } else {
        t!("sensitivity-very-low")
    };
    t!("audio-sensitivity-line",
        "name" => level,
        "threshold" => format!("{threshold:.3}"))
}

/// Open the settings window (creating it on first use).
pub fn open(app: &mut AppState) {
    if app.settings_win.is_none() {
        let win = match SettingsWindow::new() {
            Ok(w) => w,
            Err(e) => {
                app.show_error(&e.to_string());
                return;
            }
        };
        wire(&win);
        app.settings_win = Some(win);
    }

    populate(app);
    start_meter_timer(app);

    if let Some(win) = &app.settings_win {
        let _ = win.show();
    }
}

/// Fill every `SettingsState` property from current settings + devices.
pub fn populate(app: &mut AppState) {
    let Some(win) = &app.settings_win else { return };
    app::fill_tr(&win.global::<ui::Tr>());
    let s = win.global::<SettingsState>();
    let settings = &app.settings;

    // Language list: "System default" + native names.
    let mut langs: Vec<slint::SharedString> = vec![t!("language-system").into()];
    langs.extend(i18n::LOCALES.iter().map(|l| l.native_name.into()));
    let lang_index = settings
        .language
        .as_deref()
        .and_then(|code| i18n::LOCALES.iter().position(|l| l.code == code))
        .map_or(0, |i| i + 1);
    s.set_languages(ModelRc::new(VecModel::from(langs)));
    s.set_language_index(i32::try_from(lang_index).unwrap_or(0));

    s.set_server_address(settings.server.address.as_str().into());
    s.set_token_url(
        settings
            .server
            .token_url
            .as_deref()
            .unwrap_or("")
            .into(),
    );

    // Device lists: "System default" first, saved selection restored.
    let sys_default = t!("audio-system-default");
    let input_devices = audio::VoiceChat::list_input_devices().unwrap_or_default();
    let output_devices = audio::VoiceChat::list_output_devices().unwrap_or_default();
    let dev_model = |devices: &[(String, bool)]| -> Vec<slint::SharedString> {
        std::iter::once(sys_default.as_str().into())
            .chain(devices.iter().map(|(n, _)| n.as_str().into()))
            .collect()
    };
    let dev_index = |devices: &[(String, bool)], saved: Option<&str>| -> i32 {
        saved
            .and_then(|name| devices.iter().position(|(n, _)| n == name))
            .map_or(0, |i| i32::try_from(i + 1).unwrap_or(0))
    };
    s.set_input_devices(ModelRc::new(VecModel::from(dev_model(&input_devices))));
    s.set_output_devices(ModelRc::new(VecModel::from(dev_model(&output_devices))));
    s.set_input_index(dev_index(
        &input_devices,
        settings.audio.input.device.as_deref(),
    ));
    s.set_output_index(dev_index(
        &output_devices,
        settings.audio.output.device.as_deref(),
    ));

    s.set_input_gain(settings.audio.input.gain);
    s.set_output_gain(settings.audio.output.gain);
    s.set_gate_threshold(settings.audio.gate_threshold);
    s.set_sensitivity_line(format_sensitivity(settings.audio.gate_threshold).into());
    s.set_testing_mic(app.is_testing_mic);
    s.set_test_hint(
        if app.is_testing_mic {
            t!("audio-testing-hint")
        } else {
            t!("audio-test-hint")
        }
        .into(),
    );
    s.set_mic_level(0.0);
    s.set_out_level(0.0);
    s.set_capturing(false);

    // Keybinds.
    let rows: Vec<KeybindRow> = BindableAction::ALL
        .iter()
        .map(|&action| KeybindRow {
            action_label: action_label(action).into(),
            bind: settings.bind_for(action).into(),
            recording: false,
        })
        .collect();
    s.set_keybinds(ModelRc::new(VecModel::from(rows)));

    set_credits(&s);
    set_about(&s);
}

/// Wire `SettingsLogic` callbacks (once per window instance).
#[allow(clippy::too_many_lines)]
fn wire(win: &SettingsWindow) {
    let logic = win.global::<SettingsLogic>();

    logic.on_language_selected(|index| {
        app::with_current(move |app| {
            let lang = usize::try_from(index - 1)
                .ok()
                .and_then(|i| i18n::LOCALES.get(i))
                .map(|l| l.code.to_string());
            app.set_language(lang);
            // Re-fill this window's labels too (fill_tr covers the
            // main window; populate refreshes localized rows here).
            populate(app);
        });
    });

    logic.on_server_address_edited(|text| {
        app::with_current(move |app| {
            app.settings.server.address = text.to_string();
        });
    });
    logic.on_token_url_edited(|text| {
        app::with_current(move |app| {
            let trimmed = text.trim().to_string();
            app.settings.server.token_url = (!trimmed.is_empty()).then_some(trimmed);
        });
    });

    logic.on_input_device_selected(|index| {
        app::with_current(move |app| {
            let device = device_at(&audio::VoiceChat::list_input_devices(), index);
            app.set_input_device(device);
        });
    });
    logic.on_output_device_selected(|index| {
        app::with_current(move |app| {
            let device = device_at(&audio::VoiceChat::list_output_devices(), index);
            app.set_output_device(device);
        });
    });

    logic.on_input_gain_changed(|gain| {
        app::with_current(move |app| {
            app.settings.audio.input.gain = gain;
            app.voice_tx().send(VoiceControl::SetInputGain(gain)).ok();
        });
    });
    logic.on_output_gain_changed(|gain| {
        app::with_current(move |app| {
            app.settings.audio.output.gain = gain;
            app.voice_tx().send(VoiceControl::SetOutputGain(gain)).ok();
        });
    });
    logic.on_gate_threshold_changed(|threshold| {
        app::with_current(move |app| {
            app.settings.audio.gate_threshold = threshold;
            app.voice_tx()
                .send(VoiceControl::SetGateThreshold(threshold))
                .ok();
            if let Some(win) = &app.settings_win {
                win.global::<SettingsState>()
                    .set_sensitivity_line(format_sensitivity(threshold).into());
            }
        });
    });

    logic.on_toggle_test_mic(|| {
        app::with_current(|app| {
            let enable = !app.is_testing_mic;
            app.handle_test_microphone(enable);
            if let Some(win) = &app.settings_win {
                let s = win.global::<SettingsState>();
                s.set_testing_mic(enable);
                s.set_test_hint(
                    if enable {
                        t!("audio-testing-hint")
                    } else {
                        t!("audio-test-hint")
                    }
                    .into(),
                );
            }
        });
    });

    // ── Keybind capture ─────────────────────────────────────────
    logic.on_record_keybind(|index| {
        app::with_current(move |app| {
            app.keybind_recording = usize::try_from(index).ok();
            if let Some(win) = &app.settings_win {
                let s = win.global::<SettingsState>();
                set_row_recording(&s, index, true);
                s.set_capturing(true);
                win.invoke_focus_capture();
            }
        });
    });
    logic.on_capture_key(|text, ctrl, alt, shift| {
        app::with_current(move |app| finish_capture(app, &text, ctrl, alt, shift));
    });
    logic.on_capture_cancel(|| {
        app::with_current(cancel_capture);
    });
    logic.on_capture_clear(|| {
        app::with_current(|app| {
            if let Some(i) = app.keybind_recording {
                let action = BindableAction::ALL[i.min(BindableAction::ALL.len() - 1)];
                app.set_keybind(action, String::new());
                app.settings.save();
            }
            cancel_capture(app);
            populate_binds_only(app);
        });
    });

    logic.on_open_config_folder(|| {
        // Open the folder that actually holds settings.json — the exe
        // directory for portable installs, or the platform config dir
        // when the exe directory isn't writable (client::config picks).
        if let Some(dir) = client::config::settings_file_path().parent()
            && let Err(e) = open::that_detached(dir)
        {
            log::warn!("Failed to open config folder: {e}");
        }
    });

    logic.on_reset_defaults(|| {
        app::with_current(|app| {
            app.apply_reset_defaults();
            populate(app);
        });
    });

    logic.on_save_and_close(|| {
        app::with_current(close);
    });

    let weak = win.as_weak();
    win.window().on_close_requested(move || {
        let _ = &weak;
        app::with_current(close);
        slint::CloseRequestResponse::HideWindow
    });
}

/// `index` 0 = system default; otherwise the (index-1)-th device.
/// Error-generic: only the Ok shape matters here, so the real crate's
/// error type (Box<dyn Error>) and any future change both fit.
fn device_at<E>(
    devices: &Result<Vec<(String, bool)>, E>,
    index: i32,
) -> Option<String> {
    if index <= 0 {
        return None;
    }
    devices
        .as_ref()
        .ok()
        .and_then(|list| usize::try_from(index - 1).ok().and_then(|i| list.get(i)))
        .map(|(name, _)| name.clone())
}

fn set_row_recording(s: &SettingsState, index: i32, recording: bool) {
    let Ok(index) = usize::try_from(index) else { return };
    let rows = s.get_keybinds();
    if let Some(mut row) = rows.row_data(index) {
        row.recording = recording;
        rows.set_row_data(index, row);
    }
}

/// Map a Slint key event to a `global_hotkey::Code` name. Returns
/// `None` for keys that can't be a binding chord terminal (bare
/// modifiers etc.).
fn key_code_for(text: &str) -> Option<String> {
    use slint::platform::Key;
    let c = text.chars().next()?;

    // Bare modifier presses arrive as their PUA characters — ignore.
    for modifier in [
        Key::Control,
        Key::Shift,
        Key::Alt,
        Key::AltGr,
        Key::Meta,
        Key::CapsLock,
    ] {
        if c == char::from(modifier) {
            return None;
        }
    }

    // Function keys.
    let fkeys = [
        (Key::F1, "F1"), (Key::F2, "F2"), (Key::F3, "F3"), (Key::F4, "F4"),
        (Key::F5, "F5"), (Key::F6, "F6"), (Key::F7, "F7"), (Key::F8, "F8"),
        (Key::F9, "F9"), (Key::F10, "F10"), (Key::F11, "F11"), (Key::F12, "F12"),
    ];
    for (key, name) in fkeys {
        if c == char::from(key) {
            return Some(name.to_string());
        }
    }

    // Navigation & editing keys.
    let named = [
        (Key::UpArrow, "ArrowUp"),
        (Key::DownArrow, "ArrowDown"),
        (Key::LeftArrow, "ArrowLeft"),
        (Key::RightArrow, "ArrowRight"),
        (Key::Home, "Home"),
        (Key::End, "End"),
        (Key::PageUp, "PageUp"),
        (Key::PageDown, "PageDown"),
        (Key::Insert, "Insert"),
        (Key::Delete, "Delete"),
        (Key::Tab, "Tab"),
        (Key::Return, "Enter"),
    ];
    for (key, name) in named {
        if c == char::from(key) {
            return Some(name.to_string());
        }
    }

    match c {
        ' ' => Some("Space".to_string()),
        'a'..='z' => Some(format!("Key{}", c.to_ascii_uppercase())),
        'A'..='Z' => Some(format!("Key{c}")),
        '0'..='9' => Some(format!("Digit{c}")),
        '-' => Some("Minus".to_string()),
        '=' => Some("Equal".to_string()),
        ',' => Some("Comma".to_string()),
        '.' => Some("Period".to_string()),
        '/' => Some("Slash".to_string()),
        ';' => Some("Semicolon".to_string()),
        '\'' => Some("Quote".to_string()),
        '[' => Some("BracketLeft".to_string()),
        ']' => Some("BracketRight".to_string()),
        '\\' => Some("Backslash".to_string()),
        '`' => Some("Backquote".to_string()),
        _ => None,
    }
}

fn finish_capture(app: &mut AppState, text: &str, ctrl: bool, alt: bool, shift: bool) {
    let Some(i) = app.keybind_recording else { return };
    let Some(code) = key_code_for(text) else {
        // A bare modifier — keep recording.
        return;
    };
    let action = BindableAction::ALL[i.min(BindableAction::ALL.len() - 1)];
    let bind = build_bind_string(ctrl, alt, shift, &code);
    app.set_keybind(action, bind);
    app.settings.save();
    cancel_capture(app);
    populate_binds_only(app);
}

/// Leave recording mode without changing the bind.
fn cancel_capture(app: &mut AppState) {
    app.keybind_recording = None;
    if let Some(win) = &app.settings_win {
        let s = win.global::<SettingsState>();
        s.set_capturing(false);
        let rows = s.get_keybinds();
        for i in 0..rows.row_count() {
            if let Some(mut row) = rows.row_data(i)
                && row.recording {
                    row.recording = false;
                    rows.set_row_data(i, row);
                }
        }
    }
}

/// Refresh just the keybind rows (after a capture or clear).
fn populate_binds_only(app: &mut AppState) {
    let Some(win) = &app.settings_win else { return };
    let rows: Vec<KeybindRow> = BindableAction::ALL
        .iter()
        .map(|&action| KeybindRow {
            action_label: action_label(action).into(),
            bind: app.settings.bind_for(action).into(),
            recording: false,
        })
        .collect();
    win.global::<SettingsState>()
        .set_keybinds(ModelRc::new(VecModel::from(rows)));
}

/// 8 Hz meter loop while the window is open (the FLTK build's
/// `MeterUpdate` tick).
fn start_meter_timer(app: &mut AppState) {
    let (output_level, denoised_level) = app.voice_levels();
    app.meter_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(125),
        move || {
            let mic = f32::from_bits(denoised_level.load(Ordering::Relaxed));
            let out = f32::from_bits(output_level.load(Ordering::Relaxed));
            app::with_current(move |app| {
                if let Some(win) = &app.settings_win {
                    let s = win.global::<SettingsState>();
                    s.set_mic_level(mic);
                    s.set_out_level(out);
                }
            });
        },
    );
}

/// Close semantics (both the Save button and the OS close box):
/// cancel any recording, stop a running mic test, persist, hide.
fn close(app: &mut AppState) {
    cancel_capture(app);
    if app.is_testing_mic {
        app.handle_test_microphone(false);
        if let Some(win) = &app.settings_win {
            win.global::<SettingsState>().set_testing_mic(false);
        }
    }
    app.meter_timer.stop();
    app.settings.save();
    if let Some(win) = &app.settings_win {
        let _ = win.hide();
    }
}

/// Push the Credits tab roster (same names the FLTK panel carried).
/// Super-testers render gold in the UI, hence the structured models
/// instead of one preformatted text blob.
fn set_credits(s: &ui::SettingsState) {
    const SUPER_TESTERS: &[&str] = &["empe"];
    const TESTERS: &[&str] = &[
        "Bartrix", "BDK", "ChichoV3", "Cholo", "willybal", "Nachovsky",
        "naqel", "Nicky21", "oveer", "Patitutitu", "piksel", "Pin0",
        "Scryer",
    ];
    let to_model = |names: &[&str]| {
        ModelRc::new(VecModel::from(
            names
                .iter()
                .map(|n| slint::SharedString::from(*n))
                .collect::<Vec<_>>(),
        ))
    };
    s.set_credits_super_heading(t!("credits-super-testers").into());
    s.set_credits_super(to_model(SUPER_TESTERS));
    s.set_credits_beta_heading(t!("credits-beta-testers").into());
    s.set_credits_testers(to_model(TESTERS));
}

/// Push the About tab: head (version, url, paths) and the built-with
/// roster as separate blocks — the "open config location" button sits
/// between them, right under the config-file line.
fn set_about(s: &ui::SettingsState) {
    let exe_path = std::env::current_exe().map_or_else(|_| t!("unknown"), |p| p.display().to_string());
    let config_path = client::config::settings_file_path().display().to_string();
    let head = format!(
        "baucord {}\nhttps://baucord.mpw.sh\n\n{} {}\n{} {}",
        app::CURRENT_VERSION,
        t!("about-running-from"),
        exe_path,
        t!("about-config-file"),
        config_path,
    );
    let tech: Vec<String> = [
        "tech-rust",
        "tech-slint",
        "tech-renet",
        "tech-ffmpeg",
        "tech-xcap",
        "tech-cpal",
        "tech-opus",
        "tech-rnnoise",
        "tech-rubato",
    ]
    .iter()
    .map(|key| t!(key))
    .collect();
    s.set_about_head(head.into());
    s.set_about_tech(format!("{}\n{}", t!("about-built-with"), tech.join("\n")).into());
}
