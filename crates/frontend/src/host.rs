//! What the app asks of the system it runs on and no engine crate can
//! answer: file dialogs, opening a link or a file, the Downloads
//! folder, notifications, staying alive off screen.
//!
//! One module for every host (review P3-33; the phones' used to be
//! `stubs/host.rs`, beside a desktop twin that was mostly no-ops). The
//! answers are a [`Hooks`]: the desktop's own is `DesktopHooks`
//! (`rfd`, `pollster`, `open` — feature `desktop`), there from the
//! first call; a phone installs its hooks at startup ([`install`] —
//! `crates/android/src/host.rs`, `crates/ios/src/host.rs`). Without
//! either, every call logs and does nothing.
//!
//! Dialogs are async on the portal backend, so each runs on a scratch
//! thread and calls `on_pick` there; callers hop back to the UI with
//! `slint::invoke_from_event_loop` themselves.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use i18n::t;

pub use crate::notice::{ChimeRoute, Notice, NoticeKind, Tone};

pub type Filter = (String, Vec<String>);

/// Saving asks where ([`save_file`]): the desktop. A phone has no such
/// dialog, and callers put the file in Downloads instead
/// ([`save_download`]).
pub const HAS_SAVE_DIALOG: bool = cfg!(feature = "desktop");

/// The clipboard takes pictures (`clipboard::set_clipboard_image`); a
/// phone's has no way in from here, and its menus leave "Copy image"
/// out.
pub const HAS_IMAGE_CLIPBOARD: bool = cfg!(feature = "desktop");

/// The callback a picked file is handed to (any thread).
pub type OnPick = Box<dyn FnOnce(PathBuf) + Send>;

/// A file written into the user's Downloads.
pub struct Saved {
    /// What the app opens and checks later — a path, or on Android a
    /// `content://` URI carried in a `PathBuf`.
    pub target: PathBuf,
    /// Where it went, for the toast ("Download/report.pdf").
    pub shown: String,
}

/// What a host without a desktop supplies for files and links.
pub trait Hooks: Send + Sync {
    /// Show the system picker for one file of `mime` (`*/*`,
    /// `image/*`) and hand a readable local copy to `on_pick`. Nothing
    /// happens on cancel; a failure goes to [`pick_failed`].
    fn pick_file(&self, mime: &str, max_bytes: u64, on_pick: OnPick);
    /// Write `bytes` into the user's Downloads as `name` (renamed by
    /// the system if taken).
    ///
    /// # Errors
    /// The host's own message.
    fn save_download(&self, name: &str, mime: &str, bytes: &[u8]) -> Result<Saved, String>;
    /// Open a file [`Self::save_download`] wrote with whatever handles
    /// `mime`.
    ///
    /// # Errors
    /// The host's own message (no app for that type, say).
    fn open_target(&self, target: &Path, mime: &str) -> Result<(), String>;
    /// Whether a [`Saved::target`] is still there. A host whose saves
    /// are not all plain files (Android's `MediaStore` rows) says how.
    fn target_exists(&self, target: &Path) -> bool {
        target.exists()
    }
    /// Open a link in the browser.
    ///
    /// # Errors
    /// The host's own message.
    fn open_url(&self, url: &str) -> Result<(), String>;
    /// Keep the app running while a session is connected and the app is
    /// off screen (`Some`: the notification's words), or stop (`None`).
    fn keep_alive(&self, _notice: Option<&KeepAlive>) {}
    /// Keep the screen on (`true`: a stream is being watched) or let the
    /// system dim and lock it again (`false`). Called on the UI thread.
    fn keep_screen_on(&self, _on: bool) {}
    /// Post (or replace, by [`Notice::key`]) a notification. A tap on one
    /// with [`Notice::open_dm`] comes back through [`open_conversation`].
    /// Called on the UI thread.
    fn notify(&self, _notice: &Notice) {}
    /// Whether a notification of `kind` would be shown now: the user
    /// may have refused the permission or switched the channel off, and
    /// then a DM in the background chimes instead of vanishing.
    fn notices_allowed(&self, _kind: NoticeKind) -> bool {
        true
    }
    /// Take down the notification posted under `key`, if it is up.
    fn cancel_notice(&self, _key: &str) {}
    /// How an alert chime may play right now: asked before each one, so
    /// a phone switched to silent a moment ago is honoured.
    fn chime_route(&self) -> ChimeRoute {
        ChimeRoute::App
    }
    /// Play chime `tone` (its sound: `wav`) itself, for when the voice
    /// engine has no playback open (Android out of a voice room).
    /// `false`: the host cannot, and the engine is asked as before.
    fn play_chime(&self, _tone: Tone, _wav: &[u8]) -> bool {
        false
    }
    /// Ask the user for one existing file, through a dialog that takes
    /// a filter (the desktop's). A host asked by MIME type instead gets
    /// [`Self::pick_file`]: images when the filter's extensions are all
    /// images, any file otherwise.
    fn pick_filtered(&self, filter: Option<Filter>, on_pick: OnPick) {
        let images_only = filter.as_ref().is_some_and(|(_, exts)| {
            !exts.is_empty()
                && exts.iter().all(|e| {
                    app_core::images::IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())
                })
        });
        let mime = if images_only { "image/*" } else { "*/*" };
        self.pick_file(mime, MAX_PICK_BYTES, on_pick);
    }
    /// Ask the user where to write a file, suggesting `default_name`
    /// ([`HAS_SAVE_DIALOG`]).
    fn save_file(&self, _default_name: &str, _filter: Option<Filter>, _on_pick: OnPick) {
        log::info!("no save dialog on this host");
    }
    /// Received files are saved through [`Self::save_download`] (a
    /// phone's own way into Downloads). Where they are not, the app
    /// writes the folder itself (`media::save_to_downloads`).
    fn owns_downloads(&self) -> bool {
        true
    }
    /// Put plain text on the system clipboard (Settings → Debug's
    /// "Copy log", an invite code). Called on the UI thread.
    ///
    /// # Errors
    /// The host's own message; by default, that there is no clipboard.
    fn copy_text(&self, _text: &str) -> Result<(), String> {
        Err(NO_CLIPBOARD.to_string())
    }
}

const NO_CLIPBOARD: &str = "no clipboard on this host";

/// Put plain text on the host's clipboard (`crate::clipboard`'s phone
/// twin comes here).
///
/// # Errors
/// As [`Hooks::copy_text`], or no host installed.
pub fn copy_text(text: &str) -> Result<(), String> {
    match hooks() {
        Some(hooks) => hooks.copy_text(text),
        None => Err(NO_CLIPBOARD.to_string()),
    }
}

/// What the keep-alive notification says (Android's foreground service).
pub struct KeepAlive {
    /// The notification channel's name in the system settings.
    pub channel: String,
    pub title: String,
    pub text: String,
    /// In a voice room (the microphone open), not the lobby: Android
    /// types its service by it (D63).
    pub in_room: bool,
}

/// Connected to `server` (`Some`) — in a voice room or the lobby — or
/// not any more (`None`): the host keeps the process alive meanwhile,
/// where it has a way to. Called again whenever `in_room` changes.
pub fn keep_alive(server: Option<&str>, in_room: bool) {
    let Some(hooks) = hooks() else { return };
    match server {
        Some(server) => hooks.keep_alive(Some(&KeepAlive {
            channel: t!("notif-channel-voice"),
            title: t!("notif-connected-title", "server" => server),
            text: t!("notif-connected-text"),
            in_room,
        })),
        None => hooks.keep_alive(None),
    }
}

/// A stream is on the screen (`true`) or no longer (`false`): the host
/// keeps the display awake meanwhile, as a video player does (IOS-SPEC.md
/// §14 D45). Only while it is watched — chat and the voice lists let the
/// phone sleep as usual.
pub fn keep_screen_on(on: bool) {
    // The desktop's viewer is a window the user arranges; the display's
    // sleep is left to the OS.
    if cfg!(feature = "desktop") {
        return;
    }
    if let Some(hooks) = hooks() {
        log::info!(
            "screen {}",
            if on {
                "kept on (watching a stream)"
            } else {
                "may sleep again"
            }
        );
        hooks.keep_screen_on(on);
    }
}

/// Whether this host shows notifications: a phone that installed its
/// hooks. On the desktop the window, the tray and the chimes carry
/// everything.
#[must_use]
pub fn notifies() -> bool {
    !cfg!(feature = "desktop") && hooks().is_some()
}

/// Post a notification, if this host shows them.
pub fn notify(notice: &Notice) {
    if !notifies() {
        return;
    }
    if let Some(hooks) = hooks() {
        log::info!(
            "notice {} ({:?}{})",
            notice.key,
            notice.kind,
            if notice.on_screen { ", sound only" } else { "" }
        );
        hooks.notify(notice);
    }
}

/// Whether a notification of `kind` would be shown now (the host
/// shows them, and the user has not refused or silenced them).
#[must_use]
pub fn notices_allowed(kind: NoticeKind) -> bool {
    notifies() && hooks().is_some_and(|h| h.notices_allowed(kind))
}

/// Take down the notification posted under `key`.
pub fn cancel_notice(key: &str) {
    if let Some(hooks) = hooks() {
        hooks.cancel_notice(key);
    }
}

/// How an alert chime plays here now ([`ChimeRoute`]); the app's own
/// mix where no host says otherwise.
#[must_use]
pub fn chime_route() -> ChimeRoute {
    hooks().map_or(ChimeRoute::App, Hooks::chime_route)
}

/// A notification was tapped: open the DM conversation it was about
/// (`dm` is the user id in hex). Any thread.
pub fn open_conversation(dm: String) {
    crate::app::open_noticed_conversation(dm);
}

/// Have the host play the message chime itself (`Hooks::play_chime`);
/// `false` when it cannot.
#[must_use]
pub fn play_chime(tone: Tone) -> bool {
    hooks().is_some_and(|h| h.play_chime(tone, chime_wav(tone)))
}

/// Chime `tone` as a WAV file, for a host that plays it through the
/// system (iOS writes it where notifications find their sounds). Made
/// once a run; every chime borrows it.
#[must_use]
pub fn chime_wav(tone: Tone) -> &'static [u8] {
    static MESSAGE: OnceLock<Vec<u8>> = OnceLock::new();
    static MENTION: OnceLock<Vec<u8>> = OnceLock::new();
    let (cell, sound) = match tone {
        Tone::DirectMessage => (&MESSAGE, audio::SoundType::DirectMessage),
        Tone::Mention => (&MENTION, audio::SoundType::Mention),
    };
    cell.get_or_init(|| {
        let bank = audio::sounds::SoundBank::new();
        audio::sounds::wav_bytes(bank.get(sound))
    })
}

static HOOKS: OnceLock<Box<dyn Hooks>> = OnceLock::new();

/// Install the host's hooks; the first call wins.
pub fn install(hooks: Box<dyn Hooks>) {
    if HOOKS.set(hooks).is_err() {
        log::warn!("host hooks already installed");
    }
}

/// The host's answers: the desktop's own, or what a phone installed
/// (none before it did).
#[cfg_attr(feature = "desktop", allow(clippy::unnecessary_wraps))]
fn hooks() -> Option<&'static dyn Hooks> {
    #[cfg(feature = "desktop")]
    {
        Some(HOOKS.get_or_init(|| Box::new(DesktopHooks)).as_ref())
    }
    #[cfg(not(feature = "desktop"))]
    {
        HOOKS.get().map(AsRef::as_ref)
    }
}

/// The desktop: native dialogs on scratch threads, the system's own
/// launcher for links and files. Everything else is the trait's
/// default — a desktop process stays alive by itself, has no
/// notifications of its own yet, and chimes through the voice engine.
#[cfg(feature = "desktop")]
struct DesktopHooks;

#[cfg(feature = "desktop")]
impl DesktopHooks {
    fn dialog(filter: Option<&Filter>) -> rfd::AsyncFileDialog {
        let dialog = rfd::AsyncFileDialog::new();
        match filter {
            Some((name, exts)) => {
                let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
                dialog.add_filter(name, &exts)
            }
            None => dialog,
        }
    }
}

#[cfg(feature = "desktop")]
impl Hooks for DesktopHooks {
    fn pick_file(&self, _mime: &str, _max_bytes: u64, on_pick: OnPick) {
        self.pick_filtered(None, on_pick);
    }

    fn pick_filtered(&self, filter: Option<Filter>, on_pick: OnPick) {
        std::thread::spawn(move || {
            let dialog = Self::dialog(filter.as_ref());
            if let Some(handle) = pollster::block_on(dialog.pick_file()) {
                on_pick(handle.path().to_path_buf());
            }
        });
    }

    fn save_file(&self, default_name: &str, filter: Option<Filter>, on_pick: OnPick) {
        let default_name = default_name.to_string();
        std::thread::spawn(move || {
            let dialog = Self::dialog(filter.as_ref()).set_file_name(&default_name);
            if let Some(handle) = pollster::block_on(dialog.save_file()) {
                on_pick(handle.path().to_path_buf());
            }
        });
    }

    /// Not asked: the app writes Downloads itself here.
    fn owns_downloads(&self) -> bool {
        false
    }

    fn save_download(&self, _name: &str, _mime: &str, _bytes: &[u8]) -> Result<Saved, String> {
        Err("the desktop writes its Downloads folder itself".to_string())
    }

    fn open_target(&self, target: &Path, _mime: &str) -> Result<(), String> {
        open::that_detached(target).map_err(|e| e.to_string())
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        open::that_detached(url).map_err(|e| e.to_string())
    }
}

/// The largest file a picker hands back: a file card's limit, and room
/// for a camera photo that is shrunk to the image budget after.
const MAX_PICK_BYTES: u64 = 64 * 1024 * 1024;

/// Ask the user for one existing file. `on_pick` runs on whatever
/// thread the host answers on.
pub fn pick_file(filter: Option<Filter>, on_pick: impl FnOnce(PathBuf) + Send + 'static) {
    match hooks() {
        Some(hooks) => hooks.pick_filtered(filter, Box::new(on_pick)),
        None => log::info!("no file picker on this host"),
    }
}

/// The picker could not hand a file back: say so in the window.
/// `error` is `"too-large"` for a file past the limit, `"busy"` while
/// the last pick is still being read, else the host's message.
pub fn pick_failed(name: String, error: String) {
    let _ = slint::invoke_from_event_loop(move || {
        crate::app::with_current(move |app| {
            let shown = if name.is_empty() {
                "file".to_string()
            } else {
                name
            };
            let line = if error == "busy" {
                t!("file-pick-busy")
            } else if error == "too-large" {
                t!("file-too-large",
                    "name" => shown.as_str(),
                    "max" => client::protocol::MAX_BLOB_BYTES / (1024 * 1024))
            } else {
                t!("file-read-error", "name" => shown.as_str(), "error" => error.as_str())
            };
            app.show_error(&line);
        });
    });
}

/// Ask the user where to write a file, suggesting `default_name`
/// ([`HAS_SAVE_DIALOG`]).
pub fn save_file(
    default_name: &str,
    filter: Option<Filter>,
    on_pick: impl FnOnce(PathBuf) + Send + 'static,
) {
    match hooks() {
        Some(hooks) => hooks.save_file(default_name, filter, Box::new(on_pick)),
        None => log::info!("no save dialog on this host"),
    }
}

/// Downloads through the host, when it has its own way there; `None`
/// leaves it to the plain folder write.
#[must_use]
pub fn save_download(name: &str, mime: &str, bytes: &[u8]) -> Option<Result<Saved, String>> {
    hooks()
        .filter(|h| h.owns_downloads())
        .map(|h| h.save_download(name, mime, bytes))
}

/// Whether a saved file is still where it was saved.
#[must_use]
pub fn target_exists(target: &Path) -> bool {
    HOOKS
        .get()
        .map_or_else(|| target.exists(), |h| h.target_exists(target))
}

/// # Errors
/// No launcher here, or the host's own message.
pub fn open_target(target: &Path) -> Result<(), String> {
    let Some(hooks) = hooks() else {
        return Err("no system launcher on this host".to_string());
    };
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    hooks.open_target(target, crate::media::mime_for(name))
}

/// Open the system's sound settings where what happens to other apps
/// during a call is chosen: Windows' Sound dialog on its Communications
/// tab (`AEC-SPEC.md` §4.6).
///
/// # Errors
/// No such setting on this system, or the launcher's own message.
pub fn open_sound_settings() -> Result<(), String> {
    #[cfg(windows)]
    {
        // By its full path: a bare name is looked up next to our own
        // executable first.
        let control = std::env::var_os("SystemRoot").map_or_else(
            || PathBuf::from("control.exe"),
            |root| PathBuf::from(root).join("System32").join("control.exe"),
        );
        std::process::Command::new(control)
            .arg("mmsys.cpl,,3")
            .spawn()
            .map(drop)
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Err("no such setting on this system".to_string())
    }
}

/// # Errors
/// No launcher here, or the host's own message.
pub fn open_url(url: &str) -> Result<(), String> {
    match hooks() {
        Some(hooks) => hooks.open_url(url),
        None => Err("no system launcher on this host".to_string()),
    }
}
