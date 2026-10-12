//! The frontend's file and link hooks on Android (ANDROID-SPEC.md D30;
//! `frontend::host::Hooks`): the system picker for attachments, the
//! avatar and the identity import; received files and saved pictures
//! into the public Downloads; `ACTION_VIEW` for those files and for
//! links; the clipboard. The Java halves are `BaucordActivity`'s,
//! reached through `java.rs`.

use std::path::{Path, PathBuf};

use app_core::files::PickSlot;
use frontend::host::{ChimeRoute, Hooks, KeepAlive, Notice, NoticeKind, OnPick, Saved};

use crate::java;

/// The pick in flight. One at a time — a second Attach while a large
/// file is still being copied would delete the copy under it and could
/// hand one pick's file to the other's callback (an attachment as the
/// avatar). A pick older than `app_core::files::PICK_STALE` is taken
/// as lost (the Activity went away with the picker open) and no longer
/// blocks a new one.
static PICK: PickSlot<OnPick> = PickSlot::new();

struct AndroidHost;

impl Hooks for AndroidHost {
    fn pick_file(&self, mime: &str, max_bytes: u64, on_pick: OnPick) {
        if PICK.begin(on_pick).is_err() {
            frontend::host::pick_failed(String::new(), "busy".to_string());
            return;
        }
        if let Err(e) = java::pick_file(mime, max_bytes) {
            log::warn!("file picker: {e}");
            PICK.finish();
            frontend::host::pick_failed(String::new(), e);
        }
    }

    fn save_download(&self, name: &str, mime: &str, bytes: &[u8]) -> Result<Saved, String> {
        let (uri, shown) = java::save_download(name, mime, bytes)?;
        Ok(Saved {
            target: PathBuf::from(uri),
            shown,
        })
    }

    fn open_target(&self, target: &Path, mime: &str) -> Result<(), String> {
        let uri = target.to_str().ok_or("not a saved download")?;
        if java::open_uri(uri, mime)? {
            Ok(())
        } else {
            Err("no app on this phone opens this kind of file".to_string())
        }
    }

    fn target_exists(&self, target: &Path) -> bool {
        match target.to_str() {
            // A MediaStore row: ask. On a JNI failure keep the card's
            // Open — the open itself will say if it is gone.
            Some(uri) if uri.starts_with("content://") => java::uri_exists(uri).unwrap_or(true),
            _ => target.exists(),
        }
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        if java::open_url(url)? {
            Ok(())
        } else {
            Err("no browser on this phone".to_string())
        }
    }

    fn keep_alive(&self, notice: Option<&KeepAlive>) {
        match notice {
            Some(n) => match java::start_voice_service(&n.channel, &n.title, &n.text, n.in_room) {
                Ok(true) => log::info!(
                    "voice service on ({}): {}",
                    if n.in_room { "room" } else { "lobby" },
                    n.title
                ),
                Ok(false) => log::warn!("voice service refused by the system"),
                Err(e) => log::warn!("voice service: {e}"),
            },
            None => match java::stop_voice_service() {
                Ok(()) => log::info!("voice service off"),
                Err(e) => log::warn!("voice service stop: {e}"),
            },
        }
    }

    fn keep_screen_on(&self, on: bool) {
        if let Err(e) = java::keep_screen_on(on) {
            log::warn!("keep screen on ({on}): {e}");
        }
    }

    // Notifications (D52): `Notices.java`, through `BaucordActivity`.
    fn notify(&self, notice: &Notice) {
        // Sound-only notices are iOS's way to chime; here the ringer
        // check of `chime_route` already decided.
        if notice.on_screen {
            return;
        }
        if let Err(e) = java::post_notice(notice) {
            log::warn!("notification {}: {e}", notice.key);
        }
    }

    fn notices_allowed(&self, kind: NoticeKind) -> bool {
        java::notices_allowed(kind).unwrap_or_else(|e| {
            log::warn!("notifications allowed? {e}");
            true
        })
    }

    fn cancel_notice(&self, key: &str) {
        if let Err(e) = java::cancel_notice(key) {
            log::warn!("cancel notification {key}: {e}");
        }
    }

    fn play_chime(&self, wav: &[u8]) -> bool {
        match java::play_chime(wav) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("chime: {e}");
                false
            }
        }
    }

    fn copy_text(&self, text: &str) -> Result<(), String> {
        if java::copy_text("baucord", text)? {
            Ok(())
        } else {
            Err("the system refused the copy".to_string())
        }
    }

    fn chime_route(&self) -> ChimeRoute {
        match java::chime_allowed() {
            Ok(true) => ChimeRoute::App,
            Ok(false) => ChimeRoute::Muted,
            Err(e) => {
                log::warn!("ringer mode: {e}");
                ChimeRoute::App
            }
        }
    }
}

/// `nativeFilePicked`: a readable copy of the picked file, a failure,
/// or (all `None`) a cancel.
pub(crate) fn picked(path: Option<String>, name: Option<String>, error: Option<String>) {
    let on_pick = PICK.finish();
    match (path, error) {
        (Some(path), _) => match on_pick {
            Some(on_pick) => on_pick(PathBuf::from(path)),
            None => log::warn!("a picked file arrived with no pick waiting"),
        },
        (None, Some(error)) => {
            log::warn!("file pick failed: {error}");
            frontend::host::pick_failed(name.unwrap_or_default(), error);
        }
        (None, None) => log::info!("file pick cancelled"),
    }
}

/// Bind the Java callback and hand the frontend its hooks; before
/// `frontend::app::run`.
pub(crate) fn install() {
    if let Err(e) = java::register_natives() {
        // An APK whose manifest still names the bare NativeActivity:
        // picks will fail with a toast, a notification's tap only brings
        // the app back; links and Downloads still work.
        log::warn!("file picker and notification taps unavailable (natives not bound): {e}");
    }
    frontend::host::install(Box::new(AndroidHost));
}
