//! Present-prep thread: the stage between "decoded" and "on screen".
//!
//! Field finding that motivates this: the sharer encodes 60 fps and the
//! viewer *decodes* ~60 fps, but frames used to reach the screen at
//! ~30 — the UI thread paid a 5-8 ms full-frame pixel copy per frame
//! (on top of Slint's own render + the rest of the drain), pushing each
//! present cycle past one frame period and coalescing every other
//! frame away.
//!
//! This thread takes ownership of the screen thread's event channel and
//! splits it:
//!
//! - `FrameReady` events have their pixel copy paid HERE — the RGBA
//!   `Vec` becomes a [`slint::SharedPixelBuffer`] off the UI thread —
//!   and land in a newest-per-stream slot map. The UI thread wraps the
//!   buffer in a `slint::Image` (a cheap handle, not a copy) and sets a
//!   property: well under a millisecond.
//! - Every other event is forwarded untouched for the normal UI-thread
//!   handler.
//!
//! Coalescing (newest frame wins when the UI is busy) happens on the
//! slot map, so a momentarily slow UI drops stale frames *before*
//! anyone pays to convert them — the old design paid the copy first and
//! discarded after.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use client::{screen::ScreenEvent, WakeFn};
use crossbeam_channel::{Receiver, Sender};
use log::{error, warn};

/// A decoded frame whose pixels already live in a Slint-ready buffer.
/// Everything the UI needs to present + log, minus any heavy work.
pub struct PreparedFrame {
    pub client_id: u64,
    pub buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    pub width: u32,
    pub height: u32,
    pub frame_bytes: usize,
    pub seq: u32,
    pub enqueued_at: Instant,
    pub decoded_at: Instant,
    pub buffer_depth_ms: f32,
    pub buffer_target_ms: f32,
    pub buffer_jitter_ms: f32,
}

/// Shared between the prep thread (producer) and the UI drain
/// (consumer).
#[derive(Default)]
pub struct PresentShared {
    /// Newest prepared frame per stream. The UI drain takes the whole
    /// map each pass.
    pub slots: Mutex<HashMap<u64, PreparedFrame>>,
    /// Frames replaced in a slot before the UI took them (rate-limited
    /// warning below). A small trickle is normal scheduling jitter;
    /// a steady stream means the UI still can't keep up.
    coalesced: AtomicU64,
}

/// Spawn the prep thread. Takes over `event_rx` (the UI must stop
/// reading it); non-frame events come back out on `forward_tx`.
pub fn spawn(
    event_rx: Receiver<ScreenEvent>,
    forward_tx: Sender<ScreenEvent>,
    shared: Arc<PresentShared>,
    wake: WakeFn,
) {
    let _ = std::thread::Builder::new()
        .name("present-prep".into())
        .spawn(move || {
            // Blocking recv: the thread sleeps whenever nothing streams.
            while let Ok(evt) = event_rx.recv() {
                match evt {
                    ScreenEvent::FrameReady {
                        client_id,
                        width,
                        height,
                        rgba,
                        frame_bytes,
                        seq,
                        is_keyframe: _,
                        enqueued_at,
                        decoded_at,
                        buffer_depth_ms,
                        buffer_target_ms,
                        buffer_jitter_ms,
                    } => {
                        let expected = width as usize * height as usize * 4;
                        if rgba.len() != expected {
                            error!(
                                "present-prep: {width}x{height} needs {expected} bytes, got {} — dropping frame seq={seq}",
                                rgba.len()
                            );
                            continue;
                        }
                        // A phone app in the background shows nothing:
                        // no copy, no redraw. The decoder keeps going (its
                        // pictures depend on each other), so the next frame
                        // after the return is current.
                        if !crate::app::host_visible() {
                            continue;
                        }
                        // The one full-frame copy, off the UI thread —
                        // straight from the decoder's bytes (`new` +
                        // `copy_from_slice` zero-filled the buffer first:
                        // a second 8 MB write per 1080p frame).
                        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &rgba, width, height,
                        );

                        let prepared = PreparedFrame {
                            client_id,
                            buffer,
                            width,
                            height,
                            frame_bytes,
                            seq,
                            enqueued_at,
                            decoded_at,
                            buffer_depth_ms,
                            buffer_target_ms,
                            buffer_jitter_ms,
                        };
                        let replaced = shared
                            .slots
                            .lock()
                            .unwrap()
                            .insert(client_id, prepared)
                            .is_some();
                        if replaced {
                            let n = shared.coalesced.fetch_add(1, Ordering::Relaxed) + 1;
                            if n.is_multiple_of(60) {
                                warn!(
                                    "present-prep has coalesced {n} frames — UI thread slower than stream"
                                );
                            }
                        }
                        wake();
                    }
                    other => {
                        if forward_tx.send(other).is_err() {
                            break;
                        }
                        wake();
                    }
                }
            }
            // Channel closed: screen thread is gone (shutdown).
        });
}
