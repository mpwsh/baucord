// settingsui.rs — the settings window: populate, wire, and the two
// live loops (8 Hz level meters, keybind capture).

use std::{sync::atomic::Ordering, time::Duration};

use client::{
    config::{build_bind_string, BindableAction, ServerEntry},
    voice::VoiceControl,
};
use i18n::t;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use ui::{KeybindRow, ServerRow, SettingsLogic, SettingsState, SettingsWindow};

use crate::app::{self, AppState};

/// Nav index of the Servers tab (Profile 0, General 1, Audio 2,
/// Keyboard 3, Servers 4, About 5) — the connect view's pencil jumps
/// here.
pub const TAB_SERVERS: i32 = 4;

/// The name-color palette (spec §7.1): sixteen colors that all pass
/// 3:1 on the chat background (#1A1D21, measured 4.9–15.5) and none
/// in the gold band (hue 38°–65°, saturation ≥ 0.35), which the server
/// reserves for admins.
pub const NAME_COLORS: [u32; 16] = [
    0x00E0_6C75, // red
    0x00FF_8A3D, // orange
    0x00F4_845F, // coral
    0x00C3_E88D, // lime
    0x007E_D321, // green
    0x002B_AC76, // emerald
    0x004E_CDC4, // teal
    0x0056_CCF2, // sky
    0x004A_9AE0, // blue
    0x0079_86CB, // indigo
    0x00B3_88FF, // violet
    0x00E0_7BE0, // orchid
    0x00FF_80AB, // pink
    0x00A3_B18A, // sage
    0x00B0_BEC5, // silver
    0x00F5_F5F5, // white
];

/// The theme's gold: on the palette only for admins, since the server
/// refuses the band to anyone else (spec §7.1).
pub const GOLD: u32 = 0x00E5_C158;

/// The palette this session may pick from.
fn palette(app: &AppState) -> Vec<u32> {
    let mut colors = NAME_COLORS.to_vec();
    if app.our_role_and_level().0 == app_core::roster::Role::Admin {
        colors.push(GOLD);
    }
    colors
}

/// The "Images you send" choices: label and budget in KB; the last
/// entry is the wire cap.
pub const IMAGE_SIZES: [(&str, u32); 5] = [
    ("512 KB", 512),
    ("1 MB", 1024),
    ("2 MB", 2048),
    ("5 MB", 5120),
    ("image-size-max", 10 * 1024),
];

/// The "Text size" choices: label key and the multiplier on every
/// font token.
pub const TEXT_SIZES: [(&str, f32); 4] = [
    ("text-size-small", 0.9),
    ("text-size-default", 1.0),
    ("text-size-large", 1.15),
    ("text-size-larger", 1.3),
];

/// `"ip:port"` → (ip, port). Splits on the *last* colon so
/// `[::1]:5000` keeps its bracketed v6 host; no colon → all ip.
fn split_address(address: &str) -> (String, String) {
    match address.rsplit_once(':') {
        Some((ip, port)) => (ip.to_string(), port.to_string()),
        None => (address.to_string(), String::new()),
    }
}

/// (ip, port) → `"ip:port"`, degrading gracefully around blanks.
fn join_address(ip: &str, port: &str) -> String {
    let (ip, port) = (ip.trim(), port.trim());
    if port.is_empty() {
        ip.to_string()
    } else {
        format!("{ip}:{port}")
    }
}

/// The Servers tab's selected row as a usable index, if any.
fn selected_server(app: &AppState) -> Option<usize> {
    let win = app.settings_win.as_ref()?;
    let index = usize::try_from(win.global::<SettingsState>().get_server_index()).ok()?;
    (index < app.settings.servers.len()).then_some(index)
}

/// Rebuild the Servers tab's row model from settings.
fn push_server_rows(app: &AppState) {
    let Some(win) = &app.settings_win else { return };
    let default_pos = app
        .settings
        .servers
        .iter()
        .position(|e| e.name == app.settings.server)
        .unwrap_or(0);
    let rows: Vec<ServerRow> = app
        .settings
        .servers
        .iter()
        .enumerate()
        .map(|(i, e)| ServerRow {
            name: e.name.as_str().into(),
            address: e.address.as_str().into(),
            is_default: i == default_pos,
        })
        .collect();
    win.global::<SettingsState>()
        .set_servers(ModelRc::new(VecModel::from(rows)));
}

/// One line under the identity buttons: what just happened.
fn set_identity_status(app: &AppState, text: &str) {
    if let Some(win) = &app.settings_win {
        win.global::<SettingsState>()
            .set_identity_status(text.into());
    }
}

/// Load `avatar.png` into the preview, or clear it.
pub fn refresh_avatar(app: &mut AppState) {
    // One decode, shared by the settings preview and the rows.
    app.reload_own_avatar();
    app.update_user_list();
    let Some(win) = &app.settings_win else { return };
    let s = win.global::<SettingsState>();
    s.set_has_avatar(app.own_avatar.is_some());
    s.set_avatar(app.own_avatar.clone().unwrap_or_default());
}

/// "On <server>: Member · Level 3", from what the session knows.
pub fn refresh_role_line(app: &AppState) {
    let Some(win) = &app.settings_win else { return };
    let s = win.global::<SettingsState>();
    s.set_connected(app.connected);
    let line = if app.connected {
        let (role, level) = app.our_role_and_level();
        let role = match role {
            app_core::roster::Role::Guest => t!("role-guest"),
            app_core::roster::Role::Member => t!("role-member"),
            app_core::roster::Role::Organizer => t!("role-organizer"),
            app_core::roster::Role::Admin => t!("role-admin"),
            app_core::roster::Role::Unknown => "…".to_string(),
        };
        if level > 0 {
            t!(
                "profile-role-line",
                "server" => app.server_name.as_str(),
                "role" => role.as_str(),
                "level" => u32::from(level)
            )
        } else {
            t!(
                "profile-role-line-no-level",
                "server" => app.server_name.as_str(),
                "role" => role.as_str()
            )
        }
    } else {
        t!("profile-not-connected")
    };
    s.set_role_line(line.into());
}

/// The identity line: this device's user id (its key's hash) and
/// where the key file lives.
pub fn refresh_identity(app: &AppState) {
    let Some(win) = &app.settings_win else { return };
    let s = win.global::<SettingsState>();
    let id = app.client_key.as_ref().map_or_else(
        || t!("identity-missing"),
        |key| app::short_id(&key.user_id()),
    );
    s.set_identity_id(id.into());
    s.set_identity_path(
        client::config::client_key_path()
            .display()
            .to_string()
            .into(),
    );
}

/// Fill the editor fields from the selected entry.
fn fill_server_editor(app: &AppState) {
    let Some(win) = &app.settings_win else { return };
    let s = win.global::<SettingsState>();
    let entry = selected_server(app).and_then(|i| app.settings.servers.get(i));
    let (ip, port) = entry.map_or_else(Default::default, |e| split_address(&e.address));
    s.set_server_name(entry.map_or("", |e| e.name.as_str()).into());
    s.set_server_ip(ip.into());
    s.set_server_port(port.into());
    s.set_server_invite_code(
        entry
            .and_then(|e| e.invite_code.as_deref())
            .unwrap_or("")
            .into(),
    );
    s.set_server_fingerprint(
        entry
            .and_then(|e| e.fingerprint.as_deref())
            .unwrap_or("")
            .into(),
    );
}

/// Refresh the Servers tab (rows + editor) after an external change —
/// the connect view's dropdown pick or an invite code used. No-op while
/// the settings window was never created.
pub fn refresh_servers(app: &AppState) {
    if app.settings_win.is_some() {
        push_server_rows(app);
        fill_server_editor(app);
    }
}

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
        win.global::<ui::Theme>()
            .set_scale(app.settings.ui.text_scale);
        let weak = win.as_weak();
        // Never armed: this window gains focus on every (re)open, so
        // the Focused trigger suffices.
        let armed = std::rc::Rc::new(std::cell::Cell::new(false));
        crate::app::repaint_on_reveal(win.window(), armed, move || {
            if let Some(w) = weak.upgrade() {
                w.set_repaint_flip(!w.get_repaint_flip());
            }
        });
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
    s.set_identity_status("".into());
    s.set_identity_regenerate_armed(false);
    refresh_identity(app);

    // General: text size, sounds, where the files are.
    let sizes: Vec<slint::SharedString> = TEXT_SIZES.iter().map(|(k, _)| t!(k).into()).collect();
    s.set_text_sizes(ModelRc::new(VecModel::from(sizes)));
    let size_index = TEXT_SIZES
        .iter()
        .position(|(_, scale)| (scale - settings.ui.text_scale).abs() < 0.01)
        .unwrap_or(1);
    s.set_text_size_index(i32::try_from(size_index).unwrap_or(1));
    s.set_config_path(
        client::config::settings_file_path()
            .display()
            .to_string()
            .into(),
    );
    // Startup / sounds / chat.
    s.set_auto_connect(settings.startup.auto_connect);
    s.set_start_minimized(settings.startup.start_minimized);
    s.set_tray_available(cfg!(feature = "tray"));
    s.set_check_updates(settings.startup.check_updates);
    s.set_sounds_enabled(settings.sounds.enabled);
    s.set_join_leave_sounds(settings.sounds.join_leave);
    s.set_mention_sound(settings.sounds.mention);
    s.set_dm_sound(settings.sounds.direct_message);
    s.set_show_timestamps(settings.ui.show_timestamps);
    s.set_compact_rows(settings.ui.compact_rows);
    let sizes: Vec<slint::SharedString> = IMAGE_SIZES
        .iter()
        .map(|(label, _)| {
            if label.starts_with("image-size") {
                t!(label).into()
            } else {
                (*label).into()
            }
        })
        .collect();
    s.set_image_sizes(ModelRc::new(VecModel::from(sizes)));
    let size_index = IMAGE_SIZES
        .iter()
        .position(|(_, kb)| *kb >= settings.ui.image_max_kb)
        .unwrap_or(IMAGE_SIZES.len() - 1);
    s.set_image_size_index(i32::try_from(size_index).unwrap_or(0));
    // Audio: transmission mode and the PTT key as bound.
    let modes: Vec<slint::SharedString> = vec![t!("audio-vad").into(), t!("audio-ptt").into()];
    s.set_transmission_modes(ModelRc::new(VecModel::from(modes)));
    s.set_transmission_index(i32::from(settings.audio.push_to_talk));
    s.set_ptt_bind(settings.bind_for(BindableAction::PushToTalk).into());

    // Profile (spec §7.1): what this device keeps.
    s.set_nickname(settings.user.nickname.as_str().into());
    s.set_initial(app_core::roster::initial_of(&settings.user.nickname).into());
    let palette = palette(app);
    s.set_is_admin(palette.len() > NAME_COLORS.len());
    let colors: Vec<slint::Color> = palette
        .iter()
        .map(|rgb| {
            let [_, r, g, b] = rgb.to_be_bytes();
            slint::Color::from_rgb_u8(r, g, b)
        })
        .collect();
    s.set_name_colors(ModelRc::new(VecModel::from(colors)));
    s.set_name_color_index(
        settings
            .profile
            .name_color
            .and_then(|c| palette.iter().position(|p| *p == c))
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(-1),
    );
    let styles: Vec<slint::SharedString> =
        ["name-style-normal", "name-style-bold", "name-style-italic"]
            .iter()
            .map(|k| t!(k).into())
            .collect();
    s.set_name_styles(ModelRc::new(VecModel::from(styles)));
    s.set_name_style_index(match settings.profile.name_style {
        client::config::NameStyle::Normal => 0,
        client::config::NameStyle::Bold => 1,
        client::config::NameStyle::Italic => 2,
    });
    // The picture was decoded at startup (and on every change).
    s.set_has_avatar(app.own_avatar.is_some());
    s.set_avatar(app.own_avatar.clone().unwrap_or_default());
    refresh_role_line(app);

    // Servers tab: rows + editor, selection starting on the default.
    let default_pos = app
        .settings
        .servers
        .iter()
        .position(|e| e.name == app.settings.server)
        .unwrap_or(0);
    s.set_server_index(i32::try_from(default_pos).unwrap_or(0));
    push_server_rows(app);
    fill_server_editor(app);

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

    // ── Profile (spec §7.1) ──────────────────────────────────────
    logic.on_nickname_edited(|text| {
        app::with_current(move |app| {
            app.settings.user.nickname = text.to_string();
            if let Some(win) = &app.settings_win {
                win.global::<SettingsState>()
                    .set_initial(app_core::roster::initial_of(&text).into());
            }
            // The connect screen shows the same name.
            app.main
                .global::<ui::State>()
                .set_nickname(text.as_str().into());
        });
    });
    // Look edits apply locally at once and reach the server together
    // on Save & close (one `SetProfile`, not one per click: the
    // server allows one change per 30 s).
    logic.on_name_color_selected(|index| {
        app::with_current(move |app| {
            let palette = palette(app);
            app.settings.profile.name_color = usize::try_from(index)
                .ok()
                .and_then(|i| palette.get(i).copied());
            app.profile_dirty = true;
            app.update_user_list();
        });
    });
    logic.on_name_style_selected(|index| {
        app::with_current(move |app| {
            app.settings.profile.name_style = match index {
                1 => client::config::NameStyle::Bold,
                2 => client::config::NameStyle::Italic,
                _ => client::config::NameStyle::Normal,
            };
            app.profile_dirty = true;
            app.update_user_list();
        });
    });
    logic.on_avatar_pick(|| {
        std::thread::spawn(|| {
            let picked = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                    .pick_file(),
            );
            let Some(handle) = picked else { return };
            let path = handle.path().to_path_buf();
            // Decode, crop and scale off the UI thread; only the
            // verdict crosses back.
            let result = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| app_core::images::avatar_png(&bytes).map_err(|e| e.to_string()))
                .and_then(|png| {
                    std::fs::write(client::config::avatar_path(), png).map_err(|e| e.to_string())
                });
            let _ = slint::invoke_from_event_loop(move || {
                app::with_current(move |app| match result {
                    Ok(()) => {
                        app.settings.profile.avatar = true;
                        refresh_avatar(app);
                        // A new picture is a new hash: uploaded on
                        // Save & close, then the profile points at it.
                        app.forget_avatar_blob();
                        app.profile_dirty = true;
                    }
                    Err(e) => app.show_error(&t!("profile-avatar-error", "error" => e.as_str())),
                });
            });
        });
    });
    logic.on_avatar_clear(|| {
        app::with_current(|app| {
            app.settings.profile.avatar = false;
            let _ = std::fs::remove_file(client::config::avatar_path());
            refresh_avatar(app);
            app.forget_avatar_blob();
            app.profile_dirty = true;
        });
    });

    // ── General ──────────────────────────────────────────────────
    logic.on_text_size_selected(|index| {
        app::with_current(move |app| {
            let scale = usize::try_from(index)
                .ok()
                .and_then(|i| TEXT_SIZES.get(i))
                .map_or(1.0, |(_, scale)| *scale);
            app.settings.ui.text_scale = scale;
            app.apply_text_scale();
        });
    });
    logic.on_sounds_enabled_changed(|on| {
        app::with_current(move |app| app.settings.sounds.enabled = on);
    });
    logic.on_join_leave_sounds_changed(|on| {
        app::with_current(move |app| app.settings.sounds.join_leave = on);
    });
    logic.on_mention_sound_changed(|on| {
        app::with_current(move |app| app.settings.sounds.mention = on);
    });
    logic.on_dm_sound_changed(|on| {
        app::with_current(move |app| app.settings.sounds.direct_message = on);
    });
    logic.on_auto_connect_changed(|on| {
        app::with_current(move |app| app.settings.startup.auto_connect = on);
    });
    logic.on_start_minimized_changed(|on| {
        app::with_current(move |app| app.settings.startup.start_minimized = on);
    });
    logic.on_check_updates_changed(|on| {
        app::with_current(move |app| app.settings.startup.check_updates = on);
    });
    logic.on_show_timestamps_changed(|on| {
        app::with_current(move |app| {
            app.settings.ui.show_timestamps = on;
            app.push_ui_prefs();
        });
    });
    logic.on_compact_rows_changed(|on| {
        app::with_current(move |app| {
            app.settings.ui.compact_rows = on;
            app.push_ui_prefs();
        });
    });
    logic.on_image_size_selected(|index| {
        app::with_current(move |app| {
            if let Some((_, kb)) = usize::try_from(index).ok().and_then(|i| IMAGE_SIZES.get(i)) {
                app.settings.ui.image_max_kb = *kb;
            }
        });
    });
    logic.on_transmission_selected(|index| {
        app::with_current(move |app| {
            app.settings.audio.push_to_talk = index == 1;
            app.apply_push_to_talk();
        });
    });

    // ── Servers tab ─────────────────────────────────────────────
    logic.on_server_selected(|_index| {
        // The row click already wrote SettingsState.server-index.
        app::with_current(|app| fill_server_editor(app));
    });

    logic.on_server_set_default(|index| {
        app::with_current(move |app| {
            if let Some(entry) = usize::try_from(index)
                .ok()
                .and_then(|i| app.settings.servers.get(i))
            {
                app.settings.server = entry.name.clone();
                app.settings.save();
            }
            push_server_rows(app);
            fill_server_editor(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_add(|| {
        app::with_current(|app| {
            // Unique display name: "New server", "New server 2", …
            let base = t!("server-new-name");
            let mut name = base.clone();
            let mut n = 1u32;
            while app.settings.servers.iter().any(|s| s.name == name) {
                n += 1;
                name = format!("{base} {n}");
            }
            app.settings.servers.push(ServerEntry {
                name,
                address: String::new(),
                password: None,
                invite_code: None,
                fingerprint: None,
                channel_passwords: std::collections::HashMap::new(),
            });
            app.settings.save();
            if let Some(win) = &app.settings_win {
                let last = app.settings.servers.len() - 1;
                win.global::<SettingsState>()
                    .set_server_index(i32::try_from(last).unwrap_or(0));
            }
            push_server_rows(app);
            fill_server_editor(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_remove(|| {
        app::with_current(|app| {
            // Never below one entry — connect needs a server to point at.
            if app.settings.servers.len() <= 1 {
                return;
            }
            let Some(index) = selected_server(app) else {
                return;
            };
            let removed = app.settings.servers.remove(index);
            if app.settings.server == removed.name {
                app.settings.server = app.settings.servers[0].name.clone();
            }
            app.settings.normalize_servers();
            app.settings.save();
            if let Some(win) = &app.settings_win {
                let clamped = index.min(app.settings.servers.len() - 1);
                win.global::<SettingsState>()
                    .set_server_index(i32::try_from(clamped).unwrap_or(0));
            }
            push_server_rows(app);
            fill_server_editor(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_name_edited(|text| {
        app::with_current(move |app| {
            let Some(index) = selected_server(app) else {
                return;
            };
            let was_default = app
                .settings
                .servers
                .iter()
                .position(|s| s.name == app.settings.server)
                == Some(index);
            let name = text.trim().to_string();
            app.settings.servers[index].name = name.clone();
            if was_default {
                // The default pointer follows its entry's rename.
                app.settings.server = name;
            }
            // Rows only — rewriting the editor's text property while
            // the user types would fight the caret.
            push_server_rows(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_ip_edited(|_| {
        app::with_current(update_selected_address);
    });
    logic.on_server_port_edited(|_| {
        app::with_current(update_selected_address);
    });

    logic.on_server_fingerprint_edited(|text| {
        app::with_current(move |app| {
            let Some(index) = selected_server(app) else {
                return;
            };
            // Accept a bare fingerprint or a whole invite string
            // (`host:port#SHA256:…`); only a well-formed pin is kept.
            let trimmed = text.trim();
            let pin = trimmed
                .rsplit_once('#')
                .map_or(trimmed, |(_, fp)| fp)
                .trim();
            let valid = pin.parse::<transport::Fingerprint>().is_ok();
            app.settings.servers[index].fingerprint = valid.then(|| pin.to_string());
        });
    });
    logic.on_server_invite_edited(|text| {
        app::with_current(move |app| {
            let Some(index) = selected_server(app) else {
                return;
            };
            // A code or a whole link; anything else leaves the stored
            // code alone until the field reads as one.
            app.settings.servers[index].set_invite_input(&text);
        });
    });

    // ── Identity (spec §3.3) ────────────────────────────────────────
    logic.on_identity_export(|| {
        std::thread::spawn(|| {
            let picked = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .set_file_name("baucord-identity.key")
                    .save_file(),
            );
            if let Some(handle) = picked {
                let path = handle.path().to_path_buf();
                let _ = slint::invoke_from_event_loop(move || {
                    app::with_current(move |app| match client::config::export_client_key(&path) {
                        Ok(()) => set_identity_status(app, &t!("identity-exported")),
                        Err(e) => app.show_error(&e),
                    });
                });
            }
        });
    });
    logic.on_identity_import(|| {
        std::thread::spawn(|| {
            let picked = pollster::block_on(rfd::AsyncFileDialog::new().pick_file());
            if let Some(handle) = picked {
                let path = handle.path().to_path_buf();
                let _ = slint::invoke_from_event_loop(move || {
                    app::with_current(move |app| match client::config::import_client_key(&path) {
                        Ok(key) => {
                            app.client_key = Some(key);
                            set_identity_status(app, &t!("identity-imported"));
                            refresh_identity(app);
                        }
                        Err(e) => app.show_error(&e),
                    });
                });
            }
        });
    });
    logic.on_identity_regenerate(|| {
        app::with_current(|app| {
            let Some(win) = &app.settings_win else { return };
            let s = win.global::<SettingsState>();
            // First click arms, second acts: a new key is a new user.
            if !s.get_identity_regenerate_armed() {
                s.set_identity_regenerate_armed(true);
                return;
            }
            s.set_identity_regenerate_armed(false);
            match client::config::regenerate_client_key() {
                Ok(key) => {
                    app.client_key = Some(key);
                    set_identity_status(app, &t!("identity-regenerated"));
                    refresh_identity(app);
                }
                Err(e) => app.show_error(&e),
            }
        });
    });
    logic.on_identity_regenerate_disarm(|| {
        app::with_current(|app| {
            if let Some(win) = &app.settings_win {
                win.global::<SettingsState>()
                    .set_identity_regenerate_armed(false);
            }
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

/// Recompute the selected entry's `address` from the editor's IP and
/// port fields (both are read fresh — either one may have changed).
fn update_selected_address(app: &mut AppState) {
    let Some(index) = selected_server(app) else {
        return;
    };
    let Some(win) = &app.settings_win else { return };
    let s = win.global::<SettingsState>();
    let address = join_address(&s.get_server_ip(), &s.get_server_port());
    app.settings.servers[index].address = address;
    push_server_rows(app);
}

/// `index` 0 = system default; otherwise the (index-1)-th device.
/// Error-generic: only the Ok shape matters here, so the real crate's
/// error type (Box<dyn Error>) and any future change both fit.
fn device_at<E>(devices: &Result<Vec<(String, bool)>, E>, index: i32) -> Option<String> {
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
    let Ok(index) = usize::try_from(index) else {
        return;
    };
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
        (Key::F1, "F1"),
        (Key::F2, "F2"),
        (Key::F3, "F3"),
        (Key::F4, "F4"),
        (Key::F5, "F5"),
        (Key::F6, "F6"),
        (Key::F7, "F7"),
        (Key::F8, "F8"),
        (Key::F9, "F9"),
        (Key::F10, "F10"),
        (Key::F11, "F11"),
        (Key::F12, "F12"),
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
    let Some(i) = app.keybind_recording else {
        return;
    };
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
                && row.recording
            {
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
    let s = win.global::<SettingsState>();
    s.set_keybinds(ModelRc::new(VecModel::from(rows)));
    // The Audio tab quotes the push-to-talk key, and so does the mic
    // button's tooltip.
    s.set_ptt_bind(app.settings.bind_for(BindableAction::PushToTalk).into());
    app.apply_push_to_talk();
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
    // Collapse blank token fields and repair the default pointer
    // before the edits hit disk.
    app.settings.normalize_servers();
    app.settings.save();
    app.push_connect_servers();
    // A new nickname while connected goes to the server, which renames
    // us for everyone or says why not (spec §7.1); the look goes out
    // as one change.
    let wanted = app.settings.user.nickname.trim().to_string();
    if !wanted.is_empty() && app.session_name().is_some_and(|current| current != wanted) {
        app.request_rename(wanted);
    }
    if app.profile_dirty {
        app.profile_dirty = false;
        app.send_own_profile();
    }
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
        "Bartrix",
        "BDK",
        "ChichoV3",
        "Cholo",
        "EltotoPA",
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
    let exe_path =
        std::env::current_exe().map_or_else(|_| t!("unknown"), |p| p.display().to_string());
    let config_path = client::config::settings_file_path().display().to_string();
    let head = format!(
        "baucord {}\nhttps://baucord.com\n\n{} {}\n{} {}",
        app::CURRENT_VERSION,
        t!("about-running-from"),
        exe_path,
        t!("about-config-file"),
        config_path,
    );
    let tech: Vec<String> = [
        "tech-rust",
        "tech-slint",
        "tech-quinn",
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
