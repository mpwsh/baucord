// media.rs — chat images: ingestion (paste/attach/URL), the outgoing
// preview, and inline GIF playback.
//
// FLTK's Fl_Anim_GIF is gone; Slint has no animated Image. The
// replacement is a small frame driver: app-core decodes the GIF to
// RGBA frames, a `slint::Timer` steps them into the chat row via
// `set_row_data`. Autoplay policy is unchanged: ~3 loops (≤30 s),
// freeze to frame 0 (dropping the decoded frames), replay on hover.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use app_core::{
    gif,
    images::{self, ImageError},
};
use i18n::t;
use log::{info, warn};
use slint::{ComponentHandle, Model};
use ui::State;

use crate::app::AppState;

/// Localize an app-core image error (keys from the real i18n catalog).
fn describe(err: &ImageError) -> String {
    match err {
        ImageError::GifTooLarge(max) => {
            t!("chat-gif-too-large", "mb" => (max / (1024 * 1024)).to_string())
        }
        ImageError::GifTooBig => {
            t!("chat-gif-too-big", "dim" => images::MAX_ANIM_GIF_DIM.to_string())
        }
        ImageError::TooLargeAfterDownscale(kb) => {
            t!("chat-image-too-large", "kb" => kb.to_string())
        }
        ImageError::Codec(e) => t!("chat-image-load-error", "error" => e.as_str()),
    }
}

// ── Incoming images → chat rows ─────────────────────────────────────────

/// Append an image message to chat. Returns false if the bytes are
/// not a picture (the caller posts a system-row notice instead).
///
/// Nothing is decoded here: the row is laid out from the size in the
/// picture's header, and its pixels are decoded when the pane first
/// shows it (`pictures.rs`). A history page of pictures used to decode
/// every one of them on the UI thread, and keep them all.
pub fn push_image_row(app: &mut AppState, meta: &app_core::chat::Meta, data: Vec<u8>) -> bool {
    let Some((w, h)) = images::received_dimensions(&data) else {
        return false;
    };
    let is_gif = data.starts_with(b"GIF8") && images::gif_is_animated(&data);

    app.mark_day(meta.day);
    let delta = app.history_mut().push_image(meta, w, h, is_gif);
    // sync_history_tail equivalent: history rows were appended; mirror
    // them (push_image appends exactly one row and may drop from front).
    app.sync_history_tail(&delta);

    // The freshly appended row is the last one.
    let last = app.chat_model().row_count() - 1;
    // Exactly the size of the picture: the bytes from the wire may
    // carry spare capacity (review 2026-10, P2-42).
    install_image(app, last, data.into(), is_gif);
    true
}

/// Register a picture's bytes (and player, for a GIF) under the row id
/// of the image row at `index`.
fn install_image(app: &mut AppState, index: usize, bytes: Arc<[u8]>, is_gif: bool) {
    let Some(row) = app.chat_model().row_data(index) else {
        return;
    };
    crate::pictures::set_max_edge(ui::convert::preview_max_edge(
        app.main.window().scale_factor(),
    ));
    crate::pictures::insert(row.row_id, Arc::clone(&bytes), is_gif);

    if is_gif {
        // Frozen, the row shows its picture again: an empty frame is
        // what puts it back (`ImageRow` draws `image` only when set).
        let mut player = GifPlayer::new(bytes, slint::Image::default());
        // Only in the conversation on screen: one a DM's picture was
        // pushed into in passing waits for a hover (review 2026-10,
        // P2-30).
        let shown = !app.conversation_borrowed();
        if shown {
            player.start_autoplay();
        }
        app.gifs_mut().insert(row.row_id, player);
        if shown {
            app.cap_gif_autoplay();
            app.ensure_gif_ticker();
        }
    }
}

/// A picture that came as a file (a DM attachment, spec §7): the card
/// for message `msg_id` in the shown conversation becomes an image row
/// with `data`. `false` when the bytes are not a picture after all.
pub fn picture_from_file(app: &mut AppState, msg_id: u64, data: Arc<[u8]>) -> bool {
    let Some((w, h)) = images::received_dimensions(&data) else {
        return false;
    };
    let is_gif = data.starts_with(b"GIF8") && images::gif_is_animated(&data);
    let Some(index) = app.history_mut().attach_image(msg_id, w, h, is_gif) else {
        return false;
    };
    let row = app.chat_row_at(index);
    app.chat_model().set_row_data(index, row);
    install_image(app, index, data, is_gif);
    // A card became a picture: taller. A reader pinned to the bottom
    // stays there (the follow only ran on appends before, which left
    // the view a few rows short after a replay's pictures landed).
    app.main.invoke_chat_follow_appended();
    true
}

// ── GIF playback ────────────────────────────────────────────────────────

enum GifMode {
    /// Playing with a wall-clock budget; freezes when it expires.
    Autoplay { deadline: Instant },
    /// Hover replay — runs until the pointer leaves.
    Hover,
    /// Runs until the player is dropped (the gallery, the send
    /// preview).
    Loop,
    /// Showing frame 0, decoded frames released.
    Frozen,
}

/// One tick's outcome for a player.
pub enum GifStep {
    /// Nothing to draw (frozen, the current frame hasn't elapsed, or the
    /// next one is still being decoded).
    Idle,
    /// Draw this freshly decoded frame (full logical screen).
    Blit(slint::SharedPixelBuffer<slint::Rgba8Pixel>),
    /// Playback ended: restore the row's static preview texture.
    Freeze { first: slint::Image },
}

/// Chat-row GIF playback state, advanced by the app's animation ticker
/// (`AppState::gif_tick`) instead of per-player timers — one timer for
/// any number of playing GIFs, set for the next frame due.
///
/// Frames are decoded *as they play* ([`gif::GifStream`]), on the GIF
/// worker thread, one ahead of the frame shown: the UI thread only
/// wraps finished pixels in an image (it used to decode each frame
/// itself, 1–4 ms a frame at 800 px). A playing GIF holds a frame or
/// two, not the whole animation. Frozen players hold nothing but the
/// compressed bytes (shared with the picture store) and the row's
/// preview texture handle.
pub struct GifPlayer {
    bytes: Arc<[u8]>,
    /// What a freeze puts back: the gallery's and the send preview's
    /// first frame; for a chat row an empty image, which hands the row
    /// back to its picture (`pictures.rs`). Cloning a `slint::Image`
    /// clones a handle, not pixels.
    first: slint::Image,
    /// The worker's stream, present only while playing.
    stream: Option<GifHandle>,
    in_frame_ms: f64,
    /// The shown frame's delay; 0 until the stream has opened.
    current_delay_ms: f64,
    mode: GifMode,
}

impl GifPlayer {
    #[must_use]
    pub fn new(bytes: Arc<[u8]>, first: slint::Image) -> Self {
        Self {
            bytes,
            first,
            stream: None,
            in_frame_ms: 0.0,
            current_delay_ms: 0.0,
            mode: GifMode::Frozen,
        }
    }

    /// Open the frame stream on the worker. Frame 0's pixels are not
    /// shown (the preview already shows it) but its delay starts the
    /// clock; a stream that will not open ends playback at the next
    /// step.
    fn open_stream(&mut self) {
        self.stream = Some(GifHandle::open(Arc::clone(&self.bytes)));
        self.current_delay_ms = 0.0;
        self.in_frame_ms = 0.0;
    }

    /// A player that loops for as long as it is kept: the gallery's
    /// and the send preview's, which show one picture and go away with
    /// it. `first` is what is on screen until the second frame is due.
    #[must_use]
    pub fn looping(bytes: Arc<[u8]>, first: slint::Image) -> Self {
        let mut player = Self::new(bytes, first);
        player.open_stream();
        player.mode = GifMode::Loop;
        player
    }

    pub fn start_autoplay(&mut self) {
        let budget = gif::autoplay_budget_secs(&self.bytes);
        self.open_stream();
        self.mode = GifMode::Autoplay {
            deadline: Instant::now() + Duration::from_secs_f64(budget),
        };
    }

    /// Pointer entered/left the image. Returns true when playback (and
    /// therefore the ticker) should be running.
    pub fn hover(&mut self, entering: bool) -> bool {
        if entering {
            if matches!(self.mode, GifMode::Frozen) {
                self.open_stream();
                self.mode = GifMode::Hover;
            }
        } else if matches!(self.mode, GifMode::Hover) {
            // Next tick freezes (restores the preview, drops the stream).
            self.mode = GifMode::Autoplay {
                deadline: Instant::now(),
            };
        }
        self.playing()
    }

    #[must_use]
    pub fn playing(&self) -> bool {
        !matches!(self.mode, GifMode::Frozen)
    }

    /// Playing by itself, not under the pointer.
    #[must_use]
    pub fn autoplaying(&self) -> bool {
        matches!(self.mode, GifMode::Autoplay { .. })
    }

    /// Stop playback immediately and release the stream, returning the
    /// preview to restore — used when the window hides to the tray.
    pub fn force_freeze(&mut self) -> Option<slint::Image> {
        if !self.playing() {
            return None;
        }
        self.freeze();
        Some(self.first.clone())
    }

    fn freeze(&mut self) {
        self.mode = GifMode::Frozen;
        self.stream = None;
        self.in_frame_ms = 0.0;
    }

    /// How long until this player has something to show, for the
    /// ticker's next run; `None` when frozen.
    #[must_use]
    pub fn due_in_ms(&self) -> Option<f64> {
        if !self.playing() {
            return None;
        }
        if self.current_delay_ms == 0.0 {
            // Opening: look again soon.
            return Some(0.0);
        }
        Some((self.current_delay_ms - self.in_frame_ms).max(0.0))
    }

    /// Advance by `dt_ms` of wall time. A frame not decoded yet when it
    /// is due shows as soon as it is, with no catch-up after it: a slow
    /// worker slows playback instead of piling up work.
    pub fn step(&mut self, dt_ms: f64) -> GifStep {
        if !self.playing() {
            return GifStep::Idle;
        }
        if let GifMode::Autoplay { deadline } = self.mode
            && Instant::now() >= deadline
        {
            self.freeze();
            return GifStep::Freeze {
                first: self.first.clone(),
            };
        }
        let Some(handle) = &self.stream else {
            return GifStep::Idle;
        };
        let mut slot = handle.lock();
        if slot.failed {
            drop(slot);
            self.freeze();
            return GifStep::Freeze {
                first: self.first.clone(),
            };
        }
        if self.current_delay_ms == 0.0 {
            match slot.first_delay_ms {
                Some(delay) => self.current_delay_ms = f64::from(delay),
                None => return GifStep::Idle,
            }
        }
        self.in_frame_ms += dt_ms;
        if self.in_frame_ms < self.current_delay_ms {
            return GifStep::Idle;
        }
        let Some(frame) = slot.next.take() else {
            self.in_frame_ms = self.current_delay_ms;
            return GifStep::Idle;
        };
        drop(slot);
        handle.want_next();
        let shown_for = self.in_frame_ms - self.current_delay_ms;
        self.current_delay_ms = f64::from(frame.delay_ms);
        self.in_frame_ms = shown_for.clamp(0.0, self.current_delay_ms);
        GifStep::Blit(frame.pixels)
    }
}

// ── The GIF worker ──────────────────────────────────────────────────────
//
// One thread decodes every playing GIF, each one frame ahead of its
// player, and waits on its channel in between (no timer). It starts
// with the first stream and ends when the last one closes.

/// A frame decoded ahead, waiting for its player to show it.
struct ReadyFrame {
    pixels: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    delay_ms: u32,
}

/// What a player and the worker share.
#[derive(Default)]
struct GifSlot {
    /// Frame 0's delay, once the stream opened (its pixels are what the
    /// row shows already).
    first_delay_ms: Option<u32>,
    next: Option<ReadyFrame>,
    /// The stream would not open, or stopped for good.
    failed: bool,
}

enum GifJob {
    Open {
        id: u64,
        bytes: Arc<[u8]>,
        slot: Arc<std::sync::Mutex<GifSlot>>,
    },
    Next {
        id: u64,
    },
    Close {
        id: u64,
    },
}

/// A player's stream on the worker; closed when dropped.
struct GifHandle {
    id: u64,
    slot: Arc<std::sync::Mutex<GifSlot>>,
}

#[derive(Default)]
struct GifWorker {
    sender: Option<std::sync::mpsc::Sender<GifJob>>,
    next_id: u64,
    open: usize,
}

thread_local! {
    static GIF_WORKER: std::cell::RefCell<GifWorker> = std::cell::RefCell::default();
}

impl GifHandle {
    fn open(bytes: Arc<[u8]>) -> Self {
        let slot = Arc::new(std::sync::Mutex::new(GifSlot::default()));
        let id = GIF_WORKER.with_borrow_mut(|w| {
            w.next_id += 1;
            w.open += 1;
            w.next_id
        });
        let handle = Self {
            id,
            slot: Arc::clone(&slot),
        };
        if !send_gif_job(GifJob::Open { id, bytes, slot }) {
            handle.lock().failed = true;
        }
        handle
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GifSlot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn want_next(&self) {
        if !send_gif_job(GifJob::Next { id: self.id }) {
            self.lock().failed = true;
        }
    }
}

impl Drop for GifHandle {
    fn drop(&mut self) {
        send_gif_job(GifJob::Close { id: self.id });
        GIF_WORKER.with_borrow_mut(|w| {
            w.open = w.open.saturating_sub(1);
            if w.open == 0 {
                // The worker ends once it has read the Close.
                w.sender = None;
            }
        });
    }
}

/// Hand `job` to the worker, starting it when none runs. `false` when
/// there is no thread to do it.
fn send_gif_job(job: GifJob) -> bool {
    let sender = GIF_WORKER.with_borrow_mut(|w| {
        if w.sender.is_none() && !matches!(job, GifJob::Close { .. }) {
            w.sender = start_gif_worker();
        }
        w.sender.clone()
    });
    sender.is_some_and(|tx| tx.send(job).is_ok())
}

fn start_gif_worker() -> Option<std::sync::mpsc::Sender<GifJob>> {
    let (tx, rx) = std::sync::mpsc::channel::<GifJob>();
    let spawned = std::thread::Builder::new()
        .name("gif-decode".into())
        .spawn(move || {
            let mut streams: std::collections::HashMap<
                u64,
                (gif::GifStream, Arc<std::sync::Mutex<GifSlot>>),
            > = std::collections::HashMap::new();
            while let Ok(job) = rx.recv() {
                match job {
                    GifJob::Open { id, bytes, slot } => {
                        let opened = gif::GifStream::open(bytes)
                            .map_err(|e| warn!("GIF stream open failed: {e}"))
                            .ok()
                            .and_then(|mut stream| {
                                let first = stream.next_frame()?;
                                Some((stream, first.delay_ms))
                            });
                        let Some((mut stream, first_delay)) = opened else {
                            lock_slot(&slot).failed = true;
                            continue;
                        };
                        lock_slot(&slot).first_delay_ms = Some(first_delay);
                        decode_ahead(&mut stream, &slot);
                        streams.insert(id, (stream, slot));
                    }
                    GifJob::Next { id } => {
                        if let Some((stream, slot)) = streams.get_mut(&id) {
                            decode_ahead(stream, slot);
                        }
                    }
                    GifJob::Close { id } => {
                        streams.remove(&id);
                    }
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            warn!("GIF playback: no worker thread: {e}");
            None
        }
    }
}

fn lock_slot(slot: &std::sync::Mutex<GifSlot>) -> std::sync::MutexGuard<'_, GifSlot> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Decode the frame after the one handed out last, looping at the end,
/// into the player's slot.
fn decode_ahead(stream: &mut gif::GifStream, slot: &std::sync::Mutex<GifSlot>) {
    let frame = stream
        .next_frame()
        .or_else(|| stream.rewind().then(|| stream.next_frame()).flatten());
    let mut slot = lock_slot(slot);
    match frame {
        Some(frame) => {
            let (width, height) = stream.dimensions();
            if frame.rgba.len() == width as usize * height as usize * 4 {
                slot.next = Some(ReadyFrame {
                    pixels: slint::SharedPixelBuffer::clone_from_slice(&frame.rgba, width, height),
                    delay_ms: frame.delay_ms,
                });
            } else {
                slot.failed = true;
            }
        }
        None => slot.failed = true,
    }
}

// ── Outgoing: paste chain, attach, preview ──────────────────────────────

/// The paste chain, highest-fidelity source first (ported semantics):
///   1. a copied image *file* (Linux file-manager copies, and pasted
///      paths/URIs)
///   2. native clipboard PNG (screenshots)
///   3. pasted text that is an image URL → fetch
///   4. plain text → append to the draft
///   5. no text at all: the clipboard's picture as pixels (the Print
///      Screen key, a paint program, every picture on Linux)
pub fn paste_into_chat(app: &mut AppState) {
    // 1. Copied file (platform clipboards expose these differently —
    //    clipboard.rs handles each).
    if let Some(path) = crate::clipboard::clipboard_file()
        && images::is_image_path(&path)
    {
        attach_path(app, &path);
        return;
    }

    // 2. Native image data (PNG from screenshots etc.), prepared off
    //    the UI thread: a large screenshot's re-encode held it (review
    //    2026-10, P2-18).
    if let Some(png) = crate::clipboard::clipboard_png() {
        let budget = app.image_budget();
        prepare_off_thread(move || images::load_image_bytes(png, budget).map_err(|e| describe(&e)));
        return;
    }

    // 3./4. Text flavors.
    if let Some(text) = crate::clipboard::clipboard_text().filter(|text| !text.is_empty()) {
        if let Some(path) = images::first_image_path_in(&text) {
            attach_path(app, &path);
            return;
        }
        #[cfg(feature = "desktop")]
        if let Some(url) = images::image_url_in(&text) {
            fetch_image_url(app, &url);
            return;
        }
        // Plain text — the capture-key handler consumed the paste
        // keystroke, so insert it ourselves (append; chat drafts are
        // effectively always cursor-at-end).
        let s = app.main.global::<State>();
        let mut draft = s.get_draft().to_string();
        draft.push_str(&app_core::chat::scrub_control_chars(&text));
        let capped: String = draft
            .chars()
            .take(app_core::chat::MAX_MESSAGE_CHARS)
            .collect();
        s.set_draft(capped.into());
        return;
    }

    // 5. A picture and no text beside it (review P2-18).
    if let Some((width, height, rgba)) = crate::clipboard::clipboard_pixels() {
        match images::load_rgba(width, height, rgba, app.image_budget()) {
            Ok(bytes) => show_preview(app, bytes),
            Err(e) => app.show_error(&describe(&e)),
        }
    }
}

/// A file dropped on the window from the OS: the same road as a picked
/// or pasted path. Several files arrive as several drops; a picture
/// takes the preview (the last one wins it), anything else uploads.
pub fn drop_path(app: &mut AppState, path: &std::path::Path) {
    if !app.connected {
        return;
    }
    attach_path(app, path);
}

/// Load an image file from disk into the preview.
/// A picked path: an image previews and goes the image way; anything
/// else uploads as a blob and posts as a card (spec §7).
fn attach_path(app: &mut AppState, path: &std::path::Path) {
    let is_image = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        let lower = e.to_ascii_lowercase();
        images::IMAGE_EXTENSIONS.contains(&lower.as_str())
    });
    if !is_image {
        start_file_upload(app, path);
        return;
    }
    // Read, decode and shrink on a thread of their own: a camera's
    // photo is a second of work, and a huge file would be read whole
    // first (review 2026-10, P2-6).
    if std::fs::metadata(path).is_ok_and(|m| m.len() > client::protocol::MAX_BLOB_BYTES as u64) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        app.show_error(&t!(
            "file-too-large",
            "name" => name,
            "max" => client::protocol::MAX_BLOB_BYTES / (1024 * 1024)
        ));
        return;
    }
    let path = path.to_path_buf();
    let budget = app.image_budget();
    let spawned = std::thread::Builder::new()
        .name("picture-load".into())
        .spawn(move || {
            let loaded = std::fs::read(&path)
                .map_err(|e| t!("chat-image-load-error", "error" => e.to_string()))
                .and_then(|raw| images::load_image_bytes(raw, budget).map_err(|e| describe(&e)));
            let _ = slint::invoke_from_event_loop(move || {
                crate::app::with_current(|app| match loaded {
                    Ok(bytes) => show_preview(app, bytes),
                    Err(line) => app.show_error(&line),
                });
            });
        });
    if let Err(e) = spawned {
        warn!("attach: no loader thread: {e}");
    }
}

/// Read a file and announce it to the server; `UploadDone` posts it
/// into the open conversation (`AppState::file_uploaded`).
///
/// The size is checked from the file's metadata before anything is
/// read, and the read happens on a thread of its own: a 4 GB drop used
/// to be read whole on the UI thread first (review 2026-10, P2-6).
fn start_file_upload(app: &mut AppState, path: &std::path::Path) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let too_large = |app: &mut AppState, name: &str| {
        app.show_error(&t!(
            "file-too-large",
            "name" => name,
            "max" => client::protocol::MAX_BLOB_BYTES / (1024 * 1024)
        ));
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() == 0 || meta.len() > client::protocol::MAX_BLOB_BYTES as u64 => {
            too_large(app, &name);
            return;
        }
        Ok(_) => {}
        Err(e) => {
            let line = t!("file-read-error", "name" => name.as_str(), "error" => e.to_string());
            app.show_error(&line);
            return;
        }
    }
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("file-read".into())
        .spawn(move || {
            let read = std::fs::read(&path);
            let _ = slint::invoke_from_event_loop(move || {
                crate::app::with_current(|app| match read {
                    // The file may have grown since its size was read.
                    Ok(bytes) if bytes.is_empty() || bytes.len() > client::protocol::MAX_BLOB_BYTES => {
                        too_large(app, &name);
                    }
                    Ok(bytes) => {
                        let mime = mime_for(&name).to_string();
                        app.begin_file_upload(name, mime, bytes);
                    }
                    Err(e) => {
                        let line =
                            t!("file-read-error", "name" => name.as_str(), "error" => e.to_string());
                        app.show_error(&line);
                    }
                });
            });
        });
    if let Err(e) = spawned {
        warn!("file upload: no reader thread: {e}");
    }
}

/// A MIME type from the extension, for the server to keep when it
/// cannot sniff one; anything unknown is an octet stream.
pub(crate) fn mime_for(name: &str) -> &'static str {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "txt" | "log" | "md" | "csv" | "toml" | "yaml" | "yml" | "rs" | "py" | "js" | "ts" => {
            "text/plain"
        }
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Write a fetched file into the Downloads folder (spec §7), never
/// over an existing one: through the host where it has its own way
/// there (Android: `MediaStore`, the public Downloads), else as a plain
/// file in the folder.
///
/// # Errors
/// The host's or the file system's message.
pub fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<crate::host::Saved, String> {
    if let Some(saved) = crate::host::save_download(name, mime_for(name), bytes) {
        return saved;
    }
    let path = write_into_downloads(name, bytes).map_err(|e| e.to_string())?;
    Ok(crate::host::Saved {
        shown: path.display().to_string(),
        target: path,
    })
}

fn write_into_downloads(name: &str, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    // One path component, and never over a file that is there.
    let path = app_core::files::write_new(&dir, &app_core::files::safe_name(name), bytes)?;
    mark_as_downloaded(&path);
    Ok(path)
}

/// Tell the system the file came from elsewhere, as a browser does, so
/// it asks before running it: on Windows the Mark of the Web
/// (`Zone.Identifier`, zone 3: `SmartScreen`, Office's protected view);
/// on macOS the quarantine flag (Gatekeeper). It used to be written
/// plainly, a program from a DM as trusted as one of ours (review
/// 2026-10, P1-9; PRODUCTION-PLAN CLI-8).
fn mark_as_downloaded(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        if let Err(e) = std::fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n") {
            warn!("downloads: no Zone.Identifier on {}: {e}", path.display());
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // flags;time;agent — the form Safari and Mail write.
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let value = format!("0083;{secs:x};baucord;");
        // SAFETY: both strings are NUL-terminated (CString) or sized
        // (the value's length is passed), and the call reads them only.
        let set = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if set != 0 {
            warn!("downloads: no quarantine flag on {}", path.display());
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = path;
}

/// Async native file dialog → attach. rfd's portal backend is async;
/// run it on a scratch thread and hop back via the event loop. Any
/// file: images preview, the rest upload as cards.
pub fn attach_image_from_disk() {
    crate::host::pick_file(None, |path| {
        let _ = slint::invoke_from_event_loop(move || {
            crate::app::with_current(move |app| attach_path(app, &path));
        });
    });
}

/// Run `prepare` (read, fetch, decode, re-encode a picture) on a
/// thread of its own, then show its preview — or say why not — on the
/// UI thread.
fn prepare_off_thread(prepare: impl FnOnce() -> Result<Vec<u8>, String> + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("picture-load".into())
        .spawn(move || {
            let prepared = prepare();
            let _ = slint::invoke_from_event_loop(move || {
                crate::app::with_current(|app| match prepared {
                    Ok(bytes) => show_preview(app, bytes),
                    Err(line) => app.show_error(&line),
                });
            });
        });
    if let Err(e) = spawned {
        warn!("picture: no loader thread: {e}");
    }
}

/// Fetch a pasted image URL off-thread, then preview it; a toast says
/// the fetch is running. Desktop only: the phones have no clipboard to
/// paste from (`stubs/clipboard.rs`).
#[cfg(feature = "desktop")]
fn fetch_image_url(app: &mut AppState, url: &str) {
    app.show_toast(ui::ToastKind::Info, &t!("chat-downloading-image"));
    let url = url.to_string();
    info!("Fetching pasted image URL: {url}");
    let budget = app.image_budget();
    // Fetched and prepared on this thread, the picture's re-encode
    // included: it ran on the UI thread once fetched (P2-18).
    prepare_off_thread(move || {
        fetch_bytes(&url)
            .map_err(|e| t!("chat-image-load-error", "error" => e.as_str()))
            .and_then(|raw| images::load_image_bytes(raw, budget).map_err(|e| describe(&e)))
    });
}

#[cfg(feature = "desktop")]
fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::config::Config::builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
    // Cap the read at budget+1 so an oversized body fails fast instead
    // of buffering hundreds of MB.
    // `.take` is std::io::Read's — fully qualified so it can't resolve
    // to Iterator::take on some future BodyReader.
    let mut reader = std::io::Read::take(
        resp.body_mut().as_reader(),
        client::MAX_CHAT_IMAGE_BYTES as u64 + 1,
    );
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut buf).map_err(|e| e.to_string())?;
    if buf.len() > client::MAX_CHAT_IMAGE_BYTES {
        return Err(t!("chat-image-too-large",
            "kb" => (buf.len() / 1024).to_string()));
    }
    Ok(buf)
}

/// Fill and open the in-window preview overlay. Animated GIFs play in
/// the preview (looping via the app's animation ticker), same as the
/// FLTK build.
fn show_preview(app: &mut AppState, bytes: Vec<u8>) {
    // At the size it is drawn; the caption still gives the picture's
    // own dimensions.
    let max_edge = ui::convert::send_preview_max_edge(app.main.window().scale_factor());
    match ui::convert::decode_to_preview(&bytes, max_edge) {
        Ok((image, w, h)) => {
            let kb = bytes.len() / 1024;
            let s = app.main.global::<State>();
            s.set_preview_image(image.clone());
            s.set_preview_caption(format!("{w}×{h} — {kb} KB").into());
            s.set_preview_open(true);

            // An animation plays in the preview, frame by frame from
            // its own copy of the (compressed) bytes.
            app.preview_gif = (bytes.starts_with(b"GIF8") && images::gif_is_animated(&bytes))
                .then(|| GifPlayer::looping(bytes.as_slice().into(), image.clone()));
            if app.preview_gif.is_some() {
                app.ensure_gif_ticker();
            }

            app.set_pending_preview(bytes);
        }
        Err(e) => app.show_error(&describe(&e)),
    }
}

/// "Copy image": the row's picture onto the clipboard as pixels.
pub fn copy_image(_app: &AppState, row_id: i32) {
    if let Some((bytes, _)) = crate::pictures::bytes(row_id) {
        crate::clipboard::set_clipboard_image(&bytes);
    }
}

/// Save an image row to disk via the native save dialog.
pub fn save_image_dialog(app: &mut AppState, row_id: i32) {
    let Some((bytes, _)) = crate::pictures::bytes(row_id) else {
        return;
    };
    // Named for what the bytes are: they are written as received.
    let ext = images::extension_for(&bytes);
    // Timestamped so several saves land side by side without renaming.
    let default_name = format!(
        "baucord-{}.{ext}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    // No dialog on this host (Android): straight into Downloads, and a
    // toast says where, as a received file's save does.
    if !crate::host::HAS_SAVE_DIALOG {
        match save_to_downloads(&default_name, &bytes) {
            Ok(saved) => app.show_toast(
                ui::ToastKind::Success,
                &t!("file-saved", "path" => saved.shown.as_str()),
            ),
            Err(e) => app.show_error(&t!("chat-image-save-error", "error" => e.as_str())),
        }
        return;
    }
    let (filter_name, filter) = match ext {
        "gif" => (t!("image-save-filter-gif"), vec!["gif".to_string()]),
        "jpg" => (
            t!("image-save-filter-jpeg"),
            vec!["jpg".to_string(), "jpeg".to_string()],
        ),
        _ => (t!("image-save-filter"), vec!["png".to_string()]),
    };
    crate::host::save_file(&default_name, Some((filter_name, filter)), move |path| {
        let write_result = std::fs::write(&path, &bytes);
        let _ = slint::invoke_from_event_loop(move || {
            crate::app::with_current(move |app| {
                if let Err(e) = write_result {
                    app.show_error(&t!("chat-image-save-error", "error" => e.to_string()));
                } else {
                    info!("Saved image to {}", path.display());
                }
            });
        });
    });
}
