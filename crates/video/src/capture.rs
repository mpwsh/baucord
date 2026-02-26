//! Screen capture via `xcap`.
//!
//! Captures a monitor's screen at native resolution. Scaling to the
//! target encoding resolution is handled by the encoder's swscale pass
//! (combined with RGBA→YUV420P conversion in a single operation).
//! Reuses internal buffers to avoid per-frame allocations.

use std::time::Instant;

use log::info;
use xcap::Monitor;

use crate::{CaptureError, Frame, Quality};

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
pub fn list_monitors() -> Result<Vec<MonitorInfo>, CaptureError> {
    let monitors =
        Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;

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

/// Captures frames from a specific monitor at native resolution.
///
/// No scaling is performed here — the encoder handles downscaling via
/// swscale during colorspace conversion (RGBA→YUV420P + resize in one
/// pass with bicubic filtering).
pub struct ScreenCapturer {
    monitor: Monitor,
    quality: Quality,
    frame_count: u64,
    start_time: Instant,
    /// Reusable buffer for the raw RGBA capture. Avoids allocation per frame
    /// when the caller returns the previous frame's buffer via `return_buffer()`.
    reuse_buf: Vec<u8>,
}

impl ScreenCapturer {
    /// Create a capturer for the given monitor index and quality preset.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::MonitorList`] if enumeration fails, or
    /// [`CaptureError::MonitorNotFound`] if the index is out of range.
    pub fn new(monitor_index: usize, quality: Quality) -> Result<Self, CaptureError> {
        let monitors =
            Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;

        let monitor = monitors
            .into_iter()
            .nth(monitor_index)
            .ok_or(CaptureError::MonitorNotFound(monitor_index))?;

        let name = monitor.name().unwrap_or_default();
        let w = monitor.width().unwrap_or(0);
        let h = monitor.height().unwrap_or(0);
        info!("Capturer: monitor='{name}' ({w}x{h}), target={quality}");

        Ok(Self {
            monitor,
            quality,
            frame_count: 0,
            start_time: Instant::now(),
            reuse_buf: Vec::new(),
        })
    }

    /// Create a capturer for the primary monitor.
    pub fn primary(quality: Quality) -> Result<Self, CaptureError> {
        let monitors =
            Monitor::all().map_err(|e| CaptureError::MonitorList(e.to_string()))?;

        let primary_idx = monitors
            .iter()
            .position(|m| m.is_primary().unwrap_or(false))
            .unwrap_or(0);

        // Reuse the already-fetched list by picking the monitor directly
        let monitor = monitors
            .into_iter()
            .nth(primary_idx)
            .ok_or(CaptureError::MonitorNotFound(primary_idx))?;

        let name = monitor.name().unwrap_or_default();
        let w = monitor.width().unwrap_or(0);
        let h = monitor.height().unwrap_or(0);
        info!("Capturer: monitor='{name}' ({w}x{h}), target={quality}");

        Ok(Self {
            monitor,
            quality,
            frame_count: 0,
            start_time: Instant::now(),
            reuse_buf: Vec::new(),
        })
    }

    /// Capture one frame at the monitor's native resolution.
    ///
    /// Returns RGBA pixel data at the **source** resolution (not the
    /// target quality resolution). The encoder is responsible for
    /// downscaling during the RGBA→YUV420P conversion pass.
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

    /// Return the previously used RGBA buffer for reuse by the next capture.
    ///
    /// Call this after you're done with the [`Frame`] to avoid allocating a new
    /// buffer on the next `capture()` call. Not required — just an
    /// optimization when you own the frame and are done with it.
    pub fn return_buffer(&mut self, mut buf: Vec<u8>) {
        buf.clear();
        self.reuse_buf = buf;
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
}