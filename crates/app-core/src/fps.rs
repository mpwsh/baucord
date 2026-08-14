//! Frames-per-second measurement for streaming views.
//!
//! Ported unchanged from the FLTK-era `ui::fps` — toolkit-free, so it
//! moves down into app-core where the mobile shells can share it.

use std::time::Instant;

/// Rolling per-second FPS counter.
///
/// Call [`tick`](Self::tick) once per displayed frame; the returned
/// estimate updates once per wall-clock second.
pub struct FpsCounter {
    frames_this_second: u32,
    last_fps: u32,
    last_tick: Instant,
    total_frames: u64,
    start: Instant,
}

impl FpsCounter {
    /// Start counting from now.
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            frames_this_second: 0,
            last_fps: 0,
            last_tick: now,
            total_frames: 0,
            start: now,
        }
    }

    /// Record one frame. Returns the current FPS estimate, which
    /// updates once per second (0 until the first second completes).
    pub fn tick(&mut self) -> u32 {
        self.frames_this_second += 1;
        self.total_frames += 1;

        if self.last_tick.elapsed().as_secs_f32() >= 1.0 {
            self.last_fps = self.frames_this_second;
            self.frames_this_second = 0;
            self.last_tick = Instant::now();
        }

        self.last_fps
    }

    /// Whether the frame recorded by the latest [`tick`](Self::tick)
    /// was the first of a new second — the moment for once-per-second
    /// UI refreshes (stats labels, console summaries).
    #[must_use]
    pub fn second_started(&self) -> bool {
        self.frames_this_second == 1
    }

    /// Average FPS since construction or the last
    /// [`reset`](Self::reset).
    #[must_use]
    // Frame counts sit far below f64's 2^52 integer range, so the
    // u64 → f64 cast is lossless in practice.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_fps(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.total_frames as f64 / elapsed
        } else {
            0.0
        }
    }

    /// Seconds elapsed since construction or the last
    /// [`reset`](Self::reset).
    #[must_use]
    pub fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Restart all counters from now.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for FpsCounter {
    fn default() -> Self {
        Self::new()
    }
}
