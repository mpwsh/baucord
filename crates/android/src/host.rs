//! The frontend's file and link hooks on Android (ANDROID-SPEC.md D30;
//! `frontend::host::Hooks`): the system picker for attachments, the
//! avatar and the identity import; received files and saved pictures
//! into the public Downloads; `ACTION_VIEW` for those files and for
//! links. The Java halves are `BaucordActivity`'s, reached through
//! `java.rs`.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use frontend::host::{Hooks, KeepAlive, OnPick, Saved};

use crate::java;

/// The pick in flight: its callback and when it started. One at a
/// time — a second Attach while a large file is still being copied
/// would delete the copy under it and could hand one pick's file to
/// the other's callback (an attachment as the avatar).
static PENDING: Mutex<Option<(OnPick, Instant)>> = Mutex::new(None);

/// A pick older than this is taken as lost (the Activity went away
/// with the picker open) and no longer blocks a new one.
const PICK_STALE: Duration = Duration::from_mins(10);

fn pending() -> MutexGuard<'static, Option<(OnPick, Instant)>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

struct AndroidHost;

impl Hooks for AndroidHost {
    fn pick_file(&self, mime: &str, max_bytes: u64, on_pick: OnPick) {
        {
            let mut slot = pending();
            if slot
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < PICK_STALE)
            {
                drop(slot);
                frontend::host::pick_failed(String::new(), "busy".to_string());
                return;
            }
            *slot = Some((on_pick, Instant::now()));
        }
        if let Err(e) = java::pick_file(mime, max_bytes) {
            log::warn!("file picker: {e}");
            pending().take();
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
            Some(n) => match java::start_voice_service(&n.channel, &n.title, &n.text) {
                Ok(true) => log::info!("voice service on: {}", n.title),
                Ok(false) => log::warn!("voice service refused by the system"),
                Err(e) => log::warn!("voice service: {e}"),
            },
            None => match java::stop_voice_service() {
                Ok(()) => log::info!("voice service off"),
                Err(e) => log::warn!("voice service stop: {e}"),
            },
        }
    }
}

/// `nativeFilePicked`: a readable copy of the picked file, a failure,
/// or (all `None`) a cancel.
pub(crate) fn picked(path: Option<String>, name: Option<String>, error: Option<String>) {
    let on_pick = pending().take().map(|(on_pick, _)| on_pick);
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
        // picks will fail with a toast; links and Downloads still work.
        log::warn!("file picker unavailable (nativeFilePicked not bound): {e}");
    }
    frontend::host::install(Box::new(AndroidHost));
}
