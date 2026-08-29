//! Sender-side rate adaptation: bitrate, bandwidth, frame length, RED.
//!
//! Perkins ch. 10 sets the constraints: react to *loss events* rather than
//! raw loss fractions, average over several seconds so bursts don't cause
//! oscillation, and — the one users feel — prefer stable quality to a
//! higher average. So this is a ladder of a few fixed profiles (like
//! AMR's modes), stepped slowly with hysteresis, not a continuous rate.
//!
//! Two signals are distinguished because they want opposite responses:
//!
//! - **Random loss** (Wi-Fi interference, a lossy hop) with a flat RTT:
//!   there is bandwidth; add redundancy so listeners can repair it. Cost
//!   is paid by dropping one profile so the total stays near budget.
//! - **Congestion** — loss *with* RTT inflation (queues filling): every
//!   extra byte makes it worse. Drop profiles, never enable RED, and let
//!   Opus' in-band LBRR (cheap) carry the repair.
//!
//! Recovery is deliberately slow (`UP_AFTER` clean evaluations, doubling
//! after a failed step) so a marginal link settles on a working profile
//! instead of sawing between two.
//!
//! Evaluated once per `encoder::FEC_UPDATE_INTERVAL_FRAMES` (~2 s), from
//! the same smoothed loss window the FEC hint uses.

/// Codec bandwidth, mirrored from `opus::Bandwidth` so this module stays
/// codec-agnostic and testable without libopus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Narrow,
    Wide,
    SuperWide,
}

/// One rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub bitrate: i32,
    pub band: Band,
    /// Opus frame length per packet. 40 ms halves the per-packet header
    /// overhead (44 B × 50/s = 17.6 kbps at 20 ms), which matters exactly
    /// when the codec rate is low.
    pub frame_ms: u32,
}

/// Best first. Total wire rate at each rung (payload + 44 B/packet
/// overhead, RED off): ≈ 66, 50, 33, 25, 21 kbps.
pub const LADDER: [Profile; 5] = [
    Profile { bitrate: 48_000, band: Band::SuperWide, frame_ms: 20 },
    Profile { bitrate: 32_000, band: Band::Wide, frame_ms: 20 },
    Profile { bitrate: 24_000, band: Band::Wide, frame_ms: 40 },
    Profile { bitrate: 16_000, band: Band::Wide, frame_ms: 40 },
    Profile { bitrate: 12_000, band: Band::Narrow, frame_ms: 40 },
];

/// Smoothed loss at/above which RED turns on (random loss).
const RED_ON_LOSS: f32 = 8.0;
/// Smoothed loss below which RED turns off, after `RED_OFF_AFTER` clean
/// evaluations.
const RED_OFF_LOSS: f32 = 3.0;
const RED_OFF_AFTER: u32 = 5;
/// RED never runs on the top rung: 2 × 48 kbps is not a budget anyone
/// with loss has.
const RED_MIN_LEVEL: usize = 1;
/// Loss that steps down a rung even without RTT evidence of congestion.
const HEAVY_LOSS: f32 = 25.0;
/// Congestion: loss at/above this *and* RTT inflated.
const CONGESTED_LOSS: f32 = 2.0;
/// RTT is "inflated" above `rtt_min × RTT_INFLATION + RTT_MARGIN_MS`.
const RTT_INFLATION: f32 = 1.5;
const RTT_MARGIN_MS: f32 = 30.0;
/// Clean evaluations before stepping up one rung (× 2 s ≈ 16 s).
const UP_AFTER: u32 = 8;
const UP_AFTER_MAX: u32 = 64;
/// Clean means smoothed loss below this and no congestion.
const CLEAN_LOSS: f32 = 2.0;

/// One evaluation's inputs.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkSample {
    /// Loss the transport reports for our packets (what listeners see).
    pub uplink_loss_pct: f32,
    /// Round-trip time, if the transport knows it.
    pub rtt_ms: Option<f32>,
}

/// What the encoder should be doing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub level: usize,
    pub profile: Profile,
    pub red: bool,
    /// Something changed since the previous decision.
    pub changed: bool,
}

pub struct RateController {
    level: usize,
    red: bool,
    loss: f32,
    rtt_min: Option<f32>,
    rtt: Option<f32>,
    clean_streak: u32,
    red_clean_streak: u32,
    up_after: u32,
    /// Level we stepped up from most recently, to detect a failed step.
    last_up_from: Option<usize>,
    evaluations_since_up: u32,
}

impl Default for RateController {
    fn default() -> Self {
        Self::new()
    }
}

impl RateController {
    pub fn new() -> Self {
        Self {
            level: 0,
            red: false,
            loss: 0.0,
            rtt_min: None,
            rtt: None,
            clean_streak: 0,
            red_clean_streak: 0,
            up_after: UP_AFTER,
            last_up_from: None,
            evaluations_since_up: u32::MAX,
        }
    }

    #[cfg(test)]
    pub fn current(&self) -> Decision {
        Decision {
            level: self.level,
            profile: LADDER[self.level],
            red: self.red,
            changed: false,
        }
    }

    fn congested(&self) -> bool {
        match (self.rtt, self.rtt_min) {
            (Some(rtt), Some(min)) => {
                rtt > min * RTT_INFLATION + RTT_MARGIN_MS && self.loss >= CONGESTED_LOSS
            }
            _ => false,
        }
    }

    /// Feed one sample (every ~2 s) and get the profile to use.
    pub fn evaluate(&mut self, sample: LinkSample) -> Decision {
        let before = (self.level, self.red);

        // Smoothing: half-life of one evaluation, so a single lossy window
        // moves the estimate but does not own it.
        self.loss = 0.5 * self.loss + 0.5 * sample.uplink_loss_pct.max(0.0);
        if let Some(rtt) = sample.rtt_ms {
            self.rtt = Some(match self.rtt {
                Some(prev) => 0.5 * prev + 0.5 * rtt,
                None => rtt,
            });
            // Baseline follows the minimum, creeping up slowly so a path
            // change to a genuinely longer route is eventually accepted.
            self.rtt_min = Some(match self.rtt_min {
                Some(m) => (m * 1.02).min(rtt),
                None => rtt,
            });
        }
        let congested = self.congested();
        self.evaluations_since_up = self.evaluations_since_up.saturating_add(1);

        // ── Down ────────────────────────────────────────────────────
        if congested && self.level + 1 < LADDER.len() {
            self.step_down(1);
        } else if self.loss >= HEAVY_LOSS && self.level + 1 < LADDER.len() {
            self.step_down(1);
        }

        // ── RED ─────────────────────────────────────────────────────
        if congested {
            self.red = false;
            self.red_clean_streak = 0;
        } else if self.loss >= RED_ON_LOSS {
            if !self.red {
                self.red = true;
                if self.level < RED_MIN_LEVEL {
                    self.level = RED_MIN_LEVEL;
                    self.clean_streak = 0;
                }
            }
            self.red_clean_streak = 0;
        } else if self.red {
            if self.loss < RED_OFF_LOSS {
                self.red_clean_streak += 1;
                if self.red_clean_streak >= RED_OFF_AFTER {
                    self.red = false;
                    self.red_clean_streak = 0;
                }
            } else {
                self.red_clean_streak = 0;
            }
        }

        // ── Up ──────────────────────────────────────────────────────
        if !congested && self.loss < CLEAN_LOSS {
            self.clean_streak += 1;
            let floor = if self.red { RED_MIN_LEVEL } else { 0 };
            if self.clean_streak >= self.up_after && self.level > floor {
                self.last_up_from = Some(self.level);
                self.level -= 1;
                self.clean_streak = 0;
                self.evaluations_since_up = 0;
            }
        } else {
            self.clean_streak = 0;
        }

        let changed = (self.level, self.red) != before;
        Decision {
            level: self.level,
            profile: LADDER[self.level],
            red: self.red,
            changed,
        }
    }

    fn step_down(&mut self, n: usize) {
        self.level = (self.level + n).min(LADDER.len() - 1);
        self.clean_streak = 0;
        // A step down soon after a step up means that rung doesn't hold:
        // wait twice as long before trying it again.
        if self.last_up_from.is_some() && self.evaluations_since_up <= UP_AFTER {
            self.up_after = (self.up_after * 2).min(UP_AFTER_MAX);
        } else {
            self.up_after = UP_AFTER;
        }
        self.last_up_from = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(loss: f32, rtt: f32) -> LinkSample {
        LinkSample {
            uplink_loss_pct: loss,
            rtt_ms: Some(rtt),
        }
    }

    #[test]
    fn clean_link_stays_on_top_rung_without_red() {
        let mut c = RateController::new();
        let mut last = c.current();
        for _ in 0..50 {
            last = c.evaluate(sample(0.5, 40.0));
        }
        assert_eq!((last.level, last.red), (0, false));
    }

    #[test]
    fn random_loss_enables_red_and_drops_one_rung() {
        let mut c = RateController::new();
        for _ in 0..5 {
            c.evaluate(sample(15.0, 40.0)); // flat RTT: not congestion
        }
        let d = c.current();
        assert!(d.red, "RED should be on under random loss");
        assert_eq!(d.level, RED_MIN_LEVEL, "RED costs one rung, never runs at 48 kbps");
    }

    #[test]
    fn congestion_steps_down_and_never_enables_red() {
        let mut c = RateController::new();
        c.evaluate(sample(0.0, 40.0)); // establish baseline RTT
        for _ in 0..4 {
            c.evaluate(sample(12.0, 200.0)); // loss + RTT ×5: congestion
        }
        let d = c.current();
        assert!(!d.red, "RED must not add bytes on a congested link");
        assert!(d.level >= 2, "expected several rungs down, got {}", d.level);
    }

    #[test]
    fn recovery_is_slow_and_steps_one_rung_at_a_time() {
        let mut c = RateController::new();
        c.evaluate(sample(0.0, 40.0));
        for _ in 0..4 {
            c.evaluate(sample(30.0, 40.0));
        }
        let low = c.current().level;
        assert!(low >= 2);
        let mut ups = 0;
        for i in 0..UP_AFTER * 3 {
            let d = c.evaluate(sample(0.0, 40.0));
            if d.changed && d.level < low - ups {
                ups += 1;
                assert!(i + 1 >= UP_AFTER, "stepped up too soon at evaluation {i}");
            }
        }
        assert!(ups >= 1, "should have recovered at least one rung");
    }

    #[test]
    fn failed_step_up_doubles_the_wait() {
        let mut c = RateController::new();
        c.evaluate(sample(0.0, 40.0));
        for _ in 0..3 {
            c.evaluate(sample(30.0, 40.0));
        }
        // Let the smoothed loss clear, recover one rung, then get punished
        // immediately with a heavy-loss window.
        let mut stepped_up = false;
        for _ in 0..UP_AFTER + 6 {
            if c.evaluate(sample(0.0, 40.0)).changed {
                stepped_up = true;
            }
        }
        assert!(stepped_up, "precondition: a step up happened");
        c.evaluate(sample(60.0, 40.0));
        assert_eq!(c.up_after, UP_AFTER * 2);
    }
}
