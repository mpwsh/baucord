// viewers.rs — per-peer stream viewer windows, plus the in-window
// image viewer overlay (chat image tapped → darkened scrim, ◀ ▶
// stepping, save).

use std::rc::Rc;

use slint::{ComponentHandle, Model};

use app_core::fps::FpsCounter;
use app_core::stats::FrameRing;
use i18n::t;
use ui::{State, StreamViewerWindow};

use crate::app::{self, AppState};

// ── Stream viewer ───────────────────────────────────────────────────────

pub struct Viewer {
    win: StreamViewerWindow,
    fps: FpsCounter,
    ring: FrameRing,
    total_bytes: u64,
    width: u32,
    height: u32,
    peer_name: String,
}

impl Viewer {
    /// Open a viewer window for `client_id`, sized to the sharer's
    /// announced dimensions.
    // Pixel dimensions (≤ 8K) fit losslessly in f32.
    #[allow(clippy::cast_precision_loss)]
    pub fn open(
        client_id: u64,
        peer_name: &str,
        width: u32,
        height: u32,
    ) -> Result<Self, slint::PlatformError> {
        let win = StreamViewerWindow::new()?;
        app::fill_tr(&win.global::<ui::Tr>());
        win.set_peer_name(peer_name.into());
        win.window().set_size(slint::LogicalSize::new(
            width.clamp(320, 1920) as f32,
            (height.clamp(180, 1080) + 40) as f32, // + toolbar
        ));

        // Controls → app actions.
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

        Ok(Self {
            win,
            fps: FpsCounter::new(),
            ring: FrameRing::default(),
            total_bytes: 0,
            width,
            height,
            peer_name: peer_name.to_string(),
        })
    }

    /// Blit a decoded frame and refresh the once-per-second stats line.
    #[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
    pub fn update_frame(
        &mut self,
        rgba: &[u8],
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
            // `title` is bound to peer-name in the .slint — pushing the
            // resolution through it retitles the OS window.
            self.win.set_peer_name(
                t!("viewer-title",
                    "name" => self.peer_name.as_str(),
                    "width" => width,
                    "height" => height)
                .into(),
            );
        }

        self.win
            .set_frame(ui::convert::rgba_image(rgba, width, height));

        self.total_bytes += frame_bytes as u64;
        self.ring.push(frame_bytes as u64);
        let fps = self.fps.tick();

        if self.fps.second_started() {
            let avg_kb = self.ring.avg_kb();
            let throughput_kb_s = avg_kb * fps as f32;
            let total_mb = self.total_bytes as f64 / (1024.0 * 1024.0);
            self.win.set_stats(
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
        let _ = self.win.hide();
    }
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
                    &first.rgba, anim.width, anim.height,
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
    let Some(current) = app.gallery_row else { return };
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
    app.main.global::<State>().set_viewer_open(false);
}
