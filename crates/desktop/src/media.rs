// media.rs — chat images: ingestion (paste/attach/URL), the outgoing
// preview, and inline GIF playback.
//
// FLTK's Fl_Anim_GIF is gone; Slint has no animated Image. The
// replacement is a small frame driver: app-core decodes the GIF to
// RGBA frames, a `slint::Timer` steps them into the chat row via
// `set_row_data`. Autoplay policy is unchanged: ~3 loops (≤30 s),
// freeze to frame 0 (dropping the decoded frames), replay on hover.

use std::{
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use app_core::{
    gif::{self, GifAnimation},
    images::{self, ImageError},
};
use i18n::t;
use log::{info, warn};
use slint::{ComponentHandle, Model};
use ui::State;

use crate::app::AppState;

/// Original wire bytes of a chat image row, kept for save / gallery /
/// GIF replay.
pub struct ImageEntry {
    pub bytes: Rc<Vec<u8>>,
    pub is_gif: bool,
}

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

/// Append an image message to chat. Returns false if the bytes don't
/// decode (the caller posts a system-row notice instead).
pub fn push_image_row(app: &mut AppState, meta: &app_core::chat::Meta, data: Vec<u8>) -> bool {
    // Preview-sized texture for the row (the layout box still comes
    // from the original dimensions); the gallery viewer decodes the
    // full resolution from `app.images` on demand.
    let max_edge = ui::convert::preview_max_edge(app.main.window().scale_factor());
    let Ok((frame, w, h)) = ui::convert::decode_to_preview(&data, max_edge) else {
        return false;
    };
    let is_gif = data.starts_with(b"GIF8") && images::gif_is_animated(&data);

    app.mark_day(meta.day);
    let delta = app.history_mut().push_image(meta, w, h, is_gif);
    // sync_history_tail equivalent: history rows were appended; mirror
    // them (push_image appends exactly one row and may drop from front).
    app.sync_delta(&delta);

    // The freshly appended row is the last one — install the decoded
    // frame (convert::chat_row only sets a placeholder).
    let last = app.chat_model().row_count() - 1;
    install_image(app, last, frame, data, is_gif);
    true
}

/// Put a decoded picture on the image row at `index` and register its
/// bytes (and player, for a GIF) under that row's id.
fn install_image(
    app: &mut AppState,
    index: usize,
    frame: slint::Image,
    data: Vec<u8>,
    is_gif: bool,
) {
    let model = app.chat_model().clone();
    let Some(mut row) = model.row_data(index) else {
        return;
    };
    let preview = frame.clone();
    row.image = frame;
    let row_id = row.row_id;
    model.set_row_data(index, row);

    let bytes = Rc::new(data);
    app.images.insert(
        row_id,
        ImageEntry {
            bytes: bytes.clone(),
            is_gif,
        },
    );

    if is_gif {
        let mut player = GifPlayer::new(bytes, preview);
        player.start_autoplay();
        app.gifs_mut().insert(row_id, player);
        app.ensure_gif_ticker();
    }
}

/// A picture that came as a file (a DM attachment, spec §7): the card
/// for message `msg_id` in the shown conversation becomes an image row
/// with `data`. `false` when the bytes are not a picture after all.
pub fn picture_from_file(app: &mut AppState, msg_id: u64, data: Vec<u8>) -> bool {
    let max_edge = ui::convert::preview_max_edge(app.main.window().scale_factor());
    let Ok((frame, w, h)) = ui::convert::decode_to_preview(&data, max_edge) else {
        return false;
    };
    let is_gif = data.starts_with(b"GIF8") && images::gif_is_animated(&data);
    let Some(index) = app.history_mut().attach_image(msg_id, w, h, is_gif) else {
        return false;
    };
    let row = app.chat_row_at(index);
    app.chat_model().set_row_data(index, row);
    install_image(app, index, frame, data, is_gif);
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
    /// Showing frame 0, decoded frames released.
    Frozen,
}

/// One tick's outcome for a player.
pub enum GifStep {
    /// Nothing to draw (frozen, or the current frame hasn't elapsed).
    Idle,
    /// Draw this freshly decoded frame (full logical screen).
    Blit {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// Playback ended: restore the row's static preview texture.
    Freeze { first: slint::Image },
}

/// Chat-row GIF playback state, advanced by the app's single 33 ms
/// animation ticker (`AppState::gif_tick`) instead of per-player
/// timers — one repeating timer for any number of playing GIFs, and
/// no timer-restarted-from-its-own-callback patterns.
///
/// Frames are decoded *as they play* ([`gif::GifStream`]): a playing
/// GIF holds one frame instead of the whole animation, so several
/// playing at once cost megabytes, not the old 48 MB-per-GIF budget.
/// Frozen players hold nothing but the compressed bytes (shared with
/// the viewer map) and the row's preview texture handle.
pub struct GifPlayer {
    bytes: Rc<Vec<u8>>,
    /// The row's static preview — the freeze target. Cloning a
    /// `slint::Image` clones a handle, not pixels, and this buffer is
    /// alive anyway (it's what the frozen row displays).
    first: slint::Image,
    /// Present only while playing.
    stream: Option<gif::GifStream>,
    in_frame_ms: f64,
    current_delay_ms: f64,
    mode: GifMode,
}

impl GifPlayer {
    pub fn new(bytes: Rc<Vec<u8>>, first: slint::Image) -> Self {
        Self {
            bytes,
            first,
            stream: None,
            in_frame_ms: 0.0,
            current_delay_ms: 0.0,
            mode: GifMode::Frozen,
        }
    }

    /// Open the frame stream. Frame 0's pixels are discarded (the
    /// preview already shows it) but its delay starts the clock.
    fn open_stream(&mut self) -> bool {
        let mut stream = match gif::GifStream::open(Rc::clone(&self.bytes)) {
            Ok(stream) => stream,
            Err(e) => {
                warn!("GIF stream open failed: {e}");
                return false;
            }
        };
        let Some(first) = stream.next_frame() else {
            return false;
        };
        self.current_delay_ms = f64::from(first.delay_ms);
        self.in_frame_ms = 0.0;
        self.stream = Some(stream);
        true
    }

    pub fn start_autoplay(&mut self) {
        let budget = GifAnimation::autoplay_budget_secs(&self.bytes);
        if self.open_stream() {
            self.mode = GifMode::Autoplay {
                deadline: Instant::now() + Duration::from_secs_f64(budget),
            };
        }
    }

    /// Pointer entered/left the image. Returns true when playback (and
    /// therefore the ticker) should be running.
    pub fn hover(&mut self, entering: bool) -> bool {
        if entering {
            if matches!(self.mode, GifMode::Frozen) && self.open_stream() {
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

    pub fn playing(&self) -> bool {
        !matches!(self.mode, GifMode::Frozen)
    }

    /// Stop playback immediately and release the stream, returning the
    /// preview to restore — used when the window hides to the tray.
    pub fn force_freeze(&mut self) -> Option<slint::Image> {
        if !self.playing() {
            return None;
        }
        self.mode = GifMode::Frozen;
        self.stream = None;
        self.in_frame_ms = 0.0;
        Some(self.first.clone())
    }

    /// Advance by `dt_ms` of wall time, decoding at most a few frames
    /// of catch-up so a stall slows playback instead of spiraling.
    pub fn step(&mut self, dt_ms: f64) -> GifStep {
        if !self.playing() {
            return GifStep::Idle;
        }
        if let GifMode::Autoplay { deadline } = self.mode
            && Instant::now() >= deadline
        {
            self.mode = GifMode::Frozen;
            self.stream = None;
            self.in_frame_ms = 0.0;
            return GifStep::Freeze {
                first: self.first.clone(),
            };
        }
        let Some(stream) = &mut self.stream else {
            return GifStep::Idle;
        };

        self.in_frame_ms += dt_ms;
        if self.in_frame_ms < self.current_delay_ms {
            return GifStep::Idle;
        }
        let mut latest = None;
        for _ in 0..4 {
            self.in_frame_ms -= self.current_delay_ms;
            let frame = match stream.next_frame() {
                Some(frame) => frame,
                None => {
                    // End of stream: loop.
                    if !stream.rewind() {
                        break;
                    }
                    match stream.next_frame() {
                        Some(frame) => frame,
                        None => break,
                    }
                }
            };
            self.current_delay_ms = f64::from(frame.delay_ms);
            latest = Some(frame);
            if self.in_frame_ms < self.current_delay_ms {
                break;
            }
        }
        match latest {
            Some(frame) => {
                // Drop any remaining catch-up debt: decoding more than
                // a few frames per tick would stall the UI thread.
                self.in_frame_ms = self.in_frame_ms.min(self.current_delay_ms);
                let (width, height) = self
                    .stream
                    .as_ref()
                    .map_or((0, 0), gif::GifStream::dimensions);
                GifStep::Blit {
                    rgba: frame.rgba,
                    width,
                    height,
                }
            }
            None => {
                // Rewind failed or stream empty: end playback cleanly.
                self.mode = GifMode::Frozen;
                self.stream = None;
                GifStep::Freeze {
                    first: self.first.clone(),
                }
            }
        }
    }
}

// ── Outgoing: paste chain, attach, preview ──────────────────────────────

/// The paste chain, highest-fidelity source first (ported semantics):
///   1. a copied image *file* (Linux file-manager copies, and pasted
///      paths/URIs)
///   2. native clipboard PNG (screenshots)
///   3. pasted text that is an image URL → fetch
///   4. plain text → append to the draft
pub fn paste_into_chat(app: &mut AppState) {
    // 1. Copied file (platform clipboards expose these differently —
    //    clipboard.rs handles each).
    if let Some(path) = crate::clipboard::clipboard_file()
        && images::is_image_path(&path)
    {
        attach_path(app, &path);
        return;
    }

    // 2. Native image data (PNG from screenshots etc.).
    if let Some(png) = crate::clipboard::clipboard_png() {
        match images::load_image_bytes(png, app.image_budget()) {
            Ok(bytes) => {
                show_preview(app, bytes);
                return;
            }
            Err(e) => {
                app.show_error(&describe(&e));
                return;
            }
        }
    }

    // 3./4. Text flavors.
    if let Some(text) = crate::clipboard::clipboard_text() {
        if let Some(path) = images::first_image_path_in(&text) {
            attach_path(app, &path);
            return;
        }
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
    }
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
    match std::fs::read(path) {
        Ok(raw) => match images::load_image_bytes(raw, app.image_budget()) {
            Ok(bytes) => show_preview(app, bytes),
            Err(e) => app.show_error(&describe(&e)),
        },
        Err(e) => app.show_error(&t!("chat-image-load-error", "error" => e.to_string())),
    }
}

/// Read a file and announce it to the server; `UploadDone` posts it
/// into the open conversation (`AppState::file_uploaded`).
fn start_file_upload(app: &mut AppState, path: &std::path::Path) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            let line = t!("file-read-error", "name" => name.as_str(), "error" => e.to_string());
            app.show_error(&line);
            return;
        }
    };
    if bytes.is_empty() || bytes.len() > client::protocol::MAX_BLOB_BYTES {
        app.show_error(&t!(
            "file-too-large",
            "name" => name.as_str(),
            "max" => client::protocol::MAX_BLOB_BYTES / (1024 * 1024)
        ));
        return;
    }
    let mime = mime_for(&name).to_string();
    app.begin_file_upload(name, mime, bytes);
}

/// A MIME type from the extension, for the server to keep when it
/// cannot sniff one; anything unknown is an octet stream.
fn mime_for(name: &str) -> &'static str {
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
/// over an existing one. Returns the path written.
pub fn save_to_downloads(name: &str, bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = std::path::Path::new(&safe);
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let ext = base
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let mut path = dir.join(&safe);
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Async native file dialog → attach. rfd's portal backend is async;
/// run it on a scratch thread and hop back via the event loop. Any
/// file: images preview, the rest upload as cards.
pub fn attach_image_from_disk() {
    std::thread::spawn(|| {
        let picked = pollster::block_on(rfd::AsyncFileDialog::new().pick_file());
        if let Some(handle) = picked {
            let path = handle.path().to_path_buf();
            let _ = slint::invoke_from_event_loop(move || {
                crate::app::with_current(move |app| attach_path(app, &path));
            });
        }
    });
}

/// Fetch a pasted image URL off-thread, then preview it; a toast says
/// the fetch is running.
fn fetch_image_url(app: &mut AppState, url: &str) {
    app.show_toast(ui::ToastKind::Info, &t!("chat-downloading-image"));
    let url = url.to_string();
    info!("Fetching pasted image URL: {url}");
    std::thread::spawn(move || {
        let result = fetch_bytes(&url);
        let _ = slint::invoke_from_event_loop(move || {
            crate::app::with_current(move |app| match result {
                Ok(raw) => match images::load_image_bytes(raw, app.image_budget()) {
                    Ok(bytes) => show_preview(app, bytes),
                    Err(e) => app.show_error(&describe(&e)),
                },
                Err(e) => {
                    app.show_error(&t!("chat-image-load-error", "error" => e.as_str()));
                }
            });
        });
    });
}

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
    match ui::convert::decode_to_image(&bytes) {
        Ok((image, w, h)) => {
            let kb = bytes.len() / 1024;
            let s = app.main.global::<State>();
            s.set_preview_image(image);
            s.set_preview_caption(format!("{w}×{h} — {kb} KB").into());
            s.set_preview_open(true);

            app.preview_gif = None;
            if bytes.starts_with(b"GIF8")
                && images::gif_is_animated(&bytes)
                && let Ok(anim) = gif::decode_animation(&bytes)
                && !anim.frames.is_empty()
            {
                app.preview_gif = Some(crate::viewers::GalleryGif {
                    anim: Rc::new(anim),
                    idx: 0,
                    in_frame_ms: 0.0,
                });
                app.ensure_gif_ticker();
            }

            app.set_pending_preview(bytes);
        }
        Err(e) => app.show_error(&describe(&e)),
    }
}

/// Save an image row to disk via the native save dialog.
pub fn save_image_dialog(app: &mut AppState, row_id: i32) {
    let Some(entry) = app.images.get(&row_id) else {
        return;
    };
    let bytes: Arc<Vec<u8>> = Arc::new(entry.bytes.as_ref().clone());
    let (ext, filter) = if entry.is_gif {
        ("gif", &["gif"][..])
    } else {
        ("png", &["png"][..])
    };
    // Timestamped so several saves land side by side without renaming.
    let default_name = format!(
        "baucord-{}.{ext}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let filter_name = if entry.is_gif {
        t!("image-save-filter-gif")
    } else {
        t!("image-save-filter")
    };
    let filter: Vec<String> = filter.iter().map(|s| (*s).to_string()).collect();
    std::thread::spawn(move || {
        let picked = pollster::block_on(
            rfd::AsyncFileDialog::new()
                .set_file_name(&default_name)
                .add_filter(&filter_name, &filter)
                .save_file(),
        );
        if let Some(handle) = picked {
            let path = handle.path().to_path_buf();
            let write_result = std::fs::write(&path, bytes.as_slice());
            let _ = slint::invoke_from_event_loop(move || {
                crate::app::with_current(move |app| {
                    if let Err(e) = write_result {
                        app.show_error(&t!("chat-image-save-error", "error" => e.to_string()));
                    } else {
                        info!("Saved image to {}", path.display());
                    }
                });
            });
        }
    });
}
