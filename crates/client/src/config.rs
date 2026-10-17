use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use audio::PeerAudioSettings;
use log::{info, warn};
use serde::{Deserialize, Serialize};

/// The key per-person audio settings were filed under until 1.2.1: the
/// display name. A rename lost them and a namesake inherited them
/// (review P2-24); they are filed under the person's id now
/// ([`peer_id_key`]), and an entry under a name is moved there the
/// first time its owner is seen ([`Settings::claim_peer_audio`]). Still
/// read for a session whose id is not known yet.
#[must_use]
pub fn peer_key(username: &str) -> String {
    username.to_string()
}

/// The key of a person's audio settings: who they are, whatever they
/// are called today. A name cannot look like one — the colon is not a
/// name character.
#[must_use]
pub fn peer_id_key(user: &protocol::UserId) -> String {
    use std::fmt::Write;
    let mut key = String::with_capacity(3 + 64);
    key.push_str("id:");
    for byte in user {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

// ── Bindable actions ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindableAction {
    ToggleMute,
    ToggleDeafen,
    ToggleScreenShare,
    PushToTalk,
}

impl BindableAction {
    /// Actions shown in the keybind UI. Push to talk is a hold, not a
    /// toggle: the hotkey layer forwards its release as well.
    pub const ALL: &'static [BindableAction] = &[
        BindableAction::ToggleMute,
        BindableAction::ToggleDeafen,
        BindableAction::ToggleScreenShare,
        BindableAction::PushToTalk,
    ];

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            BindableAction::ToggleMute => "Toggle Mute",
            BindableAction::ToggleDeafen => "Toggle Deafen",
            BindableAction::ToggleScreenShare => "Toggle Screen Share",
            BindableAction::PushToTalk => "Push to talk",
        }
    }
}

// ── Keybind entry ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeybindEntry {
    pub action: BindableAction,
    pub bind: String,
}

impl KeybindEntry {
    #[must_use]
    pub fn parse(&self) -> Option<ParsedKeybind> {
        if self.bind.is_empty() {
            return None;
        }
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut code: Option<String> = None;

        for part in self.bind.split('+') {
            let part = part.trim();
            match part.to_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                _ => code = Some(part.to_string()),
            }
        }

        Some(ParsedKeybind {
            ctrl,
            alt,
            shift,
            code: code?,
        })
    }
}

pub struct ParsedKeybind {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub code: String,
}

#[must_use]
pub fn build_bind_string(ctrl: bool, alt: bool, shift: bool, code: &str) -> String {
    let mut parts = Vec::new();
    if ctrl {
        parts.push("Ctrl");
    }
    if alt {
        parts.push("Alt");
    }
    if shift {
        parts.push("Shift");
    }
    parts.push(code);
    parts.join("+")
}

// ── Settings sub-sections ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct UserSettings {
    pub nickname: String,
}

/// How the app itself reads and sounds — nothing a server sees.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct UiSettings {
    /// Multiplier on every font size in the UI (1.0 = the design size).
    #[serde(default = "default_text_scale")]
    pub text_scale: f32,
    /// `[HH:MM]` in front of chat rows.
    #[serde(default = "default_true")]
    pub show_timestamps: bool,
    /// Tighter chat rows.
    #[serde(default)]
    pub compact_rows: bool,
    /// Outgoing chat images are shrunk to fit this many KB; the wire
    /// cap (`protocol::MAX_CHAT_IMAGE_BYTES`) is the ceiling.
    #[serde(default = "default_image_max_kb")]
    pub image_max_kb: u32,
    /// The sidebar's Messages section is folded to its header, giving
    /// the voice channels the room.
    #[serde(default)]
    pub messages_collapsed: bool,
    /// The sidebar's width in logical pixels (dragged at its edge).
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    /// The Messages section's height (dragged at its divider).
    #[serde(default = "default_messages_height")]
    pub messages_height: u16,
    /// Conversations closed with their ✕ (user ids, hex): they stay
    /// out of the Messages list across logins until a new message
    /// arrives or "Message" is picked on that person again.
    #[serde(default)]
    pub hidden_dms: Vec<String>,
}

fn default_sidebar_width() -> u16 {
    260
}

fn default_messages_height() -> u16 {
    200
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            text_scale: default_text_scale(),
            show_timestamps: true,
            compact_rows: false,
            image_max_kb: default_image_max_kb(),
            messages_collapsed: false,
            sidebar_width: default_sidebar_width(),
            messages_height: default_messages_height(),
            hidden_dms: Vec::new(),
        }
    }
}

fn default_text_scale() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

fn default_image_max_kb() -> u32 {
    u32::try_from(protocol::MAX_CHAT_IMAGE_BYTES / 1024).unwrap_or(u32::MAX)
}

/// Which notification sounds play. `enabled` is the master switch;
/// the rest pick. Ordinary chat messages never make a sound — a busy
/// general chat must not be able to disturb anyone.
// One bool per toggle in Settings → Sounds, and the same keys in
// `settings.json`; the file format is the reason, not a state machine.
#[expect(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct SoundSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Someone joins or leaves our channel.
    #[serde(default = "default_true")]
    pub join_leave: bool,
    /// A message names us (the nickname as a word, or `@nickname`).
    /// Off by default.
    #[serde(default)]
    pub mention: bool,
    /// A direct message arrives (spec §6). On by default: it is for
    /// us, and only us.
    #[serde(default = "default_true")]
    pub direct_message: bool,
    /// Someone in our channel starts or stops sharing their screen (the
    /// name is from before the stop had a sound; kept for saved files).
    #[serde(default = "default_true")]
    pub share_started: bool,
    /// Someone starts watching the stream we share (a softer sound,
    /// to the sharer alone).
    #[serde(default = "default_true")]
    pub viewer_joined: bool,
    /// We mute, unmute, deafen or undeafen ourselves: one soft note.
    #[serde(default = "default_true")]
    pub mute_toggle: bool,
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            join_leave: true,
            mention: false,
            direct_message: true,
            share_started: true,
            viewer_joined: true,
            mute_toggle: true,
        }
    }
}

impl SoundSettings {
    /// Whether a given sound plays: the master switch, then its own.
    #[must_use]
    pub fn join_leave_on(&self) -> bool {
        self.enabled && self.join_leave
    }

    #[must_use]
    pub fn mention_on(&self) -> bool {
        self.enabled && self.mention
    }

    #[must_use]
    pub fn direct_message_on(&self) -> bool {
        self.enabled && self.direct_message
    }

    #[must_use]
    pub fn share_started_on(&self) -> bool {
        self.enabled && self.share_started
    }

    #[must_use]
    pub fn viewer_joined_on(&self) -> bool {
        self.enabled && self.viewer_joined
    }

    #[must_use]
    pub fn mute_toggle_on(&self) -> bool {
        self.enabled && self.mute_toggle
    }
}

/// What a phone tells the user while the app is in the background
/// (ANDROID-SPEC.md D52, IOS-SPEC.md D50): system notifications, which
/// follow the phone's own silent, vibrate and Do Not Disturb modes.
/// The desktop has none yet; the fields are kept there all the same.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct NotificationSettings {
    /// A direct message arrives while the app is in the background.
    #[serde(default = "default_true")]
    pub messages: bool,
    /// Someone joins a voice channel while nobody else is in one — the
    /// call is starting. Only while the app stays connected.
    #[serde(default = "default_true")]
    pub first_join: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            messages: true,
            first_join: true,
        }
    }
}

/// What happens when the app launches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct StartupSettings {
    /// Connect to the default server as soon as the connect screen
    /// would appear (needs a nickname).
    #[serde(default)]
    pub auto_connect: bool,
    /// Start in the tray without showing the window (builds with the
    /// tray only; ignored otherwise).
    #[serde(default)]
    pub start_minimized: bool,
    /// Look for a newer release before showing the connect screen.
    #[serde(default = "default_true")]
    pub check_updates: bool,
}

impl Default for StartupSettings {
    fn default() -> Self {
        Self {
            auto_connect: false,
            start_minimized: false,
            check_updates: true,
        }
    }
}

/// How the user's name is drawn (spec §7.1). Kept here, sent in
/// `SetProfile` once servers carry profiles (1.1.2).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NameStyle {
    #[default]
    Normal,
    Bold,
    Italic,
}

/// The client-held profile (spec §7.1): name color, name style, and
/// whether an avatar file sits next to the settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct ProfileSettings {
    /// `0xRRGGBB`; `None` = the theme's default name color.
    #[serde(default)]
    pub name_color: Option<u32>,
    #[serde(default)]
    pub name_style: NameStyle,
    /// `avatar.png` (128×128) exists in the config directory.
    #[serde(default)]
    pub avatar: bool,
}

/// One saved server the user can connect to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerEntry {
    /// Display name, and what `Settings::server` points at.
    pub name: String,
    /// `ip:port` (or `[v6]:port`) — parsed as a `SocketAddr` on connect.
    pub address: String,
    /// The invite code to present on the first visit (spec §3.1),
    /// typed when the server asks. Sent once: cleared the moment the
    /// server admits us, because from then on the key is the login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<String>,
    /// The server's pinned identity, `SHA256:<base64>` (spec §3.2).
    /// Filled from an invite string, or on first successful connection
    /// (trust on first use). A server presenting a different key is
    /// refused until the user accepts the change.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Saved voice-channel passwords for this server, keyed by channel
    /// *name* (channel ids are per-server-run indexes; names are the
    /// stable handle an operator gives a room). Filled when a join with
    /// a typed password succeeds path-wise (we save on OK — a wrong
    /// password just re-opens the prompt). Plain text: they gate a room
    /// among people already admitted, not the server.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub channel_passwords: HashMap<String, String>,
}

impl ServerEntry {
    /// The official baucord server — the out-of-the-box entry. Its
    /// fingerprint is learned on first use.
    #[must_use]
    pub fn official() -> Self {
        Self {
            name: "baucord".to_string(),
            address: OFFICIAL_ADDRESS.to_string(),
            invite_code: None,
            fingerprint: None,
            channel_passwords: HashMap::new(),
        }
    }

    /// Take a typed code as the one to present next time. Returns
    /// whether the input reads as a code (blank clears it).
    pub fn set_invite_input(&mut self, input: &str) -> bool {
        let input = input.trim();
        if input.is_empty() {
            self.invite_code = None;
            return true;
        }
        match protocol::invite::normalize_code(input) {
            Some(code) => {
                self.invite_code = Some(code);
                true
            }
            None => false,
        }
    }
}

fn default_servers() -> Vec<ServerEntry> {
    vec![ServerEntry::official()]
}

/// The official server, on the port it moved to in 1.1.5.
const OFFICIAL_ADDRESS: &str = "216.238.121.18:51000";
/// The official server's old port: still answered (Docker publishes
/// both), but a saved entry pointing there is moved on load — the
/// port is the reason three users on one ISP heard each other at
/// 16 kbps (the session guide's Movistar case).
const OFFICIAL_ADDRESS_OLD_PORT: &str = "216.238.121.18:5000";

#[derive(Debug, Clone, Serialize, Deserialize)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct AudioInputSettings {
    pub device: Option<String>,
    pub gain: f32,
}

impl Default for AudioInputSettings {
    fn default() -> Self {
        Self {
            device: None,
            gain: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct AudioOutputSettings {
    pub device: Option<String>,
    pub gain: f32,
}

impl Default for AudioOutputSettings {
    fn default() -> Self {
        Self {
            device: None,
            gain: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct AudioSettings {
    pub input: AudioInputSettings,
    pub output: AudioOutputSettings,
    pub gate_threshold: f32,
    /// Transmit only while the push-to-talk key is held (the gate
    /// still applies); off = voice activity.
    #[serde(default)]
    pub push_to_talk: bool,
    /// Echo cancellation (`AEC-SPEC.md` §3.1): `"auto"` (the default:
    /// only on speakers), `"always"`, `"off"`.
    #[serde(default)]
    pub echo_cancellation: audio::AecMode,
    /// The user was told, once, that Windows is lowering their other
    /// apps and where the way out is (`AEC-SPEC.md` E15).
    #[serde(default)]
    pub lowering_told: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input: AudioInputSettings::default(),
            output: AudioOutputSettings::default(),
            gate_threshold: 0.010,
            push_to_talk: false,
            echo_cancellation: audio::AecMode::Auto,
            lowering_told: false,
        }
    }
}

/// The main window's size. Its *position* is not kept (1.1.5): a
/// saved spot on a monitor that is no longer there lost people their
/// window, so every launch centres it on the monitor it opens on.
/// Older files still carry `x`/`y`; serde ignores them.
#[derive(Debug, Clone, Serialize, Deserialize)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct WindowSettings {
    pub width: i32,
    pub height: i32,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: 1000,
            height: 700,
        }
    }
}

/// Last-used screen-share choices. The share dialog opens pre-filled
/// with these, and the global share hotkey starts a stream with them
/// directly when Baucord is not the focused window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct ShareSettings {
    /// Quality preset key (`video::Quality::key()`, e.g. "fhd30").
    /// Stored as a string so settings files survive preset reordering
    /// in future versions; unknown keys fall back to the default.
    pub quality: String,
    /// Last selected monitor index. `None` = use the primary monitor.
    /// Validated against the current monitor list at use time (monitors
    /// come and go); out-of-range falls back to primary.
    pub monitor: Option<u32>,
    /// Share visibility: `true` = everyone on the server, `false` =
    /// current voice channel only.
    pub everyone: bool,
    /// Show the mouse pointer in the share (Windows draws it into the
    /// picture itself, `video/src/dxgi.rs`). No switch in the app: on,
    /// unless this file says `false`.
    #[serde(default = "default_true")]
    pub pointer: bool,
}

impl Default for ShareSettings {
    fn default() -> Self {
        Self {
            quality: "fhd30".to_string(),
            monitor: None,
            everyone: false,
            pointer: true,
        }
    }
}

fn default_keybinds() -> Vec<KeybindEntry> {
    vec![
        KeybindEntry {
            action: BindableAction::ToggleMute,
            bind: "F7".to_string(),
        },
        KeybindEntry {
            action: BindableAction::ToggleDeafen,
            bind: "F8".to_string(),
        },
        KeybindEntry {
            action: BindableAction::ToggleScreenShare,
            bind: "F9".to_string(),
        },
    ]
}

// ── Root settings ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub user: UserSettings,
    /// Name of the default server in [`Self::servers`] — the entry
    /// preselected on the connect screen at startup.
    #[serde(default, deserialize_with = "server_name")]
    pub server: String,
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
    #[serde(default)]
    pub audio: AudioSettings,
    #[serde(default)]
    pub window: WindowSettings,
    #[serde(default = "default_keybinds")]
    pub keybinds: Vec<KeybindEntry>,
    #[serde(default)]
    pub peer_settings: HashMap<String, PeerAudioSettings>,
    /// UI language as a BCP-47 code (e.g. "es-AR"). `None` = follow the
    /// OS locale. Which locales exist is the frontends' business.
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub share: ShareSettings,
    #[serde(default)]
    pub ui: UiSettings,
    #[serde(default)]
    pub profile: ProfileSettings,
    #[serde(default)]
    pub startup: StartupSettings,
    #[serde(default)]
    pub sounds: SoundSettings,
    #[serde(default)]
    pub logging: LoggingSettings,
    #[serde(default)]
    pub notifications: NotificationSettings,
}

/// How much `baucord.log` records (1.1.5). `RUST_LOG` in the
/// environment still wins when set.
#[derive(Debug, Clone, Serialize, Deserialize)]
// A key missing from the file takes its default, not the whole file
// with it (review 2026-10, P3-43).
#[serde(default)]
pub struct LoggingSettings {
    /// One of `error`, `warn`, `info`, `debug`, `trace`.
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

impl LoggingSettings {
    /// The level as the `log` crate wants it; anything unknown reads
    /// as `info`.
    #[must_use]
    pub fn level_filter(&self) -> log::LevelFilter {
        match self.level.as_str() {
            "error" => log::LevelFilter::Error,
            "warn" => log::LevelFilter::Warn,
            "debug" => log::LevelFilter::Debug,
            "trace" => log::LevelFilter::Trace,
            _ => log::LevelFilter::Info,
        }
    }
}

/// `Settings::server` from a file: the name it is — or nothing, for
/// anything else. Before 0.3 the field was an object (one inline
/// server), and a field of the wrong type fails the whole file: the
/// nickname, the keybinds and the audio settings of someone coming
/// from that far back still load, with the default server. (A shadow
/// of every field of [`Settings`] used to exist to carry that one
/// server over — review P3-17; no server from then admits a 1.1
/// client without a new invite anyway.)
fn server_name<'de, D: serde::Deserializer<'de>>(de: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(de)? {
        serde_json::Value::String(name) => name,
        _ => String::new(),
    })
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            user: UserSettings::default(),
            server: ServerEntry::official().name,
            servers: default_servers(),
            audio: AudioSettings::default(),
            window: WindowSettings::default(),
            keybinds: default_keybinds(),
            peer_settings: HashMap::new(),
            language: None,
            share: ShareSettings::default(),
            ui: UiSettings::default(),
            profile: ProfileSettings::default(),
            startup: StartupSettings::default(),
            sounds: SoundSettings::default(),
            logging: LoggingSettings::default(),
            notifications: NotificationSettings::default(),
        }
    }
}

impl Settings {
    fn get_settings_path() -> PathBuf {
        config_base_dir().join("settings.json")
    }

    /// The pre-0.2 file next to the executable — still probed by
    /// `load` so ancient installs migrate.
    fn exe_adjacent_settings_path() -> PathBuf {
        let mut path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        path.push("settings.json");
        path
    }

    /// The audio settings saved for the person `user`, called `name`
    /// right now — moving an entry filed under that name (how they
    /// were kept until 1.2.1) to the id. The flag says something moved
    /// and the settings want saving.
    pub fn claim_peer_audio(
        &mut self,
        user: &protocol::UserId,
        name: &str,
    ) -> (Option<PeerAudioSettings>, bool) {
        let key = peer_id_key(user);
        if let Some(saved) = self.peer_settings.get(&key) {
            return (Some(saved.clone()), false);
        }
        match self.peer_settings.remove(&peer_key(name)) {
            Some(saved) => {
                self.peer_settings.insert(key, saved.clone());
                (Some(saved), true)
            }
            None => (None, false),
        }
    }

    /// Save (`Some`) or forget (`None`) a person's audio settings: under
    /// their id when it is known, under the name otherwise. Either way
    /// nothing is left under the name for a namesake to pick up.
    pub fn set_peer_audio(
        &mut self,
        user: Option<&protocol::UserId>,
        name: &str,
        audio: Option<PeerAudioSettings>,
    ) {
        let key = match user {
            Some(user) => {
                self.peer_settings.remove(&peer_key(name));
                peer_id_key(user)
            }
            None => peer_key(name),
        };
        match audio {
            Some(audio) => {
                self.peer_settings.insert(key, audio);
            }
            None => {
                self.peer_settings.remove(&key);
            }
        }
    }

    /// Settings from a file's text, with the server list's invariants
    /// repaired ([`Self::normalize_servers`]): the one way in.
    ///
    /// # Errors
    /// Not JSON, or not settings.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut settings: Settings = serde_json::from_str(json)?;
        settings.normalize_servers();
        Ok(settings)
    }

    #[must_use]
    pub fn load() -> Self {
        // One-time migration from previous locations, newest scheme
        // first — the 0.2 platform config dir, then the pre-0.2
        // exe-adjacent file — so upgrading users keep their nickname,
        // keybinds, and per-peer audio settings.
        let old_paths: Vec<PathBuf> = [
            platform_config_dir().map(|d| d.join("settings.json")),
            Some(Self::exe_adjacent_settings_path()),
        ]
        .into_iter()
        .flatten()
        .collect();
        Self::load_at(&Self::get_settings_path(), &old_paths)
    }

    /// [`Self::load`] for the file at `path`, migrating from the first
    /// of `old_paths` that loads — only when there is no file at
    /// `path` at all.
    fn load_at(path: &Path, old_paths: &[PathBuf]) -> Self {
        match Stored::read(path) {
            Stored::Loaded(settings) => {
                info!("Loaded settings from {}", path.display());
                return *settings;
            }
            Stored::Corrupt(why) => {
                // A file that is there and does not parse is not "no
                // file": the migration below used to run, and an older
                // install's settings — its servers, its name — came
                // back as if they were current (review P3-18). It is
                // kept beside the new one, so the next save does not
                // write over what a hand could still repair.
                log::error!("{} could not be read: {why}", path.display());
                let aside = with_suffix(path, ".corrupt");
                match fs::rename(path, &aside) {
                    Ok(()) => warn!("The unreadable settings are kept as {}", aside.display()),
                    Err(e) => warn!("…and could not be set aside: {e}"),
                }
                return Self::from_backup(path).unwrap_or_else(|| {
                    warn!("Using default settings");
                    Settings::default()
                });
            }
            Stored::Absent => {}
        }
        // A save interrupted between its two renames leaves only this.
        if let Some(settings) = Self::from_backup(path) {
            return settings;
        }
        for old in old_paths {
            if old != path
                && let Stored::Loaded(settings) = Stored::read(old)
            {
                info!(
                    "Migrating settings from {} to {}",
                    old.display(),
                    path.display()
                );
                settings.save_at(path);
                if let (Some(from), Some(to)) = (old.parent(), path.parent()) {
                    migrate_neighbours(from, to);
                }
                return *settings;
            }
        }

        info!("Using default settings");
        Settings::default()
    }

    /// The last save before the current one, if it loads.
    fn from_backup(path: &Path) -> Option<Self> {
        let backup = with_suffix(path, ".bak");
        let Stored::Loaded(settings) = Stored::read(&backup) else {
            return None;
        };
        warn!("Settings restored from {}", backup.display());
        Some(*settings)
    }

    pub fn save(&self) {
        self.save_at(&Self::get_settings_path());
    }

    /// Write the settings to `path` without ever leaving a torn file
    /// there: the new contents go to a file beside it, the current file
    /// becomes the backup, and the new one takes its place. Written in
    /// place, a crash mid-write lost every server and pin (plan CLI-6).
    fn save_at(&self, path: &Path) {
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            log::error!(
                "Failed to create settings directory {}: {e}",
                parent.display()
            );
            return;
        }
        let contents = match serde_json::to_string_pretty(self) {
            Ok(contents) => contents,
            Err(e) => {
                log::error!("Failed to serialize settings: {e}");
                return;
            }
        };
        // The file already says this: nothing to write. Every save
        // rewrote it — and its backup — whatever changed (review
        // 2026-10, P3-43).
        if fs::read_to_string(path).is_ok_and(|on_disk| on_disk == contents) {
            return;
        }
        let staged = with_suffix(path, ".tmp");
        // On the disk before it replaces the old file: a power cut after
        // the rename must not leave an empty one.
        let write_synced = |at: &Path| -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = fs::File::create(at)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        };
        let written = write_synced(&staged).and_then(|()| {
            if path.exists() {
                // Best effort: without a backup the save still stands.
                let _ = fs::rename(path, with_suffix(path, ".bak"));
            }
            fs::rename(&staged, path)
        });
        // Somewhere a rename is refused (a scanner holding the file):
        // in place, as it always was.
        let written = written.or_else(|e| {
            let _ = fs::remove_file(&staged);
            log::debug!("settings not replaced by rename ({e}); writing in place");
            write_synced(path)
        });
        match written {
            Ok(()) => info!("Settings saved to {}", path.display()),
            Err(e) => log::error!("Failed to save settings: {e}"),
        }
    }

    pub fn reset(&mut self) {
        *self = Settings::default();
        self.save();
    }

    /// Repair the server list invariants after load or mutation: at
    /// least one entry exists, blank optional fields collapse to
    /// `None`, and `server` names an existing entry (first entry wins
    /// when the pointer is dangling).
    pub fn normalize_servers(&mut self) {
        if self.servers.is_empty() {
            self.servers = default_servers();
        }
        for entry in &mut self.servers {
            // The official server changed port (1.1.5): follow it,
            // keeping the pin and passwords — same host, same key.
            if entry.address == OFFICIAL_ADDRESS_OLD_PORT {
                entry.address = OFFICIAL_ADDRESS.to_string();
            }
            let blank = |v: &Option<String>| v.as_deref().is_some_and(|s| s.trim().is_empty());
            // Codes are stored canonical; anything else is not a code.
            entry.invite_code = entry
                .invite_code
                .as_deref()
                .and_then(protocol::invite::normalize_code);
            if blank(&entry.fingerprint)
                || entry
                    .fingerprint
                    .as_deref()
                    .is_some_and(|fp| fp.parse::<transport::Fingerprint>().is_err())
            {
                entry.fingerprint = None;
            }
        }
        if !self.servers.iter().any(|s| s.name == self.server) {
            self.server = self.servers[0].name.clone();
        }
    }

    /// The default server — the entry named by `server` (first name
    /// match when names collide), falling back to the first entry.
    #[must_use]
    pub fn default_server(&self) -> Option<&ServerEntry> {
        self.servers
            .iter()
            .find(|s| s.name == self.server)
            .or_else(|| self.servers.first())
    }

    /// Mutable variant of [`Self::default_server`].
    pub fn default_server_mut(&mut self) -> Option<&mut ServerEntry> {
        let index = self
            .servers
            .iter()
            .position(|s| s.name == self.server)
            .unwrap_or(0);
        self.servers.get_mut(index)
    }

    #[must_use]
    pub fn bind_for(&self, action: BindableAction) -> &str {
        self.keybinds
            .iter()
            .find(|e| e.action == action)
            .map_or("", |e| e.bind.as_str())
    }

    pub fn set_bind(&mut self, action: BindableAction, bind: String) {
        if let Some(entry) = self.keybinds.iter_mut().find(|e| e.action == action) {
            entry.bind = bind;
        } else {
            self.keybinds.push(KeybindEntry { action, bind });
        }
    }
}

// ── Storage location ────────────────────────────────────────────────────

/// Directory holding baucord's settings.json.
///
/// Release builds prefer the executable's directory so users can find
/// their config next to the app (baucord installs are portable-style;
/// the platform config dir is invisible to most users). When the exe
/// directory isn't writable — Program Files, /usr/bin, a macOS app
/// bundle — fall back to the platform config dir (`%APPDATA%\baucord`,
/// `~/.config/baucord`, `~/Library/Application Support/baucord`) so
/// settings still persist instead of erroring on every save. Debug
/// builds always use the platform config dir, keeping state out of
/// target/.
fn config_base_dir() -> PathBuf {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return dir.clone();
    }
    if let Ok(dir) = std::env::var(CONFIG_DIR_ENV)
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir.trim());
    }
    #[cfg(not(debug_assertions))]
    {
        if let Some(dir) = writable_exe_dir() {
            return dir;
        }
    }
    platform_config_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Environment variable that relocates the whole config directory —
/// settings, identity key, everything — for this process.
pub const CONFIG_DIR_ENV: &str = "BAUCORD_CONFIG_DIR";

static CONFIG_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Relocate the config directory for this process (`--config-dir`,
/// `--profile`). Call before anything reads settings or the key; a
/// second call is ignored.
pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

/// The directory a named profile lives in: a second settings file and
/// identity under the platform config dir, so two people (or two test
/// instances) share one machine without sharing a key.
#[must_use]
pub fn profile_dir(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    platform_config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("profiles")
        .join(safe)
}

fn platform_config_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("baucord"))
}

/// The executable's directory, if files can actually be created there.
/// Probing with a real write is the only reliable check — directory
/// metadata reflects neither Windows ACLs nor read-only mounts. Cached
/// for the run: the answer can't change, and `Settings::save` runs on
/// every settings-window keystroke path.
#[cfg(not(debug_assertions))]
fn writable_exe_dir() -> Option<PathBuf> {
    static WRITABLE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    WRITABLE
        .get_or_init(|| {
            let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
            let probe = dir.join(".baucord-write-probe");
            fs::write(&probe, b"").ok()?;
            let _ = fs::remove_file(&probe);
            Some(dir)
        })
        .clone()
}

/// Absolute path of the live settings file (shown on the About tab;
/// its folder is what "open config location" opens).
#[must_use]
pub fn settings_file_path() -> PathBuf {
    Settings::get_settings_path()
}

/// Where the client's Ed25519 identity key lives: next to settings.json.
#[must_use]
pub fn client_key_path() -> PathBuf {
    config_base_dir().join("identity.key")
}

/// Where the profile avatar lives (128×128 PNG): next to settings.json.
#[must_use]
pub fn avatar_path() -> PathBuf {
    config_base_dir().join("avatar.png")
}

/// Where other people's avatars are cached by hash (spec §7.1: one
/// transfer per client per change): `avatar-cache/<hex>` next to
/// settings. Only a picture some look names as its avatar is written
/// here.
#[must_use]
pub fn avatar_cache_dir() -> PathBuf {
    config_base_dir().join("avatar-cache")
}

/// Delete the avatar cache's folder up to 1.2.1, `avatars/`: every
/// received chat picture went into it too, DM pictures included, and
/// nothing ever emptied it (review 2026-10, P0-1). The avatars in it
/// are fetched again, one transfer each. Slow on a big folder: call it
/// off the UI thread.
pub fn remove_old_avatar_cache() {
    let old = config_base_dir().join("avatars");
    if !old.is_dir() {
        return;
    }
    match std::fs::remove_dir_all(&old) {
        Ok(()) => log::info!("removed the old picture cache {}", old.display()),
        Err(e) => log::warn!(
            "could not remove the old picture cache {}: {e}",
            old.display()
        ),
    }
}

/// Whether settings live next to the executable (a portable install),
/// for the telemetry client info. Never true under a relocated
/// config directory.
#[must_use]
pub fn is_portable_install() -> bool {
    if CONFIG_DIR_OVERRIDE.get().is_some() || std::env::var_os(CONFIG_DIR_ENV).is_some() {
        return false;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .is_some_and(|dir| dir == config_base_dir())
}

/// Load (or create) the client's identity key. Failure is logged and
/// treated as "no key": since 1.1 that means no session — the app
/// shows the error instead of connecting.
#[must_use]
pub fn load_client_key() -> Option<std::sync::Arc<transport::ClientKey>> {
    let path = client_key_path();
    match transport::ClientKey::load_or_create(&path) {
        Ok(key) => Some(std::sync::Arc::new(key)),
        Err(e) => {
            log::warn!("client key unavailable ({e}); cannot connect without one");
            None
        }
    }
}

/// Copy the identity key file to `to` (Settings → Identity → Export),
/// so another machine can be the same user.
///
/// # Errors
/// No key file yet, or the copy failed.
pub fn export_client_key(to: &std::path::Path) -> Result<(), String> {
    let from = client_key_path();
    if !from.is_file() {
        return Err(format!("no identity key at {}", from.display()));
    }
    std::fs::copy(&from, to)
        .map(|_| ())
        .map_err(|e| format!("could not write {}: {e}", to.display()))
}

/// Replace the identity key with the file at `from` (Settings →
/// Identity → Import). The file must be a PKCS#8 Ed25519 key; the
/// current key, if any, is overwritten — this device becomes whoever
/// the imported key is. Returns the new key.
///
/// # Errors
/// Unreadable or malformed file, or the write failed.
pub fn import_client_key(
    from: &std::path::Path,
) -> Result<std::sync::Arc<transport::ClientKey>, String> {
    let der = std::fs::read(from).map_err(|e| format!("could not read {}: {e}", from.display()))?;
    transport::ClientKey::from_pkcs8(&der)?;
    let to = client_key_path();
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(&to, &der).map_err(|e| format!("could not write {}: {e}", to.display()))?;
    transport::ClientKey::load_or_create(&to)
        .map(std::sync::Arc::new)
        .map_err(|e| e.to_string())
}

/// What is at a settings path.
enum Stored {
    Absent,
    Loaded(Box<Settings>),
    /// There, and unreadable or not valid settings: why.
    Corrupt(String),
}

impl Stored {
    fn read(path: &Path) -> Self {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Stored::Absent,
            Err(e) => return Stored::Corrupt(e.to_string()),
        };
        match Settings::from_json(&contents) {
            Ok(settings) => Stored::Loaded(Box::new(settings)),
            Err(e) => Stored::Corrupt(e.to_string()),
        }
    }
}

/// `path` with `suffix` after its whole name: `settings.json.bak`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Throw the identity key away and make a fresh one (Settings →
/// Identity → Regenerate). The next connection is an unknown device
/// that needs an invite: a new user, on purpose.
///
/// # Errors
/// The old file could not be removed or the new one written.
pub fn regenerate_client_key() -> Result<std::sync::Arc<transport::ClientKey>, String> {
    let path = client_key_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| format!("could not remove {}: {e}", path.display()))?;
    }
    transport::ClientKey::load_or_create(&path)
        .map(std::sync::Arc::new)
        .map_err(|e| e.to_string())
}

/// The files that live beside settings.json and belong with it: the
/// identity (who the user is to every server) and their own avatar.
/// Settings moving to a new folder left them behind, and the user was
/// silently someone new (review 2026-10, P2-17; plan CLI-7). Copied, not
/// moved — the old folder stays as it was — and only where the new one
/// has none.
fn migrate_neighbours(from: &Path, to: &Path) {
    for name in ["identity.key", "avatar.png"] {
        let (old, new) = (from.join(name), to.join(name));
        if new.exists() || !old.is_file() {
            continue;
        }
        match fs::copy(&old, &new) {
            Ok(_) => info!("Migrated {} to {}", old.display(), new.display()),
            Err(e) => warn!("Could not migrate {}: {e}", old.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory for the load and save tests.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "baucord-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn named(name: &str) -> Settings {
        let mut settings = Settings::default();
        settings.user.nickname = name.to_string();
        settings
    }

    /// A file that is there and does not parse is kept aside and is
    /// not replaced by another install's settings: those came back as
    /// current, servers and name and all (review P3-18).
    #[test]
    fn a_corrupt_file_is_set_aside_not_replaced_by_an_older_install() {
        let dir = scratch("corrupt");
        let path = dir.join("settings.json");
        let old = dir.join("old-install.json");
        named("from an old install").save_at(&old);
        fs::write(&path, "{ \"username\": \"cut off mid-wri").expect("write");

        let loaded = Settings::load_at(&path, std::slice::from_ref(&old));
        assert_eq!(loaded.user.nickname, Settings::default().user.nickname);
        assert!(!path.exists(), "moved out of the next save's way");
        assert!(fs::read_to_string(dir.join("settings.json.corrupt"))
            .expect("kept")
            .contains("cut off mid-wri"));
        // With no file at all, the old install is where settings come
        // from, once.
        let migrated = Settings::load_at(&path, std::slice::from_ref(&old));
        assert_eq!(migrated.user.nickname, "from an old install");
        assert!(path.exists(), "saved at the new place");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A section missing one key keeps the rest of the file; a save
    /// that changes nothing writes nothing (review 2026-10, P3-43).
    #[test]
    fn a_missing_key_takes_its_default_and_an_unchanged_save_writes_nothing() {
        let dir = scratch("serde-default");
        let path = dir.join("settings.json");
        // `audio.gate_threshold` and `window.height` left out.
        fs::write(
            &path,
            r#"{"user":{"nickname":"kept"},"audio":{"input":{"device":null,"gain":1.0},"output":{"device":null,"gain":0.5}},"window":{"width":800}}"#,
        )
        .expect("write");
        let loaded = Settings::load_at(&path, &[]);
        assert_eq!(loaded.user.nickname, "kept");
        assert!((loaded.audio.output.gain - 0.5).abs() < f32::EPSILON);
        assert_eq!(loaded.window.width, 800);
        assert_eq!(loaded.window.height, WindowSettings::default().height);
        loaded.save_at(&path);
        let backup = dir.join("settings.json.bak");
        let _ = fs::remove_file(&backup);
        loaded.save_at(&path);
        assert!(!backup.exists(), "nothing changed: nothing rewritten");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The identity and the avatar move with the settings: the user is
    /// not someone new in the new folder (review 2026-10, P2-17).
    #[test]
    fn a_migration_brings_the_identity_and_avatar_along() {
        let old_dir = scratch("migrate-old");
        let new_dir = scratch("migrate-new");
        let old = old_dir.join("settings.json");
        named("moved").save_at(&old);
        fs::write(old_dir.join("identity.key"), b"key bytes").expect("write");
        fs::write(old_dir.join("avatar.png"), b"png bytes").expect("write");
        let path = new_dir.join("settings.json");
        assert_eq!(
            Settings::load_at(&path, std::slice::from_ref(&old))
                .user
                .nickname,
            "moved"
        );
        assert_eq!(
            fs::read(new_dir.join("identity.key")).expect("copied"),
            b"key bytes"
        );
        assert_eq!(
            fs::read(new_dir.join("avatar.png")).expect("copied"),
            b"png bytes"
        );
        assert!(old_dir.join("identity.key").exists(), "copied, not moved");
        let _ = fs::remove_dir_all(&old_dir);
        let _ = fs::remove_dir_all(&new_dir);
    }

    /// Each save keeps the one before it, and that is what loads when
    /// the current file is torn or gone (plan CLI-6).
    #[test]
    fn a_save_keeps_the_previous_one_and_it_is_the_fallback() {
        let dir = scratch("backup");
        let path = dir.join("settings.json");
        named("first").save_at(&path);
        assert!(!dir.join("settings.json.bak").exists());
        named("second").save_at(&path);
        assert!(!dir.join("settings.json.tmp").exists());
        assert_eq!(Settings::load_at(&path, &[]).user.nickname, "second");

        // Torn: the save before it loads, not the defaults.
        fs::write(&path, "{").expect("write");
        assert_eq!(Settings::load_at(&path, &[]).user.nickname, "first");
        // Gone (a save cut between its renames): the same.
        let _ = fs::remove_file(&path);
        assert_eq!(Settings::load_at(&path, &[]).user.nickname, "first");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Volume and mute follow the person, not the name: an entry kept
    /// under a name moves to its owner's id the first time they are
    /// seen, and from then on a namesake gets nothing and a rename
    /// loses nothing (review P2-24).
    #[test]
    fn peer_audio_is_kept_by_who_not_by_what_they_are_called() {
        let quiet = PeerAudioSettings {
            volume: 0.4,
            ..PeerAudioSettings::default()
        };
        let (ana, namesake) = ([1u8; 32], [2u8; 32]);
        let mut s = Settings::default();
        s.peer_settings.insert(peer_key("ana"), quiet.clone());

        let (saved, moved) = s.claim_peer_audio(&ana, "ana");
        assert_eq!(saved.map(|a| a.volume), Some(0.4));
        assert!(moved, "from the name to the id: save");
        assert!(!s.peer_settings.contains_key("ana"));
        // Someone else by that name, later: nothing to inherit.
        assert_eq!(s.claim_peer_audio(&namesake, "ana"), (None, false));
        // Renamed: still hers, and nothing more to move.
        let (saved, moved) = s.claim_peer_audio(&ana, "ana maria");
        assert_eq!(saved.map(|a| a.volume), Some(0.4));
        assert!(!moved);

        s.set_peer_audio(Some(&ana), "ana maria", None);
        assert!(s.peer_settings.is_empty());
        // No id yet (a session whose details have not come): the name.
        s.set_peer_audio(None, "bo", Some(quiet.clone()));
        assert!(s.peer_settings.contains_key("bo"));
        // Then the id is known: under it, and the name's entry goes.
        s.set_peer_audio(Some(&namesake), "bo", Some(quiet));
        assert_eq!(s.peer_settings.len(), 1);
        assert!(s.peer_settings.contains_key(&peer_id_key(&namesake)));
        assert!(peer_id_key(&namesake).starts_with("id:0202"));
    }

    fn parse(json: &str) -> Settings {
        Settings::from_json(json).expect("settings should deserialize")
    }

    /// A file from before 0.3 — `server` an object, no list — still
    /// loads: what it says about the person survives, and the server is
    /// the default one.
    #[test]
    fn a_pre_0_3_file_keeps_everything_but_its_inline_server() {
        let s = parse(
            r#"{
            "user": { "nickname": "empe" },
            "server": {
                "address": "10.0.0.7:6000",
                "token_url": "https://vc.example.com/token",
                "token_password": "sekrit"
            },
            "keybinds": [ { "action": "toggle_mute", "bind": "F2" } ]
        }"#,
        );
        assert_eq!(s.server, "baucord");
        assert_eq!(s.servers, vec![ServerEntry::official()]);
        assert_eq!(s.user.nickname, "empe");
        assert_eq!(s.keybinds[0].bind, "F2");
    }

    #[test]
    fn the_current_format_round_trips_and_old_keys_are_ignored() {
        let s = parse(
            r#"{
            "server": "baucord",
            "servers": [{
                "name": "baucord",
                "address": "216.238.121.18:5000",
                "token_url": "https://auth.baucord.com/token",
                "token_password": ""
            }]
        }"#,
        );
        assert_eq!(s.server, "baucord");
        // The official server's old port is followed to the new one.
        assert_eq!(s.servers[0].address, OFFICIAL_ADDRESS);

        let json = serde_json::to_string_pretty(&s).expect("serialize");
        assert!(!json.contains("password"), "the 1.0 room password is gone");
        let back = parse(&json);
        assert_eq!(back.server, "baucord");
        assert_eq!(back.servers, s.servers);
    }

    #[test]
    fn dangling_default_name_falls_back_to_first_entry() {
        let s = parse(
            r#"{ "server": "gone",
                 "servers": [ { "name": "a", "address": "1.2.3.4:1" },
                              { "name": "b", "address": "1.2.3.4:2" } ] }"#,
        );
        assert_eq!(s.server, "a");
    }

    #[test]
    fn empty_file_gets_official_default() {
        let s = parse("{}");
        assert_eq!(s.server, "baucord");
        assert_eq!(s.servers, vec![ServerEntry::official()]);
    }

    #[test]
    fn default_server_accessors_follow_the_name() {
        let mut s = parse(
            r#"{ "server": "b",
                 "servers": [ { "name": "a", "address": "1.1.1.1:1" },
                              { "name": "b", "address": "2.2.2.2:2" } ] }"#,
        );
        assert_eq!(
            s.default_server().map(|e| e.address.as_str()),
            Some("2.2.2.2:2")
        );
        if let Some(server) = s.default_server_mut() {
            server.address = "3.3.3.3:3".to_string();
        }
        assert_eq!(s.servers[1].address, "3.3.3.3:3");
    }

    #[test]
    fn one_point_oh_fields_round_trip_and_invites_parse() {
        let s = parse(
            r#"{ "server": "x", "servers": [ { "name": "x", "address": "1.2.3.4:5000",
                "password": "pw", "fingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" } ] }"#,
        );
        assert!(s.servers[0].fingerprint.is_some());
        // A malformed pin is dropped rather than trusted.
        let s = parse(
            r#"{ "server": "x", "servers": [ { "name": "x", "address": "1.2.3.4:5000",
                "fingerprint": "garbage" } ] }"#,
        );
        assert_eq!(s.servers[0].fingerprint, None);

        // Codes are stored canonical and sent once.
        let mut s = parse(
            r#"{ "server": "x", "servers": [ { "name": "x", "address": "1.2.3.4:5000",
                "invite_code": "k7fq 2mx9 abcd" } ] }"#,
        );
        assert_eq!(s.servers[0].invite_code.as_deref(), Some("K7FQ2MX9ABCD"));
        assert!(!s.servers[0].set_invite_input("garbage"));
        assert!(s.servers[0].set_invite_input("abcd-efgh-jkmn"));
        assert_eq!(s.servers[0].invite_code.as_deref(), Some("ABCDEFGHJKMN"));
        assert!(s.servers[0].set_invite_input("  "));
        assert_eq!(s.servers[0].invite_code, None);
    }

    #[test]
    fn channel_passwords_round_trip_and_default_empty() {
        // Old files without the key: empty map, no error.
        let s = parse(r#"{ "servers": [ { "name": "x", "address": "1.2.3.4:5" } ] }"#);
        assert!(s.servers[0].channel_passwords.is_empty());

        // Saved passwords survive a serialize/deserialize cycle.
        let mut s = Settings::default();
        s.default_server_mut()
            .expect("default entry exists")
            .channel_passwords
            .insert("Gaming".to_string(), "hunter2".to_string());
        let json = serde_json::to_string(&s).expect("serialize");
        let back = parse(&json);
        assert_eq!(
            back.servers[0]
                .channel_passwords
                .get("Gaming")
                .map(String::as_str),
            Some("hunter2")
        );
    }

    /// A 1.2 test build kept which terms had been accepted
    /// (`terms_version`); the connect card says it now. A file that
    /// still has the field loads all the same.
    #[test]
    fn a_file_from_the_terms_dialog_builds_still_loads() {
        let s = parse(
            r#"{ "terms_version": 1, "servers": [ { "name": "x", "address": "1.2.3.4:5" } ] }"#,
        );
        assert_eq!(s.servers[0].name, "x");
    }

    #[test]
    fn legacy_object_is_not_duplicated_when_servers_list_also_present() {
        // A half-migrated / hand-edited file: legacy object AND a list
        // already containing the same name — the object must not dupe.
        let s = parse(
            r#"{ "server": { "address": "64.176.16.18:5000" },
                 "servers": [ { "name": "baucord", "address": "216.238.121.18:5000" } ] }"#,
        );
        assert_eq!(s.servers.len(), 1);
        assert_eq!(s.server, "baucord");
    }
}
