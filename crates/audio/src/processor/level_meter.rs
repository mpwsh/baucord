//! RMS level meter processor.
//!
//! A "read-only" processor that measures the signal without modifying
//! it. Writes the scaled RMS to a [`FrameContext`] field so it can be
//! forwarded to stats/UI by the worker thread.
//!
//! Insert at any point in the chain where you need a level reading.
//! The `target` field determines which `FrameContext` field gets written.
//!
//! ## Context interaction
//!
//! - **Reads**: nothing
//! - **Writes**: one of `ctx.denoised_level` or `ctx.pipeline_level`
//!   (depending on `MeterTarget`)

use super::{compute_rms, AudioProcessor, FrameContext};

/// Which [`FrameContext`] field this meter writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterTarget {
    /// Post-denoise level — used for gate threshold visualisation.
    Denoised,
    /// Post-pipeline level — what the encoder actually sees.
    Pipeline,
}

/// RMS level meter that measures without modifying the signal.
///
/// Writes a UI-scaled (×3, clamped 0–1) RMS value into `FrameContext`.
pub struct LevelMeter {
    target: MeterTarget,
}

impl LevelMeter {
    /// Create a level meter that writes to the specified context field.
    #[must_use]
    pub fn new(target: MeterTarget) -> Self {
        Self { target }
    }
}

impl AudioProcessor for LevelMeter {
    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        let rms = compute_rms(samples);
        // Scale ×3 and clamp for UI-friendly 0–1 range.
        // Voice RMS typically sits 0.01–0.15, so ×3 puts normal
        // speech around 0.03–0.45 on the meter.
        let scaled = (rms * 3.0).clamp(0.0, 1.0);

        match self.target {
            MeterTarget::Denoised => ctx.denoised_level = scaled,
            MeterTarget::Pipeline => ctx.pipeline_level = scaled,
        }
    }

    fn reset(&mut self) {
        // Stateless — nothing to reset.
    }
}
