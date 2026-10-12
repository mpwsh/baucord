//! How a level meter moves: up at once, down slowly.
//!
//! The voice engine publishes the level of its last 20 ms frame. Drawn
//! as read, a bar jumps between unrelated samples: speech falls to
//! nothing between syllables, several times a second. [`Meter`] holds
//! what the bar shows between reads — a rise is taken at once, a fall
//! takes [`RELEASE`] — and says when it has moved far enough to be
//! worth a redraw, so a quiet microphone draws nothing.
//!
//! The UI glides the bar to each value it is given (`animate width` in
//! `widgets.slint`), so the rise is not a jump on screen either.

use std::time::Duration;

/// How long a fall takes to cover 63% of the way down. At 150 ms the
/// bar followed every syllable to the floor and back and read as
/// jumping (first phone look, 2026-10-12); at twice that it rides over
/// the gaps inside a word and comes down between words, which is what
/// a gate threshold is set against.
pub const RELEASE: Duration = Duration::from_millis(300);

/// The least movement worth a redraw: about a pixel of a phone's bar.
pub const STEP: f32 = 0.004;

/// One meter's bar.
#[derive(Debug, Default, Clone, Copy)]
pub struct Meter {
    /// Where the bar is now.
    shown: f32,
    /// What the UI was last given.
    drawn: f32,
}

impl Meter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            shown: 0.0,
            drawn: 0.0,
        }
    }

    /// A reading of `level` (0–1), `dt` after the one before: the value
    /// to draw, or `None` when the bar has not moved a [`STEP`] since
    /// the last one drawn.
    pub fn read(&mut self, level: f32, dt: Duration) -> Option<f32> {
        let level = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let fallen = self.shown * (-dt.as_secs_f32() / RELEASE.as_secs_f32()).exp();
        self.shown = level.max(fallen);
        // A decay never arrives: the last sliver goes in one step, so
        // the bar ends empty and the redraws end with it.
        if self.shown < STEP {
            self.shown = 0.0;
        }
        let moved = (self.shown - self.drawn).abs() >= STEP;
        let emptied = self.shown == 0.0 && self.drawn != 0.0;
        if !(moved || emptied) {
            return None;
        }
        self.drawn = self.shown;
        Some(self.shown)
    }

    /// Back to empty, as for a meter whose source went away. Returns
    /// whether the UI holds anything to clear.
    pub fn clear(&mut self) -> bool {
        let held = self.drawn != 0.0;
        *self = Self::new();
        held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(50);

    #[test]
    fn a_rise_shows_at_once() {
        let mut m = Meter::new();
        assert_eq!(m.read(0.6, TICK), Some(0.6));
        assert_eq!(m.read(0.9, TICK), Some(0.9));
    }

    #[test]
    fn a_fall_is_eased_and_never_below_the_level() {
        let mut m = Meter::new();
        m.read(0.8, TICK);
        // One release time later the bar is 63% of the way down.
        let after = m.read(0.0, RELEASE).expect("it moved");
        assert!((after - 0.8 * (-1.0f32).exp()).abs() < 1e-4, "{after}");
        // A level above where the fall has got to wins.
        assert_eq!(m.read(0.5, TICK), Some(0.5));
        // And the fall stops at the level it meets.
        let mut m = Meter::new();
        m.read(0.8, TICK);
        assert_eq!(m.read(0.7, Duration::from_secs(5)), Some(0.7));
    }

    #[test]
    fn silence_ends_empty_and_then_draws_nothing() {
        let mut m = Meter::new();
        m.read(1.0, TICK);
        let mut last = 1.0;
        let mut draws = 0;
        for _ in 0..90 {
            if let Some(v) = m.read(0.0, TICK) {
                assert!(v < last, "{v} after {last}");
                last = v;
                draws += 1;
            }
        }
        // The bar came down in steps, not at once, and is empty.
        assert!(last == 0.0, "ended at {last}");
        assert!(draws > 20, "{draws} steps is not a smooth fall");
        assert!(draws < 60, "{draws} draws for one fall");
        assert_eq!(m.read(0.0, TICK), None);
    }

    #[test]
    fn a_steady_level_and_noise_under_a_step_draw_once() {
        let mut m = Meter::new();
        assert_eq!(m.read(0.3, TICK), Some(0.3));
        for i in 0..30 {
            let wobble = if i % 2 == 0 { 0.001 } else { -0.001 };
            assert_eq!(m.read(0.3 + wobble, TICK), None);
        }
    }

    #[test]
    fn odd_readings_are_read_as_what_a_bar_can_show() {
        let mut m = Meter::new();
        assert_eq!(m.read(7.0, TICK), Some(1.0));
        assert_eq!(m.read(f32::NAN, Duration::from_secs(10)), Some(0.0));
        assert_eq!(m.read(-1.0, TICK), None);
        // A read with no time passed since the last holds the bar.
        m.read(0.5, TICK);
        assert_eq!(m.read(0.0, Duration::ZERO), None);
    }

    #[test]
    fn clearing_says_whether_anything_was_drawn() {
        let mut m = Meter::new();
        assert!(!m.clear());
        m.read(0.4, TICK);
        assert!(m.clear());
        assert_eq!(m.read(0.4, TICK), Some(0.4));
    }
}
