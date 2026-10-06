use std::{collections::HashMap, fs, path::PathBuf};

use audio::PeerAudioSettings;
use log::info;
use serde::{Deserialize, Serialize};

/// Key used to store per-peer audio settings.
///
/// Historically this hashed the username with `DefaultHasher`, but that
/// hash is explicitly not guaranteed stable across Rust releases — a
/// toolchain bump could silently orphan every saved per-peer setting.
/// The username itself is the key now. Old hashed entries remain in
/// settings.json but are simply never read again (harmless dead weight).
#[must_use]
pub fn peer_key(username: &str) -> String {
    username.to_string()
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
pub struct UserSettings {
    pub nickname: String,
}

/// How the app itself reads and sounds — nothing a server sees.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Someone in our channel starts sharing their screen.
    #[serde(default = "default_true")]
    pub share_started: bool,
    /// Someone starts watching the stream we share (a softer sound,
    /// to the sharer alone).
    #[serde(default = "default_true")]
    pub viewer_joined: bool,
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
}

/// What happens when the app launches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// The 1.0 room password. Servers ignore it since 1.1 (the device
    /// key is the login and invites are the door); kept so old files
    /// load and old entries survive a round trip.
    #[serde(
        default,
        alias = "token_password",
        skip_serializing_if = "Option::is_none"
    )]
    pub password: Option<String>,
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
    /// password just re-opens the prompt). Same plain-text rationale as
    /// `password` above.
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
            password: None,
            invite_code: None,
            fingerprint: None,
            channel_passwords: HashMap::new(),
        }
    }

    /// Parse the address line the server prints at startup:
    /// `host:port` or `host:port#SHA256:…` (the pin is optional; it is
    /// learned on first connect otherwise). The name defaults to the
    /// address.
    #[must_use]
    pub fn from_address(line: &str) -> Option<Self> {
        let line = line.trim();
        let (address, fingerprint) = match line.split_once('#') {
            Some((addr, fp)) => (addr.trim(), Some(fp.trim())),
            None => (line, None),
        };
        if address.is_empty() || !address.contains(':') {
            return None;
        }
        let fingerprint = match fingerprint {
            Some(fp) if !fp.is_empty() => {
                // Only a well-formed fingerprint is worth pinning.
                fp.parse::<transport::Fingerprint>().ok()?;
                Some(fp.to_string())
            }
            _ => None,
        };
        Some(Self {
            name: address.to_string(),
            address: address.to_string(),
            password: None,
            invite_code: None,
            fingerprint,
            channel_passwords: HashMap::new(),
        })
    }

    /// The address with the pin once known, the way the server prints
    /// it — what to hand a friend along with a code.
    #[must_use]
    pub fn pinned_address(&self) -> String {
        match &self.fingerprint {
            Some(fp) => format!("{}#{fp}", self.address),
            None => self.address.clone(),
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

/// The pre-0.3 single inline `server` object, kept so old settings.json
/// files still deserialize (they migrate to a `servers` entry on load).
#[derive(Debug, Deserialize)]
struct LegacyServer {
    address: String,
    #[serde(default)]
    token_password: Option<String>,
}

/// Address of the retired official server: a legacy file pointing there
/// migrates to [`ServerEntry::official`] instead of keeping dead config.
const LEGACY_OFFICIAL_ADDRESS: &str = "64.176.16.18:5000";
/// The official server, on the port it moved to in 1.1.5.
const OFFICIAL_ADDRESS: &str = "216.238.121.18:51000";
/// The official server's old port: still answered (Docker publishes
/// both), but a saved entry pointing there is moved on load — the
/// port is the reason three users on one ISP heard each other at
/// 16 kbps (the session guide's Movistar case).
const OFFICIAL_ADDRESS_OLD_PORT: &str = "216.238.121.18:5000";

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input: AudioInputSettings::default(),
            output: AudioOutputSettings::default(),
            gate_threshold: 0.010,
            push_to_talk: false,
            echo_cancellation: audio::AecMode::Auto,
        }
    }
}

/// The main window's size. Its *position* is not kept (1.1.5): a
/// saved spot on a monitor that is no longer there lost people their
/// window, so every launch centres it on the monitor it opens on.
/// Older files still carry `x`/`y`; serde ignores them.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Default for ShareSettings {
    fn default() -> Self {
        Self {
            quality: "fhd30".to_string(),
            monitor: None,
            everyone: false,
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
#[serde(from = "SettingsDe")]
pub struct Settings {
    #[serde(default)]
    pub user: UserSettings,
    /// Name of the default server in [`Self::servers`] — the entry
    /// preselected on the connect screen at startup.
    pub server: String,
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
}

/// How much `baucord.log` records (1.1.5). `RUST_LOG` in the
/// environment still wins when set.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Deserialization shadow of [`Settings`]: `server` accepts both the
/// current name string and the pre-0.3 inline object, so upgrading
/// users keep their custom server instead of falling back to defaults
/// (a parse failure would discard the whole file — nickname, keybinds,
/// per-peer audio and all).
#[derive(Deserialize)]
struct SettingsDe {
    #[serde(default)]
    user: UserSettings,
    #[serde(default)]
    server: ServerField,
    #[serde(default)]
    servers: Vec<ServerEntry>,
    #[serde(default)]
    audio: AudioSettings,
    #[serde(default)]
    window: WindowSettings,
    #[serde(default = "default_keybinds")]
    keybinds: Vec<KeybindEntry>,
    #[serde(default)]
    peer_settings: HashMap<String, PeerAudioSettings>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    share: ShareSettings,
    #[serde(default)]
    ui: UiSettings,
    #[serde(default)]
    profile: ProfileSettings,
    #[serde(default)]
    startup: StartupSettings,
    #[serde(default)]
    sounds: SoundSettings,
    #[serde(default)]
    logging: LoggingSettings,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ServerField {
    Name(String),
    Legacy(LegacyServer),
}

impl Default for ServerField {
    fn default() -> Self {
        ServerField::Name(String::new())
    }
}

impl From<SettingsDe> for Settings {
    fn from(de: SettingsDe) -> Self {
        let mut servers = de.servers;
        let server = match de.server {
            ServerField::Name(name) => name,
            ServerField::Legacy(legacy) => {
                // Pre-0.3 file. The retired official default becomes
                // the new official entry; a custom server is preserved
                // under its address as the name.
                let entry = if legacy.address == LEGACY_OFFICIAL_ADDRESS {
                    ServerEntry::official()
                } else {
                    ServerEntry {
                        name: legacy.address.clone(),
                        address: legacy.address,
                        password: legacy.token_password,
                        invite_code: None,
                        fingerprint: None,
                        channel_passwords: HashMap::new(),
                    }
                };
                let name = entry.name.clone();
                if !servers.iter().any(|s| s.name == entry.name) {
                    servers.insert(0, entry);
                }
                name
            }
        };
        let mut settings = Settings {
            user: de.user,
            server,
            servers,
            audio: de.audio,
            window: de.window,
            keybinds: de.keybinds,
            peer_settings: de.peer_settings,
            language: de.language,
            share: de.share,
            ui: de.ui,
            profile: de.profile,
            startup: de.startup,
            sounds: de.sounds,
            logging: de.logging,
        };
        settings.normalize_servers();
        settings
    }
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

    fn read_from(path: &PathBuf) -> Option<Self> {
        if !path.exists() {
            return None;
        }
        let contents = fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    #[must_use]
    pub fn load() -> Self {
        let path = Self::get_settings_path();
        if let Some(settings) = Self::read_from(&path) {
            info!("Loaded settings from {}", path.display());
            return settings;
        }

        // One-time migration from previous locations, newest scheme
        // first — the 0.2 platform config dir, then the pre-0.2
        // exe-adjacent file — so upgrading users keep their nickname,
        // keybinds, and per-peer audio settings.
        let old_paths = [
            platform_config_dir().map(|d| d.join("settings.json")),
            Some(Self::exe_adjacent_settings_path()),
        ];
        for old in old_paths.into_iter().flatten() {
            if old != path
                && let Some(settings) = Self::read_from(&old)
            {
                info!(
                    "Migrating settings from {} to {}",
                    old.display(),
                    path.display()
                );
                settings.save();
                return settings;
            }
        }

        info!("Using default settings");
        Settings::default()
    }

    pub fn save(&self) {
        let path = Self::get_settings_path();
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            log::error!(
                "Failed to create settings directory {}: {e}",
                parent.display()
            );
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(contents) => {
                if let Err(e) = fs::write(&path, contents) {
                    log::error!("Failed to save settings: {e}");
                } else {
                    info!("Settings saved to {}", path.display());
                }
            }
            Err(e) => log::error!("Failed to serialize settings: {e}"),
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
            if blank(&entry.password) {
                entry.password = None;
            }
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
/// transfer per client per change): `avatars/<hex>` next to settings.
#[must_use]
pub fn avatar_cache_dir() -> PathBuf {
    config_base_dir().join("avatars")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Settings {
        serde_json::from_str(json).expect("settings should deserialize")
    }

    #[test]
    fn legacy_custom_server_migrates_to_entry_named_by_address() {
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
        assert_eq!(s.server, "10.0.0.7:6000");
        assert_eq!(s.servers.len(), 1);
        let e = &s.servers[0];
        assert_eq!(e.address, "10.0.0.7:6000");
        // The 0.5 token password becomes the room password; the issuer
        // URL is gone with the issuer.
        // The 0.5 token password still loads (a 1.1 server ignores it).
        assert_eq!(e.password.as_deref(), Some("sekrit"));
        assert_eq!(e.invite_code, None);
        assert_eq!(e.fingerprint, None);
        // Pre-existing settings survive the migration.
        assert_eq!(s.user.nickname, "empe");
        assert_eq!(s.keybinds[0].bind, "F2");
    }

    #[test]
    fn legacy_official_server_becomes_new_official_entry() {
        let s = parse(
            r#"{ "server": { "address": "64.176.16.18:5000",
                "token_url": "https://key.mpw.sh/token", "token_password": null } }"#,
        );
        assert_eq!(s.server, "baucord");
        assert_eq!(s.servers, vec![ServerEntry::official()]);
    }

    #[test]
    fn new_format_round_trips_and_normalizes_blank_password() {
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
        assert_eq!(s.servers[0].password, None);
        // The official server's old port is followed to the new one.
        assert_eq!(s.servers[0].address, OFFICIAL_ADDRESS);

        let json = serde_json::to_string_pretty(&s).expect("serialize");
        let back: Settings = serde_json::from_str(&json).expect("re-parse");
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
            server.password = Some("pw".to_string());
        }
        assert_eq!(s.servers[1].password.as_deref(), Some("pw"));
    }

    #[test]
    fn one_point_oh_fields_round_trip_and_invites_parse() {
        let s = parse(
            r#"{ "server": "x", "servers": [ { "name": "x", "address": "1.2.3.4:5000",
                "password": "pw", "fingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" } ] }"#,
        );
        assert_eq!(s.servers[0].password.as_deref(), Some("pw"));
        assert!(s.servers[0].fingerprint.is_some());
        assert_eq!(
            s.servers[0].pinned_address(),
            "1.2.3.4:5000#SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
        // A malformed pin is dropped rather than trusted.
        let s = parse(
            r#"{ "server": "x", "servers": [ { "name": "x", "address": "1.2.3.4:5000",
                "fingerprint": "garbage" } ] }"#,
        );
        assert_eq!(s.servers[0].fingerprint, None);

        let e = ServerEntry::from_address(
            " 10.0.0.1:5000#SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= ",
        )
        .expect("address");
        assert_eq!(e.address, "10.0.0.1:5000");
        assert!(e.fingerprint.is_some());
        assert_eq!(
            ServerEntry::from_address("10.0.0.1:5000")
                .expect("bare")
                .fingerprint,
            None
        );
        assert!(ServerEntry::from_address("nope").is_none());
        assert!(ServerEntry::from_address("10.0.0.1:5000#bad").is_none());

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
