// ui/messages.rs
use renet::ClientId;

/// Result of the background version check.
#[derive(Debug, Clone)]
pub enum VersionCheckResult {
    /// Remote version is newer.
    UpdateAvailable { remote_version: String },
    /// Up to date, or the check failed (don't block the user).
    UpToDate,
}

#[derive(Debug, Clone)]
pub enum UiMessage {
    // ── Connection flow ─────────────────────────────────────────────
    Connect {
        nickname: String,
        server: String,
    },
    Disconnect,

    // ── Chat ────────────────────────────────────────────────────────
    SendChatMessage(String),

    // ── Voice controls ──────────────────────────────────────────────
    ToggleMute,
    ToggleDeafen,
    TestMicrophone(bool),
    SetGateThreshold(f32),
    SetInputGain(f32),
    SetOutputGain(f32),

    // ── Per-user audio ──────────────────────────────────────────────
    MuteUser {
        client_id: ClientId,
        username: String,
        muted: bool,
    },
    SetUserVolume {
        client_id: ClientId,
        username: String,
        volume: f32,
    },
    ResetUserAudio {
        client_id: ClientId,
        username: String,
    },

    // ── Screen share ────────────────────────────────────────────────
    /// User clicked "Share Screen" — start capturing.
    StartShareScreen,
    /// User clicked "Stop Sharing" — stop capturing.
    StopShareScreen,
    /// User clicked "Watch Stream" on a peer's context menu.
    WatchPeer { client_id: ClientId },
    /// User closed a viewer window — unsubscribe from that peer's stream.
    UnwatchPeer { client_id: u64 },

    // ── Per-user audio popup ────────────────────────────────────────
    /// Open the per-user context menu (volume, mute, watch stream).
    OpenUserAudioPopup {
        client_id: ClientId,
        username: String,
        click_x: i32,
        click_y: i32,
    },

    // ── Settings window ─────────────────────────────────────────────
    OpenSettings,
    SaveSettings,
    ResetSettings,
    SettingsWindowClosed,

    // ── Device management ───────────────────────────────────────────
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),

    // ── Update flow ─────────────────────────────────────────────────
    /// Background version check finished.
    VersionCheckComplete(VersionCheckResult),
    /// User chose to skip the update and proceed to connect.
    SkipUpdate,
    /// User clicked "Update" — start the download.
    StartDownload(String),
    /// Download thread finished successfully. Contains the file path.
    DownloadComplete(String),
    /// Download thread hit an error.
    DownloadFailed(String),
    /// User cancelled an in-progress download.
    DownloadCancelled,
    /// User clicked "Open File" — open the zip and shut down.
    OpenUpdateAndQuit(String),

    // ── UI updates ──────────────────────────────────────────────────
    UpdateStreamStats,
    ShowError(String),
    ClearError,

    // ── Keybinds ────────────────────────────────────────────────────
    SetKeybind {
        action: crate::config::BindableAction,
        bind: String,
    },

    // ── Application lifecycle ───────────────────────────────────────
    Shutdown,
}