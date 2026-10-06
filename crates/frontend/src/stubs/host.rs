//! `host` without a desktop: no `rfd`, no `open`. A host that can do
//! these things another way installs [`Hooks`] at startup — Android
//! does (`crates/android/src/host.rs`: the system file picker, the
//! public Downloads folder through `MediaStore`, `ACTION_VIEW`), the
//! first slice of the `Platform` trait of ANDROID-SPEC.md §5. Without
//! hooks every call logs and does nothing, as before.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use i18n::t;

pub type Filter = (String, Vec<String>);

/// No save dialog on this host: callers that would ask where to save
/// put the file in Downloads instead ([`save_download`]).
pub const HAS_SAVE_DIALOG: bool = false;

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
    /// Whether a [`Saved::target`] is still there.
    fn target_exists(&self, target: &Path) -> bool;
    /// Open a link in the browser.
    ///
    /// # Errors
    /// The host's own message.
    fn open_url(&self, url: &str) -> Result<(), String>;
    /// Keep the app running while a session is connected and the app is
    /// off screen (`Some`: the notification's words), or stop (`None`).
    fn keep_alive(&self, _notice: Option<&KeepAlive>) {}
}

/// What the keep-alive notification says (Android's foreground service).
pub struct KeepAlive {
    /// The notification channel's name in the system settings.
    pub channel: String,
    pub title: String,
    pub text: String,
}

/// Connected to `server` (`Some`) or not any more (`None`): the host
/// keeps the process alive meanwhile, where it has a way to.
pub fn keep_alive(server: Option<&str>) {
    let Some(hooks) = HOOKS.get() else { return };
    match server {
        Some(server) => hooks.keep_alive(Some(&KeepAlive {
            channel: t!("notif-channel-voice"),
            title: t!("notif-connected-title", "server" => server),
            text: t!("notif-connected-text"),
        })),
        None => hooks.keep_alive(None),
    }
}

static HOOKS: OnceLock<Box<dyn Hooks>> = OnceLock::new();

/// Install the host's hooks; the first call wins.
pub fn install(hooks: Box<dyn Hooks>) {
    if HOOKS.set(hooks).is_err() {
        log::warn!("host hooks already installed");
    }
}

/// The largest file a picker hands back: a file card's limit, and room
/// for a camera photo that is shrunk to the image budget after.
const MAX_PICK_BYTES: u64 = 64 * 1024 * 1024;

/// Ask the user for one existing file. A filter whose extensions are
/// all images asks the system for images; anything else for any file.
// By value: the desktop twin's signature, which moves it to a thread.
#[allow(clippy::needless_pass_by_value)]
pub fn pick_file(filter: Option<Filter>, on_pick: impl FnOnce(PathBuf) + Send + 'static) {
    let Some(hooks) = HOOKS.get() else {
        log::info!("no file picker on this host");
        return;
    };
    let images_only = filter.as_ref().is_some_and(|(_, exts)| {
        !exts.is_empty()
            && exts.iter().all(|e| {
                app_core::images::IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())
            })
    });
    let mime = if images_only { "image/*" } else { "*/*" };
    hooks.pick_file(mime, MAX_PICK_BYTES, Box::new(on_pick));
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

pub fn save_file(
    _default_name: &str,
    _filter: Option<Filter>,
    _on_pick: impl FnOnce(PathBuf) + Send + 'static,
) {
    log::info!("no save dialog on this host");
}

/// Downloads through the host, when it has its own way there; `None`
/// leaves it to the plain folder write.
pub fn save_download(name: &str, mime: &str, bytes: &[u8]) -> Option<Result<Saved, String>> {
    HOOKS.get().map(|h| h.save_download(name, mime, bytes))
}

/// Whether a saved file is still where it was saved.
pub fn target_exists(target: &Path) -> bool {
    HOOKS
        .get()
        .map_or_else(|| target.exists(), |h| h.target_exists(target))
}

/// # Errors
/// No launcher here, or the host's own message.
pub fn open_target(target: &Path) -> Result<(), String> {
    let Some(hooks) = HOOKS.get() else {
        return Err("no system launcher on this host".to_string());
    };
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    hooks.open_target(target, crate::media::mime_for(name))
}

/// # Errors
/// No launcher here, or the host's own message.
pub fn open_url(url: &str) -> Result<(), String> {
    match HOOKS.get() {
        Some(hooks) => hooks.open_url(url),
        None => Err("no system launcher on this host".to_string()),
    }
}
