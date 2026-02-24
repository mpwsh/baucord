//! Screen capture via `scap` (native streaming APIs).
//!
//! Uses ScreenCaptureKit (macOS), DXGI (Windows), or PipeWire (Linux) to
//! receive frames pushed by the OS at the configured framerate. Unlike the
//! previous `xcap`-based screenshot approach, no polling or timer is needed —
//! `get_next_frame()` blocks until the OS delivers the next frame.
//!
//! The capturer requests **BGRA** output at a hardware-downscaled resolution
//! matching the [`Quality`] preset, so the encoder receives frames at the
//! target resolution without a separate software prescale step.

use std::time::Instant;

use log::info;
use scap::capturer::{Capturer, Options, Resolution};
use scap::frame::{Frame as ScapFrame, FrameType, VideoFrame};
use scap::Target;

use crate::{CaptureError, Frame, Quality};

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Summary of a connected display.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub is_primary: bool,
}

/// Check whether the platform supports screen capture via `scap`.
pub fn is_supported() -> bool {
    scap::is_supported()
}

/// Check whether screen recording permission has been granted.
///
/// On macOS this queries the ScreenCaptureKit entitlement. On other
/// platforms this always returns `true`.
pub fn has_permission() -> bool {
    scap::has_permission()
}

/// Request screen recording permission from the OS.
///
/// On macOS this triggers the system permission dialog. The app may need
/// to be restarted after the user grants access. On other platforms this
/// is a no-op that returns `true`.
pub fn request_permission() -> bool {
    scap::request_permission()
}

/// Lists available display targets.
///
/// On Linux (PipeWire) this may return an empty list — monitors are
/// selected interactively during capture initialization via the portal
/// dialog.
///
/// # Errors
///
/// Returns [`CaptureError::NotSupported`] if the platform lacks support.
pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
    if !scap::is_supported() {
        return Err(CaptureError::NotSupported);
    }

    let targets = scap::get_all_targets();
    let monitors: Vec<MonitorInfo> = targets
        .into_iter()
        .enumerate()
        .filter_map(|(i, target)| match target {
            Target::Display(d) => Some(MonitorInfo {
                index: i,
                name: d.title.clone(),
                is_primary: i == 0,
            }),
            _ => None,
        })
        .collect();

    Ok(monitors)
}

// ---------------------------------------------------------------------------
// Quality → scap resolution mapping
// ---------------------------------------------------------------------------

/// Map a [`Quality`] preset to the closest `scap` output resolution.
///
/// `scap` performs hardware downscaling on the GPU, so the capturer
/// delivers frames already at the target resolution.
fn quality_to_resolution(quality: Quality) -> Resolution {
    match quality {
        Quality::HD30 | Quality::HD60 => Resolution::_720p,
        Quality::FHD30 | Quality::FHD60 => Resolution::_1080p,
    }
}

// ---------------------------------------------------------------------------
// ScreenCapturer
// ---------------------------------------------------------------------------

/// Captures frames from a display using native OS streaming APIs.
///
/// The capturer is started lazily on the first call to [`capture()`]. Each
/// subsequent call blocks until the OS delivers the next frame at the
/// configured framerate — no sleep or timer logic is needed on the caller
/// side.
///
/// # Pixel format
///
/// Frames are delivered in **BGRA** byte order on all platforms. The
/// encoder must use `Pixel::BGRA` (not `Pixel::RGBA`) as its swscale
/// source format.
///
/// [`capture()`]: ScreenCapturer::capture
pub struct ScreenCapturer {
    capturer: Capturer,
    quality: Quality,
    started: bool,
    frame_count: u64,
    start_time: Instant,
}

impl ScreenCapturer {
    /// Create a capturer for the given monitor index and quality preset.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::NotSupported`] if the platform lacks support,
    /// [`CaptureError::MonitorNotFound`] if the index is out of range, or
    /// [`CaptureError::InitFailed`] if `scap` fails to build the capturer.
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        if !scap::is_supported() {
            return Err(CaptureError::NotSupported);
        }

        let targets = scap::get_all_targets();
        let displays: Vec<_> = targets
            .into_iter()
            .filter_map(|t| match t {
                Target::Display(d) => Some(d),
                _ => None,
            })
            .collect();

        let display = displays
            .into_iter()
            .nth(monitor_index)
            .ok_or(CaptureError::MonitorNotFound(monitor_index))?;

        info!(
            "Capturer: display='{}', target={quality}, fps={}",
            display.title,
            quality.fps(),
        );

        build_capturer(Target::Display(display), quality)
    }

    /// Create a capturer for the primary display.
    ///
    /// Falls back to the first available display if no primary is detected.
    pub fn primary(quality: Quality) -> Result<Self, CaptureError> {
        if !scap::is_supported() {
            return Err(CaptureError::NotSupported);
        }

        let targets = scap::get_all_targets();

        // Pick the first display target (OS typically reports primary first).
        let display = targets
            .into_iter()
            .find_map(|t| match t {
                Target::Display(d) => Some(d),
                _ => None,
            })
            .ok_or(CaptureError::MonitorNotFound(0))?;

        info!(
            "Capturer: display='{}' (primary), target={quality}, fps={}",
            display.title,
            quality.fps(),
        );

        build_capturer(Target::Display(display), quality)
    }

    /// Receive the next frame from the OS.
    ///
    /// On the first call this starts the capture stream. Subsequent calls
    /// block until the OS delivers a frame at the configured framerate.
    ///
    /// # Pixel format
    ///
    /// The returned [`Frame`] contains **BGRA** pixel data.
    pub fn capture(&mut self) -> Result<Frame, CaptureError> {
        if !self.started {
            self.capturer.start_capture();
            self.started = true;
            self.start_time = Instant::now();
        }

        // Loop until we get a BGRA video frame (skip audio and other formats).
        loop {
            let scap_frame = self
                .capturer
                .get_next_frame()
                .map_err(|e| CaptureError::Capture(format!("{e}")))?;

            match scap_frame {
                ScapFrame::Video(VideoFrame::BGRA(bgra)) => {
                    // scap sends empty frames (width=0) on idle/blank screen.
                    if bgra.width <= 0 || bgra.height <= 0 {
                        continue;
                    }

                    self.frame_count += 1;
                    let timestamp_ms = self.start_time.elapsed().as_millis() as u64;

                    return Ok(Frame {
                        data: bgra.data,
                        width: bgra.width as u32,
                        height: bgra.height as u32,
                        timestamp_ms,
                    });
                }
                // Skip audio frames, YUV, RGB, and other video formats we
                // didn't request. Defensive — we configured BGRAFrame
                // output, but the OS may deliver other types during
                // transitions.
                _ => continue,
            }
        }
    }

    /// No-op — `scap` manages its own buffers internally.
    ///
    /// Kept for API compatibility with callers that previously recycled
    /// buffers from the `xcap`-based capturer.
    pub fn return_buffer(&mut self, _buf: Vec<u8>) {
        // scap handles buffer management internally.
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

impl Drop for ScreenCapturer {
    fn drop(&mut self) {
        if self.started {
            self.capturer.stop_capture();
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build a `scap::Capturer` with the given display target and quality.
fn build_capturer(target: Target, quality: Quality) -> Result<ScreenCapturer, CaptureError> {
    let options = Options {
        fps: quality.fps(),
        target: Some(target),
        show_cursor: true,
        show_highlight: false,
        excluded_targets: None,
        output_type: FrameType::BGRAFrame,
        output_resolution: quality_to_resolution(quality),
        crop_area: None,
        ..Default::default()
    };

    let capturer =
        Capturer::build(options).map_err(|e| CaptureError::InitFailed(format!("{e}")))?;

    Ok(ScreenCapturer {
        capturer,
        quality,
        started: false,
        frame_count: 0,
        start_time: Instant::now(),
    })
}
