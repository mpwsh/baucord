//! Peak safety stage — catches transients before they hit the encoder.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → AGC → compressor → gain → **clamp** → speaking → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: nothing
//! - **Writes**: nothing
//!
//! ## Why this exists
//!
//! Both the upstream AGC (RMS-based) and compressor (5 ms attack) react to
//! *sustained* signal level, so plosives and sibilants — which have short
//! peaks 5–10× above their RMS — pass through with the AGC gain applied but
//! without being reduced by the compressor's envelope. Without this stage
//! they'd be hard-clipped at the encoder input, producing odd-order
//! harmonics that sound like a raspy "s" or a buzzy "t".
//!
//! ## What this does
//!
//! Two-step per frame:
//!
//! 1. Scan the 480 samples once for the peak magnitude and the first
//!    over-ceiling sample. If the frame needs ducking, glide the gain down
//!    across the samples *before* that transient so it arrives at exactly
//!    the value needed to bring the peak to the ceiling — 10 ms of
//!    effective lookahead at zero latency cost, and no gain step at the
//!    frame boundary.
//! 2. Walk the samples applying that gain, with a fast per-sample release
//!    (~50 ms) toward unity. The release runs during the quieter portion
//!    of the frame so the vowel after a consonant is not ducked longer
//!    than the consonant needs.
//!
//! A hard clip at the ceiling stays at the end of the loop as a safety
//! belt for pathological input (NaN/inf) or misconfigured ceilings. In
//! normal use it never engages — the pre-arm has already brought the peak
//! sample to the ceiling, not above it.
//!
//! No dB math, no soft knee, no envelope with separate coefficients: this
//! is the final safety stage, not the artistic dynamics processor.

use super::{AudioProcessor, FrameContext};

/// Peak-catching safety stage. Snaps gain to the level needed to bring the
/// current frame's peak to `ceiling`, then releases gently toward unity
/// over ~50 ms so the following vowel is not held down.
pub struct ClampStage {
    /// Absolute value above which we start ducking.
    ceiling: f32,
    /// Current gain being applied (starts at 1.0, goes down when peaks appear).
    gain: f32,
    /// How aggressively gain drops when a peak appears (per-sample coefficient).
    /// 0.15 → reaches ~90% of target reduction in about 15 samples (~0.3 ms).
    attack_coeff: f32,
    /// How gently gain recovers after a peak (per-sample coefficient).
    /// 0.0004 → ~50 ms release at 48 kHz.
    release_coeff: f32,
}

impl Default for ClampStage {
    fn default() -> Self {
        Self::new()
    }
}

impl ClampStage {
    /// Create a peak stage with the default ceiling (0.95).
    #[must_use]
    pub fn new() -> Self {
        Self::with_ceiling(0.95)
    }

    /// Create a peak stage with a custom ceiling in the range [0.1, 1.0].
    #[must_use]
    pub fn with_ceiling(ceiling: f32) -> Self {
        Self {
            ceiling: ceiling.clamp(0.1, 1.0),
            gain: 1.0,
            attack_coeff: 0.15,
            release_coeff: 0.000_4,
        }
    }

    /// Current gain being applied (for diagnostics). 1.0 = no reduction.
    #[must_use]
    pub fn current_gain(&self) -> f32 {
        self.gain
    }
}

impl AudioProcessor for ClampStage {
    fn process(&mut self, samples: &mut [f32], _ctx: &mut FrameContext) {
        let ceiling = self.ceiling;

        // Scan the frame once for the peak. We already have all 480 samples
        // in memory — this costs nothing in latency and lets the smoother
        // start ducking before the transient sample arrives.
        let mut frame_peak = 0.0f32;
        let mut first_over: Option<usize> = None;
        for (i, &s) in samples.iter().enumerate() {
            let abs = s.abs();
            if abs > frame_peak {
                frame_peak = abs;
            }
            if first_over.is_none() && abs > ceiling {
                first_over = Some(i);
            }
        }

        // Target gain for this frame: enough to bring the peak to the ceiling.
        // If the frame peak is already under the ceiling, target is 1.0 and
        // the release smoother handles the recovery naturally.
        let frame_target = if frame_peak > ceiling {
            ceiling / frame_peak
        } else {
            1.0
        };

        // Pre-peak glide: if we need to duck this frame, ramp the gain down
        // across the samples *before* the first over-ceiling sample, so the
        // gain has already reached the target when the transient arrives.
        // This keeps the "10 ms of free lookahead" property of the old
        // frame-scoped pre-arm, but without its flaw: the pre-arm snapped
        // the gain instantly at the frame boundary, which multiplied the
        // first samples of the frame by a step function — a small broadband
        // click stamped onto every transient, at exactly the moments the
        // listener is paying attention.
        //
        // If the peak IS the very first sample there is nothing to ramp
        // across; we snap for that sample only (the alternative is clipping
        // it), which is the same behavior as before but now confined to the
        // one case where it's unavoidable.
        let mut ramp_len = 0usize;
        let ramp_start = self.gain;
        if frame_target < self.gain {
            ramp_len = first_over.unwrap_or(0);
            if ramp_len == 0 {
                self.gain = frame_target;
            }
        }

        for (i, s) in samples.iter_mut().enumerate() {
            if i < ramp_len {
                // Linear glide toward the frame target; reaches it exactly
                // at the sample before the transient.
                #[allow(clippy::cast_precision_loss)]
                let t = (i + 1) as f32 / ramp_len as f32;
                self.gain = ramp_start + (frame_target - ramp_start) * t;
            } else {
                let abs = s.abs();

                let desired = if abs > ceiling { ceiling / abs } else { 1.0 };

                let coeff = if desired < self.gain {
                    self.attack_coeff
                } else {
                    self.release_coeff
                };
                self.gain += coeff * (desired - self.gain);
            }

            *s *= self.gain;

            // Final safety net. With the frame-scoped pre-arm above, this
            // should engage only on pathological input (NaN/inf) or when the
            // ceiling has been misconfigured. If it fires under normal use,
            // upstream gain staging is the real bug.
            if *s > ceiling {
                *s = ceiling;
            } else if *s < -ceiling {
                *s = -ceiling;
            }
        }
    }

    fn reset(&mut self) {
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_peak(samples: &[f32]) -> f32 {
        samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
    }

    /// Steady-state signal well under the ceiling passes through untouched.
    #[test]
    fn quiet_signal_passes_through() {
        let mut stage = ClampStage::new();
        let mut ctx = FrameContext::default();

        let input: Vec<f32> = (0..480).map(|i| 0.3 * (i as f32 * 0.1).sin()).collect();
        let mut output = input.clone();
        stage.process(&mut output, &mut ctx);

        for (i, (&a, &b)) in input.iter().zip(output.iter()).enumerate() {
            assert!(
                (a - b).abs() < 0.001,
                "quiet signal changed at {i}: {a} -> {b}"
            );
        }
    }

    /// A brief high-amplitude transient does not exceed the ceiling in the
    /// steady-state part of the frame that follows it.
    #[test]
    fn transient_is_caught() {
        let mut stage = ClampStage::new();
        let mut ctx = FrameContext::default();

        // 5-sample spike at 1.5 amplitude, then steady vowel-like signal.
        let mut frame = vec![0.3f32; 480];
        for f in frame.iter_mut().take(5) {
            *f = 1.5;
        }

        stage.process(&mut frame, &mut ctx);

        // Late samples should be at or below ceiling.
        let tail_peak = frame_peak(&frame[100..]);
        assert!(
            tail_peak <= 0.96,
            "tail peak {tail_peak} exceeded ceiling after transient"
        );
    }

    /// Gain recovers after a transient so the following vowel is not ducked
    /// forever. This is the key difference from a sustained-signal limiter.
    #[test]
    fn gain_recovers_after_transient() {
        let mut stage = ClampStage::new();
        let mut ctx = FrameContext::default();

        // Hit with a transient.
        let mut spike = vec![1.5f32; 20];
        stage.process(&mut spike, &mut ctx);
        let gain_after_spike = stage.current_gain();
        assert!(
            gain_after_spike < 0.9,
            "gain should have dropped after transient, was {gain_after_spike}"
        );

        // Then feed 300 ms of quiet audio (30 frames) — several release
        // time constants, so gain should be effectively back to 1.0.
        for _ in 0..30 {
            let mut quiet = vec![0.05f32; 480];
            stage.process(&mut quiet, &mut ctx);
        }

        // Gain should be back near 1.0.
        assert!(
            stage.current_gain() > 0.99,
            "gain did not recover, still at {}",
            stage.current_gain()
        );
    }

    /// The hard-safety belt is engaged only when the smoothed gain has not
    /// caught up yet; nothing ever exits above the ceiling.
    #[test]
    fn output_never_exceeds_ceiling() {
        let mut stage = ClampStage::with_ceiling(0.9);
        let mut ctx = FrameContext::default();

        // A frame of pure loud noise.
        let mut frame: Vec<f32> = (0..480)
            .map(|i| if i % 2 == 0 { 2.0 } else { -2.0 })
            .collect();
        stage.process(&mut frame, &mut ctx);

        for (i, &s) in frame.iter().enumerate() {
            assert!(s.abs() <= 0.9 + 1e-6, "sample {i} = {s} exceeded ceiling");
        }
    }

    /// Reset clears the smoothing state.
    #[test]
    fn reset_restores_unity_gain() {
        let mut stage = ClampStage::new();
        let mut ctx = FrameContext::default();
        let mut spike = vec![1.5f32; 100];
        stage.process(&mut spike, &mut ctx);
        assert!(stage.current_gain() < 1.0);
        stage.reset();
        // `reset` assigns the literal 1.0; nothing is computed.
        #[expect(clippy::float_cmp)]
        {
            assert_eq!(stage.current_gain(), 1.0);
        }
    }
}
