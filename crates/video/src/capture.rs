//! Screen capture backends.
//!
//! Two backends, one [`Capturer`] front:
//!
//! - [`StreamCapturer`] — xcap's `Monitor::video_recorder()`. On Windows
//!   this is **DXGI Desktop Duplication**: frames are produced by the OS
//!   on a dedicated thread, only when the screen actually changes, with
//!   near-zero contention with running games. This is the default on
//!   Windows.
//! - [`ScreenCapturer`] — per-frame screenshots via `capture_image()`
//!   (GDI `BitBlt` on Windows, `CGWindowList` on macOS, X11/Wayland
//!   otherwise). Every call synchronously copies a full frame — cheap
//!   enough as a fallback, but the GDI path in particular fights the
//!   compositor and is what made games stutter while sharing. Kept as
//!   the fallback for non-Windows targets and for recorder init failure.
//!
//! [`Capturer::new`] picks the backend and logs which one is active.
//!
//! # Streaming-backend semantics the caller must know
//!
//! DXGI only delivers frames when the screen *changes*. [`Capturer::poll`]
//! therefore distinguishes [`CaptureStatus::Fresh`] (new content — encode
//! it) from [`CaptureStatus::Unchanged`] (nothing new — skip the encode,
//! unless a keyframe is owed to a viewer or the periodic refresh is due).
//! The last frame is held internally so it can be re-encoded on demand
//! ([`Capturer::frame`] with `advance_timestamp = true`).
//!
//! The recorder's capture thread dies on `DXGI_ERROR_ACCESS_LOST`
//! (display mode changes, fullscreen transitions, UAC prompts — routine
//! events around games). `poll()` detects the disconnect and rebuilds
//! the recorder automatically, re-seeding from a one-shot screenshot so
//! the stream shows the post-transition screen even if it is static.

#[cfg(not(any(target_os = "ios", target_os = "android")))]
use std::sync::mpsc::{Receiver, TryRecvError};
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use std::time::{Duration, Instant};

#[cfg(not(any(target_os = "ios", target_os = "android")))]
use log::{info, warn};
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use xcap::Monitor;

use crate::{CaptureError, Frame, Quality};

/// What [`Capturer::poll`] found on this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStatus {
    /// New screen content was captured — encode it.
    Fresh,
    /// Nothing changed since the last frame. Skip the encode unless a
    /// keyframe is pending or the periodic refresh is due; the held
    /// frame is available via [`Capturer::frame`].
    Unchanged,
    /// No frame has been captured yet (startup, or a recorder rebuild
    /// in progress with no prior frame to hold).
    Empty,
}

/// Summary of a connected monitor.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

/// Lists available monitors.
///
/// # Errors
///
/// Returns [`CaptureError::MonitorList`] if the OS fails to enumerate monitors.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
    let monitors = Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;

    Ok(monitors
        .into_iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            index: i,
            name: m.name().unwrap_or_else(|_| format!("Monitor {i}")),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            is_primary: m.is_primary().unwrap_or(false),
        })
        .collect())
}

/// PID of the process owning the currently focused window, if the
/// platform can tell.
///
/// Frontends use this to decide what the share hotkey does: our own
/// window focused → open the share dialog; any other app focused →
/// start streaming immediately with saved defaults. Returns `None`
/// when focus cannot be determined — callers should treat that as
/// "show the dialog" (never start a stream the user didn't clearly
/// ask for).
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub fn focused_pid() -> Option<u32> {
    let windows = xcap::Window::all().ok()?;
    windows
        .into_iter()
        .find(|w| w.is_focused().unwrap_or(false))
        .and_then(|w| w.pid().ok())
}

// ── Backend selection ───────────────────────────────────────────────────

/// Name of the screenshot API behind `capture_image()` on this platform,
/// for backend logging.
#[cfg(target_os = "windows")]
const SCREENSHOT_API: &str = "GDI BitBlt";
#[cfg(target_os = "macos")]
const SCREENSHOT_API: &str = "CGWindowList";
#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "ios",
    target_os = "android"
)))]
const SCREENSHOT_API: &str = "X11/Wayland";

/// Unified capture front: picks the best backend for the platform and
/// exposes the poll/frame API the screen-share loop drives.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub enum Capturer {
    /// OS streaming capture (xcap `VideoRecorder`).
    Stream(StreamCapturer),
    /// Per-frame screenshot capture (legacy path, always available).
    Screenshot(ScreenCapturer),
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
impl Capturer {
    /// Create a capturer for the given monitor index and quality preset.
    ///
    /// Windows: tries the streaming backend (DXGI desktop duplication)
    /// first and falls back to screenshots if it cannot start. Other
    /// platforms: screenshots. The chosen backend is logged either way.
    ///
    /// # Errors
    ///
    /// Returns an error only if *no* backend can be created (monitor
    /// enumeration failed or the index is out of range).
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        // Streaming is Windows-only for now. xcap's recorder backends
        // exist elsewhere too, but: on X11 it is literally an unpaced
        // screenshot loop (worse than ours), and the macOS (AVFoundation)
        // and Wayland (PipeWire portal) paths are unvalidated for this
        // pipeline. Revisit per-platform when there's test coverage.
        #[cfg(target_os = "windows")]
        {
            match StreamCapturer::new(monitor_index, quality) {
                Ok(s) => {
                    info!(
                        "video capture backend: STREAMING — DXGI desktop duplication \
                         (xcap VideoRecorder), change-driven frames"
                    );
                    return Ok(Self::Stream(s));
                }
                Err(e) => {
                    warn!(
                        "streaming capture unavailable, falling back to per-frame \
                         screenshots: {e}"
                    );
                }
            }
        }

        let s = ScreenCapturer::new(monitor_index, quality)?;
        info!(
            "video capture backend: SCREENSHOTS — {SCREENSHOT_API} via xcap capture_image, \
             one full-frame copy per tick"
        );
        Ok(Self::Screenshot(s))
    }

    /// Pump the backend once. See [`CaptureStatus`] for what the caller
    /// should do with each result.
    ///
    /// For the screenshot backend this performs the (blocking) capture;
    /// for the streaming backend it is a non-blocking channel drain.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Capture`] if the screenshot backend fails
    /// to capture. The streaming backend does not error from `poll` — a
    /// dead recorder degrades to `Unchanged`/`Empty` while rebuilding.
    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        match self {
            Self::Stream(s) => Ok(s.poll()),
            Self::Screenshot(s) => s.poll(),
        }
    }

    /// The most recent captured frame, if any.
    ///
    /// With `advance_timestamp`, the frame's timestamp is first bumped
    /// to "now" — use this when re-encoding an unchanged frame so the
    /// encoder's PTS, keyframe cadence, and rate control keep tracking
    /// real time.
    pub fn frame(&mut self, advance_timestamp: bool) -> Option<&Frame> {
        match self {
            Self::Stream(s) => s.frame(advance_timestamp),
            Self::Screenshot(s) => s.frame(advance_timestamp),
        }
    }

    /// Short human-readable backend description for logs/diagnostics.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Stream(_) => "streaming (DXGI desktop duplication)",
            Self::Screenshot(_) => concat_backend_name(),
        }
    }
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
const fn concat_backend_name() -> &'static str {
    // const-friendly "screenshots (<API>)" without runtime formatting.
    #[cfg(target_os = "windows")]
    {
        "screenshots (GDI BitBlt)"
    }
    #[cfg(target_os = "macos")]
    {
        "screenshots (CGWindowList)"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "screenshots (X11/Wayland)"
    }
}

// ── Streaming backend ───────────────────────────────────────────────────

/// How long to wait between recorder rebuild attempts after the capture
/// thread dies. `ACCESS_LOST` around fullscreen transitions typically
/// resolves within a second; retrying every poll tick would just spam
/// failed DXGI setup calls in the meantime.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
const REBUILD_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Streaming capture via xcap's `Monitor::video_recorder()`.
///
/// Frames arrive on a rendezvous channel from a capture thread xcap
/// owns, *only when the screen changes*. This type owns the channel
/// drain, last-frame retention, timestamping (the recorder's frames
/// carry none), and automatic rebuild when the capture thread dies.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub struct StreamCapturer {
    monitor_index: usize,
    quality: Quality,
    recorder: xcap::VideoRecorder,
    rx: Receiver<xcap::Frame>,
    /// Most recent frame, retained for repeat-encodes while static.
    last: Option<Frame>,
    /// `last` holds content the caller hasn't been handed as Fresh yet
    /// (the seed screenshot at startup / after a rebuild).
    fresh_pending: bool,
    /// Whether the recorder's capture thread is believed alive.
    alive: bool,
    start_time: Instant,
    last_rebuild_attempt: Option<Instant>,
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
impl StreamCapturer {
    /// Create and start a streaming capturer for the given monitor.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if the monitor cannot be found or the
    /// OS recorder cannot be created/started (no fallback here — that
    /// is [`Capturer::new`]'s job).
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        let (monitor, recorder, rx) = open_recorder(monitor_index)?;

        let name = monitor.name().unwrap_or_default();
        let w = monitor.width().unwrap_or(0);
        let h = monitor.height().unwrap_or(0);
        info!("StreamCapturer: monitor='{name}' ({w}x{h}), target={quality}");

        let mut cap = Self {
            monitor_index,
            quality,
            recorder,
            rx,
            last: None,
            fresh_pending: false,
            alive: true,
            start_time: Instant::now(),
            last_rebuild_attempt: None,
        };
        // DXGI only delivers on *change* — on a static screen the first
        // real frame may be a long way off (xcap also skips the initial
        // accumulated frame). Seed with one screenshot so sharing shows
        // a picture immediately.
        cap.seed(&monitor);
        Ok(cap)
    }

    /// One-shot screenshot into `last`, marked as fresh. Best-effort:
    /// a failure just means the stream starts on the first real change.
    fn seed(&mut self, monitor: &Monitor) {
        match monitor.capture_image() {
            Ok(image) => {
                let width = image.width();
                let height = image.height();
                self.last = Some(Frame {
                    data: image.into_raw(),
                    width,
                    height,
                    timestamp_ms: self.start_time.elapsed().as_millis() as u64,
                });
                self.fresh_pending = true;
            }
            Err(e) => warn!("seed screenshot failed (stream starts on first change): {e}"),
        }
    }

    /// Non-blocking pump: drain the recorder channel, keep the newest
    /// frame, detect a dead capture thread, and rebuild when needed.
    /// Never blocks and never fails — degraded states report as
    /// `Unchanged` (freeze-frame) or `Empty`.
    pub fn poll(&mut self) -> CaptureStatus {
        // Drain everything available, keeping only the newest. The
        // channel is a rendezvous (capacity 0), so at most one frame is
        // pending — the loop is belt-and-braces.
        let mut newest: Option<xcap::Frame> = None;
        loop {
            match self.rx.try_recv() {
                Ok(f) => newest = Some(f),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.alive {
                        self.alive = false;
                        warn!(
                            "streaming capture thread died (display mode change, \
                             fullscreen transition, or UAC prompt are the usual \
                             causes) — will rebuild"
                        );
                    }
                    break;
                }
            }
        }

        if !self.alive {
            self.try_rebuild();
        }

        if let Some(f) = newest {
            self.last = Some(Frame {
                data: f.raw,
                width: f.width,
                height: f.height,
                timestamp_ms: self.start_time.elapsed().as_millis() as u64,
            });
            self.fresh_pending = false;
            return CaptureStatus::Fresh;
        }

        if self.fresh_pending && self.last.is_some() {
            self.fresh_pending = false;
            return CaptureStatus::Fresh;
        }

        if self.last.is_some() {
            CaptureStatus::Unchanged
        } else {
            CaptureStatus::Empty
        }
    }

    /// See [`Capturer::frame`].
    pub fn frame(&mut self, advance_timestamp: bool) -> Option<&Frame> {
        if advance_timestamp {
            if let Some(f) = &mut self.last {
                f.timestamp_ms = self.start_time.elapsed().as_millis() as u64;
            }
        }
        self.last.as_ref()
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    /// Attempt to recreate the recorder after the capture thread died.
    /// Rate-limited by [`REBUILD_RETRY_INTERVAL`]; retried on every
    /// `poll` tick until it succeeds. Until then viewers keep seeing
    /// the held frame via the caller's repeat-encode path.
    fn try_rebuild(&mut self) {
        if let Some(t) = self.last_rebuild_attempt {
            if t.elapsed() < REBUILD_RETRY_INTERVAL {
                return;
            }
        }
        self.last_rebuild_attempt = Some(Instant::now());

        match open_recorder(self.monitor_index) {
            Ok((monitor, recorder, rx)) => {
                self.recorder = recorder;
                self.rx = rx;
                self.alive = true;
                self.last_rebuild_attempt = None;
                info!("streaming capture backend rebuilt");
                // The screen likely changed during the gap (that's what
                // killed the recorder) — re-seed so a now-static screen
                // still shows its new content.
                self.seed(&monitor);
            }
            Err(e) => warn!("streaming capture rebuild failed (will retry): {e}"),
        }
    }
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
impl Drop for StreamCapturer {
    fn drop(&mut self) {
        // Parks xcap's capture thread. xcap offers no way to *join* it,
        // so one parked OS thread outlives each streaming session —
        // known upstream limitation, harmless at "shares per app run"
        // scale. Without this stop() the thread would keep doing full
        // GPU readbacks at display rate with nobody receiving.
        if let Err(e) = self.recorder.stop() {
            warn!("failed to stop video recorder: {e}");
        }
    }
}

/// Enumerate monitors and start a recorder on `monitor_index`.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
fn open_recorder(
    monitor_index: usize,
) -> Result<(Monitor, xcap::VideoRecorder, Receiver<xcap::Frame>), CaptureError> {
    let monitors = Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;
    let monitor = monitors
        .into_iter()
        .nth(monitor_index)
        .ok_or(CaptureError::MonitorNotFound(monitor_index))?;

    let (recorder, rx) = monitor
        .video_recorder()
        .map_err(|e| CaptureError::Capture(format!("video_recorder: {e}")))?;
    recorder
        .start()
        .map_err(|e| CaptureError::Capture(format!("recorder start: {e}")))?;

    Ok((monitor, recorder, rx))
}

// ── Screenshot backend ──────────────────────────────────────────────────

/// Captures frames from a specific monitor at native resolution via
/// per-frame screenshots.
///
/// No scaling is performed here — the encoder handles aspect-preserving
/// downscaling via swscale during the RGBA→YUV420P conversion pass.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub struct ScreenCapturer {
    monitor: Monitor,
    quality: Quality,
    frame_count: u64,
    start_time: Instant,
    /// Most recent frame, held for the [`Capturer`] poll/frame API.
    /// Untouched by the plain [`ScreenCapturer::capture`] path.
    last: Option<Frame>,
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
impl ScreenCapturer {
    /// Create a capturer for the given monitor index and quality preset.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::MonitorList`] if enumeration fails, or
    /// [`CaptureError::MonitorNotFound`] if the index is out of range.
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        let monitors = Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;

        let monitor = monitors
            .into_iter()
            .nth(monitor_index)
            .ok_or(CaptureError::MonitorNotFound(monitor_index))?;

        Ok(Self::from_monitor(monitor, quality))
    }

    /// Create a capturer for the primary monitor (falls back to the first
    /// monitor if none is marked primary).
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::MonitorList`] if enumeration fails, or
    /// [`CaptureError::MonitorNotFound`] if no monitors exist.
    pub fn primary(quality: Quality) -> Result<Self, CaptureError> {
        let monitors = Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;

        let primary_idx = monitors
            .iter()
            .position(|m| m.is_primary().unwrap_or(false))
            .unwrap_or(0);

        let monitor = monitors
            .into_iter()
            .nth(primary_idx)
            .ok_or(CaptureError::MonitorNotFound(primary_idx))?;

        Ok(Self::from_monitor(monitor, quality))
    }

    fn from_monitor(monitor: Monitor, quality: Quality) -> Self {
        let name = monitor.name().unwrap_or_default();
        let w = monitor.width().unwrap_or(0);
        let h = monitor.height().unwrap_or(0);
        info!("Capturer: monitor='{name}' ({w}x{h}), target={quality}");

        Self {
            monitor,
            quality,
            frame_count: 0,
            start_time: Instant::now(),
            last: None,
        }
    }

    /// Capture one frame at the monitor's native resolution.
    ///
    /// Returns RGBA pixel data at the **source** resolution (not the
    /// target quality resolution). The encoder is responsible for
    /// aspect-preserving downscaling during the RGBA→YUV420P pass.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Capture`] if the OS capture fails.
    pub fn capture(&mut self) -> Result<Frame, CaptureError> {
        let image = self
            .monitor
            .capture_image()
            .map_err(|e| CaptureError::Capture(e.to_string()))?;

        let width = image.width();
        let height = image.height();
        let timestamp_ms = self.start_time.elapsed().as_millis() as u64;
        self.frame_count += 1;

        let data = image.into_raw();

        Ok(Frame {
            data,
            width,
            height,
            timestamp_ms,
        })
    }

    /// [`Capturer`] poll API: performs one (blocking) screenshot. A
    /// screenshot backend cannot tell whether the screen changed, so a
    /// successful capture always reports [`CaptureStatus::Fresh`].
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Capture`] if the OS capture fails.
    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        let frame = self.capture()?;
        self.last = Some(frame);
        Ok(CaptureStatus::Fresh)
    }

    /// See [`Capturer::frame`].
    pub fn frame(&mut self, advance_timestamp: bool) -> Option<&Frame> {
        if advance_timestamp {
            if let Some(f) = &mut self.last {
                f.timestamp_ms = self.start_time.elapsed().as_millis() as u64;
            }
        }
        self.last.as_ref()
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

// ── Mobile stub ─────────────────────────────────────────────────────────
// iOS / Android have no xcap backend. The same API surface exists so
// dependent crates compile unchanged; construction fails cleanly and the
// client's existing StartSharing error path surfaces it in the UI.
// Methods past construction are unreachable — no instance can exist.

/// Stub monitor list for platforms without a capture backend.
#[cfg(any(target_os = "ios", target_os = "android"))]
pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
    Ok(Vec::new())
}

/// Stub focus query for platforms without a capture backend.
#[cfg(any(target_os = "ios", target_os = "android"))]
pub fn focused_pid() -> Option<u32> {
    None
}

/// Stub capturer for platforms without a capture backend.
#[cfg(any(target_os = "ios", target_os = "android"))]
pub struct ScreenCapturer {
    _private: (),
}

#[cfg(any(target_os = "ios", target_os = "android"))]
impl ScreenCapturer {
    pub fn new(_monitor_index: usize, _quality: Quality) -> Result<Self, CaptureError> {
        Err(CaptureError::Capture(
            "screen capture is not supported on this platform".into(),
        ))
    }

    pub fn primary(quality: Quality) -> Result<Self, CaptureError> {
        Self::new(0, quality)
    }

    pub fn capture(&mut self) -> Result<Frame, CaptureError> {
        unreachable!("stub ScreenCapturer cannot be constructed")
    }

    pub fn quality(&self) -> Quality {
        unreachable!("stub ScreenCapturer cannot be constructed")
    }

    pub fn frame_count(&self) -> u64 {
        unreachable!("stub ScreenCapturer cannot be constructed")
    }
}

/// Stub unified capturer for platforms without a capture backend.
#[cfg(any(target_os = "ios", target_os = "android"))]
pub struct Capturer {
    _private: (),
}

#[cfg(any(target_os = "ios", target_os = "android"))]
impl Capturer {
    pub fn new(_monitor_index: usize, _quality: Quality) -> Result<Self, CaptureError> {
        Err(CaptureError::Capture(
            "screen capture is not supported on this platform".into(),
        ))
    }

    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        unreachable!("stub Capturer cannot be constructed")
    }

    pub fn frame(&mut self, _advance_timestamp: bool) -> Option<&Frame> {
        unreachable!("stub Capturer cannot be constructed")
    }

    pub fn backend_name(&self) -> &'static str {
        unreachable!("stub Capturer cannot be constructed")
    }
}
