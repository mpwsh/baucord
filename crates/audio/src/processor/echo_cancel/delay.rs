//! Cross-correlation delay estimator.
//!
//! Finds the time offset between a reference signal (voice output)
//! and a captured signal (mic input or system loopback) using
//! frequency-domain cross-correlation.
//!
//! ## Algorithm
//!
//! 1. Collect ~50 ms of capture and ~200 ms of reference
//! 2. FFT both (zero-padded to same power-of-2 length)
//! 3. Cross-power spectrum: `capture_fft × conj(reference_fft)`
//! 4. IFFT → correlation function
//! 5. Peak position = delay in samples
//! 6. Peak height / total energy = confidence
//!
//! ## Re-estimation
//!
//! The delay is stable for a given audio device configuration but can
//! change on device hot-swap. The estimator re-runs every few seconds
//! and only updates the estimate when confidence exceeds a threshold.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Minimum correlation confidence to accept a delay estimate.
/// Below this, the signals aren't correlated enough (nobody talking,
/// or the reference doesn't match the capture).
const MIN_CONFIDENCE: f32 = 0.15;

/// How often to re-estimate the delay.
const UPDATE_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum delay to search for (in samples at 48 kHz).
/// 200 ms = 9600 samples — covers the longest acoustic echo path.
const MAX_DELAY_SAMPLES: usize = 9600;

/// Minimum delay to consider (in samples).
/// Below ~2 ms (96 samples) is physically impossible for acoustic echo
/// and implausible even for digital loopback.
const MIN_DELAY_SAMPLES: usize = 48;

pub struct DelayEstimator {
    /// Current best delay estimate (samples at 48 kHz).
    delay_samples: usize,
    /// Confidence of the current estimate (0.0–1.0).
    confidence: f32,
    /// When the last successful estimate was made.
    last_update: Instant,

    // ── FFT infrastructure (allocated once, reused) ─────────────
    fft_size: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,

    /// Scratch buffers for correlation computation.
    capture_fft: Vec<Complex<f32>>,
    reference_fft: Vec<Complex<f32>>,
    correlation: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,

    // ── Collection buffers ──────────────────────────────────────
    /// Accumulated capture samples for the next estimate.
    capture_accum: Vec<f32>,
    /// Accumulated reference samples for the next estimate.
    reference_accum: Vec<f32>,
    /// Capture samples needed before running correlation.
    capture_target: usize,
    /// Reference samples needed (longer window to search for delay).
    reference_target: usize,
}

impl DelayEstimator {
    /// Create a new delay estimator.
    ///
    /// - `initial_delay`: starting guess in samples (e.g., 960 for 20 ms)
    /// - `sample_rate`: audio sample rate (48000)
    pub fn new(initial_delay: usize, sample_rate: usize) -> Self {
        // Capture window: ~50 ms
        let capture_target = (sample_rate * 50) / 1000;
        // Reference window: ~250 ms (capture + max search range)
        let reference_target = capture_target + MAX_DELAY_SAMPLES;

        // FFT size: next power of 2 above reference_target + capture_target
        // (for linear cross-correlation without circular aliasing)
        let min_fft = reference_target + capture_target;
        let fft_size = min_fft.next_power_of_two();

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let ifft = planner.plan_fft_inverse(fft_size);
        let scratch_len = fft
            .get_inplace_scratch_len()
            .max(ifft.get_inplace_scratch_len());

        Self {
            delay_samples: initial_delay,
            confidence: 0.0,
            last_update: Instant::now(),

            fft_size,
            fft,
            ifft,

            capture_fft: vec![Complex::new(0.0, 0.0); fft_size],
            reference_fft: vec![Complex::new(0.0, 0.0); fft_size],
            correlation: vec![Complex::new(0.0, 0.0); fft_size],
            scratch: vec![Complex::new(0.0, 0.0); scratch_len],

            capture_accum: Vec::with_capacity(capture_target),
            reference_accum: Vec::with_capacity(reference_target),
            capture_target,
            reference_target,
        }
    }

    /// Current delay estimate in samples.
    pub fn delay_samples(&self) -> usize {
        self.delay_samples
    }

    /// Confidence of the current estimate (0.0–1.0).
    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    /// Whether the estimate is reliable enough for subtraction.
    pub fn is_confident(&self) -> bool {
        self.confidence >= MIN_CONFIDENCE
    }

    /// Feed capture audio (mic or loopback) for delay estimation.
    ///
    /// Call this every frame with the same audio the echo canceller
    /// processes. Samples accumulate until enough are collected for
    /// the next correlation pass.
    pub fn feed_capture(&mut self, samples: &[f32]) {
        if self.last_update.elapsed() < UPDATE_INTERVAL {
            return; // not time to re-estimate yet
        }

        let remaining = self.capture_target.saturating_sub(self.capture_accum.len());
        let take = samples.len().min(remaining);
        self.capture_accum.extend_from_slice(&samples[..take]);
    }

    /// Feed reference audio (from VoiceReferenceRing) for delay estimation.
    ///
    /// Call this every frame alongside `feed_capture`. The reference
    /// window is longer than the capture window to allow searching
    /// for the delay offset.
    pub fn feed_reference(&mut self, samples: &[f32]) {
        if self.last_update.elapsed() < UPDATE_INTERVAL {
            return;
        }

        let remaining = self
            .reference_target
            .saturating_sub(self.reference_accum.len());
        let take = samples.len().min(remaining);
        self.reference_accum.extend_from_slice(&samples[..take]);
    }

    /// Try to compute a new delay estimate.
    ///
    /// Returns `true` if a new estimate was computed (whether or not
    /// it was confident enough to update the stored delay).
    ///
    /// Call this every frame after feeding capture and reference.
    /// It only runs when both buffers have enough samples.
    pub fn try_estimate(&mut self) -> bool {
        // Not enough data yet
        if self.capture_accum.len() < self.capture_target
            || self.reference_accum.len() < self.reference_target
        {
            return false;
        }

        // Check that at least one signal has meaningful energy.
        let capture_energy: f32 = self.capture_accum.iter().map(|s| s * s).sum();
        let reference_energy: f32 = self.reference_accum.iter().map(|s| s * s).sum();

        let capture_rms = (capture_energy / self.capture_accum.len() as f32).sqrt();
        let reference_rms = (reference_energy / self.reference_accum.len() as f32).sqrt();

        if capture_rms < 0.001 || reference_rms < 0.001 {
            self.capture_accum.clear();
            self.reference_accum.clear();
            self.last_update = Instant::now();
            return false;
        }

        // ── Build zero-padded FFT input buffers ─────────────────
        // Right-align both signals so that a positive lag in the
        // cross-correlation directly equals the physical delay
        // (capture is delayed relative to reference).

        // Capture: right-aligned
        for c in self.capture_fft.iter_mut() {
            *c = Complex::new(0.0, 0.0);
        }
        let capture_offset = self.fft_size - self.capture_accum.len();
        for (i, &s) in self.capture_accum.iter().enumerate() {
            self.capture_fft[capture_offset + i] = Complex::new(s, 0.0);
        }

        // Reference: right-aligned
        for c in self.reference_fft.iter_mut() {
            *c = Complex::new(0.0, 0.0);
        }
        let reference_offset = self.fft_size - self.reference_accum.len();
        for (i, &s) in self.reference_accum.iter().enumerate() {
            self.reference_fft[reference_offset + i] = Complex::new(s, 0.0);
        }

        // ── Forward FFT both signals ────────────────────────────
        self.fft
            .process_with_scratch(&mut self.capture_fft, &mut self.scratch);
        self.fft
            .process_with_scratch(&mut self.reference_fft, &mut self.scratch);

        // ── Cross-power spectrum: capture × conj(reference) ─────
        for i in 0..self.fft_size {
            self.correlation[i] = self.capture_fft[i] * self.reference_fft[i].conj();
        }

        // ── IFFT → cross-correlation ────────────────────────────
        self.ifft
            .process_with_scratch(&mut self.correlation, &mut self.scratch);

        let norm = 1.0 / self.fft_size as f32;

        // ── Find peak in valid delay range ──────────────────────
        let search_end = MAX_DELAY_SAMPLES.min(self.fft_size / 2);
        let mut best_lag = self.delay_samples;
        let mut best_value = 0.0f32;
        let mut total_energy = 0.0f32;

        for lag in MIN_DELAY_SAMPLES..search_end {
            let value = self.correlation[lag].re * norm;
            let abs_value = value.abs();
            total_energy += abs_value;

            if abs_value > best_value {
                best_value = abs_value;
                best_lag = lag;
            }
        }

        // ── Compute confidence ──────────────────────────────────
        let avg_energy = if search_end > MIN_DELAY_SAMPLES {
            total_energy / (search_end - MIN_DELAY_SAMPLES) as f32
        } else {
            0.0
        };

        let new_confidence = if avg_energy > 1e-10 {
            (best_value / avg_energy - 1.0).max(0.0) / 10.0
        } else {
            0.0
        };
        let new_confidence = new_confidence.clamp(0.0, 1.0);

        // ── Update estimate if confident ────────────────────────
        if new_confidence >= MIN_CONFIDENCE {
            self.delay_samples = best_lag;
            self.confidence = new_confidence;
        } else {
            self.confidence *= 0.9;
        }

        // ── Reset for next cycle ────────────────────────────────
        self.capture_accum.clear();
        self.reference_accum.clear();
        self.last_update = Instant::now();

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_delay() {
        let mut estimator = DelayEstimator::new(0, 48000);
        estimator.last_update = Instant::now() - UPDATE_INTERVAL - Duration::from_secs(1);

        let delay = 480usize; // 10 ms

        // Use deterministic pseudo-random broadband signal.
        // LCG gives us repeatable "noise" without needing a rand crate.
        let mut seed: u32 = 12345;
        let mut lcg = || -> f32 {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            // Map to -1.0..1.0
            (seed >> 16) as f32 / 32768.0 - 1.0
        };

        let reference: Vec<f32> = (0..estimator.reference_target).map(|_| lcg()).collect();

        // Capture: delayed copy. capture[t] = reference[t - delay + offset]
        // where offset aligns capture window within reference.
        let offset = estimator.reference_target - estimator.capture_target;
        let capture: Vec<f32> = (0..estimator.capture_target)
            .map(|i| {
                let ref_idx = offset + i;
                if ref_idx >= delay {
                    reference[ref_idx - delay]
                } else {
                    0.0
                }
            })
            .collect();

        estimator.feed_reference(&reference);
        estimator.feed_capture(&capture);

        assert!(estimator.try_estimate());
        assert!(
            estimator.is_confident(),
            "Confidence too low: {}",
            estimator.confidence()
        );

        let estimated = estimator.delay_samples();
        assert!(
            estimated.abs_diff(delay) <= 5,
            "Expected delay ~{delay}, got {estimated} (confidence: {})",
            estimator.confidence()
        );
    }

    #[test]
    fn silence_returns_no_estimate() {
        let mut estimator = DelayEstimator::new(960, 48000);
        estimator.last_update = Instant::now() - UPDATE_INTERVAL - Duration::from_secs(1);

        let silence = vec![0.0f32; estimator.reference_target];
        let capture_silence = vec![0.0f32; estimator.capture_target];

        estimator.feed_reference(&silence);
        estimator.feed_capture(&capture_silence);

        // Should run but not find a confident estimate
        estimator.try_estimate();
        assert!(!estimator.is_confident());

        // Delay should remain at initial value
        assert_eq!(estimator.delay_samples(), 960);
    }

    #[test]
    fn skips_when_not_due() {
        let estimator_delay = 960;
        let mut estimator = DelayEstimator::new(estimator_delay, 48000);
        // last_update is now — should skip

        let samples = vec![1.0f32; 480];
        estimator.feed_capture(&samples);
        estimator.feed_reference(&samples);

        // Accumulators should be empty (feeds were skipped)
        assert!(estimator.capture_accum.is_empty());
        assert!(estimator.reference_accum.is_empty());
    }
}
