// ui/messages.rs
use renet::ClientId;

/// Result of the background version check.
#[derive(Debug, Clone, PartialEq)]
pub enum VersionCheckResult {
    /// Remote version is newer.
    UpdateAvailable { remote_version: String },
    /// Up to date, or the check failed (don't block the user).
    UpToDate,
}

#[derive(Debug, Clone, PartialEq)]
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
    ToggleScreenShare,
    /// User confirmed screen share settings from the popup.
    StartScreenShare {
        quality: video::Quality,
        monitor_index: usize,
    },
    /// User clicked "Watch Stream" on a peer's context menu.
    WatchPeer {
        client_id: ClientId,
    },
    /// User closed a viewer window — unsubscribe from that peer's stream.
    UnwatchPeer {
        client_id: u64,
    },

    // ── Stream audio controls ───────────────────────────────────────
    /// Viewer volume slider changed.
    SetStreamVolume {
        client_id: u64,
        volume: f32,
    },
    /// Viewer mute button toggled.
    MuteStream {
        client_id: u64,
        muted: bool,
    },

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

    /// Fired by the device-polling timer when the set of audio devices
    /// (or the identity of the system default) has changed.
    DevicesChanged {
        input_devices: Vec<(String, bool)>,
        output_devices: Vec<(String, bool)>,
    },

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
    /// User clicked "Install now" — open the zip and shut down.
    InstallUpdate(String),

    // ── UI updates ──────────────────────────────────────────────────
    UpdateStreamStats,
    ShowError(String),
    ClearError,

    // ── Keybinds ────────────────────────────────────────────────────
    SetKeybind {
        action: client::config::BindableAction,
        bind: String,
    },
    /// User clicked "Show" in tray menu — restore the main window.
    RestoreFromTray,
    /// User clicked "Quit" in tray menu — actually shut down.
    QuitFromTray,

    /// Change the UI language. `None` = follow the OS locale.
    SetLanguage(Option<String>),

    // ── Application lifecycle ───────────────────────────────────────
    /// OS close button on the main window. App decides what it means:
    /// hide to tray when connected, quit from the connect view.
    MainWindowClose,
    /// Explicit quit — the Quit button on the connect view.
    Shutdown,
    /// Deferred voice-engine stop after a disconnect, so the leave
    /// sound can finish before the output stream is torn down.
    StopVoiceAfterDisconnect,
}
