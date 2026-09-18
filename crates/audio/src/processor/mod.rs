//! Audio processor trait and pipeline chain.
//!
//! Defines a common interface for audio processing stages that compose
//! into ordered chains. Each processor receives a mutable frame of f32
//! samples and transforms it in-place, reading and writing shared
//! metadata through [`FrameContext`].
//!
//! ## Design principles
//!
//! - **Zero-alloc hot path**: processors work on borrowed `&mut [f32]`
//!   slices and fixed-size scratch — no allocation per frame.
//! - **Frame metadata via context**: processors communicate state
//!   (e.g., "gate is open", "`RNNoise` thinks this is speech") through
//!   [`FrameContext`], not return values, so information flows downstream
//!   naturally.
//! - **Bypass via context**: processors that can be toggled at runtime
//!   check a field in [`FrameContext`] and early-return when disabled.
//!   The chain always has the same shape — good for debugging.
//! - **Stateful + Send**: processors own their state (envelopes,
//!   coefficients, buffers) and must be `Send` for the worker thread.
//!   They don't need `Sync` — only the worker calls `process`.
//!
//! ## Frame length
//!
//! One frame is [`crate::FRAME_SIZE`] samples (20 ms at 48 kHz). Stages
//! with per-frame smoothing coefficients are tuned for that cadence.

pub mod agc;
pub mod clamp;
pub mod compressor;
pub mod denoise;
pub mod gain;
pub mod gate;
pub mod level_meter;
pub mod limiter;
pub mod speaking;

// ── Shared utilities ────────────────────────────────────────────────────

/// Compute RMS (root mean square) of a sample buffer.
///
/// Used by gate, AGC, and level metering. Centralised here to avoid
/// duplicating the same three-line function across processors.
#[expect(
    clippy::cast_precision_loss,
    reason = "frame length ≤ 960 samples, well within f32 precision"
)]
#[must_use]
pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

// ── Frame context ───────────────────────────────────────────────────────

/// Metadata that flows through the processor chain alongside the audio.
///
/// Processors read and write fields here to coordinate behaviour.
/// The worker thread populates config fields (from atomics) once per
/// frame before calling the chain, and reads result fields after.
///
/// Stack-allocated, cheap to create per frame.
// The flags are independent per-frame switches read by different
// stages; a state enum would have to enumerate their product.
#[expect(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct FrameContext {
    // ── Pipeline config (set by worker before chain runs) ───────────
    /// Whether noise suppression is enabled this frame.
    pub denoise_enabled: bool,

    /// Whether the compressor is enabled this frame.
    pub compressor_enabled: bool,

    /// Whether AGC is enabled this frame.
    pub agc_enabled: bool,

    /// Input gain multiplier (read from user setting).
    pub input_gain: f32,

    // ── Inter-processor state (written/read during chain) ───────────
    /// Whether the noise gate is currently open (signal passing).
    /// Written by: `NoiseGate`. Read by: `AGC`, `Compressor`, `SpeakingIndicator`.
    pub gate_open: bool,

    /// Whether the gate is in its post-speech hold window (open, but
    /// the envelope has fallen below the close threshold). During the
    /// hold only the residual noise floor is passing, so gain stages
    /// must not adapt to it — adapting boosts the noise tail.
    /// Written by: `NoiseGate`. Read by: `AGC`, `Compressor`.
    pub gate_holding: bool,

    /// `RNNoise` VAD probability (0.0–1.0; 1.0 when denoise is bypassed).
    /// Written by: `Denoiser`. Read by: `SpeakingIndicator`.
    pub rnnoise_vad_prob: f32,

    /// Whether the frame counts as speech (with hold).
    /// Written by: `SpeakingIndicator`. Read by: the worker (stats).
    pub is_speech: bool,

    // ── Metering (written by LevelMeter instances) ──────────────────
    /// Post-denoise RMS level (0.0–1.0, scaled for UI).
    /// Used for gate threshold visualisation.
    pub denoised_level: f32,

    /// Post-pipeline RMS level (0.0–1.0, scaled for UI).
    /// What the encoder actually sees.
    pub pipeline_level: f32,
}

impl Default for FrameContext {
    fn default() -> Self {
        Self {
            denoise_enabled: true,
            compressor_enabled: true,
            agc_enabled: true,
            input_gain: 1.0,
            gate_open: true,
            gate_holding: false,
            rnnoise_vad_prob: 1.0,
            is_speech: false,
            denoised_level: 0.0,
            pipeline_level: 0.0,
        }
    }
}

// ── Processor trait ─────────────────────────────────────────────────────

/// Trait for audio processing stages.
///
/// Each processor transforms audio samples in-place and can read/write
/// the shared [`FrameContext`] to coordinate with other processors in
/// the chain.
///
/// Processors that support runtime bypass should check the relevant
/// `FrameContext` field (e.g., `ctx.denoise_enabled`) and early-return
/// inside `process` — the chain always runs every stage.
pub trait AudioProcessor: Send {
    /// Human-readable name for logging and debug.
    fn name(&self) -> &str;

    /// Process one frame of audio in-place.
    ///
    /// The frame is always at the pipeline's internal sample rate (48 kHz)
    /// and has been resampled from the device rate before reaching here.
    ///
    /// `ctx` carries metadata from upstream processors and should be
    /// updated with any state this processor produces.
    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext);

    /// Reset all internal state.
    ///
    /// Called on device switch, mute toggle, or other pipeline resets.
    /// After reset, the next `process` call should behave as if this
    /// is the first frame ever processed.
    fn reset(&mut self);
}

// ── Processor chain ─────────────────────────────────────────────────────

/// An ordered chain of audio processors, applied in insertion order.
pub struct ProcessorChain {
    name: String,
    processors: Vec<Box<dyn AudioProcessor>>,
}

impl ProcessorChain {
    /// Create an empty chain with a name (for logging).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            processors: Vec::new(),
        }
    }

    /// Add a processor to the end of the chain.
    pub fn add(&mut self, processor: Box<dyn AudioProcessor>) -> &mut Self {
        log::debug!("Chain '{}': added '{}'", self.name, processor.name());
        self.processors.push(processor);
        self
    }

    /// Process a frame through all processors in order.
    pub fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        for processor in &mut self.processors {
            processor.process(samples, ctx);
        }
    }

    /// Reset all processors in the chain.
    pub fn reset(&mut self) {
        for processor in &mut self.processors {
            processor.reset();
        }
    }
}
