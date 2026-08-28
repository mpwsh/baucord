//! Simple gain multiplier processor.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → AGC → compressor → **gain** → clamp → speaking → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.input_gain` (multiplier from user setting)
//! - **Writes**: nothing

use super::{AudioProcessor, FrameContext};

/// Applies a gain multiplier to all samples.
///
/// The gain value comes from `FrameContext::input_gain`, which the
/// worker populates from the user's atomic setting each frame.
/// Unity gain (1.0) is a no-op.
#[derive(Default)]
pub struct GainStage;

impl GainStage {
    pub fn new() -> Self {
        Self
    }
}

impl AudioProcessor for GainStage {
    fn name(&self) -> &str {
        "gain"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        let gain = ctx.input_gain;
        // Skip multiply when gain is unity (common case)
        if (gain - 1.0).abs() <= f32::EPSILON {
            return;
        }
        for s in samples.iter_mut() {
            *s *= gain;
        }
    }

    fn reset(&mut self) {
        // Stateless — nothing to reset.
    }
}
