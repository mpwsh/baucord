//! Wall time for the tick (spec §10).
//!
//! The tick runs on `Instant` — fake in the simulator — but message
//! timestamps, invite expiry and restriction `until` need wall time. A
//! [`Clock`] anchors a wall-clock reading to an `Instant` and derives
//! every later reading as `base_wall + (now − base_instant)`, so the
//! simulator's fake `Instant` advances wall time for free and an
//! "expired invite" test simply runs the clock forward.

use std::time::{Instant, SystemTime};

/// Milliseconds since the Unix epoch.
pub type WallMs = u64;

#[derive(Debug, Clone, Copy)]
pub struct Clock {
    base_wall_ms: WallMs,
    base_instant: Instant,
}

impl Clock {
    /// Anchor the system clock to `base_instant` (the tick's clock at
    /// the same moment).
    #[must_use]
    pub fn system(base_instant: Instant) -> Self {
        let base_wall_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Self {
            base_wall_ms,
            base_instant,
        }
    }

    /// Anchor an arbitrary wall-clock reading to `base_instant`. Tests
    /// use this to make time deterministic.
    #[must_use]
    pub fn fixed(base_wall_ms: WallMs, base_instant: Instant) -> Self {
        Self {
            base_wall_ms,
            base_instant,
        }
    }

    /// Wall time at `now` (the tick's clock), in milliseconds since the
    /// Unix epoch. Saturating: never goes backwards past the anchor.
    #[must_use]
    pub fn wall_ms(&self, now: Instant) -> WallMs {
        let elapsed = now.saturating_duration_since(self.base_instant);
        self.base_wall_ms
            .saturating_add(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn wall_time_follows_the_instant() {
        let base = Instant::now();
        let clock = Clock::fixed(1_000_000, base);
        assert_eq!(clock.wall_ms(base), 1_000_000);
        assert_eq!(clock.wall_ms(base + Duration::from_millis(2500)), 1_002_500);
    }

    #[test]
    fn an_instant_before_the_anchor_reads_as_the_anchor() {
        let base = Instant::now();
        let clock = Clock::fixed(500, base + Duration::from_secs(10));
        assert_eq!(clock.wall_ms(base), 500);
    }

    #[test]
    fn system_clock_starts_near_now() {
        let base = Instant::now();
        let clock = Clock::system(base);
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        assert!(now_ms.abs_diff(clock.wall_ms(base)) < 5_000);
    }
}
