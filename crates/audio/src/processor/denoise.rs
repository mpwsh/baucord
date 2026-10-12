//! Noise suppression via nnnoiseless (Mozilla `RNNoise`, pure Rust).
//!
//! `RNNoise`'s frame is fixed at 480 samples (10 ms @ 48 kHz); the pipeline
//! frame is 960, so each call runs the network twice. The VAD probability
//! reported is the larger of the two halves (speech anywhere in the frame
//! counts).
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → **denoise** → gate → AGC → compressor → gain → clamp → speaking → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.denoise_enabled` (bypass when `false`)
//! - **Writes**: `ctx.rnnoise_vad_prob` (0.0–1.0, used by the speaking indicator)

use nnnoiseless::DenoiseState;

use super::{AudioProcessor, FrameContext};

/// Real-time noise suppressor wrapping nnnoiseless.
///
/// The denoiser expects f32 samples scaled to i16 range (−32768..32767),
/// and returns cleaned samples in the same range. The conversion from/to
/// normalised f32 (−1.0..1.0) is done in fixed scratch — nothing is
/// allocated per frame.
///
/// Pre-warmed during construction by feeding a silent frame, so the
/// first real frame is processed correctly without onset artifacts.
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    input_buf: [f32; DenoiseState::FRAME_SIZE],
    output_buf: [f32; DenoiseState::FRAME_SIZE],
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    #[must_use]
    pub fn new() -> Self {
        let mut state = DenoiseState::new();
        Self::warm(&mut state);
        Self {
            state,
            input_buf: [0.0; DenoiseState::FRAME_SIZE],
            output_buf: [0.0; DenoiseState::FRAME_SIZE],
        }
    }

    /// Prime the RNN state so the first real frame doesn't have fade-in
    /// artifacts (bad for push-to-talk).
    fn warm(state: &mut DenoiseState<'static>) {
        let silent = [0.0f32; DenoiseState::FRAME_SIZE];
        let mut warmup = [0.0f32; DenoiseState::FRAME_SIZE];
        state.process_frame(&mut warmup, &silent);
    }

    /// Denoise one 480-sample block in place; returns its VAD probability.
    fn process_block(&mut self, block: &mut [f32]) -> f32 {
        debug_assert_eq!(block.len(), DenoiseState::FRAME_SIZE);
        for (dst, &s) in self.input_buf.iter_mut().zip(block.iter()) {
            *dst = s * 32767.0;
        }
        let vad_prob = self
            .state
            .process_frame(&mut self.output_buf, &self.input_buf);
        for (out, &denoised) in block.iter_mut().zip(self.output_buf.iter()) {
            *out = (denoised / 32767.0).clamp(-1.0, 1.0);
        }
        vad_prob
    }
}

impl AudioProcessor for Denoiser {
    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        if !ctx.denoise_enabled {
            // Assume speech when denoise is off — no RNNoise VAD available;
            // the gate alone decides downstream.
            ctx.rnnoise_vad_prob = 1.0;
            return;
        }
        let mut vad = 0.0f32;
        for block in samples.chunks_exact_mut(DenoiseState::FRAME_SIZE) {
            vad = vad.max(self.process_block(block));
        }
        ctx.rnnoise_vad_prob = vad;
    }

    fn reset(&mut self) {
        self.state = DenoiseState::new();
        Self::warm(&mut self.state);
        self.output_buf = [0.0; DenoiseState::FRAME_SIZE];
    }
}
