//! Automatic Gain Control (AGC) for microphone input.
//!
//! Dynamically adjusts gain so quiet mics sound loud and loud mics
//! don't clip — the single biggest factor in perceived audio quality
//! for voice chat.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → **AGC** → compressor → gain → clamp → speaking → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.gate_open` (hold gain when closed),
//!   `ctx.agc_enabled` (bypass when `false`)
//! - **Writes**: nothing (its gain is internal state; `current_gain()` exposes it)
//!
//! ## How it works
//!
//! Measures frame RMS and computes a gain multiplier toward a target
//! loudness. Asymmetric attack (fast, catches shouts) / release (slow,
//! prevents pumping). Gate-aware: holds gain during silence so the next
//! speech onset isn't blasted at max gain.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use super::{compute_rms, AudioProcessor, FrameContext};

/// Tuneable parameters for automatic gain control.
///
/// Lives inside [`InputConfig`](crate::input_config::InputConfig).
pub struct Knobs {
    /// Target RMS level (f32 bits, 0.01–0.5). Default: 0.10.
    pub target_rms: AtomicU32,
}

impl Knobs {
    pub fn with_target(target_rms: f32) -> Self {
        Self {
            target_rms: AtomicU32::new(target_rms.clamp(0.01, 0.5).to_bits()),
        }
    }
}

impl Default for Knobs {
    fn default() -> Self {
        Self::with_target(0.10)
    }
}

/// Real-time automatic gain control processor.
///
/// All parameters are adjustable at runtime via atomic handles.
pub struct AutoGainControl {
    // ── Target & limits ─────────────────────────────────────────────
    max_gain: f32,
    min_gain: f32,

    // ── Smoothing coefficients ──────────────────────────────────────
    attack_coeff: f32,
    release_coeff: f32,

    // ── Runtime state ───────────────────────────────────────────────
    current_gain: f32,
    knobs: Arc<Knobs>,
}

impl AutoGainControl {
    /// Create an AGC that reads its target from shared knobs.
    ///
    /// - `knobs`: shared parameter panel.
    /// - `max_gain_db`: maximum boost in dB (0–60). 30 dB ≈ 31.6×.
    pub fn new(knobs: Arc<Knobs>, max_gain_db: f32) -> Self {
        let max_gain = 10.0_f32.powf(max_gain_db.clamp(0.0, 60.0) / 20.0);
        Self {
            knobs,
            max_gain,
            min_gain: 0.5,
            // Per-frame smoothing at a 20 ms frame cadence. These are the
            // 10 ms-frame values (0.1 / 0.005) compounded twice, so the
            // wall-clock behaviour is unchanged: gain drops quickly on
            // loud input, rises over ~2 s on quiet input.
            attack_coeff: 0.19,
            release_coeff: 0.01,
            current_gain: 1.0,
        }
    }

    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Inner processing: adjusts gain toward target RMS.
    #[expect(
        clippy::cast_precision_loss,
        reason = "frame length ≤ 960 samples, well within f32 precision"
    )]
    fn process_inner(&mut self, samples: &mut [f32], gate_open: bool) -> f32 {
        // Gate closed → hold gain, don't adapt to silence — but still
        // APPLY the held gain, otherwise the gate's Closing fade tail
        // (which reaches this stage un-gained) steps down/up in level
        // relative to the boosted speech right before it.
        if !gate_open {
            let g = self.current_gain;
            if (g - 1.0).abs() > f32::EPSILON {
                for s in samples.iter_mut() {
                    *s *= g;
                }
            }
            return g;
        }

        let target = f32::from_bits(self.knobs.target_rms.load(Ordering::Relaxed));
        let rms = compute_rms(samples);

        // Near-silence that snuck past the gate (e.g., hold period):
        // hold the gain, apply it, don't adapt.
        if rms < 0.001 {
            let g = self.current_gain;
            if (g - 1.0).abs() > f32::EPSILON {
                for s in samples.iter_mut() {
                    *s *= g;
                }
            }
            return g;
        }

        let desired_gain = (target / rms).clamp(self.min_gain, self.max_gain);

        // Asymmetric smoothing: fast attack, slow release.
        let coeff = if desired_gain < self.current_gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };

        let target_gain = self.current_gain + coeff * (desired_gain - self.current_gain);

        // Per-sample interpolation to avoid zipper noise.
        let gain_start = self.current_gain;
        let gain_delta = target_gain - gain_start;
        let inv_len = 1.0 / samples.len() as f32;

        for (i, s) in samples.iter_mut().enumerate() {
            let t = i as f32 * inv_len;
            let gain = gain_start + gain_delta * t;
            *s *= gain;
        }

        self.current_gain = target_gain;
        target_gain
    }
}

impl AudioProcessor for AutoGainControl {
    fn name(&self) -> &str {
        "agc"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        if !ctx.agc_enabled {
            return;
        }
        // Treat the gate's hold window like a closed gate: only the
        // residual noise floor is passing, and adapting to it ramps
        // gain upward, amplifying the post-speech noise tail. The
        // `!gate_open` path holds the current gain and still applies
        // it, which is exactly the right behavior here too.
        self.process_inner(samples, ctx.gate_open && !ctx.gate_holding);
    }

    fn reset(&mut self) {
        self.current_gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_holds_steady_during_gate_hold_window() {
        let knobs = Arc::new(Knobs::default());
        let mut agc = AutoGainControl::new(knobs, 30.0);
        let mut ctx = crate::processor::FrameContext::default();
        // Converge on quiet speech (gain rises toward target).
        for _ in 0..200 {
            let mut f = sine_frame(0.03, 300.0);
            agc.process(&mut f, &mut ctx);
        }
        let converged = agc.current_gain();
        assert!(converged > 1.5, "AGC should have boosted, got {converged}");
        // Hold window: noise floor above the near-silence guard.
        ctx.gate_holding = true;
        for _ in 0..100 {
            let mut f = sine_frame(0.005, 300.0);
            agc.process(&mut f, &mut ctx);
        }
        let held = agc.current_gain();
        assert!(
            (held - converged).abs() < f32::EPSILON,
            "gain moved during hold: {converged} -> {held}"
        );
    }

    fn sine_frame(amplitude: f32, freq_hz: f32) -> Vec<f32> {
        (0..480)
            .map(|i| amplitude * (2.0 * std::f32::consts::PI * freq_hz * i as f32 / 48000.0).sin())
            .collect()
    }

    #[test]
    fn quiet_signal_is_boosted_toward_target_after_convergence() {
        let knobs = Arc::new(Knobs {
            target_rms: AtomicU32::new((0.10_f32).to_bits()),
        });
        let mut agc = AutoGainControl::new(knobs, 30.0);
        let template = sine_frame(0.01, 440.0);

        let mut scratch = vec![0.0f32; 480];
        for _ in 0..500 {
            scratch.copy_from_slice(&template);
            agc.process_inner(&mut scratch, true);
        }

        let mut output = template;
        agc.process_inner(&mut output, true);
        let output_rms = compute_rms(&output);

        assert!(
            output_rms > 0.05,
            "AGC should boost quiet signal, got RMS {output_rms:.4}"
        );
    }

    #[test]
    fn gain_holds_steady_when_gate_is_closed() {
        let knobs = Arc::new(Knobs {
            target_rms: AtomicU32::new((0.10_f32).to_bits()),
        });
        let mut agc = AutoGainControl::new(knobs, 30.0);
        let template = sine_frame(0.05, 300.0);

        let mut scratch = vec![0.0f32; 480];
        for _ in 0..100 {
            scratch.copy_from_slice(&template);
            agc.process_inner(&mut scratch, true);
        }
        let gain_before = agc.current_gain();

        let mut silence = vec![0.0f32; 480];
        for _ in 0..100 {
            agc.process_inner(&mut silence, false);
        }

        assert!(
            (gain_before - agc.current_gain()).abs() < 0.001,
            "Gain should hold: before={gain_before:.3} after={:.3}",
            agc.current_gain()
        );
    }

    #[test]
    fn gain_does_not_exceed_max() {
        let knobs = Arc::new(Knobs {
            target_rms: AtomicU32::new((0.10_f32).to_bits()),
        });
        let mut agc = AutoGainControl::new(knobs, 20.0);

        let frame = [0.001f32; 480];
        let mut scratch = vec![0.0f32; 480];
        for _ in 0..1000 {
            scratch.copy_from_slice(&frame);
            agc.process_inner(&mut scratch, true);
        }

        assert!(
            agc.current_gain() <= 10.1,
            "Gain should not exceed max: {:.1}",
            agc.current_gain()
        );
    }

    #[test]
    fn attack_responds_faster_than_release() {
        let knobs = Arc::new(Knobs {
            target_rms: AtomicU32::new((0.10_f32).to_bits()),
        });
        let mut agc = AutoGainControl::new(knobs, 30.0);
        let quiet = sine_frame(0.02, 300.0);
        let loud = sine_frame(0.5, 300.0);

        let mut scratch = vec![0.0f32; 480];
        for _ in 0..200 {
            scratch.copy_from_slice(&quiet);
            agc.process_inner(&mut scratch, true);
        }
        let gain_at_quiet = agc.current_gain();

        scratch.copy_from_slice(&loud);
        agc.process_inner(&mut scratch, true);
        let attack_delta = (gain_at_quiet - agc.current_gain()).abs();

        scratch.copy_from_slice(&quiet);
        agc.process_inner(&mut scratch, true);
        // Compare attack step vs one release step
        let gain_after_loud = agc.current_gain();
        scratch.copy_from_slice(&quiet);
        agc.process_inner(&mut scratch, true);
        let release_delta = (agc.current_gain() - gain_after_loud).abs();

        assert!(
            attack_delta > release_delta * 2.0,
            "Attack should be faster: attack={attack_delta:.4} release={release_delta:.4}"
        );
    }
}
