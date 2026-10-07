// viewers.rs — per-peer stream viewers (a window each on the desktop,
// one page inside the main window on single-window hosts), plus the
// in-window image viewer overlay (chat image tapped → darkened scrim,
// ◀ ▶ stepping, save).

use std::rc::Rc;

use app_core::{fps::FpsCounter, stats::FrameRing};
use i18n::t;
use slint::{ComponentHandle, Model};
use ui::{MainWindow, State, StreamViewerWindow};

use crate::app::{self, AppState};

// ── Stream viewer ───────────────────────────────────────────────────────

/// Where a viewer draws (ANDROID-SPEC.md D23). The desktop gives every
/// watched peer a `StreamViewerWindow`; Android has one window per
/// process, so the same `StreamViewerPanel` sits in `MainWindow` as a
/// page bound to `State.watch-*`, and one stream shows at a time.
enum Surface {
    Window(StreamViewerWindow),
    Page(MainWindow),
}

pub struct Viewer {
    surface: Surface,
    fps: FpsCounter,
    ring: FrameRing,
    total_bytes: u64,
    width: u32,
    height: u32,
    peer_name: String,
}

impl Viewer {
    /// Open a viewer for `client_id`, sized to the sharer's announced
    /// dimensions (the window; the page fills `main`).
    // Pixel dimensions (≤ 8K) fit losslessly in f32.
    #[allow(clippy::cast_precision_loss)]
    pub fn open(
        client_id: u64,
        peer_name: &str,
        width: u32,
        height: u32,
        text_scale: f32,
        main: &MainWindow,
    ) -> Result<Self, slint::PlatformError> {
        let surface = if app::SINGLE_WINDOW_HOST {
            let s = main.global::<State>();
            s.set_watch_peer_name(peer_name.into());
            s.set_watch_stats("--".into());
            s.set_watch_frame(slint::Image::default());
            s.set_watch_muted(false);
            s.set_watch_volume(1.0);
            s.set_watch_open(true);
            // A phone watching a stream keeps its screen on (IOS-SPEC.md
            // §14 D45); `close` lets it sleep again.
            crate::host::keep_screen_on(true);
            Surface::Page(main.clone_strong())
        } else {
            let win = StreamViewerWindow::new()?;
            app::fill_tr(&win.global::<ui::Tr>());
            win.global::<ui::Theme>().set_scale(text_scale);
            win.set_peer_name(peer_name.into());
            win.window().set_size(slint::LogicalSize::new(
                width.clamp(320, 1920) as f32,
                (height.clamp(180, 1080) + 40) as f32, // + toolbar
            ));

            // Controls → app actions. (The page's go through
            // Logic.watch-*, wired once in `wire_page`.)
            let id = client_id;
            win.on_volume_changed(move |v| {
                app::with_current(move |app| app.set_stream_volume(id, v));
            });
            win.on_mute_toggled(move |muted| {
                app::with_current(move |app| app.set_stream_muted(id, muted));
            });
            win.on_stop_watching(move || {
                app::with_current(move |app| app.handle_unwatch_peer(id));
            });
            // Your own mic and headphones, from the stream (IOS-SPEC.md
            // §14 D43): the main window's toggles; the new state comes
            // back through `set_self_audio`.
            win.on_self_mute_toggled(|| app::with_current(AppState::handle_toggle_mute));
            win.on_self_deafen_toggled(|| app::with_current(AppState::handle_toggle_deafen));
            win.window().on_close_requested(move || {
                // Unwatch tears the viewer down (including hiding this
                // window); returning HideWindow avoids a flash.
                app::with_current(move |app| app.handle_unwatch_peer(id));
                slint::CloseRequestResponse::HideWindow
            });

            win.set_stats("--".into());
            let weak = win.as_weak();
            let armed = std::rc::Rc::new(std::cell::Cell::new(false));
            crate::app::repaint_on_reveal(win.window(), armed, move || {
                if let Some(w) = weak.upgrade() {
                    w.set_repaint_flip(!w.get_repaint_flip());
                }
            });
            win.show()?;
            Surface::Window(win)
        };

        Ok(Self {
            surface,
            fps: FpsCounter::new(),
            ring: FrameRing::default(),
            total_bytes: 0,
            width,
            height,
            peer_name: peer_name.to_string(),
        })
    }

    /// Bring an already-open viewer forward (the window) / back on
    /// screen (the page, if something hid it).
    pub fn raise(&self) {
        match &self.surface {
            Surface::Window(win) => {
                let _ = win.show();
            }
            Surface::Page(main) => main.global::<State>().set_watch_open(true),
        }
    }

    /// The desktop's text-size setting, pushed to a viewer window; the
    /// page inherits the main window's.
    pub fn apply_text_scale(&self, scale: f32) {
        if let Surface::Window(win) = &self.surface {
            win.global::<ui::Theme>().set_scale(scale);
        }
    }

    /// The session's mute and deafen, for a window's own buttons. The
    /// page binds `State.muted` / `State.deafened` itself.
    pub fn set_self_audio(&self, muted: bool, deafened: bool) {
        if let Surface::Window(win) = &self.surface {
            win.set_self_muted(muted);
            win.set_self_deafened(deafened);
        }
    }

    /// True for the viewer drawn as the main window's page.
    #[must_use]
    pub fn is_page(&self) -> bool {
        matches!(self.surface, Surface::Page(_))
    }

    fn set_title(&self, title: slint::SharedString) {
        match &self.surface {
            // `title` is bound to peer-name in the .slint — pushing the
            // resolution through it retitles the OS window.
            Surface::Window(win) => win.set_peer_name(title),
            Surface::Page(main) => main.global::<State>().set_watch_peer_name(title),
        }
    }

    fn set_frame(&self, image: slint::Image) {
        match &self.surface {
            Surface::Window(win) => win.set_frame(image),
            Surface::Page(main) => main.global::<State>().set_watch_frame(image),
        }
    }

    fn set_stats(&self, stats: slint::SharedString) {
        match &self.surface {
            Surface::Window(win) => win.set_stats(stats),
            Surface::Page(main) => main.global::<State>().set_watch_stats(stats),
        }
    }

    /// Blit a decoded frame and refresh the once-per-second stats line.
    #[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
    pub fn update_frame(
        &mut self,
        buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
        width: u32,
        height: u32,
        frame_bytes: usize,
        buffer_depth_ms: f32,
        buffer_target_ms: f32,
        buffer_jitter_ms: f32,
    ) {
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.set_title(
                t!("viewer-title",
                    "name" => self.peer_name.as_str(),
                    "width" => width,
                    "height" => height)
                .into(),
            );
        }

        // The pixel copy already happened on the present-prep thread;
        // this is a cheap handle wrap + property set.
        self.set_frame(slint::Image::from_rgba8(buffer));

        self.total_bytes += frame_bytes as u64;
        self.ring.push(frame_bytes as u64);
        let fps = self.fps.tick();

        if self.fps.second_started() {
            let avg_kb = self.ring.avg_kb();
            let throughput_kb_s = avg_kb * fps as f32;
            let total_mb = self.total_bytes as f64 / (1024.0 * 1024.0);
            self.set_stats(
                t!("viewer-stats",
                    "res" => format!("{width}x{height}"),
                    "fps" => fps,
                    "kbps" => format!("{throughput_kb_s:.0}"),
                    "mb" => format!("{total_mb:.1}"),
                    "depth" => format!("{buffer_depth_ms:.0}"),
                    "target" => format!("{buffer_target_ms:.0}"),
                    "jitter" => format!("{buffer_jitter_ms:.0}"))
                .into(),
            );
        }
    }

    pub fn close(&self) {
        match &self.surface {
            Surface::Window(win) => {
                let _ = win.hide();
            }
            Surface::Page(main) => {
                let s = main.global::<State>();
                s.set_watch_open(false);
                crate::host::keep_screen_on(false);
                // The last frame's texture goes with the page.
                s.set_watch_frame(slint::Image::default());
                // The page held the keyboard focus: hand it back, or
                // the next Back finds no handler and closes the app.
                main.invoke_focus_composer();
            }
        }
    }
}

/// The page's controls → app actions, once per process (single-window
/// hosts only; the desktop wires each window in `Viewer::open`). The
/// page shows one stream, so the target is whichever viewer is a page.
pub(crate) fn wire_page(main: &MainWindow) {
    let logic = main.global::<ui::Logic>();
    logic.on_watch_volume_changed(|v| {
        app::with_current(move |app| {
            if let Some(id) = app.page_viewer_id() {
                app.set_stream_volume(id, v);
            }
        });
    });
    logic.on_watch_mute_toggled(|muted| {
        app::with_current(move |app| {
            if let Some(id) = app.page_viewer_id() {
                app.set_stream_muted(id, muted);
            }
        });
    });
    logic.on_watch_stop(|| {
        app::with_current(|app| {
            if let Some(id) = app.page_viewer_id() {
                app.handle_unwatch_peer(id);
            } else {
                // No viewer behind the page (it was torn down by a
                // disconnect, say): just close it.
                app.main.global::<State>().set_watch_open(false);
                crate::host::keep_screen_on(false);
            }
        });
    });
}

// ── Image viewer overlay ────────────────────────────────────────────────

/// GIF playback state for the overlay: loops continuously while shown
/// (no autoplay budget — matches the old viewer's behavior), advanced
/// by the app's animation ticker.
pub struct GalleryGif {
    pub anim: Rc<app_core::gif::GifAnimation>,
    pub idx: usize,
    pub in_frame_ms: f64,
}

/// Every image row-id in chat order (stepping order).
fn image_row_ids(app: &AppState) -> Vec<i32> {
    let model = app.chat_model();
    (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .filter(|r| r.kind == 2)
        .map(|r| r.row_id)
        .collect()
}

/// Open (or retarget) the overlay at `row_id`.
pub fn open_gallery(app: &mut AppState, row_id: i32) {
    let model = app.chat_model().clone();
    let row = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .find(|r| r.row_id == row_id);
    let Some(row) = row else { return };
    let Some(entry) = app.images.get(&row_id) else {
        return;
    };
    let bytes = entry.bytes.clone();
    let is_gif = entry.is_gif;

    let ids = image_row_ids(app);
    let pos = ids.iter().position(|&id| id == row_id).unwrap_or(0);

    app.gallery_row = Some(row_id);
    app.gallery_gif = None;

    let s = app.main.global::<State>();
    s.set_viewer_heading(format!("[{}] {}", row.ts, row.user).into());
    s.set_viewer_has_prev(pos > 0);
    s.set_viewer_has_next(pos + 1 < ids.len());

    if is_gif {
        if let Ok(anim) = app_core::gif::decode_animation(&bytes) {
            if let Some(first) = anim.frames.first() {
                s.set_viewer_image(ui::convert::rgba_image(
                    &first.rgba,
                    anim.width,
                    anim.height,
                ));
            }
            app.gallery_gif = Some(GalleryGif {
                anim: Rc::new(anim),
                idx: 0,
                in_frame_ms: 0.0,
            });
            app.ensure_gif_ticker();
        } else if let Ok((img, _, _)) = ui::convert::decode_to_image(&bytes) {
            s.set_viewer_image(img);
        }
    } else if let Ok((img, _, _)) = ui::convert::decode_to_image(&bytes) {
        s.set_viewer_image(img);
    }

    app.main.global::<State>().set_viewer_open(true);
}

/// Move to the previous/next image row.
pub fn step_gallery(app: &mut AppState, dir: i32) {
    let ids = image_row_ids(app);
    let Some(current) = app.gallery_row else {
        return;
    };
    let Some(pos) = ids.iter().position(|&id| id == current) else {
        return;
    };
    let next = if dir < 0 {
        pos.checked_sub(1)
    } else {
        (pos + 1 < ids.len()).then_some(pos + 1)
    };
    if let Some(next) = next {
        open_gallery(app, ids[next]);
    }
}

/// Close the overlay and drop any GIF playback state.
pub fn close_gallery(app: &mut AppState) {
    app.gallery_row = None;
    app.gallery_gif = None;
    let s = app.main.global::<State>();
    s.set_viewer_open(false);
    // The full-resolution decode goes with the overlay; the row keeps
    // only its preview-sized texture.
    s.set_viewer_image(slint::Image::default());
}
