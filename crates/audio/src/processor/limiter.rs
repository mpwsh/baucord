//! Per-peer output limiter for hearing protection.
//!
//! Prevents loud transients (screams, mic bumps, feedback) from
//! saturating headphone speakers. Gain only goes *down*, never above
//! 1.0 — quiet talkers are never boosted.
//!
//! ## Pipeline position (output side)
//!
//! ```text
//! peer buffer → volume scale → **limiter** → sum into mix
//! ```
//!
//! Each peer gets their own limiter instance in the jitter buffer so
//! one screamer doesn't duck everyone else in the mix.
//!
//! ## Context interaction
//!
//! - **Reads**: nothing
//! - **Writes**: nothing
//!
//! The limiter is self-contained: it tracks its own envelope and gain.
//! It is used per-peer in the jitter buffer and once more on the master
//! bus after output gain — both on the output worker, neither in a
//! `ProcessorChain`, so it does not implement `AudioProcessor`.

/// Per-peer output limiter.
///
/// Instantiate one per peer. Call [`Limiter::process_samples`] on each
/// peer's samples before mixing.
pub struct Limiter {
    threshold: f32,
    gain: f32,
    envelope: f32,
    attack_coeff: f32,
    release_coeff: f32,
    env_attack_coeff: f32,
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
    /// Per-sample coefficients for 48 kHz; any block length works.
    /// Threshold 0.4 (~-8 dBFS) — normal speech sits around 0.05–0.15.
    pub fn new() -> Self {
        // Coefficients for 48 kHz:
        // attack ~1 ms:   1 - e^(-1 / (0.001 * 48000)) ≈ 0.021
        // release ~500 ms: 1 - e^(-1 / (0.500 * 48000)) ≈ 0.000042
        // env_attack ~2 ms:  ≈ 0.010
        // env_release ~50 ms: ≈ 0.000417
        Self {
            threshold: 0.4,
            gain: 1.0,
            envelope: 0.0,
            attack_coeff: 0.021,
            release_coeff: 0.000_042,
            env_attack_coeff: 0.010,
            env_release_coeff: 0.000_417,
        }
    }

    /// Limit samples in place. Output is bounded to ±0.99, so nothing
    /// downstream needs to clamp again.
    pub fn process_samples(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            // Asymmetric envelope: fast rise, slower fall
            let abs = s.abs();
            let env_coeff = if abs > self.envelope {
                self.env_attack_coeff
            } else {
                self.env_release_coeff
            };
            self.envelope += env_coeff * (abs - self.envelope);

            // Desired gain: soft ratio compression above threshold
            let desired = if self.envelope > self.threshold {
                self.threshold / self.envelope
            } else {
                1.0
            };

            // Smooth gain changes (fast attack, slow release)
            let coeff = if desired < self.gain {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.gain += coeff * (desired - self.gain);
            self.gain = self.gain.min(1.0);

            *s = (*s * self.gain).clamp(-0.99, 0.99);
        }
    }
}
