// app.rs — Main application coordinator (Slint edition).
//
// Owns the Slint event loop and delegates to:
//   - NetworkThreadHandle (client::network) for renet client + protocol
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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use log::{info, warn};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use client::{
    config::{BindableAction, Settings, peer_key},
    network::{NetworkControl, NetworkMessage, NetworkThreadHandle},
    screen::{ScreenControl, ScreenEvent, ScreenThreadHandle},
    voice::{VoiceControl, VoiceEvent, VoiceThreadHandle},
};

use app_core::chat::ChatHistory;
use app_core::roster::{self, PeerAudio, UserState};
use audio::{PeerAudioSettings, SoundType};
use i18n::t;
use ui::{ChatRow, Logic, MainWindow, Phase, State, Tr, UserRow};

use crate::hotkeys::HotkeyManager;
use crate::media::{self, GifPlayer};
use crate::viewers::Viewer;

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

    // Domain state
    users: HashMap<u64, UserState>,
    our_client_id: Option<u64>,
    connected: bool,
    pub settings: Settings,
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
    frames_coalesced: u64,

    // Own-process resource sampling (sidebar stats line)
    sys: sysinfo::System,
    /// `None` if the OS refused to identify our own process.
    pid: Option<sysinfo::Pid>,

    hotkeys: Option<HotkeyManager>,
    pub keybind_recording: Option<usize>,

    // Timers — held so they keep firing.
    timers: Vec<slint::Timer>,
    error_timer: slint::Timer,
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
fn spawn_workers(
    is_testing_mic_shared: &Arc<AtomicBool>,
) -> (VoiceThreadHandle, ScreenThreadHandle, NetworkThreadHandle) {
    let wake_fn: client::voice::WakeFn = Arc::new(wake);
    let (network_tx, network_control_rx) = crossbeam_channel::bounded::<NetworkControl>(256);
    let (network_msg_tx, network_msg_rx) = crossbeam_channel::bounded::<NetworkMessage>(256);

    let voice = VoiceThreadHandle::spawn(
        network_tx.clone(),
        is_testing_mic_shared.clone(),
        wake_fn.clone(),
    );
    let screen = ScreenThreadHandle::spawn(
        network_tx.clone(),
        wake_fn.clone(),
        voice.shared.output_level.clone(),
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
    (voice, screen, network)
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
    let (voice, screen, network) = spawn_workers(&is_testing_mic_shared);

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
        .map(|(_, glyph, section)| ui::EmojiRow {
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
        connected: false,
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
        frames_coalesced: 0,
        sys: sysinfo::System::new(),
        pid: sysinfo::get_current_pid().ok(),
        hotkeys: None,
        keybind_recording: None,
        timers: Vec::new(),
        error_timer: slint::Timer::default(),
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

    state.borrow().main.show().expect("failed to show the main window");

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
            let s = app.main.global::<State>();
            let nickname = s.get_nickname().to_string();
            let password = s.get_password().to_string();
            if nickname.trim().is_empty() {
                app.show_error(&t!("error-nickname-empty"));
            } else {
                app.handle_connect(&nickname, &password);
            }
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
                let muted = app
                    .users
                    .get(&id)
                    .is_some_and(|u| u.audio.is_muted);
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
    logic.on_start_share(move |monitor_index, quality_index| {
        with_app(&w, |app| {
            let quality = usize::try_from(quality_index)
                .ok()
                .and_then(|i| video::Quality::ALL.get(i))
                .copied()
                .unwrap_or(video::Quality::FHD30);
            app.screen
                .tx
                .send(ScreenControl::StartSharing {
                    quality,
                    monitor_index: usize::try_from(monitor_index).unwrap_or(0),
                })
                .ok();
        });
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
                && let Err(e) = crate::updater::extract_and_install(&path) {
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

        let mut latest_frames: HashMap<u64, ScreenEvent> = HashMap::new();
        for _ in 0..64 {
            match st.screen.event_rx.try_recv() {
                Ok(evt @ ScreenEvent::FrameReady { .. }) => {
                    if let ScreenEvent::FrameReady { client_id, .. } = &evt
                        && latest_frames.insert(*client_id, evt).is_some() {
                            st.frames_coalesced += 1;
                            if st.frames_coalesced.is_multiple_of(60) {
                                warn!(
                                    "📊 UI coalescing has skipped {} frames — main thread slower than stream",
                                    st.frames_coalesced
                                );
                            }
                        }
                }
                Ok(evt) => st.handle_screen_event(evt),
                Err(_) => break,
            }
        }
        for (_, evt) in latest_frames.drain() {
            st.handle_screen_event(evt);
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
    fn handle_screen_event(&mut self, evt: ScreenEvent) {
        match evt {
            ScreenEvent::SharingStarted { quality } => {
                info!("📺 Screen share started: {quality}");
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
                                        app.main
                                            .global::<State>()
                                            .set_stream_stats("".into());
                                    });
                                }
                            },
                        );
                        self.timers.push(t);
                    }
                }
            }
            ScreenEvent::SharingStopped => {
                info!("📺 Screen share stopped");
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
                #[allow(clippy::cast_precision_loss)] // KB for a log line
                let frame_kb = frame_bytes as f64 / 1024.0;
                log::debug!(
                    "📊 [seq={seq} {frame_type} {frame_kb:.0}KB] net→decode: {net_to_screen_ms:.1}ms | \
        decode→app: {screen_to_app_ms:.1}ms | total: {total_ms:.1}ms | \
        buf: {buffer_depth_ms:.0}/{buffer_target_ms:.0}ms jit: {buffer_jitter_ms:.0}ms",
                );
                if total_ms > 25.0 {
                    warn!(
                        "📊 SLOW FRAME seq={seq}: {total_ms:.1}ms total \
            (net→decode: {net_to_screen_ms:.1}ms, decode→app: {screen_to_app_ms:.1}ms)"
                    );
                }
                if let Some(viewer) = self.viewers.get_mut(&client_id) {
                    let blit_start = Instant::now();
                    viewer.update_frame(
                        &rgba,
                        width,
                        height,
                        frame_bytes,
                        buffer_depth_ms,
                        buffer_target_ms,
                        buffer_jitter_ms,
                    );
                    let blit_ms = blit_start.elapsed().as_secs_f32() * 1000.0;
                    if blit_ms > 5.0 {
                        warn!("📊 Slow blit: {blit_ms:.1}ms for seq {seq}");
                    }
                }
            }
            ScreenEvent::PeerStartedSharing {
                client_id,
                width,
                height,
            } => {
                let username = self.username_of(client_id);
                info!("📺 {username} started sharing ({width}x{height})");
                self.push_chat_system(&t!("chat-peer-sharing", "name" => username.as_str()));
            }
            ScreenEvent::PeerStoppedSharing { client_id } => {
                let username = self.username_of(client_id);
                info!("📺 {username} stopped sharing");
                self.push_chat_system(&t!("chat-peer-stopped-sharing", "name" => username.as_str()));
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
                self.main.global::<State>().set_error_text("".into());
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
            } => {
                self.handle_init_client(client_id, &users, &server_name, &greeting);
            }
            NetworkMessage::UserJoined {
                client_id,
                username,
            } => self.handle_user_joined(client_id, username),
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
                    s.set_upload_line(
                        t!("chat-upload-progress", "pct" => u32::from(pct)).into(),
                    );
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
                self.push_chat_system(&format!(
                    "Can't watch {username}: {}",
                    reason.describe()
                ));
            }
            NetworkMessage::ScreenShareStarted {
                client_id,
                width,
                height,
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
            | NetworkMessage::KeyframeRequested { .. } => {}
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  CONNECTION FLOW
    // ════════════════════════════════════════════════════════════════

    fn handle_connect(&mut self, nickname: &str, password: &str) {
        self.settings.user.nickname = nickname.to_string();
        let password = password.trim();
        self.settings.server.token_password =
            (!password.is_empty()).then(|| password.to_string());
        self.settings.save();

        let Ok(addr) = self.settings.server.address.trim().parse::<SocketAddr>() else {
            self.show_error(&t!("error-invalid-address"));
            return;
        };

        self.network
            .tx
            .send(NetworkControl::Connect {
                addr,
                nickname: nickname.to_string(),
                token_url: self.settings.server.token_url.clone(),
                token_password: self.settings.server.token_password.clone(),
            })
            .ok();
    }

    fn handle_init_client(
        &mut self,
        client_id: u64,
        users: &HashMap<u64, String>,
        server_name: &str,
        greeting: &str,
    ) {
        // Fresh chat per connection — same semantics as the old
        // rebuild-the-main-view path.
        self.history.clear();
        self.chat_model.set_vec(Vec::new());
        self.images.clear();
        self.gifs.clear();

        self.main.global::<State>().set_phase(Phase::Main);

        self.our_client_id = Some(client_id);
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
        let _ = self.main.show();
    }

    fn handle_user_joined(&mut self, client_id: u64, username: String) {
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
        self.users
            .insert(client_id, UserState::new(username, peer_audio_from(&saved)));
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserJoined))
            .ok();
        self.update_user_list();
    }

    fn handle_user_left(&mut self, client_id: u64, username: &str) {
        self.voice.tx.send(VoiceControl::RemovePeer(client_id)).ok();
        self.users.remove(&client_id);
        self.active_sharers.remove(&client_id);
        self.sharer_ids = self.active_sharers.keys().copied().collect();

        // Close viewer if we were watching this user
        if let Some(viewer) = self.viewers.remove(&client_id) {
            viewer.close();
        }

        self.push_chat_system(&t!("chat-user-left", "name" => username));
        self.voice
            .tx
            .send(VoiceControl::PlaySound(SoundType::UserLeft))
            .ok();
        self.update_user_list();
    }

    /// Shared teardown for both user-initiated and server-initiated
    /// disconnects.
    fn teardown_connection(&mut self) {
        self.connected = false;
        self.is_testing_mic = false;
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
        self.network.tx.send(NetworkControl::Disconnect).ok();
        self.push_chat_system(
            &t!("chat-disconnected", "reason" => t!("disconnect-reason-user")),
        );
        self.teardown_connection();
        self.show_connect_phase();
    }

    pub fn show_connect_phase(&mut self) {
        self.refresh_device_warning();
        self.main.global::<State>().set_phase(Phase::Connect);
        let _ = self.main.show();
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
        } else {
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
            let primary_idx = monitors.iter().position(|m| m.is_primary).unwrap_or(0);
            let default_q = video::Quality::ALL
                .iter()
                .position(|q| *q == video::Quality::FHD30)
                .unwrap_or(0);
            let s = self.main.global::<State>();
            s.set_monitors(ModelRc::new(VecModel::from(labels)));
            s.set_qualities(ModelRc::new(VecModel::from(qualities)));
            s.set_default_monitor(i32::try_from(primary_idx).unwrap_or(0));
            s.set_default_quality(i32::try_from(default_q).unwrap_or(0));
            self.main.invoke_open_share_dialog();
        }
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
        let rendered = app_core::emoji::replace_shortcodes(
            &app_core::chat::scrub_control_chars(raw.trim()),
        );
        let text: String = rendered
            .chars()
            .take(app_core::chat::MAX_MESSAGE_CHARS)
            .collect();
        if text.is_empty() {
            return;
        }
        self.network
            .tx
            .send(NetworkControl::SendMessage(text))
            .ok();
        s.set_draft("".into());
        self.main.invoke_focus_composer();
    }

    // ════════════════════════════════════════════════════════════════
    //  PER-USER AUDIO
    // ════════════════════════════════════════════════════════════════

    /// The client id behind roster row `index`, if it still exists.
    pub fn user_id_at(&self, index: i32) -> Option<u64> {
        let row = self.users_model.row_data(usize::try_from(index).ok()?)?;
        Some(roster::join_id(row.client_id_lo, row.client_id_hi))
    }

    pub fn username_of(&self, client_id: u64) -> String {
        self.users
            .get(&client_id).map_or_else(|| t!("fallback-username", "id" => client_id.to_string()), |u| u.username.clone())
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
        info!("📺 Watch {username} ({client_id}): opening viewer at {share_w}x{share_h}");
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
                        app.push_chat_system(
                            &t!("chat-watching", "name" => username.as_str()),
                        );
                        info!("📺 Viewer open + subscribed to {client_id}");
                    }
                    Err(e) => {
                        warn!("📺 Viewer open failed: {e}");
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
                self.screen
                    .tx
                    .send(ScreenControl::RestartSystemAudio)
                    .ok();
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
        self.voice.tx.send(VoiceControl::SetInputDevice(device)).ok();
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

    pub fn voice_levels(&self) -> (Arc<std::sync::atomic::AtomicU32>, Arc<std::sync::atomic::AtomicU32>) {
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

        let mut any_playing = false;
        let mut blits: Vec<(i32, Rc<app_core::gif::GifAnimation>, usize, bool)> = Vec::new();
        for (&row_id, player) in &mut self.gifs {
            match player.step(DT_MS) {
                GifStep::Idle => {}
                GifStep::Blit { anim, idx } => blits.push((row_id, anim, idx, false)),
                GifStep::Freeze { anim } => blits.push((row_id, anim, 0, true)),
            }
            any_playing |= player.playing();
        }
        for (row_id, anim, idx, _) in &blits {
            let frame = &anim.frames[*idx];
            for i in 0..self.chat_model.row_count() {
                if let Some(mut row) = self.chat_model.row_data(i)
                    && row.row_id == *row_id {
                        row.image =
                            ui::convert::rgba_image(&frame.rgba, anim.width, anim.height);
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
        let rows = roster::rows(
            &self.users,
            self.our_client_id,
            &self.sharer_ids,
            self.is_muted,
            self.is_deafened,
        );
        // In-place update: write rows positionally, growing/shrinking
        // only when membership changed — preserves the ListView's
        // scroll offset exactly like the FLTK incremental path.
        let converted: Vec<UserRow> = rows.iter().map(ui::convert::user_row).collect();
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
        use app_core::stats::{LatencyGrade, latency_grade, net_str};
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
            let cores = std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get) as f32;
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
        self.main
            .global::<State>()
            .set_error_text(error.into());
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
        s.set_password(
            self.settings
                .server
                .token_password
                .as_deref()
                .unwrap_or("")
                .into(),
        );
        s.set_server_note(
            t!("connect-server-note", "server" => self.settings.server.address.as_str()).into(),
        );
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
    tr.set_tab_misc(t!("tab-misc").into());
    tr.set_tab_audio(t!("tab-audio").into());
    tr.set_tab_keyboard(t!("tab-keyboard").into());
    tr.set_tab_credits(t!("tab-credits").into());
    tr.set_tab_about(t!("tab-about").into());
    tr.set_about_open_config(t!("about-open-config").into());
    tr.set_save_and_close(t!("save-and-close").into());
    tr.set_reset_defaults(t!("reset-defaults").into());
    tr.set_language(t!("language").into());
    tr.set_language_note(t!("language-note").into());
    tr.set_settings_server(t!("settings-server").into());
    tr.set_server_address(t!("server-address").into());
    tr.set_server_token_url(t!("server-token-url").into());
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
    use log::warn;
    use slint::ComponentHandle;

    use super::{with_current, AppState};
    use i18n::t;

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
            with_current(|app| {
                let _ = app.main.show();
            });
        });
        tray.on_quit_app(|| {
            with_current(AppState::save_settings_now);
            let _ = slint::quit_event_loop();
        });
        TRAY.with(|slot| *slot.borrow_mut() = Some(tray));
    }
}
