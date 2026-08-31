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
//! ## Dwell (why the ladder no longer free-falls)
//!
//! The transport's loss figure is a ~6 s sliding window (renet:
//! `connection_stats.rs`, 300 ms buckets, newest ~0.9 s excluded), and we
//! evaluate every ~2 s. Without a hold, one loss event is counted by
//! three or four consecutive evaluations and every episode rides the
//! ladder to the bottom — the 2026-08-29..31 chicho logs show exactly
//! this: 48 → 12 kbps in 6–8 s on every burst regardless of severity.
//! So after any rung change the controller now dwells [`DWELL_EVALS`]
//! evaluations (> the loss window) before stepping again; a step acts on
//! evidence that postdates the previous step. The same trap and the same
//! cure are documented on `RATE_DWELL` in the screen-share ladder
//! (client/src/screen.rs), which this mirrors. Severity still matters:
//! loss at/above [`SEVERE_LOSS`] steps two rungs at once, so a disaster
//! reaches the low rungs in two moves instead of four.
//!
//! ## Recovery
//!
//! Recovery is deliberately slow for long episodes (`UP_AFTER` clean
//! evaluations per rung, doubled after a failed probe) so a marginal
//! link settles on a working profile instead of sawing between two.
//! *Short* episodes — dirty for at most [`SHORT_EPISODE_EVALS`]
//! evaluations — are treated as blips: the first climb starts after
//! [`UP_AFTER_SHORT`] clean evaluations and returns directly to the rung
//! held before the episode, because a link that was clean at 48 kbps
//! nine seconds ago does not need four probes to prove itself again.
//!
//! ## The bottom rung
//!
//! 12 kbps Narrowband is unintelligible enough that users describe it as
//! "broken" and relog; it is also only ~4 kbps cheaper on the wire than
//! 16 kbps Wideband. The ordinary floor is therefore the 16 kbps
//! Wideband rung ([`NORMAL_FLOOR_LEVEL`]); Narrowband is reachable only
//! under *sustained congestion* ([`NARROW_AFTER_CONGESTED_EVALS`]
//! consecutive congested evaluations), where shedding every possible
//! byte is the point.
//!
//! ## Stale input
//!
//! The loss estimate arrives from another thread and can stop refreshing
//! (transport gated it, sampler stalled, we are muted and nothing is
//! being sent). A stale reading is passed here as `None`: the smoothed
//! loss then decays instead of latching, so the last bad number before
//! the signal went quiet cannot pin the ladder down forever. The
//! 2026-08-30 00:17 log shows the latch this replaces: a 38 % reading
//! held verbatim for two minutes of silence.
//!
//! Evaluated once per ~2 s of wall clock (see `encoder`), from the same
//! smoothed loss window the FEC hint uses.

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
    Profile {
        bitrate: 48_000,
        band: Band::SuperWide,
        frame_ms: 20,
    },
    Profile {
        bitrate: 32_000,
        band: Band::Wide,
        frame_ms: 20,
    },
    Profile {
        bitrate: 24_000,
        band: Band::Wide,
        frame_ms: 40,
    },
    Profile {
        bitrate: 16_000,
        band: Band::Wide,
        frame_ms: 40,
    },
    Profile {
        bitrate: 12_000,
        band: Band::Narrow,
        frame_ms: 40,
    },
];

/// Lowest rung ordinary loss can reach: 16 kbps Wideband. See the module
/// docs ("The bottom rung").
const NORMAL_FLOOR_LEVEL: usize = 3;
/// Consecutive congested evaluations (~10 s) before the Narrowband rung
/// below [`NORMAL_FLOOR_LEVEL`] becomes reachable.
const NARROW_AFTER_CONGESTED_EVALS: u32 = 5;

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
/// Loss at/above which a (dwelled) step drops two rungs at once. Every
/// logged incident crossed 30 % within one window of onset, and the rung
/// one down demonstrably didn't fit either.
const SEVERE_LOSS: f32 = 30.0;
/// Congestion: loss at/above this *and* RTT inflated.
const CONGESTED_LOSS: f32 = 2.0;
/// RTT is "inflated" above `rtt_min × RTT_INFLATION + RTT_MARGIN_MS`.
const RTT_INFLATION: f32 = 1.5;
const RTT_MARGIN_MS: f32 = 30.0;

/// Evaluations to hold after any rung change before stepping again
/// (~8 s at the 2 s cadence — longer than renet's 6 s loss window, so
/// the next step acts on post-change evidence). A step *down* that
/// immediately undoes a climb is exempt: the climb was the fresh event,
/// and a failed probe should be reverted at once.
const DWELL_EVALS: u32 = 4;

/// Clean evaluations before stepping up one rung after a *long* episode
/// (× 2 s ≈ 16 s).
const UP_AFTER: u32 = 8;
/// Clean evaluations before the climb after a *short* episode (≈ 8 s).
const UP_AFTER_SHORT: u32 = 4;
/// An episode dirty for at most this many evaluations (~10 s) is short:
/// recovery returns directly to the pre-episode rung.
const SHORT_EPISODE_EVALS: u32 = 5;
/// A step down within this many evaluations of a step up is a failed
/// probe: the climb hold is doubled (multiplier capped at
/// [`PROBE_BACKOFF_MAX`]), so a link that fits at one rung but not the
/// next is not re-probed every few seconds.
const PROBE_FAIL_EVALS: u32 = 10;
const PROBE_BACKOFF_MAX: u32 = 8;
/// Clean means smoothed loss below this and no congestion.
const CLEAN_LOSS: f32 = 2.0;
/// Per-evaluation decay applied to the smoothed loss when the sample
/// carries no fresh reading (stale/unknown): the estimate drains at the
/// same half-life a stream of zero readings would give it.
const STALE_DECAY: f32 = 0.5;

/// One evaluation's inputs.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkSample {
    /// Loss the transport reports for our packets (what listeners see).
    /// `None` when there is no fresh reading — the transport withheld
    /// it, or the last one is older than the caller's staleness bound.
    pub uplink_loss_pct: Option<f32>,
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

    /// Consecutive clean evaluations (climb hold).
    clean_streak: u32,
    red_clean_streak: u32,
    /// Consecutive congested evaluations (Narrowband gate).
    congested_streak: u32,
    /// Evaluations since the last rung change (dwell).
    evals_since_change: u32,
    /// Evaluations since the last step *up* (failed-probe window).
    evals_since_up: u32,
    /// The last rung change was a climb (probe that can be undone).
    last_change_was_up: bool,

    /// Climb-hold multiplier: doubled by each failed probe, reset by a
    /// step down that wasn't one.
    probe_backoff: u32,
    /// Base climb hold, chosen by the length of the last episode.
    up_after_base: u32,
    /// Dirty evaluations in the current episode (0 = currently clean).
    dirty_evals: u32,
    /// Rung held when the current episode began.
    pre_episode_level: Option<usize>,
    /// After a short episode: climb directly back to this rung.
    restore_to: Option<usize>,
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
            congested_streak: 0,
            evals_since_change: u32::MAX,
            evals_since_up: u32::MAX,
            last_change_was_up: false,
            probe_backoff: 1,
            up_after_base: UP_AFTER,
            dirty_evals: 0,
            pre_episode_level: None,
            restore_to: None,
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

    /// Smoothed loss estimate, for the encoder's log line.
    pub fn smoothed_loss_pct(&self) -> f32 {
        self.loss
    }

    fn congested(&self) -> bool {
        match (self.rtt, self.rtt_min) {
            (Some(rtt), Some(min)) => {
                rtt > min * RTT_INFLATION + RTT_MARGIN_MS && self.loss >= CONGESTED_LOSS
            }
            _ => false,
        }
    }

    /// The lowest rung currently reachable by a step down.
    fn floor_level(&self) -> usize {
        if self.congested_streak >= NARROW_AFTER_CONGESTED_EVALS {
            LADDER.len() - 1
        } else {
            NORMAL_FLOOR_LEVEL
        }
    }

    /// Feed one sample (every ~2 s) and get the profile to use.
    pub fn evaluate(&mut self, sample: LinkSample) -> Decision {
        let before = (self.level, self.red);

        // ── Estimate updates ────────────────────────────────────────
        // Smoothing: half-life of one evaluation, so a single lossy
        // window moves the estimate but does not own it. A stale sample
        // decays the estimate instead of freezing it.
        match sample.uplink_loss_pct {
            Some(l) => self.loss = 0.5 * self.loss + 0.5 * l.max(0.0),
            None => self.loss *= STALE_DECAY,
        }
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
        self.congested_streak = if congested {
            self.congested_streak.saturating_add(1)
        } else {
            0
        };
        self.evals_since_change = self.evals_since_change.saturating_add(1);
        self.evals_since_up = self.evals_since_up.saturating_add(1);

        // ── Episode bookkeeping ─────────────────────────────────────
        let dirty = congested || self.loss >= CLEAN_LOSS;
        if dirty {
            if self.dirty_evals == 0 {
                self.pre_episode_level = Some(self.level);
            }
            self.dirty_evals = self.dirty_evals.saturating_add(1);
            self.clean_streak = 0;
        }

        // ── Down ────────────────────────────────────────────────────
        // Dwell: act only on evidence that postdates the previous step —
        // unless that step was a climb, which fresh loss disproves and
        // should undo immediately.
        let may_step_down = self.evals_since_change >= DWELL_EVALS || self.last_change_was_up;
        if may_step_down && (congested || self.loss >= HEAVY_LOSS) {
            let rungs = if self.loss >= SEVERE_LOSS { 2 } else { 1 };
            self.step_down(rungs);
        }

        // ── RED ─────────────────────────────────────────────────────
        if congested {
            self.red = false;
            self.red_clean_streak = 0;
        } else if self.loss >= RED_ON_LOSS {
            if !self.red {
                self.red = true;
                if self.level < RED_MIN_LEVEL {
                    // Pay for the redundancy by dropping off the top
                    // rung. A rung change like any other for the dwell.
                    self.level = RED_MIN_LEVEL;
                    self.note_change_down();
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
        if !dirty {
            // First clean evaluation closes the episode and sizes the
            // climb: short blip → quick return to where we were; long
            // episode → the slow rung-by-rung probe.
            if self.dirty_evals > 0 {
                if self.dirty_evals <= SHORT_EPISODE_EVALS {
                    self.up_after_base = UP_AFTER_SHORT;
                    self.restore_to = self.pre_episode_level;
                } else {
                    self.up_after_base = UP_AFTER;
                    self.restore_to = None;
                }
                self.dirty_evals = 0;
                self.pre_episode_level = None;
            }
            self.clean_streak += 1;

            let floor = if self.red { RED_MIN_LEVEL } else { 0 };
            let hold = self.up_after_base.saturating_mul(self.probe_backoff);
            if self.clean_streak >= hold
                && self.level > floor
                && self.evals_since_change >= DWELL_EVALS
            {
                let target = match self.restore_to.take() {
                    Some(t) if t < self.level => t.max(floor),
                    _ => self.level - 1,
                };
                self.level = target;
                self.clean_streak = 0;
                self.evals_since_change = 0;
                self.evals_since_up = 0;
                self.last_change_was_up = true;
            }
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
        let floor = self.floor_level();
        let target = (self.level + n).min(floor);
        if target <= self.level {
            return;
        }
        self.level = target;
        self.note_change_down();
    }

    /// Shared bookkeeping for any downward rung change.
    fn note_change_down(&mut self) {
        self.clean_streak = 0;
        self.evals_since_change = 0;
        // A step down soon after a step up is a failed probe: wait twice
        // as long before trying that rung again. Any other step down is
        // a fresh episode; the backoff earned against an old one no
        // longer describes the link.
        if self.evals_since_up <= PROBE_FAIL_EVALS {
            self.probe_backoff = (self.probe_backoff * 2).min(PROBE_BACKOFF_MAX);
        } else {
            self.probe_backoff = 1;
        }
        self.last_change_was_up = false;
        // The rung we were told to return to has just been disproved.
        self.restore_to = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(loss: f32, rtt: f32) -> LinkSample {
        LinkSample {
            uplink_loss_pct: Some(loss),
            rtt_ms: Some(rtt),
        }
    }

    fn stale(rtt: f32) -> LinkSample {
        LinkSample {
            uplink_loss_pct: None,
            rtt_ms: Some(rtt),
        }
    }

    /// Drive to a steady clean state at the top rung.
    fn settled() -> RateController {
        let mut c = RateController::new();
        for _ in 0..10 {
            c.evaluate(sample(0.0, 40.0));
        }
        assert_eq!(c.current().level, 0);
        c
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
        let mut c = settled();
        for _ in 0..5 {
            c.evaluate(sample(15.0, 40.0)); // flat RTT: not congestion
        }
        let d = c.current();
        assert!(d.red, "RED should be on under random loss");
        assert_eq!(
            d.level, RED_MIN_LEVEL,
            "RED costs one rung, never runs at 48 kbps"
        );
    }

    /// The 2026-08-29..31 failure mode: one loss event, counted by every
    /// evaluation of the same 6 s transport window, used to ride the
    /// ladder 48 → 12 in three ticks. With the dwell, at most one rung
    /// change happens per loss window: here the RED turn-on (which pays
    /// one rung), then — a full dwell later — the severe two-rung step
    /// to the floor.
    #[test]
    fn dwell_prevents_free_fall_within_one_loss_window() {
        let mut c = settled();
        for _ in 0..3 {
            c.evaluate(sample(40.0, 40.0));
        }
        assert_eq!(
            c.current().level,
            1,
            "one rung change within the first loss window, not one per evaluation"
        );
        for _ in 0..2 {
            c.evaluate(sample(40.0, 40.0));
        }
        assert_eq!(
            c.current().level,
            NORMAL_FLOOR_LEVEL,
            "the dwelled severe step lands on the Wideband floor"
        );
    }

    #[test]
    fn ordinary_loss_floors_at_wideband_not_narrowband() {
        let mut c = settled();
        for _ in 0..40 {
            c.evaluate(sample(45.0, 40.0)); // heavy random loss, flat RTT
        }
        let d = c.current();
        assert_eq!(
            d.level, NORMAL_FLOOR_LEVEL,
            "flat-RTT loss must bottom out on 16k Wide, got rung {}",
            d.level
        );
        assert_eq!(LADDER[d.level].band, Band::Wide);
    }

    #[test]
    fn sustained_congestion_reaches_narrowband() {
        let mut c = settled();
        for _ in 0..40 {
            c.evaluate(sample(20.0, 300.0)); // loss + RTT ×7: congestion
        }
        assert_eq!(
            c.current().level,
            LADDER.len() - 1,
            "sustained congestion may use the Narrowband basement"
        );
        assert!(
            !c.current().red,
            "RED must not add bytes on a congested link"
        );
    }

    #[test]
    fn congestion_steps_down_and_never_enables_red() {
        let mut c = RateController::new();
        c.evaluate(sample(0.0, 40.0)); // establish baseline RTT
        for _ in 0..8 {
            c.evaluate(sample(12.0, 200.0));
        }
        let d = c.current();
        assert!(!d.red, "RED must not add bytes on a congested link");
        assert!(d.level >= 2, "expected several rungs down, got {}", d.level);
    }

    /// A blip (dirty ≤ ~10 s) must not cost minutes: quick climb hold,
    /// and the climb returns directly to the pre-episode rung.
    #[test]
    fn short_episode_recovers_directly_to_previous_rung() {
        let mut c = settled();
        for _ in 0..2 {
            c.evaluate(sample(40.0, 40.0)); // short severe burst
        }
        // Within one loss window the burst costs the RED rung only.
        assert_eq!(c.current().level, 1);
        let mut recovered_at = None;
        for i in 0..12 {
            let d = c.evaluate(sample(0.0, 40.0));
            if d.level == 0 {
                recovered_at = Some(i);
                break;
            }
        }
        let at = recovered_at.expect("should have returned to the top rung");
        // One quick jump once RED has drained (the climb floor is rung 1
        // while RED is on). Well under the old rung-by-rung minute-plus.
        assert!(
            at <= 11,
            "short episode should recover within ~22 s, took {at} evaluations"
        );
    }

    #[test]
    fn long_episode_recovers_one_rung_at_a_time() {
        let mut c = settled();
        for _ in 0..12 {
            c.evaluate(sample(40.0, 40.0)); // long episode → floor rung
        }
        assert_eq!(c.current().level, NORMAL_FLOOR_LEVEL);
        let mut levels = vec![c.current().level];
        for _ in 0..UP_AFTER * 6 {
            let d = c.evaluate(sample(0.0, 40.0));
            if d.level != *levels.last().unwrap() {
                levels.push(d.level);
            }
        }
        assert_eq!(
            levels,
            vec![3, 2, 1, 0],
            "long episodes climb rung by rung, got {levels:?}"
        );
    }

    /// The 2026-08-30 00:17 latch: the last loss reading before the
    /// signal went quiet must not pin the ladder. Stale samples decay
    /// the estimate and recovery proceeds.
    #[test]
    fn stale_loss_decays_and_recovery_proceeds() {
        let mut c = settled();
        for _ in 0..8 {
            c.evaluate(sample(40.0, 40.0));
        }
        assert_eq!(c.current().level, NORMAL_FLOOR_LEVEL);
        let mut level = c.current().level;
        for _ in 0..UP_AFTER * 8 {
            level = c.evaluate(stale(40.0)).level;
        }
        assert_eq!(level, 0, "stale input must decay and allow the climb");
    }

    /// While the loss signal stays dirty (the pinned screen-share case),
    /// the ladder must hold its rung — no false climbs.
    #[test]
    fn pinned_loss_never_climbs() {
        let mut c = settled();
        for _ in 0..30 {
            c.evaluate(sample(30.0, 190.0));
        }
        let low = c.current().level;
        for _ in 0..UP_AFTER * 4 {
            assert_eq!(c.evaluate(sample(30.0, 190.0)).level, low);
        }
    }

    #[test]
    fn failed_probe_doubles_the_wait_and_is_undone_immediately() {
        let mut c = settled();
        for _ in 0..12 {
            c.evaluate(sample(40.0, 40.0)); // long episode
        }
        let low = c.current().level;
        // Climb once…
        let mut climbed = None;
        for _ in 0..UP_AFTER * 2 {
            let d = c.evaluate(sample(0.0, 40.0));
            if d.level < low {
                climbed = Some(d.level);
                break;
            }
        }
        let up = climbed.expect("precondition: a climb happened");
        assert_eq!(c.probe_backoff, 1);
        // …and get punished on the very next window: undone at once
        // (no dwell for a probe revert), and the hold doubles. 60 % so a
        // single window lifts the smoothed estimate past SEVERE_LOSS.
        let d = c.evaluate(sample(60.0, 40.0));
        assert!(d.level > up, "failed probe must be reverted immediately");
        assert_eq!(c.probe_backoff, 2);
    }
}
