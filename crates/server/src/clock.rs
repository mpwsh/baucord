//! Wall time for the tick (spec §10).
//!
//! The tick runs on `Instant` — fake in the simulator — but message
//! timestamps, invite expiry and restriction `until` need wall time. A
//! [`Clock`] anchors a wall-clock reading to an `Instant` and derives
//! every later reading as `base_wall + (now − base_instant)`, so the
//! simulator's fake `Instant` advances wall time for free and an
//! "expired invite" test simply runs the clock forward.
//!
//! `Instant` does not count time the machine spent paused (a VM live
//! migration, a suspended host), so a clock anchored once at start
//! fell behind for good after one: bans and invites ran late and
//! messages were stamped in the past. The running server's clock
//! ([`Clock::following_system`]) therefore moves its anchor forward
//! when the system clock has got ahead ([`Clock::catch_up`]); never
//! backwards, so stamps stay ordered. The simulator's clocks do not:
//! there, simulated time is the only time.

/// How far the system clock may be ahead before the anchor moves.
const CATCH_UP_AFTER_MS: u64 = 2_000;

use std::time::{Instant, SystemTime};

/// Milliseconds since the Unix epoch.
pub type WallMs = u64;

#[derive(Debug, Clone, Copy)]
pub struct Clock {
    base_wall_ms: WallMs,
    base_instant: Instant,
    /// Read from the system clock, and so follows it forward. A fixed
    /// (test) clock never moves on its own.
    follows_system: bool,
}

fn system_ms() -> WallMs {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Clock {
    /// Anchor the system clock to `base_instant` (the tick's clock at
    /// the same moment).
    #[must_use]
    pub fn system(base_instant: Instant) -> Self {
        Self {
            base_wall_ms: system_ms(),
            base_instant,
            follows_system: false,
        }
    }

    /// [`Self::system`], kept up with the system clock by
    /// [`Self::catch_up`]: the running server's, where `base_instant`
    /// is a real `Instant`.
    #[must_use]
    pub fn following_system(base_instant: Instant) -> Self {
        Self {
            follows_system: true,
            ..Self::system(base_instant)
        }
    }

    /// Anchor an arbitrary wall-clock reading to `base_instant`. Tests
    /// use this to make time deterministic.
    #[must_use]
    pub fn fixed(base_wall_ms: WallMs, base_instant: Instant) -> Self {
        Self {
            base_wall_ms,
            base_instant,
            follows_system: false,
        }
    }

    /// For a clock following the system's: when it is more than two
    /// seconds ahead of this one at `now`, move the anchor up to it
    /// and say by how many milliseconds. Never backwards.
    pub fn catch_up(&mut self, now: Instant) -> Option<u64> {
        if !self.follows_system {
            return None;
        }
        self.catch_up_to(system_ms(), now)
    }

    fn catch_up_to(&mut self, real_ms: WallMs, now: Instant) -> Option<u64> {
        let ours = self.wall_ms(now);
        let ahead = real_ms
            .checked_sub(ours)
            .filter(|d| *d > CATCH_UP_AFTER_MS)?;
        self.base_wall_ms = real_ms;
        self.base_instant = now;
        Some(ahead)
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
    fn a_clock_left_behind_catches_up_and_never_goes_back() {
        let base = Instant::now();
        let mut clock = Clock::following_system(base);
        clock.base_wall_ms = 1_000_000;
        let later = base + Duration::from_secs(1);
        // The machine was paused for a minute: real time is ahead.
        assert_eq!(clock.catch_up_to(1_061_000, later), Some(60_000));
        assert_eq!(clock.wall_ms(later), 1_061_000);
        // A small drift, or the system clock stepping back, moves
        // nothing.
        assert_eq!(clock.catch_up_to(1_062_500, later), None);
        assert_eq!(clock.catch_up_to(900_000, later), None);
        assert_eq!(clock.wall_ms(later), 1_061_000);
        // A fixed clock is the test's, and stays put; so does a plain
        // system reading (the simulator's).
        assert_eq!(Clock::fixed(5, base).catch_up(base), None);
        assert_eq!(Clock::system(base).catch_up(base), None);
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
