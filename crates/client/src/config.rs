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
    /// Canonical settings location: the platform config directory
    /// (`%APPDATA%\baucord`, `~/.config/baucord`, `~/Library/Application
    /// Support/baucord`). The old location — next to the executable —
    /// broke under any install layout where the exe directory isn't
    /// writable (Program Files, /usr/bin, macOS app bundles) and made
    /// every new build directory silently reset the user's settings.
    fn get_settings_path() -> PathBuf {
        match dirs::config_dir() {
            Some(dir) => dir.join("baucord").join("settings.json"),
            None => Self::legacy_settings_path(),
        }
    }

    /// Pre-0.2 location: next to the executable.
    fn legacy_settings_path() -> PathBuf {
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

        // One-time migration from the old exe-adjacent location, so
        // upgrading users keep their nickname, keybinds, and per-peer
        // audio settings.
        let legacy = Self::legacy_settings_path();
        if legacy != path
            && let Some(settings) = Self::read_from(&legacy)
        {
            info!("Migrating settings from {:?} to {:?}", legacy, path);
            settings.save();
            return settings;
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
