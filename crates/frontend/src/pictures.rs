//! The chat's pictures: each image row's wire bytes, and the decoded
//! previews of the rows recently on screen.
//!
//! A row used to carry its decoded preview for as long as it existed —
//! up to 2 MB of RGBA a picture on a 2× display, for every picture in
//! every conversation, whether anyone scrolled to it or not. A few
//! pictures and a "Load earlier" doubled the app. Now the row holds no
//! pixels: the pane asks for them (`Logic.chat-picture`) only for the
//! rows its `ListView` has built, which are the ones on screen. A
//! preview not here yet is decoded on a worker thread (the pane shows
//! the picture's empty frame meanwhile, at its final size), and the row
//! is told to ask again when it is ready. What was decoded stays while
//! it fits [`PREVIEW_BUDGET`], the least recently asked for going first.
//!
//! Lives outside `AppState`: Slint calls the pane's callback while it
//! lays out or draws, which may be inside a call on the app.

use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::mpsc};

use log::warn;

/// Decoded previews kept for rows that are not on screen any more, in
/// bytes of RGBA. Scrolling back over the last few screens of pictures
/// finds them ready; past that they are decoded again. The rows on
/// screen hold their own previews whatever this says.
pub const PREVIEW_BUDGET: usize = 16 * 1024 * 1024;

/// One image row's picture.
struct Picture {
    /// The wire bytes, as received: what the gallery, Save, Copy and a
    /// GIF's playback read.
    bytes: Rc<Vec<u8>>,
    is_gif: bool,
    /// A decode is queued or running.
    decoding: bool,
    /// The bytes did not decode: the frame stays empty, and nothing
    /// asks again.
    failed: bool,
    /// When the pane last asked for it (a tick of [`Store::clock`]).
    wanted: u64,
}

struct Preview {
    image: slint::Image,
    bytes: usize,
}

#[derive(Default)]
struct Store {
    pictures: HashMap<i32, Picture>,
    previews: HashMap<i32, Preview>,
    preview_bytes: usize,
    clock: u64,
    /// The longest edge a preview is decoded to (the window's scale).
    max_edge: u32,
    decoder: Option<mpsc::Sender<Job>>,
}

struct Job {
    row_id: i32,
    bytes: Vec<u8>,
    max_edge: u32,
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

/// Keep `bytes` as the picture of row `row_id`.
pub fn insert(row_id: i32, bytes: Rc<Vec<u8>>, is_gif: bool) {
    STORE.with_borrow_mut(|s| {
        s.drop_preview(row_id);
        s.pictures.insert(
            row_id,
            Picture {
                bytes,
                is_gif,
                decoding: false,
                failed: false,
                wanted: 0,
            },
        );
    });
}

/// The row went: its bytes and preview with it.
pub fn remove(row_id: i32) {
    STORE.with_borrow_mut(|s| {
        s.pictures.remove(&row_id);
        s.drop_preview(row_id);
    });
}

/// Every picture goes (a cleared chat, a new connection).
pub fn clear() {
    STORE.with_borrow_mut(|s| {
        s.pictures.clear();
        s.previews.clear();
        s.preview_bytes = 0;
    });
}

/// Row `row_id`'s wire bytes, and whether they are an animated GIF.
#[must_use]
pub fn bytes(row_id: i32) -> Option<(Rc<Vec<u8>>, bool)> {
    STORE.with_borrow(|s| {
        s.pictures
            .get(&row_id)
            .map(|p| (Rc::clone(&p.bytes), p.is_gif))
    })
}

/// The longest edge previews are decoded to from now on.
pub fn set_max_edge(max_edge: u32) {
    STORE.with_borrow_mut(|s| s.max_edge = max_edge);
}

/// What the pane draws for row `row_id`: its preview, or an empty
/// image while that is decoded (the app then bumps the row's
/// `image-rev`, and the pane asks again: `AppState::picture_ready`).
/// Called by Slint, from the row's binding.
#[must_use]
pub fn preview(row_id: i32) -> slint::Image {
    let (image, job) = STORE.with_borrow_mut(|s| {
        s.clock += 1;
        let now = s.clock;
        let max_edge = s.max_edge.max(1);
        let Some(picture) = s.pictures.get_mut(&row_id) else {
            return (None, None);
        };
        picture.wanted = now;
        if let Some(preview) = s.previews.get(&row_id) {
            return (Some(preview.image.clone()), None);
        }
        if picture.decoding || picture.failed {
            return (None, None);
        }
        picture.decoding = true;
        let job = Job {
            row_id,
            bytes: picture.bytes.as_ref().clone(),
            max_edge,
        };
        (None, Some(job))
    });
    if let Some(job) = job {
        send(job);
    }
    image.unwrap_or_default()
}

/// Hand `job` to the decoder thread, starting it the first time.
fn send(job: Job) {
    let sender = STORE.with_borrow_mut(|s| {
        if s.decoder.is_none() {
            s.decoder = start_decoder();
        }
        s.decoder.clone()
    });
    let row_id = job.row_id;
    if sender.is_none_or(|tx| tx.send(job).is_err()) {
        // No thread to do it: nothing will, so do not wait for it.
        STORE.with_borrow_mut(|s| {
            if let Some(p) = s.pictures.get_mut(&row_id) {
                p.decoding = false;
                p.failed = true;
            }
        });
    }
}

/// The decoder: one thread for every picture, the newest request
/// first (it is the one on screen when someone scrolls fast). Each
/// result goes back to the UI thread, which wraps the pixels in an
/// image (`slint::Image` stays on the thread that draws it).
fn start_decoder() -> Option<mpsc::Sender<Job>> {
    let (tx, rx) = mpsc::channel::<Job>();
    let spawned = std::thread::Builder::new()
        .name("picture-decode".into())
        .spawn(move || {
            let mut queue: Vec<Job> = Vec::new();
            loop {
                if queue.is_empty() {
                    match rx.recv() {
                        Ok(job) => queue.push(job),
                        Err(_) => return,
                    }
                }
                queue.extend(rx.try_iter());
                let Some(job) = queue.pop() else { continue };
                let pixels = ui::convert::decode_preview_pixels(&job.bytes, job.max_edge)
                    .map(|(pixels, _, _)| pixels)
                    .map_err(|e| e.to_string());
                let row_id = job.row_id;
                let posted = slint::invoke_from_event_loop(move || {
                    if decoded(row_id, pixels) {
                        crate::app::with_current(|app| app.picture_ready(row_id));
                    }
                });
                if posted.is_err() {
                    // The event loop is gone: the app is ending.
                    return;
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            warn!("pictures: no decoder thread: {e}");
            None
        }
    }
}

/// A decode came back (UI thread). True when the row is still there
/// and has a preview to show now.
fn decoded(
    row_id: i32,
    pixels: Result<slint::SharedPixelBuffer<slint::Rgba8Pixel>, String>,
) -> bool {
    STORE.with_borrow_mut(|s| {
        let Some(picture) = s.pictures.get_mut(&row_id) else {
            return false;
        };
        picture.decoding = false;
        match pixels {
            Ok(pixels) => {
                let bytes = pixels.as_bytes().len();
                s.drop_preview(row_id);
                s.previews.insert(
                    row_id,
                    Preview {
                        image: slint::Image::from_rgba8(pixels),
                        bytes,
                    },
                );
                s.preview_bytes += bytes;
                s.trim(row_id);
                true
            }
            Err(e) => {
                warn!("pictures: row {row_id} did not decode: {e}");
                picture.failed = true;
                false
            }
        }
    })
}

impl Store {
    fn drop_preview(&mut self, row_id: i32) {
        if let Some(old) = self.previews.remove(&row_id) {
            self.preview_bytes -= old.bytes;
        }
    }

    /// Back under [`PREVIEW_BUDGET`]: the least recently wanted go,
    /// never `keep` (just decoded for a row on screen).
    fn trim(&mut self, keep: i32) {
        while self.preview_bytes > PREVIEW_BUDGET {
            let oldest = self
                .previews
                .keys()
                .copied()
                .filter(|&id| id != keep)
                .min_by_key(|id| self.pictures.get(id).map_or(0, |p| p.wanted));
            let Some(oldest) = oldest else { break };
            self.drop_preview(oldest);
        }
    }
}

/// What `/mem` reports: pictures kept, their wire bytes, previews
/// decoded and their pixels.
#[must_use]
pub fn stats() -> (usize, usize, usize, usize) {
    STORE.with_borrow(|s| {
        (
            s.pictures.len(),
            s.pictures.values().map(|p| p.bytes.len()).sum(),
            s.previews.len(),
            s.preview_bytes,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes() -> Rc<Vec<u8>> {
        Rc::new(vec![0; 16])
    }

    fn pixels(w: u32, h: u32) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
        slint::SharedPixelBuffer::new(w, h)
    }

    // Each test runs on a thread of its own: a store of its own.

    #[test]
    fn previews_stay_within_the_budget_least_recently_wanted_first() {
        // 1024² RGBA is 4 MB: four fit, the fifth pushes one out.
        for id in 1..=5 {
            insert(id, bytes(), false);
            STORE.with_borrow_mut(|s| {
                s.clock += 1;
                let now = s.clock;
                s.pictures.get_mut(&id).expect("picture").wanted = now;
            });
            assert!(decoded(id, Ok(pixels(1024, 1024))));
        }
        let (count, _, previews, bytes) = stats();
        assert_eq!(count, 5);
        assert_eq!(previews, 4);
        assert!(bytes <= PREVIEW_BUDGET);
        // Row 1 was the least recently wanted.
        STORE.with_borrow(|s| assert!(!s.previews.contains_key(&1)));
    }

    #[test]
    fn a_decoded_preview_is_answered_without_decoding_again() {
        insert(3, bytes(), false);
        assert!(decoded(3, Ok(pixels(8, 6))));
        let size = preview(3).size();
        assert_eq!((size.width, size.height), (8, 6));
        STORE.with_borrow(|s| assert!(!s.pictures[&3].decoding));
    }

    #[test]
    fn a_result_for_a_row_gone_is_dropped() {
        insert(7, bytes(), false);
        remove(7);
        assert!(!decoded(7, Ok(pixels(8, 8))));
        assert_eq!(stats(), (0, 0, 0, 0));
    }

    #[test]
    fn a_picture_that_does_not_decode_is_not_asked_for_again() {
        insert(9, bytes(), false);
        assert!(!decoded(9, Err("bad".into())));
        STORE.with_borrow(|s| assert!(s.pictures[&9].failed));
        // Asked again, it stays empty and queues nothing.
        assert_eq!(preview(9).size().width, 0);
        STORE.with_borrow(|s| assert!(!s.pictures[&9].decoding));
    }
}
