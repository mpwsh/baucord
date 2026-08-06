//! Screen capture via `xcap`.
//!
//! Captures a monitor's screen at native resolution. Scaling to the
//! target encoding resolution is handled by the encoder's swscale pass
//! (combined with RGBA→YUV420P conversion in a single operation).
//!
//! `xcap` is a per-frame screenshot API: every `capture()` allocates and
//! fills a fresh full-frame RGBA buffer. There is no buffer reuse to
//! offer here — that only becomes possible with the OS streaming APIs
//! (DXGI Desktop Duplication / Windows.Graphics.Capture, ScreenCaptureKit,
//! PipeWire), which are the planned replacement for this backend.

#[cfg(not(any(target_os = "ios", target_os = "android")))]
use std::time::Instant;

#[cfg(not(any(target_os = "ios", target_os = "android")))]
use log::info;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
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

/// Captures frames from a specific monitor at native resolution.
///
/// No scaling is performed here — the encoder handles aspect-preserving
/// downscaling via swscale during the RGBA→YUV420P conversion pass.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub struct ScreenCapturer {
    monitor: Monitor,
    quality: Quality,
    frame_count: u64,
    start_time: Instant,
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
        }
    }

    /// Capture one frame at the monitor's native resolution.
    ///
    /// Returns RGBA pixel data at the **source** resolution (not the
    /// target quality resolution). The encoder is responsible for
    /// aspect-preserving downscaling during the RGBA→YUV420P pass.
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
