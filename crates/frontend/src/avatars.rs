//! Other people's avatars, decoded off the UI thread.
//!
//! A look names its picture by hash; the picture comes from the cache
//! beside the settings (`avatar-cache/<hex>`) or, once, from the
//! server. Either way it used to be decoded on the UI thread, at
//! whatever size the sender made it. Here one worker thread decodes
//! them — refusing anything over `MAX_RECEIVED_AVATAR_DIM`, keeping
//! `AVATAR_SIDE`² of it — and hands the pixels back to the UI thread,
//! which wraps them in an image (`AppState::avatar_decoded`). The
//! worker starts when there is a picture to decode and ends when the
//! queue is empty: nothing stays behind, nothing wakes while idle.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use client::protocol::BlobId;
use log::warn;

/// Where the bytes of a picture to decode are.
pub enum Source {
    /// The cache on disk: read there, by the worker.
    Cache,
    /// Just arrived from the server: decoded, then written to the cache.
    Wire(Arc<[u8]>),
}

/// What became of a picture (`AppState::avatar_decoded`).
pub enum Outcome {
    Decoded(slint::SharedPixelBuffer<slint::Rgba8Pixel>),
    /// Not in the cache: ask the server.
    NotCached,
    /// The bytes did not decode, or the picture is too big to show.
    Refused,
}

struct Job {
    blob: BlobId,
    source: Source,
}

static QUEUE: Mutex<VecDeque<Job>> = Mutex::new(VecDeque::new());
/// A worker is running (or about to look at the queue once more).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Decode `blob`'s picture from `source`, and say so on the UI thread.
pub fn request(blob: BlobId, source: Source) {
    let Ok(mut queue) = QUEUE.lock() else {
        return;
    };
    queue.push_back(Job { blob, source });
    drop(queue);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("avatar-decode".into())
        .spawn(work)
    {
        warn!("avatars: no decoder thread: {e}");
        RUNNING.store(false, Ordering::Release);
    }
}

/// The worker: until the queue is empty. It says it is gone while
/// holding the queue's lock, so a job queued a moment later starts a
/// new one rather than waiting for this.
fn work() {
    loop {
        let job = {
            let Ok(mut queue) = QUEUE.lock() else {
                RUNNING.store(false, Ordering::Release);
                return;
            };
            let Some(job) = queue.pop_front() else {
                RUNNING.store(false, Ordering::Release);
                return;
            };
            job
        };
        let blob = job.blob;
        let outcome = decode(job);
        let posted = slint::invoke_from_event_loop(move || {
            crate::app::with_current(|app| app.avatar_decoded(blob, outcome));
        });
        if posted.is_err() {
            // The event loop is gone: the app is ending.
            RUNNING.store(false, Ordering::Release);
            return;
        }
    }
}

fn decode(job: Job) -> Outcome {
    let path = client::config::avatar_cache_dir().join(crate::app::hex(&job.blob));
    let (bytes, fresh) = match job.source {
        Source::Wire(bytes) => (bytes, true),
        Source::Cache => match std::fs::read(&path) {
            Ok(bytes) => (Arc::from(bytes), false),
            Err(_) => return Outcome::NotCached,
        },
    };
    match app_core::images::decode_avatar(&bytes) {
        Ok(rgba) => {
            if fresh
                && let Some(dir) = path.parent()
                && std::fs::create_dir_all(dir).is_ok()
            {
                let _ = std::fs::write(&path, &bytes);
            }
            Outcome::Decoded(slint::SharedPixelBuffer::clone_from_slice(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
            ))
        }
        Err(e) => {
            warn!("avatars: a picture refused: {e}");
            if !fresh {
                // A cached file that does not decode is of no use.
                let _ = std::fs::remove_file(&path);
            }
            Outcome::Refused
        }
    }
}
