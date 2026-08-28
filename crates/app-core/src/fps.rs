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
}

impl FpsCounter {
    /// Start counting from now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames_this_second: 0,
            last_fps: 0,
            last_tick: Instant::now(),
        }
    }

    /// Record one frame. Returns the current FPS estimate, which
    /// updates once per second (0 until the first second completes).
    pub fn tick(&mut self) -> u32 {
        self.frames_this_second += 1;

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
}

impl Default for FpsCounter {
    fn default() -> Self {
        Self::new()
    }
}
