//! Noise suppression via nnnoiseless (Mozilla RNNoise, pure Rust).
//!
//! Operates on 480-sample frames (10 ms @ 48 kHz) — exactly our Opus frame size.
//! Sits in the input pipeline: capture → resample → **denoise** → gate → VAD → encode.

use nnnoiseless::DenoiseState;

/// Real-time noise suppressor wrapping nnnoiseless.
///
/// The denoiser expects f32 samples scaled to i16 range (−32768..32767),
/// and returns cleaned samples in the same range. We handle the conversion
/// from/to normalized f32 (−1.0..1.0) internally.
///
/// The denoiser is pre-warmed during construction by feeding a silent frame,
/// so the first real frame of audio is processed correctly without artifacts.
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    output_buf: [f32; DenoiseState::FRAME_SIZE],
}

impl Denoiser {
    pub fn new() -> Self {
        let mut state = DenoiseState::new();

        // Pre-warm: feed a silent frame so the RNN state is primed.
        // Without this, the first real frame has fade-in artifacts
        // that clip the onset of speech (bad for push-to-talk).
        let silent_input = [0.0f32; DenoiseState::FRAME_SIZE];
        let mut warmup_buf = [0.0f32; DenoiseState::FRAME_SIZE];
        state.process_frame(&mut warmup_buf, &silent_input);

        Self {
            state,
            output_buf: [0.0; DenoiseState::FRAME_SIZE],
        }
    }

    /// Process one frame of 480 normalized f32 samples (−1.0..1.0) in-place.
    ///
    /// Returns the VAD probability from the RNN (0.0–1.0), which can be used
    /// as a pre-filter for the earshot VAD to skip expensive processing on
    /// frames that are clearly not speech.
    pub fn process_frame(&mut self, samples: &mut [f32]) -> f32 {
        debug_assert_eq!(samples.len(), DenoiseState::FRAME_SIZE);

        // nnnoiseless expects i16-scaled floats
        let input: Vec<f32> = samples.iter().map(|&s| s * 32767.0).collect();

        let vad_prob = self.state.process_frame(&mut self.output_buf, &input);

        // Convert back to normalized f32
        for (out, &denoised) in samples.iter_mut().zip(self.output_buf.iter()) {
            *out = (denoised / 32767.0).clamp(-1.0, 1.0);
        }

        vad_prob
    }

    /// Reset state (e.g., after device switch).
    /// Re-warms with a silent frame so the next real frame is clean.
    pub fn reset(&mut self) {
        self.state = DenoiseState::new();
        self.output_buf = [0.0; DenoiseState::FRAME_SIZE];

        // Re-warm after reset
        let silent_input = [0.0f32; DenoiseState::FRAME_SIZE];
        let mut warmup_buf = [0.0f32; DenoiseState::FRAME_SIZE];
        self.state.process_frame(&mut warmup_buf, &silent_input);
    }
}