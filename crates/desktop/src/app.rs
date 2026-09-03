// app.rs — Main application coordinator (Slint edition).
//
// Owns the Slint event loop and delegates to:
//   - NetworkThreadHandle (client::network) for the QUIC session + protocol
//   - VoiceThreadHandle   (client::voice)   for audio capture/playback
//   - ScreenThreadHandle  (client::screen)  for screen capture/encode/decode
//
// Architecture vs the FLTK build:
//   - Worker threads keep their crossbeam channels; the `WakeFn` they
//     already call now schedules a drain on the Slint event loop via
//     `invoke_from_event_loop` (the one-for-one replacement of
//     `fltk::app::awake` + the manual `while app.wait()` drain).
//   - All periodic work (device poll 2.5 s, resources 2 s, stream
//     stats 1 s, error clear 5 s) runs on `slint::Timer`s.
//   - UI state lives in `State`/`Logic` globals; row models are
//     `Rc<VecModel<…>>` mutated in place (push/set_row_data), so
//     roster refreshes preserve scroll position exactly like the old
//     incremental sidebar updates.
//   - Every floating helper (emoji palette, per-user popover, share
//     dialog, image preview) is an in-window overlay — the orphaned-
//     popup lifecycle code has no equivalent here because the failure
//     mode is structurally impossible.
//
// Speaking state is driven by voice events, not inferred from packet flow.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    net::SocketAddr,
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use app_core::{
    chat::ChatHistory,
    roster::{self, ChannelMeta, PeerAudio, UserState},
};
use audio::{PeerAudioSettings, SoundType};
use client::{
    config::{peer_key, BindableAction, Settings},
    network::{NetworkControl, NetworkMessage, NetworkThreadHandle},
    screen::{ScreenControl, ScreenEvent, ScreenThreadHandle},
    voice::{VoiceControl, VoiceEvent, VoiceThreadHandle},
};
use i18n::t;
use log::{info, warn};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use ui::{ChatRow, Logic, MainWindow, Phase, State, Tr, UserRow};

use crate::{
    hotkeys::HotkeyManager,
    media::{self, GifPlayer},
    viewers::Viewer,
};

/// Compiled-in version of this binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── The UI-thread singleton ─────────────────────────────────────────────

thread_local! {
    /// The app lives on the UI thread; worker wakes reach it through
    /// `invoke_from_event_loop` + this slot.
    static APP: RefCell<Option<Rc<RefCell<AppState>>>> = const { RefCell::new(None) };
}

/// Schedule a drain on the UI thread. Cheap and coalescing: multiple
/// wakes before the loop runs just queue multiple no-op-ish drains.
fn wake() {
    let _ = slint::invoke_from_event_loop(|| {
        let app = APP.with(|a| a.borrow().clone());
        if let Some(app) = app {
            AppState::drain(&app);
        }
    });
}

// struct_excessive_bools: connected/is_muted/is_deafened/is_testing_mic
// are four independent facts about the session, not an encoded state
// machine — a bitflags/enum here would only obscure the call sites.
#[allow(clippy::struct_excessive_bools)]
pub struct AppState {
    // Thread handles (own the threads + their channels)
    network: NetworkThreadHandle,
    voice: VoiceThreadHandle,
    screen: ScreenThreadHandle,
    /// Prepared frames + the forward channel from the present-prep
    /// thread (see present.rs), which owns `screen.event_rx` now.
    present: Arc<crate::present::PresentShared>,
    ui_screen_rx: crossbeam_channel::Receiver<ScreenEvent>,

    // Domain state
    users: HashMap<u64, UserState>,
    our_client_id: Option<u64>,
    /// Server display name (from Init) — prefixes server-originated
    /// system chat lines, same style as the greeting.
    server_name: String,
    /// Voice channels this server defines (from Init), in id order.
    channels: Vec<ChannelMeta>,
    /// Channel a JoinChannel is in flight for (the password prompt's
    /// target; also lets a WrongPassword reply reopen the prompt).
    pending_channel_join: Option<u16>,
    connected: bool,
    pub settings: Settings,
    /// Our Ed25519 identity key (next to settings.json), sent in Hello
    /// so the server can derive a stable user id. `None` if it could
    /// not be loaded; the session works without it.
    client_key: Option<Arc<transport::ClientKey>>,
    is_muted: bool,
    is_deafened: bool,
    pub is_testing_mic: bool,
    is_testing_mic_shared: Arc<AtomicBool>,

    /// Active screen sharers: `client_id` → (width, height).
    active_sharers: HashMap<u64, (u32, u32)>,
    /// Sharer display order for the roster projection.
    sharer_ids: HashSet<u64>,

    // Chat
    history: ChatHistory,
    chat_model: Rc<VecModel<ChatRow>>,
    /// Image bytes per chat row-id (save / viewer / GIF replay).
    pub images: HashMap<i32, media::ImageEntry>,
    gifs: HashMap<i32, GifPlayer>,
    /// Pending outgoing image awaiting Send/Cancel in the preview.
    pending_preview: Option<Vec<u8>>,

    // Roster
    users_model: Rc<VecModel<UserRow>>,

    // Windows
    pub main: MainWindow,
    /// One-shot for repaint_on_reveal: set before showing a hidden
    /// main window (tray Show, reconnect paths).
    main_repaint_arm: Rc<Cell<bool>>,
    pub settings_win: Option<ui::SettingsWindow>,
    viewers: HashMap<u64, Viewer>,
    /// Image-viewer overlay: the chat row shown, and GIF playback
    /// state while that row is an animated GIF.
    pub gallery_row: Option<i32>,
    pub gallery_gif: Option<crate::viewers::GalleryGif>,
    /// GIF playback for the *send preview* overlay (the FLTK preview
    /// animated GIFs too).
    pub preview_gif: Option<crate::viewers::GalleryGif>,

    // Update flow
    update_version: Option<String>,
    update_zip_path: Option<String>,
    download_cancelled: Arc<AtomicBool>,

    // Device hot-reload polling
    last_input_devices: Vec<(String, bool)>,
    last_output_devices: Vec<(String, bool)>,

    // Own-process resource sampling (sidebar stats line)
    sys: sysinfo::System,
    /// `None` if the OS refused to identify our own process.
    pid: Option<sysinfo::Pid>,

    hotkeys: Option<HotkeyManager>,
    pub keybind_recording: Option<usize>,

    // Timers — held so they keep firing.
    timers: Vec<slint::Timer>,
    error_timer: slint::Timer,
    /// Backoff before an automatic reconnect after a transient loss.
    reconnect_timer: slint::Timer,
    /// Consecutive automatic reconnects since the last successful
    /// admission; capped so a dead server does not spin forever.
    reconnect_attempts: u32,
    stream_stats_timer_active: Arc<AtomicBool>,
    pub meter_timer: slint::Timer,
    /// The single animation ticker driving every playing GIF (chat
    /// rows + the viewer overlay). Runs only while something plays.
    gif_ticker: slint::Timer,
}

/// Spawn the three worker threads. Network needs `voice_tx/screen_tx` to
/// route hot-path packets, so channels are created up front. The wake
/// callback is how workers nudge the Slint loop — the only reference
/// to the wake mechanism they ever get.
#[allow(clippy::type_complexity)] // one-shot startup plumbing
fn spawn_workers(
    is_testing_mic_shared: &Arc<AtomicBool>,
) -> (
    VoiceThreadHandle,
    ScreenThreadHandle,
    NetworkThreadHandle,
    Arc<crate::present::PresentShared>,
    crossbeam_channel::Receiver<ScreenEvent>,
) {
    let wake_fn: client::WakeFn = Arc::new(wake);
    let (network_tx, network_control_rx) = crossbeam_channel::bounded::<NetworkControl>(256);
    let (network_msg_tx, network_msg_rx) = crossbeam_channel::bounded::<NetworkMessage>(256);

    let voice = VoiceThreadHandle::spawn(
        network_tx.clone(),
        is_testing_mic_shared.clone(),
        wake_fn.clone(),
    );
    let screen = ScreenThreadHandle::spawn(network_tx.clone(), wake_fn.clone());
    // Present-prep thread: takes over screen.event_rx (the UI drain
    // must not read it), pays the per-frame pixel copy off the UI
    // thread, forwards non-frame events back via this channel.
    let present_shared = Arc::new(crate::present::PresentShared::default());
    let (screen_fwd_tx, screen_fwd_rx) = crossbeam_channel::unbounded();
    crate::present::spawn(
        screen.event_rx.clone(),
        screen_fwd_tx,
        Arc::clone(&present_shared),
        wake_fn.clone(),
    );
    let network = NetworkThreadHandle::spawn(
        network_tx,
        network_control_rx,
        network_msg_tx,
        network_msg_rx,
        voice.tx.clone(),
        screen.tx.clone(),
        wake_fn,
    );
    (voice, screen, network, present_shared, screen_fwd_rx)
}

/// Prefer the Skia renderer when compiled in: femtovg (the default)
/// can't rasterize many color-emoji glyph formats, which shows as tofu
/// boxes in the emoji palette. Skia renders them all.
fn select_renderer() {
    // An explicit SLINT_BACKEND still wins — the no-rebuild escape
    // hatch (`winit-software` forces the basic software renderer;
    // testing backends work too).
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }

    // Skia on its CPU raster backend ("skia-software"): presents via
    // softbuffer, so no OpenGL/Metal/D3D surface ever exists — nothing
    // for Windows Game Mode or GPU driver suites to classify as a game
    // — while Skia's text stack still rasterizes color-emoji glyphs
    // (palette AND inline chat text) and hints fonts properly, the two
    // things Slint's basic software renderer cannot do. The ~150 MB
    // RSS overhead measured with the old GPU Skia build came from the
    // GL context + Ganesh caches; the raster backend skips all of it.
    if slint::BackendSelector::new()
        .renderer_name("skia-software".into())
        .select()
        .is_ok()
    {
        return;
    }
    warn!("Skia raster renderer unavailable, falling back to the basic software renderer");
    if let Err(e) = slint::BackendSelector::new()
        .renderer_name("software".into())
        .select()
    {
        warn!("Software renderer unavailable, using the default: {e}");
    }
}

/// Build everything and run the event loop until quit.
pub fn run() {
    select_renderer();
    let is_testing_mic_shared = Arc::new(AtomicBool::new(false));
    let (voice, screen, network, present_shared, screen_fwd_rx) =
        spawn_workers(&is_testing_mic_shared);

    let settings = Settings::load();

    // Localize before any UI is built. None = follow the OS locale.
    i18n::init(settings.language.as_deref());

    let main = MainWindow::new().expect("failed to create the main window");

    // Saved geometry (falls back to the window's preferred size,
    // centered by the WM).
    // Saved px dimensions; i32→f32 precision is irrelevant at window
    // scale.
    #[allow(clippy::cast_precision_loss)]
    {
        main.window().set_size(slint::LogicalSize::new(
            settings.window.width.max(500) as f32,
            settings.window.height.max(400) as f32,
        ));
        if let (Some(x), Some(y)) = (settings.window.x, settings.window.y) {
            main.window()
                .set_position(slint::LogicalPosition::new(x as f32, y as f32));
        }
    }

    let chat_model = Rc::new(VecModel::<ChatRow>::default());
    let users_model = Rc::new(VecModel::<UserRow>::default());
    main.global::<State>()
        .set_chat(ModelRc::from(chat_model.clone()));
    main.global::<State>()
        .set_users(ModelRc::from(users_model.clone()));

    // The emoji palette — straight from the shared catalog.
    let emoji_rows: Vec<ui::EmojiRow> = app_core::EMOJI
        .iter()
        .map(|(name, glyph, section)| ui::EmojiRow {
            code: (*name).into(),
            glyph: (*glyph).into(),
            section: (*section).into(),
        })
        .collect();
    main.global::<State>()
        .set_emoji(ModelRc::new(VecModel::from(emoji_rows)));

    let state = Rc::new(RefCell::new(AppState {
        network,
        voice,
        screen,
        users: HashMap::new(),
        our_client_id: None,
        server_name: String::new(),
        channels: Vec::new(),
        pending_channel_join: None,
        connected: false,
        client_key: client::config::load_client_key(),
        settings,
        is_muted: false,
        is_deafened: false,
        is_testing_mic: false,
        is_testing_mic_shared,
        active_sharers: HashMap::new(),
        sharer_ids: HashSet::new(),
        history: ChatHistory::new(),
        chat_model,
        images: HashMap::new(),
        gifs: HashMap::new(),
        pending_preview: None,
        users_model,
        main,
        main_repaint_arm: Rc::new(Cell::new(false)),
        settings_win: None,
        viewers: HashMap::new(),
        gallery_row: None,
        gallery_gif: None,
        preview_gif: None,
        update_version: None,
        update_zip_path: None,
        download_cancelled: Arc::new(AtomicBool::new(false)),
        last_input_devices: Vec::new(),
        last_output_devices: Vec::new(),
        present: present_shared,
        ui_screen_rx: screen_fwd_rx,
        sys: sysinfo::System::new(),
        pid: sysinfo::get_current_pid().ok(),
        hotkeys: None,
        keybind_recording: None,
        timers: Vec::new(),
        error_timer: slint::Timer::default(),
        reconnect_timer: slint::Timer::default(),
        reconnect_attempts: 0,
        stream_stats_timer_active: Arc::new(AtomicBool::new(false)),
        meter_timer: slint::Timer::default(),
        gif_ticker: slint::Timer::default(),
    }));

    APP.with(|a| *a.borrow_mut() = Some(state.clone()));

    {
        let mut st = state.borrow_mut();

        // Register global hotkeys on the main thread (required by the OS).
        st.hotkeys = Some(HotkeyManager::new(&st.settings, Arc::new(wake)));

        st.push_tr();
        st.push_connect_fields();
        st.main.global::<State>().set_phase(Phase::Splash);

        // Seed the initial device snapshot so the first poll has
        // something to diff against.
        st.last_input_devices = audio::VoiceChat::list_input_devices().unwrap_or_default();
        st.last_output_devices = audio::VoiceChat::list_output_devices().unwrap_or_default();
        st.refresh_device_warning();
    }

    wire_logic(&state);
    start_timers(&state);
    crate::updater::spawn_version_check();

    state
        .borrow()
        .main
        .show()
        .expect("failed to show the main window");
    {
        let weak = state.borrow().main.as_weak();
        let armed = state.borrow().main_repaint_arm.clone();
        repaint_on_reveal(state.borrow().main.window(), armed, move || {
            if let Some(main) = weak.upgrade() {
                main.set_repaint_flip(!main.get_repaint_flip());
            }
        });
    }

    // Tray icon: created from inside the running event loop (macOS
    // wants the NSApplication loop live before status items appear).
    #[cfg(feature = "tray")]
    slint::Timer::single_shot(Duration::from_millis(1), || {
        with_current(tray::setup);
    });

    // The loop keeps running while every window is hidden (tray) —
    // the FLTK build needed an off-screen sentinel window for this.
    slint::run_event_loop_until_quit().expect("event loop failed");

    // Loop exited (quit_event_loop called). Clean shutdown.
    let st = state;
    APP.with(|a| a.borrow_mut().take());
    st.borrow_mut().shutdown();
}

// ── Callback wiring ─────────────────────────────────────────────────────

/// Convenience: run `f` with a `&mut AppState` if the app still exists.
fn with_app(weak: &Weak<RefCell<AppState>>, f: impl FnOnce(&mut AppState)) {
    if let Some(rc) = weak.upgrade() {
        f(&mut rc.borrow_mut());
    }
}

/// Run `f` against the live app (UI thread only). No-op after shutdown.
pub fn with_current(f: impl FnOnce(&mut AppState)) {
    let app = APP.with(|a| a.borrow().clone());
    if let Some(app) = app {
        f(&mut app.borrow_mut());
    }
}

#[allow(clippy::too_many_lines)]
fn wire_logic(state: &Rc<RefCell<AppState>>) {
    let st = state.borrow();
    let logic = st.main.global::<Logic>();

    // ── Connect view ────────────────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_connect_clicked(move || {
        with_app(&w, |app| {
            let nickname = app.main.global::<State>().get_nickname().to_string();
            if nickname.trim().is_empty() {
                app.show_error(&t!("error-nickname-empty"));
            } else {
                app.handle_connect(&nickname);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_connect_server_selected(move |index| {
        with_app(&w, |app| {
            // The dropdown pick becomes the default server — one
            // source of truth with the Servers tab's radio.
            if let Some(entry) = usize::try_from(index)
                .ok()
                .and_then(|i| app.settings.servers.get(i))
            {
                app.settings.server = entry.name.clone();
                app.settings.save();
            }
            // A pending 403 prompt referred to the previous server.
            let s = app.main.global::<State>();
            s.set_password_prompt(false);
            s.set_password("".into());
            crate::settingsui::refresh_servers(app);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_edit_servers(move || {
        with_app(&w, |app| {
            crate::settingsui::open(app);
            if let Some(win) = &app.settings_win {
                win.set_tab(crate::settingsui::TAB_SERVERS);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_save_password(move || {
        with_app(&w, |app| {
            let typed = app.main.global::<State>().get_password().to_string();
            app.save_prompt_password(&typed);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_open_settings(move || {
        with_app(&w, crate::settingsui::open);
    });
    logic.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // ── Sidebar ─────────────────────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_toggle_mute(move || with_app(&w, AppState::handle_toggle_mute));
    let w = Rc::downgrade(state);
    logic.on_toggle_deafen(move || with_app(&w, AppState::handle_toggle_deafen));
    let w = Rc::downgrade(state);
    logic.on_toggle_share(move || with_app(&w, AppState::handle_toggle_screenshare));
    let w = Rc::downgrade(state);
    logic.on_disconnect(move || with_app(&w, AppState::handle_disconnect));

    let w = Rc::downgrade(state);
    logic.on_user_menu(move |index| {
        with_app(&w, |app| app.open_user_popup(index));
    });
    let w = Rc::downgrade(state);
    logic.on_popup_set_volume(move |index, volume| {
        with_app(&w, |app| {
            if let Some(id) = app.user_id_at(index) {
                let username = app.username_of(id);
                app.handle_set_user_volume(id, &username, volume);
                app.main.global::<State>().set_user_popup_open(false);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_popup_toggle_mute(move |index| {
        with_app(&w, |app| {
            if let Some(id) = app.user_id_at(index) {
                let username = app.username_of(id);
                let muted = app.users.get(&id).is_some_and(|u| u.audio.is_muted);
                app.handle_mute_user(id, &username, !muted);
                app.main.global::<State>().set_user_popup_open(false);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_popup_reset_audio(move |index| {
        with_app(&w, |app| {
            if let Some(id) = app.user_id_at(index) {
                let username = app.username_of(id);
                app.handle_reset_peer_audio(id, &username);
                app.main.global::<State>().set_user_popup_open(false);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_popup_watch(move |index| {
        with_app(&w, |app| {
            if let Some(id) = app.user_id_at(index) {
                app.handle_watch_peer(id);
                app.main.global::<State>().set_user_popup_open(false);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_popup_closed(move || {
        with_app(&w, |app| {
            app.main.global::<State>().set_user_popup_open(false);
        });
    });

    // ── Chat ────────────────────────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_send_message(move || with_app(&w, AppState::send_current_message));
    let w = Rc::downgrade(state);
    logic.on_draft_edited(move || {
        with_app(&w, |app| {
            // Enforce the character cap host-side (chars, not bytes —
            // FLTK's maximum_size counted UTF-8 characters too) and
            // scrub control chars the moment they appear.
            let s = app.main.global::<State>();
            let draft = s.get_draft().to_string();
            let cleaned = app_core::chat::scrub_control_chars(&draft);
            let capped: String = cleaned
                .chars()
                .take(app_core::chat::MAX_MESSAGE_CHARS)
                .collect();
            if capped != draft {
                s.set_draft(capped.into());
            }
        });
    });
    logic.on_attach_image(|| {
        media::attach_image_from_disk();
    });
    let w = Rc::downgrade(state);
    logic.on_clear_chat(move || {
        with_app(&w, AppState::clear_chat);
    });
    let w = Rc::downgrade(state);
    logic.on_composer_newline(move |cursor, anchor| {
        with_app(&w, |app| {
            let s = app.main.global::<State>();
            let (text, pos) = app_core::composer::insert_newline(
                s.get_draft().as_str(),
                usize::try_from(cursor).unwrap_or(0),
                usize::try_from(anchor).unwrap_or(0),
            );
            s.set_draft(text.into());
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            app.main.invoke_composer_set_cursor(pos as i32);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_composer_line_edge(move |cursor, to_end| {
        with_app(&w, |app| {
            let s = app.main.global::<State>();
            let pos = app_core::composer::line_edge(
                s.get_draft().as_str(),
                usize::try_from(cursor).unwrap_or(0),
                to_end,
            );
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            app.main.invoke_composer_set_cursor(pos as i32);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_emoji_picked(move |glyph| {
        with_app(&w, |app| {
            let s = app.main.global::<State>();
            let mut draft = s.get_draft().to_string();
            draft.push_str(glyph.as_str());
            s.set_draft(draft.into());
            app.main.invoke_focus_composer();
        });
    });
    let w = Rc::downgrade(state);
    logic.on_paste_rich(move || {
        with_app(&w, media::paste_into_chat);
    });
    logic.on_link_clicked(|url| {
        if let Err(e) = open::that_detached(url.as_str()) {
            warn!("Failed to open link: {e}");
        }
    });
    logic.on_copy_text(|text| {
        crate::clipboard::set_clipboard_text(text.as_str());
    });
    let w = Rc::downgrade(state);
    logic.on_row_save_image(move |row_id| {
        with_app(&w, |app| media::save_image_dialog(app, row_id));
    });
    let w = Rc::downgrade(state);
    logic.on_row_open_image(move |row_id| {
        with_app(&w, |app| crate::viewers::open_gallery(app, row_id));
    });
    let w = Rc::downgrade(state);
    logic.on_viewer_step(move |dir| {
        with_app(&w, |app| crate::viewers::step_gallery(app, dir));
    });
    let w = Rc::downgrade(state);
    logic.on_viewer_save(move || {
        with_app(&w, |app| {
            if let Some(id) = app.gallery_row {
                media::save_image_dialog(app, id);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_viewer_close(move || {
        with_app(&w, crate::viewers::close_gallery);
    });
    let w = Rc::downgrade(state);
    logic.on_gif_hover(move |row_id, entering| {
        with_app(&w, |app| app.gif_hover(row_id, entering));
    });

    // ── Image preview overlay ───────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_preview_send(move || {
        with_app(&w, |app| {
            if let Some(png) = app.pending_preview.take() {
                // Same broadcast semantics as SendMessage: the server
                // relays ImageMessage back to everyone (sender
                // included), so no local echo is needed here.
                app.network.tx.send(NetworkControl::SendImage(png)).ok();
            }
            app.preview_gif = None;
            app.main.global::<State>().set_preview_open(false);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_preview_cancel(move || {
        with_app(&w, |app| {
            app.pending_preview = None;
            app.preview_gif = None;
            app.main.global::<State>().set_preview_open(false);
        });
    });

    // ── Screen share dialog ─────────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_start_share(move |monitor_index, quality_index, everyone| {
        with_app(&w, |app| {
            let monitor_index = usize::try_from(monitor_index).unwrap_or(0);
            let quality = usize::try_from(quality_index)
                .ok()
                .and_then(|i| video::Quality::ALL.get(i))
                .copied()
                .unwrap_or(video::Quality::FHD30);
            // Remember the choices: the dialog pre-fills from these
            // next time, and the share hotkey starts a stream with
            // them directly when Baucord isn't the focused window.
            app.settings.share.quality = quality.key().to_string();
            app.settings.share.monitor = u32::try_from(monitor_index).ok();
            app.settings.share.everyone = everyone;
            app.settings.save();
            app.screen
                .tx
                .send(ScreenControl::StartSharing {
                    quality,
                    monitor_index,
                    scope: if everyone {
                        client::ShareScope::Everyone
                    } else {
                        client::ShareScope::Channel
                    },
                })
                .ok();
        });
    });

    // ── Voice channels ──────────────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_channel_clicked(move |channel_id| {
        with_app(&w, |app| {
            if let Ok(id) = u16::try_from(channel_id) {
                app.channel_clicked(id);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_channel_password_ok(move || {
        with_app(&w, AppState::channel_password_ok);
    });
    let w = Rc::downgrade(state);
    logic.on_channel_password_cancel(move || {
        with_app(&w, AppState::channel_password_cancel);
    });

    // ── Update flow ─────────────────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_update_now(move || {
        with_app(&w, crate::updater::start_download);
    });
    let w = Rc::downgrade(state);
    logic.on_update_skip(move || {
        with_app(&w, AppState::show_connect_phase);
    });
    let w = Rc::downgrade(state);
    logic.on_update_cancel(move || {
        with_app(&w, |app| {
            app.download_cancelled.store(true, Ordering::Relaxed);
            app.show_connect_phase();
        });
    });
    let w = Rc::downgrade(state);
    logic.on_update_install(move || {
        with_app(&w, |app| {
            if let Some(path) = app.update_zip_path.clone()
                && let Err(e) = crate::updater::extract_and_install(&path)
            {
                app.show_error(&e);
            }
        });
    });

    drop(st);

    // OS close button on the main window. App decides what it means:
    // hide when connected (tray keeps the process reachable), quit from
    // the connect view.
    let w = Rc::downgrade(state);
    let main = state.borrow().main.clone_strong();
    main.window().on_close_requested(move || {
        let mut response = slint::CloseRequestResponse::HideWindow;
        with_app(&w, |app| {
            if cfg!(feature = "tray") {
                // Going to the tray: stop all GIF work right now rather
                // than on the next tick — nothing is visible anymore.
                app.freeze_chat_gifs();
                response = slint::CloseRequestResponse::HideWindow;
            } else {
                app.save_window_geometry();
                response = slint::CloseRequestResponse::HideWindow;
                let _ = slint::quit_event_loop();
            }
        });
        response
    });
}

/// Force one full repaint of `win` whenever the OS signals the window
/// surface may have been invalidated: focus gain (restore — however
/// fast, alt-tab back, show-from-tray, the first map after creation)
/// or un-occlusion. Windows repaints revealed surfaces with the
/// background brush while the renderer's damage tracking still trusts
/// its back buffer; partial presents then patch only what changed
/// since. `flip` toggles the window's imperceptible full-window
/// overlay, dirtying everything for exactly one frame. Event-driven —
/// unlike the earlier is_minimized() polling (lost fast
/// minimize/restore races) and flip-on-show (fired before the OS
/// mapped and cleared the real surface).
/// Two trigger classes:
/// * Focus gain / un-occlusion — restore (however fast), alt-tab
///   back, activating a newly created window.
/// * `armed` — a one-shot consumed on the first OS-driven paint
///   (`RedrawRequested`) after the app shows a hidden window. Windows
///   does NOT activate a window shown from a tray interaction
///   (foreground-lock rules), so no Focused event arrives there; and
///   flipping at the `show()` call itself fires before the OS maps
///   and clears the real surface. The first paint of the fresh
///   surface is the exact right moment.
pub fn repaint_on_reveal(win: &slint::Window, armed: Rc<Cell<bool>>, flip: impl Fn() + 'static) {
    use i_slint_backend_winit::WinitWindowAccessor;
    win.on_winit_window_event(move |_w, ev| {
        use i_slint_backend_winit::winit::event::WindowEvent;
        let reveal = matches!(
            ev,
            WindowEvent::Focused(true) | WindowEvent::Occluded(false)
        ) || (matches!(ev, WindowEvent::RedrawRequested) && armed.take());
        if reveal {
            flip();
        }
        i_slint_backend_winit::EventResult::Propagate
    });
}

// ── Timers ──────────────────────────────────────────────────────────────

fn start_timers(state: &Rc<RefCell<AppState>>) {
    let mut timers = Vec::new();

    // Device poll — 2.5 s. Enumeration is cheap; all diffing logic
    // lives in app-core's reducer.
    let w = Rc::downgrade(state);
    let t = slint::Timer::default();
    t.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(2500),
        move || {
            let input = audio::VoiceChat::list_input_devices().unwrap_or_default();
            let output = audio::VoiceChat::list_output_devices().unwrap_or_default();
            with_app(&w, |app| app.handle_devices_changed(input, output));
        },
    );
    timers.push(t);

    // Resource sampler — 2 s cadence, matching NetworkStats. The
    // handler skips the sampling entirely while the main screen isn't
    // on screen (tray/minimized/connect view) so the idle cost is one
    // no-op closure per tick.
    let w = Rc::downgrade(state);
    let t = slint::Timer::default();
    t.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(2000),
        move || {
            with_app(&w, AppState::sample_resources);
        },
    );
    timers.push(t);

    state.borrow_mut().timers = timers;
}

// ── The drain (worker → UI) ─────────────────────────────────────────────

impl AppState {
    /// Drain every worker channel. Bounded like the FLTK loop was, and
    /// `FrameReady` is coalesced per stream: if frames queued while the
    /// UI thread was busy, blitting each stale frame just replays the
    /// past — show the newest, skip the rest.
    fn drain(app: &Rc<RefCell<AppState>>) {
        let mut st = app.borrow_mut();

        // Global hotkeys — collect first, then handle.
        let mut hotkey_actions = Vec::new();
        if let Some(hotkeys) = &st.hotkeys {
            while let Some(action) = hotkeys.poll() {
                hotkey_actions.push(action);
            }
        }
        for action in hotkey_actions {
            if !st.connected {
                continue;
            }
            match action {
                BindableAction::ToggleMute => st.handle_toggle_mute(),
                BindableAction::ToggleDeafen => st.handle_toggle_deafen(),
                BindableAction::ToggleScreenShare => st.handle_toggle_screenshare(),
                BindableAction::PushToTalk => {}
            }
        }

        for _ in 0..256 {
            match st.network.msg_rx.try_recv() {
                Ok(msg) => st.handle_network_message(msg),
                Err(_) => break,
            }
        }

        for _ in 0..32 {
            match st.voice.event_rx.try_recv() {
                Ok(evt) => st.handle_voice_event(evt),
                Err(_) => break,
            }
        }

        // Non-frame screen events, forwarded by the present-prep
        // thread (which owns screen.event_rx now — see present.rs).
        for _ in 0..64 {
            match st.ui_screen_rx.try_recv() {
                Ok(evt) => st.handle_screen_event(evt),
                Err(_) => break,
            }
        }
        // Frames arrive as ready-made SharedPixelBuffers: the copy was
        // paid on the prep thread; presenting is an Image wrap + a
        // property set. Newest-per-stream (older frames were coalesced
        // prep-side, before anyone paid for their conversion).
        let prepared: Vec<crate::present::PreparedFrame> = {
            let mut slots = st.present.slots.lock().unwrap();
            slots.drain().map(|(_, f)| f).collect()
        };
        for frame in prepared {
            st.present_frame(frame);
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  EVENT HANDLERS
    // ════════════════════════════════════════════════════════════════

    fn handle_voice_event(&mut self, evt: VoiceEvent) {
        match evt {
            VoiceEvent::Started => {
                // No sound here: this event fires whenever the engine
                // boots, including for standalone mic tests. The join
                // ding is sent from handle_init_client instead.
                info!("Voice engine ready");
            }
            VoiceEvent::Error(e) => self.show_error(&e),
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

    #[allow(clippy::too_many_lines)]
    /// Present one prepared frame: wrap the (already-copied) pixel
    /// buffer in an Image handle and hand it to the viewer window. The
    /// heavy work happened on the prep thread; this path stays well
    /// under a millisecond, which is what lets the UI keep up with
    /// 60 fps streams.
    fn present_frame(&mut self, f: crate::present::PreparedFrame) {
        let present_at = Instant::now();
        let net_to_decode_ms = f.decoded_at.duration_since(f.enqueued_at).as_secs_f32() * 1000.0;
        let decode_to_present_ms = present_at.duration_since(f.decoded_at).as_secs_f32() * 1000.0;
        let total_ms = present_at.duration_since(f.enqueued_at).as_secs_f32() * 1000.0;
        if total_ms > 25.0 {
            warn!(
                "SLOW FRAME seq={}: {total_ms:.1}ms total (net→decode: {net_to_decode_ms:.1}ms, decode→present: {decode_to_present_ms:.1}ms)",
                f.seq
            );
        }
        if let Some(viewer) = self.viewers.get_mut(&f.client_id) {
            let set_start = Instant::now();
            viewer.update_frame(
                f.buffer,
                f.width,
                f.height,
                f.frame_bytes,
                f.buffer_depth_ms,
                f.buffer_target_ms,
                f.buffer_jitter_ms,
            );
            let set_ms = set_start.elapsed().as_secs_f32() * 1000.0;
            if set_ms > 5.0 {
                warn!("Slow present: {set_ms:.1}ms for seq {}", f.seq);
            }
        }
    }

    fn handle_screen_event(&mut self, evt: ScreenEvent) {
        match evt {
            ScreenEvent::SharingStarted { quality } => {
                info!("Screen share started: {quality}");
                self.push_chat_system(&t!("chat-share-started", "quality" => quality.to_string()));
                self.main.global::<State>().set_sharing(true);
                // Start the 1-second stream stats timer once. It stops
                // itself when sharing ends; the guard prevents stacking
                // a new repeating timer on every share start.
                if !self.stream_stats_timer_active.swap(true, Ordering::Relaxed) {
                    let timer_active = self.stream_stats_timer_active.clone();
                    let is_sharing = self.screen.shared.is_sharing.clone();
                    let t = slint::Timer::default();
                    let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
                    if let Some(weak) = weak {
                        t.start(
                            slint::TimerMode::Repeated,
                            Duration::from_secs(1),
                            move || {
                                if is_sharing.load(Ordering::Relaxed) {
                                    with_app(&weak, AppState::update_stream_stats);
                                } else {
                                    timer_active.store(false, Ordering::Relaxed);
                                    with_app(&weak, |app| {
                                        // Timer removes itself by
                                        // dropping out of the vec on
                                        // the next share start.
                                        app.main.global::<State>().set_stream_stats("".into());
                                    });
                                }
                            },
                        );
                        self.timers.push(t);
                    }
                }
            }
            ScreenEvent::SharingStopped => {
                info!("Screen share stopped");
                self.push_chat_system(&t!("chat-share-stopped"));
                let s = self.main.global::<State>();
                s.set_sharing(false);
                // Clear the stats line — it used to keep showing the
                // last sample after the share ended.
                s.set_stream_stats("".into());
            }
            ScreenEvent::Error(e) => {
                self.show_error(&t!("error-screen-share", "error" => e.as_str()));
            }
            // Frames never reach this handler anymore: the
            // present-prep thread consumes them from screen.event_rx
            // and the UI receives PreparedFrames through the slot map
            // instead (see present.rs / present_frame below).
            ScreenEvent::FrameReady { .. } => {
                debug_assert!(false, "FrameReady is consumed by the present-prep thread");
            }
            ScreenEvent::PeerStartedSharing {
                client_id,
                width,
                height,
            } => {
                let username = self.username_of(client_id);
                info!("{username} started sharing ({width}x{height})");
                self.push_chat_system(&t!("chat-peer-sharing", "name" => username.as_str()));
            }
            ScreenEvent::PeerStoppedSharing { client_id } => {
                let username = self.username_of(client_id);
                info!("{username} stopped sharing");
                self.push_chat_system(
                    &t!("chat-peer-stopped-sharing", "name" => username.as_str()),
                );
                if let Some(viewer) = self.viewers.remove(&client_id) {
                    viewer.close();
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_network_message(&mut self, msg: NetworkMessage) {
        match msg {
            NetworkMessage::Connected => {
                self.connected = true;
                self.reconnect_attempts = 0;
                let s = self.main.global::<State>();
                s.set_error_text("".into());
                s.set_password_prompt(false);
            }
            NetworkMessage::ConnectionLost { reason } => {
                // Network, not credentials: come back on our own, with
                // backoff (2, 4, 8, 16, 30 s), instead of dumping the
                // user on the connect screen with a transport error.
                self.push_chat_system(&t!("chat-disconnected", "reason" => reason.as_str()));
                self.teardown_connection();
                self.show_connect_phase();
                const MAX_AUTO_RECONNECTS: u32 = 5;
                if self.reconnect_attempts >= MAX_AUTO_RECONNECTS {
                    self.show_error(&reason);
                } else {
                    self.reconnect_attempts += 1;
                    let secs = (2u64 << (self.reconnect_attempts - 1)).min(30);
                    let secs_text = secs.to_string();
                    self.show_error(&t!(
                        "error-connection-lost",
                        "reason" => reason.as_str(),
                        "secs" => secs_text.as_str()
                    ));
                    let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
                    if let Some(weak) = weak {
                        self.reconnect_timer.start(
                            slint::TimerMode::SingleShot,
                            Duration::from_secs(secs),
                            move || {
                                with_app(&weak, |app| {
                                    if !app.connected {
                                        let nickname = app.settings.user.nickname.clone();
                                        app.handle_connect(&nickname);
                                    }
                                });
                            },
                        );
                    }
                }
            }
            NetworkMessage::ServerIdentity {
                fingerprint,
                first_use,
            } => {
                // Trust on first use (or a signed key rotation): pin it
                // on the server entry so a different key is refused
                // from now on.
                let changed = self
                    .settings
                    .default_server()
                    .is_none_or(|s| s.fingerprint.as_deref() != Some(fingerprint.as_str()));
                if changed {
                    if let Some(server) = self.settings.default_server_mut() {
                        server.fingerprint = Some(fingerprint.clone());
                    }
                    self.settings.save();
                    crate::settingsui::refresh_servers(self);
                    if first_use {
                        info!("Pinned server identity {fingerprint}");
                        self.push_chat_system(
                            &t!("chat-identity-pinned", "fingerprint" => fingerprint.as_str()),
                        );
                    }
                }
            }
            NetworkMessage::IdentityChanged { expected, observed } => {
                // Nothing was sent to that server. No silent accept: the
                // user decides, by editing (or clearing) the pin.
                let text = t!(
                    "error-identity-changed",
                    "expected" => expected.as_str(),
                    "observed" => observed.as_str()
                );
                self.push_chat_system(&text);
                self.teardown_connection();
                self.show_connect_phase();
                self.show_error(&text);
            }
            NetworkMessage::AccessDenied => {
                // The server refused our Hello for the room password:
                // surface the inline password prompt on the connect card
                // (persistent, unlike the 5s error toast) so the user
                // can save the password and hit Connect again.
                info!("Connect refused: wrong or missing server password");
                let s = self.main.global::<State>();
                s.set_password_prompt(true);
                s.set_password("".into());
            }
            NetworkMessage::Disconnected(reason) => {
                if self.connected {
                    self.push_chat_system(&t!("chat-disconnected", "reason" => reason.as_str()));
                    self.teardown_connection();
                    self.show_connect_phase();
                    self.show_error(&reason);
                } else {
                    self.show_error(&reason);
                }
            }
            NetworkMessage::InitClient {
                users,
                client_id,
                server_name,
                greeting,
                channels,
                memberships,
            } => {
                self.handle_init_client(
                    client_id,
                    &users,
                    &server_name,
                    &greeting,
                    &channels,
                    &memberships,
                );
            }
            NetworkMessage::UserJoined {
                client_id,
                username,
                channel_id,
            } => self.handle_user_joined(client_id, username, channel_id),
            NetworkMessage::UserMovedChannel {
                client_id,
                channel_id,
            } => self.handle_user_moved(client_id, channel_id),
            NetworkMessage::ChannelJoinDenied { channel_id, reason } => {
                self.handle_channel_join_denied(channel_id, reason);
            }
            NetworkMessage::ChannelListUpdated {
                channels,
                memberships,
            } => self.handle_channels_updated(&channels, &memberships),
            NetworkMessage::UserLeft {
                client_id,
                username,
            } => self.handle_user_left(client_id, &username),
            NetworkMessage::TextMessage { username, text } => {
                let timestamp = chrono::Local::now().format("%H:%M").to_string();
                // Render shortcodes from clients that send them raw.
                let text = app_core::emoji::replace_shortcodes(&text);
                self.push_chat_message(&timestamp, &username, &text);
            }
            NetworkMessage::UploadProgress { pct } => {
                let s = self.main.global::<State>();
                if pct >= 100 {
                    s.set_upload_line("".into());
                } else {
                    s.set_upload_line(t!("chat-upload-progress", "pct" => u32::from(pct)).into());
                }
            }
            NetworkMessage::ImageMessage { username, data } => {
                let timestamp = chrono::Local::now().format("%H:%M").to_string();
                if !media::push_image_row(self, &timestamp, &username, data) {
                    self.push_chat_system(
                        &t!("chat-image-undisplayable", "name" => username.as_str()),
                    );
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
            NetworkMessage::NetworkStats {
                rtt_ms,
                loss_pct,
                kbps_up,
                kbps_down,
            } => {
                // Skip the label churn when nobody can see it (window in
                // the tray or minimized). Labels refresh at the next 2 s
                // sample once visible again.
                if self.main_on_screen() {
                    let audio_ms = self.voice.shared.jitter_buffer_ms.load(Ordering::Relaxed);
                    self.update_latency(rtt_ms, loss_pct, Some(audio_ms), kbps_up, kbps_down);
                }
            }
            NetworkMessage::ShareDenied { reason } => {
                // The screen thread starts capturing optimistically
                // before the server answers — stop it so local state
                // matches the server's.
                if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                    self.screen.tx.send(ScreenControl::StopSharing).ok();
                }
                self.show_error(&format!("Screen share denied: {}", reason.describe()));
            }
            NetworkMessage::SubscribeDenied { target_id, reason } => {
                // Close the viewer window WatchPeer opened optimistically.
                if let Some(viewer) = self.viewers.remove(&target_id) {
                    viewer.close();
                }
                let username = self.username_of(target_id);
                self.push_chat_system(&format!("Can't watch {username}: {}", reason.describe()));
            }
            NetworkMessage::ScreenShareStarted {
                client_id,
                width,
                height,
                scope: _,
            } => {
                self.active_sharers.insert(client_id, (width, height));
                self.sharer_ids = self.active_sharers.keys().copied().collect();
                self.update_user_list();
            }
            NetworkMessage::ScreenShareStopped { client_id } => {
                self.active_sharers.remove(&client_id);
                self.sharer_ids = self.active_sharers.keys().copied().collect();
                self.update_user_list();
            }
            // Hot-path variants routed straight to the voice/screen
            // threads by the network loop. They should never arrive
            // here; if they do we silently ignore them.
            NetworkMessage::SystemAudioData { .. }
            | NetworkMessage::VoiceData { .. }
            | NetworkMessage::VideoFrame { .. }
            | NetworkMessage::StreamHealth { .. }
            | NetworkMessage::VoiceReceiverReport { .. }
            | NetworkMessage::UplinkVideo { .. }
            | NetworkMessage::KeyframeRequested { .. } => {}
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  CONNECTION FLOW
    // ════════════════════════════════════════════════════════════════

    fn handle_connect(&mut self, nickname: &str) {
        self.reconnect_timer.stop();
        self.settings.user.nickname = nickname.to_string();

        // A password typed into the 403 prompt but not yet saved still
        // counts — clicking Connect means "use what I typed".
        let s = self.main.global::<State>();
        if s.get_password_prompt() {
            let typed = s.get_password().trim().to_string();
            if !typed.is_empty() {
                self.save_prompt_password(&typed);
            }
        }
        self.settings.save();

        let Some(server) = self.settings.default_server().cloned() else {
            self.show_error(&t!("error-invalid-address"));
            return;
        };
        let Ok(addr) = server.address.trim().parse::<SocketAddr>() else {
            self.show_error(&t!("error-invalid-address"));
            return;
        };

        self.network
            .tx
            .send(NetworkControl::Connect {
                addr,
                nickname: nickname.to_string(),
                password: server.password,
                fingerprint: server.fingerprint,
                client_key: self.client_key.clone(),
                portable: client::config::is_portable_install(),
            })
            .ok();
    }

    /// Store `password` on the default (= dropdown-selected) server
    /// and dismiss the 403 prompt. Empty input clears the saved one.
    fn save_prompt_password(&mut self, password: &str) {
        let password = password.trim();
        if let Some(server) = self.settings.default_server_mut() {
            server.password = (!password.is_empty()).then(|| password.to_string());
        }
        self.settings.save();
        let s = self.main.global::<State>();
        s.set_password_prompt(false);
        s.set_password("".into());
        // Mirror into the settings window's editor if it's open.
        crate::settingsui::refresh_servers(self);
    }

    fn handle_init_client(
        &mut self,
        client_id: u64,
        users: &HashMap<u64, String>,
        server_name: &str,
        greeting: &str,
        channels: &[client::ChannelInfo],
        memberships: &HashMap<u64, u16>,
    ) {
        // Fresh chat per connection — same semantics as the old
        // rebuild-the-main-view path.
        self.history.clear();
        self.chat_model.set_vec(Vec::new());
        self.images.clear();
        self.gifs.clear();

        self.main.global::<State>().set_phase(Phase::Main);

        self.our_client_id = Some(client_id);
        self.server_name = server_name.to_string();
        self.channels = channels
            .iter()
            .map(|c| ChannelMeta {
                id: c.id,
                name: c.name.clone(),
                locked: c.has_password,
                user_limit: c.user_limit,
            })
            .collect();
        self.pending_channel_join = None;
        let default_audio = PeerAudioSettings::default();
        for (&id, name) in users {
            self.users.insert(
                id,
                UserState {
                    username: name.clone(),
                    is_speaking: false,
                    audio: peer_audio_from(
                        self.settings
                            .peer_settings
                            .get(&peer_key(name))
                            .unwrap_or(&default_audio),
                    ),
                    is_self_muted: false,
                    is_self_deafened: false,
                    channel_id: memberships.get(&id).copied().unwrap_or_default(),
                },
            );
        }

        // Start voice engine
        self.voice
            .tx
            .send(VoiceControl::Start {
                settings: self.settings.clone(),
                our_client_id: client_id,
            })
            .ok();
        // Join ding, queued behind Start on the same channel so it
        // plays on the freshly started (or still-running) engine.
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserJoined))
            .ok();

        // Sync per-user audio settings to voice engine
        for (&id, user) in &self.users {
            self.voice.tx.send(VoiceControl::AddPeer(id)).ok();
            self.voice
                .tx
                .send(VoiceControl::SetPeerAudio(id, peer_audio_to(user.audio)))
                .ok();
        }

        let s = self.main.global::<State>();
        s.set_status_text(format!("●  {}", t!("status-connected")).into());
        s.set_status_color(slint::Color::from_rgb_u8(0x2B, 0xAC, 0x76));
        self.push_chat_system(&format!("{server_name}: {greeting}"));
        self.update_user_list();
        self.show_main();
    }

    fn handle_user_joined(&mut self, client_id: u64, username: String, channel_id: u16) {
        let saved = self
            .settings
            .peer_settings
            .get(&peer_key(&username))
            .cloned()
            .unwrap_or_default();

        self.voice.tx.send(VoiceControl::AddPeer(client_id)).ok();
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(client_id, saved.clone()))
            .ok();

        self.push_chat_system(&t!("chat-user-joined", "name" => username.as_str()));
        self.users.insert(
            client_id,
            UserState::new(username, peer_audio_from(&saved), channel_id),
        );
        // Join ding only for arrivals into *our* channel — like the
        // classics, other rooms come and go silently.
        if Some(channel_id) == self.our_channel() {
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::UserJoined))
                .ok();
        }
        self.update_user_list();
    }

    fn handle_user_left(&mut self, client_id: u64, username: &str) {
        self.voice.tx.send(VoiceControl::RemovePeer(client_id)).ok();
        let was_in_our_channel = self
            .users
            .remove(&client_id)
            .is_some_and(|u| Some(u.channel_id) == self.our_channel());
        self.active_sharers.remove(&client_id);
        self.sharer_ids = self.active_sharers.keys().copied().collect();

        // Close viewer if we were watching this user
        if let Some(viewer) = self.viewers.remove(&client_id) {
            viewer.close();
        }

        self.push_chat_system(&t!("chat-user-left", "name" => username));
        if was_in_our_channel {
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::UserLeft))
                .ok();
        }
        self.update_user_list();
    }

    /// The channel we are currently in, per server echo.
    fn our_channel(&self) -> Option<u16> {
        self.our_client_id
            .and_then(|id| self.users.get(&id))
            .map(|u| u.channel_id)
    }

    fn channel_name(&self, channel_id: u16) -> String {
        self.channels
            .iter()
            .find(|c| c.id == channel_id)
            .map_or_else(|| format!("#{channel_id}"), |c| c.name.clone())
    }

    fn handle_user_moved(&mut self, client_id: u64, channel_id: u16) {
        let our_channel_before = self.our_channel();
        let old = if let Some(user) = self.users.get_mut(&client_id) {
            let old = user.channel_id;
            user.channel_id = channel_id;
            Some(old)
        } else {
            None
        };

        if Some(client_id) == self.our_client_id {
            // Our own move confirmed — close the prompt if it drove
            // this, and ding into the new room.
            self.pending_channel_join = None;
            let state = self.main.global::<State>();
            state.set_channel_prompt_open(false);
            state.set_channel_password("".into());
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::UserJoined))
                .ok();
        } else if Some(channel_id) == our_channel_before {
            // Someone stepped into our room.
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::UserJoined))
                .ok();
        } else if old.is_some() && old == our_channel_before {
            // Someone stepped out of our room.
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::UserLeft))
                .ok();
        }
        self.update_user_list();
    }

    /// The server hot-reloaded its channel list: resync wholesale.
    /// Ids are renumbered, so an open password prompt is re-targeted by
    /// channel *name* (and closed if that channel is gone). Deliberately
    /// no join/leave sounds — an admin edit shouldn't set off a chorus.
    fn handle_channels_updated(
        &mut self,
        channels: &[client::ChannelInfo],
        memberships: &HashMap<u64, u16>,
    ) {
        let pending_name = self.pending_channel_join.and_then(|id| {
            self.channels
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
        });

        self.channels = channels
            .iter()
            .map(|c| ChannelMeta {
                id: c.id,
                name: c.name.clone(),
                locked: c.has_password,
                user_limit: c.user_limit,
            })
            .collect();
        for (id, channel_id) in memberships {
            if let Some(user) = self.users.get_mut(id) {
                user.channel_id = *channel_id;
            }
        }

        self.pending_channel_join = pending_name
            .as_ref()
            .and_then(|name| self.channels.iter().find(|c| &c.name == name))
            .map(|c| c.id);
        if pending_name.is_some() && self.pending_channel_join.is_none() {
            // The channel the prompt was for no longer exists.
            self.channel_password_cancel();
        }

        self.push_chat_system(&format!(
            "{}: {}",
            self.server_name,
            t!("chat-channels-updated"),
        ));
        self.update_user_list();
    }

    fn handle_channel_join_denied(&mut self, channel_id: u16, reason: client::ChannelDenyReason) {
        match reason {
            client::ChannelDenyReason::WrongPassword => {
                // A saved password can go stale; whatever was stored is
                // wrong now, so re-prompt (and overwrite on OK).
                self.open_channel_prompt(channel_id, true);
            }
            client::ChannelDenyReason::ChannelFull => {
                self.pending_channel_join = None;
                self.show_error(&t!("channel-full"));
            }
            client::ChannelDenyReason::NoSuchChannel => {
                self.pending_channel_join = None;
                self.show_error(reason.describe());
            }
        }
    }

    /// Open the channel password prompt for `channel_id`.
    fn open_channel_prompt(&mut self, channel_id: u16, wrong: bool) {
        self.pending_channel_join = Some(channel_id);
        let name = self.channel_name(channel_id);
        let state = self.main.global::<State>();
        state.set_channel_prompt_name(name.into());
        state.set_channel_prompt_error(if wrong {
            t!("channel-wrong-password").into()
        } else {
            "".into()
        });
        state.set_channel_password("".into());
        state.set_channel_prompt_open(true);
    }

    /// A channel header was clicked: join directly, or via saved /
    /// prompted password for locked channels.
    fn channel_clicked(&mut self, channel_id: u16) {
        if Some(channel_id) == self.our_channel() {
            return;
        }
        let Some(meta) = self.channels.iter().find(|c| c.id == channel_id) else {
            return;
        };
        if !meta.locked {
            self.pending_channel_join = Some(channel_id);
            self.network
                .tx
                .send(NetworkControl::JoinChannel {
                    channel_id,
                    password: None,
                })
                .ok();
            return;
        }
        // Locked: try the saved password first; the server's
        // WrongPassword answer reopens the prompt if it went stale.
        let name = meta.name.clone();
        let saved = self
            .settings
            .default_server()
            .and_then(|server| server.channel_passwords.get(&name).cloned());
        match saved {
            Some(password) => {
                self.pending_channel_join = Some(channel_id);
                self.network
                    .tx
                    .send(NetworkControl::JoinChannel {
                        channel_id,
                        password: Some(password),
                    })
                    .ok();
            }
            None => self.open_channel_prompt(channel_id, false),
        }
    }

    /// OK on the password prompt: save the password on the server
    /// entry, then retry the join. The prompt closes when the server
    /// echoes our move (or reopens on WrongPassword).
    fn channel_password_ok(&mut self) {
        let Some(channel_id) = self.pending_channel_join else {
            self.main.global::<State>().set_channel_prompt_open(false);
            return;
        };
        let password = self
            .main
            .global::<State>()
            .get_channel_password()
            .to_string();
        if password.is_empty() {
            return;
        }
        let name = self.channel_name(channel_id);
        if let Some(server) = self.settings.default_server_mut() {
            server.channel_passwords.insert(name, password.clone());
            self.settings.save();
        }
        self.main.global::<State>().set_channel_prompt_open(false);
        self.network
            .tx
            .send(NetworkControl::JoinChannel {
                channel_id,
                password: Some(password),
            })
            .ok();
    }

    fn channel_password_cancel(&mut self) {
        self.pending_channel_join = None;
        let state = self.main.global::<State>();
        state.set_channel_prompt_open(false);
        state.set_channel_password("".into());
    }

    /// Shared teardown for both user-initiated and server-initiated
    /// disconnects.
    fn teardown_connection(&mut self) {
        self.connected = false;
        self.is_testing_mic = false;
        self.channels.clear();
        self.pending_channel_join = None;
        {
            let state = self.main.global::<State>();
            state.set_channel_prompt_open(false);
            state.set_channel_password("".into());
        }
        // Also clear the shared flag the voice loop reads — leaving it
        // set (disconnect during a mic test) kept the loop gating
        // outgoing audio after the next connect.
        self.is_testing_mic_shared.store(false, Ordering::Relaxed);

        // Play the leave sound for ourselves too. The server's UserLeft
        // broadcast is never sent to the leaver — and stopping the
        // engine immediately would cut the sound off, so the Stop is
        // deferred. Its closure skips the Stop if a new session or mic
        // test started in the meantime.
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserLeft))
            .ok();
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            slint::Timer::single_shot(Duration::from_millis(900), move || {
                with_app(&weak, |app| {
                    if !app.connected && !app.is_testing_mic {
                        app.voice.tx.send(VoiceControl::Stop).ok();
                    }
                });
            });
        }

        // Stop our screen share if active
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
        }
        self.active_sharers.clear();
        self.sharer_ids.clear();
        for (_, viewer) in self.viewers.drain() {
            viewer.close();
        }
        crate::viewers::close_gallery(self);
        self.gifs.clear();

        self.users.clear();
        self.our_client_id = None;
        self.is_muted = false;
        self.is_deafened = false;

        let s = self.main.global::<State>();
        s.set_muted(false);
        s.set_deafened(false);
        s.set_sharing(false);
        s.set_user_popup_open(false);
        s.set_stream_stats("".into());
        s.set_status_text(format!("●  {}", t!("status-disconnected")).into());
        s.set_status_color(slint::Color::from_rgb_u8(0xE0, 0x1E, 0x5A));
        s.set_latency_line(
            t!("latency-line",
                "audio" => "--", "network" => "--", "up" => "--", "down" => "--")
            .into(),
        );
        s.set_resource_line(" ".into());
        s.set_users_count_label(t!("users-count", "count" => 0u32).into());
        self.users_model.set_vec(Vec::new());
    }

    fn handle_disconnect(&mut self) {
        self.reconnect_timer.stop();
        self.reconnect_attempts = 0;
        self.network.tx.send(NetworkControl::Disconnect).ok();
        self.push_chat_system(&t!("chat-disconnected", "reason" => t!("disconnect-reason-user")));
        self.teardown_connection();
        self.show_connect_phase();
    }

    pub fn show_connect_phase(&mut self) {
        self.refresh_device_warning();
        self.main.global::<State>().set_phase(Phase::Connect);
        self.show_main();
    }

    // ════════════════════════════════════════════════════════════════
    //  VOICE CONTROLS
    // ════════════════════════════════════════════════════════════════

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
        self.main.global::<State>().set_muted(self.is_muted);
        self.update_user_list();
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
        self.main.global::<State>().set_deafened(self.is_deafened);
        self.update_user_list();
    }

    fn handle_toggle_screenshare(&mut self) {
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
            return;
        }
        // Hotkey UX: when the user is in another app (typically a
        // game), don't steal focus with a picker — start streaming
        // immediately with the saved settings. The dialog shows only
        // when Baucord itself is focused (which covers the in-app
        // share button — clicking it focuses us) or when focus can't
        // be determined (never start a stream on a guess).
        let baucord_focused = video::capture::focused_pid().map(|pid| pid == std::process::id());
        match baucord_focused {
            Some(false) => self.start_share_with_saved(),
            _ => self.open_share_dialog(),
        }
    }

    /// Start sharing immediately with the last-used settings (falls
    /// back to primary monitor / FHD30 / channel-only). Hotkey path
    /// while another app is focused.
    fn start_share_with_saved(&mut self) {
        let monitors = video::capture::list_monitors().unwrap_or_default();
        if monitors.is_empty() {
            self.show_error(&t!("error-no-monitors"));
            return;
        }
        let monitor_index = self.saved_or_primary_monitor(&monitors);
        let quality =
            video::Quality::from_key(&self.settings.share.quality).unwrap_or(video::Quality::FHD30);
        let everyone = self.settings.share.everyone;
        log::info!(
            "share hotkey pressed outside Baucord — starting immediately with saved \
             settings: {quality}, monitor {monitor_index}, everyone={everyone}"
        );
        self.screen
            .tx
            .send(ScreenControl::StartSharing {
                quality,
                monitor_index,
                scope: if everyone {
                    client::ShareScope::Everyone
                } else {
                    client::ShareScope::Channel
                },
            })
            .ok();
    }

    /// Open the share picker, pre-filled with the last-used settings.
    fn open_share_dialog(&mut self) {
        let monitors = video::capture::list_monitors().unwrap_or_default();
        if monitors.is_empty() {
            self.show_error(&t!("error-no-monitors"));
            return;
        }
        let labels: Vec<slint::SharedString> = monitors
            .iter()
            .map(|m| {
                let primary = if m.is_primary {
                    format!(" {}", t!("monitor-primary"))
                } else {
                    String::new()
                };
                format!("{} — {}x{}{}", m.name, m.width, m.height, primary).into()
            })
            .collect();
        let qualities: Vec<slint::SharedString> = video::Quality::ALL
            .iter()
            .map(|q| q.to_string().into())
            .collect();
        let default_monitor = self.saved_or_primary_monitor(&monitors);
        let saved_quality =
            video::Quality::from_key(&self.settings.share.quality).unwrap_or(video::Quality::FHD30);
        let default_q = video::Quality::ALL
            .iter()
            .position(|q| *q == saved_quality)
            .unwrap_or(0);
        let s = self.main.global::<State>();
        s.set_monitors(ModelRc::new(VecModel::from(labels)));
        s.set_qualities(ModelRc::new(VecModel::from(qualities)));
        s.set_default_monitor(i32::try_from(default_monitor).unwrap_or(0));
        s.set_default_quality(i32::try_from(default_q).unwrap_or(0));
        s.set_default_scope(i32::from(self.settings.share.everyone));
        self.main.invoke_open_share_dialog();
    }

    /// The saved monitor index if that monitor still exists, otherwise
    /// the primary (monitors come and go between sessions).
    fn saved_or_primary_monitor(&self, monitors: &[video::capture::MonitorInfo]) -> usize {
        let primary_idx = monitors.iter().position(|m| m.is_primary).unwrap_or(0);
        self.settings
            .share
            .monitor
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < monitors.len())
            .unwrap_or(primary_idx)
    }

    pub fn handle_test_microphone(&mut self, enabled: bool) {
        // Capture this BEFORE flipping is_testing_mic — checking
        // afterwards made the "start the engine for a mic test while
        // disconnected" branch unreachable.
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

    /// Send the composer draft: shortcodes rendered, control chars
    /// scrubbed, char cap enforced. The server echoes `TextMessage` back
    /// to everyone (sender included), so there is no local append here.
    fn send_current_message(&mut self) {
        let s = self.main.global::<State>();
        let raw = s.get_draft().to_string();
        let rendered =
            app_core::emoji::replace_shortcodes(&app_core::chat::scrub_control_chars(raw.trim()));
        let text: String = rendered
            .chars()
            .take(app_core::chat::MAX_MESSAGE_CHARS)
            .collect();
        if text.is_empty() {
            return;
        }
        self.network.tx.send(NetworkControl::SendMessage(text)).ok();
        s.set_draft("".into());
        self.main.invoke_focus_composer();
    }

    // ════════════════════════════════════════════════════════════════
    //  PER-USER AUDIO
    // ════════════════════════════════════════════════════════════════

    /// The client id behind roster row `index`, if it still exists.
    pub fn user_id_at(&self, index: i32) -> Option<u64> {
        let row = self.users_model.row_data(usize::try_from(index).ok()?)?;
        if row.is_channel {
            return None;
        }
        Some(roster::join_id(row.client_id_lo, row.client_id_hi))
    }

    pub fn username_of(&self, client_id: u64) -> String {
        self.users.get(&client_id).map_or_else(
            || t!("fallback-username", "id" => client_id.to_string()),
            |u| u.username.clone(),
        )
    }

    fn open_user_popup(&mut self, index: i32) {
        let Some(id) = self.user_id_at(index) else {
            return;
        };
        if Some(id) == self.our_client_id {
            return;
        }
        let volume = self.users.get(&id).map_or(1.0, |u| u.audio.volume);
        let s = self.main.global::<State>();
        s.set_popup_user_index(index);
        s.set_popup_volume(volume);
        s.set_user_popup_open(true);
    }

    fn handle_mute_user(&mut self, client_id: u64, username: &str, muted: bool) {
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio.is_muted = muted;
            self.settings
                .peer_settings
                .insert(peer_key(username), peer_audio_to(user.audio));
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.users
                    .get(&client_id)
                    .map(|u| peer_audio_to(u.audio))
                    .unwrap_or_default(),
            ))
            .ok();
        self.settings.save();
        self.update_user_list();
    }

    fn handle_set_user_volume(&mut self, client_id: u64, username: &str, volume: f32) {
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio.volume = volume;
            self.settings
                .peer_settings
                .insert(peer_key(username), peer_audio_to(user.audio));
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.users
                    .get(&client_id)
                    .map(|u| peer_audio_to(u.audio))
                    .unwrap_or_default(),
            ))
            .ok();
        self.settings.save();
        self.update_user_list();
    }

    fn handle_reset_peer_audio(&mut self, client_id: u64, username: &str) {
        self.settings.peer_settings.remove(&peer_key(username));
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio = PeerAudio::default();
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

    fn handle_watch_peer(&mut self, client_id: u64) {
        if self.viewers.contains_key(&client_id) {
            return;
        }
        let username = self.username_of(client_id);
        // Open at the sharer's announced dimensions so a 1080p stream
        // doesn't flash open as a 720p window before the first frame.
        let (share_w, share_h) = self
            .active_sharers
            .get(&client_id)
            .copied()
            .unwrap_or((1280, 720));
        info!("Watch {username} ({client_id}): opening viewer at {share_w}x{share_h}");
        // Window creation + show() runs OUTSIDE the app borrow: on
        // macOS, ordering a window front can dispatch events
        // synchronously, and a callback re-entering the app state
        // while it is borrowed would abort the process.
        let _ = slint::invoke_from_event_loop(move || {
            with_current(move |app| {
                if app.viewers.contains_key(&client_id) {
                    return;
                }
                match Viewer::open(client_id, &username, share_w, share_h) {
                    Ok(viewer) => {
                        app.viewers.insert(client_id, viewer);
                        app.network
                            .tx
                            .send(NetworkControl::SubscribeScreen {
                                target_id: client_id,
                            })
                            .ok();
                        app.push_chat_system(&t!("chat-watching", "name" => username.as_str()));
                        info!("Viewer open + subscribed to {client_id}");
                    }
                    Err(e) => {
                        warn!("Viewer open failed: {e}");
                        app.show_error(&e.to_string());
                    }
                }
            });
        });
    }

    pub fn handle_unwatch_peer(&mut self, client_id: u64) {
        if let Some(viewer) = self.viewers.remove(&client_id) {
            viewer.close();
        }
        self.network
            .tx
            .send(NetworkControl::UnsubscribeScreen {
                target_id: client_id,
            })
            .ok();
        let username = self.username_of(client_id);
        self.push_chat_system(&t!("chat-stopped-watching", "name" => username.as_str()));
    }

    pub fn set_stream_volume(&mut self, client_id: u64, volume: f32) {
        self.screen
            .tx
            .send(ScreenControl::SetStreamVolume { client_id, volume })
            .ok();
    }

    pub fn set_stream_muted(&mut self, client_id: u64, muted: bool) {
        self.screen
            .tx
            .send(ScreenControl::MuteStream { client_id, muted })
            .ok();
    }

    // ════════════════════════════════════════════════════════════════
    //  DEVICES
    // ════════════════════════════════════════════════════════════════

    /// Called by the 2.5 s polling timer. All diff logic lives in
    /// app-core; this applies the side effects.
    pub fn handle_devices_changed(
        &mut self,
        new_input: Vec<(String, bool)>,
        new_output: Vec<(String, bool)>,
    ) {
        use app_core::devices::{self, DeviceNotice};

        let diff = devices::diff(
            &self.last_input_devices,
            &new_input,
            &self.last_output_devices,
            &new_output,
            self.settings.audio.input.device.as_deref(),
            self.settings.audio.output.device.as_deref(),
        );
        if diff.unchanged {
            return;
        }

        for notice in &diff.notices {
            let text = match notice {
                DeviceNotice::MicConnected(n) => t!("chat-mic-connected", "name" => n.as_str()),
                DeviceNotice::MicDisconnected(n) => {
                    t!("chat-mic-disconnected", "name" => n.as_str())
                }
                DeviceNotice::SpeakerConnected(n) => {
                    t!("chat-speaker-connected", "name" => n.as_str())
                }
                DeviceNotice::SpeakerDisconnected(n) => {
                    t!("chat-speaker-disconnected", "name" => n.as_str())
                }
                DeviceNotice::InputLost => t!("chat-input-lost"),
                DeviceNotice::InputDefaultChanged(name) => {
                    let fallback = t!("unknown-device");
                    t!("chat-input-default-changed",
                        "name" => name.as_deref().unwrap_or(&fallback))
                }
                DeviceNotice::OutputLost => t!("chat-output-lost"),
                DeviceNotice::OutputDefaultChanged(name) => {
                    let fallback = t!("unknown-device");
                    t!("chat-output-default-changed",
                        "name" => name.as_deref().unwrap_or(&fallback))
                }
            };
            self.push_chat_system(&text);
        }

        if diff.rebuild_input {
            self.settings.audio.input.device = None;
            self.voice.tx.send(VoiceControl::SetInputDevice(None)).ok();
        }
        if diff.rebuild_output {
            self.settings.audio.output.device = None;
            self.voice.tx.send(VoiceControl::SetOutputDevice(None)).ok();
            // WASAPI loopback captures from the default output device,
            // so when that device changes the capture stream dies too.
            // Restart it if a screen share is currently active.
            if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                self.screen.tx.send(ScreenControl::RestartSystemAudio).ok();
            }
        }

        self.last_input_devices = new_input;
        self.last_output_devices = new_output;
        self.refresh_device_warning();
    }

    /// Compare the devices saved in settings against what's connected
    /// right now and surface the result on the connect view.
    pub fn refresh_device_warning(&self) {
        let (missing_in, missing_out) = app_core::devices::missing_saved(
            self.settings.audio.input.device.as_deref(),
            self.settings.audio.output.device.as_deref(),
            &self.last_input_devices,
            &self.last_output_devices,
        );
        let mut missing: Vec<String> = Vec::new();
        if let Some(name) = missing_in {
            missing.push(t!("device-missing-mic", "name" => name));
        }
        if let Some(name) = missing_out {
            missing.push(t!("device-missing-output", "name" => name));
        }
        let text = if missing.is_empty() {
            String::new()
        } else {
            let joiner = format!(" {} ", t!("list-and"));
            format!(
                "{}\n{}",
                t!("device-warning", "devices" => missing.join(&joiner)),
                t!("device-warning-hint"),
            )
        };
        self.main.global::<State>().set_device_warning(text.into());
    }

    pub fn set_input_device(&mut self, device: Option<String>) {
        self.settings.audio.input.device.clone_from(&device);
        self.voice
            .tx
            .send(VoiceControl::SetInputDevice(device))
            .ok();
        self.refresh_device_warning();
    }

    pub fn set_output_device(&mut self, device: Option<String>) {
        self.settings.audio.output.device.clone_from(&device);
        self.voice
            .tx
            .send(VoiceControl::SetOutputDevice(device))
            .ok();
        self.refresh_device_warning();
    }

    pub fn voice_tx(&self) -> &crossbeam_channel::Sender<VoiceControl> {
        &self.voice.tx
    }

    pub fn voice_levels(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU32>,
        Arc<std::sync::atomic::AtomicU32>,
    ) {
        (
            self.voice.shared.output_level.clone(),
            self.voice.shared.denoised_level.clone(),
        )
    }

    pub fn apply_reset_defaults(&mut self) {
        self.settings.reset();
        // Push the defaults to the running subsystems so behavior
        // matches the file immediately.
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
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.update(&self.settings);
        }
        i18n::set_locale(self.settings.language.as_deref());
        self.push_tr();
        self.push_connect_fields();
        self.refresh_device_warning();
    }

    pub fn set_language(&mut self, lang: Option<String>) {
        self.settings.language = lang;
        i18n::set_locale(self.settings.language.as_deref());
        // Labels are properties — pushing new strings re-renders every
        // view live, chat history stays as-is. (The FLTK build had to
        // rebuild views for this.)
        self.push_tr();
        self.push_connect_fields();
    }

    pub fn set_keybind(&mut self, action: BindableAction, bind: String) {
        self.settings.set_bind(action, bind);
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.update(&self.settings);
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  CHAT MODEL
    // ════════════════════════════════════════════════════════════════

    /// Show the main window, arming the reveal hook so the full
    /// repaint lands on the first OS paint of the (possibly fresh)
    /// surface — see [`repaint_on_reveal`].
    pub fn show_main(&self) {
        self.main_repaint_arm.set(true);
        let _ = self.main.show();
    }

    // NOTE: there is deliberately no scroll-after-append here. The
    // ListView pins itself to the bottom reactively (chat.slint's
    // stick-to-bottom `changed` handlers), which runs AFTER layout —
    // a host-side invoke right after a model push reads a stale
    // viewport-height and lands short (or permanently unsticks once an
    // image row overshoots the at-bottom slack).

    /// Mirror a history delta into the `VecModel` (drop from front, append
    /// the converted new rows).
    fn sync_history_tail(&mut self, delta: &app_core::chat::HistoryDelta) {
        for _ in 0..delta.dropped_front {
            if self.chat_model.row_count() > 0 {
                let old = self.chat_model.row_data(0);
                self.chat_model.remove(0);
                if let Some(old) = old {
                    // Free any GIF/image state owned by the dropped row.
                    self.gifs.remove(&old.row_id);
                    self.images.remove(&old.row_id);
                }
            }
        }
        // Pre-draw the ids (next_id needs &mut history; rows() holds &).
        // Truncating u64→i32 is deliberate: ids only need uniqueness
        // within the 500-row window, and 2^31 messages per session is
        // out of scope by ~six orders of magnitude.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let ids: Vec<i32> = (0..delta.appended)
            .map(|_| self.history.next_id() as i32)
            .collect();
        let rows = self.history.rows();
        let start = rows.len() - delta.appended;
        for (item, row_id) in rows[start..].iter().zip(ids) {
            let mut row = ui::convert::chat_row(item, row_id);
            // Message rows carrying WhatsApp-style markup (*bold*,
            // _italic_, `code`) get a pre-parsed StyledText; plain
            // ones skip it and stay mouse-selectable. to_commonmark
            // escapes everything else, so from_markdown can only fail
            // on pathological input — which then renders plain.
            if row.kind == 0 {
                // Tint matches Theme.mono-text in theme.slint.
                let (cm, styled) =
                    app_core::markdown::to_commonmark(row.body.as_str(), Some("#86AA90"));
                if styled && let Ok(st) = slint::StyledText::from_markdown(&cm) {
                    row.body_styled = st;
                    row.styled = true;
                }
            }
            self.chat_model.push(row);
        }
    }

    /// Public mirror-sync for media.rs (image rows).
    pub fn sync_delta(&mut self, delta: &app_core::chat::HistoryDelta) {
        self.sync_history_tail(delta);
    }

    pub fn push_chat_system(&mut self, text: &str) {
        let delta = self.history.push_system(text);
        self.sync_history_tail(&delta);
    }

    pub fn push_chat_message(&mut self, ts: &str, user: &str, body: &str) {
        let delta = self.history.push_message(ts, user, body);
        self.sync_history_tail(&delta);
    }

    /// Media-row plumbing lives in `media.rs`; these expose the model
    /// internals it needs.
    pub fn chat_model(&self) -> &Rc<VecModel<ChatRow>> {
        &self.chat_model
    }

    pub fn history_mut(&mut self) -> &mut ChatHistory {
        &mut self.history
    }

    pub fn gifs_mut(&mut self) -> &mut HashMap<i32, GifPlayer> {
        &mut self.gifs
    }

    pub fn set_pending_preview(&mut self, png: Vec<u8>) {
        self.pending_preview = Some(png);
    }

    fn gif_hover(&mut self, row_id: i32, entering: bool) {
        let needs_ticker = self
            .gifs
            .get_mut(&row_id)
            .is_some_and(|player| player.hover(entering));
        if needs_ticker {
            self.ensure_gif_ticker();
        }
    }

    /// Stop every chat-row GIF and the in-window preview overlay,
    /// restoring row previews and releasing frame streams. Called when
    /// the main window hides to the tray (there is nothing to animate)
    /// and from the hidden-window guard in
    /// [`gif_tick`](Self::gif_tick).
    pub fn freeze_chat_gifs(&mut self) {
        let mut restores: Vec<(i32, slint::Image)> = Vec::new();
        for (&row_id, player) in &mut self.gifs {
            if let Some(first) = player.force_freeze() {
                restores.push((row_id, first));
            }
        }
        for (row_id, image) in restores {
            for i in 0..self.chat_model.row_count() {
                if let Some(mut row) = self.chat_model.row_data(i)
                    && row.row_id == row_id
                {
                    row.image = image.clone();
                    self.chat_model.set_row_data(i, row);
                    break;
                }
            }
        }
        self.preview_gif = None;
    }

    /// Wipe the local chat: history, rows, image bytes, GIF players,
    /// textures. Purely local — everyone clears their own scrollback
    /// (people were reconnecting just to get an empty chat). Also the
    /// bluntest memory-release lever the user has.
    pub fn clear_chat(&mut self) {
        self.history_mut().clear();
        self.chat_model.set_vec(Vec::new());
        self.images.clear();
        self.gifs.clear();
        self.gif_ticker.stop();
    }

    /// Start the 33 ms animation ticker if it isn't already running.
    pub fn ensure_gif_ticker(&mut self) {
        if self.gif_ticker.running() {
            return;
        }
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            self.gif_ticker.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(33),
                move || with_app(&weak, AppState::gif_tick),
            );
        }
    }

    /// One animation tick: advance every playing chat GIF and the
    /// overlay GIF; stop the ticker when nothing is playing.
    fn gif_tick(&mut self) {
        use crate::media::GifStep;
        const DT_MS: f64 = 33.0;

        // (fn defined at top of gif_tick to satisfy items-after-statements)
        fn step_overlay(g: &mut crate::viewers::GalleryGif, dt_ms: f64) -> Option<slint::Image> {
            g.in_frame_ms += dt_ms;
            let mut advanced = false;
            for _ in 0..g.anim.frames.len() {
                let delay = f64::from(g.anim.frames[g.idx].delay_ms);
                if g.in_frame_ms < delay {
                    break;
                }
                g.in_frame_ms -= delay;
                g.idx = (g.idx + 1) % g.anim.frames.len();
                advanced = true;
            }
            advanced.then(|| {
                let frame = &g.anim.frames[g.idx];
                ui::convert::rgba_image(&frame.rgba, g.anim.width, g.anim.height)
            })
        }

        // Tray/minimized: chat rows and the in-window preview are
        // invisible — stop paying decode for them. The gallery viewer
        // is its own window and may still be on screen, so it keeps
        // animating; the ticker dies below once nothing plays.
        if !self.main.window().is_visible() {
            self.freeze_chat_gifs();
        }

        let mut any_playing = false;
        let mut blits: Vec<(i32, slint::Image)> = Vec::new();
        for (&row_id, player) in &mut self.gifs {
            match player.step(DT_MS) {
                GifStep::Idle => {}
                GifStep::Blit {
                    rgba,
                    width,
                    height,
                } => blits.push((row_id, ui::convert::rgba_image(&rgba, width, height))),
                GifStep::Freeze { first } => blits.push((row_id, first)),
            }
            any_playing |= player.playing();
        }
        for (row_id, image) in blits {
            for i in 0..self.chat_model.row_count() {
                if let Some(mut row) = self.chat_model.row_data(i)
                    && row.row_id == row_id
                {
                    row.image = image.clone();
                    self.chat_model.set_row_data(i, row);
                    break;
                }
            }
        }

        if let Some(g) = &mut self.gallery_gif {
            any_playing = true;
            if let Some(img) = step_overlay(g, DT_MS) {
                self.main.global::<State>().set_viewer_image(img);
            }
        }
        if let Some(g) = &mut self.preview_gif {
            any_playing = true;
            if let Some(img) = step_overlay(g, DT_MS) {
                self.main.global::<State>().set_preview_image(img);
            }
        }

        if !any_playing {
            self.gif_ticker.stop();
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  ROSTER / STATUS
    // ════════════════════════════════════════════════════════════════

    fn update_user_list(&mut self) {
        let items = roster::grouped_rows(
            &self.channels,
            &self.users,
            self.our_client_id,
            &self.sharer_ids,
            self.is_muted,
            self.is_deafened,
        );
        // In-place update: write rows positionally, growing/shrinking
        // only when membership changed — preserves the ListView's
        // scroll offset exactly like the FLTK incremental path.
        let converted: Vec<UserRow> = items.iter().map(ui::convert::roster_item).collect();
        let current = self.users_model.row_count();
        for (i, row) in converted.iter().enumerate() {
            if i < current {
                self.users_model.set_row_data(i, row.clone());
            } else {
                self.users_model.push(row.clone());
            }
        }
        for i in (converted.len()..current).rev() {
            self.users_model.remove(i);
        }
        self.main
            .global::<State>()
            .set_users_count_label(t!("users-count", "count" => self.users.len()).into());
    }

    fn update_latency(
        &self,
        rtt_ms: f64,
        loss_pct: f64,
        audio_latency_ms: Option<u32>,
        kbps_up: f64,
        kbps_down: f64,
    ) {
        use app_core::stats::{latency_grade, net_str, LatencyGrade};
        let s = self.main.global::<State>();
        s.set_status_text(format!("●  {}", t!("status-connected")).into());
        s.set_status_color(match latency_grade(rtt_ms) {
            LatencyGrade::Good => slint::Color::from_rgb_u8(0x2B, 0xAC, 0x76),
            LatencyGrade::Fair => slint::Color::from_rgb_u8(0xEC, 0xB2, 0x2E),
            LatencyGrade::Bad => slint::Color::from_rgb_u8(0xE0, 0x1E, 0x5A),
        });
        let audio_str = match audio_latency_ms {
            Some(ms) => format!("{ms}ms"),
            None => "--".to_string(),
        };
        s.set_latency_line(
            t!("latency-line",
                "audio" => audio_str,
                "network" => net_str(rtt_ms, loss_pct),
                "up" => format!("{:.1}", kbps_up / 1000.0),
                "down" => format!("{:.1}", kbps_down / 1000.0))
            .into(),
        );
    }

    #[allow(clippy::cast_precision_loss)]
    fn update_stream_stats(&mut self) {
        if !self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            return;
        }
        let sh = &self.screen.shared;
        let frame_bytes = sh.last_frame_bytes.load(Ordering::Relaxed);
        let encode_us = sh.last_encode_us.load(Ordering::Relaxed);
        let viewers = sh.stream_viewers.load(Ordering::Relaxed);
        let fps_scale_pct = sh.fps_scale_pct.load(Ordering::Relaxed);
        let loss_pct = f32::from_bits(sh.stream_loss_pct.load(Ordering::Relaxed));
        let quality_str = sh
            .current_quality
            .lock()
            .ok()
            .and_then(|q| *q)
            .map_or_else(|| t!("unknown"), |q| q.to_string());
        if !self.main_on_screen() {
            return;
        }
        let stats = app_core::stats::StreamStats {
            quality: quality_str,
            avg_frame_kb: frame_bytes as f32 / 1024.0,
            encode_ms: encode_us as f32 / 1000.0,
            viewers,
            fps_scale_pct,
            loss_pct,
        };
        let line1 = t!("stream-stats-quality",
            "quality" => stats.quality.as_str(), "viewers" => stats.viewers);
        let mut line2 = t!("stream-stats-frame",
            "kb" => format!("{:.0}", stats.avg_frame_kb),
            "ms" => format!("{:.1}", stats.encode_ms));
        if stats.degraded() {
            line2.push_str(" · ");
            line2.push_str(&t!("stream-stats-degraded",
                "fps" => stats.fps_scale_pct,
                "loss" => format!("{:.1}", stats.loss_pct)));
        }
        let s = self.main.global::<State>();
        s.set_stream_stats(format!("{line1}\n{line2}").into());
        s.set_stream_degraded(stats.degraded());
    }

    /// Whether the main screen is actually being rendered: false on the
    /// connect view, hidden to the tray, or minimized. Stats updates
    /// and resource sampling are skipped while this is false.
    fn main_on_screen(&self) -> bool {
        self.main.global::<State>().get_phase() == Phase::Main
            && self.main.window().is_visible()
            && !self.main.window().is_minimized()
    }

    #[allow(clippy::cast_precision_loss)]
    fn sample_resources(&mut self) {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};

        // Sample only when someone can actually see the result. The
        // check comes first so a hidden/minimized app pays nothing —
        // not even the process-table refresh.
        let Some(pid) = self.pid else { return };
        if !self.main_on_screen() {
            return;
        }
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        if let Some(proc) = self.sys.process(pid) {
            // cpu_usage() is % of one core; normalize to % of the whole
            // machine (task-manager style).
            let cores =
                std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f32;
            let cpu_pct = proc.cpu_usage() / cores;
            // Memory: the platform-native figure (what the user's own
            // task monitor shows), with sysinfo's RSS as the fallback.
            let mem_bytes =
                crate::resources::native_monitor_memory().unwrap_or_else(|| proc.memory());
            let mem_mb = mem_bytes as f64 / (1024.0 * 1024.0);
            self.main.global::<State>().set_resource_line(
                t!("resource-line",
                    "cpu" => format!("{cpu_pct:.1}"),
                    "mem" => format!("{mem_mb:.0}"))
                .into(),
            );
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  MISC
    // ════════════════════════════════════════════════════════════════

    /// Persist geometry + settings immediately (tray Quit path — the
    /// normal shutdown also does this, but only after the loop exits).
    #[cfg_attr(not(feature = "tray"), allow(dead_code))]
    pub fn save_settings_now(&mut self) {
        self.save_window_geometry();
        self.settings.save();
    }

    /// Keep a timer alive for the app's lifetime.
    pub fn hold_timer(&mut self, timer: slint::Timer) {
        self.timers.push(timer);
    }

    pub fn show_error(&mut self, error: &str) {
        self.main.global::<State>().set_error_text(error.into());
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            self.error_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_secs(5),
                move || {
                    with_app(&weak, |app| {
                        app.main.global::<State>().set_error_text("".into());
                    });
                },
            );
        }
    }

    /// Update-flow state transitions (called from `updater.rs`).
    pub fn set_update_available(&mut self, remote_version: String) {
        let s = self.main.global::<State>();
        s.set_update_heading(t!("update-available-heading").into());
        s.set_update_body(
            t!("update-version-info",
                "current" => CURRENT_VERSION,
                "latest" => remote_version.as_str())
            .into(),
        );
        s.set_phase(Phase::UpdatePrompt);
        self.update_version = Some(remote_version);
    }

    pub fn update_version(&self) -> Option<&str> {
        self.update_version.as_deref()
    }

    pub fn download_cancel_flag(&self) -> Arc<AtomicBool> {
        self.download_cancelled.clone()
    }

    pub fn set_download_done(&mut self, path: String) {
        let s = self.main.global::<State>();
        s.set_update_heading(t!("update-downloaded-heading").into());
        s.set_update_body(t!("update-ready-info").into());
        s.set_update_progress(1.0);
        s.set_update_progress_text("".into());
        s.set_phase(Phase::DownloadDone);
        self.update_zip_path = Some(path);
    }

    pub fn set_download_failed(&mut self, error: &str) {
        let s = self.main.global::<State>();
        s.set_update_heading(t!("update-failed-heading").into());
        s.set_update_body(error.into());
        s.set_phase(Phase::DownloadFailed);
    }

    // Window px values fit comfortably in every one of these casts.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn save_window_geometry(&mut self) {
        let size = self.main.window().size();
        let pos = self.main.window().position();
        let scale = self.main.window().scale_factor();
        self.settings.window.width = (size.width as f32 / scale) as i32;
        self.settings.window.height = (size.height as f32 / scale) as i32;
        self.settings.window.x = Some(pos.x);
        self.settings.window.y = Some(pos.y);
    }

    /// Push every localized static label into the Tr global (called at
    /// startup and on language change — this is live language
    /// switching, no rebuilds).
    #[allow(clippy::too_many_lines)]
    pub fn push_tr(&self) {
        fill_tr(&self.main.global::<Tr>());
        if let Some(win) = &self.settings_win {
            fill_tr(&win.global::<Tr>());
        }
    }

    fn push_connect_fields(&self) {
        let s = self.main.global::<State>();
        s.set_nickname(self.settings.user.nickname.as_str().into());
        s.set_password("".into());
        s.set_password_prompt(false);
        self.push_connect_servers();
    }

    /// Refresh the connect view's server dropdown (names + which entry
    /// is selected). Called whenever the Servers tab mutates the list.
    pub fn push_connect_servers(&self) {
        let s = self.main.global::<State>();
        let names: Vec<slint::SharedString> = self
            .settings
            .servers
            .iter()
            .map(|e| e.name.as_str().into())
            .collect();
        let default_pos = self
            .settings
            .servers
            .iter()
            .position(|e| e.name == self.settings.server)
            .unwrap_or(0);
        s.set_server_names(ModelRc::new(VecModel::from(names)));
        s.set_server_index(i32::try_from(default_pos).unwrap_or(0));
    }

    /// Clean shutdown: signal threads, join with timeout, save settings.
    ///
    /// Order matters:
    /// 1. Save settings (while the window still exists for geometry)
    /// 2. Stop voice engine (drops audio devices, closes threads)
    /// 3. Disconnect network (sends goodbye to server)
    /// 4. Signal all worker threads to exit
    /// 5. Join with hard timeout — never hang forever
    fn shutdown(&mut self) {
        info!("Shutting down");

        self.save_window_geometry();
        self.settings.save();

        self.voice.tx.send(VoiceControl::Stop).ok();

        // No delay needed: the control channel is FIFO, so the network
        // thread flushes the disconnect packet before it ever sees the
        // Shutdown queued below — and step 5 joins the thread regardless.
        self.network.tx.send(NetworkControl::Disconnect).ok();

        self.voice.tx.send(VoiceControl::Shutdown).ok();
        self.network.tx.send(NetworkControl::Shutdown).ok();
        self.screen.tx.send(ScreenControl::Shutdown).ok();

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

        match done_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => info!("Shutdown complete"),
            Err(_) => warn!("Shutdown timed out — threads still running, exiting anyway"),
        }
    }
}

/// `audio::PeerAudioSettings` ↔ app-core's `PeerAudio` mirror.
fn peer_audio_from(a: &PeerAudioSettings) -> PeerAudio {
    PeerAudio {
        volume: a.volume,
        is_muted: a.is_muted,
    }
}

fn peer_audio_to(a: PeerAudio) -> PeerAudioSettings {
    PeerAudioSettings {
        volume: a.volume,
        is_muted: a.is_muted,
    }
}

/// Fill a `Tr` global instance from fluent. Each window component tree
/// carries its own global storage, so this runs per window.
#[allow(clippy::too_many_lines)]
pub fn fill_tr(tr: &Tr) {
    tr.set_app_tagline(t!("app-tagline").into());
    tr.set_nickname(t!("nickname").into());
    tr.set_server_password(t!("server-password").into());
    tr.set_connect(t!("connect").into());
    tr.set_settings(t!("settings").into());
    tr.set_quit(t!("quit").into());
    tr.set_mute(t!("mute").into());
    tr.set_unmute(t!("unmute").into());
    tr.set_deafen(t!("deafen").into());
    tr.set_undeafen(t!("undeafen").into());
    tr.set_share_screen(t!("share-screen").into());
    tr.set_channel_password_title(t!("channel-password-title").into());
    tr.set_share_visibility(t!("share-visibility").into());
    tr.set_share_channel_only(t!("share-channel-only").into());
    tr.set_share_everyone(t!("share-everyone").into());
    tr.set_stop_sharing(t!("stop-sharing").into());
    tr.set_disconnect(t!("disconnect").into());
    tr.set_live(t!("live").into());
    tr.set_user_volume(t!("user-volume").into());
    tr.set_watch_stream(t!("watch-stream").into());
    tr.set_reset_audio(t!("reset-audio").into());
    tr.set_send(t!("send").into());
    tr.set_cancel(t!("cancel").into());
    tr.set_chat_emoji_tooltip(t!("chat-emoji-tooltip").into());
    tr.set_chat_attach_tooltip(t!("chat-attach-tooltip").into());
    tr.set_chat_clear_tooltip(t!("chat-clear-tooltip").into());
    tr.set_image_preview_title(t!("image-preview-title").into());
    tr.set_menu_cut(t!("menu-cut").into());
    tr.set_menu_copy(t!("menu-copy").into());
    tr.set_menu_paste(t!("menu-paste").into());
    tr.set_menu_copy_message(t!("menu-copy-message").into());
    tr.set_menu_copy_link(t!("menu-copy-link").into());
    tr.set_menu_open_link(t!("menu-open-link").into());
    tr.set_menu_save_image(t!("menu-save-image").into());
    tr.set_menu_open_image(t!("menu-open-image").into());
    tr.set_monitor(t!("monitor").into());
    tr.set_quality(t!("quality").into());
    tr.set_start_sharing(t!("start-sharing").into());
    tr.set_stop_watching(t!("stop-watching").into());
    tr.set_settings_title(t!("settings-title").into());
    tr.set_tab_general(t!("tab-general").into());
    tr.set_tab_audio(t!("tab-audio").into());
    tr.set_tab_keyboard(t!("tab-keyboard").into());
    tr.set_tab_servers(t!("tab-servers").into());
    tr.set_tab_credits(t!("tab-credits").into());
    tr.set_tab_about(t!("tab-about").into());
    tr.set_about_open_config(t!("about-open-config").into());
    tr.set_save_and_close(t!("save-and-close").into());
    tr.set_reset_defaults(t!("reset-defaults").into());
    tr.set_language(t!("language").into());
    tr.set_language_note(t!("language-note").into());
    tr.set_settings_server(t!("settings-server").into());
    tr.set_server_name(t!("server-name").into());
    tr.set_server_ip(t!("server-ip").into());
    tr.set_server_port(t!("server-port").into());
    tr.set_server_default(t!("server-default").into());
    tr.set_server_add(t!("server-add").into());
    tr.set_server_remove(t!("server-remove").into());
    tr.set_server_manage(t!("server-manage").into());
    tr.set_save(t!("save").into());
    tr.set_connect_access_denied(t!("connect-access-denied").into());
    tr.set_server_fingerprint(t!("server-fingerprint").into());
    tr.set_server_config_note(t!("server-config-note").into());
    tr.set_audio_devices_header(t!("audio-devices-header").into());
    tr.set_audio_input_device(t!("audio-input-device").into());
    tr.set_audio_output_device(t!("audio-output-device").into());
    tr.set_audio_system_default(t!("audio-system-default").into());
    tr.set_audio_input_header(t!("audio-input-header").into());
    tr.set_audio_capture_level(t!("audio-capture-level").into());
    tr.set_audio_input_gain(t!("audio-input-gain").into());
    tr.set_audio_input_sensitivity(t!("audio-input-sensitivity").into());
    tr.set_audio_gate_hint(t!("audio-gate-hint").into());
    tr.set_audio_test_mic(t!("audio-test-mic").into());
    tr.set_audio_stop_testing(t!("audio-stop-testing").into());
    tr.set_audio_output_header(t!("audio-output-header").into());
    tr.set_audio_master_volume(t!("audio-master-volume").into());
    tr.set_keybinds_header(t!("keybinds-header").into());
    tr.set_keybinds_hint(t!("keybinds-hint").into());
    tr.set_keybind_record(t!("keybind-record").into());
    tr.set_keybind_press_combo(t!("keybind-press-combo").into());
    tr.set_keybind_not_set(t!("keybind-not-set").into());
    tr.set_update_now(t!("update-now").into());
    tr.set_update_skip(t!("update-skip").into());
    tr.set_update_not_now(t!("update-not-now").into());
    tr.set_update_continue(t!("update-continue").into());
    tr.set_update_install_restart(t!("update-install-restart").into());
    tr.set_splash_checking(t!("splash-checking").into());
}

#[cfg(feature = "tray")]
pub mod tray {
    //! System tray via Slint's builtin `SystemTrayIcon` (the `tray`
    //! cargo feature adds `slint/system-tray`; native backends on all
    //! three desktop platforms). With this feature on, closing the
    //! main window hides it and the tray keeps the app reachable:
    //! left-clicking the icon or its Show entry reopens the window,
    //! Quit exits. Everything runs in-process on the Slint event loop
    //! — no cross-thread event hop like the old `tray-icon` crate,
    //! and a visible tray icon by itself keeps
    //! `run_event_loop_until_quit` alive.
    use i18n::t;
    use log::warn;

    use super::{with_current, AppState};

    thread_local! {
        /// Keeps the OS tray item alive for the app's lifetime —
        /// dropping the component removes the icon.
        static TRAY: std::cell::RefCell<Option<ui::BaucordTray>> =
            const { std::cell::RefCell::new(None) };
    }

    pub fn setup(_app: &mut AppState) {
        let tray = match ui::BaucordTray::new() {
            Ok(tray) => tray,
            Err(e) => {
                warn!("Tray icon unavailable: {e}");
                return;
            }
        };
        tray.set_show_label(t!("tray-show").into());
        tray.set_quit_label(t!("tray-quit").into());
        tray.on_show_app(|| {
            with_current(|app| app.show_main());
        });
        tray.on_quit_app(|| {
            with_current(AppState::save_settings_now);
            let _ = slint::quit_event_loop();
        });
        TRAY.with(|slot| *slot.borrow_mut() = Some(tray));
    }
}
