//! Per-peer output limiter for hearing protection.
//!
//! Prevents loud transients (screams, mic bumps, feedback) from
//! saturating headphone speakers. Gain only goes *down*, never above
//! 1.0 — quiet talkers are never boosted (users adjust input gain
//! manually for that).
//!
//! ## Design
//!
//! - **Threshold**: RMS level above which gain reduction kicks in.
//!   Below this, signal passes at unity gain (1.0).
//! - **Fast attack** (~1ms): Catch transients before they hurt.
//!   The limiter reacts within ~48 samples at 48 kHz.
//! - **Slow release** (~500ms): Don't pump back up immediately after
//!   a scream — hold the reduced gain to avoid jarring volume jumps.
//! - **Soft knee**: Gradual onset of gain reduction around the
//!   threshold, avoiding the hard click a brick-wall limiter produces.
//! - **Per-peer**: Each peer gets their own limiter instance so one
//!   screamer doesn't duck everyone else in the mix.
//!
//! ## Pipeline position
//!
//! Sits in the jitter buffer mixer, applied per-peer after their
//! individual volume setting but before summing into the output mix:
//!
//! ```text
//! peer buffer → volume scale → LIMITER → sum into mix
//! ```

/// Per-peer output limiter.
///
/// Instantiate one per peer in the jitter buffer. Call [`process`]
/// on each peer's samples before mixing.
pub struct Limiter {
    /// RMS threshold (linear, 0.0–1.0) above which gain reduction begins.
    /// Default 0.4 (~-8 dBFS) — normal speech sits around 0.05–0.15,
    /// so this only activates on genuinely loud signals.
    threshold: f32,

    /// Current applied gain (0.0–1.0, never above 1.0).
    gain: f32,

    /// Smoothed RMS envelope for stable level tracking.
    envelope: f32,

    /// Attack coefficient (per-sample). Fast — respond to loud transients
    /// within ~48 samples (1ms at 48 kHz).
    attack_coeff: f32,

    /// Release coefficient (per-sample). Slow — hold reduced gain for
    /// ~500ms after the loud signal ends to avoid pumping.
    release_coeff: f32,

    /// Envelope attack coefficient (per-sample). Tracks rising levels
    /// faster than falling levels.
    env_attack_coeff: f32,

    /// Envelope release coefficient (per-sample).
    env_release_coeff: f32,
}
impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}
impl Limiter {
    /// Create a new limiter with default voice-chat tuning.
    ///
    /// Suitable for 48 kHz, 480-sample (10ms) frames.
    pub fn new() -> Self {
        // Coefficients derived for 48 kHz sample rate:
        //
        // attack ~1ms:   1 - e^(-1 / (0.001 * 48000)) ≈ 0.021
        // release ~500ms: 1 - e^(-1 / (0.500 * 48000)) ≈ 0.000042
        //
        // Envelope tracking:
        // env_attack ~2ms:  1 - e^(-1 / (0.002 * 48000)) ≈ 0.010
        // env_release ~50ms: 1 - e^(-1 / (0.050 * 48000)) ≈ 0.000417
        Self {
            threshold: 0.4,
            gain: 1.0,
            envelope: 0.0,
            attack_coeff: 0.021,
            release_coeff: 0.000042,
            env_attack_coeff: 0.010,
            env_release_coeff: 0.000417,
        }
    }

    /// Create a limiter with a custom threshold.
    ///
    /// - `threshold`: RMS level (0.05–0.9) above which limiting starts.
    ///   Lower values are more aggressive. 0.4 is a good default.
    pub fn with_threshold(threshold: f32) -> Self {
        let mut limiter = Self::new();
        limiter.threshold = threshold.clamp(0.05, 0.9);
        limiter
    }

    /// Process a slice of samples in-place.
    ///
    /// Call once per frame (480 samples at 48 kHz) per peer,
    /// after applying per-peer volume but before summing into the mix.
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            // Update envelope (asymmetric: fast rise, slower fall)
            let abs = s.abs();
            let env_coeff = if abs > self.envelope {
                self.env_attack_coeff
            } else {
                self.env_release_coeff
            };
            self.envelope = self.envelope + env_coeff * (abs - self.envelope);

            // Compute desired gain
            let desired = if self.envelope > self.threshold {
                // Soft knee: ratio compression above threshold.
                // desired_gain = threshold / envelope
                // This gives smooth gain reduction proportional to how far
                // above threshold we are, rather than a hard clip.
                self.threshold / self.envelope
            } else {
                1.0
            };

            // Smooth gain changes (asymmetric attack/release)
            let coeff = if desired < self.gain {
                self.attack_coeff // gain decreasing — fast
            } else {
                self.release_coeff // gain recovering — slow
            };
            self.gain = self.gain + coeff * (desired - self.gain);

            // Never boost — clamp gain to [0, 1]
            self.gain = self.gain.min(1.0);

            *s = (*s * self.gain).clamp(-0.99, 0.99);
        }
    }

    /// Reset limiter state (e.g., when a peer reconnects).
    pub fn reset(&mut self) {
        self.gain = 1.0;
        self.envelope = 0.0;
    }

    /// Current gain being applied (for diagnostics). 1.0 = no reduction.
    pub fn current_gain(&self) -> f32 {
        self.gain
    }
}
