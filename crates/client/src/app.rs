// app.rs — Main application coordinator.
//
// Owns the FLTK event loop and delegates to:
//   - NetworkThreadHandle (network.rs) for renet client + protocol
//   - VoiceThreadHandle   (voice.rs)   for audio capture/playback/encoding
//   - ScreenThreadHandle  (screen.rs)  for screen capture/encode/decode
//
// Speaking state is driven by vc.is_speaking(), not inferred from packet flow.

use std::sync::atomic::{AtomicBool, Ordering};

use fltk::{prelude::*, *};
use log::{info, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trayicon::{MenuBuilder, TrayIcon, TrayIconBuilder};

use crate::{
    config::{BindableAction, Settings, peer_key},
    hotkeys::HotkeyManager,
    network::{NetworkControl, NetworkMessage, NetworkThreadHandle},
    screen::{ScreenControl, ScreenEvent, ScreenThreadHandle},
    ui::{chat::Chat, messages::UiMessage, screenshare, sidebar::Sidebar, viewer::ViewerWindow},
    voice::{VoiceControl, VoiceEvent, VoiceThreadHandle},
};
use audio::{PeerAudioSettings, SoundType};
use ui::theme;

// ── Public types ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UserState {
    pub username: String,
    pub is_speaking: bool,
    pub last_activity: Instant,
    pub audio_settings: PeerAudioSettings,
    /// Remote user's self-reported mute state (distinct from local per-user mute).
    pub is_self_muted: bool,
    /// Remote user's self-reported deafen state.
    pub is_self_deafened: bool,
}

// ── App ────────────────────────────────────────────────────────────────

pub struct App {
    app: app::App,
    sender: app::Sender<UiMessage>,
    receiver: app::Receiver<UiMessage>,

    // Thread handles (own the threads + their channels)
    network: NetworkThreadHandle,
    voice: VoiceThreadHandle,
    screen: ScreenThreadHandle,

    // State
    users: HashMap<renet::ClientId, UserState>,
    our_client_id: Option<renet::ClientId>,
    connected: bool,
    settings: Settings,
    is_muted: bool,
    is_deafened: bool,
    is_testing_mic: bool,
    is_testing_mic_shared: Arc<AtomicBool>,

    /// Active screen sharers: client_id → (width, height).
    /// Updated when ScreenShareStarted/Stopped arrives from network.
    active_sharers: HashMap<renet::ClientId, (u32, u32)>,
    viewer_windows: HashMap<u64, ViewerWindow>,

    // UI
    splash_window: Option<window::Window>,
    update_window: Option<window::Window>,
    connection_window: Option<window::Window>,
    main_window: Option<window::Window>,
    sidebar: Option<Sidebar>,
    chat: Option<Chat>,
    error_frame: Option<frame::Frame>,
    settings_window_open: bool,
    hotkeys: Option<HotkeyManager>,
    tray_icon: Option<TrayIcon<UiMessage>>,
    sentinel_window: Option<window::Window>,
}

impl App {
    pub fn new() -> Self {
        let app = app::App::default();
        theme::apply();

        let (sender, receiver) = app::channel::<UiMessage>();

        let is_testing_mic_shared = Arc::new(AtomicBool::new(false));

        // Wake callback — worker threads call this to nudge the FLTK event loop.
        // This is the ONLY place fltk::app::awake is referenced for worker threads.
        let wake: crate::voice::WakeFn = Arc::new(fltk::app::awake);

        // Spawn threads via their handles.
        // Network needs voice_tx to route incoming voice packets,
        // so we use spawn_with_channels to share the sender.
        let (network_tx, network_control_rx) = crossbeam_channel::bounded::<NetworkControl>(256);
        let (network_msg_tx, network_msg_rx) = crossbeam_channel::bounded::<NetworkMessage>(256);

        let voice = VoiceThreadHandle::spawn(
            network_tx.clone(),
            is_testing_mic_shared.clone(),
            wake.clone(),
        );

        // After spawning voice, before spawning network:
        let screen = ScreenThreadHandle::spawn(
            network_tx.clone(),
            wake.clone(),
            voice.shared.output_level.clone(),
        );

        let network = NetworkThreadHandle::spawn(
            network_tx,
            network_control_rx,
            network_msg_tx,
            network_msg_rx,
            voice.tx.clone(),
            screen.tx.clone(),
            wake,
        );

        let settings = Settings::load();

        Self {
            app,
            sender,
            receiver,
            network,
            voice,
            screen,
            users: HashMap::new(),
            our_client_id: None,
            connected: false,
            settings,
            is_muted: false,
            is_deafened: false,
            is_testing_mic: false,
            is_testing_mic_shared,
            active_sharers: HashMap::new(),
            viewer_windows: HashMap::new(),
            splash_window: None,
            update_window: None,
            connection_window: None,
            main_window: None,
            sidebar: None,
            chat: None,
            error_frame: None,
            settings_window_open: false,
            hotkeys: None,
            tray_icon: None,
            sentinel_window: None,
        }
    }

    pub fn run(&mut self) {
        // Register global hotkeys on the main thread (required by the OS).
        self.hotkeys = Some(HotkeyManager::new(&self.settings));

        let handles = crate::ui::splash::show(self.sender);
        self.splash_window = Some(handles.window);

        while self.app.wait() {
            // UI messages
            while let Some(msg) = self.receiver.recv() {
                self.handle_ui_message(msg);
            }

            // Global hotkeys — collect first, then handle (avoids borrow conflict)
            let mut hotkey_actions = Vec::new();
            if let Some(ref hotkeys) = self.hotkeys {
                while let Some(action) = hotkeys.poll() {
                    hotkey_actions.push(action);
                }
            }
            for action in hotkey_actions {
                if !self.connected {
                    continue;
                }
                match action {
                    BindableAction::ToggleMute => self.handle_toggle_mute(),
                    BindableAction::ToggleDeafen => self.handle_toggle_deafen(),
                    BindableAction::ToggleScreenShare => self.handle_toggle_screenshare(),
                    BindableAction::PushToTalk => {}
                }
            }

            // Network messages (bounded drain)
            let mut needs_redraw = false;
            for _ in 0..256 {
                match self.network.msg_rx.try_recv() {
                    Ok(msg) => {
                        self.handle_network_message(msg);
                        needs_redraw = true;
                    }
                    Err(_) => break,
                }
            }

            // Voice events (bounded drain)
            for _ in 0..32 {
                match self.voice.event_rx.try_recv() {
                    Ok(evt) => {
                        self.handle_voice_event(evt);
                        needs_redraw = true;
                    }
                    Err(_) => break,
                }
            }

            // Screen events (bounded drain)
            for _ in 0..64 {
                match self.screen.event_rx.try_recv() {
                    Ok(evt) => {
                        self.handle_screen_event(evt);
                        needs_redraw = true;
                    }
                    Err(_) => break,
                }
            }

            if needs_redraw {
                app::redraw();
            }
        }

        // FLTK loop exited (all windows closed or app.quit() called).
        self.shutdown();
    }

    // ════════════════════════════════════════════════════════════════════
    //  EVENT HANDLERS
    // ════════════════════════════════════════════════════════════════════

    fn handle_voice_event(&mut self, evt: VoiceEvent) {
        match evt {
            VoiceEvent::Started => {
                info!("Voice engine ready");
                self.voice
                    .tx
                    .send(VoiceControl::PlaySound(SoundType::UserJoined))
                    .ok();
            }
            VoiceEvent::Error(e) => {
                self.show_error(&e);
            }
            VoiceEvent::SpeakingChanged(speaking) => {
                if let Some(id) = self.our_client_id
                    && let Some(user) = self.users.get_mut(&id)
                {
                    user.is_speaking = speaking;
                    user.last_activity = Instant::now();
                }
                self.update_user_list();
            }
        }
    }

    fn handle_screen_event(&mut self, evt: ScreenEvent) {
        match evt {
            ScreenEvent::SharingStarted { quality } => {
                info!("📺 Screen share started: {quality}");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message(format!("Screen share started ({quality})"));
                }
                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.set_share_state(true);
                }
                // Start 1-second stream stats timer
                let sender = self.sender;
                app::add_timeout3(1.0, move |handle| {
                    sender.send(UiMessage::UpdateStreamStats);
                    app::repeat_timeout3(1.0, handle);
                });
            }
            ScreenEvent::SharingStopped => {
                info!("📺 Screen share stopped");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message("Screen share stopped".to_string());
                }
                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.set_share_state(false);
                }
            }
            ScreenEvent::Error(e) => {
                self.show_error(&format!("Screen share: {e}"));
            }
            ScreenEvent::FrameReady {
                client_id,
                width,
                height,
                rgba,
                frame_bytes,
                seq,
                is_keyframe,
                enqueued_at,
                decoded_at,
                buffer_depth_ms,
                buffer_target_ms,
                buffer_jitter_ms,
            } => {
                use std::time::Instant;
                let app_drain_at = Instant::now();
                let net_to_screen_ms =
                    decoded_at.duration_since(enqueued_at).as_secs_f32() * 1000.0;
                let screen_to_app_ms =
                    app_drain_at.duration_since(decoded_at).as_secs_f32() * 1000.0;
                let total_ms = app_drain_at.duration_since(enqueued_at).as_secs_f32() * 1000.0;
                let frame_type = if is_keyframe { "K" } else { "P" };
                log::debug!(
                    "📊 [seq={seq} {frame_type} {:.0}KB] net→decode: {net_to_screen_ms:.1}ms | \
        decode→app: {screen_to_app_ms:.1}ms | total: {total_ms:.1}ms | \
        buf: {buffer_depth_ms:.0}/{buffer_target_ms:.0}ms jit: {buffer_jitter_ms:.0}ms",
                    frame_bytes as f64 / 1024.0,
                );
                if total_ms > 25.0 {
                    log::warn!(
                        "📊 SLOW FRAME seq={seq}: {total_ms:.1}ms total \
            (net→decode: {net_to_screen_ms:.1}ms, decode→app: {screen_to_app_ms:.1}ms)"
                    );
                }
                if let Some(viewer) = self.viewer_windows.get_mut(&client_id) {
                    let blit_start = Instant::now();
                    viewer.update_frame(
                        rgba,
                        width,
                        height,
                        frame_bytes,
                        buffer_depth_ms,
                        buffer_target_ms,
                        buffer_jitter_ms,
                    );
                    let blit_ms = blit_start.elapsed().as_secs_f32() * 1000.0;
                    if blit_ms > 5.0 {
                        log::warn!("📊 Slow blit: {blit_ms:.1}ms for seq {seq}");
                    }
                }
            }
            ScreenEvent::PeerStartedSharing {
                client_id,
                width,
                height,
            } => {
                let username = self.username_for_u64(client_id);
                info!("📺 {username} started sharing ({width}x{height})");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message(format!("{username} started sharing their screen"));
                }
            }
            ScreenEvent::PeerStoppedSharing { client_id } => {
                let username = self.username_for_u64(client_id);
                info!("📺 {username} stopped sharing");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message(format!("{username} stopped sharing their screen"));
                }
                // Close viewer window if open
                if let Some(mut viewer) = self.viewer_windows.remove(&client_id) {
                    viewer.close();
                }
            }
        }
    }

    fn handle_network_message(&mut self, msg: NetworkMessage) {
        match msg {
            NetworkMessage::Connected => {
                self.handle_connection_established();
            }
            NetworkMessage::Disconnected(reason) => {
                if self.connected {
                    self.handle_disconnect_event(&reason);
                } else {
                    self.show_error(&reason);
                }
            }
            NetworkMessage::SystemAudioData { .. } => {
                // System audio packets are routed directly to the screen
                // thread by the network loop (hot path). They should never
                // arrive here, but if they do we silently ignore them.
            }
            NetworkMessage::InitClient {
                users,
                client_id,
                server_name,
                greeting,
            } => {
                self.handle_init_client(client_id, users, server_name, greeting);
            }
            NetworkMessage::UserJoined {
                client_id,
                username,
            } => {
                self.handle_user_joined(client_id, username);
            }
            NetworkMessage::UserLeft {
                client_id,
                username,
            } => {
                self.handle_user_left(client_id, username);
            }
            NetworkMessage::TextMessage { username, text } => {
                if let Some(chat) = &mut self.chat {
                    chat.handle_text_message(username, text);
                }
            }
            NetworkMessage::UserSpeaking {
                client_id,
                is_speaking,
            } => {
                if let Some(user) = self.users.get_mut(&client_id) {
                    user.is_speaking = is_speaking;
                    user.last_activity = Instant::now();
                }
                self.update_user_list();
            }
            NetworkMessage::UserAudioState {
                client_id,
                muted,
                deafened,
            } => {
                if let Some(user) = self.users.get_mut(&client_id) {
                    user.is_self_muted = muted;
                    user.is_self_deafened = deafened;
                }
                self.update_user_list();
            }
            NetworkMessage::VoiceData { .. } => {}
            NetworkMessage::LatencyUpdate { rtt_ms } => {
                if let Some(sidebar) = &mut self.sidebar {
                    let audio_ms = self.voice.shared.jitter_buffer_ms.load(Ordering::Relaxed);
                    sidebar.update_latency(rtt_ms, Some(audio_ms));
                }
            }

            // ── Screen share signaling ──────────────────────────────
            NetworkMessage::ScreenShareStarted {
                client_id,
                width,
                height,
            } => {
                self.active_sharers.insert(client_id, (width, height));
                self.sync_sharers_to_sidebar();
            }
            NetworkMessage::ScreenShareStopped { client_id } => {
                self.active_sharers.remove(&client_id);
                self.sync_sharers_to_sidebar();
            }
            NetworkMessage::VideoFrame { .. } => {
                // Video frames are routed directly to the screen thread by the
                // network loop. They should never arrive here, but if they do
                // we silently ignore them.
            }
        }
    }

    fn handle_ui_message(&mut self, msg: UiMessage) {
        match msg {
            UiMessage::StartScreenShare {
                quality,
                monitor_index,
            } => {
                self.screen
                    .tx
                    .send(ScreenControl::StartSharing {
                        quality,
                        monitor_index,
                    })
                    .ok();
            }
            // ── Screen share buttons ────────────────────────────────
            UiMessage::ToggleScreenShare => {
                if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                    self.screen.tx.send(ScreenControl::StopSharing).ok();
                } else if let Some(ref main_win) = self.main_window {
                    screenshare::show(self.sender, main_win);
                }
            }
            UiMessage::RestoreFromTray => {
                if let Some(w) = &mut self.main_window {
                    w.show();
                }
            }
            UiMessage::QuitFromTray => {
                self.app.quit(); // triggers shutdown() after loop exits
            }
            UiMessage::UpdateStreamStats => {
                if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                    let frame_bytes = self.screen.shared.last_frame_bytes.load(Ordering::Relaxed);
                    let encode_us = self.screen.shared.last_encode_us.load(Ordering::Relaxed);
                    let avg_kb = frame_bytes as f32 / 1024.0;
                    let encode_ms = encode_us as f32 / 1000.0;
                    let quality_str = self
                        .screen
                        .shared
                        .current_quality
                        .lock()
                        .unwrap()
                        .map(|q| q.to_string())
                        .unwrap_or_else(|| "Unknown".into());
                    if let Some(sidebar) = &mut self.sidebar {
                        sidebar.update_stream_stats(Some((&quality_str, avg_kb, encode_ms)));
                    }
                }
            }
            UiMessage::WatchPeer { client_id } => {
                let raw_id = client_id;
                if self.viewer_windows.contains_key(&raw_id) {
                    return;
                }
                let username = self.get_username(client_id);
                let viewer = ViewerWindow::new(raw_id, &username, 1280, 720, self.sender);
                self.viewer_windows.insert(raw_id, viewer);
                self.network
                    .tx
                    .send(NetworkControl::SubscribeScreen { target_id: raw_id })
                    .ok();
                self.show_chat_system(&format!("Watching {username}'s screen..."));
            }
            UiMessage::UnwatchPeer { client_id } => {
                if let Some(mut viewer) = self.viewer_windows.remove(&client_id) {
                    viewer.close();
                }
                self.network
                    .tx
                    .send(NetworkControl::UnsubscribeScreen {
                        target_id: client_id,
                    })
                    .ok();
                let username = self.username_for_u64(client_id);
                self.show_chat_system(&format!("Stopped watching {username}."));
            }

            UiMessage::SetStreamVolume { client_id, volume } => {
                self.screen
                    .tx
                    .send(ScreenControl::SetStreamVolume { client_id, volume })
                    .ok();
            }
            UiMessage::MuteStream { client_id, muted } => {
                self.screen
                    .tx
                    .send(ScreenControl::MuteStream { client_id, muted })
                    .ok();
            }

            UiMessage::Connect { nickname, server } => {
                self.handle_connect(nickname, server);
            }
            UiMessage::Disconnect => {
                self.handle_disconnect();
            }
            UiMessage::SendChatMessage(text) => {
                self.network.tx.send(NetworkControl::SendMessage(text)).ok();
            }
            UiMessage::ToggleMute => {
                self.handle_toggle_mute();
            }
            UiMessage::ToggleDeafen => {
                self.handle_toggle_deafen();
            }
            UiMessage::TestMicrophone(enabled) => {
                self.handle_test_microphone(enabled);
            }
            UiMessage::SetGateThreshold(t) => {
                self.settings.audio.gate_threshold = t;
                self.voice.tx.send(VoiceControl::SetGateThreshold(t)).ok();
            }
            UiMessage::SetInputGain(g) => {
                self.settings.audio.input.gain = g;
                self.voice.tx.send(VoiceControl::SetInputGain(g)).ok();
            }
            UiMessage::SetOutputGain(g) => {
                self.settings.audio.output.gain = g;
                self.voice.tx.send(VoiceControl::SetOutputGain(g)).ok();
            }
            UiMessage::MuteUser {
                client_id,
                username,
                muted,
            } => {
                self.handle_mute_user(client_id, username, muted);
            }
            UiMessage::SetUserVolume {
                client_id,
                username,
                volume,
            } => {
                self.handle_set_user_volume(client_id, username, volume);
            }
            UiMessage::ResetUserAudio {
                client_id,
                username,
            } => {
                self.handle_reset_peer_audio(client_id, username);
            }
            UiMessage::OpenSettings => {
                if !self.settings_window_open {
                    self.settings_window_open = true;
                    self.show_settings_window();
                }
            }
            UiMessage::OpenUserAudioPopup {
                client_id,
                username,
                click_x,
                click_y,
            } => {
                let (volume, muted) = self
                    .users
                    .get(&client_id)
                    .map(|u| (u.audio_settings.volume, u.audio_settings.is_muted))
                    .unwrap_or((1.0, false));

                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.handle_open_audio_popup(
                        client_id, &username, volume, muted, click_x, click_y,
                    );
                }
            }
            UiMessage::SaveSettings => {
                self.settings.save();
            }
            UiMessage::ResetSettings => {
                self.settings.reset();
            }
            UiMessage::SetInputDevice(d) => {
                self.settings.audio.input.device = d.clone();
                self.voice.tx.send(VoiceControl::SetInputDevice(d)).ok();
            }
            UiMessage::SetOutputDevice(d) => {
                self.settings.audio.output.device = d.clone();
                self.voice.tx.send(VoiceControl::SetOutputDevice(d)).ok();
            }

            // ── Update flow ─────────────────────────────────────────
            UiMessage::VersionCheckComplete(result) => {
                use crate::ui::messages::VersionCheckResult;
                match result {
                    VersionCheckResult::UpdateAvailable { remote_version } => {
                        if let Some(splash) = &mut self.splash_window {
                            let prompt = crate::ui::splash::show_update_prompt(
                                self.sender,
                                &remote_version,
                                splash,
                            );
                            // The splash is now hidden; track the prompt as the new splash
                            self.splash_window = Some(prompt);
                        }
                    }
                    VersionCheckResult::UpToDate => {
                        self.dismiss_update_flow_and_connect();
                    }
                }
            }
            UiMessage::SkipUpdate => {
                self.dismiss_update_flow_and_connect();
            }
            UiMessage::StartDownload(remote_version) => {
                if let Some(mut w) = self.splash_window.take() {
                    w.end(); // end any active group context
                    w.hide();
                }
                let wind = crate::ui::update::show(self.sender, &remote_version);
                self.update_window = Some(wind);
            }
            UiMessage::DownloadComplete(path) => {
                if let Some(wind) = &mut self.update_window {
                    let complete = crate::ui::update::show_complete(self.sender, &path, wind);
                    self.update_window = Some(complete);
                }
            }
            UiMessage::DownloadFailed(error) => {
                if let Some(wind) = &mut self.update_window {
                    let err_wind = crate::ui::update::show_error(self.sender, &error, wind);
                    self.update_window = Some(err_wind);
                }
            }
            UiMessage::DownloadCancelled => {
                self.dismiss_update_flow_and_connect();
            }
            UiMessage::OpenUpdateAndQuit(path) => {
                if let Some(parent) = std::path::Path::new(&path).parent()
                    && let Err(e) = open::that(parent)
                {
                    warn!("Failed to open folder: {e}");
                }
                // Open the zip itself in the system zip handler
                if let Err(e) = open::that(&path) {
                    warn!("Failed to open {path}: {e}");
                }
                self.app.quit();
            }

            UiMessage::ShowError(e) => {
                self.show_error(&e);
            }
            UiMessage::ClearError => {
                if let Some(frame) = &mut self.error_frame {
                    frame.set_label("");
                }
            }
            UiMessage::SettingsWindowClosed => {
                self.settings_window_open = false;
                if self.is_testing_mic {
                    self.handle_test_microphone(false);
                }
                self.settings.save();
            }
            UiMessage::SetKeybind { action, bind } => {
                self.settings.set_bind(action, bind);
                if let Some(ref mut hotkeys) = self.hotkeys {
                    hotkeys.update(&self.settings);
                }
            }
            UiMessage::Shutdown => {
                // Just quit FLTK — actual cleanup happens after the event
                // loop exits in run(), which calls shutdown().
                self.app.quit();
            }
        }
    }

    // ════════════════════════════════════════════════════════════════════
    //  HELPERS
    // ════════════════════════════════════════════════════════════════════

    /// Push the current active_sharers set to the sidebar so it can show
    /// "Live" badges and "Watch Stream" in the popup.
    fn sync_sharers_to_sidebar(&mut self) {
        if let Some(sidebar) = &mut self.sidebar {
            sidebar.set_active_sharers(&self.active_sharers);
        }
        // Trigger a user list refresh so badges update immediately
        self.update_user_list();
    }

    /// Show a system message in chat (not sent to server).
    fn show_chat_system(&mut self, text: &str) {
        if let Some(chat) = &mut self.chat {
            chat.handle_system_message(text.to_string());
        }
    }

    /// Look up username by u64 client id (for screen thread events which use u64).
    fn username_for_u64(&self, raw_id: u64) -> String {
        self.users
            .iter()
            .find(|&(&id, _)| id == raw_id)
            .map(|(_, u)| u.username.clone())
            .unwrap_or_else(|| format!("User#{raw_id}"))
    }

    // ════════════════════════════════════════════════════════════════════
    //  CONNECTION HANDLERS
    // ════════════════════════════════════════════════════════════════════

    fn handle_connect(&mut self, nickname: String, server: String) {
        self.settings.user.nickname = nickname.clone();
        self.settings.server.address = server.clone();
        self.settings.save();

        let addr = match server.parse::<SocketAddr>() {
            Ok(a) => a,
            Err(_) => {
                self.show_error("Invalid address (use ip:port)");
                return;
            }
        };

        self.network
            .tx
            .send(NetworkControl::Connect { addr, nickname })
            .ok();
    }

    fn handle_connection_established(&mut self) {
        self.connected = true;
        if let Some(frame) = &mut self.error_frame {
            frame.set_label("");
        }
    }
    fn setup_tray_icon(&mut self) {
        let icon_data: &'static [u8] = include_bytes!("../../../assets/icons/icon.ico");
        let sender = self.sender;

        let tray = TrayIconBuilder::new()
            .sender(move |e: &UiMessage| {
                sender.send(e.clone());
                fltk::app::awake(); // nudge FLTK loop
            })
            .icon_from_buffer(icon_data)
            .tooltip("baucord")
            .on_click(UiMessage::RestoreFromTray)
            .menu(
                MenuBuilder::new()
                    .item("Show", UiMessage::RestoreFromTray)
                    .separator()
                    .item("Quit", UiMessage::QuitFromTray),
            )
            .build()
            .unwrap();

        self.tray_icon = Some(tray);
    }

    fn handle_init_client(
        &mut self,
        client_id: u64,
        users: HashMap<renet::ClientId, String>,
        server_name: String,
        greeting: String,
    ) {
        if let Some(mut w) = self.connection_window.take() {
            w.hide();
        }

        self.show_main_screen();
        self.setup_tray_icon();

        let mut sentinel = window::Window::new(-10000, -10000, 1, 1, None);
        sentinel.end();
        sentinel.set_border(false);
        sentinel.show();
        self.sentinel_window = Some(sentinel);
        
        self.our_client_id = Some(client_id);
        for (&id, name) in &users {
            self.users.insert(
                id,
                UserState {
                    username: name.clone(),
                    is_speaking: false,
                    last_activity: Instant::now(),
                    audio_settings: self
                        .settings
                        .peer_settings
                        .get(&peer_key(name))
                        .cloned()
                        .unwrap_or_default(),
                    is_self_muted: false,
                    is_self_deafened: false,
                },
            );
        }

        // Start voice engine
        if let Some(id) = self.our_client_id {
            self.voice
                .tx
                .send(VoiceControl::Start {
                    settings: self.settings.clone(),
                    our_client_id: id,
                })
                .ok();
        }

        // Sync per-user audio settings to voice engine
        for (&id, user) in &self.users {
            self.voice.tx.send(VoiceControl::AddPeer(id)).ok();
            self.voice
                .tx
                .send(VoiceControl::SetPeerAudio(id, user.audio_settings.clone()))
                .ok();
        }

        if let Some(chat) = &mut self.chat {
            chat.handle_connected(server_name, greeting);
        }
        self.update_user_list();
    }

    fn handle_user_joined(&mut self, client_id: renet::ClientId, username: String) {
        let audio = self
            .settings
            .peer_settings
            .get(&peer_key(&username))
            .cloned()
            .unwrap_or_default();

        self.voice.tx.send(VoiceControl::AddPeer(client_id)).ok();
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(client_id, audio.clone()))
            .ok();

        self.users.insert(
            client_id,
            UserState {
                username: username.clone(),
                is_speaking: false,
                last_activity: Instant::now(),
                audio_settings: audio,
                is_self_muted: false,
                is_self_deafened: false,
            },
        );

        if let Some(chat) = &mut self.chat {
            chat.handle_user_joined(username);
        }
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserJoined))
            .ok();
        self.update_user_list();
    }

    fn handle_user_left(&mut self, client_id: renet::ClientId, username: String) {
        self.voice.tx.send(VoiceControl::RemovePeer(client_id)).ok();
        self.users.remove(&client_id);
        self.active_sharers.remove(&client_id);
        self.sync_sharers_to_sidebar();

        // Close viewer if we were watching this user
        if let Some(mut viewer) = self.viewer_windows.remove(&client_id) {
            viewer.close();
        }

        if let Some(chat) = &mut self.chat {
            chat.handle_user_left(username);
        }
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserLeft))
            .ok();
        self.update_user_list();
    }

    fn handle_toggle_mute(&mut self) {
        self.is_muted = !self.is_muted;
        self.voice
            .tx
            .send(VoiceControl::SetMuted(self.is_muted))
            .ok();
        self.network
            .tx
            .send(NetworkControl::SendAudioState {
                muted: self.is_muted,
                deafened: self.is_deafened,
            })
            .ok();

        if let Some(sidebar) = &mut self.sidebar {
            sidebar.set_mute_state(self.is_muted);
        }
    }

    fn handle_toggle_deafen(&mut self) {
        self.is_deafened = !self.is_deafened;
        self.voice
            .tx
            .send(VoiceControl::SetDeafened(self.is_deafened))
            .ok();
        self.network
            .tx
            .send(NetworkControl::SendAudioState {
                muted: self.is_muted,
                deafened: self.is_deafened,
            })
            .ok();

        if let Some(sidebar) = &mut self.sidebar {
            sidebar.set_deafen_state(self.is_deafened);
        }
    }
    fn handle_toggle_screenshare(&mut self) {
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
        } else if let Some(ref main_win) = self.main_window {
            screenshare::show(self.sender, main_win);
        }
    }

    fn handle_test_microphone(&mut self, enabled: bool) {
        self.is_testing_mic = enabled;
        self.is_testing_mic_shared.store(enabled, Ordering::Relaxed);

        if enabled {
            if self.vc_is_none() {
                self.voice
                    .tx
                    .send(VoiceControl::Start {
                        settings: self.settings.clone(),
                        our_client_id: 0,
                    })
                    .ok();
            }
            self.voice.tx.send(VoiceControl::SetLoopback(true)).ok();
        } else {
            self.voice.tx.send(VoiceControl::SetLoopback(false)).ok();
            if !self.connected {
                self.voice.tx.send(VoiceControl::Stop).ok();
            }
        }
    }

    fn vc_is_none(&self) -> bool {
        !self.connected && !self.is_testing_mic
    }

    fn handle_mute_user(&mut self, client_id: renet::ClientId, username: String, muted: bool) {
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio_settings.is_muted = muted;
            self.settings
                .peer_settings
                .insert(peer_key(&username), user.audio_settings.clone());
        }

        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.users
                    .get(&client_id)
                    .map(|u| u.audio_settings.clone())
                    .unwrap_or_default(),
            ))
            .ok();
        self.settings.save();
        self.update_user_list();
    }

    fn handle_set_user_volume(
        &mut self,
        client_id: renet::ClientId,
        username: String,
        volume: f32,
    ) {
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio_settings.volume = volume;
            self.settings
                .peer_settings
                .insert(peer_key(&username), user.audio_settings.clone());
        }

        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.users
                    .get(&client_id)
                    .map(|u| u.audio_settings.clone())
                    .unwrap_or_default(),
            ))
            .ok();
        self.settings.save();
    }

    fn handle_reset_peer_audio(&mut self, client_id: renet::ClientId, username: String) {
        self.settings.peer_settings.remove(&peer_key(&username));

        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio_settings = PeerAudioSettings::default();
        }

        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                PeerAudioSettings::default(),
            ))
            .ok();
        self.settings.save();
        self.update_user_list();
    }

    /// Shared teardown for both user-initiated and server-initiated disconnects.
    fn teardown_connection(&mut self) {
        log::warn!(
            "DIAG teardown_connection called, is_sharing={}, connected={}",
            self.screen.shared.is_sharing.load(Ordering::Relaxed),
            self.connected
        );
           
    log::warn!("DIAG: backtrace:\n{}", std::backtrace::Backtrace::force_capture());
        self.connected = false;
        self.is_testing_mic = false;
        self.voice.tx.send(VoiceControl::Stop).ok();

        // Stop our screen share if active
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
        }
        self.active_sharers.clear();
        // Close all viewer windows
        for (_, mut viewer) in self.viewer_windows.drain() {
            viewer.close();
        }

        if let Some(sidebar) = &mut self.sidebar {
            sidebar.handle_disconnected();
        }

        self.users.clear();
        self.our_client_id = None;
        self.is_muted = false;
        self.is_deafened = false;

        if let Some(mut w) = self.main_window.take() {
            w.hide();
        }
    }

    fn handle_disconnect(&mut self) {
        log::warn!("DIAG handle_disconnect (user-initiated)");
        self.network.tx.send(NetworkControl::Disconnect).ok();

        if let Some(chat) = &mut self.chat {
            chat.handle_disconnected("User disconnected".to_string());
        }

        self.teardown_connection();
        self.show_connection_screen();
    }

    fn handle_disconnect_event(&mut self, reason: &str) {
            log::warn!("DIAG handle_disconnect_event reason=\"{reason}\"");

        if let Some(chat) = &mut self.chat {
            chat.handle_disconnected(reason.to_string());
        }

        self.teardown_connection();
        self.show_connection_screen();
        self.show_error(reason);
    }

    // ════════════════════════════════════════════════════════════════════
    //  UI
    // ════════════════════════════════════════════════════════════════════

    fn update_user_list(&mut self) {
        let timeout = Duration::from_millis(500);
        let now = Instant::now();
        for user in self.users.values_mut() {
            if user.is_speaking && now.duration_since(user.last_activity) > timeout {
                user.is_speaking = false;
            }
        }

        if let Some(sidebar) = &mut self.sidebar {
            sidebar.update_users(&self.users, self.our_client_id);
        }
    }

    /// Dismiss any update-flow windows (splash or update) and show connect.
    fn dismiss_update_flow_and_connect(&mut self) {
        if let Some(mut w) = self.splash_window.take() {
            w.hide();
        }
        if let Some(mut w) = self.update_window.take() {
            w.hide();
        }
        if self.connection_window.is_none() {
            self.show_connection_screen();
        }
    }

    fn show_connection_screen(&mut self) {
        let (wind, error_frame) = crate::ui::connect::show(self.sender, &self.settings);
        self.error_frame = Some(error_frame);
        self.connection_window = Some(wind);
    }

    fn show_main_screen(&mut self) {
        let mut wind = window::Window::default()
            .with_size(self.settings.window.width, self.settings.window.height)
            .with_label("baucord");
        if let (Some(x), Some(y)) = (self.settings.window.x, self.settings.window.y) {
            wind.set_pos(x, y);
        } else {
            let (sw, sh) = app::screen_size();
            let cx = (sw as i32 - self.settings.window.width) / 2;
            let cy = (sh as i32 - self.settings.window.height) / 2;
            wind.set_pos(cx, cy);
        }
        wind.make_resizable(true);
        wind.set_color(theme::BG_PRIMARY);
        crate::ui::set_window_icon(&mut wind);

        if let (Some(x), Some(y)) = (self.settings.window.x, self.settings.window.y) {
            wind.set_pos(x, y);
        }

        let mut main_row = group::Flex::default_fill().row();
        main_row.set_color(theme::BG_PRIMARY);

        let mut sidebar = Sidebar::new(self.sender);
        let sidebar_group = sidebar.create_ui();
        main_row.fixed(&sidebar_group, 250);
        sidebar.set_mute_state(self.is_muted);

        let mut chat = Chat::new(self.sender);
        let chat_group = chat.create_ui();
        main_row.add(&chat_group);

        main_row.end();
        wind.end();

        /*
        let sender = self.sender;
        wind.set_callback(move |w| {
            if app::event() == enums::Event::Close {
                sender.send(UiMessage::Shutdown);
                w.hide();
            }
        });*/
        wind.set_callback(move |w| {
            if app::event() == enums::Event::Close {
                w.hide(); // Just hide — don't shutdown
                // The FLTK app keeps running because other internal
                // mechanisms (timers, awake) keep the loop alive.
            }
        });

        wind.show();

        self.main_window = Some(wind);
        self.sidebar = Some(sidebar);
        self.chat = Some(chat);
    }

    fn show_settings_window(&mut self) {
        use crate::ui::settings::SettingsWindow;
        use audio::VoiceChat;

        let input_devices = VoiceChat::list_input_devices()
            .inspect_err(|e| warn!("Failed to list input devices: {e}"))
            .unwrap_or_default();
        let output_devices = VoiceChat::list_output_devices()
            .inspect_err(|e| warn!("Failed to list output devices: {e}"))
            .unwrap_or_default();

        // Center settings window on the main window
        let (px, py, pw, ph) = if let Some(ref w) = self.main_window {
            (w.x(), w.y(), w.w(), w.h())
        } else {
            let (sw, sh) = app::screen_size();
            (0, 0, sw as i32, sh as i32)
        };

        SettingsWindow::show(
            self.sender,
            self.settings.clone(),
            input_devices,
            output_devices,
            self.voice.shared.output_level.clone(),
            self.voice.shared.denoised_level.clone(),
            self.voice.shared.speaking.clone(),
            px,
            py,
            pw,
            ph,
        );
    }

    fn show_error(&mut self, error: &str) {
        if let Some(frame) = &mut self.error_frame {
            frame.set_label(error);
            frame.set_label_color(theme::DANGER);
        }

        let sender = self.sender;
        app::add_timeout3(5.0, move |_| {
            sender.send(UiMessage::ClearError);
        });
    }

    fn get_username(&self, client_id: renet::ClientId) -> String {
        self.users
            .get(&client_id)
            .map(|u| u.username.clone())
            .unwrap_or_else(|| format!("User{client_id}"))
    }

    /// Clean shutdown: signal threads, join with timeout, save settings.
    ///
    /// Order matters:
    /// 1. Save settings (while windows still exist for geometry)
    /// 2. Stop voice engine (drops audio devices, closes internal threads)
    /// 3. Disconnect network (sends goodbye to server)
    /// 4. Signal both worker threads to exit
    /// 5. Join with hard timeout — never hang forever
    fn shutdown(&mut self) {
        info!("Shutting down");

        // 1. Save window geometry while the window still exists
        if let Some(ref w) = self.main_window {
            self.settings.window.width = w.w();
            self.settings.window.height = w.h();
            self.settings.window.x = Some(w.x());
            self.settings.window.y = Some(w.y());
        }
        self.settings.save();

        // 2. Stop the voice engine (releases audio devices + internal threads)
        self.voice.tx.send(VoiceControl::Stop).ok();

        // 3. Disconnect from server cleanly
        self.network.tx.send(NetworkControl::Disconnect).ok();

        // Small delay to let the disconnect packet go out
        std::thread::sleep(Duration::from_millis(50));

        // 4. Signal all threads to exit their loops
        self.voice.tx.send(VoiceControl::Shutdown).ok();
        self.network.tx.send(NetworkControl::Shutdown).ok();
        self.screen.tx.send(ScreenControl::Shutdown).ok();

        // 5. Join with hard timeout — take handles so Drop won't try again.
        //    If a thread is stuck (cpal deadlock, etc.), we bail after 2s
        //    and let the OS clean up on process exit.
        let voice_handle = self.voice.join_handle.take();
        let network_handle = self.network.join_handle.take();
        let screen_handle = self.screen.join_handle.take();

        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            if let Some(h) = voice_handle {
                let _ = h.join();
            }
            if let Some(h) = network_handle {
                let _ = h.join();
            }
            if let Some(h) = screen_handle {
                let _ = h.join();
            }
            let _ = done_tx.send(());
        });
        self.tray_icon.take();

        match done_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => info!("Shutdown complete"),
            Err(_) => warn!("Shutdown timed out — threads still running, exiting anyway"),
        }
    }
}
