//! Voice cancellation for stream audio playback.
//!
//! When a viewer watches a screen share, the stream audio contains a
//! delayed copy of everyone's voice chat (captured by the streamer's
//! loopback). The viewer already hears voices through the low-latency
//! voice channel, so this duplicate is pure echo.
//!
//! This module suppresses the voice echo in the stream audio using the
//! viewer's own voice output energy as a reference signal.
//!
//! ## Current implementation: energy-based ducking
//!
//! When the voice channel is producing audio (anyone is talking), the
//! stream audio is attenuated proportionally. This is the same approach
//! Discord, Xbox, and PS5 party chat use — game audio dips slightly
//! during speech, which users expect and barely notice.
//!
//! ## Future: spectral subtraction
//!
//! The `set_voice_reference()` method accepts raw samples (not just
//! energy) so a future implementation can perform FFT-based spectral
//! subtraction. The public interface won't change — only the internal
//! processing gets smarter.

/// Removes or attenuates voice chat audio from stream playback.
///
/// Created per-peer by [`StreamAudioOutput`]. Each processing cycle:
/// 1. Voice output energy is read from a shared atomic
/// 2. `process()` applies the appropriate suppression to stream frames
///
/// The canceller is stateful — it tracks energy history and smooths
/// gain transitions to avoid clicks.
pub struct VoiceCanceller {
    /// Smoothed voice energy (from the voice output atomic).
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
    /// Gain smoothing coefficient per sample.
    /// Controls attack/release speed. ~5ms at 48kHz.
    smooth_coeff: f32,
}

impl VoiceCanceller {
    pub fn new() -> Self {
        Self {
            voice_energy: 0.0,
            current_gain: 1.0,
            target_gain: 1.0,
            silence_threshold: 0.005,              // ~-40 dBFS
            min_gain: 0.00,                        // -16 dB duck during speech
            smooth_coeff: 1.0 / (48000.0 * 0.005), // 5ms attack/release
        }
    }

    /// Update the voice energy reference.
    ///
    /// Called each processing cycle with the current voice output RMS
    /// level (0.0–1.0, from `AudioStats::output_level`).
    ///
    /// The energy is smoothed with asymmetric attack/release:
    /// - Fast attack (70% weight) — react quickly when someone starts talking
    /// - Slow release (5% weight) — hold the duck briefly after speech ends
    ///   to cover the stream's delayed echo tail
    pub fn update_voice_energy(&mut self, voice_rms: f32) {
        // Asymmetric smoothing: fast attack, slow release
        if voice_rms > self.voice_energy {
            // Fast attack — voice just started
            self.voice_energy = self.voice_energy * 0.3 + voice_rms * 0.7;
        } else {
            // Slow release — hold duck after speech ends.
            // 0.95 coefficient = ~100ms decay at 5ms tick rate.
            // This covers the typical 50-200ms stream audio delay.
            self.voice_energy = self.voice_energy * 0.995 + voice_rms * 0.005;
        }

        // Compute target gain from voice energy
        if self.voice_energy > self.silence_threshold {
            // Proportional ducking: louder voice = more attenuation.
            // ratio < 1.0 when voice is above threshold.
            let ratio = self.silence_threshold / self.voice_energy;
            self.target_gain = ratio.clamp(self.min_gain, 1.0);
        } else {
            self.target_gain = 1.0;
        }
    }

    /// Process stream audio in-place, applying voice suppression.
    ///
    /// Smooths the gain per-sample to prevent clicks on transitions.
    ///
    /// Current: energy-based ducking (simple gain reduction).
    /// Future: spectral subtraction using FFT of voice reference.
    pub fn process(&mut self, stream_samples: &mut [f32]) {
        for sample in stream_samples.iter_mut() {
            // Per-sample gain smoothing to avoid clicks
            if self.current_gain < self.target_gain {
                self.current_gain = (self.current_gain + self.smooth_coeff).min(self.target_gain);
            } else {
                self.current_gain = (self.current_gain - self.smooth_coeff).max(self.target_gain);
            }

            *sample *= self.current_gain;
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
}
