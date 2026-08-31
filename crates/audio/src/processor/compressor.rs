//! Dynamic range compressor for voice chat.
//!
//! Reduces the dynamic range of audio by attenuating signals that exceed
//! a threshold. Makes voices sound fuller and more consistent, and
//! prevents harsh peaks when multiple people speak simultaneously.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → AGC → **compressor** → gain → clamp → speaking → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.gate_open` (hold gain when closed),
//!   `ctx.compressor_enabled` (bypass when `false`)
//! - **Writes**: nothing (downstream processors don't need compressor state)
//!
//! ## Design
//!
//! Based on the Giannoulis/Massberg/Reiss paper on digital dynamic range
//! compressor design (JAES 2012). Peak envelope detector with asymmetric
//! attack/release, soft knee, gate-aware hold, per-frame gain ramp.
//!
//! The envelope is tracked per sample (two multiplies), but the gain
//! computer runs once per frame on the frame's peak envelope: gain
//! reduction is monotonic in envelope level, so the minimum gain over the
//! frame is exactly the gain at the maximum envelope. The old version ran
//! `log10` + `powf` on every sample (~96k transcendental calls per second)
//! only to take that same minimum.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use super::{AudioProcessor, FrameContext};

/// Dynamic range compressor for real-time voice processing.
///
/// All parameters are adjustable at runtime via atomic handles.
pub struct Compressor {
    // ── Envelope detector coefficients ───────────────────────────────
    attack_coeff: f32,
    release_coeff: f32,

    // ── Runtime state ────────────────────────────────────────────────
    envelope: f32,
    current_gain: f32,

    knobs: Arc<Knobs>,
}

impl Compressor {
    pub fn new(knobs: Arc<Knobs>, attack_ms: f32, release_ms: f32) -> Self {
        let sample_rate = 48_000.0_f32;
        let attack_coeff = (-1.0_f32 / (attack_ms.clamp(0.1, 50.0) * 0.001 * sample_rate)).exp();
        let release_coeff =
            (-1.0_f32 / (release_ms.clamp(10.0, 1000.0) * 0.001 * sample_rate)).exp();

        Self {
            knobs,
            attack_coeff,
            release_coeff,
            envelope: 0.0,
            current_gain: 1.0,
        }
    }

    /// Create with shared knobs and default voice-chat timing (5ms/150ms).
    pub fn with_defaults(knobs: Arc<Knobs>) -> Self {
        Self::new(knobs, 5.0, 150.0)
    }

    /// Current gain being applied (for diagnostics). 1.0 = no reduction.
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Inner processing shared by the trait impl and direct callers.
    #[expect(
        clippy::cast_precision_loss,
        reason = "frame length ≤ 960 samples, well within f32 precision"
    )]
    fn process_inner(&mut self, samples: &mut [f32], gate_open: bool, enabled: bool) -> f32 {
        if !enabled {
            return 1.0;
        }

        // Gate closed / holding: freeze the gain (don't adapt to the noise
        // floor) but keep applying it so there is no level step.
        if !gate_open {
            let g = self.current_gain;
            if (g - 1.0).abs() > f32::EPSILON {
                for s in samples.iter_mut() {
                    *s *= g;
                }
            }
            return g;
        }

        let threshold_db = f32::from_bits(self.knobs.threshold_db.load(Ordering::Relaxed));
        let ratio = f32::from_bits(self.knobs.ratio.load(Ordering::Relaxed));
        let knee_db = f32::from_bits(self.knobs.knee_db.load(Ordering::Relaxed));
        let makeup_db = f32::from_bits(self.knobs.makeup_db.load(Ordering::Relaxed));
        let makeup_linear = db_to_linear(makeup_db);

        // ── Envelope detector (per sample), peak over the frame ─────
        let mut peak_env = 0.0f32;
        for &s in samples.iter() {
            let abs = s.abs();
            let coeff = if abs > self.envelope {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.envelope = coeff * self.envelope + (1.0 - coeff) * abs;
            peak_env = peak_env.max(self.envelope);
        }

        // ── Gain computer (once per frame) ──────────────────────────
        let min_gain = if peak_env < 1e-6 {
            1.0
        } else {
            let gain_reduction_db =
                compute_gain_reduction(linear_to_db(peak_env), threshold_db, ratio, knee_db);
            if gain_reduction_db < -0.01 {
                db_to_linear(gain_reduction_db)
            } else {
                1.0
            }
        };

        // ── Apply: linear ramp from last frame's gain to this one ───
        let frame_target_gain = min_gain * makeup_linear;
        let gain_start = self.current_gain;
        let gain_delta = frame_target_gain - gain_start;
        let inv_len = 1.0 / samples.len() as f32;
        for (i, s) in samples.iter_mut().enumerate() {
            let t = i as f32 * inv_len;
            *s *= gain_start + gain_delta * t;
        }
        self.current_gain = frame_target_gain;
        frame_target_gain
    }
}

impl AudioProcessor for Compressor {
    fn name(&self) -> &str {
        "compressor"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        // See AGC: the hold window carries only noise floor, so hold
        // gain rather than releasing upward through the noise tail.
        self.process_inner(
            samples,
            ctx.gate_open && !ctx.gate_holding,
            ctx.compressor_enabled,
        );
    }

    fn reset(&mut self) {
        self.envelope = 0.0;
        self.current_gain = 1.0;
    }
}

// ── Helper functions ────────────────────────────────────────────────────

/// Compute gain reduction in dB using the soft-knee compressor curve.
///
/// Based on the Giannoulis/Massberg/Reiss gain computer.
fn compute_gain_reduction(input_db: f32, threshold_db: f32, ratio: f32, knee_db: f32) -> f32 {
    let half_knee = knee_db * 0.5;
    let delta = input_db - threshold_db;

    if knee_db < 0.01 {
        if delta <= 0.0 {
            0.0
        } else {
            delta * (1.0 / ratio - 1.0)
        }
    } else if delta < -half_knee {
        0.0
    } else if delta > half_knee {
        delta * (1.0 / ratio - 1.0)
    } else {
        let x = delta + half_knee;
        (1.0 / ratio - 1.0) * x * x / (2.0 * knee_db)
    }
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn linear_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        -96.0
    } else {
        20.0 * linear.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_frame(amplitude: f32, freq_hz: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| amplitude * (2.0 * std::f32::consts::PI * freq_hz * i as f32 / 48000.0).sin())
            .collect()
    }

    fn frame_rms(samples: &[f32]) -> f32 {
        crate::processor::compute_rms(samples)
    }

    #[test]
    fn loud_signal_is_compressed() {
        let knobs = Arc::new(Knobs {
            threshold_db: AtomicU32::new((-20.0_f32).to_bits()),
            ratio: AtomicU32::new(4.0_f32.to_bits()),
            knee_db: AtomicU32::new(0.0_f32.to_bits()),
            makeup_db: AtomicU32::new(0.0_f32.to_bits()),
        });
        let mut comp = Compressor::new(knobs, 1.0, 50.0);
        let frame = sine_frame(0.5, 300.0, 480);
        let input_rms = frame_rms(&frame);

        for _ in 0..200 {
            let mut f = frame.clone();
            comp.process_inner(&mut f, true, true);
        }

        let mut output = frame.clone();
        comp.process_inner(&mut output, true, true);
        let output_rms = frame_rms(&output);

        assert!(
            output_rms < input_rms * 0.8,
            "Loud signal should be compressed: in={input_rms:.4} out={output_rms:.4}"
        );
    }

    #[test]
    fn quiet_signal_passes_through() {
        let knobs = Arc::new(Knobs {
            threshold_db: AtomicU32::new((-10.0_f32).to_bits()),
            ratio: AtomicU32::new(4.0_f32.to_bits()),
            knee_db: AtomicU32::new(0.0_f32.to_bits()),
            makeup_db: AtomicU32::new(0.0_f32.to_bits()),
        });
        let mut comp = Compressor::new(knobs, 1.0, 50.0);
        let frame = sine_frame(0.01, 300.0, 480);
        let input_rms = frame_rms(&frame);

        for _ in 0..100 {
            let mut f = frame.clone();
            comp.process_inner(&mut f, true, true);
        }

        let mut output = frame.clone();
        comp.process_inner(&mut output, true, true);
        let ratio = frame_rms(&output) / input_rms;

        assert!(
            ratio > 0.95 && ratio < 1.05,
            "Quiet signal should pass unchanged: ratio={ratio:.4}"
        );
    }

    #[test]
    fn holds_gain_when_gate_closed() {
        let knobs = Arc::new(Knobs {
            threshold_db: AtomicU32::new((-20.0_f32).to_bits()),
            ratio: AtomicU32::new(4.0_f32.to_bits()),
            knee_db: AtomicU32::new(0.0_f32.to_bits()),
            makeup_db: AtomicU32::new(0.0_f32.to_bits()),
        });
        let mut comp = Compressor::new(knobs, 1.0, 50.0);

        let loud = sine_frame(0.5, 300.0, 480);

        for _ in 0..200 {
            let mut f = loud.clone();
            comp.process_inner(&mut f, true, true);
        }
        let gain_before = comp.current_gain();

        let mut silence = vec![0.0f32; 480];
        for _ in 0..100 {
            comp.process_inner(&mut silence, false, true);
        }

        assert!(
            (gain_before - comp.current_gain()).abs() < 0.001,
            "Gain should hold: before={gain_before:.4} after={:.4}",
            comp.current_gain()
        );
    }

    #[test]
    fn bypass_passes_through() {
        let knobs = Arc::new(Knobs {
            threshold_db: AtomicU32::new((-20.0_f32).to_bits()),
            ratio: AtomicU32::new(4.0_f32.to_bits()),
            knee_db: AtomicU32::new(0.0_f32.to_bits()),
            makeup_db: AtomicU32::new(0.0_f32.to_bits()),
        });
        let mut comp = Compressor::new(knobs, 0.0, 0.0);
        let frame = sine_frame(0.8, 300.0, 480);
        let mut output = frame.clone();
        let gain = comp.process_inner(&mut output, true, false);

        assert!((gain - 1.0).abs() < 0.001);
        for (a, b) in frame.iter().zip(output.iter()) {
            assert!((a - b).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn soft_knee_is_monotonic() {
        let mut prev_gr = 0.0_f32;
        for i in 0..100 {
            let input_db = -30.0 + (i as f32 * 0.2);
            let gr = compute_gain_reduction(input_db, -20.0, 4.0, 6.0);
            assert!(
                gr <= prev_gr + 0.001,
                "Monotonic at {input_db:.1}dB: prev={prev_gr:.4} curr={gr:.4}"
            );
            prev_gr = gr;
        }
    }
}

/// Tuneable parameters for the compressor.
///
/// Lives inside [`InputConfig`](crate::input_config::InputConfig).
/// All fields are atomic f32 bits for lock-free runtime adjustment.
pub struct Knobs {
    /// Threshold in dBFS (-60..0). Default: -18.0.
    pub threshold_db: AtomicU32,

    /// Compression ratio (1.0..20.0). Default: 4.0.
    pub ratio: AtomicU32,

    /// Soft knee width in dB (0.0..12.0). Default: 6.0.
    pub knee_db: AtomicU32,

    /// Make-up gain in dB (0.0..24.0). Default: 0.0.
    pub makeup_db: AtomicU32,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            threshold_db: AtomicU32::new((-18.0_f32).to_bits()),
            ratio: AtomicU32::new(4.0_f32.to_bits()),
            knee_db: AtomicU32::new(6.0_f32.to_bits()),
            makeup_db: AtomicU32::new(0.0_f32.to_bits()),
        }
    }
}
