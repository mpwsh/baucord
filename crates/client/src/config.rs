use log::info;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use audio::PeerAudioSettings;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub address: String,
    /// Token issuer URL for secure servers, e.g.
    /// `https://vc.example.com/token` (the deployment's reverse proxy
    /// routes this to the server's in-process issuer).
    ///
    /// `Some` → fetch a connect token and authenticate securely.
    /// `None` → open (unsecure) connect, for LAN/dev servers running
    /// with `authentication.secure: false`.
    #[serde(default)]
    pub token_url: Option<String>,
    /// Shared password sent to the issuer in the `x-baucord-pass`
    /// header, if the server configures one.
    ///
    /// Stored in settings.json as plain text on purpose: this is a
    /// shared room secret for a friends server, not a personal
    /// credential — treat it accordingly (and don't reuse a personal
    /// password for it). An OS-keychain integration would add a
    /// per-platform dependency for a secret every member knows.
    #[serde(default)]
    pub token_password: Option<String>,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            address: "64.176.16.18:5000".to_string(),
            token_url: Some("https://key.mpw.sh/token".to_string()),
            token_password: None,
        }
    }
}

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
            gate_threshold: 0.005,
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
    #[serde(default)]
    pub server: ServerSettings,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            user: UserSettings::default(),
            server: ServerSettings::default(),
            audio: AudioSettings::default(),
            window: WindowSettings::default(),
            keybinds: default_keybinds(),
            peer_settings: HashMap::new(),
            language: None,
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
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
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

/// Directory holding baucord's config files (settings.json,
/// windows.json).
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

// ── Window-position store ───────────────────────────────────────────────
//
// Deliberately a separate tiny file, NOT part of `Settings`: dialogs
// persist their position with a load-modify-save on close, and going
// through Settings would race the app's own in-memory copy (whichever
// saved last would clobber the other's changes).

fn window_positions_path() -> PathBuf {
    config_base_dir().join("windows.json")
}

fn read_window_positions() -> HashMap<String, (i32, i32)> {
    let read = |p: PathBuf| -> Option<HashMap<String, (i32, i32)>> {
        serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
    };
    read(window_positions_path())
        // Positions written before the storage move (or by a debug
        // build) live in the platform config dir — read them as a
        // fallback rather than forgetting every window position once.
        .or_else(|| {
            platform_config_dir()
                .map(|d| d.join("windows.json"))
                .and_then(read)
        })
        .unwrap_or_default()
}

/// Last saved position of the named window, if any. Callers should
/// clamp to the current screen before applying — monitors change.
#[must_use]
pub fn window_pos(key: &str) -> Option<(i32, i32)> {
    read_window_positions().get(key).copied()
}

/// Persist the named window's position. Failures are silent — losing a
/// window position is not worth an error dialog.
pub fn set_window_pos(key: &str, pos: (i32, i32)) {
    let path = window_positions_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut map = read_window_positions();
    map.insert(key.to_string(), pos);
    if let Ok(contents) = serde_json::to_string_pretty(&map) {
        let _ = fs::write(&path, contents);
    }
}
