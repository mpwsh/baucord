// settingsui.rs — the Settings page: populate, wire, and the two live
// loops (the level meters, keybind capture).

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashSet,
    time::{Duration, Instant},
};

use app_core::{
    chat::human_size,
    logfile,
    meter::{Meter, Run as MeterRun},
};
use client::{
    config::{build_bind_string, BindableAction, ServerEntry, Settings},
    network::{NetworkControl, LOG_UPLOAD_MIN_INTERVAL},
    protocol::MAX_LOG_UPLOAD_BYTES,
    voice::VoiceControl,
};
use i18n::t;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use ui::{
    BlockedRow, KeybindRow, LicenseRow, MainWindow, ServerRow, SettingsLogic, SettingsState, State,
};

use crate::app::{self, AppState};

/// The Settings page: the main window's `SettingsPanel`, shown while
/// `State.settings-open` on every host (ANDROID-SPEC.md D22, D71).
/// Everything in this module reaches the `SettingsState` /
/// `SettingsLogic` globals through here.
#[derive(Clone, Copy)]
pub(crate) struct SettingsSurface<'a>(&'a MainWindow);

impl<'a> SettingsSurface<'a> {
    fn state(self) -> SettingsState<'a> {
        self.0.global::<SettingsState>()
    }

    fn logic(self) -> SettingsLogic<'a> {
        self.0.global::<SettingsLogic>()
    }

    fn tr(self) -> ui::Tr<'a> {
        self.0.global::<ui::Tr>()
    }

    /// Select a nav tab (`TAB_*`).
    pub(crate) fn set_tab(self, tab: i32) {
        let s = self.0.global::<State>();
        s.set_settings_tab(tab);
        // Asked for a tab (the connect view's pencil): the narrow page
        // goes straight to that section (D28).
        s.set_settings_section_open(true);
    }

    /// Whether anyone can see it: the page open in a window that is up
    /// and not minimized (or in the tray).
    fn showing(self) -> bool {
        let window = self.0.window();
        self.0.global::<State>().get_settings_open()
            && window.is_visible()
            && !window.is_minimized()
    }

    /// The selected nav tab (`TAB_*`).
    fn tab(self) -> i32 {
        self.0.global::<State>().get_settings_tab()
    }

    /// Whether the selected tab's contents are what shows. The narrow
    /// page (D28) is either its section list or one section: the tab
    /// stays selected behind the list.
    fn section_showing(self) -> bool {
        !self.0.get_narrow() || self.0.global::<State>().get_settings_section_open()
    }

    fn show(self) {
        let s = self.0.global::<State>();
        // The narrow page opens on its section list (D28).
        s.set_settings_section_open(false);
        s.set_settings_open(true);
    }

    fn hide(self) {
        self.0.global::<State>().set_settings_open(false);
        // The page held the keyboard focus: the composer takes it back
        // on the desktop; on a touch host the main screen's Back
        // handler does, or Android's Back would find none and close
        // the app.
        self.0.invoke_focus_composer();
    }
}

/// The surface to drive: always the main window's page.
pub(crate) fn surface(app: &AppState) -> SettingsSurface<'_> {
    SettingsSurface(&app.main)
}

/// What the page found when it opened (`open_on`); its Close puts the
/// page's part of it back (`discard_and_close`). Every control applies
/// at once, so closing without saving has to undo, not only skip the
/// save.
pub(crate) struct OpenedWith {
    settings: Settings,
    /// The picture file's bytes (`None`: no picture). Picking or
    /// clearing one writes the file at once.
    avatar: Option<Vec<u8>>,
    profile_dirty: bool,
    /// The page edited the server list: its list at open is put back.
    /// Untouched, the list stays as it is now, with whatever a
    /// connection pinned or spent meanwhile (`events.rs`).
    servers_edited: bool,
    /// The page picked a microphone or an output. Unpicked, the devices
    /// stay as they are now: one unplugged meanwhile (the poll fell
    /// back to the default) is not put back by Close (review 2026-10,
    /// P2-3).
    devices_picked: bool,
}

impl OpenedWith {
    fn now(app: &AppState) -> Self {
        Self {
            settings: app.settings.clone(),
            avatar: std::fs::read(client::config::avatar_path()).ok(),
            profile_dirty: app.profile_dirty,
            servers_edited: false,
            devices_picked: false,
        }
    }

    /// `now` with the page's fields as they were at open. What is not
    /// the page's stays as it is: the main window's layout (sidebar
    /// width, the Messages divider, hidden DMs), per-person volumes,
    /// the share dialog's defaults, the ducking notice's flag.
    fn restored_onto(&self, now: &Settings) -> Settings {
        let mut restored = self.settings.clone();
        restored.window.clone_from(&now.window);
        restored.peer_settings.clone_from(&now.peer_settings);
        restored.share.clone_from(&now.share);
        restored.audio.lowering_told = now.audio.lowering_told;
        restored.ui.messages_collapsed = now.ui.messages_collapsed;
        restored.ui.sidebar_width = now.ui.sidebar_width;
        restored.ui.messages_height = now.ui.messages_height;
        restored.ui.hidden_dms.clone_from(&now.ui.hidden_dms);
        if !self.servers_edited {
            restored.servers.clone_from(&now.servers);
            restored.server.clone_from(&now.server);
        }
        if !self.devices_picked {
            restored
                .audio
                .input
                .device
                .clone_from(&now.audio.input.device);
            restored
                .audio
                .output
                .device
                .clone_from(&now.audio.output.device);
        }
        restored
    }
}

/// A server entry was added, removed or edited on the page.
fn note_servers_edited(app: &mut AppState) {
    if let Some(opened) = &mut app.settings_opened {
        opened.servers_edited = true;
    }
}

/// The page picked a microphone or an output.
fn note_devices_picked(app: &mut AppState) {
    if let Some(opened) = &mut app.settings_opened {
        opened.devices_picked = true;
    }
}

/// A route's name as the picker shows it: the engine's English names
/// for the built-in ones translated, a headset's own name as it is.
fn route_label(name: &str) -> String {
    match name {
        audio::route::SPEAKER => t!("audio-route-speaker"),
        audio::route::EARPIECE => t!("audio-route-earpiece"),
        audio::route::WIRED => t!("audio-route-wired"),
        other => other.to_string(),
    }
}

/// Nav index of the Servers tab (Profile 0, General 1, Audio 2,
/// Sounds 3, Keyboard 4, Servers 5, Credits 6, About 7, Debug 8,
/// Licenses 9) — the connect view's pencil jumps here. Mirrors the `NavItem`s in
/// settings.slint.
pub const TAB_SERVERS: i32 = 5;
/// Nav index of the Audio tab, whose meters the settings timer drives.
const TAB_AUDIO: i32 = 2;
/// Nav index of the Debug tab, whose log view the settings timer
/// refreshes while it shows.
const TAB_DEBUG: i32 = 8;
const TAB_LICENSES: i32 = 9;

/// The Debug tab's log view: this many of the last lines…
const LOG_TAIL_LINES: usize = 100;
/// …read from at most the log's last this many bytes.
const LOG_TAIL_BYTES: usize = 128 * 1024;
/// The settings timer on the Audio tab: the meters' 20 readings a
/// second, held by `app_core::meter` and set only when a bar moved; the
/// bar itself glides to each one (`widgets.slint`). At 8 a second and
/// drawn as read they stepped (first iPhone look, 2026-10-12).
const METER_TICK: Duration = Duration::from_millis(50);
/// The Audio tab's echo-cancellation line, which changes when streams
/// reopen: looked at this often, not with every meter reading.
const AEC_TICK: Duration = Duration::from_millis(250);
/// …and on every other tab, or off screen: the Debug tab's log view
/// reads the file again this often.
const IDLE_TICK: Duration = Duration::from_secs(2);

thread_local! {
    /// The Audio tab's meters are on screen: since when, and what was
    /// seen of them, for the log line when they leave.
    static METER_RUN: Cell<Option<(Instant, MeterRun)>> = const { Cell::new(None) };
    /// When this run last sent its log: the server takes one per
    /// `LOG_UPLOAD_MIN_INTERVAL`, and the network thread drops a second
    /// one silently, so the button says so instead.
    static LOG_SENT: Cell<Option<Instant>> = const { Cell::new(None) };
}

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
    // The band follows the permission, not the role: a custom role
    // may hold it, and an "everything but gold" role may not.
    if client::permissions::has(app.session.permissions, client::permissions::GOLD_NAME) {
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

/// The "Log level" choices: label key and the value stored in
/// `settings.json` (`logging.level`).
pub const LOG_LEVELS: [(&str, &str); 5] = [
    ("log-level-error", "error"),
    ("log-level-warn", "warn"),
    ("log-level-info", "info"),
    ("log-level-debug", "debug"),
    ("log-level-trace", "trace"),
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
    let sf = surface(app);
    let index = usize::try_from(sf.state().get_server_index()).ok()?;
    (index < app.settings.servers.len()).then_some(index)
}

/// Rebuild the Servers tab's row model from settings.
fn push_server_rows(app: &AppState) {
    let sf = surface(app);
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
    sf.state().set_servers(ModelRc::new(VecModel::from(rows)));
}

/// One line under the identity buttons: what just happened.
fn set_identity_status(app: &AppState, text: &str) {
    let sf = surface(app);
    sf.state().set_identity_status(text.into());
}

/// Load `avatar.png` into the preview, or clear it.
pub fn refresh_avatar(app: &mut AppState) {
    // One decode, shared by the settings preview and the rows.
    app.reload_own_avatar();
    app.update_user_list();
    app.refresh_own_rows();
    let sf = surface(app);
    let s = sf.state();
    s.set_has_avatar(app.own_avatar.is_some());
    s.set_avatar(app.own_avatar.clone().unwrap_or_default());
}

/// "On <server>: Member · Level 3", from what the session knows.
pub fn refresh_role_line(app: &AppState) {
    let sf = surface(app);
    let s = sf.state();
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

/// The Profile tab's server sections (1.2): who this account blocked
/// on the connected server, and deleting the account there. Shown only
/// on a server that takes them.
pub fn refresh_safety(app: &AppState) {
    let sf = surface(app);
    let s = sf.state();
    let on = app.connected && app.session.safety.on;
    s.set_safety(on);
    let mut rows: Vec<BlockedRow> = app
        .session
        .safety
        .blocked
        .iter()
        .map(|(user, name)| BlockedRow {
            user_id: app::hex(user).into(),
            name: name.as_str().into(),
        })
        .collect();
    rows.sort_by_key(|r| r.name.to_lowercase());
    s.set_blocked(ModelRc::new(VecModel::from(rows)));
    s.set_account_server(
        if on {
            t!("account-delete-lead", "server" => app.server_name.as_str())
        } else {
            String::new()
        }
        .into(),
    );
    if !on {
        s.set_account_delete_armed(false);
        s.set_account_status("".into());
    }
}

/// The line under the delete button: what the server said.
pub fn set_account_status(app: &AppState, text: &str) {
    let sf = surface(app);
    let s = sf.state();
    s.set_account_delete_armed(false);
    s.set_account_status(text.into());
}

/// The identity line: this device's user id (its key's hash) and
/// where the key file lives.
pub fn refresh_identity(app: &AppState) {
    let sf = surface(app);
    let s = sf.state();
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
    let sf = surface(app);
    let s = sf.state();
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
/// the connect view's dropdown pick or an invite code used.
pub fn refresh_servers(app: &AppState) {
    push_server_rows(app);
    fill_server_editor(app);
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

/// Open the Settings page where it was left.
pub fn open(app: &mut AppState) {
    open_on(app, None);
}

/// Open the Settings page, on `tab` (`TAB_*`) if one is asked for.
pub fn open_on(app: &mut AppState, tab: Option<i32>) {
    // One page at a time: the admin panel's (`adminui`) gives way.
    crate::adminui::close(app);
    // What Close goes back to: taken when the page opens, not when it
    // is asked for again while open (the connect view's pencil).
    if app.settings_opened.is_none() {
        app.settings_opened = Some(OpenedWith::now(app));
    }
    populate(app);
    start_meter_timer(app);

    let sf = surface(app);
    sf.show();
    if let Some(tab) = tab {
        sf.set_tab(tab);
    }
    // Opened on the Audio tab: its microphone now, not a tick later —
    // and only once the tab asked for is the one selected, or a look
    // at the servers would open the microphone the Audio tab was left
    // with.
    sync_audio_tab(app);
}

/// Wire the page's callbacks — once, at startup (`app::run`).
pub(crate) fn wire_page(main: &MainWindow) {
    wire(SettingsSurface(main));
}

/// Close the page if it is open: another page is taking the window.
/// Only Save and close keeps what the page changed, so this does what
/// its Close does.
pub(crate) fn close_if_open(app: &mut AppState) {
    if app.main.global::<State>().get_settings_open() {
        discard_and_close(app);
    }
}

/// The page's plain switches, one row each: the `SettingsState`
/// setter, the `SettingsLogic` handler, and the field of `Settings`
/// both stand for (then what to refresh, where a change shows at
/// once). `populate` and `wire` each expand the table, so a switch
/// cannot be shown from one field and saved into another, and a new
/// one is a line here — it was a setter in one function and a closure
/// 350 lines further down (review P3-35).
macro_rules! switches {
    ($apply:ident) => {
        $apply! {
            set_auto_connect, on_auto_connect_changed => startup.auto_connect;
            set_start_minimized, on_start_minimized_changed => startup.start_minimized;
            set_check_updates, on_check_updates_changed => startup.check_updates;
            set_sounds_enabled, on_sounds_enabled_changed => sounds.enabled;
            set_join_leave_sounds, on_join_leave_sounds_changed => sounds.join_leave;
            set_mention_sound, on_mention_sound_changed => sounds.mention;
            set_dm_sound, on_dm_sound_changed => sounds.direct_message;
            set_share_started_sound, on_share_started_sound_changed => sounds.share_started;
            set_viewer_joined_sound, on_viewer_joined_sound_changed => sounds.viewer_joined;
            set_mute_toggle_sound, on_mute_toggle_sound_changed => sounds.mute_toggle;
            set_notify_messages, on_notify_messages_changed => notifications.messages;
            set_notify_first_join, on_notify_first_join_changed => notifications.first_join;
            set_show_timestamps, on_show_timestamps_changed => ui.show_timestamps, push_ui_prefs;
            set_compact_rows, on_compact_rows_changed => ui.compact_rows, push_ui_prefs;
        }
    };
}

/// Fill every `SettingsState` property from current settings + devices.
pub fn populate(app: &mut AppState) {
    let sf = surface(app);
    app::fill_tr(&sf.tr());
    let s = sf.state();
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
    s.set_account_delete_armed(false);
    s.set_account_status("".into());
    refresh_identity(app);
    refresh_safety(app);

    // General: text size, sounds, where the files are.
    let sizes: Vec<slint::SharedString> = TEXT_SIZES.iter().map(|(k, _)| t!(k).into()).collect();
    s.set_text_sizes(ModelRc::new(VecModel::from(sizes)));
    let size_index = TEXT_SIZES
        .iter()
        .position(|(_, scale)| (scale - settings.ui.text_scale).abs() < 0.01)
        .unwrap_or(1);
    s.set_text_size_index(i32::try_from(size_index).unwrap_or(1));
    let levels: Vec<slint::SharedString> = LOG_LEVELS.iter().map(|(k, _)| t!(k).into()).collect();
    s.set_log_levels(ModelRc::new(VecModel::from(levels)));
    let level_index = LOG_LEVELS
        .iter()
        .position(|(_, v)| *v == settings.logging.level)
        .unwrap_or(2);
    s.set_log_level_index(i32::try_from(level_index).unwrap_or(2));
    s.set_config_path(
        client::config::settings_file_path()
            .display()
            .to_string()
            .into(),
    );
    // Debug: what the last send or clear said belongs to that visit.
    s.set_debug_status("".into());
    refresh_log(app);
    // Startup / sounds / chat: the switches, each from its field.
    macro_rules! show {
        ($($set:ident, $on:ident => $($field:ident).+ $(, $then:ident)?;)*) => {
            $( s.$set(settings.$($field).+); )*
        };
    }
    switches!(show);
    s.set_tray_available(cfg!(feature = "tray"));
    s.set_notifications_available(crate::host::notifies());
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
    // Audio: transmission mode and the PTT key as bound. A phone has no
    // key to hold, so it gets voice activity alone.
    let modes: Vec<slint::SharedString> = if app::SINGLE_WINDOW_HOST {
        vec![t!("audio-vad").into()]
    } else {
        vec![t!("audio-vad").into(), t!("audio-ptt").into()]
    };
    s.set_transmission_modes(ModelRc::new(VecModel::from(modes)));
    s.set_transmission_index(i32::from(settings.audio.push_to_talk));
    s.set_ptt_bind(settings.bind_for(BindableAction::PushToTalk).into());
    // macOS without the Accessibility permission: the Keyboard tab
    // says what that costs and offers the prompt.
    s.set_keybinds_need_access(!crate::hotkeys::access_granted());
    // Echo cancellation: the picker in AecMode::ALL order, and what the
    // running session got (refreshed by the meter timer).
    let aec_modes: Vec<slint::SharedString> = audio::AecMode::ALL
        .iter()
        .map(|m| aec_mode_label(*m).into())
        .collect();
    s.set_aec_modes(ModelRc::new(VecModel::from(aec_modes)));
    s.set_aec_index(i32::try_from(settings.audio.echo_cancellation.index()).unwrap_or(0));
    let report = app.voice_aec().lock().ok().and_then(|g| g.clone());
    s.set_aec_status(aec_status(report.as_ref()).into());
    s.set_aec_lowering(
        report
            .as_ref()
            .is_some_and(audio::AecReport::lowering_others),
    );

    // Profile (spec §7.1): what this device keeps.
    s.set_nickname(settings.user.nickname.as_str().into());
    s.set_initial(app_core::roster::initial_of(&settings.user.nickname).into());
    let palette = palette(app);
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

    s.set_input_gain(settings.audio.input.gain);
    s.set_output_gain(settings.audio.output.gain);
    s.set_gate_threshold(settings.audio.gate_threshold);
    s.set_sensitivity_line(format_sensitivity(settings.audio.gate_threshold).into());
    s.set_testing_mic(app.is_testing_mic);
    s.set_test_hint(test_hint(app.is_testing_mic).into());
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
    // The licence list (hundreds of rows) when its tab shows, not on
    // every open (review 2026-10, P3-10).
    if sf.tab() == TAB_LICENSES {
        set_licenses(&s);
    }

    // The device poll's lists, when it has run: enumerating again here
    // was a round trip to every driver on the UI thread at each open
    // (P3-10).
    let names = |devices: &[(String, bool)]| devices.iter().map(|(n, _)| n.clone()).collect();
    let (inputs, outputs) =
        if app.last_input_devices.is_empty() && app.last_output_devices.is_empty() {
            (
                names(&audio::VoiceChat::list_input_devices().unwrap_or_default()),
                names(&audio::VoiceChat::list_output_devices().unwrap_or_default()),
            )
        } else {
            (
                names(&app.last_input_devices),
                names(&app.last_output_devices),
            )
        };
    refresh_devices(app, inputs, outputs);
}

/// The device pickers: "System default" first, the saved selection
/// restored. Filled with Settings and again whenever a device comes or
/// goes (`AppState::handle_devices_changed`), and the names shown are
/// kept: a pick is an index into the list on screen. Resolving it
/// against a fresh enumeration, as before, picked the neighbour of the
/// device the user clicked once something had been plugged in with
/// Settings open (review P3-35).
pub fn refresh_devices(app: &mut AppState, inputs: Vec<String>, outputs: Vec<String>) {
    let sf = surface(app);
    let s = sf.state();
    let sys_default = t!("audio-system-default");
    let dev_model = |devices: &[String]| -> Vec<slint::SharedString> {
        std::iter::once(sys_default.as_str().into())
            .chain(devices.iter().map(|n| n.as_str().into()))
            .collect()
    };
    // A phone's outputs are routes (audio::route): "Automatic"
    // first, the built-in ones in the user's language. The setting
    // keeps the engine's names; only the shown text changes.
    let output_model: Vec<slint::SharedString> = if app::SINGLE_WINDOW_HOST {
        std::iter::once(t!("audio-route-auto").into())
            .chain(outputs.iter().map(|n| route_label(n).into()))
            .collect()
    } else {
        dev_model(&outputs)
    };
    let dev_index = |devices: &[String], saved: Option<&str>| -> i32 {
        saved
            .and_then(|name| devices.iter().position(|n| n == name))
            .map_or(0, |i| i32::try_from(i + 1).unwrap_or(0))
    };
    s.set_input_devices(ModelRc::new(VecModel::from(dev_model(&inputs))));
    s.set_output_devices(ModelRc::new(VecModel::from(output_model)));
    s.set_input_index(dev_index(
        &inputs,
        app.settings.audio.input.device.as_deref(),
    ));
    s.set_output_index(dev_index(
        &outputs,
        app.settings.audio.output.device.as_deref(),
    ));
    app.picker_inputs = inputs;
    app.picker_outputs = outputs;
}

/// Wire `SettingsLogic` callbacks (once per surface).
#[allow(clippy::too_many_lines)]
fn wire(sf: SettingsSurface<'_>) {
    let logic = sf.logic();

    logic.on_language_selected(|index| {
        app::with_current(move |app| {
            let lang = usize::try_from(index - 1)
                .ok()
                .and_then(|i| i18n::LOCALES.get(i))
                .map(|l| l.code.to_string());
            app.set_language(lang);
            // Re-fill the labels the page builds itself (fill_tr covers the
            // Tr global; populate refreshes localized rows here).
            populate(app);
        });
    });

    // ── Profile (spec §7.1) ──────────────────────────────────────
    logic.on_nickname_edited(|text| {
        app::with_current(move |app| {
            app.settings.user.nickname = text.to_string();
            let sf = surface(app);
            sf.state()
                .set_initial(app_core::roster::initial_of(&text).into());
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
            app.refresh_own_rows();
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
            app.refresh_own_rows();
        });
    });
    logic.on_license_toggled(|entry| {
        app::with_current(move |app| {
            if let Ok(entry) = usize::try_from(entry) {
                toggle_license(&surface(app).state(), entry);
            }
        });
    });
    logic.on_avatar_pick(|| {
        let filter = (
            "Images".to_string(),
            ["png", "jpg", "jpeg", "webp"].map(String::from).to_vec(),
        );
        crate::host::pick_file(Some(filter), |path| {
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
    logic.on_log_level_selected(|index| {
        app::with_current(move |app| {
            let level = usize::try_from(index)
                .ok()
                .and_then(|i| LOG_LEVELS.get(i))
                .map_or("info", |(_, v)| *v);
            app.settings.logging.level = level.to_string();
            app::apply_log_level(&app.settings);
            log::info!("log level set to {level}");
        });
    });
    // ── Debug ──────────────────────────────────────────────────────
    // Deferred: the panel's `init` can run while the UI is being laid
    // out from inside a handler that already holds the app.
    logic.on_refresh_log(|| {
        slint::Timer::single_shot(Duration::ZERO, || app::with_current(|app| refresh_log(app)));
    });
    logic.on_send_log(|| {
        app::with_current(|app| {
            let status = send_log(app);
            let sf = surface(app);
            sf.state().set_debug_status(status.into());
            refresh_log(app);
        });
    });
    logic.on_copy_log(|| {
        app::with_current(|app| {
            let status = copy_log();
            let sf = surface(app);
            sf.state().set_debug_status(status.into());
            refresh_log(app);
        });
    });
    logic.on_clear_log(|| {
        app::with_current(|app| {
            let status = clear_log();
            let sf = surface(app);
            sf.state().set_debug_status(status.into());
            refresh_log(app);
        });
    });
    // The switches: each saves into the field `populate` showed it
    // from.
    macro_rules! save {
        ($($set:ident, $on:ident => $($field:ident).+ $(, $then:ident)?;)*) => {
            $(
                logic.$on(|on| {
                    app::with_current(move |app| {
                        app.settings.$($field).+ = on;
                        $( app.$then(); )?
                    });
                });
            )*
        };
    }
    switches!(save);
    logic.on_image_size_selected(|index| {
        app::with_current(move |app| {
            if let Some((_, kb)) = usize::try_from(index).ok().and_then(|i| IMAGE_SIZES.get(i)) {
                app.settings.ui.image_max_kb = *kb;
            }
        });
    });
    logic.on_transmission_selected(|index| {
        app::with_current(move |app| {
            app.settings.audio.push_to_talk = index == 1 && !app::SINGLE_WINDOW_HOST;
            app.apply_push_to_talk();
            app.refresh_hotkeys();
        });
    });
    logic.on_open_sound_settings(|| {
        if let Err(e) = crate::host::open_sound_settings() {
            log::warn!("sound settings: {e}");
        }
    });
    // The one place the system's prompt comes from (review P2-30).
    logic.on_ask_keybind_access(crate::hotkeys::ask_for_access);
    logic.on_aec_selected(|index| {
        app::with_current(move |app| {
            let mode = audio::AecMode::from_index(usize::try_from(index).unwrap_or(0));
            app.settings.audio.echo_cancellation = mode;
            app.voice_tx()
                .send(VoiceControl::SetEchoCancellation(mode))
                .ok();
        });
    });

    // ── Servers tab ─────────────────────────────────────────────
    logic.on_server_selected(|_index| {
        // The row click already wrote SettingsState.server-index.
        app::with_current(|app| fill_server_editor(app));
    });

    logic.on_server_set_default(|index| {
        app::with_current(move |app| {
            note_servers_edited(app);
            if let Some(entry) = usize::try_from(index)
                .ok()
                .and_then(|i| app.settings.servers.get(i))
            {
                app.settings.server = entry.name.clone();
                app.save_settings();
            }
            push_server_rows(app);
            fill_server_editor(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_add(|| {
        app::with_current(|app| {
            note_servers_edited(app);
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
                invite_code: None,
                fingerprint: None,
                channel_passwords: std::collections::HashMap::new(),
            });
            app.save_settings();
            let sf = surface(app);
            let last = app.settings.servers.len() - 1;
            sf.state()
                .set_server_index(i32::try_from(last).unwrap_or(0));
            push_server_rows(app);
            fill_server_editor(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_remove(|| {
        app::with_current(|app| {
            note_servers_edited(app);
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
            app.save_settings();
            let sf = surface(app);
            let clamped = index.min(app.settings.servers.len() - 1);
            sf.state()
                .set_server_index(i32::try_from(clamped).unwrap_or(0));
            push_server_rows(app);
            fill_server_editor(app);
            app.push_connect_servers();
        });
    });

    logic.on_server_name_edited(|text| {
        app::with_current(move |app| {
            note_servers_edited(app);
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
            if was_default {
                // The default pointer follows its entry's rename.
                app.settings.server = name.clone();
            }
            app.settings.servers[index].name = name;
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
            note_servers_edited(app);
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
            note_servers_edited(app);
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
        // No save dialog (Android): into Downloads, and the status line
        // says where — the user moves it to the other device from there.
        if !crate::host::HAS_SAVE_DIALOG {
            app::with_current(|app| {
                let key = client::config::client_key_path();
                let saved = std::fs::read(&key)
                    .map_err(|e| format!("no identity key at {}: {e}", key.display()))
                    .and_then(|bytes| {
                        crate::media::save_to_downloads("baucord-identity.key", &bytes)
                    });
                match saved {
                    Ok(saved) => {
                        set_identity_status(app, &t!("file-saved", "path" => saved.shown.as_str()));
                    }
                    Err(e) => app.show_error(&e),
                }
            });
            return;
        }
        crate::host::save_file("baucord-identity.key", None, |path| {
            let _ = slint::invoke_from_event_loop(move || {
                app::with_current(move |app| match client::config::export_client_key(&path) {
                    Ok(()) => set_identity_status(app, &t!("identity-exported")),
                    Err(e) => app.show_error(&e),
                });
            });
        });
    });
    logic.on_identity_import(|| {
        crate::host::pick_file(None, |path| {
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
        });
    });
    logic.on_identity_regenerate(|| {
        app::with_current(|app| {
            let sf = surface(app);
            let s = sf.state();
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
    // ── This server (1.2): unblock, delete the account ────────────
    logic.on_unblock(|hex| {
        app::with_current(move |app| {
            if let Some(user) = app::parse_hex_id(&hex) {
                app.unblock(user);
            }
        });
    });
    logic.on_account_delete(|| {
        app::with_current(|app| {
            let sf = surface(app);
            let s = sf.state();
            if !s.get_account_delete_armed() {
                s.set_account_delete_armed(true);
                s.set_account_status("".into());
                return;
            }
            s.set_account_delete_armed(false);
            s.set_account_status(t!("account-deleting").into());
            app.delete_account();
        });
    });
    logic.on_account_delete_disarm(|| {
        app::with_current(|app| {
            let sf = surface(app);
            sf.state().set_account_delete_armed(false);
        });
    });
    logic.on_identity_regenerate_disarm(|| {
        app::with_current(|app| {
            let sf = surface(app);
            sf.state().set_identity_regenerate_armed(false);
        });
    });

    logic.on_input_device_selected(|index| {
        app::with_current(move |app| {
            let device = device_at(&app.picker_inputs, index);
            note_devices_picked(app);
            app.set_input_device(device);
        });
    });
    logic.on_output_device_selected(|index| {
        app::with_current(move |app| {
            let device = device_at(&app.picker_outputs, index);
            note_devices_picked(app);
            app.set_output_device(device);
        });
    });

    logic.on_input_gain_changed(|gain| {
        app::with_current(move |app| {
            app.settings.audio.input.gain = gain;
            app.voice_tx().send(VoiceControl::SetInputGain(gain)).ok();
        });
    });
    logic.on_tab_shown(|| {
        app::with_current(|app| {
            let sf = surface(app);
            if sf.tab() == TAB_LICENSES && sf.state().get_licenses().row_count() == 0 {
                set_licenses(&sf.state());
            }
            sync_audio_tab(app);
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
            let sf = surface(app);
            sf.state()
                .set_sensitivity_line(format_sensitivity(threshold).into());
        });
    });

    logic.on_toggle_test_mic(|| {
        app::with_current(|app| {
            let enable = !app.is_testing_mic;
            app.handle_test_microphone(enable);
            let sf = surface(app);
            let s = sf.state();
            s.set_testing_mic(enable);
            s.set_test_hint(test_hint(enable).into());
        });
    });

    // ── Keybind capture ─────────────────────────────────────────
    logic.on_record_keybind(|index| {
        app::with_current(move |app| {
            app.keybind_recording = usize::try_from(index).ok();
            let sf = surface(app);
            let s = sf.state();
            set_row_recording(&s, index, true);
            // The panel takes the keyboard itself when this flips.
            s.set_capturing(true);
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
                app.save_settings();
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
            && let Err(e) = crate::host::open_target(dir)
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
    logic.on_discard_and_close(|| {
        app::with_current(discard_and_close);
    });
}

/// Recompute the selected entry's `address` from the editor's IP and
/// port fields (both are read fresh — either one may have changed).
fn update_selected_address(app: &mut AppState) {
    note_servers_edited(app);
    let Some(index) = selected_server(app) else {
        return;
    };
    let sf = surface(app);
    let s = sf.state();
    let address = join_address(&s.get_server_ip(), &s.get_server_port());
    app.settings.servers[index].address = address;
    push_server_rows(app);
}

/// `index` 0 = system default; otherwise the (index-1)-th of the
/// devices the picker is showing (`refresh_devices`).
fn device_at(shown: &[String], index: i32) -> Option<String> {
    let index = usize::try_from(index).ok()?.checked_sub(1)?;
    shown.get(index).cloned()
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
    app.save_settings();
    cancel_capture(app);
    populate_binds_only(app);
}

/// Leave recording mode without changing the bind.
fn cancel_capture(app: &mut AppState) {
    app.keybind_recording = None;
    let sf = surface(app);
    let s = sf.state();
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

/// Refresh just the keybind rows (after a capture or clear).
fn populate_binds_only(app: &mut AppState) {
    let sf = surface(app);
    let rows: Vec<KeybindRow> = BindableAction::ALL
        .iter()
        .map(|&action| KeybindRow {
            action_label: action_label(action).into(),
            bind: app.settings.bind_for(action).into(),
            recording: false,
        })
        .collect();
    let s = sf.state();
    s.set_keybinds(ModelRc::new(VecModel::from(rows)));
    // The Audio tab quotes the push-to-talk key, and so does the mic
    // button's tooltip.
    s.set_ptt_bind(app.settings.bind_for(BindableAction::PushToTalk).into());
    app.apply_push_to_talk();
}

/// The settings timer: the Audio tab's meters 20 times a second and
/// its echo-cancellation line 4 times, the Debug tab's log view every
/// 2 s, a 2 s look on the other tabs — and nothing minimized, in the
/// tray or with the app in the background. It ran at 8 Hz on every tab
/// for as long as Settings was open (review P2-40). `tab-shown`
/// re-paces it at once on a tab change ([`pace_meter_timer`]); coming
/// back on screen restarts it (`sync_audio_tab`, from `host_shown` and
/// `main_revealed`).
fn start_meter_timer(app: &mut AppState) {
    let levels = app.voice_levels();
    let aec = app.voice_aec();
    let (mut mic_bar, mut out_bar) = (Meter::new(), Meter::new());
    let mut read_at = Instant::now();
    let mut aec_at: Option<Instant> = None;
    app.meter_timer
        .start(slint::TimerMode::Repeated, METER_TICK, move || {
            app::with_current(|app| {
                if !pace_meter_timer(app) {
                    return;
                }
                // The permission is granted in System Settings, not
                // here: while the note about it shows, look whether it
                // has been, and watch the keys from then on.
                let state = surface(app).state();
                let granted = state.get_keybinds_need_access() && crate::hotkeys::access_granted();
                if granted {
                    state.set_keybinds_need_access(false);
                }
                if granted {
                    app.rearm_hotkeys();
                }
                let sf = surface(app);
                if !sf.section_showing() {
                    return;
                }
                match sf.tab() {
                    TAB_DEBUG => refresh_log(app),
                    TAB_AUDIO => {
                        let now = Instant::now();
                        let since = now.duration_since(read_at);
                        read_at = now;
                        // No engine reads as silence on the bars.
                        let heard = levels.meters();
                        let (mic, out) = heard.unwrap_or((0.0, 0.0));
                        let s = sf.state();
                        // A bar that did not move is not drawn again:
                        // a quiet room costs no frames.
                        let mic = mic_bar.read(mic, since);
                        let out = out_bar.read(out, since);
                        if let Some(level) = mic {
                            s.set_mic_level(level);
                        }
                        if let Some(level) = out {
                            s.set_out_level(level);
                        }
                        if let Some((began, mut run)) = METER_RUN.get() {
                            run.note(
                                now.duration_since(began),
                                heard.map(|(mic, _)| mic),
                                app.is_testing_mic,
                                mic.is_some() || out.is_some(),
                            );
                            METER_RUN.set(Some((began, run)));
                        }
                        if aec_at.is_some_and(|at| now.duration_since(at) < AEC_TICK) {
                            return;
                        }
                        aec_at = Some(now);
                        // Reopening the streams (a headset plugged in,
                        // the setting changed) changes this; property
                        // sets of an equal string cost nothing visible.
                        let report = aec.lock().ok().and_then(|g| g.clone());
                        let aec_line = aec_status(report.as_ref());
                        // Read again each time for the same reason, and
                        // one more: the way out is a Windows setting
                        // the user may have just changed.
                        let lowering = report
                            .as_ref()
                            .is_some_and(audio::AecReport::lowering_others);
                        if s.get_aec_status() != aec_line.as_str() {
                            s.set_aec_status(aec_line.into());
                        }
                        if s.get_aec_lowering() != lowering {
                            s.set_aec_lowering(lowering);
                        }
                    }
                    _ => {}
                }
            });
        });
}

/// Set the settings timer's pace for what is showing now, open or
/// close the Audio tab's microphone to match, and say whether Settings
/// is on screen at all.
fn pace_meter_timer(app: &mut AppState) -> bool {
    let sf = surface(app);
    let showing = app::host_visible() && sf.showing();
    let audio = showing && sf.tab() == TAB_AUDIO && sf.section_showing();
    let pace = if audio { METER_TICK } else { IDLE_TICK };
    if !showing {
        // Out of sight there is nothing to read: no tick at all, until
        // `sync_audio_tab` finds the page back (review 2026-10, P3-7).
        app.meter_timer.stop();
    } else if !app.meter_timer.running() {
        app.meter_timer.restart();
        app.meter_timer.set_interval(pace);
    } else if app.meter_timer.interval() != pace {
        app.meter_timer.set_interval(pace);
    }
    // The meter is the microphone's: while the tab shows, it is open —
    // not connected and in the lobby too (`set_audio_tab_open`).
    app.set_audio_tab_open(audio);
    note_meter_run(audio);
    showing
}

/// The meters came on screen or left it: start counting, or log what
/// the run came to.
fn note_meter_run(on_screen: bool) {
    match (on_screen, METER_RUN.get()) {
        (true, None) => METER_RUN.set(Some((Instant::now(), MeterRun::default()))),
        (false, Some((began, run))) => {
            METER_RUN.set(None);
            log::info!("settings: {}", run.summary(began.elapsed()));
        }
        _ => {}
    }
}

/// What is on screen changed — a tab, the narrow page's section, the
/// app back from the background: the meter's pace and its microphone
/// follow now.
pub(crate) fn sync_audio_tab(app: &mut AppState) {
    pace_meter_timer(app);
}

/// A microphone test ended without its button (the session it ran in
/// closed): the button and its hint say so.
pub(crate) fn test_ended(app: &AppState) {
    let sf = surface(app);
    let s = sf.state();
    s.set_testing_mic(false);
    s.set_test_hint(test_hint(false).into());
}

/// The line beside the Test microphone button. Where the test plays the
/// voice back late (a phone, `audio::LOOPBACK_DELAY`), it says so:
/// silence after speaking reads as a test that is broken.
fn test_hint(testing: bool) -> String {
    match (testing, audio::LOOPBACK_DELAY.is_zero()) {
        (false, _) => t!("audio-test-hint"),
        (true, true) => t!("audio-testing-hint"),
        (true, false) => t!("audio-testing-hint-delayed"),
    }
}

// ── Debug ───────────────────────────────────────────────────────────────

/// The Debug tab's log view: where the file is, how big, and its last
/// lines. A read of the file's last 128 KB; properties are set only
/// when they changed, so an idle log redraws nothing.
pub fn refresh_log(app: &AppState) {
    let sf = surface(app);
    let s = sf.state();
    let Some(path) = logfile::path() else {
        s.set_log_file_available(false);
        return;
    };
    s.set_log_file_available(true);
    let line = t!(
        "debug-log-file",
        "path" => path.display().to_string(),
        "size" => human_size(logfile::total_len(path))
    );
    if s.get_log_file_line() != line.as_str() {
        s.set_log_file_line(line.into());
    }
    let tail = logfile::tail_lines(path, LOG_TAIL_LINES, LOG_TAIL_BYTES)
        .unwrap_or_else(|e| t!("debug-read-failed", "error" => e.to_string()));
    if s.get_log_tail() != tail.as_str() {
        s.set_log_tail(tail.into());
    }
}

/// "Send log to server": the end of the log, up to the protocol's
/// cap, to the connected server (spec §9.1), which files it under its
/// identity folder's `logs/`. Says what happened under the buttons.
fn send_log(app: &AppState) -> String {
    let Some(path) = logfile::path() else {
        return t!("debug-no-log-file");
    };
    if !app.connected {
        return t!("debug-send-offline");
    }
    if let Some(left) = LOG_SENT
        .get()
        .and_then(|at| LOG_UPLOAD_MIN_INTERVAL.checked_sub(at.elapsed()))
    {
        return t!("debug-send-wait", "minutes" => left.as_secs().div_ceil(60).max(1));
    }
    // Who sent it and from what, for whoever opens the file.
    let header = format!(
        "# baucord {} ({}/{}), sent from Settings → Debug at {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
    );
    match logfile::upload_bytes(path, &header, MAX_LOG_UPLOAD_BYTES) {
        Err(e) => t!("debug-read-failed", "error" => e.to_string()),
        Ok(bytes) if bytes.len() <= header.len() + 1 => t!("debug-send-empty"),
        Ok(bytes) => {
            let size = human_size(bytes.len() as u64);
            app.network.tx.send(NetworkControl::SendLog(bytes)).ok();
            LOG_SENT.set(Some(Instant::now()));
            log::info!("log sent to the server from Settings → Debug ({size})");
            let server = if app.server_name.is_empty() {
                app.settings
                    .default_server()
                    .map(|s| s.name.clone())
                    .unwrap_or_default()
            } else {
                app.server_name.clone()
            };
            t!("debug-sent", "server" => server, "size" => size)
        }
    }
}

/// How much of the log "Copy log" takes: enough for a session's
/// stream and audio lines at info level (a 20-minute share was 143 KB),
/// small enough to paste into a message. Less on a phone, where the
/// whole clip crosses one binder transaction on Android (1 MB, the text
/// as UTF-16).
const LOG_COPY_BYTES: usize = if cfg!(feature = "desktop") {
    512 * 1024
} else {
    128 * 1024
};

/// "Copy log": the end of the log onto the clipboard, under a line
/// naming the build, the platform, when, and the file it came from — a
/// pasted log then says which machine and which build wrote it.
fn copy_log() -> String {
    let Some(path) = logfile::path() else {
        return t!("debug-no-log-file");
    };
    let header = format!(
        "# baucord {} ({}/{}), copied from Settings → Debug at {}, from {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
        path.display(),
    );
    match logfile::upload_bytes(path, &header, LOG_COPY_BYTES) {
        Err(e) => t!("debug-read-failed", "error" => e.to_string()),
        Ok(bytes) if bytes.len() <= header.len() + 1 => t!("debug-copy-empty"),
        Ok(bytes) => {
            let size = human_size(bytes.len() as u64);
            match crate::clipboard::copy_text(&String::from_utf8_lossy(&bytes)) {
                Ok(()) => {
                    log::info!("log copied to the clipboard from Settings → Debug ({size})");
                    t!("debug-copied", "size" => size)
                }
                Err(e) => t!("debug-copy-failed", "error" => e),
            }
        }
    }
}

/// "Clear log": empty the file (and the rolled-over one). The line
/// saying so is the new log's first.
fn clear_log() -> String {
    let Some(path) = logfile::path() else {
        return t!("debug-no-log-file");
    };
    match logfile::clear(path) {
        Ok(()) => {
            log::info!("log cleared from Settings → Debug");
            t!("debug-cleared")
        }
        Err(e) => t!("debug-clear-failed", "error" => e.to_string()),
    }
}

/// The picker's word for `mode`.
fn aec_mode_label(mode: audio::AecMode) -> String {
    match mode {
        audio::AecMode::Auto => t!("audio-aec-auto"),
        audio::AecMode::Always => t!("audio-aec-always"),
        audio::AecMode::Off => t!("audio-aec-off"),
    }
}

/// The status line under the picker: what the open streams got
/// (AEC-SPEC.md §3.3), in the user's language.
fn aec_status(report: Option<&audio::AecReport>) -> String {
    use audio::aec::reasons;
    let Some(report) = report else {
        return t!("aec-status-idle");
    };
    match report.tier {
        audio::AecTier::Platform => t!("aec-status-on"),
        audio::AecTier::None => match report.reason {
            reasons::OFF => t!("aec-status-off"),
            reasons::HEADPHONES => t!("aec-status-headphones"),
            reasons::NO_CANCELLER => t!("aec-status-no-canceller"),
            reasons::CANCELLER_OFF => t!("aec-status-canceller-off"),
            reasons::UNSUPPORTED => t!("aec-status-unsupported"),
            _ => t!("aec-status-refused"),
        },
    }
}

/// Save and close, the one way out that keeps what changed: cancel any
/// recording, stop a running mic test, persist, hide.
fn close(app: &mut AppState) {
    app.settings_opened = None;
    leave(app);
    // Collapse blank token fields and repair the default pointer
    // before the edits hit disk.
    app.settings.normalize_servers();
    app.save_settings();
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
    let sf = surface(app);
    sf.hide();
}

/// What both ways out do first: cancel a recording, stop a running mic
/// test, and close the microphone the Audio tab's meter held.
fn leave(app: &mut AppState) {
    cancel_capture(app);
    if app.is_testing_mic {
        app.handle_test_microphone(false);
        let sf = surface(app);
        sf.state().set_testing_mic(false);
    }
    app.meter_timer.stop();
    // Nothing shows the meter any more: its microphone closes.
    app.set_audio_tab_open(false);
    note_meter_run(false);
}

/// The footer's Close, and Escape, Back and the arrow (`back()` in
/// settings.slint): what the page changed goes back to how it was
/// when it opened — in the running app as well, since every control
/// applied at once — and the page closes. The file is written with
/// that state when anything changed.
fn discard_and_close(app: &mut AppState) {
    leave(app);
    let before = app.settings.clone();
    if put_back_unsaved(app) {
        app.apply_settings_live(&before);
        app.save_settings();
        log::info!("settings: closed without keeping the page's changes");
    }
    surface(app).hide();
}

/// Whether the open page changed anything only Save and close would
/// keep: the question before a quit (`AppState::leave_app`).
pub(crate) fn has_unsaved(app: &AppState) -> bool {
    let Some(opened) = &app.settings_opened else {
        return false;
    };
    let restored = opened.restored_onto(&app.settings);
    serde_json::to_value(&restored).ok() != serde_json::to_value(&app.settings).ok()
        || std::fs::read(client::config::avatar_path()).ok() != opened.avatar
}

/// The settings as the file should hold them: while the page is open,
/// without what it changed — only Save and close writes that. Every
/// save goes through here (`AppState::save_settings`), so nothing the
/// page changed reaches the disk by another road (a pin, a resized
/// sidebar, a quit).
pub(crate) fn settings_to_write(app: &AppState) -> std::borrow::Cow<'_, Settings> {
    match &app.settings_opened {
        Some(opened) => std::borrow::Cow::Owned(opened.restored_onto(&app.settings)),
        None => std::borrow::Cow::Borrowed(&app.settings),
    }
}

/// Put back what the open page changed — the settings, the picture
/// file, whether the profile was due to be sent — and forget that the
/// page was open. `true` when anything changed. Close ends with this,
/// and so do a quit answered "without saving" and a shutdown nobody
/// was asked about.
pub(crate) fn put_back_unsaved(app: &mut AppState) -> bool {
    let Some(opened) = app.settings_opened.take() else {
        return false;
    };
    let restored = opened.restored_onto(&app.settings);
    let avatar_now = std::fs::read(client::config::avatar_path()).ok();
    let avatar_changed = avatar_now != opened.avatar;
    let settings_changed =
        serde_json::to_value(&restored).ok() != serde_json::to_value(&app.settings).ok();
    if !settings_changed && !avatar_changed {
        return false;
    }
    app.settings = restored;
    if avatar_changed {
        let path = client::config::avatar_path();
        let put_back = match &opened.avatar {
            Some(bytes) => std::fs::write(&path, bytes),
            None => std::fs::remove_file(&path),
        };
        if let Err(e) = put_back {
            log::warn!("settings: could not put the picture back: {e}");
        }
        app.forget_avatar_blob();
        app.reload_own_avatar();
    }
    app.profile_dirty = opened.profile_dirty;
    true
}

/// A quit's question answered "Save and quit": what Save and close
/// does.
pub(crate) fn save_for_quit(app: &mut AppState) {
    if app.main.global::<State>().get_settings_open() {
        close(app);
    }
}

/// A quit's question answered "Quit without saving": the page's
/// changes go back and the page closes; the running app is not told
/// (it is about to end), only the file.
pub(crate) fn drop_for_quit(app: &mut AppState) {
    leave(app);
    put_back_unsaved(app);
    surface(app).hide();
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

// ── Licenses (1.2) ──────────────────────────────────────────────────────

/// What baucord ships and under which licenses: `scripts/licenses.py`
/// writes it from Cargo.lock (through cargo-about) and licenses/native,
/// the same list as web/licenses.html. CI rewrites this file before
/// every release build (.github/workflows/licenses.yml), so a shipped
/// app always lists what it was built from.
const NOTICES: &str = include_str!("../licenses.json");

#[derive(serde::Deserialize)]
struct Notices {
    crate_count: usize,
    components: Vec<NoticeEntry>,
    crates: Vec<NoticeEntry>,
    /// Each distinct license text once, as paragraphs.
    texts: Vec<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct NoticeEntry {
    name: String,
    #[serde(default)]
    license: String,
    #[serde(default)]
    count: usize,
    blocks: Vec<NoticeBlock>,
}

#[derive(serde::Deserialize)]
struct NoticeBlock {
    note: String,
    text: Option<usize>,
}

impl Notices {
    /// Components first, then the license groups: the index a row's
    /// `entry` carries.
    fn entry(&self, index: usize) -> Option<(&NoticeEntry, bool)> {
        self.components.get(index).map(|e| (e, false)).or_else(|| {
            index
                .checked_sub(self.components.len())
                .and_then(|i| self.crates.get(i))
                .map(|e| (e, true))
        })
    }
}

thread_local! {
    /// Parsed the first time Settings opens, kept for the run.
    static NOTICES_DATA: OnceCell<Option<Notices>> = const { OnceCell::new() };
    /// The entries open in the list, kept across a language change.
    static LICENSES_OPEN: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

fn with_notices<R>(f: impl FnOnce(&Notices) -> R) -> Option<R> {
    NOTICES_DATA.with(|cell| {
        cell.get_or_init(|| {
            serde_json::from_str(NOTICES)
                .map_err(|e| log::warn!("licenses.json: {e}"))
                .ok()
        })
        .as_ref()
        .map(f)
    })
}

/// The row for entry `index`, open or not.
fn license_entry_row(notices: &Notices, index: usize, open: bool) -> Option<LicenseRow> {
    let (entry, is_crates) = notices.entry(index)?;
    let detail = if is_crates {
        t!("licenses-crates-count", "count" => entry.count)
    } else {
        entry.license.clone()
    };
    Some(LicenseRow {
        kind: 1,
        text: entry.name.as_str().into(),
        detail: detail.into(),
        open,
        entry: i32::try_from(index).unwrap_or(-1),
    })
}

/// What an open entry shows under it: each block's note (copyright and
/// source, or the crates), then its text paragraph by paragraph.
fn license_body_rows(notices: &Notices, index: usize) -> Vec<LicenseRow> {
    let Some((entry, is_crates)) = notices.entry(index) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for block in &entry.blocks {
        if !block.note.is_empty() {
            let note = if is_crates {
                t!("licenses-used-by", "crates" => block.note.as_str())
            } else {
                block.note.clone()
            };
            rows.push(LicenseRow {
                kind: 2,
                text: note.into(),
                ..LicenseRow::default()
            });
        }
        if let Some(paragraphs) = block.text.and_then(|i| notices.texts.get(i)) {
            rows.extend(paragraphs.iter().map(|p| LicenseRow {
                kind: 3,
                text: p.as_str().into(),
                ..LicenseRow::default()
            }));
        }
    }
    rows
}

/// The language changed: the licence list's headings are words too.
/// Rebuilt now if its tab is up, else when it next shows.
pub(crate) fn relabel_licenses(app: &AppState) {
    let sf = surface(app);
    if sf.showing() && sf.tab() == TAB_LICENSES {
        set_licenses(&sf.state());
    } else {
        sf.state().set_licenses(ModelRc::default());
    }
}

/// The whole list, with the open entries expanded.
fn set_licenses(s: &ui::SettingsState) {
    let rows = with_notices(|notices| {
        LICENSES_OPEN.with(|open| {
            let open = open.borrow();
            let mut rows = Vec::new();
            let mut index = 0;
            let sections = [
                (t!("licenses-section-components"), notices.components.len()),
                (
                    t!("licenses-section-crates", "count" => notices.crate_count),
                    notices.crates.len(),
                ),
            ];
            for (title, len) in sections {
                rows.push(LicenseRow {
                    kind: 0,
                    text: title.into(),
                    ..LicenseRow::default()
                });
                for _ in 0..len {
                    let is_open = open.contains(&index);
                    rows.extend(license_entry_row(notices, index, is_open));
                    if is_open {
                        rows.extend(license_body_rows(notices, index));
                    }
                    index += 1;
                }
            }
            rows
        })
    })
    .unwrap_or_default();
    s.set_licenses(ModelRc::new(VecModel::from(rows)));
}

/// Open or close one entry in place: rows go in under it or come out,
/// and the rest of the list stays where it is (a new model would put
/// the reader back at the top).
fn toggle_license(s: &ui::SettingsState, index: usize) {
    let model = s.get_licenses();
    let Some(rows) = model.as_any().downcast_ref::<VecModel<LicenseRow>>() else {
        return;
    };
    let Ok(wanted) = i32::try_from(index) else {
        return;
    };
    let Some(at) = (0..rows.row_count()).find(|&i| {
        rows.row_data(i)
            .is_some_and(|r| r.kind == 1 && r.entry == wanted)
    }) else {
        return;
    };
    let opening = LICENSES_OPEN.with(|open| {
        let mut open = open.borrow_mut();
        if open.remove(&index) {
            false
        } else {
            open.insert(index);
            true
        }
    });
    if let Some(mut row) = rows.row_data(at) {
        row.open = opening;
        rows.set_row_data(at, row);
    }
    if opening {
        let body = with_notices(|notices| license_body_rows(notices, index)).unwrap_or_default();
        for (i, row) in body.into_iter().enumerate() {
            rows.insert(at + 1 + i, row);
        }
    } else {
        while rows.row_data(at + 1).is_some_and(|r| r.kind >= 2) {
            rows.remove(at + 1);
        }
    }
}
