//! Capture-clock skew estimation and zero-latency correction.
//!
//! Consumer capture devices do not run at exactly their nominal rate:
//! crystal tolerance is ±100 ppm, and USB / wireless headset dongles
//! (which bring their own clock domain) can sit several hundred ppm
//! off. A sender whose clock is slow delivers audio slower than every
//! receiver consumes it, so every receiver's jitter buffer *for that
//! sender* slowly drains and punctures — recurring mid-word holes that
//! are indistinguishable from packet loss, on links that measure
//! clean. Field signature: the same few users "break up" for the
//! whole room after minutes of continuous talking, and a reconnect
//! (or any speech pause long enough to re-prime) temporarily fixes it.
//!
//! The fix is applied at the source, in two parts:
//!
//! - [`CaptureClockEstimator`] measures the device's true rate by
//!   ordinary least squares over (elapsed, samples-delivered)
//!   observations. Sample counts are exact — they must be taken in
//!   the capture callback, before mute checks and ring pushes — so
//!   only the timestamps carry scheduler noise, which the regression
//!   averages out. The estimate is inside ~10 ppm within a couple of
//!   minutes and keeps tightening.
//! - [`SampleSlip`] corrects the 48 kHz stream by inserting or
//!   dropping single samples (midpoint-merged, at the quietest point
//!   of each chunk) at the estimated skew rate. Unlike steering a
//!   sinc resampler this adds **zero** group delay — this codebase is
//!   latency-first — and it also covers devices that claim native
//!   48 kHz and therefore have no resampler in their path at all,
//!   which is exactly the USB-dongle case that triggered this work.
//!
//! At the ±2000 ppm correction ceiling the slip rate is one sample
//! per ~500 (a ~3.5-cent time-scale change, below audibility); at
//! realistic skews it is one sample per several thousand.
//!
//! The receiving side carries an independent per-peer defense in
//! `transport::jitter_buffer`, which protects listeners from senders
//! running builds that predate this module.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Ignore the first stretch of a stream: drivers deliver ragged
/// bursts while their buffers settle, which would bias the regression.
const WARMUP: Duration = Duration::from_secs(10);

/// Minimum observed span before an estimate is reported. Shorter
/// spans put the regression's own error above the signal.
const MIN_SPAN: Duration = Duration::from_secs(30);

/// Cadence at which (time, samples) observations are recorded.
const OBSERVATION_INTERVAL: Duration = Duration::from_millis(100);

/// Observation ring capacity: 5 minutes at the cadence above. Old
/// points age out so a thermally drifting clock is tracked rather
/// than averaged away over an entire marathon session.
const MAX_OBSERVATIONS: usize = 3_000;

/// Skew magnitudes below this are measurement noise; report zero so
/// the corrector leaves healthy devices completely untouched.
const SKEW_DEADZONE: f64 = 20e-6;

/// Hard clamp on the reported skew (±2000 ppm). Anything larger is a
/// broken driver, not a crystal, and correcting harder than this
/// risks audibility.
const MAX_SKEW: f64 = 2_000e-6;

/// Estimates the true rate of a capture clock against the monotonic
/// clock from cumulative sample counts.
///
/// Feed it the running total of samples the *device* produced (count
/// in the capture callback, before any ring-buffer drops, so overruns
/// and mute cannot bias the measurement) and query
/// [`skew`](Self::skew).
pub struct CaptureClockEstimator {
    nominal_rate: f64,
    created: Instant,
    /// Anchor for relative coordinates: (time, cumulative samples) of
    /// the first post-warmup observation. Only differences from the
    /// anchor enter the math, keeping every value small and exact.
    origin: Option<(Instant, u64)>,
    last_observation: Option<Instant>,
    /// (seconds since origin, samples since origin).
    points: VecDeque<(f64, f64)>,
}

impl CaptureClockEstimator {
    #[must_use]
    pub fn new(nominal_rate: u32, now: Instant) -> Self {
        Self {
            nominal_rate: f64::from(nominal_rate),
            created: now,
            origin: None,
            last_observation: None,
            points: VecDeque::with_capacity(MAX_OBSERVATIONS),
        }
    }

    /// Record one observation of the device's cumulative sample count.
    /// Call as often as convenient; internally gated to
    /// [`OBSERVATION_INTERVAL`].
    pub fn observe(&mut self, now: Instant, cumulative_samples: u64) {
        if now.duration_since(self.created) < WARMUP {
            return;
        }
        if let Some(last) = self.last_observation {
            if now.duration_since(last) < OBSERVATION_INTERVAL {
                return;
            }
        }
        self.last_observation = Some(now);

        let (t0, s0) = *self.origin.get_or_insert((now, cumulative_samples));
        // f64 holds relative sample counts exactly far past any session
        // length (2^53 samples ≈ 5,900 years at 48 kHz).
        #[allow(clippy::cast_precision_loss)] // counts are far below 2^53
        let point = (
            now.duration_since(t0).as_secs_f64(),
            cumulative_samples.wrapping_sub(s0) as f64,
        );
        if self.points.len() == MAX_OBSERVATIONS {
            self.points.pop_front();
        }
        self.points.push_back(point);
    }

    /// Estimated clock skew as a signed fraction of nominal
    /// (`+` = device fast, `-` = device slow), or `None` before the
    /// estimator has enough span to be trustworthy. Values inside the
    /// dead zone report as `Some(0.0)`.
    #[must_use]
    pub fn skew(&self) -> Option<f64> {
        let (first, last) = (self.points.front()?, self.points.back()?);
        if last.0 - first.0 < MIN_SPAN.as_secs_f64() {
            return None;
        }

        // Ordinary least squares: the slope of samples over seconds is
        // the measured device rate.
        #[allow(clippy::cast_precision_loss)] // point count ≤ 3000
        let n = self.points.len() as f64;
        let (mut sum_t, mut sum_s) = (0.0f64, 0.0f64);
        for (t, s) in &self.points {
            sum_t += t;
            sum_s += s;
        }
        let (t_mean, s_mean) = (sum_t / n, sum_s / n);
        let (mut cov, mut var) = (0.0f64, 0.0f64);
        for (t, s) in &self.points {
            cov += (t - t_mean) * (s - s_mean);
            var += (t - t_mean) * (t - t_mean);
        }
        if var <= f64::EPSILON {
            return None;
        }

        let skew = (cov / var / self.nominal_rate - 1.0).clamp(-MAX_SKEW, MAX_SKEW);
        Some(if skew.abs() < SKEW_DEADZONE { 0.0 } else { skew })
    }
}

// ────────────────────────────────────────────────────────────────────

/// Fraction of the remaining distance to the target the applied
/// correction moves per [`SampleSlip::set_target_skew`] call (the
/// caller's ~1 s cadence), so a changed estimate ramps in over a few
/// seconds instead of stepping.
const SLEW_FRACTION: f64 = 0.2;

/// Chunks shorter than this are passed through untouched — too short
/// to place a slip away from the edges.
const MIN_SLIP_CHUNK: usize = 8;

/// Zero-latency rate correction by single-sample insert/drop.
///
/// `skew > 0` (device fast → surplus samples) drops one sample per
/// `1/skew` samples; `skew < 0` inserts. Slips are midpoint merges
/// placed at the quietest sample of the middle half of each chunk, so
/// the waveform stays first-order continuous — no step, no click.
pub struct SampleSlip {
    /// Correction currently applied (slewed toward `target`).
    applied: f64,
    target: f64,
    /// Fractional slip carry across chunks. Stays within (-2, 2): one
    /// 480-sample chunk at the ±2000 ppm ceiling adds at most 0.96.
    frac: f64,
}

impl Default for SampleSlip {
    fn default() -> Self {
        Self::new()
    }
}

impl SampleSlip {
    #[must_use]
    pub fn new() -> Self {
        Self {
            applied: 0.0,
            target: 0.0,
            frac: 0.0,
        }
    }

    /// Update the target correction from the estimator. `None` (the
    /// estimator is still warming up) holds the current target.
    pub fn set_target_skew(&mut self, skew: Option<f64>) {
        if let Some(s) = skew {
            self.target = s.clamp(-MAX_SKEW, MAX_SKEW);
        }
        self.applied += SLEW_FRACTION * (self.target - self.applied);
    }

    /// Currently applied correction in ppm (for logs and stats).
    #[must_use]
    pub fn correction_ppm(&self) -> f64 {
        self.applied * 1e6
    }

    /// Apply the correction to one chunk of mono samples, mutating it
    /// in place. Length changes by at most ±1 per call at the clamp.
    pub fn process(&mut self, samples: &mut Vec<f32>) {
        if self.applied == 0.0 || samples.len() < MIN_SLIP_CHUNK {
            return;
        }
        #[allow(clippy::cast_precision_loss)] // chunk length ≤ a few thousand
        {
            self.frac += samples.len() as f64 * self.applied;
        }
        while self.frac >= 1.0 {
            drop_one(samples);
            self.frac -= 1.0;
        }
        while self.frac <= -1.0 {
            insert_one(samples);
            self.frac += 1.0;
        }
    }
}

/// Index of the smallest-magnitude sample in the middle half of the
/// chunk — the least audible place to slip.
fn quietest_index(samples: &[f32]) -> usize {
    let (lo, hi) = (samples.len() / 4, samples.len() * 3 / 4);
    let mut best = lo;
    for i in lo..hi {
        if samples[i].abs() < samples[best].abs() {
            best = i;
        }
    }
    best
}

/// Remove one sample by merging a neighboring pair into its midpoint —
/// first-order continuous, no step.
fn drop_one(samples: &mut Vec<f32>) {
    let i = quietest_index(samples).min(samples.len() - 2);
    samples[i] = 0.5 * (samples[i] + samples[i + 1]);
    samples.remove(i + 1);
}

/// Insert one sample as the midpoint of a neighboring pair.
fn insert_one(samples: &mut Vec<f32>) {
    let i = quietest_index(samples).max(1);
    let mid = 0.5 * (samples[i - 1] + samples[i]);
    samples.insert(i, mid);
}

#[cfg(test)]
mod tests {
    use super::*;

    mod capture_clock_estimator {
        use super::*;

        /// Simulate `secs` seconds of a device running at `rate` Hz,
        /// observed every 100 ms with `noise_ms` of timing jitter
        /// applied cyclically.
        fn feed(rate: f64, secs: u64, noise_ms: &[f64]) -> CaptureClockEstimator {
            let t0 = Instant::now();
            let mut est = CaptureClockEstimator::new(48_000, t0);
            for k in 0..(secs * 10) {
                let ms = k * 100;
                let jitter = noise_ms[(k as usize) % noise_ms.len()];
                let now = t0 + Duration::from_micros(ms * 1000 + (jitter * 1000.0) as u64);
                let samples = (ms as f64 / 1000.0 * rate) as u64;
                est.observe(now, samples);
            }
            est
        }

        #[test]
        fn reports_none_before_min_span() {
            let est = feed(48_000.0, 20, &[0.0]);
            assert!(est.skew().is_none());
        }

        #[test]
        fn reports_zero_for_a_clock_inside_the_dead_zone() {
            // ~4 ppm fast — real crystals are never this good; the
            // dead zone must swallow it.
            let est = feed(48_000.2, 120, &[0.0, 0.4, 0.9]);
            assert_eq!(est.skew(), Some(0.0));
        }

        #[test]
        fn measures_a_500ppm_fast_clock_within_50ppm_despite_timing_noise() {
            let est = feed(48_024.0, 120, &[0.0, 1.3, 0.2, 1.9, 0.7]);
            let ppm = est.skew().expect("span reached") * 1e6;
            assert!((ppm - 500.0).abs() < 50.0, "estimated {ppm:.0} ppm");
        }

        #[test]
        fn measures_a_slow_clock_with_negative_sign() {
            let est = feed(47_976.0, 120, &[0.0, 1.1]); // 500 ppm slow
            assert!(est.skew().expect("span reached") < -400e-6);
        }

        #[test]
        fn clamps_a_broken_driver_to_the_correction_ceiling() {
            let est = feed(49_000.0, 120, &[0.0]); // ~2 % fast
            let skew = est.skew().expect("span reached");
            assert!((skew - MAX_SKEW).abs() < 1e-9, "skew {skew}");
        }
    }

    mod sample_slip {
        use super::*;

        fn sine(len: usize) -> Vec<f32> {
            (0..len)
                .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
                .collect()
        }

        fn converged(skew: f64) -> SampleSlip {
            let mut slip = SampleSlip::new();
            for _ in 0..60 {
                slip.set_target_skew(Some(skew));
            }
            slip
        }

        #[test]
        fn drops_about_one_sample_per_thousand_at_plus_1000ppm() {
            let mut slip = converged(1_000e-6);
            let mut total = 0usize;
            for _ in 0..100 {
                let mut chunk = sine(480);
                slip.process(&mut chunk);
                total += chunk.len();
            }
            let dropped = 48_000 - total;
            assert!((46..=50).contains(&dropped), "dropped {dropped}");
        }

        #[test]
        fn inserts_samples_for_a_slow_clock() {
            let mut slip = converged(-1_000e-6);
            let mut total = 0usize;
            for _ in 0..100 {
                let mut chunk = sine(480);
                slip.process(&mut chunk);
                total += chunk.len();
            }
            assert!(total > 48_040, "inserted only {}", total as i64 - 48_000);
        }

        #[test]
        fn slip_introduces_no_step_discontinuity_on_a_sine() {
            let mut slip = converged(2_000e-6);
            let clean = sine(480);
            let max_delta_clean = clean
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0f32, f32::max);
            let mut worst = 0.0f32;
            for _ in 0..50 {
                let mut chunk = sine(480);
                slip.process(&mut chunk);
                worst = chunk
                    .windows(2)
                    .map(|w| (w[1] - w[0]).abs())
                    .fold(worst, f32::max);
            }
            assert!(
                worst <= max_delta_clean * 1.5,
                "worst step {worst} vs clean {max_delta_clean}"
            );
        }

        #[test]
        fn zero_skew_leaves_the_chunk_untouched() {
            let mut slip = SampleSlip::new();
            slip.set_target_skew(Some(0.0));
            let original = sine(480);
            let mut chunk = original.clone();
            slip.process(&mut chunk);
            assert_eq!(chunk, original);
        }
    }
}
