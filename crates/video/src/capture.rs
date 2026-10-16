//! Screen capture backends.
//!
//! Three backends, one [`Capturer`] front:
//!
//! - [`StreamCapturer`] (Windows) — the **vendored DXGI desktop
//!   duplication** module ([`crate::dxgi`]): frames are produced by the
//!   OS on a dedicated, joinable worker thread, only when the screen
//!   actually changes, delivered as BGRA with zero extra CPU passes.
//!   Duplication loss (mode changes, fullscreen transitions, UAC) is
//!   rebuilt inside the module; permanent failure (HDR desktop, driver
//!   trouble, a rotated output) makes the [`Capturer`] fall back to
//!   screenshots at runtime, and a drought (a locked screen) tries
//!   streaming again every 10 s from there.
//! - `crate::x11::X11Capturer` (Linux under X11) — one connection,
//!   reading the monitor only where DAMAGE reports drawing.
//! - [`ScreenCapturer`] — per-frame screenshots via xcap's
//!   `capture_image()` (GDI `BitBlt` on Windows, `CGWindowList` on
//!   macOS, Wayland). Every call synchronously copies a full frame, so
//!   a screen still for 2 s is looked at five times a second until it
//!   changes.
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

#[cfg(any(not(any(target_os = "ios", target_os = "android")), test))]
use std::time::Duration;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use std::time::Instant;

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

/// Whether a share shows the sharer's mouse pointer (Windows, where the
/// capture has to draw it in: `dxgi.rs`). On unless the settings file
/// says otherwise (`share.pointer`): the way back if it draws wrong on
/// some machine.
static POINTER_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Set before a share starts; a running one follows at its next frame.
pub fn set_pointer_shown(shown: bool) {
    POINTER_SHOWN.store(shown, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(target_os = "windows")]
pub(crate) fn pointer_shown() -> bool {
    POINTER_SHOWN.load(std::sync::atomic::Ordering::Relaxed)
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
#[must_use]
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
    /// X11 reads where DAMAGE reported a change (Linux, not Wayland).
    #[cfg(target_os = "linux")]
    X11(Box<crate::x11::X11Capturer>),
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

        #[cfg(target_os = "linux")]
        if crate::x11::is_x11_session() {
            let m = &s.monitor;
            let at = (|| Ok::<_, xcap::XCapError>((m.x()?, m.y()?, m.width()?, m.height()?)))();
            match at
                .map_err(|e| CaptureError::MonitorList(e.to_string()))
                .and_then(|(x, y, w, h)| crate::x11::X11Capturer::new(x, y, w, h))
            {
                Ok(x11) => {
                    info!(
                        "video capture backend: X11 — GetImage where DAMAGE reports a \
                         change, one connection"
                    );
                    return Ok(Self::X11(Box::new(x11)));
                }
                Err(e) => log::warn!("X11 damage capture unavailable, using screenshots: {e}"),
            }
        }

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
    /// streaming backend gives up (HDR desktop, driver trouble, a long
    /// drought), this transparently switches to the screenshot backend
    /// and keeps going; after a drought it tries streaming again every
    /// 10 s.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Capture`] if the screenshot backend fails
    /// to capture (or can't be created during a failure switch).
    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        #[cfg(target_os = "windows")]
        {
            let switch = if let Self::Stream(s) = &*self {
                s.failure().map(|f| (f, s.monitor_index, s.quality))
            } else {
                None
            };
            if let Some((failure, monitor_index, quality)) = switch {
                warn!(
                    "streaming capture failed ({}) — switching to the screenshot backend",
                    failure.message
                );
                let mut sc = ScreenCapturer::new(monitor_index, quality)?;
                info!(
                    "video capture backend: SCREENSHOTS — {SCREENSHOT_API} via xcap \
                     capture_image (runtime fallback)"
                );
                // A drought is usually the secure desktop: a locked
                // screen, a UAC prompt. Streaming works again once it is
                // gone, so it is tried again from here; before, a lock
                // longer than 12 s left the rest of the share on GDI
                // (review 2026-10, P2-12).
                if failure.passing {
                    sc.stream_retry = Some(StreamRetry {
                        monitor_index,
                        quality,
                        at: Instant::now() + STREAM_RETRY_INTERVAL,
                    });
                }
                *self = Self::Screenshot(sc);
            }

            let retry = match &mut *self {
                Self::Screenshot(sc) => sc.stream_retry.take_if(|r| r.at <= Instant::now()),
                Self::Stream(_) => None,
            };
            if let Some(mut retry) = retry {
                match StreamCapturer::new(retry.monitor_index, retry.quality) {
                    Ok(s) => {
                        info!(
                            "video capture backend: STREAMING again — vendored DXGI \
                             desktop duplication"
                        );
                        *self = Self::Stream(s);
                    }
                    Err(e) => {
                        log::debug!("streaming capture still unavailable: {e}");
                        retry.at = Instant::now() + STREAM_RETRY_INTERVAL;
                        if let Self::Screenshot(sc) = self {
                            sc.stream_retry = Some(retry);
                        }
                    }
                }
            }
        }

        match self {
            #[cfg(target_os = "windows")]
            Self::Stream(s) => Ok(s.poll()),
            #[cfg(target_os = "linux")]
            Self::X11(s) => s.poll(),
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
            #[cfg(target_os = "linux")]
            Self::X11(s) => s.frame(advance_timestamp),
            Self::Screenshot(s) => s.frame(advance_timestamp),
        }
    }

    /// Short human-readable backend description for logs/diagnostics.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "windows")]
            Self::Stream(_) => "streaming (vendored DXGI desktop duplication)",
            #[cfg(target_os = "linux")]
            Self::X11(_) => "X11 (GetImage on DAMAGE)",
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

/// How often the screenshot backend tries streaming again after a
/// drought sent the share there. A failed try builds a D3D device and
/// is refused by `DuplicateOutput`: a few milliseconds.
#[cfg(target_os = "windows")]
const STREAM_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// Why the streaming backend gave up, as [`StreamCapturer::failure`]
/// reports it.
#[cfg(target_os = "windows")]
struct StreamFailure {
    message: String,
    /// A drought, which passes (a locked screen ends), rather than a
    /// format the module cannot read.
    passing: bool,
}

/// When and how the screenshot backend tries streaming again.
#[cfg(target_os = "windows")]
struct StreamRetry {
    monitor_index: usize,
    quality: Quality,
    at: Instant,
}

/// How long the streaming backend tolerates total silence before the
/// GDI belt-and-braces probe runs. The vendored module reports its own
/// health, so unlike the xcap era this probe should never find a
/// silently dead backend — it exists to guarantee the stream can't
/// freeze even if the OS invents a new way to wedge a duplication.
#[cfg(target_os = "windows")]
const SILENCE_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// …and how often it runs once a probe found the screen as the backend
/// left it: a still screen, which is the usual reason for silence. The
/// probe is a blit of the whole desktop and a compare; every 2 s for as
/// long as nothing moved was most of what a still share cost (review
/// P2-41). A probe that *did* find a change, or a duplication that is
/// lost, keeps the short interval: then the probe is the picture.
#[cfg(target_os = "windows")]
const STILL_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Streaming capture via the vendored DXGI module ([`crate::dxgi`]).
///
/// Owns last-frame retention, timestamping, the seed screenshot (DXGI
/// delivers nothing until the screen changes), and the GDI silence
/// probe. Frames are **BGRA**; seeds and probes are converted to match
/// so every frame this backend emits has one consistent format.
#[cfg(target_os = "windows")]
#[allow(clippy::struct_excessive_bools)] // independent facts about the held frame, not a state machine
pub struct StreamCapturer {
    /// `None` between a loss and a successful recreation.
    dup: Option<crate::dxgi::DxgiDuplicator>,
    monitor_index: usize,
    quality: Quality,
    /// Most recent frame (BGRA), retained for repeat-encodes.
    last: Option<Frame>,
    /// What the pointer drawn in it covers, if it is drawn: a
    /// screenshot never has one, so the probe does not compare that
    /// part, and a pointer move puts it back before drawing again.
    last_under: Option<crate::pointer::Under>,
    /// `last` was halved on the GPU (`dxgi::Halver`): a probe's
    /// screenshot is halved to match.
    last_halved: bool,
    /// `last` holds content the caller hasn't been handed as Fresh yet
    /// (the seed screenshot at startup, or a probe frame).
    fresh_pending: bool,
    start_time: Instant,
    /// When a frame (real, seed, or probe) last arrived.
    last_rx: Instant,
    /// Last GDI probe (rate limiting).
    last_probe: Option<Instant>,
    /// That probe found the screen unchanged: the next one waits
    /// [`STILL_PROBE_INTERVAL`].
    probe_found_still: bool,
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
        let dup = crate::dxgi::DxgiDuplicator::new(monitor_index, quality)?;
        info!("StreamCapturer: monitor {monitor_index}, target={quality}");

        let mut cap = Self {
            dup: Some(dup),
            monitor_index,
            quality,
            last: None,
            last_under: None,
            last_halved: false,
            fresh_pending: false,
            start_time: Instant::now(),
            last_rx: Instant::now(),
            last_probe: None,
            probe_found_still: false,
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
    /// surfaced via [`Self::failure`].
    pub fn poll(&mut self) -> CaptureStatus {
        if let Some(dup) = &self.dup {
            let every = Duration::from_secs(1) / self.quality.fps();
            let taken = dup.take_frame(every);
            if let Some(crate::dxgi::Captured::PointerMoved { to, .. }) = taken {
                self.stream_drought_since = None;
                self.last_rx = Instant::now();
                return if self.redraw_pointer(to.as_ref()) {
                    self.fresh_pending = false;
                    CaptureStatus::Fresh
                } else if self.last.is_some() {
                    CaptureStatus::Unchanged
                } else {
                    CaptureStatus::Empty
                };
            }
            if let Some(crate::dxgi::Captured::Desktop(cf)) = taken {
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
                self.hold(Frame {
                    data: cf.data,
                    width: cf.width,
                    height: cf.height,
                    timestamp_ms: self.start_time.elapsed().as_millis() as u64,
                    format: PixelFormat::Bgra,
                });
                self.last_under = cf.under;
                self.last_halved = cf.halved;
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
        if let Some(t) = self.last_recreate
            && t.elapsed() < RECREATE_INTERVAL
        {
            return;
        }
        self.last_recreate = Some(Instant::now());
        // Old one must be fully gone before the new DuplicateOutput
        // (one duplication per output per process).
        self.dup = None;
        match crate::dxgi::DxgiDuplicator::new(self.monitor_index, self.quality) {
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
        let healthy = self
            .dup
            .as_ref()
            .is_some_and(|d| d.health() != crate::dxgi::Health::Lost);
        let every = if self.probe_found_still && healthy {
            STILL_PROBE_INTERVAL
        } else {
            SILENCE_PROBE_INTERVAL
        };
        if let Some(t) = self.last_probe
            && t.elapsed() < every
        {
            return false;
        }
        self.last_probe = Some(Instant::now());

        let (mut data, mut width, mut height) = match screenshot_bgra(self.monitor_index) {
            Ok(p) => p,
            Err(e) => {
                warn!("capture liveness probe failed: {e}");
                return false;
            }
        };
        // Streamed frames are halved on the GPU: so is the probe, here,
        // and the two round differently by a step.
        let tolerance = if self.last_halved {
            data = crate::pointer::halve(&data, width, height);
            (width, height) = (width / 2, height / 2);
            2
        } else {
            0
        };

        // The held frame may have the pointer drawn in, which no
        // screenshot has: that is not the screen changing. Compared
        // whole, every probe after the mouse stopped found a "change"
        // and put a picture without the pointer in its place.
        let unchanged = self.last.as_ref().is_some_and(|f| {
            let pointer = self.last_under.as_ref().map(|u| u.rect);
            f.width == width
                && f.height == height
                && if tolerance == 0 {
                    crate::pointer::same_outside(&f.data, &data, width, height, pointer)
                } else {
                    crate::pointer::close_outside(&f.data, &data, width, height, pointer, tolerance)
                }
        });
        self.probe_found_still = unchanged;
        if unchanged {
            return false;
        }

        info!(
            "screen changed while the streaming backend was silent for {:.1}s — \
             delivering a probe frame",
            self.last_rx.elapsed().as_secs_f32()
        );
        self.hold(Frame {
            data,
            width,
            height,
            timestamp_ms: self.start_time.elapsed().as_millis() as u64,
            format: PixelFormat::Bgra,
        });
        // A screenshot: no pointer in it until the next streamed frame.
        self.last_under = None;
        self.fresh_pending = false;
        self.last_rx = Instant::now();
        true
    }

    /// The pointer moved over an unchanged desktop: put back what it
    /// covered in the held picture and draw it where it is now (`None`:
    /// not shown). `false` when there is no picture to draw on, or
    /// nothing changed in it.
    fn redraw_pointer(&mut self, to: Option<&crate::dxgi::PointerAt>) -> bool {
        let now_ms = self.start_time.elapsed().as_millis() as u64;
        let Some(frame) = &mut self.last else {
            return false;
        };
        let had = self.last_under.take();
        if let Some(under) = &had {
            under.restore(&mut frame.data, frame.width);
        }
        self.last_under = to.and_then(|p| {
            p.shape
                .draw_keeping(&mut frame.data, frame.width, frame.height, p.x, p.y)
        });
        let changed = had.is_some() || self.last_under.is_some();
        if changed {
            frame.timestamp_ms = now_ms;
        }
        changed
    }

    /// See [`Capturer::frame`].
    pub fn frame(&mut self, advance_timestamp: bool) -> Option<&Frame> {
        if advance_timestamp && let Some(f) = &mut self.last {
            f.timestamp_ms = self.start_time.elapsed().as_millis() as u64;
        }
        self.last.as_ref()
    }

    /// Hold `frame` as the latest, giving the one it replaces back to
    /// `crate::pool`, where the capture worker takes its next buffer
    /// from (review P1-11).
    fn hold(&mut self, frame: Frame) {
        if let Some(old) = self.last.replace(frame) {
            crate::pool::recycle(old.data);
        }
    }

    /// Reason to abandon streaming (side-effect-free peek): the module
    /// failed permanently (unsupported/HDR format), or recreations have
    /// produced no real frame for [`STREAM_DROUGHT_GIVE_UP`]. The
    /// [`Capturer`] switches to the screenshot backend on seeing it —
    /// and because this variant is destroyed by a successful switch,
    /// while a failed switch leaves it in place, the switch naturally
    /// retries every poll until it lands. Only a drought is tried again
    /// later: an unreadable format stays unreadable.
    fn failure(&self) -> Option<StreamFailure> {
        if let Some(d) = &self.dup
            && d.health() == crate::dxgi::Health::Failed
        {
            return Some(StreamFailure {
                message: d
                    .failure()
                    .unwrap_or_else(|| "unknown capture failure".into()),
                passing: false,
            });
        }
        if let Some(since) = self.stream_drought_since
            && since.elapsed() >= STREAM_DROUGHT_GIVE_UP
        {
            return Some(StreamFailure {
                message: format!(
                    "no streamed frames for {:.0}s despite duplicator recreations",
                    since.elapsed().as_secs_f32()
                ),
                passing: true,
            });
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
    start_time: Instant,
    /// Most recent frame, held for the [`Capturer`] poll/frame API.
    /// Untouched by the plain [`ScreenCapturer::capture`] path.
    last: Option<Frame>,
    /// Set when a passing failure sent a streaming share here.
    #[cfg(target_os = "windows")]
    stream_retry: Option<StreamRetry>,
    /// Since when every screenshot has matched the held one.
    still_since: Option<Instant>,
    /// When the last screenshot was taken.
    last_grab: Option<Instant>,
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

    fn from_monitor(monitor: Monitor, quality: Quality) -> Self {
        let name = monitor.name().unwrap_or_default();
        let w = monitor.width().unwrap_or(0);
        let h = monitor.height().unwrap_or(0);
        info!("Capturer: monitor='{name}' ({w}x{h}), target={quality}");

        Self {
            monitor,
            start_time: Instant::now(),
            last: None,
            #[cfg(target_os = "windows")]
            stream_retry: None,
            still_since: None,
            last_grab: None,
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
    /// screenshot API cannot say whether the screen changed, so the
    /// picture is compared with the one before: the same bytes are
    /// [`CaptureStatus::Unchanged`], and the share loop encodes nothing
    /// for them but its once-a-second refresh. Every screenshot used to
    /// be `Fresh`, so a still screen on macOS and Linux — where this is
    /// the only backend — was scaled and encoded 30 times a second
    /// (review P2-41). The compare is a `memcmp` that stops at the first
    /// difference: under a millisecond for an unchanged 1080p frame,
    /// against several for its scale and encode.
    ///
    /// The screenshot itself is not cheap — a full-screen grab and copy
    /// (and on X11 under xcap a conversion per pixel) — so once the
    /// screen has been still for [`STILL_AFTER`], one is taken every
    /// [`STILL_GRAB_EVERY`] instead of every tick; the first change
    /// brings the full rate back (review 2026-10, P2-43). On macOS,
    /// Wayland and the Windows GDI fallback that change is up to 200 ms
    /// late after a still spell. X11 sessions read only where DAMAGE
    /// says (`crate::x11`) and wait for nothing.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Capture`] if the OS capture fails.
    pub fn poll(&mut self) -> Result<CaptureStatus, CaptureError> {
        let now = Instant::now();
        let resting = self.last.is_some()
            && self
                .still_since
                .is_some_and(|since| now.duration_since(since) >= STILL_AFTER)
            && self
                .last_grab
                .is_some_and(|at| now.duration_since(at) < STILL_GRAB_EVERY);
        if resting {
            return Ok(CaptureStatus::Unchanged);
        }
        self.last_grab = Some(now);
        let frame = self.capture()?;
        let status = hold_if_changed(&mut self.last, frame);
        if status == CaptureStatus::Unchanged {
            self.still_since.get_or_insert(now);
        } else {
            self.still_since = None;
        }
        Ok(status)
    }

    /// See [`Capturer::frame`].
    pub fn frame(&mut self, advance_timestamp: bool) -> Option<&Frame> {
        if advance_timestamp && let Some(f) = &mut self.last {
            f.timestamp_ms = self.start_time.elapsed().as_millis() as u64;
        }
        self.last.as_ref()
    }
}

/// A screen unchanged this long is taken to be resting
/// ([`ScreenCapturer::poll`]).
#[cfg(not(any(target_os = "ios", target_os = "android")))]
const STILL_AFTER: Duration = Duration::from_secs(2);

/// How often a resting screen is looked at: five screenshots a second
/// instead of 30–60.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
const STILL_GRAB_EVERY: Duration = Duration::from_millis(200);

/// Does the screen make pictures well faster than we take them? Two
/// queued frames `apart`, against one taken every `every`: at three
/// quarters of our interval or less there is one to spare per tick, and
/// the older of the two can go (`dxgi::DxgiDuplicator::take_frame`,
/// review P3-28). A 60 fps video on a 144 Hz screen presents 14 and
/// 21 ms apart: against a 60 fps share (16.7 ms) that is the same rate
/// and nothing is dropped; against a 30 fps one it is twice as fast.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn source_outpaces(apart: Duration, every: Duration) -> bool {
    apart * 4 <= every * 3
}

/// Hold `frame` as the latest unless it is the picture already held
/// (see [`ScreenCapturer::poll`]).
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub(crate) fn hold_if_changed(last: &mut Option<Frame>, frame: Frame) -> CaptureStatus {
    let same = last.as_ref().is_some_and(|last| {
        last.width == frame.width && last.height == frame.height && last.data == frame.data
    });
    if same {
        return CaptureStatus::Unchanged;
    }
    *last = Some(frame);
    CaptureStatus::Fresh
}

#[cfg(all(test, not(any(target_os = "ios", target_os = "android"))))]
mod tests {
    use super::*;

    fn shot(width: u32, height: u32, fill: u8, timestamp_ms: u64) -> Frame {
        Frame {
            data: vec![fill; (width * height * 4) as usize],
            width,
            height,
            timestamp_ms,
            format: crate::PixelFormat::Rgba,
        }
    }

    /// Which of two queued desktop frames a share takes (review P3-28).
    #[test]
    fn the_older_frame_goes_only_when_the_screen_is_faster() {
        let ms = Duration::from_millis;
        let (fps30, fps60) = (Duration::from_secs(1) / 30, Duration::from_secs(1) / 60);
        // A 60 Hz desktop under a 30 fps share: one to spare each tick.
        assert!(source_outpaces(ms(17), fps30));
        // A 60 fps video on a 144 Hz screen, 14 and 21 ms apart.
        assert!(source_outpaces(ms(14), fps30) && source_outpaces(ms(21), fps30));
        assert!(!source_outpaces(ms(14), fps60) && !source_outpaces(ms(21), fps60));
        // The same rate on both sides: every frame is wanted.
        assert!(!source_outpaces(ms(17), fps60));
        assert!(!source_outpaces(ms(33), fps30));
        // A 144 fps game under a 60 fps share.
        assert!(source_outpaces(ms(7), fps60));
    }

    #[test]
    fn the_same_screenshot_twice_is_unchanged() {
        let mut last = None;
        assert_eq!(
            hold_if_changed(&mut last, shot(4, 2, 7, 0)),
            CaptureStatus::Fresh
        );
        // Held as it was: the share loop moves its timestamp itself.
        assert_eq!(
            hold_if_changed(&mut last, shot(4, 2, 7, 33)),
            CaptureStatus::Unchanged
        );
        assert_eq!(last.as_ref().map(|f| f.timestamp_ms), Some(0));

        let mut moved = shot(4, 2, 7, 66);
        moved.data[5] = 8;
        assert_eq!(hold_if_changed(&mut last, moved), CaptureStatus::Fresh);
        assert_eq!(last.as_ref().map(|f| f.timestamp_ms), Some(66));
        // The same bytes at another size are another picture.
        assert_eq!(
            hold_if_changed(&mut last, shot(2, 4, 7, 99)),
            CaptureStatus::Fresh
        );
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
