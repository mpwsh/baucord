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
    /// Actions shown in the keybind UI. `PushToTalk` is intentionally
    /// excluded (not implemented yet) — but the enum variant must stay:
    /// removing it would break deserialization of settings.json files
    /// that already contain a `push_to_talk` keybind entry.
    pub const ALL: &'static [BindableAction] = &[
        BindableAction::ToggleMute,
        BindableAction::ToggleDeafen,
        BindableAction::ToggleScreenShare,
    ];

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

/// One saved server the user can connect to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerEntry {
    /// Display name, and what `Settings::server` points at.
    pub name: String,
    /// `ip:port` (or `[v6]:port`) — parsed as a `SocketAddr` on connect.
    pub address: String,
    /// Shared room password sent in `Hello`, if the server requires one
    /// (`authentication.secure: true`).
    ///
    /// Stored in settings.json as plain text on purpose: this is a
    /// shared room secret for a friends server, not a personal
    /// credential — treat it accordingly (and don't reuse a personal
    /// password for it). An OS-keychain integration would add a
    /// per-platform dependency for a secret every member knows.
    ///
    /// (0.5 files called this `token_password`; it migrates on load.)
    #[serde(default, alias = "token_password")]
    pub password: Option<String>,
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
            address: "216.238.121.18:5000".to_string(),
            password: Some("conos".to_string()),
            fingerprint: None,
            channel_passwords: HashMap::new(),
        }
    }

    /// Parse an invite string: `host:port` or `host:port#SHA256:…`
    /// (spec §3.2). The name defaults to the address.
    #[must_use]
    pub fn from_invite(invite: &str) -> Option<Self> {
        let invite = invite.trim();
        let (address, fingerprint) = match invite.split_once('#') {
            Some((addr, fp)) => (addr.trim(), Some(fp.trim())),
            None => (invite, None),
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
            fingerprint,
            channel_passwords: HashMap::new(),
        })
    }

    /// The invite string for this entry (with the pin, once known).
    #[must_use]
    pub fn invite(&self) -> String {
        match &self.fingerprint {
            Some(fp) => format!("{}#{fp}", self.address),
            None => self.address.clone(),
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
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input: AudioInputSettings::default(),
            output: AudioOutputSettings::default(),
            gate_threshold: 0.010,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSettings {
    pub width: i32,
    pub height: i32,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            width: 1000,
            height: 700,
            x: None,
            y: None,
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

    pub fn load() -> Self {
        let path = Self::get_settings_path();
        if let Some(settings) = Self::read_from(&path) {
            info!("Loaded settings from {:?}", path);
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
                info!("Migrating settings from {:?} to {:?}", old, path);
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
            log::error!("Failed to create settings directory {:?}: {e}", parent);
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(contents) => {
                if let Err(e) = fs::write(&path, contents) {
                    log::error!("Failed to save settings: {e}");
                } else {
                    info!("Settings saved to {:?}", path);
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
            let blank = |v: &Option<String>| v.as_deref().is_some_and(|s| s.trim().is_empty());
            if blank(&entry.password) {
                entry.password = None;
            }
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

    pub fn bind_for(&self, action: BindableAction) -> &str {
        self.keybinds
            .iter()
            .find(|e| e.action == action)
            .map(|e| e.bind.as_str())
            .unwrap_or("")
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
    #[cfg(not(debug_assertions))]
    {
        if let Some(dir) = writable_exe_dir() {
            return dir;
        }
    }
    platform_config_dir().unwrap_or_else(|| PathBuf::from("."))
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

/// Whether settings live next to the executable (a portable install),
/// for the telemetry client info.
#[must_use]
pub fn is_portable_install() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .is_some_and(|dir| dir == config_base_dir())
}

/// Load (or create) the client's identity key. Failure is logged and
/// treated as "no key": the session still works, without a stable user
/// id.
#[must_use]
pub fn load_client_key() -> Option<std::sync::Arc<transport::ClientKey>> {
    let path = client_key_path();
    match transport::ClientKey::load_or_create(&path) {
        Ok(key) => Some(std::sync::Arc::new(key)),
        Err(e) => {
            log::warn!("client key unavailable ({e}); connecting without one");
            None
        }
    }
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
        assert_eq!(e.password.as_deref(), Some("sekrit"));
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
            s.servers[0].invite(),
            "1.2.3.4:5000#SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
        // A malformed pin is dropped rather than trusted.
        let s = parse(
            r#"{ "server": "x", "servers": [ { "name": "x", "address": "1.2.3.4:5000",
                "fingerprint": "garbage" } ] }"#,
        );
        assert_eq!(s.servers[0].fingerprint, None);

        let e = ServerEntry::from_invite(
            " 10.0.0.1:5000#SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= ",
        )
        .expect("invite");
        assert_eq!(e.address, "10.0.0.1:5000");
        assert!(e.fingerprint.is_some());
        assert_eq!(
            ServerEntry::from_invite("10.0.0.1:5000")
                .expect("bare")
                .fingerprint,
            None
        );
        assert!(ServerEntry::from_invite("nope").is_none());
        assert!(ServerEntry::from_invite("10.0.0.1:5000#bad").is_none());
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
