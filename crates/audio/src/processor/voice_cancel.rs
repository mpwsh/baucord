//! Voice cancellation for stream audio playback.
//!
//! When a viewer watches a screen share, the stream audio contains a
//! delayed copy of everyone's voice chat (captured by the streamer's
//! loopback). The viewer already hears voices through the low-latency
//! voice channel, so this duplicate is pure echo.
//!
//! This module suppresses the voice echo using energy-based ducking:
//! when the voice channel is producing audio, the stream audio is
//! attenuated proportionally. Same approach as Discord, Xbox, and PS5
//! party chat — game audio dips slightly during speech.
//!
//! ## Pipeline position (stream output)
//!
//! ```text
//! stream buffer → **voice_cancel** → gain → clamp → resample → playback
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.voice_energy` (RMS level from voice output, 0.0–1.0)
//! - **Writes**: nothing

use super::{AudioProcessor, FrameContext};

/// Removes or attenuates voice chat audio from stream playback.
///
/// Each frame:
/// 1. Reads voice energy from `FrameContext`
/// 2. Computes a target ducking gain (proportional to voice loudness)
/// 3. Applies the gain per-sample with smoothing to avoid clicks
pub struct VoiceCanceller {
    /// Smoothed voice energy (from FrameContext each frame).
    voice_energy: f32,
    /// Current ducking gain (0.0–1.0), smoothed to avoid clicks.
    current_gain: f32,
    /// Target gain based on voice energy.
    target_gain: f32,

    // ── Configuration ───────────────────────────────────────────
    /// Energy threshold below which voice is considered silent (linear).
    /// ~-40 dBFS — only real speech triggers ducking, not idle noise.
    silence_threshold: f32,
    /// Minimum gain when voice is active (0.0 = full mute, 1.0 = no duck).
    min_gain: f32,
    /// Gain smoothing coefficient per sample (~5 ms at 48 kHz).
    smooth_coeff: f32,
}

impl Default for VoiceCanceller {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceCanceller {
    pub fn new() -> Self {
        Self {
            voice_energy: 0.0,
            current_gain: 1.0,
            target_gain: 1.0,
            silence_threshold: 0.005,
            min_gain: 0.25,
            smooth_coeff: 1.0 / (48_000.0 * 0.005),
        }
    }

    /// Current ducking gain (for debug/stats display).
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Whether the canceller is currently ducking (gain < 1.0).
    pub fn is_active(&self) -> bool {
        self.current_gain < 0.99
    }

    /// Update the smoothed voice energy and recompute target gain.
    fn update_voice_energy(&mut self, voice_rms: f32) {
        // Asymmetric smoothing: fast attack (70%), slow release (10%).
        // Fast attack reacts quickly when someone starts talking.
        // Slow release holds the duck briefly after speech ends to
        // cover the stream's delayed echo tail.
        if voice_rms > self.voice_energy {
            self.voice_energy = self.voice_energy * 0.3 + voice_rms * 0.7;
        } else {
            self.voice_energy = self.voice_energy * 0.9 + voice_rms * 0.1;
        }

        if self.voice_energy > self.silence_threshold {
            let ratio = self.silence_threshold / self.voice_energy;
            self.target_gain = ratio.clamp(self.min_gain, 1.0);
        } else {
            self.target_gain = 1.0;
        }
    }

    /// Apply per-sample gain smoothing to avoid clicks on transitions.
    fn apply_ducking(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            if self.current_gain < self.target_gain {
                self.current_gain = (self.current_gain + self.smooth_coeff).min(self.target_gain);
            } else {
                self.current_gain = (self.current_gain - self.smooth_coeff).max(self.target_gain);
            }
            *s *= self.current_gain;
        }
    }
}

impl AudioProcessor for VoiceCanceller {
    fn name(&self) -> &str {
        "voice_cancel"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        self.update_voice_energy(ctx.voice_energy);
        self.apply_ducking(samples);
    }

    fn reset(&mut self) {
        self.voice_energy = 0.0;
        self.current_gain = 1.0;
        self.target_gain = 1.0;
    }
}