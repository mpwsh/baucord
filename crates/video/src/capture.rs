//! Screen capture backends.
//!
//! Two backends, one [`Capturer`] front:
//!
//! - [`StreamCapturer`] (Windows) — the **vendored DXGI desktop
//!   duplication** module ([`crate::dxgi`]): frames are produced by the
//!   OS on a dedicated, joinable worker thread, only when the screen
//!   actually changes, delivered as BGRA with zero extra CPU passes.
//!   Duplication loss (mode changes, fullscreen transitions, UAC) is
//!   rebuilt inside the module; permanent failure (HDR desktop, driver
//!   trouble) makes the [`Capturer`] fall back to screenshots at
//!   runtime.
//! - [`ScreenCapturer`] — per-frame screenshots via xcap's
//!   `capture_image()` (GDI `BitBlt` on Windows, `CGWindowList` on
//!   macOS, X11/Wayland otherwise). Every call synchronously copies a
//!   full frame — fine as a fallback, and the only backend on
//!   non-Windows targets for now.
//!
//! [`Capturer::new`] picks the backend and logs which one is active.
//!
//! # Streaming-backend semantics the caller must know
//!
//! DXGI only delivers frames when the screen *changes*.
//! [`Capturer::poll`] therefore distinguishes [`CaptureStatus::Fresh`]
//! (new content — encode it) from [`CaptureStatus::Unchanged`] (nothing
//! new — skip the encode, unless a keyframe is owed to a viewer or the
//! periodic refresh is due). The last frame is held internally so it
//! can be re-encoded on demand ([`Capturer::frame`] with
//! `advance_timestamp = true`).
//!
//! A GDI probe remains as belt-and-braces: after sustained silence, one
//! screenshot is compared against the held frame — if the screen moved
//! while the backend said nothing, the probe itself is delivered so the
//! stream never freezes, whatever new failure mode the OS invents.

#[cfg(not(any(target_os = "ios", target_os = "android")))]
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::time::Duration;

#[cfg(not(any(target_os = "ios", target_os = "android")))]
use log::info;
#[cfg(target_os = "windows")]
use log::warn;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use xcap::Monitor;

#[cfg(target_os = "windows")]
use crate::PixelFormat;
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
    /// No frame has been captured yet (startup, or a backend rebuild
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
    /// Vendored DXGI streaming capture (Windows).
    #[cfg(target_os = "windows")]
    Stream(StreamCapturer),
    /// Per-frame screenshot capture (fallback, always available).
    Screenshot(ScreenCapturer),
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
impl Capturer {
    /// Create a capturer for the given monitor index and quality preset.
    ///
    /// Windows: tries the vendored DXGI streaming backend first and
    /// falls back to screenshots if it cannot start. Other platforms:
    /// screenshots. The chosen backend is logged either way.
    ///
    /// # Errors
    ///
    /// Returns an error only if *no* backend can be created (monitor
    /// enumeration failed or the index is out of range).
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        #[cfg(target_os = "windows")]
        {
            match StreamCapturer::new(monitor_index, quality) {
                Ok(s) => {
                    info!(
                        "video capture backend: STREAMING — vendored DXGI desktop \
                         duplication (BGRA passthrough), change-driven frames"
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
    /// for the streaming backend it is a non-blocking slot check. If the
    /// streaming backend reports permanent failure (HDR desktop, driver
    /// trouble), this transparently switches to the screenshot backend
    /// and keeps going.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Capture`] if the screenshot backend fails
    /// to capture (or can't be created during a failure switch).
    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        #[cfg(target_os = "windows")]
        {
            let switch = if let Self::Stream(s) = &*self {
                s.failure_message()
                    .map(|msg| (msg, s.monitor_index, s.quality))
            } else {
                None
            };
            if let Some((msg, monitor_index, quality)) = switch {
                warn!(
                    "streaming capture failed permanently ({msg}) — switching to the \
                     screenshot backend"
                );
                let sc = ScreenCapturer::new(monitor_index, quality)?;
                info!(
                    "video capture backend: SCREENSHOTS — {SCREENSHOT_API} via xcap \
                     capture_image (runtime fallback)"
                );
                *self = Self::Screenshot(sc);
            }
        }

        match self {
            #[cfg(target_os = "windows")]
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
            #[cfg(target_os = "windows")]
            Self::Stream(s) => s.frame(advance_timestamp),
            Self::Screenshot(s) => s.frame(advance_timestamp),
        }
    }

    /// Short human-readable backend description for logs/diagnostics.
    pub fn backend_name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "windows")]
            Self::Stream(_) => "streaming (vendored DXGI desktop duplication)",
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

// ── Streaming backend (Windows, vendored DXGI) ──────────────────────────

/// Minimum spacing between duplicator recreations after a loss. Games
/// kill freshly built duplications several times during a fullscreen
/// transition storm; recreation is cheap, so retry briskly but not in
/// a busy loop.
#[cfg(target_os = "windows")]
const RECREATE_INTERVAL: Duration = Duration::from_millis(300);

/// Give up after this long without a single *real* streamed frame
/// despite recreations, and fall back to the screenshot backend. A
/// transition storm resolves within a few seconds; a sustained drought
/// means this output genuinely can't be duplicated right now.
#[cfg(target_os = "windows")]
const STREAM_DROUGHT_GIVE_UP: Duration = Duration::from_secs(12);

/// How long the streaming backend tolerates total silence before the
/// GDI belt-and-braces probe runs. The vendored module reports its own
/// health, so unlike the xcap era this probe should never find a
/// silently dead backend — it exists to guarantee the stream can't
/// freeze even if the OS invents a new way to wedge a duplication.
#[cfg(target_os = "windows")]
const SILENCE_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Streaming capture via the vendored DXGI module ([`crate::dxgi`]).
///
/// Owns last-frame retention, timestamping, the seed screenshot (DXGI
/// delivers nothing until the screen changes), and the GDI silence
/// probe. Frames are **BGRA**; seeds and probes are converted to match
/// so every frame this backend emits has one consistent format.
#[cfg(target_os = "windows")]
pub struct StreamCapturer {
    /// `None` between a loss and a successful recreation.
    dup: Option<crate::dxgi::DxgiDuplicator>,
    monitor_index: usize,
    quality: Quality,
    /// Most recent frame (BGRA), retained for repeat-encodes.
    last: Option<Frame>,
    /// `last` holds content the caller hasn't been handed as Fresh yet
    /// (the seed screenshot at startup, or a probe frame).
    fresh_pending: bool,
    start_time: Instant,
    /// When a frame (real, seed, or probe) last arrived.
    last_rx: Instant,
    /// Last GDI probe (rate limiting).
    last_probe: Option<Instant>,
    /// Set at the first duplicator loss without a delivered frame,
    /// cleared by any real streamed frame. Sustained past
    /// [`STREAM_DROUGHT_GIVE_UP`] => fall back to screenshots.
    stream_drought_since: Option<Instant>,
    /// Last recreation attempt (rate limiting).
    last_recreate: Option<Instant>,
    /// When the current duplicator was built, for first-frame latency
    /// logging (proof in the log that real streaming resumed).
    dup_built_at: Instant,
    /// Whether the current duplicator has delivered a frame yet.
    dup_delivered: bool,
}

#[cfg(target_os = "windows")]
impl StreamCapturer {
    /// Create and start a streaming capturer for the given monitor.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] if the monitor cannot be found or the
    /// duplication cannot be created (no fallback here — that is
    /// [`Capturer::new`]'s job).
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        let dup = crate::dxgi::DxgiDuplicator::new(monitor_index)?;
        info!("StreamCapturer: monitor {monitor_index}, target={quality}");

        let mut cap = Self {
            dup: Some(dup),
            monitor_index,
            quality,
            last: None,
            fresh_pending: false,
            start_time: Instant::now(),
            last_rx: Instant::now(),
            last_probe: None,
            stream_drought_since: None,
            last_recreate: None,
            dup_built_at: Instant::now(),
            dup_delivered: false,
        };
        // DXGI only delivers on *change* — on a static screen the first
        // real frame may be a long way off. Seed with one screenshot so
        // sharing shows a picture immediately.
        cap.seed();
        Ok(cap)
    }

    /// One-shot screenshot into `last` (converted to BGRA), marked as
    /// fresh. Best-effort: a failure just means the stream starts on
    /// the first real change.
    fn seed(&mut self) {
        match screenshot_bgra(self.monitor_index) {
            Ok((data, width, height)) => {
                self.last = Some(Frame {
                    data,
                    width,
                    height,
                    timestamp_ms: self.start_time.elapsed().as_millis() as u64,
                    format: PixelFormat::Bgra,
                });
                self.fresh_pending = true;
                self.last_rx = Instant::now();
            }
            Err(e) => warn!("seed screenshot failed (stream starts on first change): {e}"),
        }
    }

    /// Non-blocking pump: check the frame slot, run the silence probe
    /// when warranted. Degraded states (module rebuilding) report as
    /// `Unchanged` (freeze-frame) or `Empty`; permanent failure is
    /// surfaced via [`Self::take_failure`].
    pub fn poll(&mut self) -> CaptureStatus {
        if let Some(dup) = &self.dup {
            if let Some(cf) = dup.take_frame() {
                if !self.dup_delivered {
                    self.dup_delivered = true;
                    info!(
                        "dxgi streaming: first frame {} ms after duplicator build ({}x{})",
                        self.dup_built_at.elapsed().as_millis(),
                        cf.width,
                        cf.height
                    );
                }
                self.stream_drought_since = None;
                self.last = Some(Frame {
                    data: cf.data,
                    width: cf.width,
                    height: cf.height,
                    timestamp_ms: self.start_time.elapsed().as_millis() as u64,
                    format: PixelFormat::Bgra,
                });
                self.fresh_pending = false;
                self.last_rx = Instant::now();
                return CaptureStatus::Fresh;
            }
        }

        // A lost duplicator is recreated HERE, on the consumer thread —
        // duplications created from the capture worker die instantly
        // while a fullscreen app owns the output (field-observed),
        // while consumer-thread creation works. Rate-limited; a
        // transition storm just means a few recreations in a row.
        let lost = self
            .dup
            .as_ref()
            .is_none_or(|d| d.health() == crate::dxgi::Health::Lost);
        if lost {
            self.stream_drought_since.get_or_insert_with(Instant::now);
            self.try_recreate();
        }

        // Belt-and-braces: if the screen moved while the backend was
        // silent for a while, deliver a probe frame so the stream can't
        // freeze regardless of what went wrong underneath — including
        // during recreation gaps.
        if self.probe_finds_change() {
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

    /// Drop the dead duplicator (joining its worker, releasing the
    /// duplication) and build a fresh one on this thread.
    fn try_recreate(&mut self) {
        if let Some(t) = self.last_recreate {
            if t.elapsed() < RECREATE_INTERVAL {
                return;
            }
        }
        self.last_recreate = Some(Instant::now());
        // Old one must be fully gone before the new DuplicateOutput
        // (one duplication per output per process).
        self.dup = None;
        match crate::dxgi::DxgiDuplicator::new(self.monitor_index) {
            Ok(d) => {
                info!("dxgi streaming: duplicator recreated");
                self.dup = Some(d);
                self.dup_built_at = Instant::now();
                self.dup_delivered = false;
            }
            Err(e) => {
                warn!("dxgi streaming: recreate failed (will retry): {e}");
            }
        }
    }

    /// Run the GDI probe when silence has lasted long enough. On a
    /// detected change, installs the probe as the current frame and
    /// returns `true`.
    fn probe_finds_change(&mut self) -> bool {
        if self.last_rx.elapsed() < SILENCE_PROBE_INTERVAL {
            return false;
        }
        if let Some(t) = self.last_probe {
            if t.elapsed() < SILENCE_PROBE_INTERVAL {
                return false;
            }
        }
        self.last_probe = Some(Instant::now());

        let (data, width, height) = match screenshot_bgra(self.monitor_index) {
            Ok(p) => p,
            Err(e) => {
                warn!("capture liveness probe failed: {e}");
                return false;
            }
        };

        let unchanged = self
            .last
            .as_ref()
            .is_some_and(|f| f.width == width && f.height == height && f.data == data);
        if unchanged {
            return false;
        }

        info!(
            "screen changed while the streaming backend was silent for {:.1}s — \
             delivering a probe frame",
            self.last_rx.elapsed().as_secs_f32()
        );
        self.last = Some(Frame {
            data,
            width,
            height,
            timestamp_ms: self.start_time.elapsed().as_millis() as u64,
            format: PixelFormat::Bgra,
        });
        self.fresh_pending = false;
        self.last_rx = Instant::now();
        true
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

    /// Reason to abandon streaming (side-effect-free peek): the module
    /// failed permanently (unsupported/HDR format), or recreations have
    /// produced no real frame for [`STREAM_DROUGHT_GIVE_UP`]. The
    /// [`Capturer`] switches to the screenshot backend on seeing it —
    /// and because this variant is destroyed by a successful switch,
    /// while a failed switch leaves it in place, the switch naturally
    /// retries every poll until it lands.
    fn failure_message(&self) -> Option<String> {
        if let Some(d) = &self.dup {
            if d.health() == crate::dxgi::Health::Failed {
                return Some(
                    d.failure()
                        .unwrap_or_else(|| "unknown capture failure".into()),
                );
            }
        }
        if let Some(since) = self.stream_drought_since {
            if since.elapsed() >= STREAM_DROUGHT_GIVE_UP {
                return Some(format!(
                    "no streamed frames for {:.0}s despite duplicator recreations",
                    since.elapsed().as_secs_f32()
                ));
            }
        }
        None
    }
}

/// One screenshot of the given monitor, converted to tightly packed
/// BGRA (the streaming backend's uniform format).
#[cfg(target_os = "windows")]
fn screenshot_bgra(monitor_index: usize) -> Result<(Vec<u8>, u32, u32), CaptureError> {
    let monitors = Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;
    let monitor = monitors
        .into_iter()
        .nth(monitor_index)
        .ok_or(CaptureError::MonitorNotFound(monitor_index))?;
    let image = monitor
        .capture_image()
        .map_err(|e| CaptureError::Capture(e.to_string()))?;
    let (width, height) = (image.width(), image.height());
    let mut data = image.into_raw();
    // RGBA → BGRA in place (swap R and B).
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok((data, width, height))
}

// ── Screenshot backend ──────────────────────────────────────────────────

/// Captures frames from a specific monitor at native resolution via
/// per-frame screenshots (RGBA).
///
/// No scaling is performed here — the encoder handles aspect-preserving
/// downscaling via swscale during the pixel-format conversion pass.
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
            format: crate::PixelFormat::Rgba,
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
// iOS / Android have no capture backend. The same API surface exists so
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
