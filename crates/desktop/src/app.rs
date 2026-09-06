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
    chat::{ChatHistory, Meta},
    roster::{self, ChannelMeta, PeerAudio, Role, UserState},
};
use audio::{PeerAudioSettings, SoundType};
use client::{
    config::{peer_key, BindableAction, Settings},
    network::{DisconnectKind, LossKind, NetworkControl, NetworkMessage, NetworkThreadHandle},
    screen::{ScreenControl, ScreenEvent, ScreenThreadHandle},
    voice::{VoiceControl, VoiceEvent, VoiceThreadHandle},
    AdminCommand, AdminTarget,
};
use i18n::t;
use log::{info, warn};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use ui::{
    ChannelOption, ChatRow, DmRow, Logic, MainWindow, ModAction, Phase, State, ToastKind, Tr,
    UserRow,
};

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
    pub server_name: String,
    /// Voice channels this server defines (from Init), in id order —
    /// derived from `channel_infos` + `channel_details` + our rank by
    /// `rebuild_channels`.
    channels: Vec<ChannelMeta>,
    /// The wire list as last received (`Init` / `ChannelListUpdated`).
    channel_infos: Vec<client::ChannelInfo>,
    /// Channel a JoinChannel is in flight for (the password prompt's
    /// target; also lets a WrongPassword reply reopen the prompt).
    pending_channel_join: Option<u16>,
    pub connected: bool,
    pub settings: Settings,
    /// Our Ed25519 identity key (next to settings.json): the login
    /// (spec §3). `None` if it could not be loaded — then there is no
    /// connecting until it can.
    pub client_key: Option<Arc<transport::ClientKey>>,
    /// What the server said we may do this session (`Session`,
    /// spec §10); the UI renders what this allows.
    pub permissions: u64,
    /// Who we are on this server (`Session.user_id`): history rows
    /// authored by this id are ours to edit and delete.
    our_user_id: Option<client::UserId>,
    /// `Session.persistent`: `false` earns a standing notice in chat,
    /// pushed after `Init` (which resets the chat) rather than on
    /// arrival.
    server_persistent: bool,
    /// Who each session is (`UserDetails`): user id and roles, from
    /// the server's tables. Slash commands resolve names through it.
    details: HashMap<u64, (client::UserId, Vec<client::RoleId>)>,
    /// Admin requests in flight: what to do with the answer.
    pending_admin: HashMap<u32, PendingAdmin>,
    next_request_id: u32,
    /// The last telemetry fetched per session for the roster tooltip
    /// (spec §8), and when, so a hover does not ask twice in ten
    /// seconds.
    telemetry_notes: HashMap<u64, TelemetryNote>,
    /// The moderation dialog's subject while it is open.
    moderation: Option<Moderation>,
    /// The "Move to" submenu's model, rebuilt with the channel list.
    channel_options: Rc<VecModel<ChannelOption>>,
    /// A server voice mute stands (spec §4.2): the mute button is
    /// locked, and `Some(was_muted)` remembers what to restore.
    server_muted: Option<bool>,
    /// A server text mute stands: the composer refuses to send.
    text_muted: Option<Option<u64>>,
    /// The oldest history id shown, for paging ("load earlier",
    /// `/history`).
    oldest_history_id: Option<u64>,
    /// The conversation whose `FetchHistory` page is in flight: its
    /// page goes in front of that list; the general replay after
    /// `Init` is appended.
    history_page_pending: Option<Option<client::UserId>>,
    /// Which conversation the chat pane shows: `None` = General, else
    /// the DM partner (spec §6). `history` / `chat_model` /
    /// `oldest_history_id` are that conversation's; the others are
    /// parked with theirs.
    active_dm: Option<client::UserId>,
    parked: HashMap<Option<client::UserId>, Conversation>,
    /// DM partners as the Messages section lists them, newest first,
    /// and what the list knows about each.
    dm_order: Vec<client::UserId>,
    dm_meta: HashMap<client::UserId, DmMeta>,
    dms_model: Rc<VecModel<DmRow>>,
    /// Row ids are unique across conversations (images and GIFs are
    /// keyed by them), so the counter lives here, not per history.
    next_row_id: u64,
    /// Our DMs waiting for the server: `client_ref` → (conversation,
    /// placeholder message id).
    next_client_ref: u32,
    pending_dm_sends: HashMap<u32, (Option<client::UserId>, u64)>,
    /// The active conversation's "older pages exist" (swapped with it).
    history_more: bool,
    /// General messages that arrived while a DM was open.
    general_unread: u32,
    /// The full text of our own messages by id (rows hold segments):
    /// what the composer starts from on "Edit message".
    own_texts: HashMap<u64, String>,
    /// The composer is rewriting one of our messages.
    editing: Option<Editing>,
    /// An edit / delete we sent, until the server's echo confirms it;
    /// the server is silent when it refuses (too old), so a timer
    /// says so instead.
    pending_edit: Option<u64>,
    pending_delete: Option<u64>,
    /// What the generic confirm dialog is about.
    pending_confirm: Option<Confirm>,
    /// The channel dialog while it is open.
    channel_dialog: Option<ChannelDialog>,
    /// The connect screen has been reached once: auto-connect and
    /// start-in-tray apply only then, not after a disconnect.
    startup_done: bool,
    /// Our own avatar, decoded once for the rows (spec §7.1).
    pub own_avatar: Option<slint::Image>,
    /// The hash of our own avatar as uploaded, once the server has it.
    own_avatar_blob: Option<client::protocol::BlobId>,
    /// The look changed in Settings since the server last got it;
    /// sent as one `SetProfile` on Save & close.
    pub profile_dirty: bool,
    /// Everyone's look, from the server (spec §7.1).
    profiles: HashMap<u64, client::protocol::Profile>,
    /// Decoded pictures by hash: the on-disk cache, warmed on demand.
    avatar_images: HashMap<client::protocol::BlobId, slint::Image>,
    /// Blobs asked for and not yet here (no double fetches).
    avatar_fetches: HashSet<client::protocol::BlobId>,
    /// Per-channel facts from `ChannelDetails`, keyed by wire id.
    channel_details: HashMap<u16, ChannelFacts>,
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
    let channel_options = Rc::new(VecModel::<ChannelOption>::default());
    let dms_model = Rc::new(VecModel::<DmRow>::default());
    main.global::<State>()
        .set_dms(ModelRc::from(dms_model.clone()));
    main.global::<State>()
        .set_chat(ModelRc::from(chat_model.clone()));
    main.global::<State>()
        .set_users(ModelRc::from(users_model.clone()));
    main.global::<State>()
        .set_channel_options(ModelRc::from(channel_options.clone()));

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
        channel_infos: Vec::new(),
        pending_channel_join: None,
        connected: false,
        client_key: client::config::load_client_key(),
        permissions: 0,
        our_user_id: None,
        server_persistent: true,
        details: HashMap::new(),
        pending_admin: HashMap::new(),
        next_request_id: 1,
        telemetry_notes: HashMap::new(),
        moderation: None,
        channel_options,
        server_muted: None,
        text_muted: None,
        oldest_history_id: None,
        history_page_pending: None,
        active_dm: None,
        parked: HashMap::new(),
        dm_order: Vec::new(),
        dm_meta: HashMap::new(),
        dms_model,
        next_row_id: 0,
        next_client_ref: 1,
        pending_dm_sends: HashMap::new(),
        history_more: false,
        general_unread: 0,
        own_texts: HashMap::new(),
        editing: None,
        pending_edit: None,
        pending_delete: None,
        pending_confirm: None,
        channel_dialog: None,
        startup_done: false,
        own_avatar: None,
        own_avatar_blob: None,
        profile_dirty: false,
        profiles: HashMap::new(),
        avatar_images: HashMap::new(),
        avatar_fetches: HashSet::new(),
        channel_details: HashMap::new(),
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
        st.apply_text_scale();
        st.main.global::<State>().set_phase(Phase::Splash);

        // Seed the initial device snapshot so the first poll has
        // something to diff against.
        st.last_input_devices = audio::VoiceChat::list_input_devices().unwrap_or_default();
        st.last_output_devices = audio::VoiceChat::list_output_devices().unwrap_or_default();
        st.refresh_device_warning();
    }

    wire_logic(&state);
    start_timers(&state);

    // "Start in the tray": the window stays hidden until the tray icon
    // shows it (only meaningful when the tray is compiled in).
    let start_hidden = {
        let st = state.borrow();
        st.settings.startup.start_minimized && cfg!(feature = "tray")
    };
    if !start_hidden {
        state
            .borrow()
            .main
            .show()
            .expect("failed to show the main window");
    }
    {
        let weak = state.borrow().main.as_weak();
        let armed = state.borrow().main_repaint_arm.clone();
        repaint_on_reveal(state.borrow().main.window(), armed, move || {
            if let Some(main) = weak.upgrade() {
                main.set_repaint_flip(!main.get_repaint_flip());
            }
        });
    }
    {
        let mut st = state.borrow_mut();
        st.reload_own_avatar();
        st.push_ui_prefs();
        st.apply_push_to_talk();
    }
    // The version check gates the splash; without it, straight to the
    // connect screen (and whatever startup asks for there).
    if state.borrow().settings.startup.check_updates {
        crate::updater::spawn_version_check();
    } else {
        state.borrow_mut().show_connect_phase();
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
            // A pending invite prompt referred to the previous server.
            app.clear_connect_verdicts();
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

    // ── Roster moderation (spec §4.1, §8) ────────────────────────
    let w = Rc::downgrade(state);
    logic.on_user_hovered(move |index, hovering| {
        if hovering {
            with_app(&w, |app| app.user_hovered(index));
        }
    });
    let w = Rc::downgrade(state);
    logic.on_author_hovered(move |name, hovering| {
        with_app(&w, |app| app.author_hovered(name.as_str(), hovering));
    });
    let w = Rc::downgrade(state);
    logic.on_user_moderate(move |index, action| {
        with_app(&w, |app| app.moderate(index, action));
    });
    let w = Rc::downgrade(state);
    logic.on_user_move(move |index, channel_id| {
        with_app(&w, |app| {
            if let (Some(target), Ok(channel_id)) =
                (app.admin_target_at(index), u16::try_from(channel_id))
            {
                app.send_admin(
                    AdminCommand::MoveUser { target, channel_id },
                    PendingAdmin::Show,
                );
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_user_set_role(move |index, role| {
        with_app(&w, |app| {
            if let (Some(target), Ok(role)) = (app.admin_target_at(index), u32::try_from(role)) {
                // The built-ins nest, so one role is the whole set.
                app.send_admin(
                    AdminCommand::SetRoles {
                        target,
                        roles: vec![role],
                    },
                    PendingAdmin::Show,
                );
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_user_dm(move |index| {
        with_app(&w, |app| app.dm_from_roster(index));
    });
    let w = Rc::downgrade(state);
    logic.on_dm_clicked(move |index| {
        with_app(&w, |app| {
            if let Some(user) = usize::try_from(index)
                .ok()
                .and_then(|i| app.dm_order.get(i).copied())
            {
                app.switch_conversation(Some(user));
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_general_clicked(move || {
        with_app(&w, |app| app.switch_conversation(None));
    });
    let w = Rc::downgrade(state);
    logic.on_dm_closed(move |index| {
        with_app(&w, |app| {
            if let Some(user) = usize::try_from(index)
                .ok()
                .and_then(|i| app.dm_order.get(i).copied())
            {
                app.close_dm(user);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_user_channel_kick(move |index| {
        with_app(&w, |app| {
            let Some(client_id) = app.user_id_at(index) else {
                return;
            };
            let (Some((user_id, _)), Some(user)) =
                (app.details.get(&client_id), app.users.get(&client_id))
            else {
                return;
            };
            app.network
                .tx
                .send(NetworkControl::ChannelKick {
                    channel_id: user.channel_id,
                    user_id: *user_id,
                })
                .ok();
        });
    });
    let w = Rc::downgrade(state);
    logic.on_moderation_confirm(move || with_app(&w, AppState::confirm_moderation));
    let w = Rc::downgrade(state);
    logic.on_moderation_cancel(move || with_app(&w, AppState::cancel_moderation));

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
    logic.on_composer_doc_end(move || {
        with_app(&w, |app| {
            let len = app.main.global::<State>().get_draft().len();
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            app.main.invoke_composer_set_cursor(len as i32);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_type_through(move |text| {
        with_app(&w, |app| {
            // Only what a person would have typed: one printable
            // character, not a special key (Slint names those with
            // private-use code points) or a control character.
            let printable = text.chars().count() == 1
                && text
                    .chars()
                    .all(|c| !c.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&c));
            if !printable {
                return;
            }
            let s = app.main.global::<State>();
            let mut draft = s.get_draft().to_string();
            draft.push_str(text.as_str());
            s.set_draft(draft.into());
            app.main.invoke_focus_composer();
            let len = s.get_draft().len();
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            app.main.invoke_composer_set_cursor(len as i32);
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
    let w = Rc::downgrade(state);
    logic.on_row_edit(move |lo, hi| {
        with_app(&w, |app| app.row_edit(roster::join_id(lo, hi)));
    });
    let w = Rc::downgrade(state);
    logic.on_row_delete(move |lo, hi| {
        with_app(&w, |app| app.row_delete(roster::join_id(lo, hi)));
    });
    let w = Rc::downgrade(state);
    logic.on_edit_cancel(move || with_app(&w, AppState::edit_cancel));
    let w = Rc::downgrade(state);
    logic.on_load_earlier(move || with_app(&w, |app| app.load_earlier(50)));
    let w = Rc::downgrade(state);
    logic.on_confirm_accept(move || with_app(&w, AppState::confirm_accept));
    let w = Rc::downgrade(state);
    logic.on_confirm_cancel(move || with_app(&w, AppState::confirm_cancel));
    let w = Rc::downgrade(state);
    logic.on_toast_dismiss(move || {
        with_app(&w, |app| {
            app.error_timer.stop();
            app.main.global::<State>().set_error_text("".into());
        });
    });

    // ── Image preview overlay ───────────────────────────────────
    let w = Rc::downgrade(state);
    logic.on_preview_send(move || {
        with_app(&w, |app| {
            if let Some(png) = app.pending_preview.take() {
                if let Some(until) = app.text_muted {
                    // The server would drop it; say so here instead.
                    let until = format!(" {}", describe_until(until));
                    app.show_toast(
                        ToastKind::Warning,
                        &t!(
                            "restricted-text",
                            "until" => until.as_str(),
                            "reason" => ""
                        ),
                    );
                } else if app.active_dm.is_some() {
                    // Images in DMs ride the blob path later (1.1.2).
                    app.show_toast(ToastKind::Warning, &t!("chat-dm-no-images"));
                } else {
                    // Same broadcast semantics as SendMessage: the server
                    // relays ImageMessage back to everyone (sender
                    // included), so no local echo is needed here.
                    app.network.tx.send(NetworkControl::SendImage(png)).ok();
                }
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
    let w = Rc::downgrade(state);
    logic.on_channel_create(move || with_app(&w, AppState::channel_create));
    let w = Rc::downgrade(state);
    logic.on_channel_edit(move |channel_id| {
        with_app(&w, |app| {
            if let Ok(id) = u16::try_from(channel_id) {
                app.channel_edit(id);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_channel_delete(move |channel_id| {
        with_app(&w, |app| {
            if let Ok(id) = u16::try_from(channel_id) {
                app.channel_delete(id);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_channel_dialog_save(move || with_app(&w, AppState::channel_dialog_save));
    let w = Rc::downgrade(state);
    logic.on_channel_dialog_cancel(move || with_app(&w, AppState::channel_dialog_cancel));

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
/// * Move, resize (maximize and restore-from-maximized included) and
///   scale change — the OS repaints the exposed or reshaped surface
///   with the background brush while damage tracking still trusts the
///   back buffer, so an already-focused window shows white bands after
///   a drag or a maximize until something forces a full frame. A
///   full-window flip per such event is cheap next to the frame it
///   already costs.
pub fn repaint_on_reveal(win: &slint::Window, armed: Rc<Cell<bool>>, flip: impl Fn() + 'static) {
    use i_slint_backend_winit::WinitWindowAccessor;
    win.on_winit_window_event(move |_w, ev| {
        use i_slint_backend_winit::winit::event::WindowEvent;
        let reveal = matches!(
            ev,
            WindowEvent::Focused(true)
                | WindowEvent::Occluded(false)
                | WindowEvent::Moved(_)
                | WindowEvent::Resized(_)
                | WindowEvent::ScaleFactorChanged { .. }
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
        for (action, pressed) in hotkey_actions {
            if !st.connected {
                continue;
            }
            match (action, pressed) {
                // A hold, not a toggle: both edges matter.
                (BindableAction::PushToTalk, held) => {
                    st.voice.tx.send(VoiceControl::PushToTalkHeld(held)).ok();
                    st.main.global::<State>().set_ptt_held(held);
                }
                (_, false) => {}
                (BindableAction::ToggleMute, true) => st.handle_toggle_mute(),
                (BindableAction::ToggleDeafen, true) => st.handle_toggle_deafen(),
                (BindableAction::ToggleScreenShare, true) => st.handle_toggle_screenshare(),
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
                self.main.global::<State>().set_error_text("".into());
                self.clear_connect_verdicts();
                // Admitted: from now on the key is the login, so the
                // code has done its one job.
                if let Some(server) = self.settings.default_server_mut()
                    && server.invite_code.take().is_some()
                {
                    self.settings.save();
                    crate::settingsui::refresh_servers(self);
                }
            }
            NetworkMessage::Session {
                user_id,
                permissions,
                persistent,
            } => {
                self.permissions = permissions;
                self.our_user_id = Some(user_id);
                self.server_persistent = persistent;
                log::info!(
                    "session: user {}, permissions {permissions:#x}, persistent {persistent}",
                    short_id(&user_id)
                );
                self.push_permissions();
                self.send_own_profile();
            }
            NetworkMessage::NeedInvite { why } => {
                let s = self.main.global::<State>();
                match why {
                    client::NeedInviteReason::UnknownKey => {
                        info!("Connect refused: this device needs an invite");
                        s.set_invite_prompt_text(t!("connect-need-invite").into());
                        s.set_invite_prompt(true);
                    }
                    client::NeedInviteReason::KeyRetired => {
                        info!("Connect refused: this device was unlinked");
                        s.set_invite_prompt(false);
                        s.set_connect_notice(t!("connect-key-retired").into());
                    }
                }
            }
            NetworkMessage::InviteInvalid { why } => {
                info!("Connect refused: invite {why:?}");
                // Whatever code we stored is no good; do not resend it.
                if let Some(server) = self.settings.default_server_mut() {
                    server.invite_code = None;
                }
                self.settings.save();
                let s = self.main.global::<State>();
                s.set_invite_prompt_text(
                    t!(match why {
                        client::InviteInvalidReason::Expired => "connect-invite-expired",
                        client::InviteInvalidReason::Revoked => "connect-invite-revoked",
                        client::InviteInvalidReason::Exhausted => "connect-invite-exhausted",
                        client::InviteInvalidReason::Unknown => "connect-invite-unknown",
                    })
                    .into(),
                );
                s.set_invite_code("".into());
                s.set_invite_prompt(true);
            }
            NetworkMessage::NameInUse => {
                info!("Connect refused: name in use");
                self.main
                    .global::<State>()
                    .set_connect_notice(t!("connect-name-in-use").into());
            }
            NetworkMessage::InviteCreated {
                code,
                grants_role,
                expires_at,
                max_uses,
            } => {
                // A newcomer needs the server address, the port and this
                // code, as plain text; the code goes on the clipboard.
                let shown = client::invite::display_code(&code);
                crate::clipboard::set_clipboard_text(&shown);
                let role = role_label(grants_role);
                let expiry = describe_expiry(expires_at);
                let uses = match max_uses {
                    Some(n) => t!("invite-uses", "count" => n),
                    None => t!("invite-uses-unlimited"),
                };
                self.push_chat_system(&t!(
                    "chat-invite-created",
                    "code" => shown.as_str(),
                    "role" => role.as_str(),
                    "expiry" => expiry.as_str(),
                    "uses" => uses.as_str()
                ));
            }
            NetworkMessage::InviteDenied { reason } => {
                self.push_chat_system(&t!(match reason {
                    client::InviteDenyReason::NoPermission => "chat-invite-no-permission",
                    client::InviteDenyReason::RoleNotAllowed => "chat-invite-role-not-allowed",
                }));
            }
            NetworkMessage::History {
                conv,
                messages,
                more,
            } => {
                // A page lands in its own conversation, active or
                // parked; a DM's is always one we asked for.
                let key = match conv {
                    client::ConvRef::General => None,
                    client::ConvRef::Dm(user) => Some(user),
                };
                if let Some(user) = key
                    && let Some(meta) = self.dm_meta.get_mut(&user)
                {
                    meta.fetched = true;
                }
                self.with_conversation(key, |app| app.take_history_page(key, &messages, more));
            }
            NetworkMessage::HistoryUnavailable { retry_secs, .. } => {
                self.history_page_pending = None;
                self.main.global::<State>().set_history_loading(false);
                self.push_chat_system(&t!("chat-history-unavailable", "secs" => retry_secs));
            }
            NetworkMessage::MessageEdited { msg_id, text } => {
                if self.pending_edit == Some(msg_id) {
                    self.pending_edit = None;
                }
                if let Some(own) = self.own_texts.get_mut(&msg_id) {
                    own.clone_from(&text);
                }
                let text = app_core::emoji::replace_shortcodes(&text);
                // In place when the message is on screen; a notice
                // when it scrolled out of the window (or predates it).
                let edited = self.with_conversation(None, |app| {
                    match app.history.edit_message(msg_id, &text) {
                        Some(delta) => {
                            app.sync_range(&delta);
                            true
                        }
                        None => false,
                    }
                });
                if !edited {
                    self.push_chat_system(&t!(
                        "chat-message-edited",
                        "id" => msg_id,
                        "text" => text.as_str()
                    ));
                }
            }
            NetworkMessage::MessageDeleted { msg_id } => {
                if self.pending_delete == Some(msg_id) {
                    self.pending_delete = None;
                }
                self.own_texts.remove(&msg_id);
                if self.editing.as_ref().is_some_and(|e| e.msg_id == msg_id) {
                    self.edit_cancel();
                }
                let deleted =
                    self.with_conversation(None, |app| match app.history.delete_message(msg_id) {
                        Some(delta) => {
                            app.sync_range(&delta);
                            true
                        }
                        None => false,
                    });
                if !deleted {
                    self.push_chat_system(&t!("chat-message-deleted", "id" => msg_id));
                }
            }
            NetworkMessage::ChatRateLimited { retry_ms } => {
                let secs = retry_ms.div_ceil(1000);
                self.show_toast(ToastKind::Warning, &t!("chat-rate-limited", "secs" => secs));
            }
            NetworkMessage::ProgressUpdate { xp, level } => {
                let line = t!("chat-level-up", "level" => level, "xp" => xp);
                self.push_chat_system(&line);
                self.show_toast(ToastKind::Success, &line);
            }
            NetworkMessage::ChannelDetails {
                channel_id,
                stable_id,
                owner,
                min_role,
                permanent,
                afk_after_min,
                is_afk,
            } => {
                self.channel_details.insert(
                    channel_id,
                    ChannelFacts {
                        stable_id,
                        owner,
                        min_role,
                        permanent,
                        afk_after_min,
                        is_afk,
                    },
                );
                // One per channel after every list: cheap to re-derive
                // the sidebar each time, and the gates are right the
                // moment the last one lands.
                self.rebuild_channels();
                self.update_user_list();
            }
            NetworkMessage::ChannelDenied { reason } => {
                let what = t!(channel_denied_key(reason));
                if let Some(dialog) = self.channel_dialog.as_mut()
                    && dialog.pending
                {
                    dialog.pending = false;
                    self.main
                        .global::<State>()
                        .set_channel_dialog_error(what.into());
                } else {
                    self.show_error(&what);
                    self.push_chat_system(&what);
                }
            }
            NetworkMessage::MovedIdle { after_min } => {
                let line = t!("chat-moved-idle", "min" => after_min);
                self.push_chat_system(&line);
                self.show_toast(ToastKind::Info, &line);
            }
            NetworkMessage::UserDetails {
                client_id,
                user_id,
                roles,
                level,
            } => {
                let before = self.details.insert(client_id, (user_id, roles.clone()));
                if let Some(user) = self.users.get_mut(&client_id) {
                    user.role = role_of(&roles);
                    user.level = level;
                }
                if self.dm_meta.contains_key(&user_id) {
                    self.rebuild_dms_model();
                }
                if Some(client_id) == self.our_client_id {
                    // Our own rank decides which gated channels we see
                    // and which roles the menu may hand out.
                    self.rebuild_channels();
                    self.push_permissions();
                }
                self.update_user_list();
                if Some(client_id) == self.our_client_id
                    && let Some((_, old)) = before
                    && old != roles
                {
                    let names: Vec<String> = roles.iter().map(|r| role_label(*r)).collect();
                    let now = if names.is_empty() {
                        t!("roles-none")
                    } else {
                        names.join(", ")
                    };
                    let what = t!("chat-your-roles-changed", "roles" => now.as_str());
                    self.push_chat_system(&what);
                    self.show_toast(ToastKind::Info, &what);
                }
            }
            NetworkMessage::Restricted {
                kind,
                until,
                reason,
            } => {
                let until_ms = until;
                // "… for 2 h: spam" / "… indefinitely: spam" / a role gap
                // has neither: "… — not allowed by your role".
                let by_role = reason.is_empty() && until.is_none();
                let until = if by_role {
                    String::new()
                } else {
                    format!(" {}", describe_until(until))
                };
                let reason = if by_role {
                    format!(" — {}", t!("restricted-by-role"))
                } else if reason.is_empty() {
                    String::new()
                } else {
                    format!(": {reason}")
                };
                let what = t!(
                    restricted_key(kind),
                    "until" => until.as_str(),
                    "reason" => reason.as_str()
                );
                self.push_chat_system(&what);
                self.show_toast(ToastKind::Warning, &what);
                match kind {
                    // A server mute: stop sending (spec §4.2), and keep
                    // the button locked until it lifts.
                    client::RestrictionKind::Voice => {
                        if self.server_muted.is_none() {
                            self.server_muted = Some(self.is_muted);
                        }
                        if !self.is_muted {
                            self.set_muted(true);
                        }
                    }
                    client::RestrictionKind::GeneralText => {
                        self.text_muted = Some(until_ms);
                    }
                    _ => {}
                }
            }
            NetworkMessage::RestrictionLifted { kind } => {
                self.push_chat_system(&t!(lifted_key(kind)));
                match kind {
                    client::RestrictionKind::Voice => {
                        if let Some(was_muted) = self.server_muted.take()
                            && !was_muted
                        {
                            self.set_muted(false);
                        }
                    }
                    client::RestrictionKind::GeneralText => self.text_muted = None,
                    _ => {}
                }
            }
            NetworkMessage::AdminResult { request_id, result } => {
                self.handle_admin_result(request_id, result);
            }
            NetworkMessage::ConnectionLost { kind, detail } => {
                // Network, not credentials: come back on our own, with
                // backoff (2, 4, 8, 16, 30 s), instead of dumping the
                // user on the connect screen with a transport error.
                // The user sees one plain sentence per kind; the
                // transport's wording goes to the log only.
                log::warn!("connection lost ({kind:?}): {detail}");
                let what = t!(match kind {
                    LossKind::Timeout => "error-lost-timeout",
                    LossKind::Reset => "error-lost-reset",
                    LossKind::Unreachable => "error-lost-unreachable",
                    LossKind::ServerRestart => "error-lost-restart",
                });
                self.push_chat_system(&what);
                self.teardown_connection();
                self.show_connect_phase();
                const MAX_AUTO_RECONNECTS: u32 = 5;
                if self.reconnect_attempts >= MAX_AUTO_RECONNECTS {
                    self.show_error(&format!("{what} {}", t!("error-lost-gave-up")));
                } else {
                    self.reconnect_attempts += 1;
                    let secs = (2u64 << (self.reconnect_attempts - 1)).min(30);
                    let secs_text = secs.to_string();
                    self.show_toast(
                        ToastKind::Warning,
                        &format!(
                            "{what} {}",
                            t!("error-reconnecting", "secs" => secs_text.as_str())
                        ),
                    );
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
            NetworkMessage::Disconnected { kind, detail } => {
                log::info!("disconnected ({kind:?}): {detail}");
                let what = match &kind {
                    DisconnectKind::UserRequested => t!("disconnect-reason-user"),
                    DisconnectKind::Refused(client::RefusedReason::ServerFull) => {
                        t!("error-refused-full")
                    }
                    DisconnectKind::Refused(client::RefusedReason::RateLimited) => {
                        t!("error-refused-rate-limited")
                    }
                    DisconnectKind::Refused(client::RefusedReason::Banned { until, reason }) => {
                        let until = describe_until(*until);
                        let reason = if reason.is_empty() {
                            t!("reason-none")
                        } else {
                            reason.clone()
                        };
                        t!(
                            "error-banned",
                            "reason" => reason.as_str(),
                            "until" => until.as_str()
                        )
                    }
                    DisconnectKind::Refused(_) | DisconnectKind::Outdated => {
                        t!("error-outdated")
                    }
                    DisconnectKind::Removed { kind, reason, by } => {
                        let reason = if reason.is_empty() {
                            t!("reason-none")
                        } else {
                            reason.clone()
                        };
                        let key = match kind {
                            client::RemovedKind::Disconnected => "removed-disconnected",
                            client::RemovedKind::Kicked => "removed-kicked",
                            client::RemovedKind::Banned { .. } => "removed-banned",
                            client::RemovedKind::Replaced => "removed-replaced",
                        };
                        let until = match kind {
                            client::RemovedKind::Banned { until } => describe_until(*until),
                            _ => String::new(),
                        };
                        t!(
                            key,
                            "by" => by.as_str(),
                            "reason" => reason.as_str(),
                            "until" => until.as_str()
                        )
                    }
                    DisconnectKind::LocalError => {
                        t!("error-local", "detail" => detail.as_str())
                    }
                    DisconnectKind::Other => t!("error-lost-generic"),
                };
                if self.connected {
                    self.push_chat_system(&t!("chat-disconnected", "reason" => what.as_str()));
                    self.teardown_connection();
                    self.show_connect_phase();
                }
                match kind {
                    DisconnectKind::UserRequested => {}
                    // Verdicts that stay on the card until the next try.
                    DisconnectKind::Refused(client::RefusedReason::Banned { .. })
                    | DisconnectKind::Removed { .. } => {
                        self.main.global::<State>().set_connect_notice(what.into());
                    }
                    // A full server is theirs, not ours: a warning.
                    DisconnectKind::Refused(client::RefusedReason::ServerFull) => {
                        self.show_toast(ToastKind::Warning, &what);
                    }
                    _ => self.show_error(&what),
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
            NetworkMessage::TextMessage {
                client_id,
                id,
                ts,
                username,
                text,
            } => {
                let meta = Meta {
                    id,
                    ts: format_wall_time(ts),
                    ts_ms: ts,
                    day: local_day(ts),
                    user: username,
                    own: Some(client_id) == self.our_client_id,
                    pending: false,
                };
                if meta.own {
                    self.own_texts.insert(id, text.clone());
                }
                // Render shortcodes from clients that send them raw.
                let text = app_core::emoji::replace_shortcodes(&text);
                self.push_chat_message(&meta, &text, false);
                // Ordinary messages are silent by design; a mention may
                // chime if the person asked for it.
                if !meta.own && self.settings.sounds.mention_on() && self.mentions_us(&text) {
                    self.voice
                        .tx
                        .send(VoiceControl::PlaySound(SoundType::Mention))
                        .ok();
                }
            }
            NetworkMessage::UploadProgress { pct } => {
                let s = self.main.global::<State>();
                if pct >= 100 {
                    s.set_upload_line("".into());
                } else {
                    s.set_upload_line(t!("chat-upload-progress", "pct" => u32::from(pct)).into());
                }
            }
            NetworkMessage::ImageMessage {
                client_id,
                id,
                ts,
                username,
                data,
            } => {
                let meta = Meta {
                    id,
                    ts: format_wall_time(ts),
                    ts_ms: ts,
                    day: local_day(ts),
                    user: username.clone(),
                    own: Some(client_id) == self.our_client_id,
                    pending: false,
                };
                let shown =
                    self.with_conversation(None, |app| media::push_image_row(app, &meta, data));
                if !shown {
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
            // ── 1.1.2: profiles and blobs (spec §7, §7.1) ────────────
            NetworkMessage::UserProfile { client_id, profile } => {
                self.take_profile(client_id, profile);
            }
            NetworkMessage::ProfileUpdated { user_id, profile } => {
                let client = self
                    .details
                    .iter()
                    .find(|(_, (u, _))| *u == user_id)
                    .map(|(c, _)| *c);
                if let Some(client_id) = client {
                    self.take_profile(client_id, profile);
                }
            }
            NetworkMessage::ProfileDenied { reason } => {
                self.show_toast(ToastKind::Warning, &t!(profile_denied_key(reason)));
            }
            NetworkMessage::UserRenamed { client_id, name } => {
                let old = self.username_of(client_id);
                if let Some(user) = self.users.get_mut(&client_id) {
                    user.username.clone_from(&name);
                }
                // The Messages list and an open DM header follow a
                // partner's rename.
                let renamed_partner = self
                    .details
                    .get(&client_id)
                    .map(|(u, _)| *u)
                    .filter(|u| self.dm_meta.contains_key(u));
                if let Some(user_id) = renamed_partner {
                    if let Some(meta) = self.dm_meta.get_mut(&user_id) {
                        meta.name.clone_from(&name);
                    }
                    self.rebuild_dms_model();
                    if self.active_dm == Some(user_id) {
                        self.main
                            .global::<State>()
                            .set_conv_name(name.as_str().into());
                    }
                }
                self.push_chat_system(&t!(
                    "chat-user-renamed",
                    "old" => old.as_str(),
                    "new" => name.as_str()
                ));
                self.update_user_list();
            }
            NetworkMessage::RenameDenied { reason } => {
                self.show_toast(ToastKind::Warning, &t!(rename_denied_key(reason)));
            }
            NetworkMessage::UploadDone { upload_id, blob_id } => {
                if upload_id == AVATAR_UPLOAD_ID {
                    // The picture is on the server: now the profile
                    // can point at it.
                    self.own_avatar_blob = Some(blob_id);
                    self.avatar_images
                        .entry(blob_id)
                        .or_insert_with(|| self.own_avatar.clone().unwrap_or_default());
                    self.send_own_profile();
                }
            }
            NetworkMessage::UploadRejected { upload_id, reason } => {
                if upload_id == AVATAR_UPLOAD_ID {
                    self.show_toast(
                        ToastKind::Warning,
                        &t!("profile-avatar-refused", "why" => reason.describe()),
                    );
                    // The rest of the look still goes out.
                    self.own_avatar_blob = None;
                    self.send_own_profile();
                }
            }
            NetworkMessage::Blob { blob_id, data } => {
                self.avatar_fetches.remove(&blob_id);
                self.take_avatar_bytes(blob_id, &data, true);
            }
            NetworkMessage::BlobMissing { blob_id } => {
                self.avatar_fetches.remove(&blob_id);
            }
            // ── 1.1.1: direct messages — the conversation UI is the
            // next drop; until then a DM lands in the general view
            // marked as such, and a refusal is a toast. ───────────
            NetworkMessage::Direct {
                from,
                username,
                id,
                ts,
                text,
            } => {
                self.take_direct(from, &username, id, ts, &text);
            }
            NetworkMessage::MessageAck { client_ref, msg_id } => {
                if let Some((key, placeholder)) = self.pending_dm_sends.remove(&client_ref) {
                    self.with_conversation(key, |app| {
                        if let Some(delta) = app.history.confirm_message(placeholder, msg_id) {
                            app.sync_range(&delta);
                        }
                        app.oldest_history_id.get_or_insert(msg_id);
                    });
                    if let Some(user) = key
                        && let Some(meta) = self.dm_meta.get_mut(&user)
                    {
                        meta.last_msg_id = meta.last_msg_id.max(msg_id);
                    }
                }
            }
            NetworkMessage::MessageFailed { client_ref, reason } => {
                if let Some((key, placeholder)) = self.pending_dm_sends.remove(&client_ref) {
                    self.with_conversation(key, |app| {
                        if let Some(delta) = app.history.discard_message(placeholder) {
                            app.sync_range(&delta);
                        }
                    });
                }
                self.show_toast(ToastKind::Warning, &t!(dm_failed_key(reason)));
            }
            NetworkMessage::Conversations { dms } => {
                // The list from the store: names, unread counts, the
                // newest id per partner. Histories load when opened.
                self.dm_order.clear();
                for dm in dms {
                    let meta = self.dm_meta.entry(dm.user_id).or_insert_with(|| DmMeta {
                        name: dm.name.clone(),
                        unread: 0,
                        last_ts: 0,
                        last_msg_id: 0,
                        fetched: false,
                    });
                    meta.name = dm.name;
                    meta.unread = dm.unread;
                    meta.last_ts = dm.last_ts;
                    meta.last_msg_id = dm.last_msg_id;
                    self.dm_order.push(dm.user_id);
                }
                self.rebuild_dms_model();
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
        self.main.global::<State>().set_connect_notice("".into());

        // A code (or a whole link) typed into the invite box counts —
        // clicking Connect means "use what I typed". It is stored on
        // the entry so a restart before admission still has it.
        let s = self.main.global::<State>();
        if s.get_invite_prompt() {
            let typed = s.get_invite_code().to_string();
            if let Some(server) = self.settings.default_server_mut()
                && !server.set_invite_input(&typed)
            {
                self.show_error(&t!("error-invite-code-malformed"));
                return;
            }
        }
        self.settings.save();
        crate::settingsui::refresh_servers(self);

        let Some(client_key) = self.client_key.clone() else {
            let path = client::config::client_key_path().display().to_string();
            self.show_error(&t!("error-no-identity", "path" => path.as_str()));
            return;
        };
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
                invite_code: server.invite_code,
                fingerprint: server.fingerprint,
                client_key,
                portable: client::config::is_portable_install(),
            })
            .ok();
    }

    /// Dismiss the invite box and the sticky verdict line.
    pub fn clear_connect_verdicts(&mut self) {
        let s = self.main.global::<State>();
        s.set_invite_prompt(false);
        s.set_invite_prompt_text("".into());
        s.set_invite_code("".into());
        s.set_connect_notice("".into());
    }

    /// One slash command from the composer. Moderation commands map
    /// onto `AdminCommand` (spec §8): the server checks them, the
    /// answer lands in chat.
    fn handle_slash_command(&mut self, command: &str, args: &str) {
        match command {
            "invite" => self.handle_invite_command(args),
            "help" | "commands" => {
                let commands = self.commands_available();
                let line = t!("chat-commands-help", "commands" => commands.as_str());
                self.push_chat_system(&line);
            }
            "leave" => self.handle_disconnect(),
            "quit" | "exit" => {
                let _ = slint::quit_event_loop();
            }
            "users" => self.send_admin(AdminCommand::ListUsers, PendingAdmin::Show),
            "restrictions" | "bans" => {
                self.send_admin(AdminCommand::ListRestrictions, PendingAdmin::Show);
            }
            "invites" => self.send_admin(AdminCommand::ListInvites, PendingAdmin::Show),
            "telemetry" => {
                let target = self.target_for(args.trim());
                self.send_admin(AdminCommand::LastTelemetry { target }, PendingAdmin::Show);
            }
            "history" => {
                let limit = args.trim().parse::<u16>().unwrap_or(20).clamp(1, 200);
                self.load_earlier(limit);
            }
            "dm" | "msg" => {
                let (name, text) = match args.trim().split_once(char::is_whitespace) {
                    Some((n, t)) if !t.trim().is_empty() => (n.trim(), t.trim().to_string()),
                    _ => {
                        self.push_chat_system(&t!("chat-command-usage-dm"));
                        return;
                    }
                };
                let found = self
                    .users
                    .iter()
                    .find(|(_, u)| u.username.eq_ignore_ascii_case(name))
                    .map(|(c, u)| (*c, u.username.clone()));
                let Some((client_id, shown)) = found else {
                    self.push_chat_system(&t!("chat-dm-unknown", "name" => name));
                    return;
                };
                let Some(user_id) = self.details.get(&client_id).map(|(u, _)| *u) else {
                    return;
                };
                self.open_dm(user_id, &shown);
                self.send_dm(user_id, text);
            }
            "channel" => self.handle_channel_command(args),
            "kick" | "ban" | "unban" | "disconnect" | "mute" | "unmute" | "grant" | "revoke"
            | "move" => self.handle_user_command(command, args),
            "revoke-invite" => match client::invite::normalize_code(args) {
                Some(code) => {
                    self.send_admin(AdminCommand::RevokeInvite { code }, PendingAdmin::Show);
                }
                None => self.push_chat_system(&t!("chat-command-usage-revoke-invite")),
            },
            "unlink" => match parse_hex_id(args.trim()) {
                Some(key_hash) => {
                    self.send_admin(AdminCommand::UnlinkKey { key_hash }, PendingAdmin::Show);
                }
                None => self.push_chat_system(&t!("chat-command-usage-unlink")),
            },
            "link" => {
                let mut words = args.split_whitespace();
                match (words.next(), words.next().and_then(parse_hex_id)) {
                    (Some(name), Some(key_hash)) => {
                        let target = self.target_for(name);
                        self.send_admin(
                            AdminCommand::LinkKey { target, key_hash },
                            PendingAdmin::Show,
                        );
                    }
                    _ => self.push_chat_system(&t!("chat-command-usage-link")),
                }
            }
            _ => self.push_chat_system(&t!("chat-command-unknown", "command" => command)),
        }
    }

    /// The commands about a person:
    /// `/kick name [reason]` · `/ban name [7d] [reason]` · `/unban name`
    /// · `/disconnect name [reason]` · `/mute [voice|text] name [30m]
    /// [reason]` · `/unmute [voice|text] name` · `/grant name role` ·
    /// `/revoke name role` · `/move name channel`.
    fn handle_user_command(&mut self, command: &str, args: &str) {
        let mut words: Vec<&str> = args.split_whitespace().collect();
        // `/mute` and `/unmute` take an optional kind before the name.
        let mut kind = client::RestrictionKind::Voice;
        if matches!(command, "mute" | "unmute")
            && let Some(first) = words.first()
            && let Some(k) = client::RestrictionKind::parse(first)
            && matches!(
                k,
                client::RestrictionKind::Voice | client::RestrictionKind::GeneralText
            )
        {
            kind = k;
            words.remove(0);
        }
        if words.is_empty() {
            self.push_chat_system(&t!("chat-command-usage", "command" => command));
            return;
        }
        let name = words.remove(0);
        let target = self.target_for(name);
        let rest = words;
        // An optional duration right after the name; the rest is the reason.
        let (duration_secs, reason_words) = match rest.first().and_then(|w| parse_duration_secs(w))
        {
            Some(secs) => (Some(secs), &rest[1..]),
            None => (None, &rest[..]),
        };
        let reason = reason_words.join(" ");
        let admin = match command {
            "kick" => AdminCommand::Kick { target, reason },
            "disconnect" => AdminCommand::Disconnect { target, reason },
            "ban" => AdminCommand::Ban {
                target,
                duration_secs,
                reason,
            },
            "unban" => AdminCommand::Unban { target },
            "mute" => AdminCommand::Restrict {
                target,
                kind,
                duration_secs,
                reason,
            },
            "unmute" => AdminCommand::Unmute { target, kind },
            "grant" | "revoke" => {
                let Some(role) = rest.first().and_then(|w| parse_role(w)) else {
                    self.push_chat_system(&t!("chat-command-usage", "command" => command));
                    return;
                };
                // Their current roles, known while they are online.
                let mut roles: Vec<client::RoleId> = match &target {
                    AdminTarget::User(user_id) => self
                        .details
                        .values()
                        .find(|(u, _)| u == user_id)
                        .map(|(_, r)| r.clone())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                if command == "grant" {
                    if !roles.contains(&role) {
                        roles.push(role);
                    }
                } else {
                    roles.retain(|r| *r != role);
                }
                AdminCommand::SetRoles { target, roles }
            }
            "move" => {
                let wanted = rest.join(" ");
                let Some(channel_id) = self
                    .channels
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(wanted.trim()))
                    .and_then(|i| u16::try_from(i).ok())
                else {
                    self.push_chat_system(&t!("chat-command-usage", "command" => command));
                    return;
                };
                AdminCommand::MoveUser { target, channel_id }
            }
            _ => return,
        };
        self.send_admin(admin, PendingAdmin::Show);
    }

    /// The commands this session may use, from `Session.permissions`
    /// (spec §8): a member sees what a member can do, not what an
    /// admin can.
    fn commands_available(&self) -> String {
        use client::permissions as p;
        let has = |bit: u64| p::has(self.permissions, bit);
        let mut out: Vec<&str> = vec!["/help", "/history [n]", "/channel list", "/leave", "/quit"];
        if has(p::CREATE_INVITES) {
            out.push("/invite [role] [24h] [uses]");
        }
        if has(p::CREATE_CHANNELS) {
            out.push("/channel create <name> [password]");
        }
        if has(p::MANAGE_OWN_CHANNELS) || has(p::MANAGE_CHANNELS) {
            out.push("/channel keep <name>");
            out.push("/channel delete <name>");
            out.push("/channel kick <user>");
        }
        if has(p::MANAGE_CHANNELS) {
            out.push("/move <user> <channel>");
        }
        if has(p::MUTE) || has(p::RESTRICT) {
            out.push("/mute [voice|text] <user> [30m] [reason]");
            out.push("/unmute [voice|text] <user>");
            out.push("/bans");
        }
        if has(p::DISCONNECT) {
            out.push("/disconnect <user> [reason]");
        }
        if has(p::KICK) {
            out.push("/kick <user> [reason]");
        }
        if has(p::BAN) {
            out.push("/ban <user> [7d] [reason]");
            out.push("/unban <user>");
        }
        if has(p::MANAGE_ROLES) {
            out.push("/grant <user> <role>");
            out.push("/revoke <user> <role>");
        }
        if has(p::MANAGE_INVITES) {
            out.push("/invites");
            out.push("/revoke-invite <code>");
        }
        if has(p::MANAGE_USERS) {
            out.push("/users");
            out.push("/telemetry <user>");
            out.push("/link <user> <keyhash>");
            out.push("/unlink <keyhash>");
        }
        out.join(" · ")
    }

    /// `/channel create <name> [password]` · `/channel keep <name>` ·
    /// `/channel delete <name>` · `/channel kick <user>` (spec §5).
    /// The dialog comes with the UI step; this is enough to try it.
    fn handle_channel_command(&mut self, args: &str) {
        let mut words = args.split_whitespace();
        let Some(verb) = words.next() else {
            self.push_chat_system(&t!("chat-command-usage-channel"));
            return;
        };
        match verb {
            "list" => {
                let lines: Vec<String> = self
                    .channels
                    .iter()
                    .map(|c| {
                        let facts = self.channel_details.get(&c.id);
                        let owner = facts
                            .and_then(|f| f.owner)
                            .and_then(|o| {
                                self.details
                                    .iter()
                                    .find(|(_, (u, _))| *u == o)
                                    .and_then(|(client_id, _)| self.users.get(client_id))
                                    .map(|u| u.username.clone())
                            })
                            .unwrap_or_else(|| t!("channel-owner-server"));
                        let flags: Vec<&str> = [
                            facts.is_some_and(|f| f.is_afk).then_some("afk"),
                            facts.is_some_and(|f| !f.permanent).then_some("temporary"),
                            c.locked.then_some("password"),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        format!(
                            "#{} {} — {} — {}{}",
                            facts.map_or(0, |f| f.stable_id),
                            c.name,
                            owner,
                            if flags.is_empty() {
                                "permanent".to_string()
                            } else {
                                flags.join(", ")
                            },
                            c.user_limit
                                .map_or(String::new(), |l| format!(" — limit {l}"))
                        )
                    })
                    .collect();
                for line in lines {
                    self.push_chat_system(&line);
                }
            }
            "create" => {
                let Some(name) = words.next() else {
                    self.push_chat_system(&t!("chat-command-usage-channel"));
                    return;
                };
                let password = words.next().map(str::to_string);
                self.create_channel(client::ChannelSpec {
                    name: name.to_string(),
                    password,
                    user_limit: None,
                    min_role: None,
                    permanent: false,
                    afk_after_min: None,
                });
            }
            "keep" | "delete" => {
                let wanted = words.collect::<Vec<_>>().join(" ");
                let Some(channel_id) = self.channel_id_named(&wanted) else {
                    self.push_chat_system(&t!("chat-command-usage-channel"));
                    return;
                };
                if verb == "delete" {
                    self.network
                        .tx
                        .send(NetworkControl::DeleteChannel { channel_id })
                        .ok();
                } else if let Some(mut spec) = self.current_spec(channel_id) {
                    spec.permanent = true;
                    self.edit_channel(channel_id, spec);
                }
            }
            "kick" => {
                let Some(name) = words.next() else {
                    self.push_chat_system(&t!("chat-command-usage-channel"));
                    return;
                };
                let (Some(channel_id), AdminTarget::User(user_id)) =
                    (self.our_channel(), self.target_for(name))
                else {
                    self.push_chat_system(&t!("chat-command-no-such-user", "name" => name));
                    return;
                };
                self.network
                    .tx
                    .send(NetworkControl::ChannelKick {
                        channel_id,
                        user_id,
                    })
                    .ok();
            }
            _ => self.push_chat_system(&t!("chat-command-usage-channel")),
        }
    }

    /// Ask the server for a channel; remember its password locally so
    /// the creator joins it without a prompt and can edit it later
    /// (the server never sends passwords back).
    fn create_channel(&mut self, spec: client::ChannelSpec) {
        if let Some(password) = spec.password.clone()
            && let Some(server) = self.settings.default_server_mut()
        {
            server.channel_passwords.insert(spec.name.clone(), password);
            self.settings.save();
        }
        self.network
            .tx
            .send(NetworkControl::CreateChannel(spec))
            .ok();
    }

    /// Send a whole new spec for a channel (the server replaces every
    /// field), keeping the local password copy in step.
    fn edit_channel(&mut self, channel_id: u16, spec: client::ChannelSpec) {
        if let Some(server) = self.settings.default_server_mut() {
            let old_name = self
                .channels
                .iter()
                .find(|c| c.id == channel_id)
                .map(|c| c.name.clone());
            if let Some(old) = old_name
                && old != spec.name
            {
                server.channel_passwords.remove(&old);
            }
            match &spec.password {
                Some(p) => {
                    server
                        .channel_passwords
                        .insert(spec.name.clone(), p.clone());
                }
                None => {
                    server.channel_passwords.remove(&spec.name);
                }
            }
            self.settings.save();
        }
        self.network
            .tx
            .send(NetworkControl::EditChannel { channel_id, spec })
            .ok();
    }

    /// A channel's settings as this client knows them: the wire info,
    /// the facts, and the password only if it was saved locally (the
    /// server never sends it, so an edit without it clears it).
    fn current_spec(&self, channel_id: u16) -> Option<client::ChannelSpec> {
        let info = self.channel_infos.get(usize::from(channel_id))?;
        let facts = self.channel_details.get(&channel_id);
        let password = if info.has_password {
            self.settings
                .default_server()
                .and_then(|server| server.channel_passwords.get(&info.name).cloned())
        } else {
            None
        };
        Some(client::ChannelSpec {
            name: info.name.clone(),
            password,
            user_limit: info.user_limit,
            min_role: facts.and_then(|f| f.min_role),
            permanent: facts.is_none_or(|f| f.permanent),
            afk_after_min: facts.and_then(|f| f.afk_after_min),
        })
    }

    /// The `+`: an empty dialog.
    fn channel_create(&mut self) {
        self.channel_dialog = Some(ChannelDialog {
            editing: None,
            min_role: None,
            pending: false,
        });
        let s = self.main.global::<State>();
        s.set_channel_dialog_editing(false);
        s.set_channel_dialog_title(t!("channel-dialog-title-new").into());
        s.set_channel_dialog_name("".into());
        s.set_channel_dialog_password("".into());
        s.set_channel_dialog_password_note("".into());
        s.set_channel_dialog_limit("".into());
        s.set_channel_dialog_afk("".into());
        s.set_channel_dialog_keep(false);
        s.set_channel_dialog_members_only(false);
        s.set_channel_dialog_error("".into());
        s.set_channel_dialog_open(true);
    }

    /// "Edit channel…": the dialog filled with what this client knows
    /// (spec §5; the password only when it was saved here).
    fn channel_edit(&mut self, channel_id: u16) {
        let Some(spec) = self.current_spec(channel_id) else {
            return;
        };
        let has_password = self
            .channel_infos
            .get(usize::from(channel_id))
            .is_some_and(|c| c.has_password);
        self.channel_dialog = Some(ChannelDialog {
            editing: Some(channel_id),
            min_role: spec.min_role,
            pending: false,
        });
        let s = self.main.global::<State>();
        s.set_channel_dialog_editing(true);
        s.set_channel_dialog_title(
            t!("channel-dialog-title-edit", "name" => spec.name.as_str()).into(),
        );
        s.set_channel_dialog_name(spec.name.into());
        s.set_channel_dialog_password_note(if has_password && spec.password.is_none() {
            t!("channel-password-unknown").into()
        } else {
            "".into()
        });
        s.set_channel_dialog_password(spec.password.unwrap_or_default().into());
        s.set_channel_dialog_limit(
            spec.user_limit
                .map_or(String::new(), |l| l.to_string())
                .into(),
        );
        s.set_channel_dialog_afk(
            spec.afk_after_min
                .map_or(String::new(), |m| m.to_string())
                .into(),
        );
        s.set_channel_dialog_keep(spec.permanent);
        s.set_channel_dialog_members_only(spec.min_role.is_some());
        s.set_channel_dialog_error("".into());
        s.set_channel_dialog_open(true);
    }

    /// "Delete channel…": confirm with the name, then send.
    fn channel_delete(&mut self, channel_id: u16) {
        let name = self.channel_name(channel_id);
        self.pending_confirm = Some(Confirm::DeleteChannel { channel_id });
        let s = self.main.global::<State>();
        s.set_confirm_title(t!("confirm-delete-channel-title", "name" => name.as_str()).into());
        s.set_confirm_body(t!("confirm-delete-channel-body").into());
        s.set_confirm_label(t!("confirm-delete-channel").into());
        s.set_confirm_open(true);
    }

    /// Save on the dialog: validate what the server would refuse as
    /// `Invalid` here, with a sentence per field, then send the spec.
    fn channel_dialog_save(&mut self) {
        let Some(dialog) = self.channel_dialog.as_mut() else {
            return;
        };
        if dialog.pending {
            return;
        }
        let s = self.main.global::<State>();
        let name = s.get_channel_dialog_name().trim().to_string();
        if name.is_empty() || name.chars().count() > 40 {
            s.set_channel_dialog_error(t!("channel-error-name").into());
            return;
        }
        let Ok(user_limit) = parse_optional_count(s.get_channel_dialog_limit().as_str()) else {
            s.set_channel_dialog_error(t!("channel-error-limit").into());
            return;
        };
        let Ok(afk_after_min) = parse_optional_count(s.get_channel_dialog_afk().as_str()) else {
            s.set_channel_dialog_error(t!("channel-error-afk").into());
            return;
        };
        let password = s.get_channel_dialog_password().trim().to_string();
        let min_role = s
            .get_channel_dialog_members_only()
            .then(|| dialog.min_role.unwrap_or(client::permissions::ROLE_MEMBER));
        let spec = client::ChannelSpec {
            name,
            password: (!password.is_empty()).then_some(password),
            user_limit,
            min_role,
            permanent: s.get_channel_dialog_keep(),
            afk_after_min,
        };
        s.set_channel_dialog_error("".into());
        dialog.pending = true;
        let editing = dialog.editing;
        match editing {
            Some(channel_id) => self.edit_channel(channel_id, spec),
            None => self.create_channel(spec),
        }
    }

    fn channel_dialog_cancel(&mut self) {
        self.channel_dialog = None;
        self.main.global::<State>().set_channel_dialog_open(false);
    }

    /// The wire id of a channel by name, case-insensitive.
    fn channel_id_named(&self, name: &str) -> Option<u16> {
        self.channels
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name.trim()))
            .and_then(|i| u16::try_from(i).ok())
    }

    /// Whom a name refers to: the id when they are in the roster right
    /// now, else the name for the server to resolve (someone offline —
    /// a ban to lift, a kicked person's roles).
    fn target_for(&self, name: &str) -> AdminTarget {
        let name = name.trim_start_matches('@');
        self.users
            .iter()
            .find(|(_, u)| u.username.eq_ignore_ascii_case(name))
            .and_then(|(client_id, _)| self.details.get(client_id))
            .map_or_else(
                || AdminTarget::Name(name.to_string()),
                |(user_id, _)| AdminTarget::User(*user_id),
            )
    }

    fn send_admin(&mut self, command: AdminCommand, then: PendingAdmin) {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.pending_admin.insert(request_id, then);
        self.network
            .tx
            .send(NetworkControl::Admin {
                request_id,
                command,
            })
            .ok();
    }

    fn handle_admin_result(
        &mut self,
        request_id: u32,
        result: Result<client::AdminOutcome, client::AdminError>,
    ) {
        let then = self
            .pending_admin
            .remove(&request_id)
            .unwrap_or(PendingAdmin::Show);
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                if let PendingAdmin::Telemetry(client_id) = then {
                    // A tooltip fetch that was refused is a log line,
                    // not a chat notice.
                    log::info!("telemetry for {client_id:#x} refused: {}", e.describe());
                    return;
                }
                let why = e.describe();
                self.push_chat_system(&t!("chat-admin-refused", "why" => why.as_str()));
                return;
            }
        };
        if let PendingAdmin::Telemetry(client_id) = then {
            if let client::AdminOutcome::Telemetry(sample) = outcome {
                let text = match sample {
                    Some(t) => {
                        let ago = describe_ago(t.ts);
                        format!(
                            "{}\n{}",
                            t!("tooltip-telemetry-age", "ago" => ago.as_str()),
                            t.line.replace(" | ", "\n")
                        )
                    }
                    None => t!("tooltip-no-telemetry"),
                };
                self.telemetry_notes.insert(
                    client_id,
                    TelemetryNote {
                        fetched: Instant::now(),
                        text,
                    },
                );
                self.update_user_list();
            }
            return;
        }
        match outcome {
            client::AdminOutcome::Done => self.push_chat_system(&t!("chat-admin-done")),
            client::AdminOutcome::Users(users) => {
                self.push_chat_system(&t!("chat-admin-users-header", "count" => users.len()));
                for u in users {
                    let roles: Vec<String> = u.roles.iter().map(|r| role_label(*r)).collect();
                    let line = format!(
                        "{}{} — {} — {} — {}",
                        if u.client_id.is_some() {
                            "● "
                        } else {
                            "○ "
                        },
                        u.name,
                        if roles.is_empty() {
                            "no roles".to_string()
                        } else {
                            roles.join(", ")
                        },
                        short_id(&u.user_id),
                        describe_ago(u.last_seen)
                    );
                    self.push_chat_system(&line);
                }
            }
            client::AdminOutcome::Restrictions(list) => {
                self.push_chat_system(&t!(
                    "chat-admin-restrictions-header",
                    "count" => list.len()
                ));
                for r in list {
                    let who = r
                        .user_name
                        .clone()
                        .or_else(|| r.key_hash.map(|k| format!("key {}", short_id(&k))))
                        .or_else(|| r.address.clone())
                        .unwrap_or_default();
                    let line = format!(
                        "#{} {} {} — {} — {} — {}",
                        r.id,
                        r.kind.name(),
                        who,
                        describe_expiry(r.until),
                        r.reason,
                        r.by
                    );
                    self.push_chat_system(&line);
                }
            }
            client::AdminOutcome::Telemetry(sample) => match sample {
                Some(t) => {
                    let line = format!("{} — {}", describe_ago(t.ts), t.line);
                    self.push_chat_system(&line);
                }
                None => self.push_chat_system(&t!("chat-admin-no-telemetry")),
            },
            client::AdminOutcome::Invites(list) => {
                self.push_chat_system(&t!("chat-admin-invites-header", "count" => list.len()));
                for i in list {
                    let uses = match i.max_uses {
                        Some(max) => format!("{}/{max}", i.uses),
                        None => i.uses.to_string(),
                    };
                    let state = if i.revoked { " (revoked)" } else { "" };
                    let line = format!(
                        "{} — {} — {} — uses {uses}{state}",
                        client::invite::display_code(&i.code),
                        role_label(i.grants_role),
                        describe_expiry(i.expires_at)
                    );
                    self.push_chat_system(&line);
                }
            }
        }
    }

    /// `/invite [guest|member|organizer] [<n>m|h|d] [<uses>]` from the
    /// composer: ask the server for a code. The answer lands in chat as
    /// a link (and on the clipboard).
    fn handle_invite_command(&mut self, args: &str) {
        let mut grants_role = None;
        let mut expires_secs = None;
        let mut max_uses = None;
        for word in args.split_whitespace() {
            let lower = word.to_ascii_lowercase();
            if let Some(role) = client::permissions::BUILTIN_ROLES
                .iter()
                .find(|r| r.name == lower)
            {
                grants_role = Some(role.id);
            } else if let Some(secs) = parse_duration_secs(&lower) {
                expires_secs = Some(secs);
            } else if let Ok(n) = lower.parse::<u32>() {
                max_uses = Some(n);
            } else {
                self.push_chat_system(&t!("chat-invite-usage"));
                return;
            }
        }
        self.network
            .tx
            .send(NetworkControl::CreateInvite {
                grants_role,
                expires_secs,
                max_uses,
            })
            .ok();
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
        self.reset_chat_flow();

        self.main.global::<State>().set_phase(Phase::Main);

        self.our_client_id = Some(client_id);
        self.server_name = server_name.to_string();
        // `ChannelDetails` follow, one per channel, and are keyed by
        // these ids — a stale set from an earlier list must not leak.
        self.channel_infos = channels.to_vec();
        self.channel_details.clear();
        self.rebuild_channels();
        self.pending_channel_join = None;
        let default_audio = PeerAudioSettings::default();
        for (&id, name) in users {
            let audio = peer_audio_from(
                self.settings
                    .peer_settings
                    .get(&peer_key(name))
                    .unwrap_or(&default_audio),
            );
            let channel_id = memberships.get(&id).copied().unwrap_or_default();
            self.users
                .insert(id, UserState::new(name.clone(), audio, channel_id));
        }

        // Start voice engine
        self.voice
            .tx
            .send(VoiceControl::Start {
                settings: Box::new(self.settings.clone()),
                our_client_id: client_id,
            })
            .ok();
        // Join ding, queued behind Start on the same channel so it
        // plays on the freshly started (or still-running) engine.
        self.play_chime(SoundType::UserJoined);

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
        self.rebuild_dms_model();
        let mut user = UserState::new(username, peer_audio_from(&saved), channel_id);
        // `UserDetails` normally follows `UserJoined`; if it got here
        // first (a replaced session's details), honor it.
        if let Some((_, roles)) = self.details.get(&client_id) {
            user.role = role_of(roles);
        }
        self.users.insert(client_id, user);
        // Join ding only for arrivals into *our* channel — like the
        // classics, other rooms come and go silently.
        if Some(channel_id) == self.our_channel() {
            self.play_chime(SoundType::UserJoined);
        }
        self.update_user_list();
    }

    fn handle_user_left(&mut self, client_id: u64, username: &str) {
        self.details.remove(&client_id);
        self.telemetry_notes.remove(&client_id);
        self.profiles.remove(&client_id);
        self.rebuild_dms_model();
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
            self.play_chime(SoundType::UserLeft);
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
            self.play_chime(SoundType::UserJoined);
        } else if Some(channel_id) == our_channel_before {
            // Someone stepped into our room.
            self.play_chime(SoundType::UserJoined);
        } else if old.is_some() && old == our_channel_before {
            // Someone stepped out of our room.
            self.play_chime(SoundType::UserLeft);
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

        // Ids are renumbered: the facts keyed by the old ids are void
        // until the server's fresh `ChannelDetails` land.
        self.channel_infos = channels.to_vec();
        self.channel_details.clear();
        self.rebuild_channels();
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
        // The list the server just sent is the answer to a create or
        // an edit the dialog is waiting on.
        if self.channel_dialog.as_ref().is_some_and(|d| d.pending) {
            self.channel_dialog_cancel();
        }
        self.update_user_list();
    }

    /// Derive the sidebar's channel list from the wire list, the
    /// per-channel facts and what this session may see (spec §12: a
    /// `min_role` above our rank hides the channel; `MANAGE_CHANNELS`
    /// sees everything).
    fn rebuild_channels(&mut self) {
        let our_rank = self.our_rank();
        let sees_all =
            client::permissions::has(self.permissions, client::permissions::MANAGE_CHANNELS);
        self.channels = self
            .channel_infos
            .iter()
            .map(|c| {
                let facts = self.channel_details.get(&c.id);
                let gate_rank = facts.and_then(|f| f.min_role).and_then(builtin_rank);
                ChannelMeta {
                    id: c.id,
                    name: c.name.clone(),
                    locked: c.has_password,
                    user_limit: c.user_limit,
                    members_only: facts.is_some_and(|f| f.min_role.is_some()),
                    temporary: facts.is_some_and(|f| !f.permanent),
                    is_afk: facts.is_some_and(|f| f.is_afk),
                    owned: facts
                        .and_then(|f| f.owner)
                        .is_some_and(|owner| Some(owner) == self.our_user_id),
                    // Config channels have no owner and stable id 0.
                    from_config: facts.is_none_or(|f| f.stable_id == 0),
                    // A gate we cannot rank (a custom role) is left to
                    // the server: the channel stays visible and
                    // `JoinChannel` says no.
                    hidden: !sees_all && gate_rank.is_some_and(|needed| needed > our_rank),
                }
            })
            .collect();
        let options: Vec<ChannelOption> = self
            .channels
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| ChannelOption {
                id: i32::from(c.id),
                name: c.name.as_str().into(),
            })
            .collect();
        self.channel_options.set_vec(options);
    }

    /// What this session may do, as the bools the UI draws from
    /// (spec §8: the server decides; the client renders).
    fn push_permissions(&self) {
        use client::permissions as p;
        let has = |bit: u64| p::has(self.permissions, bit);
        let s = self.main.global::<State>();
        s.set_perm_mute(has(p::MUTE) || has(p::RESTRICT));
        s.set_perm_disconnect(has(p::DISCONNECT));
        s.set_perm_kick(has(p::KICK));
        s.set_perm_ban(has(p::BAN));
        s.set_perm_move(has(p::MANAGE_CHANNELS));
        s.set_perm_roles(has(p::MANAGE_ROLES));
        s.set_perm_users(has(p::MANAGE_USERS));
        s.set_perm_create_channels(has(p::CREATE_CHANNELS));
        s.set_perm_manage_own(has(p::MANAGE_OWN_CHANNELS));
        s.set_perm_delete_any(has(p::RESTRICT));
        s.set_perm_dm(has(p::DM));
        s.set_our_role(
            self.our_client_id
                .and_then(|id| self.details.get(&id))
                .map_or(0, |(_, roles)| role_of(roles).as_int()),
        );
        s.set_server_persistent(self.server_persistent);
        crate::settingsui::refresh_role_line(self);
    }

    /// Does a message name us — the nickname as a whole word, or
    /// `@nickname`? Case-insensitive.
    fn mentions_us(&self, text: &str) -> bool {
        let nick = self.settings.user.nickname.trim().to_lowercase();
        if nick.is_empty() {
            return false;
        }
        text.to_lowercase()
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@'))
            .any(|word| word.trim_start_matches('@') == nick)
    }

    /// The chat preferences the rows read (Settings → General → Chat).
    pub fn push_ui_prefs(&self) {
        let s = self.main.global::<State>();
        s.set_show_timestamps(self.settings.ui.show_timestamps);
        s.set_compact_rows(self.settings.ui.compact_rows);
    }

    /// Push-to-talk on or off: tell the engine, and the mic button
    /// what to say.
    pub fn apply_push_to_talk(&self) {
        let on = self.settings.audio.push_to_talk;
        self.voice.tx.send(VoiceControl::SetPushToTalk(on)).ok();
        let bind = self.settings.bind_for(BindableAction::PushToTalk);
        let s = self.main.global::<State>();
        s.set_ptt_mode(on);
        s.set_ptt_held(false);
        s.set_ptt_tooltip(
            format!(
                "{} {}",
                t!("ptt-tooltip"),
                if bind.is_empty() { "—" } else { bind }
            )
            .into(),
        );
    }

    /// The byte budget an outgoing image is shrunk to fit: the setting,
    /// never above the wire cap.
    pub fn image_budget(&self) -> usize {
        let wanted = usize::try_from(self.settings.ui.image_max_kb)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024);
        wanted.clamp(64 * 1024, client::MAX_CHAT_IMAGE_BYTES)
    }

    /// The join / leave chime, unless the settings silence it.
    fn play_chime(&self, sound: SoundType) {
        if self.settings.sounds.join_leave_on() {
            self.voice.tx.send(VoiceControl::PlaySound(sound)).ok();
        }
    }

    /// The name the server knows this session by, while connected.
    pub fn session_name(&self) -> Option<&str> {
        if !self.connected {
            return None;
        }
        self.our_client_id
            .and_then(|id| self.users.get(&id))
            .map(|u| u.username.as_str())
    }

    /// Our role badge and level as the roster knows them (for the
    /// profile tab).
    pub fn our_role_and_level(&self) -> (Role, u16) {
        self.our_client_id
            .and_then(|id| self.users.get(&id))
            .map_or((Role::Unknown, 0), |u| (u.role, u.level))
    }

    /// Push the "Text size" setting to every window's Theme (globals
    /// are per window instance).
    pub fn apply_text_scale(&self) {
        let scale = self.settings.ui.text_scale;
        self.main.global::<ui::Theme>().set_scale(scale);
        if let Some(win) = &self.settings_win {
            win.global::<ui::Theme>().set_scale(scale);
        }
        for viewer in self.viewers.values() {
            viewer.win.global::<ui::Theme>().set_scale(scale);
        }
    }

    /// A session's highest built-in rank; a guest's until its details
    /// arrive.
    fn rank_of_client(&self, client_id: u64) -> client::permissions::Rank {
        self.details
            .get(&client_id)
            .map_or(client::permissions::RANK_GUEST, |(_, roles)| {
                roles
                    .iter()
                    .filter_map(|r| builtin_rank(*r))
                    .max()
                    .unwrap_or(client::permissions::RANK_GUEST)
            })
    }

    /// Our highest built-in rank, from our roles; a guest's when the
    /// details have not arrived yet.
    fn our_rank(&self) -> client::permissions::Rank {
        self.our_client_id
            .map_or(client::permissions::RANK_GUEST, |id| {
                self.rank_of_client(id)
            })
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
                self.show_toast(ToastKind::Warning, &t!("channel-full"));
            }
            client::ChannelDenyReason::NoSuchChannel | client::ChannelDenyReason::NotPermitted => {
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
        self.permissions = 0;
        self.our_user_id = None;
        self.server_persistent = true;
        self.details.clear();
        self.pending_admin.clear();
        self.server_muted = None;
        self.text_muted = None;
        self.oldest_history_id = None;
        self.channel_details.clear();
        self.telemetry_notes.clear();
        self.moderation = None;
        self.profiles.clear();
        self.avatar_fetches.clear();
        self.own_avatar_blob = None;
        self.reset_chat_flow();
        self.is_testing_mic = false;
        self.channels.clear();
        self.channel_infos.clear();
        self.channel_options.set_vec(Vec::new());
        self.main.global::<State>().set_moderation_open(false);
        self.push_permissions();
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
        self.play_chime(SoundType::UserLeft);
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
        let first = !self.startup_done;
        self.startup_done = true;
        let stay_hidden = first && self.settings.startup.start_minimized && cfg!(feature = "tray");
        if !stay_hidden {
            self.show_main();
        }
        if first && self.settings.startup.auto_connect {
            let nickname = self.settings.user.nickname.trim().to_string();
            if !nickname.is_empty() {
                self.handle_connect(&nickname);
            }
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  VOICE CONTROLS
    // ════════════════════════════════════════════════════════════════

    fn handle_toggle_mute(&mut self) {
        if self.is_muted && self.server_muted.is_some() {
            self.show_toast(ToastKind::Warning, &t!("chat-server-muted-locked"));
            return;
        }
        self.set_muted(!self.is_muted);
    }

    /// Set the mute state and tell the voice thread, the server and
    /// the UI.
    fn set_muted(&mut self, muted: bool) {
        self.is_muted = muted;
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
                        settings: Box::new(self.settings.clone()),
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
        let raw = self.main.global::<State>().get_draft().to_string();
        let rendered =
            app_core::emoji::replace_shortcodes(&app_core::chat::scrub_control_chars(raw.trim()));
        let text: String = rendered
            .chars()
            .take(app_core::chat::MAX_MESSAGE_CHARS)
            .collect();
        if let Some(edit) = self.editing.take() {
            // Rewriting one of ours: an empty result is a cancel, not
            // a deletion.
            self.main.global::<State>().set_editing(false);
            if !text.is_empty() && self.own_texts.get(&edit.msg_id) != Some(&text) {
                self.network
                    .tx
                    .send(NetworkControl::EditMessage {
                        msg_id: edit.msg_id,
                        text,
                    })
                    .ok();
                self.pending_edit = Some(edit.msg_id);
                self.arm_apply_timeout(edit.msg_id, false);
            }
            self.main.global::<State>().set_draft("".into());
            self.main.invoke_focus_composer();
            return;
        }
        if text.is_empty() {
            return;
        }
        if let Some(rest) = text.strip_prefix('/')
            && let Some((command, args)) = split_command(rest)
        {
            self.handle_slash_command(command, args);
        } else if let Some(to) = self.active_dm {
            self.send_dm(to, text);
            self.main.invoke_chat_pin_bottom();
        } else if let Some(until) = self.text_muted {
            // The server would drop it anyway; say so here instead.
            let until = format!(" {}", describe_until(until));
            self.show_toast(
                ToastKind::Warning,
                &t!("restricted-text", "until" => until.as_str(), "reason" => ""),
            );
            return;
        } else {
            self.network.tx.send(NetworkControl::SendMessage(text)).ok();
            self.main.invoke_chat_pin_bottom();
        }
        self.main.global::<State>().set_draft("".into());
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

    /// The server-side identity behind roster row `index`, as an admin
    /// command names it.
    fn admin_target_at(&self, index: i32) -> Option<AdminTarget> {
        let client_id = self.user_id_at(index)?;
        let (user_id, _) = self.details.get(&client_id)?;
        Some(AdminTarget::User(*user_id))
    }

    /// A moderation entry was picked for the user at row `index`: the
    /// unmutes go straight out; everything else opens the dialog for a
    /// duration and a reason (spec §4.1).
    fn moderate(&mut self, index: i32, action: ModAction) {
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        let Some(target) = self.admin_target_at(index) else {
            return;
        };
        let kind_for = |a: ModAction| match a {
            ModAction::MuteVoice | ModAction::UnmuteVoice => client::RestrictionKind::Voice,
            _ => client::RestrictionKind::GeneralText,
        };
        match action {
            ModAction::UnmuteVoice | ModAction::UnmuteChat => {
                let kind = kind_for(action);
                self.send_admin(AdminCommand::Unmute { target, kind }, PendingAdmin::Show);
                return;
            }
            ModAction::MuteVoice
            | ModAction::MuteChat
            | ModAction::Disconnect
            | ModAction::Kick
            | ModAction::Ban => {}
        }
        let name = self.username_of(client_id);
        let (title, hint, confirm) = match action {
            ModAction::MuteVoice => (
                "mod-title-mute-voice",
                "mod-hint-mute-voice",
                "mod-confirm-mute",
            ),
            ModAction::MuteChat => (
                "mod-title-mute-chat",
                "mod-hint-mute-chat",
                "mod-confirm-mute",
            ),
            ModAction::Disconnect => (
                "mod-title-disconnect",
                "mod-hint-disconnect",
                "mod-confirm-disconnect",
            ),
            ModAction::Kick => ("mod-title-kick", "mod-hint-kick", "mod-confirm-kick"),
            ModAction::Ban | ModAction::UnmuteVoice | ModAction::UnmuteChat => {
                ("mod-title-ban", "mod-hint-ban", "mod-confirm-ban")
            }
        };
        // Durations: a mute defaults to half an hour, a ban to a day;
        // disconnect and kick have none.
        let (durations, default): (&[(&str, Option<u64>)], i32) = match action {
            ModAction::MuteVoice | ModAction::MuteChat => (
                &[
                    ("duration-10m", Some(10 * 60)),
                    ("duration-30m", Some(30 * 60)),
                    ("duration-2h", Some(2 * 3600)),
                    ("duration-24h", Some(24 * 3600)),
                    ("duration-forever", None),
                ],
                1,
            ),
            ModAction::Ban => (
                &[
                    ("duration-24h", Some(24 * 3600)),
                    ("duration-7d", Some(7 * 86_400)),
                    ("duration-30d", Some(30 * 86_400)),
                    ("duration-forever", None),
                ],
                0,
            ),
            _ => (&[], 0),
        };
        let labels: Vec<slint::SharedString> =
            durations.iter().map(|(k, _)| t!(k).into()).collect();
        self.moderation = Some(Moderation {
            action,
            client_id,
            duration_secs: durations.iter().map(|(_, secs)| *secs).collect(),
        });
        let s = self.main.global::<State>();
        s.set_moderation_action(action);
        s.set_moderation_title(t!(title, "name" => name.as_str()).into());
        s.set_moderation_hint(t!(hint).into());
        s.set_moderation_confirm_label(t!(confirm, "name" => name.as_str()).into());
        s.set_moderation_durations(ModelRc::new(VecModel::from(labels)));
        s.set_moderation_duration(default);
        s.set_moderation_reason("".into());
        s.set_moderation_open(true);
    }

    fn confirm_moderation(&mut self) {
        let Some(m) = self.moderation.take() else {
            self.cancel_moderation();
            return;
        };
        let s = self.main.global::<State>();
        let reason = s.get_moderation_reason().trim().to_string();
        let duration_secs = usize::try_from(s.get_moderation_duration())
            .ok()
            .and_then(|i| m.duration_secs.get(i).copied())
            .flatten();
        s.set_moderation_open(false);
        s.set_moderation_reason("".into());
        // The person may have left while the dialog was open.
        let Some((user_id, _)) = self.details.get(&m.client_id) else {
            return;
        };
        let target = AdminTarget::User(*user_id);
        let command = match m.action {
            ModAction::MuteVoice | ModAction::MuteChat => AdminCommand::Restrict {
                target,
                kind: if m.action == ModAction::MuteVoice {
                    client::RestrictionKind::Voice
                } else {
                    client::RestrictionKind::GeneralText
                },
                duration_secs,
                reason,
            },
            ModAction::Disconnect => AdminCommand::Disconnect { target, reason },
            ModAction::Kick => AdminCommand::Kick { target, reason },
            ModAction::Ban => AdminCommand::Ban {
                target,
                duration_secs,
                reason,
            },
            ModAction::UnmuteVoice | ModAction::UnmuteChat => return,
        };
        self.send_admin(command, PendingAdmin::Show);
        self.main.invoke_focus_composer();
    }

    fn cancel_moderation(&mut self) {
        self.moderation = None;
        let s = self.main.global::<State>();
        s.set_moderation_open(false);
        s.set_moderation_reason("".into());
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
                match Viewer::open(
                    client_id,
                    &username,
                    share_w,
                    share_h,
                    app.settings.ui.text_scale,
                ) {
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
        let _ = std::fs::remove_file(client::config::avatar_path());
        self.forget_avatar_blob();
        self.profile_dirty = true;
        self.apply_text_scale();
        self.push_ui_prefs();
        self.apply_push_to_talk();
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
                self.evict_row(0);
            }
        }
        if delta.dropped_front > 0 {
            self.refresh_grouping(0);
        }
        let len = self.history.rows().len();
        for i in (len - delta.appended)..len {
            let row = self.chat_row_at(i);
            self.chat_model.push(row);
        }
    }

    /// A history page for conversation `key`; the active fields hold
    /// that conversation while this runs. Oldest first. A page we
    /// asked for goes in front of the list; the general replay right
    /// after the roster is appended (nothing live precedes it).
    fn take_history_page(
        &mut self,
        key: Option<client::UserId>,
        messages: &[client::HistoryMessage],
        more: bool,
    ) {
        if let Some(first) = messages.first() {
            self.oldest_history_id = Some(first.id);
        }
        if key.is_none() {
            for m in messages {
                if Some(m.user_id) == self.our_user_id {
                    self.own_texts.insert(m.id, m.text.clone());
                }
            }
        }
        let prepared: Vec<(Meta, String, bool)> = messages
            .iter()
            .map(|m| {
                let meta = Meta {
                    id: m.id,
                    ts: format_wall_time(m.ts),
                    ts_ms: m.ts,
                    day: local_day(m.ts),
                    user: m.name.clone(),
                    own: Some(m.user_id) == self.our_user_id,
                    pending: false,
                };
                (meta, app_core::emoji::replace_shortcodes(&m.text), m.edited)
            })
            .collect();
        if key == self.active_dm {
            let s = self.main.global::<State>();
            s.set_history_more(more);
            s.set_history_loading(false);
        }
        self.history_more = more;
        // DM pages are always asked for; the general replay after
        // `Init` is the one page that is appended.
        let page = self.history_page_pending == Some(key) || key.is_some();
        if page {
            if self.history_page_pending == Some(key) {
                self.history_page_pending = None;
            }
            if prepared.is_empty() {
                if key.is_none() {
                    self.push_system_here(&t!("chat-no-earlier"));
                }
                return;
            }
            // Dividers where the day changes within the page, and one
            // before its first message.
            let mut rows = Vec::new();
            let mut day = None;
            for (meta, body, edited) in &prepared {
                if day != Some(meta.day) {
                    rows.push(app_core::chat::ChatItem::DayMarker {
                        label: day_label(meta.day),
                        day: meta.day,
                    });
                    day = Some(meta.day);
                }
                rows.extend(ChatHistory::rows_for(meta, body, *edited));
            }
            let inserted = self.history.prepend_rows(rows);
            self.sync_prepend(inserted);
            // The divider that used to open the list is redundant when
            // the page ends on the same day.
            let redundant = matches!(
                self.history.rows().get(inserted),
                Some(app_core::chat::ChatItem::DayMarker { day: d, .. }) if Some(*d) == day
            );
            if redundant && let Some(delta) = self.history.remove_row(inserted) {
                self.sync_range(&delta);
            }
        } else {
            for (meta, body, edited) in &prepared {
                self.push_message_here(meta, body, *edited);
            }
            if !prepared.is_empty() {
                let line = t!(
                    "chat-history-loaded",
                    "count" => prepared.len(),
                    "more" => if more { "yes" } else { "no" }
                );
                self.push_system_here(&line);
            }
        }
    }

    // ── Conversations (spec §6) ─────────────────────────────────────

    /// Run `f` with conversation `key` in the active fields, swapping
    /// a parked one in and back out around it. The pane keeps showing
    /// the real active conversation: only `switch_conversation` moves
    /// `State.chat`.
    fn with_conversation<R>(
        &mut self,
        key: Option<client::UserId>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if key == self.active_dm {
            return f(self);
        }
        let active = self.active_dm;
        self.swap_in(key);
        let out = f(self);
        self.swap_in(active);
        out
    }

    /// Exchange the active fields with the parked conversation `key`
    /// (created empty if new), parking the current one under its key.
    fn swap_in(&mut self, key: Option<client::UserId>) {
        if key == self.active_dm {
            return;
        }
        let mut target = self.parked.remove(&key).unwrap_or_else(|| Conversation {
            history: ChatHistory::new(),
            model: Rc::new(VecModel::default()),
            oldest_history_id: None,
            more: false,
        });
        std::mem::swap(&mut self.history, &mut target.history);
        std::mem::swap(&mut self.chat_model, &mut target.model);
        std::mem::swap(&mut self.oldest_history_id, &mut target.oldest_history_id);
        std::mem::swap(&mut self.history_more, &mut target.more);
        let old_key = self.active_dm;
        self.parked.insert(old_key, target);
        self.active_dm = key;
    }

    /// Show a conversation in the chat pane: General (`None`) or the
    /// DM with `user`. A DM opens with its history fetched once and
    /// its unread cleared (`MarkRead`).
    fn switch_conversation(&mut self, key: Option<client::UserId>) {
        if key != self.active_dm {
            self.freeze_chat_gifs();
            self.swap_in(key);
            let s = self.main.global::<State>();
            s.set_chat(ModelRc::from(self.chat_model.clone()));
            s.set_history_more(self.history_more);
            s.set_history_loading(false);
            self.history_page_pending = None;
        }
        let s = self.main.global::<State>();
        match key {
            None => {
                s.set_active_dm(-1);
                s.set_conv_name("".into());
                self.general_unread = 0;
                s.set_general_unread(0);
            }
            Some(user) => {
                let index = self.dm_order.iter().position(|u| *u == user);
                s.set_active_dm(index.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1));
                let name = self
                    .dm_meta
                    .get(&user)
                    .map(|m| m.name.clone())
                    .unwrap_or_default();
                s.set_conv_name(name.into());
                let (fetch, mark) = self.dm_meta.get_mut(&user).map_or((false, None), |meta| {
                    let mark = (meta.unread > 0).then_some(meta.last_msg_id);
                    meta.unread = 0;
                    (!meta.fetched && self.server_persistent, mark)
                });
                if fetch {
                    self.load_earlier(50);
                }
                if let Some(msg_id) = mark {
                    self.network
                        .tx
                        .send(NetworkControl::MarkRead {
                            conv: client::ConvRef::Dm(user),
                            msg_id,
                        })
                        .ok();
                }
            }
        }
        self.rebuild_dms_model();
        self.main.invoke_chat_pin_bottom();
        self.main.invoke_focus_composer();
    }

    /// Hide a DM from the list. Nothing is deleted: the server keeps
    /// the conversation, and the next message brings it back.
    fn close_dm(&mut self, user: client::UserId) {
        if self.active_dm == Some(user) {
            self.switch_conversation(None);
        }
        self.parked.remove(&Some(user));
        self.dm_meta.remove(&user);
        self.dm_order.retain(|u| *u != user);
        self.rebuild_dms_model();
    }

    /// Open (or start) the DM with `user`, named `name`.
    pub fn open_dm(&mut self, user: client::UserId, name: &str) {
        let meta = self.dm_meta.entry(user).or_insert_with(|| DmMeta {
            name: name.to_string(),
            unread: 0,
            last_ts: 0,
            last_msg_id: 0,
            // Nothing to fetch without a store.
            fetched: false,
        });
        if meta.name.is_empty() {
            meta.name = name.to_string();
        }
        if !self.dm_order.contains(&user) {
            self.dm_order.insert(0, user);
        }
        self.switch_conversation(Some(user));
    }

    /// The Messages section's rows: newest conversation first, with
    /// unread counts, online rings and the partner's look.
    fn rebuild_dms_model(&mut self) {
        let mut total = 0;
        let rows: Vec<DmRow> = self
            .dm_order
            .iter()
            .filter_map(|user| {
                let meta = self.dm_meta.get(user)?;
                total += meta.unread;
                let client = self
                    .details
                    .iter()
                    .find(|(_, (u, _))| u == user)
                    .map(|(c, _)| *c);
                let name = client
                    .and_then(|c| self.users.get(&c))
                    .map_or(meta.name.clone(), |u| u.username.clone());
                let mut row = DmRow {
                    name: name.as_str().into(),
                    unread: i32::try_from(meta.unread).unwrap_or(i32::MAX),
                    active: self.active_dm == Some(*user),
                    online: client.is_some(),
                    avatar: slint::Image::default(),
                    has_avatar: false,
                    initial: app_core::roster::initial_of(&name).into(),
                    tile_tint: ui::convert::tint(&name),
                };
                if let Some(image) = client
                    .and_then(|c| self.profiles.get(&c))
                    .and_then(|p| p.avatar)
                    .and_then(|blob| self.avatar_images.get(&blob))
                {
                    row.avatar = image.clone();
                    row.has_avatar = true;
                }
                Some(row)
            })
            .collect();
        self.dms_model.set_vec(rows);
        let s = self.main.global::<State>();
        s.set_dm_unread_total(i32::try_from(total).unwrap_or(i32::MAX));
        if let Some(user) = self.active_dm {
            let index = self.dm_order.iter().position(|u| *u == user);
            s.set_active_dm(index.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1));
        }
    }

    /// A live DM arrived (spec §6): into its conversation — shown at
    /// once if open (and marked read), counted as unread otherwise —
    /// the list moves it to the top, the chime plays.
    fn take_direct(&mut self, from: client::UserId, username: &str, id: u64, ts: u64, text: &str) {
        let is_open = self.active_dm == Some(from);
        let meta = self.dm_meta.entry(from).or_insert_with(|| DmMeta {
            name: username.to_string(),
            unread: 0,
            last_ts: 0,
            last_msg_id: 0,
            // Never opened: with a store the page will bring this one
            // too, so it is not pushed twice.
            fetched: false,
        });
        meta.name = username.to_string();
        meta.last_ts = ts;
        meta.last_msg_id = meta.last_msg_id.max(id);
        let show_now = is_open || meta.fetched || !self.server_persistent;
        if !self.server_persistent {
            meta.fetched = true;
        }
        if !is_open {
            meta.unread += 1;
        }
        self.dm_order.retain(|u| *u != from);
        self.dm_order.insert(0, from);
        if show_now {
            let meta = Meta {
                id,
                ts: format_wall_time(ts),
                ts_ms: ts,
                day: local_day(ts),
                user: username.to_string(),
                own: false,
                pending: false,
            };
            let body = app_core::emoji::replace_shortcodes(text);
            self.with_conversation(Some(from), |app| {
                app.push_message_here(&meta, &body, false);
                app.oldest_history_id.get_or_insert(id);
            });
        }
        if is_open {
            self.network
                .tx
                .send(NetworkControl::MarkRead {
                    conv: client::ConvRef::Dm(from),
                    msg_id: id,
                })
                .ok();
        }
        if self.settings.sounds.direct_message_on() {
            self.voice
                .tx
                .send(VoiceControl::PlaySound(SoundType::DirectMessage))
                .ok();
        }
        self.rebuild_dms_model();
    }

    /// Send a DM from the composer: the row appears at once, muted,
    /// under a placeholder id; `MessageAck` confirms it, `MessageFailed`
    /// removes it.
    fn send_dm(&mut self, to: client::UserId, text: String) {
        let client_ref = self.next_client_ref;
        self.next_client_ref = self.next_client_ref.wrapping_add(1).max(1);
        let placeholder = u64::MAX - u64::from(client_ref);
        let now = u64::try_from(chrono::Local::now().timestamp_millis()).unwrap_or(0);
        let meta = Meta {
            id: placeholder,
            ts: format_wall_time(now),
            ts_ms: now,
            day: local_day(now),
            user: self.settings.user.nickname.clone(),
            own: true,
            pending: true,
        };
        let body = app_core::emoji::replace_shortcodes(&text);
        self.push_message_here(&meta, &body, false);
        self.pending_dm_sends
            .insert(client_ref, (Some(to), placeholder));
        if let Some(m) = self.dm_meta.get_mut(&to) {
            m.last_ts = now;
        }
        self.dm_order.retain(|u| *u != to);
        self.dm_order.insert(0, to);
        self.rebuild_dms_model();
        self.network
            .tx
            .send(NetworkControl::SendDm {
                to,
                text,
                client_ref,
            })
            .ok();
    }

    /// The user behind roster row `index`, for "Message …".
    fn dm_from_roster(&mut self, index: i32) {
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        let Some(user_id) = self.details.get(&client_id).map(|(u, _)| *u) else {
            return;
        };
        let name = self.username_of(client_id);
        self.open_dm(user_id, &name);
    }

    /// Mirror a splice (an edit or a deletion) into the model: the
    /// replaced rows go, the new ones take their place, and the rest
    /// of the list — and the scroll position — stay where they were.
    fn sync_range(&mut self, delta: &app_core::chat::RangeDelta) {
        for _ in 0..delta.removed {
            if delta.start < self.chat_model.row_count() {
                self.evict_row(delta.start);
            }
        }
        for offset in 0..delta.inserted {
            let index = delta.start + offset;
            let row = self.chat_row_at(index);
            self.chat_model.insert(index, row);
        }
        self.refresh_grouping(delta.start + delta.inserted);
    }

    /// Re-derive one row's continuation flag after its neighbourhood
    /// changed (a splice, a prepend, a front drop). Only the flag is
    /// touched: GIF/image state keyed by the row id stays intact.
    fn refresh_grouping(&self, index: usize) {
        if let Some(mut row) = self.chat_model.row_data(index) {
            let flag = self.history.is_continuation(index);
            if row.continuation != flag {
                row.continuation = flag;
                self.chat_model.set_row_data(index, row);
            }
        }
    }

    /// Remove model row `index` and free any GIF/image state it owned.
    fn evict_row(&mut self, index: usize) {
        let old = self.chat_model.row_data(index);
        self.chat_model.remove(index);
        if let Some(old) = old {
            self.gifs.remove(&old.row_id);
            self.images.remove(&old.row_id);
        }
    }

    /// The model row for history row `index`, with a fresh row id.
    fn chat_row_at(&mut self, index: usize) -> ChatRow {
        // Truncating u64→i32 is deliberate: ids only need uniqueness
        // within the 500-row window, and 2^31 messages per session is
        // out of scope by ~six orders of magnitude.
        self.next_row_id += 1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let row_id = self.next_row_id as i32;
        let mut row = ui::convert::chat_row(&self.history.rows()[index], row_id);
        row.continuation = self.history.is_continuation(index);
        if row.kind != 1 && row.kind != 4 {
            let own = row.own;
            let name = row.user.to_string();
            self.apply_profile_look(&name, own, &mut row);
        }
        // Message rows carrying WhatsApp-style markup (*bold*,
        // _italic_, `code`) get a pre-parsed StyledText; plain
        // ones skip it and stay mouse-selectable. to_commonmark
        // escapes everything else, so from_markdown can only fail
        // on pathological input — which then renders plain.
        if row.kind == 0 && !row.deleted {
            // Tint matches Theme.mono-text in theme.slint.
            let (cm, styled) =
                app_core::markdown::to_commonmark(row.body.as_str(), Some("#86AA90"));
            if styled && let Ok(st) = slint::StyledText::from_markdown(&cm) {
                row.body_styled = st;
                row.styled = true;
            }
        }
        row
    }

    /// Public mirror-sync for media.rs (image rows).
    pub fn sync_delta(&mut self, delta: &app_core::chat::HistoryDelta) {
        self.sync_history_tail(delta);
    }

    /// A system line into the general chat, whichever conversation
    /// is open (notices are about the room).
    pub fn push_chat_system(&mut self, text: &str) {
        self.with_conversation(None, |app| app.push_system_here(text));
    }

    /// A message into the general chat, whichever conversation is
    /// open: a DM being read must not swallow the room's traffic.
    pub fn push_chat_message(&mut self, meta: &Meta, body: &str, edited: bool) {
        self.with_conversation(None, |app| app.push_message_here(meta, body, edited));
        if self.active_dm.is_some() && !meta.own && !meta.pending {
            self.general_unread += 1;
            self.main
                .global::<State>()
                .set_general_unread(i32::try_from(self.general_unread).unwrap_or(i32::MAX));
        }
    }

    /// Into whatever conversation the active fields hold right now —
    /// the DM paths, and pages already routed by `with_conversation`.
    fn push_system_here(&mut self, text: &str) {
        let delta = self.history.push_system(text);
        self.sync_history_tail(&delta);
    }

    fn push_message_here(&mut self, meta: &Meta, body: &str, edited: bool) {
        self.mark_day(meta.day);
        let delta = self.history.push_message(meta, body, edited);
        self.sync_history_tail(&delta);
    }

    /// A divider before the next message when its day differs from
    /// the last one shown.
    pub fn mark_day(&mut self, day: i64) {
        if self.history.last_day() != Some(day) {
            let delta = self.history.push_day_marker(&day_label(day), day);
            self.sync_history_tail(&delta);
        }
    }

    /// Mirror `inserted` rows put at the front of the history.
    fn sync_prepend(&mut self, inserted: usize) {
        for i in 0..inserted {
            let row = self.chat_row_at(i);
            self.chat_model.insert(i, row);
        }
        self.refresh_grouping(inserted);
    }

    /// Ask the server for the page before the oldest message shown.
    fn load_earlier(&mut self, limit: u16) {
        if self.history_page_pending.is_some() {
            return;
        }
        self.history_page_pending = Some(self.active_dm);
        self.main.global::<State>().set_history_loading(true);
        self.network
            .tx
            .send(NetworkControl::FetchHistory {
                conv: self
                    .active_dm
                    .map_or(client::ConvRef::General, client::ConvRef::Dm),
                before: self.oldest_history_id,
                limit,
            })
            .ok();
    }

    /// "Edit message" on one of ours: the composer starts from its
    /// full text; the draft that was there comes back on cancel.
    fn row_edit(&mut self, msg_id: u64) {
        let Some(text) = self.own_texts.get(&msg_id).cloned() else {
            return;
        };
        let s = self.main.global::<State>();
        let previous_draft = if self.editing.is_some() {
            // Switching from one edit to another keeps the first
            // draft, not the half-edited text.
            self.editing
                .take()
                .map(|e| e.previous_draft)
                .unwrap_or_default()
        } else {
            s.get_draft().to_string()
        };
        self.editing = Some(Editing {
            msg_id,
            previous_draft,
        });
        s.set_draft(text.into());
        s.set_editing(true);
        self.main.invoke_focus_composer();
    }

    fn edit_cancel(&mut self) {
        let s = self.main.global::<State>();
        if let Some(edit) = self.editing.take() {
            s.set_draft(edit.previous_draft.into());
        }
        s.set_editing(false);
        self.main.invoke_focus_composer();
    }

    /// "Delete message": ours through the author path, anyone's
    /// through the admin path when we hold `RESTRICT`; both confirm
    /// first.
    fn row_delete(&mut self, msg_id: u64) {
        let own = self.history.rows().iter().any(|r| match r {
            app_core::chat::ChatItem::Message { meta, .. }
            | app_core::chat::ChatItem::Code { meta, .. } => meta.id == msg_id && meta.own,
            _ => false,
        });
        let by_admin = !own;
        if by_admin && !client::permissions::has(self.permissions, client::permissions::RESTRICT) {
            return;
        }
        self.pending_confirm = Some(Confirm::DeleteMessage { msg_id, by_admin });
        let s = self.main.global::<State>();
        s.set_confirm_title(t!("confirm-delete-message-title").into());
        s.set_confirm_body(
            t!(if by_admin {
                "confirm-delete-message-body-admin"
            } else {
                "confirm-delete-message-body"
            })
            .into(),
        );
        s.set_confirm_label(t!("confirm-delete-message").into());
        s.set_confirm_open(true);
    }

    fn confirm_accept(&mut self) {
        let s = self.main.global::<State>();
        s.set_confirm_open(false);
        match self.pending_confirm.take() {
            Some(Confirm::DeleteMessage { msg_id, by_admin }) => {
                if by_admin {
                    self.send_admin(AdminCommand::DeleteMessage { msg_id }, PendingAdmin::Show);
                } else {
                    self.network
                        .tx
                        .send(NetworkControl::DeleteMessage { msg_id })
                        .ok();
                    self.pending_delete = Some(msg_id);
                    self.arm_apply_timeout(msg_id, true);
                }
            }
            Some(Confirm::DeleteChannel { channel_id }) => {
                self.network
                    .tx
                    .send(NetworkControl::DeleteChannel { channel_id })
                    .ok();
            }
            None => {}
        }
        self.main.invoke_focus_composer();
    }

    fn confirm_cancel(&mut self) {
        self.pending_confirm = None;
        self.main.global::<State>().set_confirm_open(false);
        self.main.invoke_focus_composer();
    }

    /// The server answers an edit or a deletion only by echoing it;
    /// when it refuses (the message left its ring) it says nothing.
    /// After three seconds without the echo, say so here.
    fn arm_apply_timeout(&self, msg_id: u64, delete: bool) {
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        let Some(weak) = weak else {
            return;
        };
        slint::Timer::single_shot(Duration::from_secs(3), move || {
            with_app(&weak, |app| {
                let still = if delete {
                    app.pending_delete.take_if(|id| *id == msg_id).is_some()
                } else {
                    app.pending_edit.take_if(|id| *id == msg_id).is_some()
                };
                if still {
                    app.show_toast(
                        ToastKind::Warning,
                        &t!(if delete {
                            "chat-delete-not-applied"
                        } else {
                            "chat-edit-not-applied"
                        }),
                    );
                }
            });
        });
    }

    /// Forget every in-flight chat interaction (a fresh session, or
    /// none): edits, confirms, paging.
    fn reset_chat_flow(&mut self) {
        self.own_texts.clear();
        self.editing = None;
        self.pending_edit = None;
        self.pending_delete = None;
        self.pending_confirm = None;
        self.channel_dialog = None;
        self.history_page_pending = None;
        self.pending_dm_sends.clear();
        self.parked.clear();
        self.dm_order.clear();
        self.dm_meta.clear();
        self.dms_model.set_vec(Vec::new());
        self.active_dm = None;
        self.history_more = false;
        self.general_unread = 0;
        let s = self.main.global::<State>();
        s.set_general_unread(0);
        s.set_active_dm(-1);
        s.set_conv_name("".into());
        s.set_dm_unread_total(0);
        let s = self.main.global::<State>();
        s.set_editing(false);
        s.set_confirm_open(false);
        s.set_channel_dialog_open(false);
        s.set_history_more(false);
        s.set_history_loading(false);
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

    pub fn update_user_list(&mut self) {
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
        let converted: Vec<UserRow> = items
            .iter()
            .map(|item| {
                let mut row = ui::convert::roster_item(item);
                if let roster::RosterItem::User(user) = item {
                    self.decorate_user_row(&mut row, user);
                }
                row
            })
            .collect();
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

    /// What the roster menu and tooltip need beyond the view model:
    /// the rank rule against us (spec §8) and the session's channel
    /// powers, plus the tooltip text.
    fn decorate_user_row(&self, row: &mut UserRow, user: &roster::RosterRow) {
        use client::permissions as p;
        self.apply_profile_look(&user.name, user.is_us, row);
        row.level_line = self.user_level_line(user).into();
        if user.is_us {
            return;
        }
        let ours = self.our_rank();
        let theirs = self.rank_of_client(user.client_id);
        row.outranked = theirs < ours;
        row.role_editable = theirs <= ours;
        let owns_their_channel = self
            .channels
            .iter()
            .any(|c| c.id == user.channel_id && c.owned);
        row.channel_kickable = p::has(self.permissions, p::MANAGE_CHANNELS)
            || (owns_their_channel && p::has(self.permissions, p::MANAGE_OWN_CHANNELS));
        row.telemetry = self.user_telemetry(user).into();
    }

    /// "Level 3 · Member" for the hover card.
    fn user_level_line(&self, user: &roster::RosterRow) -> String {
        let role = match user.role {
            Role::Guest => Some(t!("role-guest")),
            Role::Member => Some(t!("role-member")),
            Role::Organizer => Some(t!("role-organizer")),
            Role::Admin => Some(t!("role-admin")),
            Role::Unknown => None,
        };
        let level = (user.level > 0).then(|| t!("tooltip-level", "level" => u32::from(user.level)));
        match (level, role) {
            (Some(l), Some(r)) => format!("{l} · {r}"),
            (Some(l), None) => l,
            (None, Some(r)) => r,
            (None, None) => String::new(),
        }
    }

    /// The last telemetry for sessions that may see it — or a note
    /// that none arrived yet; empty for everyone else.
    fn user_telemetry(&self, user: &roster::RosterRow) -> String {
        if !client::permissions::has(self.permissions, client::permissions::MANAGE_USERS)
            || user.is_us
        {
            return String::new();
        }
        match self.telemetry_notes.get(&user.client_id) {
            Some(note) => note.text.clone(),
            None => t!("tooltip-no-telemetry"),
        }
    }

    /// Decode `avatar.png` (if the profile has one) for the rows.
    pub fn reload_own_avatar(&mut self) {
        self.own_avatar = self
            .settings
            .profile
            .avatar
            .then(|| std::fs::read(client::config::avatar_path()).ok())
            .flatten()
            .and_then(|bytes| app_core::images::decode_bounded(&bytes).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                ui::convert::rgba_image(rgba.as_raw(), rgba.width(), rgba.height())
            });
    }

    /// The profile look for a name: ours from the settings, everyone
    /// else's the defaults until the profile wire (1.1.2) fills them.
    /// `UserRow` and `ChatRow` carry the same fields (`ProfileLook`).
    fn apply_profile_look(&self, name: &str, is_us: bool, row: &mut dyn ProfileLook) {
        // The server's copy when it has one (that is what everyone
        // else sees); our settings before the server answered, or
        // when it does not carry profiles.
        let from_server = self
            .users
            .iter()
            .find(|(id, u)| (is_us && Some(**id) == self.our_client_id) || u.username == name)
            .and_then(|(id, _)| self.profiles.get(id));
        let ours = is_us || name == self.settings.user.nickname;
        let (color, style, avatar) = match from_server {
            Some(p) => (
                p.name_color,
                match p.name_style {
                    client::protocol::NameStyle::Normal => client::config::NameStyle::Normal,
                    client::protocol::NameStyle::Bold => client::config::NameStyle::Bold,
                    client::protocol::NameStyle::Italic => client::config::NameStyle::Italic,
                },
                p.avatar
                    .and_then(|blob| self.avatar_images.get(&blob).cloned())
                    .or_else(|| ours.then(|| self.own_avatar.clone()).flatten()),
            ),
            None if ours => (
                self.settings.profile.name_color,
                self.settings.profile.name_style,
                self.own_avatar.clone(),
            ),
            None => (None, client::config::NameStyle::Normal, None),
        };
        if let Some(rgb) = color {
            let [_, r, g, b] = rgb.to_be_bytes();
            row.set_name_color(slint::Color::from_rgb_u8(r, g, b));
        }
        row.set_name_style(match style {
            client::config::NameStyle::Normal => 0,
            client::config::NameStyle::Bold => 1,
            client::config::NameStyle::Italic => 2,
        });
        if let Some(image) = avatar {
            row.set_avatar(image);
        }
    }

    /// Our look as the server should have it (spec §7.1): sent right
    /// after `Session` and on every change. With a picture the blob
    /// goes first; `UploadDone` calls back here with its hash.
    pub fn send_own_profile(&mut self) {
        if !self.connected {
            return;
        }
        if self.settings.profile.avatar && self.own_avatar_blob.is_none() {
            if let Ok(bytes) = std::fs::read(client::config::avatar_path()) {
                self.network
                    .tx
                    .send(NetworkControl::UploadBlob {
                        upload_id: AVATAR_UPLOAD_ID,
                        name: "avatar.png".to_string(),
                        mime: "image/png".to_string(),
                        data: bytes,
                    })
                    .ok();
                return;
            }
        }
        let profile = client::protocol::Profile {
            name_color: self.settings.profile.name_color,
            name_style: match self.settings.profile.name_style {
                client::config::NameStyle::Normal => client::protocol::NameStyle::Normal,
                client::config::NameStyle::Bold => client::protocol::NameStyle::Bold,
                client::config::NameStyle::Italic => client::protocol::NameStyle::Italic,
            },
            avatar: if self.settings.profile.avatar {
                self.own_avatar_blob
            } else {
                None
            },
        };
        self.network
            .tx
            .send(NetworkControl::SetProfile(profile))
            .ok();
    }

    /// A profile changed: every chat row by that author, in every
    /// conversation, takes the new look (the roster rebuilds anyway).
    fn refresh_author_look(&mut self, name: &str) {
        let keys: Vec<Option<client::UserId>> = std::iter::once(self.active_dm)
            .chain(self.parked.keys().copied())
            .collect();
        for key in keys {
            self.with_conversation(key, |app| {
                for i in 0..app.chat_model.row_count() {
                    let Some(mut row) = app.chat_model.row_data(i) else {
                        continue;
                    };
                    if row.kind == 1 || row.kind == 4 || row.user != name {
                        continue;
                    }
                    // Reset before re-applying: a removed picture or
                    // color must clear too.
                    row.has_avatar = false;
                    row.avatar = slint::Image::default();
                    row.has_name_color = false;
                    row.name_style = 0;
                    let own = row.own;
                    app.apply_profile_look(name, own, &mut row);
                    app.chat_model.set_row_data(i, row);
                }
            });
        }
    }

    /// The picture changed locally: the server's hash for it no longer
    /// applies (the next `send_own_profile` uploads the new one).
    pub fn forget_avatar_blob(&mut self) {
        self.own_avatar_blob = None;
    }

    /// Our own look changed in Settings: our rows follow at once.
    pub fn refresh_own_rows(&mut self) {
        let name = self.settings.user.nickname.clone();
        self.refresh_author_look(&name);
    }

    /// Ask the server to rename this session (spec §7.1).
    pub fn request_rename(&mut self, name: String) {
        self.network.tx.send(NetworkControl::SetNickname(name)).ok();
    }

    /// A session's look arrived: keep it, fetch a picture we do not
    /// have (disk cache first), redraw.
    fn take_profile(&mut self, client_id: u64, profile: client::protocol::Profile) {
        if let Some(blob) = profile.avatar
            && !self.avatar_images.contains_key(&blob)
        {
            let cached = client::config::avatar_cache_dir().join(hex(&blob));
            match std::fs::read(&cached) {
                Ok(bytes) => self.take_avatar_bytes(blob, &bytes, false),
                Err(_) => {
                    if self.avatar_fetches.insert(blob) {
                        self.network
                            .tx
                            .send(NetworkControl::FetchBlob { blob_id: blob })
                            .ok();
                    }
                }
            }
        }
        self.profiles.insert(client_id, profile);
        self.update_user_list();
        self.rebuild_dms_model();
        let name = self.username_of(client_id);
        self.refresh_author_look(&name);
    }

    /// Decode a picture into the image map (and onto the disk cache
    /// when it just arrived from the wire).
    fn take_avatar_bytes(&mut self, blob: client::protocol::BlobId, bytes: &[u8], cache: bool) {
        let Ok(decoded) = app_core::images::decode_bounded(bytes) else {
            return;
        };
        let rgba = decoded.to_rgba8();
        let image = ui::convert::rgba_image(rgba.as_raw(), rgba.width(), rgba.height());
        self.avatar_images.insert(blob, image);
        if cache {
            let dir = client::config::avatar_cache_dir();
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = std::fs::write(dir.join(hex(&blob)), bytes);
            }
        }
        self.update_user_list();
        self.rebuild_dms_model();
        // Whoever's picture this is, their rows can show it now.
        let authors: Vec<String> = self
            .profiles
            .iter()
            .filter(|(_, p)| p.avatar == Some(blob))
            .filter_map(|(c, _)| self.users.get(c).map(|u| u.username.clone()))
            .collect();
        for name in authors {
            self.refresh_author_look(&name);
        }
    }

    /// The pointer entered or left an author's picture in chat: the
    /// card wants a roster row, so the name is looked up among the
    /// people online (someone who left gets no card).
    fn author_hovered(&mut self, name: &str, hovering: bool) {
        let found = (0..self.users_model.row_count()).find(|&i| {
            self.users_model
                .row_data(i)
                .is_some_and(|row| !row.is_channel && row.name == name)
        });
        let s = self.main.global::<State>();
        match (hovering, found) {
            (true, Some(index)) => {
                let index = i32::try_from(index).unwrap_or(-1);
                s.set_card_index(index);
                self.user_hovered(index);
            }
            (true, None) => s.set_card_index(-1),
            (false, _) => {
                let shown = usize::try_from(s.get_card_index()).ok();
                if shown == found {
                    s.set_card_index(-1);
                }
            }
        }
    }

    /// The pointer rests on a user row: fetch their last telemetry
    /// for the tooltip when we may, at most once every ten seconds.
    fn user_hovered(&mut self, index: i32) {
        if !client::permissions::has(self.permissions, client::permissions::MANAGE_USERS) {
            return;
        }
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        if Some(client_id) == self.our_client_id {
            return;
        }
        let fresh = self
            .telemetry_notes
            .get(&client_id)
            .is_some_and(|n| n.fetched.elapsed() < Duration::from_secs(10));
        if fresh {
            return;
        }
        let Some((user_id, _)) = self.details.get(&client_id) else {
            return;
        };
        let target = AdminTarget::User(*user_id);
        self.send_admin(
            AdminCommand::LastTelemetry { target },
            PendingAdmin::Telemetry(client_id),
        );
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
        self.show_toast(ToastKind::Error, error);
    }

    /// A transient notice in the toast's register: info for what just
    /// happened, success for good news, warning for something to act
    /// on, error for what failed.
    pub fn show_toast(&mut self, kind: ToastKind, text: &str) {
        let s = self.main.global::<State>();
        s.set_toast_kind(kind);
        s.set_error_text(text.into());
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
        s.set_invite_prompt(false);
        s.set_invite_prompt_text("".into());
        s.set_invite_code("".into());
        s.set_connect_notice("".into());
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
/// What `ChannelDetails` says about a channel (spec §5).
#[derive(Debug, Clone)]
/// What `ChannelDetails` says about a channel beyond `ChannelInfo`.
struct ChannelFacts {
    stable_id: u64,
    owner: Option<client::UserId>,
    min_role: Option<client::RoleId>,
    permanent: bool,
    afk_after_min: Option<u16>,
    is_afk: bool,
}

/// What to do with an `AdminResult` when it arrives.
enum PendingAdmin {
    /// Print the outcome.
    Show,
    /// A roster-tooltip fetch for this session: keep it, say nothing.
    Telemetry(u64),
}

/// The composer is rewriting message `msg_id`; the draft it replaced
/// comes back on cancel.
struct Editing {
    msg_id: u64,
    previous_draft: String,
}

/// What the generic confirm dialog will do.
enum Confirm {
    DeleteMessage { msg_id: u64, by_admin: bool },
    DeleteChannel { channel_id: u16 },
}

/// The channel dialog's subject (spec §5).
struct ChannelDialog {
    /// The wire id being edited; `None` = creating.
    editing: Option<u16>,
    /// The gate the channel had, kept as is while "members only"
    /// stays ticked (it may be a rank above member).
    min_role: Option<client::RoleId>,
    /// A spec went out; the server's list update closes the dialog,
    /// its refusal lands inline.
    pending: bool,
}

/// The message key for a channel refusal.
fn channel_denied_key(reason: client::ChannelUiDenyReason) -> &'static str {
    use client::ChannelUiDenyReason as R;
    match reason {
        R::NameTaken => "channel-denied-name-taken",
        R::NoPermission => "channel-denied-no-permission",
        R::ConfigOwned => "channel-denied-config-owned",
        R::Limit => "channel-denied-limit",
        R::NoSuchChannel => "channel-denied-no-such",
        R::Invalid => "channel-denied-invalid",
    }
}

/// A conversation not currently shown: its rows, its model (kept, so
/// decoded images survive a switch), and its paging cursor.
struct Conversation {
    history: ChatHistory,
    model: Rc<VecModel<ChatRow>>,
    oldest_history_id: Option<u64>,
    more: bool,
}

/// What the Messages section knows about a DM partner.
struct DmMeta {
    name: String,
    unread: u32,
    last_ts: u64,
    last_msg_id: u64,
    /// Its history page has been fetched (or there is no store to
    /// fetch from); until then live messages wait for the page.
    fetched: bool,
}

/// The last telemetry line for a session, formatted for the tooltip.
struct TelemetryNote {
    fetched: Instant,
    text: String,
}

/// The moderation dialog's subject.
struct Moderation {
    action: ModAction,
    client_id: u64,
    /// The seconds behind each entry of `State.moderation-durations`;
    /// `None` = until lifted.
    duration_secs: Vec<Option<u64>>,
}

/// The first bytes of a user id, for log lines and the identity panel.
#[must_use]
pub fn short_id(id: &[u8]) -> String {
    use std::fmt::Write;
    id.iter().take(8).fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Milliseconds since the epoch as the chat's local `HH:MM`.
/// The local calendar day of a server timestamp (days since the epoch
/// in the viewer's zone): what the day dividers compare.
fn local_day(ms: u64) -> i64 {
    use chrono::Datelike;
    let secs = i64::try_from(ms / 1000).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|utc| utc.with_timezone(&chrono::Local).date_naive())
        .map_or(0, |d| i64::from(d.num_days_from_ce()))
}

/// "Today", "Yesterday", or a spelled-out date in the current
/// language ("Thursday, August 27th"; "jueves, 27 de agosto"; the
/// year appended when it is not this one).
fn day_label(day: i64) -> String {
    use chrono::Datelike;
    let today = local_day(u64::try_from(chrono::Local::now().timestamp_millis()).unwrap_or(0));
    if day == today {
        return t!("chat-day-today");
    }
    if day == today - 1 {
        return t!("chat-day-yesterday");
    }
    let Some(date) = i32::try_from(day)
        .ok()
        .and_then(chrono::NaiveDate::from_num_days_from_ce_opt)
    else {
        return String::new();
    };
    let weekday = t!(match date.weekday() {
        chrono::Weekday::Mon => "weekday-mon",
        chrono::Weekday::Tue => "weekday-tue",
        chrono::Weekday::Wed => "weekday-wed",
        chrono::Weekday::Thu => "weekday-thu",
        chrono::Weekday::Fri => "weekday-fri",
        chrono::Weekday::Sat => "weekday-sat",
        chrono::Weekday::Sun => "weekday-sun",
    });
    let month = t!(match date.month() {
        1 => "month-1",
        2 => "month-2",
        3 => "month-3",
        4 => "month-4",
        5 => "month-5",
        6 => "month-6",
        7 => "month-7",
        8 => "month-8",
        9 => "month-9",
        10 => "month-10",
        11 => "month-11",
        _ => "month-12",
    });
    let n = date.day();
    // English ordinals; the other locales' patterns ignore `$suffix`.
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    let this_year = chrono::Local::now().year();
    let key = if date.year() == this_year {
        "chat-day-label"
    } else {
        "chat-day-label-year"
    };
    t!(
        key,
        "weekday" => weekday.as_str(),
        "month" => month.as_str(),
        "day" => n,
        "suffix" => suffix,
        "year" => date.year()
    )
}

fn format_wall_time(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "--:--".to_string())
}

/// `/kick alice spam` → (`kick`, `alice spam`); `None` for a bare `/`.
fn split_command(rest: &str) -> Option<(&str, &str)> {
    let command = rest.split_whitespace().next()?;
    if !command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let args = rest[command.len()..].trim_start();
    Some((command, args))
}

/// A role by name (`member`) or id.
fn parse_role(text: &str) -> Option<client::RoleId> {
    let lower = text.to_ascii_lowercase();
    client::permissions::BUILTIN_ROLES
        .iter()
        .find(|r| r.name == lower)
        .map(|r| r.id)
        .or_else(|| lower.parse().ok())
}

/// A full 64-hex-digit id.
fn parse_hex_id(text: &str) -> Option<client::UserId> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The Fluent key for "you are restricted" per kind.
fn restricted_key(kind: client::RestrictionKind) -> &'static str {
    match kind {
        client::RestrictionKind::Voice => "restricted-voice",
        client::RestrictionKind::GeneralText => "restricted-text",
        client::RestrictionKind::Dm => "restricted-dm",
        client::RestrictionKind::Upload => "restricted-upload",
        client::RestrictionKind::Share => "restricted-share",
        client::RestrictionKind::Profile => "restricted-profile",
        client::RestrictionKind::Server => "restricted-server",
    }
}

fn lifted_key(kind: client::RestrictionKind) -> &'static str {
    match kind {
        client::RestrictionKind::Voice => "lifted-voice",
        client::RestrictionKind::GeneralText => "lifted-text",
        client::RestrictionKind::Dm => "lifted-dm",
        client::RestrictionKind::Upload => "lifted-upload",
        client::RestrictionKind::Share => "lifted-share",
        client::RestrictionKind::Profile => "lifted-profile",
        client::RestrictionKind::Server => "lifted-server",
    }
}

/// "3 min ago", "2 d ago".
fn describe_ago(ts: u64) -> String {
    let ago = wall_now_ms().saturating_sub(ts) / 1000;
    if ago >= 86_400 {
        format!("{} d ago", ago / 86_400)
    } else if ago >= 3_600 {
        format!("{} h ago", ago / 3_600)
    } else {
        format!("{} min ago", ago / 60)
    }
}

/// The built-in role's name, or `role <id>` for a custom one.
/// The five look fields `UserRow` and `ChatRow` share, so one writer
/// serves both.
trait ProfileLook {
    fn set_name_color(&mut self, color: slint::Color);
    fn set_name_style(&mut self, style: i32);
    fn set_avatar(&mut self, image: slint::Image);
}

impl ProfileLook for UserRow {
    fn set_name_color(&mut self, color: slint::Color) {
        self.name_color = color;
        self.has_name_color = true;
    }
    fn set_name_style(&mut self, style: i32) {
        self.name_style = style;
    }
    fn set_avatar(&mut self, image: slint::Image) {
        self.avatar = image;
        self.has_avatar = true;
    }
}

impl ProfileLook for ChatRow {
    fn set_name_color(&mut self, color: slint::Color) {
        self.name_color = color;
        self.has_name_color = true;
    }
    fn set_name_style(&mut self, style: i32) {
        self.name_style = style;
    }
    fn set_avatar(&mut self, image: slint::Image) {
        self.avatar = image;
        self.has_avatar = true;
    }
}

/// The upload id our avatar always travels under (one at a time).
const AVATAR_UPLOAD_ID: u32 = 1;

/// Lowercase hex of a hash, the cache file name.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn profile_denied_key(reason: client::protocol::ProfileDenyReason) -> &'static str {
    use client::protocol::ProfileDenyReason as R;
    match reason {
        R::ReservedColor => "profile-denied-reserved-color",
        R::LowContrast => "profile-denied-low-contrast",
        R::RateLimited => "profile-denied-rate-limited",
        R::Locked => "profile-denied-locked",
        R::BadAvatar => "profile-denied-bad-avatar",
    }
}

fn dm_failed_key(reason: client::DmFailReason) -> &'static str {
    use client::DmFailReason as R;
    match reason {
        R::Offline => "dm-failed-offline",
        R::NoPermission => "dm-failed-no-permission",
        R::NoSuchUser => "dm-failed-no-such-user",
        R::Store => "dm-failed-store",
    }
}

fn rename_denied_key(reason: client::protocol::RenameDenyReason) -> &'static str {
    use client::protocol::RenameDenyReason as R;
    match reason {
        R::Taken => "rename-denied-taken",
        R::Invalid => "rename-denied-invalid",
        R::RateLimited => "rename-denied-rate-limited",
    }
}

/// The roster's badge for a role set: the highest built-in role.
/// Custom roles (`server role`, later) are invisible here until they
/// carry a badge of their own.
fn role_of(roles: &[client::RoleId]) -> Role {
    use client::permissions as p;
    roles
        .iter()
        .map(|r| match *r {
            p::ROLE_ADMIN => Role::Admin,
            p::ROLE_ORGANIZER => Role::Organizer,
            p::ROLE_MEMBER => Role::Member,
            p::ROLE_GUEST => Role::Guest,
            _ => Role::Unknown,
        })
        .max()
        .unwrap_or(Role::Unknown)
}

/// The rank of a built-in role; `None` for a custom one.
fn builtin_rank(role: client::RoleId) -> Option<client::permissions::Rank> {
    client::permissions::BUILTIN_ROLES
        .iter()
        .find(|r| r.id == role)
        .map(|r| r.rank)
}

fn role_label(role: client::RoleId) -> String {
    client::permissions::BUILTIN_ROLES
        .iter()
        .find(|r| r.id == role)
        .map_or_else(|| format!("role {role}"), |r| r.name.to_string())
}

/// A dialog field that is either empty (`Ok(None)`) or a whole number
/// of at least 1; anything else is the error.
fn parse_optional_count(text: &str) -> Result<Option<u16>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u16>()
        .ok()
        .filter(|n| *n >= 1)
        .map(Some)
        .ok_or(())
}

/// `30m`, `1h`, `24h`, `7d` → seconds.
fn parse_duration_secs(text: &str) -> Option<u64> {
    let digits = text.trim_end_matches(char::is_alphabetic);
    let n: u64 = digits.parse().ok()?;
    let per_unit = match &text[digits.len()..] {
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    Some(n.saturating_mul(per_unit))
}

fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// "expires in 3 h" / "never expires", from a wall-clock deadline.
fn describe_expiry(expires_at: Option<u64>) -> String {
    match expires_at {
        None => t!("invite-never-expires"),
        Some(at) => {
            let left = describe_left(at);
            t!("invite-expires-in", "left" => left.as_str())
        }
    }
}

/// "for 2 d" / "indefinitely", for a ban deadline.
fn describe_until(until: Option<u64>) -> String {
    match until {
        None => t!("ban-indefinite"),
        Some(at) => {
            let left = describe_left(at);
            t!("ban-for", "left" => left.as_str())
        }
    }
}

/// The time between now and `at` in the largest sensible unit.
fn describe_left(at: u64) -> String {
    let left = at.saturating_sub(wall_now_ms()) / 1000;
    if left >= 86_400 {
        format!("{} d", left / 86_400)
    } else if left >= 3_600 {
        format!("{} h", left / 3_600)
    } else {
        format!("{} min", (left / 60).max(1))
    }
}

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
    tr.set_server_invite_note(t!("server-invite-note").into());
    tr.set_connect(t!("connect").into());
    tr.set_settings(t!("settings").into());
    tr.set_quit(t!("quit").into());
    tr.set_mute(t!("mute").into());
    tr.set_unmute(t!("unmute").into());
    tr.set_deafen(t!("deafen").into());
    tr.set_undeafen(t!("undeafen").into());
    tr.set_share_screen(t!("share-screen").into());
    tr.set_channel_password_title(t!("channel-password-title").into());
    tr.set_channel_new(t!("channel-new").into());
    tr.set_channel_name(t!("channel-name").into());
    tr.set_channel_name_placeholder(t!("channel-name-placeholder").into());
    tr.set_channel_password_placeholder(t!("channel-password-placeholder").into());
    tr.set_channel_limit(t!("channel-limit").into());
    tr.set_channel_limit_placeholder(t!("channel-limit-placeholder").into());
    tr.set_channel_afk(t!("channel-afk").into());
    tr.set_channel_afk_placeholder(t!("channel-afk-placeholder").into());
    tr.set_channel_keep(t!("channel-keep").into());
    tr.set_channel_keep_hint(t!("channel-keep-hint").into());
    tr.set_channel_members_only(t!("channel-members-only").into());
    tr.set_channel_members_only_hint(t!("channel-members-only-hint").into());
    tr.set_channel_create(t!("channel-create").into());
    tr.set_channel_save(t!("channel-save").into());
    tr.set_channel_not_persistent_note(t!("channel-not-persistent-note").into());
    tr.set_menu_channel_edit(t!("menu-channel-edit").into());
    tr.set_menu_channel_delete(t!("menu-channel-delete").into());
    tr.set_menu_channel_config_owned(t!("menu-channel-config-owned").into());
    tr.set_share_visibility(t!("share-visibility").into());
    tr.set_share_channel_only(t!("share-channel-only").into());
    tr.set_share_everyone(t!("share-everyone").into());
    tr.set_stop_sharing(t!("stop-sharing").into());
    tr.set_disconnect(t!("disconnect").into());
    tr.set_live(t!("live").into());
    tr.set_user_volume(t!("user-volume").into());
    tr.set_watch_stream(t!("watch-stream").into());
    tr.set_reset_audio(t!("reset-audio").into());
    tr.set_menu_volume(t!("menu-volume").into());
    tr.set_menu_mute_for_me(t!("menu-mute-for-me").into());
    tr.set_menu_unmute_for_me(t!("menu-unmute-for-me").into());
    tr.set_menu_role(t!("menu-role").into());
    tr.set_menu_move_to(t!("menu-move-to").into());
    tr.set_menu_channel_kick(t!("menu-channel-kick").into());
    tr.set_menu_mute(t!("menu-mute").into());
    tr.set_menu_mute_voice(t!("menu-mute-voice").into());
    tr.set_menu_mute_chat(t!("menu-mute-chat").into());
    tr.set_menu_unmute_voice(t!("menu-unmute-voice").into());
    tr.set_menu_unmute_chat(t!("menu-unmute-chat").into());
    tr.set_menu_disconnect(t!("menu-disconnect").into());
    tr.set_menu_kick(t!("menu-kick").into());
    tr.set_menu_ban(t!("menu-ban").into());
    tr.set_role_guest(t!("role-guest").into());
    tr.set_role_member(t!("role-member").into());
    tr.set_role_organizer(t!("role-organizer").into());
    tr.set_role_admin(t!("role-admin").into());
    tr.set_moderation_duration(t!("moderation-duration").into());
    tr.set_moderation_reason(t!("moderation-reason").into());
    tr.set_moderation_reason_placeholder(t!("moderation-reason-placeholder").into());
    tr.set_server_not_persistent_note(t!("server-not-persistent-note").into());
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
    tr.set_menu_select_text(t!("menu-select-text").into());
    tr.set_menu_show_formatting(t!("menu-show-formatting").into());
    tr.set_channel_join(t!("channel-join").into());
    tr.set_menu_copy_link(t!("menu-copy-link").into());
    tr.set_menu_open_link(t!("menu-open-link").into());
    tr.set_menu_save_image(t!("menu-save-image").into());
    tr.set_menu_open_image(t!("menu-open-image").into());
    tr.set_chat_edited_mark(t!("chat-edited-mark").into());
    tr.set_chat_deleted_placeholder(t!("chat-deleted-placeholder").into());
    tr.set_menu_edit_message(t!("menu-edit-message").into());
    tr.set_menu_delete_message(t!("menu-delete-message").into());
    tr.set_chat_editing(t!("chat-editing").into());
    tr.set_chat_load_earlier(t!("chat-load-earlier").into());
    tr.set_chat_loading(t!("chat-loading").into());
    tr.set_banner_not_persistent(t!("chat-server-not-persistent").into());
    tr.set_messages_header(t!("messages-header").into());
    tr.set_dm_general(t!("dm-general").into());
    tr.set_dm_direct_message(t!("dm-direct-message").into());
    tr.set_dm_note_memory(t!("dm-note-memory").into());
    tr.set_dm_back(t!("dm-back").into());
    tr.set_chat_placeholder_dm(t!("chat-placeholder-dm").into());
    tr.set_menu_message(t!("menu-message").into());
    tr.set_dm_close(t!("dm-close").into());
    tr.set_monitor(t!("monitor").into());
    tr.set_quality(t!("quality").into());
    tr.set_start_sharing(t!("start-sharing").into());
    tr.set_stop_watching(t!("stop-watching").into());
    tr.set_viewer_fullscreen(t!("viewer-fullscreen").into());
    tr.set_viewer_exit_fullscreen(t!("viewer-exit-fullscreen").into());
    tr.set_settings_title(t!("settings-title").into());
    tr.set_tab_profile(t!("tab-profile").into());
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
    tr.set_invite_code(t!("invite-code").into());
    tr.set_invite_code_placeholder(t!("invite-code-placeholder").into());
    tr.set_server_fingerprint(t!("server-fingerprint").into());
    tr.set_server_config_note(t!("server-config-note").into());
    tr.set_identity_header(t!("identity-header").into());
    tr.set_identity_export(t!("identity-export").into());
    tr.set_identity_import(t!("identity-import").into());
    tr.set_identity_regenerate(t!("identity-regenerate").into());
    tr.set_identity_regenerate_confirm(t!("identity-regenerate-confirm").into());
    tr.set_profile_lead(t!("profile-lead").into());
    tr.set_profile_no_name(t!("profile-no-name").into());
    tr.set_profile_nickname_hint(t!("profile-nickname-hint").into());
    tr.set_profile_name_color(t!("profile-name-color").into());
    tr.set_profile_name_style(t!("profile-name-style").into());
    tr.set_profile_avatar_choose(t!("profile-avatar-choose").into());
    tr.set_profile_avatar_remove(t!("profile-avatar-remove").into());
    tr.set_general_lead(t!("general-lead").into());
    tr.set_text_size(t!("text-size").into());
    tr.set_text_size_hint(t!("text-size-hint").into());
    tr.set_sounds_header(t!("sounds-header").into());
    tr.set_sounds_enabled(t!("sounds-enabled").into());
    tr.set_sounds_enabled_hint(t!("sounds-enabled-hint").into());
    tr.set_sounds_join_leave(t!("sounds-join-leave").into());
    tr.set_sounds_mention(t!("sounds-mention").into());
    tr.set_sounds_mention_hint(t!("sounds-mention-hint").into());
    tr.set_sounds_dm(t!("sounds-dm").into());
    tr.set_startup_header(t!("startup-header").into());
    tr.set_startup_auto_connect(t!("startup-auto-connect").into());
    tr.set_startup_auto_connect_hint(t!("startup-auto-connect-hint").into());
    tr.set_startup_minimized(t!("startup-minimized").into());
    tr.set_startup_minimized_hint(t!("startup-minimized-hint").into());
    tr.set_startup_check_updates(t!("startup-check-updates").into());
    tr.set_startup_check_updates_hint(t!("startup-check-updates-hint").into());
    tr.set_chat_header(t!("chat-header").into());
    tr.set_chat_show_timestamps(t!("chat-show-timestamps").into());
    tr.set_chat_compact_rows(t!("chat-compact-rows").into());
    tr.set_chat_image_size(t!("chat-image-size").into());
    tr.set_chat_image_size_hint(t!("chat-image-size-hint").into());
    tr.set_privacy_header(t!("privacy-header").into());
    tr.set_privacy_note(t!("privacy-note").into());
    tr.set_audio_transmission(t!("audio-transmission").into());
    tr.set_audio_vad_hint(t!("audio-vad-hint").into());
    tr.set_audio_ptt_hint(t!("audio-ptt-hint").into());
    tr.set_audio_ptt_unbound(t!("audio-ptt-unbound").into());
    tr.set_ptt_tooltip(t!("ptt-tooltip").into());
    tr.set_files_header(t!("files-header").into());
    tr.set_about_config_file(t!("about-config-file").into());
    tr.set_files_note(t!("files-note").into());
    tr.set_servers_lead(t!("servers-lead").into());
    tr.set_audio_lead(t!("audio-lead").into());
    tr.set_identity_note(t!("identity-note").into());
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
