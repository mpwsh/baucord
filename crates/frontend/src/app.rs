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
    collections::{BTreeMap, HashMap, HashSet},
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
    menu,
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
    ChannelOption, ChatRow, DmRow, Logic, MainWindow, MenuAction, ModAction, Phase, State,
    ToastKind, Tr, UserRow,
};

use crate::{
    hotkeys::HotkeyManager,
    media::{self, GifPlayer},
    viewers::Viewer,
};

mod alerts;
mod chat;
mod commands;
mod conversations;
mod devices;
/// Compiled-in version of this binary.
// `AppState`'s methods, by concern; each file is a child module so
// nothing here had to become `pub` for them.
mod events;
mod menus;
mod presence;
mod profiles;
mod session;
mod stats;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Slint's Android backend has one window per process (every `Window`
/// component is handed the same window adapter), so the auxiliary
/// windows — Settings, the admin panel, the stream viewer — cannot open
/// there until they become in-window pages (ANDROID-SPEC.md §9). Their
/// `open` paths check this and say so instead of colliding with the
/// main window. An iPhone app is one full-screen window too (winit on
/// `UIKit`, IOS-SPEC.md §4), so iOS takes every page Android has — the
/// Settings and stream pages, the narrow layout, the touch rules.
pub(crate) const SINGLE_WINDOW_HOST: bool = cfg!(any(target_os = "android", target_os = "ios"));

/// Whether the app may end its own process. Not on iOS: winit ignores
/// `ControlFlow::Exit` there (the loop never returns, IOS-SPEC.md
/// §4.1) and Apple's rule is that only the user closes an app, from the
/// app switcher. The Quit button is hidden and `/quit` says how.
pub(crate) const HOST_CAN_QUIT: bool = !cfg!(target_os = "ios");

// ── What the host tells the app (phones) ────────────────────────────────

/// Whether the host shows the app: false while a phone app is in the
/// background (IOS-SPEC.md §14 D42). Voice goes on there; what only
/// feeds the screen — the latency and resource lines, the stream stats,
/// GIF animation, the device poll — waits until the app is shown again,
/// instead of drawing frames nobody sees. The desktop never clears it
/// (`main_on_screen` has its own tray and minimize checks).
static HOST_VISIBLE: AtomicBool = AtomicBool::new(true);

pub(crate) fn host_visible() -> bool {
    HOST_VISIBLE.load(Ordering::Relaxed)
}

/// The host's app went into the background (`false`) or came back to
/// the screen. Callable from any thread.
pub fn set_host_visible(visible: bool) {
    if HOST_VISIBLE.swap(visible, Ordering::Relaxed) == visible {
        return;
    }
    info!("host: app {}", if visible { "shown" } else { "hidden" });
    if visible {
        let _ = slint::invoke_from_event_loop(|| with_current(AppState::host_shown));
    }
}

/// A notification about the DM with `dm` (the user id in hex) was
/// tapped: open that conversation (ANDROID-SPEC.md D52). Any thread.
#[cfg_attr(feature = "desktop", expect(dead_code, reason = "phones only"))]
pub(crate) fn open_noticed_conversation(dm: String) {
    let _ = slint::invoke_from_event_loop(move || {
        with_current(|app| app.open_noticed_dm(&dm));
    });
}

/// The screen edges the host measured itself (logical px: top, left,
/// bottom, right), applied as `State.host-inset-*` (IOS-SPEC.md §14 D39).
static HOST_INSETS: std::sync::Mutex<[f32; 4]> = std::sync::Mutex::new([0.0; 4]);

/// The host's own measure of the edges the system draws over (iOS: the
/// window's safe area), whenever it changes. The pages keep clear of
/// the larger of this and the window's `safe-area-insets`. Callable
/// from any thread, before the app exists too.
pub fn set_host_insets(top: f32, left: f32, bottom: f32, right: f32) {
    if let Ok(mut insets) = HOST_INSETS.lock() {
        *insets = [top, left, bottom, right];
    }
    let _ = slint::invoke_from_event_loop(|| {
        with_current(|app| apply_host_insets(&app.main));
    });
}

fn apply_host_insets(main: &MainWindow) {
    let [top, left, bottom, right] = HOST_INSETS.lock().map_or([0.0; 4], |i| *i);
    let s = main.global::<State>();
    s.set_host_inset_top(top);
    s.set_host_inset_left(left);
    s.set_host_inset_bottom(bottom);
    s.set_host_inset_right(right);
}

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
    pub(crate) network: NetworkThreadHandle,
    voice: VoiceThreadHandle,
    screen: ScreenThreadHandle,
    /// Prepared frames + the forward channel from the present-prep
    /// thread (see present.rs), which owns `screen.event_rx` now.
    present: Arc<crate::present::PresentShared>,
    ui_screen_rx: crossbeam_channel::Receiver<ScreenEvent>,

    // Domain state
    pub(crate) users: HashMap<u64, UserState>,
    our_client_id: Option<u64>,
    /// Server display name (from Init) — prefixes server-originated
    /// system chat lines, same style as the greeting.
    pub server_name: String,
    /// Voice channels this server defines (from Init), in id order —
    /// derived from `channel_infos` + `channel_details` + our rank by
    /// `rebuild_channels`.
    pub(crate) channels: Vec<ChannelMeta>,
    /// The wire list as last received (`Init` / `ChannelListUpdated`).
    channel_infos: Vec<client::ChannelInfo>,
    /// Channel a `JoinChannel` is in flight for (the password prompt's
    /// target; also lets a `WrongPassword` reply reopen the prompt).
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
    pub(crate) details: HashMap<u64, (client::UserId, Vec<client::RoleId>)>,
    /// Admin requests in flight: what to do with the answer.
    pending_admin: HashMap<u32, PendingAdmin>,
    next_request_id: u32,
    /// The last telemetry fetched per session for the roster tooltip
    /// (spec §8), and when, so a hover does not ask twice in ten
    /// seconds.
    telemetry_notes: HashMap<u64, TelemetryNote>,
    /// The moderation dialog's subject while it is open.
    moderation: Option<Moderation>,
    /// What the open context menu is about: its entries were built
    /// for this target, and a pick resolves against it.
    menu_target: Option<MenuTarget>,
    /// The conversation a slash command was typed in: its output
    /// (and the answer to the admin command it sent) lands there —
    /// the admin channel, a DM, or General.
    command_conv: Option<client::UserId>,
    /// The server's role table (`RoleList`, spec §4), the built-ins
    /// alone until it arrives — custom roles are named by it.
    pub(crate) roles: BTreeMap<client::RoleId, client::protocol::RoleInfo>,
    /// File uploads in flight, by upload id (spec §7): where they go
    /// once the server has the bytes.
    pending_uploads: HashMap<u32, PendingUpload>,
    /// Files posted and awaiting the server's ack, by client ref: the
    /// conversation and the placeholder row.
    pending_files: HashMap<u32, (Option<client::UserId>, u64)>,
    /// Blobs fetched for a Save, by hash: the name to write them as,
    /// and the row (conversation, message id) whose card shows it.
    pending_downloads: HashMap<client::protocol::BlobId, PendingDownload>,
    next_file_upload_id: u32,
    /// Transfers shown on cards, by (conversation, message id): the
    /// kind (1 up, 2 down) and percent. Rows are rebuilt freely, so
    /// `chat_row_at` reads this rather than the row carrying it.
    transfers: HashMap<(Option<client::UserId>, u64), (i32, i32)>,
    /// The file upload the media lane is busy with, for its percent.
    uploading: Option<(Option<client::UserId>, u64)>,
    /// An image on its way: the placeholder row's message id, replaced
    /// by our own echo.
    image_upload: Option<u64>,
    /// Pictures going up as files (DMs), by upload id: the bytes, to
    /// show our own card as a picture once the post is acked.
    pending_picture_uploads: HashMap<u32, Vec<u8>>,
    /// Pictures posted as files by others, by blob: the rows waiting
    /// for the bytes to become images.
    pending_pictures: HashMap<client::protocol::BlobId, Vec<(Option<client::UserId>, u64)>>,
    /// Our own picture posts awaiting their ack, by client ref.
    picture_acks: HashMap<u32, Vec<u8>>,
    /// Files saved this session, by blob: the card offers Open.
    downloaded: HashMap<client::protocol::BlobId, std::path::PathBuf>,
    /// The "Move to" submenu's model, rebuilt with the channel list.
    channel_options: Rc<VecModel<ChannelOption>>,
    /// A server voice mute stands (spec §4.2): the mute button is
    /// locked, and `Some(was_muted)` remembers what to restore.
    server_muted: Option<bool>,
    /// A server text mute stands: the composer refuses to send. The
    /// inner value is the restriction's expiry (ms), `None` for one
    /// with no end — the shape `describe_until` reads.
    text_muted: Option<TextMute>,
    /// The oldest history id shown, for paging ("load earlier",
    /// `/history`).
    oldest_history_id: Option<u64>,
    /// The conversation whose `FetchHistory` page is in flight: its
    /// page goes in front of that list; the general replay after
    /// `Init` is appended.
    // The inner `Option` is the conversation key used throughout this
    // file (`None` = General); the outer one is "a page is in flight".
    #[expect(clippy::option_option)]
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
    /// How many were watching our stream at the last stats tick: a
    /// rise plays the viewer-joined sound (`StreamHealth` is the only
    /// word the sharer gets, and it goes to the screen thread).
    stream_viewers_seen: u32,
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
    /// One-shot for `repaint_on_reveal`: set before showing a hidden
    /// main window (tray Show, reconnect paths).
    main_repaint_arm: Rc<Cell<bool>>,
    /// The main window has been placed once (centred at its first
    /// show); later shows leave it where the user put it.
    main_placed: Cell<bool>,
    /// The server runs its default channel as a lobby (spec §12,
    /// 1.1.5): that row and whoever is in it stay out of the roster,
    /// and arrivals read "joined <room>".
    lobby: bool,
    /// The lobby's name (ids renumber on a hot reload; names hold).
    pub(crate) lobby_name: Option<String>,
    /// When a phone last said someone started a call (`alerts`): once
    /// per ten minutes at most.
    last_join_notice: Option<Instant>,
    /// What the voice engine was last told: in a voice room (or testing
    /// the microphone). A phone keeps no microphone open otherwise
    /// (`sync_voice_room`).
    voice_in_room: bool,
    /// The room to go back to after an automatic reconnect: set when
    /// the connection drops, spent by the next `Init`, cleared by a
    /// deliberate connect or disconnect.
    rejoin_channel: Option<String>,
    pub settings_win: Option<ui::SettingsWindow>,
    /// The admin panel (spec §8), created on first open.
    pub(crate) admin_win: Option<ui::AdminWindow>,
    /// The oldest audit id the panel has shown ("Load older" pages
    /// before it).
    pub(crate) admin_audit_oldest: Option<u64>,
    /// The blob cap the server reported, for the Files tab's bar.
    pub(crate) admin_blob_cap: u64,
    /// The panel's action awaiting its confirmation.
    pub(crate) admin_pending: Option<crate::adminui::PanelAction>,
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
    /// The next `show_connect_phase` is a reconnect's pause, not the
    /// end of the session: the host's keep-alive (Android's foreground
    /// service) stays on through it (D34).
    reconnect_hold: bool,
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
    // must not read it), converts or copies the newest frame of each
    // stream into the UI's pixel buffer off the UI thread, forwards
    // non-frame events back via this channel.
    let present_shared = Arc::new(crate::present::PresentShared::default());
    let (screen_fwd_tx, screen_fwd_rx) = crossbeam_channel::unbounded();
    crate::present::spawn(
        screen.event_rx.clone(),
        screen_fwd_tx,
        Arc::clone(&present_shared),
        screen.shared.present.clone(),
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
/// boxes in the emoji palette. Skia renders them all. (Android's
/// backend is Skia already and is installed by the host before `run`;
/// the iOS host selects winit + Skia on Metal itself, IOS-SPEC.md D7 —
/// the CPU raster choice below is a Windows answer that would cost a
/// phone its battery and its 120 Hz.)
#[cfg(not(any(target_os = "android", target_os = "ios")))]
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
    // On Android the host has already installed Slint's Android backend
    // (`slint::android::init`, with Skia), and the iOS host has selected
    // winit + Skia; a second selection would only fail and log.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    select_renderer();
    let is_testing_mic_shared = Arc::new(AtomicBool::new(false));
    let (voice, screen, network, present_shared, screen_fwd_rx) =
        spawn_workers(&is_testing_mic_shared);

    let settings = Settings::load();
    apply_log_level(&settings);

    // Localize before any UI is built. None = follow the OS locale.
    let locale = i18n::init(settings.language.as_deref());
    info!("Locale: {locale}");

    let main = MainWindow::new().expect("failed to create the main window");
    apply_host_insets(&main);

    // Saved size only; the position is decided at the first show
    // (centred on the monitor, see `center_on_monitor`). A phone's
    // window is the screen: nothing to size there.
    // Saved px dimensions; i32→f32 precision is irrelevant at window
    // scale.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[allow(clippy::cast_precision_loss)]
    main.window().set_size(slint::LogicalSize::new(
        settings.window.width.max(500) as f32,
        settings.window.height.max(400) as f32,
    ));

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
        menu_target: None,
        command_conv: None,
        roles: builtin_role_table(),
        pending_uploads: HashMap::new(),
        pending_files: HashMap::new(),
        pending_downloads: HashMap::new(),
        next_file_upload_id: FILE_UPLOAD_BASE,
        transfers: HashMap::new(),
        uploading: None,
        image_upload: None,
        pending_picture_uploads: HashMap::new(),
        pending_pictures: HashMap::new(),
        picture_acks: HashMap::new(),
        downloaded: HashMap::new(),
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
        stream_viewers_seen: 0,
        sharer_ids: HashSet::new(),
        history: ChatHistory::new(),
        chat_model,
        images: HashMap::new(),
        gifs: HashMap::new(),
        pending_preview: None,
        users_model,
        main,
        main_repaint_arm: Rc::new(Cell::new(false)),
        main_placed: Cell::new(false),
        lobby: false,
        lobby_name: None,
        last_join_notice: None,
        voice_in_room: true,
        rejoin_channel: None,
        settings_win: None,
        admin_win: None,
        admin_audit_oldest: None,
        admin_blob_cap: 0,
        admin_pending: None,
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
        reconnect_hold: false,
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
    // A single-window host shows Settings as a page in the main window
    // (ANDROID-SPEC.md D22): its callbacks are wired once here, where
    // the desktop wires its SettingsWindow when that is first created.
    if SINGLE_WINDOW_HOST {
        // One window means a phone: touch, a soft keyboard, Back.
        state.borrow().main.global::<State>().set_touch_host(true);
        crate::settingsui::wire_page(&state.borrow().main);
        // Same for the stream viewer page (D23).
        crate::viewers::wire_page(&state.borrow().main);
    }
    // iOS apps never end themselves (IOS-SPEC.md §14 D29).
    if !HOST_CAN_QUIT {
        state.borrow().main.global::<State>().set_can_quit(false);
    }

    // "Start in the tray": the window stays hidden until the tray icon
    // shows it (only meaningful when the tray is compiled in).
    let start_hidden = {
        let st = state.borrow();
        st.settings.startup.start_minimized && cfg!(feature = "tray")
    };
    if !start_hidden {
        let st = state.borrow();
        st.main.show().expect("failed to show the main window");
        st.place_main_once();
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
        // A phone has no key to hold: push-to-talk there would keep the
        // microphone shut for good (the picker no longer offers it, but
        // a settings file may still say so).
        if SINGLE_WINDOW_HOST {
            st.settings.audio.push_to_talk = false;
        }
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

    // Loop exited (quit_event_loop called). Clean shutdown. (Never
    // reached on iOS: UIKit owns the process and the loop does not
    // return — the host calls `persist_for_background` instead.)
    let st = state;
    APP.with(|a| a.borrow_mut().take());
    st.borrow_mut().shutdown();
}

// ── Callback wiring ─────────────────────────────────────────────────────

/// Convenience: run `f` with a `&mut AppState` if the app still exists.
/// What a dragged user row carries (inside the opaque
/// `slint::DataTransfer`): the client id, nothing else.
struct DragUser(u64);

/// A standing text restriction from the server: `until` is its expiry
/// in ms, `None` when it has none.
#[derive(Clone, Copy)]
struct TextMute {
    until: Option<u64>,
}

/// The log level from the settings, unless `RUST_LOG` is set (then the
/// environment decides and the setting is ignored).
pub(crate) fn apply_log_level(settings: &Settings) {
    if std::env::var_os("RUST_LOG").is_none() {
        log::set_max_level(settings.logging.level_filter());
    }
}

/// Un-minimize and focus a shown window: `show()` alone leaves one
/// that is merely behind others or minimized where it is.
#[cfg(feature = "desktop")]
fn raise_window(win: &slint::Window) {
    use i_slint_backend_winit::WinitWindowAccessor;
    win.with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
}

/// One window, always in front: nothing to raise.
#[cfg(not(feature = "desktop"))]
fn raise_window(_win: &slint::Window) {}

/// Fills the screen on a phone: nothing to centre.
#[cfg(not(feature = "desktop"))]
fn center_on_monitor(_win: &slint::Window) {}

/// Put a shown window in the middle of the monitor it is on (the
/// primary one if winit cannot tell). Physical pixels throughout, so
/// mixed-DPI setups land where they look centred.
#[cfg(feature = "desktop")]
fn center_on_monitor(win: &slint::Window) {
    use i_slint_backend_winit::{winit::dpi::PhysicalPosition, WinitWindowAccessor};
    win.with_winit_window(|w| {
        let Some(monitor) = w.current_monitor().or_else(|| w.primary_monitor()) else {
            return;
        };
        let area = monitor.size();
        let origin = monitor.position();
        let size = w.outer_size();
        let centred = |start: i32, room: u32, extent: u32| {
            let slack = (i64::from(room) - i64::from(extent)).max(0) / 2;
            i32::try_from(i64::from(start) + slack).unwrap_or(start)
        };
        w.set_outer_position(PhysicalPosition::new(
            centred(origin.x, area.width, size.width),
            centred(origin.y, area.height, size.height),
        ));
    });
}

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

/// The host is going into the background, where the OS may end the
/// process without `run` ever returning (iOS always: `UIKit` owns the
/// loop; IOS-SPEC.md §4.3): write the settings now, as the tray's Quit
/// does. Settings are saved on nearly every change already; this
/// catches the rest. UI thread only; a no-op before the app exists.
pub fn persist_for_background() {
    with_current(AppState::save_settings_now);
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
                app.rejoin_channel = None; // a deliberate connect lands in the lobby
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
            if let Some(sf) = crate::settingsui::surface(app) {
                sf.set_tab(crate::settingsui::TAB_SERVERS);
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

    // ── Roster hover (the profile card) ──────────────────────────
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

    // ── Context menus (app-core `menu` builds, the overlay draws) ──
    let w = Rc::downgrade(state);
    logic.on_user_context(move |index, x, y| {
        with_app(&w, |app| app.open_user_menu(index, x, y));
    });
    // Drag a user onto a channel: the payload is our own type inside
    // the opaque transfer, so nothing is parsed on the way back.
    let w = Rc::downgrade(state);
    // The payload is built from the row's own client id, never from
    // its index: the binding is cached per row, and after the roster
    // reshuffles (someone moved) an index-keyed payload would carry
    // whoever sat in that slot before.
    logic.on_user_drag_data(move |lo, hi| {
        let _ = &w;
        let mut data = slint::DataTransfer::default();
        let id = roster::join_id(lo, hi);
        if id != 0 {
            data.set_user_data(Rc::new(DragUser(id)));
        }
        data
    });
    let w = Rc::downgrade(state);
    logic.on_can_drop_user(move |data, channel_index| {
        w.upgrade()
            .is_some_and(|rc| rc.borrow().drop_target(&data, channel_index).is_some())
    });
    let w = Rc::downgrade(state);
    logic.on_drop_user(move |data, channel_index| {
        with_app(&w, |app| {
            if let Some((client_id, channel_id)) = app.drop_target(&data, channel_index) {
                app.move_user_to(client_id, channel_id);
            }
        });
    });
    // The LIVE chip: one click opens the sharer's viewer (or brings
    // it forward), no menu needed.
    let w = Rc::downgrade(state);
    logic.on_watch_user(move |index| {
        with_app(&w, |app| {
            if let Some(id) = app.user_id_at(index)
                && Some(id) != app.our_client_id
            {
                app.handle_watch_peer(id);
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_row_context(move |row, selecting, x, y| {
        with_app(&w, |app| app.open_row_menu(&row, selecting, x, y));
    });
    let w = Rc::downgrade(state);
    logic.on_composer_context(move |x, y| {
        with_app(&w, |app| {
            app.show_menu(menu::composer_menu(), MenuTarget::Composer, x, y);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_menu_activated(move |action, arg| {
        with_app(&w, |app| app.menu_activated(action, arg));
    });
    let w = Rc::downgrade(state);
    logic.on_menu_volume_changed(move |volume| {
        with_app(&w, |app| app.menu_volume(volume, false));
    });
    let w = Rc::downgrade(state);
    logic.on_menu_volume_released(move |volume| {
        with_app(&w, |app| app.menu_volume(volume, true));
    });
    let w = Rc::downgrade(state);
    logic.on_menu_closed(move || with_app(&w, AppState::close_menu));

    let w = Rc::downgrade(state);
    logic.on_dm_clicked(move |index| {
        with_app(&w, |app| {
            if let Some(user) = usize::try_from(index)
                .ok()
                .and_then(|i| app.dm_order.get(i).copied())
            {
                if user == ADMIN_CHANNEL_KEY {
                    app.open_admin_channel();
                } else {
                    app.switch_conversation(Some(user));
                }
            }
        });
    });
    let w = Rc::downgrade(state);
    logic.on_general_clicked(move || {
        with_app(&w, |app| app.switch_conversation(None));
    });
    let w = Rc::downgrade(state);
    logic.on_messages_toggle(move || with_app(&w, AppState::toggle_messages));
    let w = Rc::downgrade(state);
    logic.on_sidebar_resized(move |width| with_app(&w, |app| app.sidebar_resized(width)));
    let w = Rc::downgrade(state);
    logic.on_messages_resized(move |h| with_app(&w, |app| app.messages_resized(h)));
    let w = Rc::downgrade(state);
    logic.on_file_open(move |row_id| with_app(&w, |app| app.file_open(row_id)));
    let w = Rc::downgrade(state);
    logic.on_file_cancel(move |row_id| with_app(&w, |app| app.file_cancel(row_id)));
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
        if let Err(e) = crate::host::open_url(url.as_str()) {
            warn!("Failed to open link: {e}");
        }
    });
    let w = Rc::downgrade(state);
    logic.on_file_save(move |row_id| with_app(&w, |app| app.file_save(row_id)));
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
    logic.on_viewer_copy(move || {
        with_app(&w, |app| {
            if let Some(id) = app.gallery_row {
                media::copy_image(app, id);
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
                if let Some(TextMute { until }) = app.text_muted {
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
                } else if app.active_dm.is_some() || app.server_persistent {
                    // A picture is a file with a picture inside (spec §7):
                    // uploaded on the blob path, shown as a picture on
                    // both ends once it lands. Only a memory-mode server,
                    // which keeps no files, still gets the live relay.
                    app.pending_picture_uploads
                        .insert(app.next_file_upload_id, png.clone());
                    app.begin_file_upload("picture.png".into(), "image/png".into(), png);
                } else {
                    app.send_image_with_card(png);
                }
            }
            app.preview_gif = None;
            app.close_preview();
        });
    });
    let w = Rc::downgrade(state);
    logic.on_preview_cancel(move || {
        with_app(&w, |app| {
            app.pending_preview = None;
            app.preview_gif = None;
            app.close_preview();
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
    logic.on_invite_create(move || with_app(&w, AppState::open_invite_dialog));
    let w = Rc::downgrade(state);
    logic.on_admin_open(move || {
        with_app(&w, |app| {
            crate::adminui::open(app, crate::adminui::TAB_USERS);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_admin_channel_open(move || with_app(&w, AppState::open_admin_channel));
    let w = Rc::downgrade(state);
    logic.on_invite_dialog_create(move || with_app(&w, AppState::invite_dialog_create));
    let w = Rc::downgrade(state);
    logic.on_invite_dialog_copy(move || with_app(&w, AppState::invite_dialog_copy));
    let w = Rc::downgrade(state);
    logic.on_invite_dialog_cancel(move || {
        with_app(&w, |app| {
            app.main.global::<State>().set_invite_dialog_open(false);
        });
    });
    let w = Rc::downgrade(state);
    logic.on_channel_create(move || with_app(&w, AppState::channel_create));
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
/// unlike the earlier `is_minimized()` polling (lost fast
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
///
/// No raw window events without winit: no reveal glitches to paper
/// over, and no OS file drags either.
#[cfg(not(feature = "desktop"))]
pub fn repaint_on_reveal(_win: &slint::Window, _armed: Rc<Cell<bool>>, _flip: impl Fn() + 'static) {
}

#[cfg(feature = "desktop")]
pub fn repaint_on_reveal(win: &slint::Window, armed: Rc<Cell<bool>>, flip: impl Fn() + 'static) {
    use i_slint_backend_winit::WinitWindowAccessor;
    win.on_winit_window_event(move |_w, ev| {
        use i_slint_backend_winit::winit::event::WindowEvent;
        // OS drag-and-drop of files onto the window (Slint's DropArea
        // does not see external drops in 1.17; winit does). The hook
        // runs inside winit's dispatch, so everything is deferred to
        // the event loop: a hover shows the overlay, a drop feeds the
        // path down the paste chain's road.
        match ev {
            WindowEvent::HoveredFile(_) => set_drop_hover(true),
            WindowEvent::HoveredFileCancelled => set_drop_hover(false),
            WindowEvent::DroppedFile(path) => {
                let path = path.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    with_current(move |app| {
                        app.main.global::<State>().set_drop_hover(false);
                        media::drop_path(app, &path);
                    });
                });
            }
            _ => {}
        }
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

/// The "drop to send" overlay, from the winit hook (deferred). Only
/// the desktop's winit hook drops files on the window.
#[cfg(feature = "desktop")]
fn set_drop_hover(on: bool) {
    let _ = slint::invoke_from_event_loop(move || {
        with_current(move |app| app.main.global::<State>().set_drop_hover(on));
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
            // In the background (a phone) nobody reads the notices; the
            // first poll back on screen reports whatever changed. The
            // voice engine follows device loss on its own meanwhile.
            if !host_visible() {
                return;
            }
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
        Duration::from_secs(2),
        move || {
            with_app(&w, AppState::sample_resources);
        },
    );
    timers.push(t);

    state.borrow_mut().timers = timers;
}

// ── The drain (worker → UI) ─────────────────────────────────────────────

/// What `ChannelDetails` says about a channel beyond `ChannelInfo`.
#[derive(Debug, Clone)]
struct ChannelFacts {
    stable_id: u64,
    owner: Option<client::UserId>,
    min_role: Option<client::RoleId>,
    permanent: bool,
    afk_after_min: Option<u16>,
    is_afk: bool,
}

/// What to do with an `AdminResult` when it arrives.
pub(crate) enum PendingAdmin {
    /// Print the outcome.
    Show,
    /// A roster-tooltip fetch for this session: keep it, say nothing.
    Telemetry(u64),
    /// The admin panel's tab asked: fill its list.
    Panel(i32),
    /// The panel's Audit tab asked for an older page: append.
    AuditMore,
    /// The admin channel opened: the last page becomes its history.
    Channel,
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

/// What a DM brought: text, or a file card (spec §7).
enum Incoming<'a> {
    Text(&'a str),
    File(app_core::chat::FileMeta),
}

/// What the open context menu is about (`AppState::show_menu`): the
/// entries were built for it, and a pick resolves against it.
enum MenuTarget {
    /// A roster user row, by index into the users model.
    User(i32),
    Channel(u16),
    /// A chat row: its row id (images, the plain-view toggle), the
    /// server's message id (edit, delete) and the body (copy).
    Message {
        row_id: i32,
        msg_id: u64,
        body: String,
    },
    Composer,
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

/// `14:06` today, `Sep 6 14:06` on another day (the audit table).
pub(crate) fn format_wall_stamp(ms: u64) -> String {
    let Some(t) = chrono::DateTime::from_timestamp_millis(i64::try_from(ms).unwrap_or(0)) else {
        return String::new();
    };
    let local = t.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%b %-d %H:%M").to_string()
    }
}

pub(crate) fn format_wall_time(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0).map_or_else(
        || "--:--".to_string(),
        |utc| {
            utc.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        },
    )
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

/// A full 64-hex-digit id.
pub(crate) fn parse_hex_id(text: &str) -> Option<client::UserId> {
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
/// "3 d 4 h", "12 min", "45 s": an uptime.
pub(crate) fn describe_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs / 3_600) % 24, (secs / 60) % 60);
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        format!("{m} min")
    } else {
        format!("{secs} s")
    }
}

pub(crate) fn describe_ago(ts: u64) -> String {
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
/// File uploads take ids from here up (spec §7); the avatar keeps 1.
const FILE_UPLOAD_BASE: u32 = 1_000;

/// The admin channel (spec §8) lives in the Messages section as a
/// conversation under a reserved key: no user has this id, so it parks
/// and swaps like a DM while the composer only takes commands there.
pub(crate) const ADMIN_CHANNEL_KEY: client::UserId = [0xFF; 32];

/// A file on its way to the server (spec §7); the card already
/// carries its type and size.
struct PendingUpload {
    /// The conversation open when it was picked.
    conv: Option<client::UserId>,
    name: String,
    /// The pending card's placeholder message id.
    placeholder: u64,
}

/// A card's Save in flight.
struct PendingDownload {
    name: String,
    conv: Option<client::UserId>,
    msg_id: u64,
}

/// Lowercase hex of a hash, the cache file name.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
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
        R::NoSuchBlob => "dm-failed-no-such-blob",
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

/// How far into `level` a total of `xp` is (spec §9.1): the percent
/// for the card's bar and its caption. `None` until the server said.
fn xp_progress(level: u16, xp: Option<u64>) -> Option<(i32, String)> {
    let xp = xp?;
    if level == 0 {
        return None;
    }
    let have = xp.saturating_sub(client::protocol::xp_for_level(level));
    let need = client::protocol::cost_of_level(level).max(1);
    let have = have.min(need);
    let pct = i32::try_from(have * 100 / need).unwrap_or(100);
    let line = t!(
        "card-xp-line",
        "have" => have,
        "need" => need,
        "next" => u32::from(level) + 1
    );
    Some((pct, line))
}

/// One audit entry as a chat line: "14:06 · mpw kicked bob: spam".
fn audit_line_text(e: &client::protocol::AuditEntry) -> String {
    let actor = e.actor.clone().unwrap_or_else(|| t!("admin-actor-server"));
    let mut line = format!(
        "{} · {actor} {}",
        format_wall_time(e.ts),
        crate::adminui::audit_action_text(&e.action)
    );
    if let Some(t) = e.target.as_deref().filter(|t| !t.is_empty()) {
        line.push(' ');
        line.push_str(t);
    }
    if let Some(d) = e.detail.as_deref().filter(|d| !d.is_empty()) {
        line.push_str(": ");
        line.push_str(d);
    }
    line
}

/// Where a rank stands, in words (the panel's Roles tab).
pub(crate) fn describe_standing(rank: u16) -> String {
    use client::permissions as p;
    t!(if rank >= p::RANK_ADMIN {
        "admin-standing-admin"
    } else if rank == p::RANK_ORGANIZER {
        "role-organizer"
    } else if rank > p::RANK_ORGANIZER {
        "admin-standing-above-organizers"
    } else if rank == p::RANK_MEMBER {
        "role-member"
    } else if rank > p::RANK_MEMBER {
        "admin-standing-above-members"
    } else if rank == p::RANK_GUEST {
        "role-guest"
    } else {
        "admin-standing-above-guests"
    })
}

/// A built-in role's name, for the card and the menus.
fn role_name(role: Role) -> String {
    match role {
        Role::Guest => t!("role-guest"),
        Role::Member => t!("role-member"),
        Role::Organizer => t!("role-organizer"),
        Role::Admin => t!("role-admin"),
        Role::Unknown => String::new(),
    }
}

/// A menu label as the catalog says it (app-core keeps menus free of
/// localization).
fn menu_label(label: &menu::Label) -> slint::SharedString {
    use menu::Label as L;
    let key = match label {
        L::Text(text) => return text.as_str().into(),
        L::RoleName(role) => return role_name(*role).into(),
        L::ChannelConfigOwned => "menu-channel-config-owned",
        L::ChannelEdit => "menu-channel-edit",
        L::ChannelDelete => "menu-channel-delete",
        L::Volume => "menu-volume",
        L::MuteForMe => "menu-mute-for-me",
        L::UnmuteForMe => "menu-unmute-for-me",
        L::ResetAudio => "reset-audio",
        L::WatchStream => "watch-stream",
        L::Message => "menu-message",
        L::Role => "menu-role",
        L::MoveTo => "menu-move-to",
        L::ChannelKick => "menu-channel-kick",
        L::Mute => "menu-mute",
        L::MuteVoice => "menu-mute-voice",
        L::MuteChat => "menu-mute-chat",
        L::UnmuteVoice => "menu-unmute-voice",
        L::UnmuteChat => "menu-unmute-chat",
        L::Disconnect => "menu-disconnect",
        L::Kick => "menu-kick",
        L::Ban => "menu-ban",
        L::CopyMessage => "menu-copy-message",
        L::Copy => "menu-copy",
        L::SelectText => "menu-select-text",
        L::ShowFormatting => "menu-show-formatting",
        L::EditMessage => "menu-edit-message",
        L::DeleteMessage => "menu-delete-message",
        L::OpenImage => "menu-open-image",
        L::SaveImage => "menu-save-image",
        L::CopyImage => "menu-copy-image",
        L::SaveFile => "menu-save-file",
        L::Cut => "menu-cut",
        L::Paste => "menu-paste",
    };
    t!(key).into()
}

/// The four built-ins as a role table, for before a server sends
/// its own (or a server too old to).
fn builtin_role_table() -> BTreeMap<client::RoleId, client::protocol::RoleInfo> {
    client::permissions::BUILTIN_ROLES
        .iter()
        .map(|r| {
            (
                r.id,
                client::protocol::RoleInfo {
                    id: r.id,
                    name: r.name.to_string(),
                    rank: r.rank,
                    permissions: r.permissions,
                    builtin: true,
                    icon: String::new(),
                    color: 0,
                },
            )
        })
        .collect()
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
pub(crate) fn describe_expiry(expires_at: Option<u64>) -> String {
    match expires_at {
        None => t!("invite-never-expires"),
        Some(at) => {
            let left = describe_left(at);
            t!("invite-expires-in", "left" => left.as_str())
        }
    }
}

/// "for 2 d" / "indefinitely", for a ban deadline.
pub(crate) fn describe_until(until: Option<u64>) -> String {
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
    tr.set_share_visibility(t!("share-visibility").into());
    tr.set_share_channel_only(t!("share-channel-only").into());
    tr.set_share_everyone(t!("share-everyone").into());
    tr.set_stop_sharing(t!("stop-sharing").into());
    tr.set_disconnect(t!("disconnect").into());
    tr.set_live(t!("live").into());
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
    tr.set_channel_join(t!("channel-join").into());
    tr.set_menu_save_image(t!("menu-save-image").into());
    tr.set_menu_copy_image(t!("menu-copy-image").into());
    tr.set_lobby_banner(t!("lobby-banner").into());
    tr.set_drop_to_send(t!("drop-to-send").into());
    tr.set_lobby_mic_hint(t!("lobby-mic-hint").into());
    tr.set_chat_edited_mark(t!("chat-edited-mark").into());
    tr.set_chat_deleted_placeholder(t!("chat-deleted-placeholder").into());
    tr.set_chat_editing(t!("chat-editing").into());
    tr.set_chat_load_earlier(t!("chat-load-earlier").into());
    tr.set_chat_loading(t!("chat-loading").into());
    tr.set_banner_not_persistent(t!("chat-server-not-persistent").into());
    tr.set_messages_header(t!("messages-header").into());
    tr.set_voice_header(t!("voice-header").into());
    tr.set_messages_collapse(t!("messages-collapse").into());
    tr.set_messages_expand(t!("messages-expand").into());
    tr.set_invite_button(t!("invite-button").into());
    tr.set_card_offline(t!("card-offline").into());
    tr.set_invite_dialog_title(t!("invite-dialog-title").into());
    tr.set_invite_role(t!("invite-role").into());
    tr.set_invite_expiry(t!("invite-expiry").into());
    tr.set_invite_uses_label(t!("invite-uses-label").into());
    tr.set_invite_create(t!("invite-create").into());
    tr.set_invite_copy(t!("invite-copy").into());
    tr.set_invite_close(t!("invite-close").into());
    tr.set_file_save(t!("file-save").into());
    tr.set_file_open(t!("file-open").into());
    tr.set_file_save_again(t!("file-save-again").into());
    tr.set_file_cancel(t!("file-cancel").into());
    tr.set_file_uploading_pct(t!("file-uploading-pct").into());
    tr.set_file_downloading_pct(t!("file-downloading-pct").into());
    tr.set_admin_title(t!("admin-title").into());
    tr.set_admin_button(t!("admin-button").into());
    tr.set_admin_refresh(t!("admin-refresh").into());
    tr.set_admin_loading(t!("admin-loading").into());
    tr.set_admin_none(t!("admin-none").into());
    tr.set_admin_users(t!("admin-users").into());
    tr.set_admin_users_lead(t!("admin-users-lead").into());
    tr.set_admin_reason(t!("admin-reason").into());
    tr.set_admin_reason_placeholder(t!("admin-reason-placeholder").into());
    tr.set_admin_disconnect(t!("admin-disconnect").into());
    tr.set_admin_kick(t!("admin-kick").into());
    tr.set_admin_ban(t!("admin-ban").into());
    tr.set_admin_reset_profile(t!("admin-reset-profile").into());
    tr.set_admin_purge(t!("admin-purge").into());
    tr.set_admin_set_role(t!("admin-set-role").into());
    tr.set_admin_roles(t!("admin-roles").into());
    tr.set_admin_roles_lead(t!("admin-roles-lead").into());
    tr.set_admin_holders(t!("admin-holders").into());
    tr.set_admin_builtin(t!("admin-builtin").into());
    tr.set_admin_edit(t!("admin-edit").into());
    tr.set_admin_delete(t!("admin-delete").into());
    tr.set_admin_role_new(t!("admin-role-new").into());
    tr.set_admin_role_editing(t!("admin-role-editing").into());
    tr.set_admin_role_name(t!("admin-role-name").into());
    tr.set_admin_role_name_hint(t!("admin-role-name-hint").into());
    tr.set_admin_role_rank_hint(t!("admin-role-rank-hint").into());
    tr.set_admin_role_save(t!("admin-role-save").into());
    tr.set_admin_restrictions(t!("admin-restrictions").into());
    tr.set_admin_restrictions_lead(t!("admin-restrictions-lead").into());
    tr.set_admin_lift(t!("admin-lift").into());
    tr.set_admin_invites(t!("admin-invites").into());
    tr.set_admin_invites_lead(t!("admin-invites-lead").into());
    tr.set_admin_revoke(t!("admin-revoke").into());
    tr.set_admin_revoked(t!("admin-revoked").into());
    tr.set_admin_channels(t!("admin-channels").into());
    tr.set_admin_channels_lead(t!("admin-channels-lead").into());
    tr.set_admin_members(t!("admin-members").into());
    tr.set_admin_config_owned(t!("admin-config-owned").into());
    tr.set_admin_files(t!("admin-files").into());
    tr.set_admin_files_lead(t!("admin-files-lead").into());
    tr.set_admin_refs(t!("admin-refs").into());
    tr.set_admin_audit(t!("admin-audit").into());
    tr.set_admin_audit_lead(t!("admin-audit-lead").into());
    tr.set_admin_audit_more(t!("admin-audit-more").into());
    tr.set_admin_server(t!("admin-server").into());
    tr.set_admin_server_lead(t!("admin-server-lead").into());
    tr.set_admin_stat_version(t!("admin-stat-version").into());
    tr.set_admin_stat_uptime(t!("admin-stat-uptime").into());
    tr.set_admin_stat_online(t!("admin-stat-online").into());
    tr.set_admin_stat_users(t!("admin-stat-users").into());
    tr.set_admin_stat_messages(t!("admin-stat-messages").into());
    tr.set_admin_stat_blobs(t!("admin-stat-blobs").into());
    tr.set_admin_stat_persistence(t!("admin-stat-persistence").into());
    tr.set_admin_stat_queue(t!("admin-stat-queue").into());
    tr.set_admin_stat_dropped(t!("admin-stat-dropped").into());
    tr.set_admin_stat_errors(t!("admin-stat-errors").into());
    tr.set_admin_yours(t!("admin-yours").into());
    tr.set_admin_role_badge(t!("admin-role-badge").into());
    tr.set_admin_col_when(t!("admin-col-when").into());
    tr.set_admin_col_who(t!("admin-col-who").into());
    tr.set_admin_col_what(t!("admin-col-what").into());
    tr.set_admin_col_target(t!("admin-col-target").into());
    tr.set_admin_col_detail(t!("admin-col-detail").into());
    tr.set_invite_share_hint(t!("invite-share-hint").into());
    tr.set_invite_expiry_note(t!("invite-expiry-note").into());
    tr.set_dm_general(t!("dm-general").into());
    tr.set_dm_direct_message(t!("dm-direct-message").into());
    tr.set_dm_note_memory(t!("dm-note-memory").into());
    tr.set_reset_confirm_body(t!("reset-confirm-body").into());
    tr.set_reset_confirm(t!("reset-confirm").into());
    tr.set_chat_empty_title(t!("chat-empty-title").into());
    tr.set_chat_empty_general(t!("chat-empty-general").into());
    tr.set_chat_empty_dm(t!("chat-empty-dm").into());
    tr.set_dm_back(t!("dm-back").into());
    tr.set_chat_placeholder_dm(t!("chat-placeholder-dm").into());
    tr.set_monitor(t!("monitor").into());
    tr.set_quality(t!("quality").into());
    tr.set_start_sharing(t!("start-sharing").into());
    tr.set_stop_watching(t!("stop-watching").into());
    tr.set_viewer_fullscreen(t!("viewer-fullscreen").into());
    tr.set_viewer_exit_fullscreen(t!("viewer-exit-fullscreen").into());
    tr.set_viewer_fill(t!("viewer-fill").into());
    tr.set_viewer_fit(t!("viewer-fit").into());
    tr.set_viewer_rotate(t!("viewer-rotate").into());
    tr.set_viewer_unrotate(t!("viewer-unrotate").into());
    tr.set_viewer_mic_mute(t!("viewer-mic-mute").into());
    tr.set_viewer_mic_unmute(t!("viewer-mic-unmute").into());
    tr.set_viewer_stream_mute(t!("viewer-stream-mute").into());
    tr.set_viewer_stream_unmute(t!("viewer-stream-unmute").into());
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
    tr.set_tab_debug(t!("tab-debug").into());
    tr.set_debug_lead(t!("debug-lead").into());
    tr.set_debug_log_header(t!("debug-log-header").into());
    tr.set_debug_send_log(t!("debug-send-log").into());
    tr.set_debug_copy_log(t!("debug-copy-log").into());
    tr.set_debug_clear_log(t!("debug-clear-log").into());
    tr.set_debug_send_hint(t!("debug-send-hint").into());
    tr.set_debug_tail_label(t!("debug-tail-label").into());
    tr.set_debug_tail_empty(t!("debug-tail-empty").into());
    tr.set_debug_no_log_file(t!("debug-no-log-file").into());
    tr.set_log_level(t!("log-level").into());
    tr.set_log_level_hint(t!("log-level-hint").into());
    tr.set_sounds_header(t!("sounds-header").into());
    tr.set_sounds_enabled(t!("sounds-enabled").into());
    tr.set_sounds_enabled_hint(t!("sounds-enabled-hint").into());
    tr.set_sounds_join_leave(t!("sounds-join-leave").into());
    tr.set_sounds_mention(t!("sounds-mention").into());
    tr.set_sounds_mention_hint(t!("sounds-mention-hint").into());
    tr.set_sounds_dm(t!("sounds-dm").into());
    tr.set_notifs_header(t!("notifs-header").into());
    tr.set_notifs_lead(t!("notifs-lead").into());
    tr.set_notifs_messages(t!("notifs-messages").into());
    tr.set_notifs_messages_hint(t!("notifs-messages-hint").into());
    tr.set_notifs_first_join(t!("notifs-first-join").into());
    tr.set_notifs_first_join_hint(t!("notifs-first-join-hint").into());
    tr.set_sounds_lead(t!("sounds-lead").into());
    tr.set_sounds_share_started(t!("sounds-share-started").into());
    tr.set_sounds_viewer_joined(t!("sounds-viewer-joined").into());
    tr.set_sounds_viewer_joined_hint(t!("sounds-viewer-joined-hint").into());
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
    tr.set_audio_aec(t!("audio-aec").into());
    // The platform's side effects of the voice-call path, said where
    // the setting is (AEC-SPEC.md §4.6).
    tr.set_audio_aec_hint(
        if cfg!(windows) {
            t!("audio-aec-hint-windows")
        } else if cfg!(target_os = "android") {
            t!("audio-aec-hint-android")
        } else if cfg!(target_os = "ios") {
            t!("audio-aec-hint-ios")
        } else {
            t!("audio-aec-hint")
        }
        .into(),
    );
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
    // A phone picks a route for the call, not a sound card (audio::route).
    tr.set_audio_output_device(
        if SINGLE_WINDOW_HOST {
            t!("audio-output-device-android")
        } else {
            t!("audio-output-device")
        }
        .into(),
    );
    tr.set_audio_output_hint(
        if SINGLE_WINDOW_HOST {
            t!("audio-output-hint-android")
        } else {
            String::new()
        }
        .into(),
    );
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
