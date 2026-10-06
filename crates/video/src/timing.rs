//! Rate limiting for per-frame timing warnings.
//!
//! "Slow decode: 23.6 ms" once per frame is a useful alarm on a desktop
//! where it fires twice an hour, and 60 lines a second everywhere else
//! (the Android emulator's decoder + the dev profile's colour
//! conversion sit at 20–70 ms *every* frame, 2026-10-05). [`SlowFrames`]
//! keeps the alarm and folds it into one line a second: how many frames
//! were over budget out of how many, the worst and the average.

use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(1);

/// One per stage per stream. Feed it every frame; log what it returns.
#[derive(Debug)]
pub struct SlowFrames {
    label: &'static str,
    window_start: Instant,
    total: u32,
    slow: u32,
    slow_max_ms: f32,
    slow_sum_ms: f32,
}

impl SlowFrames {
    #[must_use]
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            window_start: Instant::now(),
            total: 0,
            slow: 0,
            slow_max_ms: 0.0,
            slow_sum_ms: 0.0,
        }
    }

    /// Record one frame that took `ms`, over budget when `slow`.
    /// Returns the summary line once per second, and only for seconds
    /// in which something was slow.
    pub fn record(&mut self, ms: f32, slow: bool) -> Option<String> {
        self.record_at(Instant::now(), ms, slow)
    }

    fn record_at(&mut self, now: Instant, ms: f32, slow: bool) -> Option<String> {
        self.total += 1;
        if slow {
            self.slow += 1;
            self.slow_max_ms = self.slow_max_ms.max(ms);
            self.slow_sum_ms += ms;
        }
        let elapsed = now.duration_since(self.window_start);
        if elapsed < WINDOW {
            return None;
        }
        let line = (self.slow > 0).then(|| {
            format!(
                "{}: {}/{} frames over budget in the last {:.1}s (worst {:.1} ms, avg {:.1} ms)",
                self.label,
                self.slow,
                self.total,
                elapsed.as_secs_f32(),
                self.slow_max_ms,
                self.slow_sum_ms / self.slow as f32,
            )
        });
        self.window_start = now;
        self.total = 0;
        self.slow = 0;
        self.slow_max_ms = 0.0;
        self.slow_sum_ms = 0.0;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_per_second_only_when_slow() {
        let t0 = Instant::now();
        let mut s = SlowFrames::new("decode");
        assert_eq!(s.record_at(t0, 30.0, true), None);
        assert_eq!(
            s.record_at(t0 + Duration::from_millis(500), 5.0, false),
            None
        );
        let line = s
            .record_at(t0 + Duration::from_millis(1100), 50.0, true)
            .expect("window closed");
        assert!(line.starts_with("decode: 2/3 frames over budget"), "{line}");
        assert!(line.contains("worst 50.0 ms"), "{line}");
        assert!(line.contains("avg 40.0 ms"), "{line}");
        // A quiet second says nothing, but still resets.
        assert_eq!(
            s.record_at(t0 + Duration::from_millis(1500), 5.0, false),
            None
        );
        assert_eq!(
            s.record_at(t0 + Duration::from_millis(2200), 5.0, false),
            None
        );
        assert_eq!(
            s.record_at(t0 + Duration::from_millis(2300), 90.0, true),
            None
        );
        let line = s
            .record_at(t0 + Duration::from_millis(3300), 1.0, false)
            .unwrap();
        assert!(line.starts_with("decode: 1/2 frames"), "{line}");
    }
}
