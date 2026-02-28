//! Noise suppression via nnnoiseless (Mozilla RNNoise, pure Rust).
//!
//! Operates on 480-sample frames (10 ms @ 48 kHz) — exactly our Opus frame size.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → **denoise** → gate → compressor → AGC → gain → clamp → VAD → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.denoise_enabled` (bypass when `false`)
//! - **Writes**: `ctx.rnnoise_vad_prob` (0.0–1.0, used by VAD pre-filter)

use nnnoiseless::DenoiseState;

use super::{AudioProcessor, FrameContext};

/// Real-time noise suppressor wrapping nnnoiseless.
///
/// The denoiser expects f32 samples scaled to i16 range (−32768..32767),
/// and returns cleaned samples in the same range. We handle the conversion
/// from/to normalised f32 (−1.0..1.0) internally.
///
/// Pre-warmed during construction by feeding a silent frame, so the
/// first real frame is processed correctly without onset artifacts.
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    output_buf: [f32; DenoiseState::FRAME_SIZE],
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    pub fn new() -> Self {
        let mut state = DenoiseState::new();

        // Pre-warm: prime the RNN state so the first real frame
        // doesn't have fade-in artifacts (bad for push-to-talk).
        let silent = [0.0f32; DenoiseState::FRAME_SIZE];
        let mut warmup = [0.0f32; DenoiseState::FRAME_SIZE];
        state.process_frame(&mut warmup, &silent);

        Self {
            state,
            output_buf: [0.0; DenoiseState::FRAME_SIZE],
        }
    }

    /// Inner processing: denoise samples in-place, return VAD probability.
    fn process_inner(&mut self, samples: &mut [f32]) -> f32 {
        debug_assert_eq!(samples.len(), DenoiseState::FRAME_SIZE);

        // nnnoiseless expects i16-scaled floats
        let input: Vec<f32> = samples.iter().map(|&s| s * 32767.0).collect();

        let vad_prob = self.state.process_frame(&mut self.output_buf, &input);

        for (out, &denoised) in samples.iter_mut().zip(self.output_buf.iter()) {
            *out = (denoised / 32767.0).clamp(-1.0, 1.0);
        }

        vad_prob
    }
}

impl AudioProcessor for Denoiser {
    fn name(&self) -> &str {
        "denoise"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        if !ctx.denoise_enabled {
            // Assume speech when denoise is off — no RNNoise VAD available
            ctx.rnnoise_vad_prob = 1.0;
            return;
        }
        ctx.rnnoise_vad_prob = self.process_inner(samples);
    }

    fn reset(&mut self) {
        self.state = DenoiseState::new();
        self.output_buf = [0.0; DenoiseState::FRAME_SIZE];

        // Re-warm after reset
        let silent = [0.0f32; DenoiseState::FRAME_SIZE];
        let mut warmup = [0.0f32; DenoiseState::FRAME_SIZE];
        self.state.process_frame(&mut warmup, &silent);
    }
}
