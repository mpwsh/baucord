//! Hard clamp processor for overflow protection.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → compressor → AGC → gain → **clamp** → VAD → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: nothing
//! - **Writes**: nothing
//!
//! Clamps all samples to [-0.95, 0.95], leaving headroom for
//! downstream processors (VAD, Opus encoder) that misbehave on
//! full-scale signals.

use super::{AudioProcessor, FrameContext};

/// Hard clamp to protect downstream from overflow.
///
/// The ceiling is 0.95 (not 1.0) to leave headroom for the Opus
/// encoder and earshot VAD, both of which can produce artifacts
/// on full-scale input.
#[derive(Default)]
pub struct ClampStage {
    ceiling: f32,
}

impl ClampStage {
    /// Create a clamp with the default ceiling (0.95).
    pub fn new() -> Self {
        Self { ceiling: 0.95 }
    }

    /// Create a clamp with a custom ceiling.
    pub fn with_ceiling(ceiling: f32) -> Self {
        Self {
            ceiling: ceiling.clamp(0.1, 1.0),
        }
    }
}

impl AudioProcessor for ClampStage {
    fn name(&self) -> &str {
        "clamp"
    }

    fn process(&mut self, samples: &mut [f32], _ctx: &mut FrameContext) {
        let ceil = self.ceiling;
        for s in samples.iter_mut() {
            *s = s.clamp(-ceil, ceil);
        }
    }

    fn reset(&mut self) {
        // Stateless — nothing to reset.
    }
}
