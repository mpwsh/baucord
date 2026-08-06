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

use client::{
    config::{BindableAction, Settings, peer_key},
    network::{NetworkControl, NetworkMessage, NetworkThreadHandle},
    screen::{ScreenControl, ScreenEvent, ScreenThreadHandle},
    voice::{VoiceControl, VoiceEvent, VoiceThreadHandle},
};

use crate::{
    hotkeys::HotkeyManager,
    ui::{chat::Chat, messages::UiMessage, screenshare, sidebar::Sidebar, viewer::ViewerWindow},
};
use audio::{PeerAudioSettings, SoundType};
use i18n::t;
use ui::theme;

// ── Public types ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct UserState {
    pub username: String,
    pub is_speaking: bool,
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
    /// True while the 1-second stream-stats timer is scheduled. Prevents
    /// starting a second timer on every share start.
    stream_stats_timer_active: Arc<AtomicBool>,

    // UI
    splash_window: Option<window::Window>,
    update_window: Option<window::Window>,
    /// The single application window. Hosts a Wizard with two views:
    /// the connect form and the main (sidebar + chat) screen.
    main_window: Option<window::Window>,
    wizard: Option<group::Wizard>,
    connect_view: Option<group::Group>,
    /// Container for the main screen; content is rebuilt per connection.
    main_view: Option<group::Group>,
    sidebar: Option<Sidebar>,
    chat: Option<Chat>,
    error_frame: Option<frame::Frame>,
    /// "Saved device isn't connected" notice on the connect view.
    device_warning: Option<frame::Frame>,
    settings_window_open: bool,
    hotkeys: Option<HotkeyManager>,
    tray_icon: Option<TrayIcon<UiMessage>>,
    sentinel_window: Option<window::Window>,

    // Device hot-reload polling
    last_input_devices: Vec<(String, bool)>,
    last_output_devices: Vec<(String, bool)>,
}

impl App {
    pub fn new() -> Self {
        let app = app::App::default();
        theme::apply();

        let (sender, receiver) = app::channel::<UiMessage>();

        let is_testing_mic_shared = Arc::new(AtomicBool::new(false));

        // Wake callback — worker threads call this to nudge the FLTK event loop.
        // This is the ONLY place fltk::app::awake is referenced for worker threads.
        let wake: client::voice::WakeFn = Arc::new(fltk::app::awake);

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

        // Localize before any UI is built. None = follow the OS locale.
        i18n::init(settings.language.as_deref());

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
            stream_stats_timer_active: Arc::new(AtomicBool::new(false)),
            splash_window: None,
            update_window: None,
            main_window: None,
            wizard: None,
            connect_view: None,
            main_view: None,
            sidebar: None,
            chat: None,
            error_frame: None,
            device_warning: None,
            settings_window_open: false,
            hotkeys: None,
            tray_icon: None,
            sentinel_window: None,
            last_input_devices: Vec::new(),
            last_output_devices: Vec::new(),
        }
    }

    pub fn run(&mut self) {
        // Register global hotkeys on the main thread (required by the OS).
        self.hotkeys = Some(HotkeyManager::new(&self.settings));

        let handles = crate::ui::splash::show(self.sender);
        self.splash_window = Some(handles.window);

        // Seed the initial device snapshot so the first poll has something to diff against.
        self.last_input_devices = audio::VoiceChat::list_input_devices().unwrap_or_default();
        self.last_output_devices = audio::VoiceChat::list_output_devices().unwrap_or_default();

        // Start a repeating timer that polls for device changes every 2.5 s.
        // The callback just enumerates devices and, if changed, sends a
        // UiMessage so all the diffing logic stays in handle_ui_message.
        let device_sender = self.sender;
        app::add_timeout3(2.5, move |handle| {
            let input = audio::VoiceChat::list_input_devices().unwrap_or_default();
            let output = audio::VoiceChat::list_output_devices().unwrap_or_default();
            device_sender.send(UiMessage::DevicesChanged {
                input_devices: input,
                output_devices: output,
            });
            app::repeat_timeout3(2.5, handle);
        });

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
                // No sound here: this event fires whenever the engine
                // boots, including for standalone mic tests. The join
                // ding is sent from handle_init_client instead.
                info!("Voice engine ready");
            }
            VoiceEvent::Error(e) => {
                self.show_error(&e);
            }
            VoiceEvent::SpeakingChanged(speaking) => {
                if let Some(id) = self.our_client_id
                    && let Some(user) = self.users.get_mut(&id)
                {
                    user.is_speaking = speaking;
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
                    chat.handle_system_message(
                        t!("chat-share-started", "quality" => quality.to_string()),
                    );
                }
                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.set_share_state(true);
                }
                // Start the 1-second stream stats timer once. It stops
                // itself when sharing ends; the guard prevents stacking a
                // new repeating timer on every share start.
                if !self.stream_stats_timer_active.swap(true, Ordering::Relaxed) {
                    let sender = self.sender;
                    let timer_active = self.stream_stats_timer_active.clone();
                    let is_sharing = self.screen.shared.is_sharing.clone();
                    app::add_timeout3(1.0, move |handle| {
                        if is_sharing.load(Ordering::Relaxed) {
                            sender.send(UiMessage::UpdateStreamStats);
                            app::repeat_timeout3(1.0, handle);
                        } else {
                            timer_active.store(false, Ordering::Relaxed);
                        }
                    });
                }
            }
            ScreenEvent::SharingStopped => {
                info!("📺 Screen share stopped");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message(t!("chat-share-stopped"));
                }
                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.set_share_state(false);
                    // Clear the stats line — it used to keep showing the
                    // last sample after the share ended.
                    sidebar.update_stream_stats(None);
                }
            }
            ScreenEvent::Error(e) => {
                self.show_error(&t!("error-screen-share", "error" => e.as_str()));
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
                let username = self.get_username(client_id);
                info!("📺 {username} started sharing ({width}x{height})");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message(
                        t!("chat-peer-sharing", "name" => username.as_str()),
                    );
                }
            }
            ScreenEvent::PeerStoppedSharing { client_id } => {
                let username = self.get_username(client_id);
                info!("📺 {username} stopped sharing");
                if let Some(chat) = &mut self.chat {
                    chat.handle_system_message(
                        t!("chat-peer-stopped-sharing", "name" => username.as_str()),
                    );
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
            NetworkMessage::NetworkStats { rtt_ms, .. } => {
                if let Some(sidebar) = &mut self.sidebar {
                    let audio_ms = self.voice.shared.jitter_buffer_ms.load(Ordering::Relaxed);
                    sidebar.update_latency(rtt_ms, Some(audio_ms));
                }
            }
            NetworkMessage::ShareDenied { reason } => {
                // The screen thread starts capturing optimistically before
                // the server answers — stop it so local state matches the
                // server's (otherwise we encode into the void while the
                // relay drops every chunk as not-sharing).
                if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                    self.screen.tx.send(ScreenControl::StopSharing).ok();
                }
                self.show_error(&format!("Screen share denied: {}", reason.describe()));
            }
            NetworkMessage::SubscribeDenied { target_id, reason } => {
                // Close the viewer window WatchPeer opened optimistically.
                if let Some(mut viewer) = self.viewer_windows.remove(&target_id) {
                    viewer.close();
                }
                let username = self.get_username(target_id);
                self.show_chat_system(&format!("Can't watch {username}: {}", reason.describe()));
            }
            NetworkMessage::StreamHealth { .. } => {
                // Routed straight to the screen thread by the network loop
                // (drives fps adaptation). Never arrives here.
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
            NetworkMessage::KeyframeRequested { .. } => {
                // Keyframe requests are routed directly to the screen thread
                // by the network loop (→ ScreenControl::ForceKeyframe). Like
                // VideoFrame, they should never arrive here.
            }
        }
    }
    #[allow(clippy::too_many_lines)]
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
                        .unwrap_or_else(|| t!("unknown"));
                    let fps_scale = self.screen.shared.fps_scale_pct.load(Ordering::Relaxed);
                    let viewers = self.screen.shared.stream_viewers.load(Ordering::Relaxed);
                    let loss_pct =
                        f32::from_bits(self.screen.shared.stream_loss_pct.load(Ordering::Relaxed));
                    let quality_str = if fps_scale < 100 {
                        format!("{quality_str} · {fps_scale}% fps · loss {loss_pct:.1}%")
                    } else if viewers > 0 {
                        format!("{quality_str} · {viewers} watching")
                    } else {
                        quality_str
                    };
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
                self.show_chat_system(&t!("chat-watching", "name" => username.as_str()));
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
                let username = self.get_username(client_id);
                self.show_chat_system(&t!("chat-stopped-watching", "name" => username.as_str()));
            }
            UiMessage::SetServerAddress(addr) => {
                self.settings.server.address = addr.trim().to_string();
            }
            UiMessage::SetServerTokenUrl(url) => {
                self.settings.server.token_url = url;
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

            UiMessage::Connect { nickname, password } => {
                self.handle_connect(nickname, password);
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
            UiMessage::SetLanguage(lang) => {
                self.settings.language = lang;
                i18n::set_locale(self.settings.language.as_deref());
                // Labels are copied into widgets at construction, so a
                // language change re-renders per tier: the connect view
                // rebuilds, sidebar buttons relabel in place, settings
                // windows update on reopen, chat history stays as-is.
                self.rebuild_connect_view();
                if let Some(sidebar) = &mut self.sidebar {
                    sidebar.relabel();
                }
            }
            UiMessage::ResetSettings => {
                self.settings.reset();
                // Push the defaults to the running subsystems so behavior
                // matches the file immediately — previously only the file
                // changed while the voice engine kept the old values.
                self.voice
                    .tx
                    .send(VoiceControl::SetGateThreshold(
                        self.settings.audio.gate_threshold,
                    ))
                    .ok();
                self.voice
                    .tx
                    .send(VoiceControl::SetInputGain(self.settings.audio.input.gain))
                    .ok();
                self.voice
                    .tx
                    .send(VoiceControl::SetOutputGain(self.settings.audio.output.gain))
                    .ok();
                self.voice.tx.send(VoiceControl::SetInputDevice(None)).ok();
                self.voice.tx.send(VoiceControl::SetOutputDevice(None)).ok();
                if let Some(ref mut hotkeys) = self.hotkeys {
                    hotkeys.update(&self.settings);
                }
                i18n::set_locale(self.settings.language.as_deref());
                self.rebuild_connect_view();
                self.refresh_device_warning();
            }
            UiMessage::SetInputDevice(d) => {
                self.settings.audio.input.device = d.clone();
                self.voice.tx.send(VoiceControl::SetInputDevice(d)).ok();
                self.refresh_device_warning();
            }
            UiMessage::SetOutputDevice(d) => {
                self.settings.audio.output.device = d.clone();
                self.voice.tx.send(VoiceControl::SetOutputDevice(d)).ok();
                self.refresh_device_warning();
            }

            UiMessage::DevicesChanged {
                input_devices,
                output_devices,
            } => {
                self.handle_devices_changed(input_devices, output_devices);
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
            UiMessage::InstallUpdate(path) => {
                if let Err(e) = crate::ui::update::extract_and_install(&path) {
                    self.sender.send(UiMessage::ShowError(e));
                }
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
            UiMessage::MainWindowClose => {
                // Hide to tray only once the tray infrastructure exists
                // (tray icon + sentinel are created on connect). From the
                // connect view, closing the window quits the app.
                if self.connected && self.sentinel_window.is_some() {
                    if let Some(w) = &mut self.main_window {
                        w.hide();
                    }
                } else {
                    self.app.quit();
                }
            }
            UiMessage::Shutdown => {
                // Explicit quit (connect-view Quit button). Cleanup runs
                // after the event loop exits in run().
                self.app.quit();
            }
            UiMessage::StopVoiceAfterDisconnect => {
                // Deferred from teardown_connection so the leave sound
                // can finish. Skip if we're already in a new session or
                // a mic test — their engine must keep running.
                if !self.connected && !self.is_testing_mic {
                    self.voice.tx.send(VoiceControl::Stop).ok();
                }
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

    // ── Device hot-reload ───────────────────────────────────────────

    /// Called by the 2.5 s polling timer whenever the device list or
    /// default identity has changed.  Produces chat messages and, when
    /// the active device has disappeared (or the system default changed
    /// underneath us), re-triggers the stream rebuild on the voice thread.
    fn handle_devices_changed(
        &mut self,
        new_input: Vec<(String, bool)>,
        new_output: Vec<(String, bool)>,
    ) {
        // Collect owned names so we don't borrow self.last_*_devices as &str
        // (which would conflict with &mut self for show_chat_system later).
        let old_input_names: std::collections::HashSet<String> = self
            .last_input_devices
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let new_input_names: std::collections::HashSet<String> =
            new_input.iter().map(|(n, _)| n.clone()).collect();

        let old_output_names: std::collections::HashSet<String> = self
            .last_output_devices
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let new_output_names: std::collections::HashSet<String> =
            new_output.iter().map(|(n, _)| n.clone()).collect();

        let old_input_default = Self::default_device(&self.last_input_devices);
        let old_output_default = Self::default_device(&self.last_output_devices);
        let new_input_default = Self::default_device(&new_input);
        let new_output_default = Self::default_device(&new_output);

        // Nothing changed — early out.
        if old_input_names == new_input_names
            && old_output_names == new_output_names
            && old_input_default == new_input_default
            && old_output_default == new_output_default
        {
            return;
        }

        // ── Chat notifications ──────────────────────────────────────

        for name in new_input_names.difference(&old_input_names) {
            self.show_chat_system(&t!("chat-mic-connected", "name" => name.as_str()));
        }
        for name in old_input_names.difference(&new_input_names) {
            self.show_chat_system(&t!("chat-mic-disconnected", "name" => name.as_str()));
        }
        for name in new_output_names.difference(&old_output_names) {
            self.show_chat_system(&t!("chat-speaker-connected", "name" => name.as_str()));
        }
        for name in old_output_names.difference(&new_output_names) {
            self.show_chat_system(&t!("chat-speaker-disconnected", "name" => name.as_str()));
        }

        // ── Input device: detect if a rebuild is needed ─────────────

        let need_input_rebuild = if let Some(ref selected) = self.settings.audio.input.device {
            !new_input_names.contains(selected)
        } else {
            old_input_default != new_input_default
        };

        if need_input_rebuild {
            if self.settings.audio.input.device.is_some() {
                self.show_chat_system(&t!("chat-input-lost"));
            } else {
                let fallback = t!("unknown-device");
                let new_default = new_input_default.as_deref().unwrap_or(&fallback);
                self.show_chat_system(&t!("chat-input-default-changed", "name" => new_default));
            }
            self.settings.audio.input.device = None;
            self.voice.tx.send(VoiceControl::SetInputDevice(None)).ok();
        }

        // ── Output device: same logic ───────────────────────────────

        let need_output_rebuild = if let Some(ref selected) = self.settings.audio.output.device {
            !new_output_names.contains(selected)
        } else {
            old_output_default != new_output_default
        };

        if need_output_rebuild {
            if self.settings.audio.output.device.is_some() {
                self.show_chat_system(&t!("chat-output-lost"));
            } else {
                let fallback = t!("unknown-device");
                let new_default = new_output_default.as_deref().unwrap_or(&fallback);
                self.show_chat_system(&t!("chat-output-default-changed", "name" => new_default));
            }
            self.settings.audio.output.device = None;
            self.voice.tx.send(VoiceControl::SetOutputDevice(None)).ok();

            // WASAPI loopback captures from the default output device,
            // so when that device changes the capture stream dies too.
            // Restart it if a screen share is currently active.
            if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                self.screen
                    .tx
                    .send(client::screen::ScreenControl::RestartSystemAudio)
                    .ok();
            }
        }

        // ── Update the snapshot ─────────────────────────────────────

        self.last_input_devices = new_input;
        self.last_output_devices = new_output;

        self.refresh_device_warning();
    }

    /// Extract the name of whichever device is marked as the OS default.
    fn default_device(devices: &[(String, bool)]) -> Option<String> {
        devices
            .iter()
            .find(|(_, is_default)| *is_default)
            .map(|(name, _)| name.clone())
    }

    // ════════════════════════════════════════════════════════════════════
    //  CONNECTION HANDLERS
    // ════════════════════════════════════════════════════════════════════

    fn handle_connect(&mut self, nickname: String, password: String) {
        self.settings.user.nickname = nickname.clone();
        let password = password.trim();
        self.settings.server.token_password = (!password.is_empty()).then(|| password.to_string());
        self.settings.save();

        let addr = match self.settings.server.address.trim().parse::<SocketAddr>() {
            Ok(a) => a,
            Err(_) => {
                self.show_error(&t!("error-invalid-address"));
                return;
            }
        };

        self.network
            .tx
            .send(NetworkControl::Connect {
                addr,
                nickname,
                token_url: self.settings.server.token_url.clone(),
                token_password: self.settings.server.token_password.clone(),
            })
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
                    .item(&t!("tray-show"), UiMessage::RestoreFromTray)
                    .separator()
                    .item(&t!("tray-quit"), UiMessage::QuitFromTray),
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
        self.show_main_screen();
        self.setup_tray_icon();

        // Invisible off-screen sentinel window. FLTK's event loop exits
        // when no windows are shown; this keeps it alive while the main
        // window is hidden to the tray. Do not remove without replacing
        // that mechanism (e.g. fltk::app::program_should_quit).
        // Created once — reconnects used to stack a new one per session.
        if self.sentinel_window.is_none() {
            let mut sentinel = window::Window::new(-10000, -10000, 1, 1, None);
            sentinel.end();
            sentinel.set_border(false);
            sentinel.show();
            self.sentinel_window = Some(sentinel);
        }

        self.our_client_id = Some(client_id);
        for (&id, name) in &users {
            self.users.insert(
                id,
                UserState {
                    username: name.clone(),
                    is_speaking: false,
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
            // Join ding, queued behind Start on the same channel so it
            // plays on the freshly started (or still-running) engine.
            // Tied to joining rather than VoiceEvent::Started so mic
            // tests stay silent and fast reconnects still ding even if
            // the deferred-stop kept the engine alive.
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::UserJoined))
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
        // Capture this BEFORE flipping is_testing_mic — the old code
        // checked afterwards, which made the "start the engine for a mic
        // test while disconnected" branch unreachable, so Start was
        // never sent and the test silently did nothing.
        let voice_already_running = self.connected || self.is_testing_mic;

        self.is_testing_mic = enabled;
        self.is_testing_mic_shared.store(enabled, Ordering::Relaxed);

        if enabled {
            if !voice_already_running {
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
        self.connected = false;
        self.is_testing_mic = false;
        // Also clear the shared flag the voice loop reads — leaving it
        // set (disconnect during a mic test) kept the loop gating
        // outgoing audio after the next connect.
        self.is_testing_mic_shared.store(false, Ordering::Relaxed);

        // Play the leave sound for ourselves too. The server's UserLeft
        // broadcast (which triggers it for everyone else) is never sent
        // to the leaver — and stopping the engine immediately would cut
        // the sound off, so the Stop is deferred. Its handler skips the
        // Stop if a new session or mic test started in the meantime.
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserLeft))
            .ok();
        let sender = self.sender;
        app::add_timeout3(0.9, move |_| {
            sender.send(UiMessage::StopVoiceAfterDisconnect);
        });

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

        // The window itself persists — callers switch the Wizard back
        // to the connect view (handle_disconnect / handle_disconnect_event
        // both call show_connection_screen right after this).
    }

    fn handle_disconnect(&mut self) {
        self.network.tx.send(NetworkControl::Disconnect).ok();

        if let Some(chat) = &mut self.chat {
            chat.handle_disconnected(t!("disconnect-reason-user"));
        }

        self.teardown_connection();
        self.show_connection_screen();
    }

    fn handle_disconnect_event(&mut self, reason: &str) {
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
        // Speaking state is purely edge-triggered now. The old 500 ms
        // "stale speaking" timeout here cleared the light mid-speech for
        // anyone talking longer than 500 ms (events only arrive on
        // transitions, so last_activity never refreshed) — the light then
        // stayed off until their next stop/start edge.
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
        self.show_connection_screen();
    }

    fn show_connection_screen(&mut self) {
        self.ensure_main_window();
        self.refresh_device_warning();
        if let (Some(wizard), Some(view)) = (&mut self.wizard, &self.connect_view) {
            wizard.set_current_widget(view);
        }
        if let Some(w) = &mut self.main_window {
            // May be hidden in the tray when a disconnect arrives.
            w.show();
            w.redraw();
        }
    }

    /// Tear down and rebuild the connect view. FLTK widgets copy
    /// their labels at construction, so a language change needs a
    /// rebuild; this is the cheap view, and it keeps showing if it was
    /// the active one.
    fn rebuild_connect_view(&mut self) {
        let Some(wizard) = &mut self.wizard else {
            return;
        };
        let old = self.connect_view.take();

        wizard.begin();
        let connect = crate::ui::connect::build_view(self.sender, &self.settings);
        wizard.end();
        // end() sets the current group to the wizard's PARENT (the main
        // window) — reset it, same trap as show_main_screen.
        group::Group::set_current(None::<&group::Group>);

        if let Some(mut old) = old {
            wizard.remove(&old);
            old.hide();
        }

        self.error_frame = Some(connect.error_frame);
        self.device_warning = Some(connect.device_warning);
        self.connect_view = Some(connect.group);
        self.refresh_device_warning();

        if !self.connected
            && let (Some(wizard), Some(view)) = (&mut self.wizard, &self.connect_view)
        {
            wizard.set_current_widget(view);
        }
        app::redraw();
    }

    /// Compare the devices saved in settings against what's connected
    /// right now and surface the result on the connect view. Runtime
    /// unplugs already fall back to the system default with a chat
    /// notice — this covers the silent case: the app launched (or sits
    /// on the connect view) with a saved device that isn't there.
    fn refresh_device_warning(&mut self) {
        let Some(frame) = &mut self.device_warning else {
            return;
        };

        let mut missing: Vec<String> = Vec::new();
        if let Some(name) = &self.settings.audio.input.device
            && !self.last_input_devices.iter().any(|(n, _)| n == name)
        {
            missing.push(i18n::t!("device-missing-mic", "name" => name.as_str()));
        }
        if let Some(name) = &self.settings.audio.output.device
            && !self.last_output_devices.iter().any(|(n, _)| n == name)
        {
            missing.push(i18n::t!("device-missing-output", "name" => name.as_str()));
        }

        if missing.is_empty() {
            frame.set_label("");
        } else {
            let joiner = format!(" {} ", i18n::t!("list-and"));
            frame.set_label(&format!(
                "{}\n{}",
                i18n::t!("device-warning", "devices" => missing.join(&joiner)),
                i18n::t!("device-warning-hint"),
            ));
        }
        frame.redraw();
    }

    /// Create the single application window (once): saved geometry, a
    /// Wizard hosting the connect view, and an (initially empty) main
    /// view container that show_main_screen() fills per connection.
    fn ensure_main_window(&mut self) {
        if self.main_window.is_some() {
            return;
        }

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
        wind.size_range(500, 400, 0, 0);
        wind.set_color(theme::BG_PRIMARY);
        crate::ui::set_window_icon(&mut wind);

        let wizard = group::Wizard::default_fill();

        // View 1: connect form. Built once — it keeps typed values
        // across failed attempts and reconnects, and error_frame stays
        // valid for the app's lifetime.
        let connect = crate::ui::connect::build_view(self.sender, &self.settings);

        // View 2: main screen container, filled by show_main_screen().
        let main_view = group::Group::default_fill();
        main_view.end();

        wizard.end();
        wind.end();

        // The OS close button means different things per view; route
        // the decision through App, which knows the connection state.
        let sender = self.sender;
        wind.set_callback(move |_| {
            if app::event() == enums::Event::Close {
                sender.send(UiMessage::MainWindowClose);
            }
        });

        wind.show();

        self.error_frame = Some(connect.error_frame);
        self.device_warning = Some(connect.device_warning);
        self.wizard = Some(wizard);
        self.connect_view = Some(connect.group);
        self.main_view = Some(main_view);
        self.main_window = Some(wind);
    }

    fn show_main_screen(&mut self) {
        self.ensure_main_window();

        // (Re)build the main view fresh for this connection — same
        // semantics as the old per-connection window: clean chat and
        // clean sidebar every time. Drop the old handles BEFORE clear()
        // so nothing holds references to deleted widgets.
        self.sidebar = None;
        self.chat = None;
        if let Some(view) = &mut self.main_view {
            view.clear();
            view.begin();

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
            view.end();
            // CRITICAL: Fl_Group::end() sets the current group to the
            // widget's PARENT — for `view` that's the Wizard, not null.
            // Without this reset, every window created afterwards
            // (settings, audio popups, viewers, the tray sentinel) is
            // silently adopted as an embedded child of the Wizard.
            group::Group::set_current(None::<&group::Group>);

            self.sidebar = Some(sidebar);
            self.chat = Some(chat);
        }

        if let (Some(wizard), Some(view)) = (&mut self.wizard, &self.main_view) {
            wizard.set_current_widget(view);
        }
        if let Some(w) = &mut self.main_window {
            w.show();
            w.redraw();
        }
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
            // ClientId is u64 — pass it pre-formatted so Fluent's f64-backed
            // numbers can't lose precision on large ids.
            .unwrap_or_else(|| t!("fallback-username", "id" => client_id.to_string()))
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
