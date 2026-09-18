//! Speaking indicator — drives the `is_speaking` UI flag and `vad_active`.
//!
//! Replaces the earshot-based VAD stage. That stage cost a biquad, a 3:1
//! decimation, an RNN classifier and a `catch_unwind` per call, and its
//! result gated nothing: transmission is decided by the gate + Opus DTX.
//! `RNNoise` already produces a speech probability for every frame as a
//! by-product of denoising, so this stage just combines it with the gate
//! and adds the hold/cooldown behaviour the UI wants.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → AGC → compressor → gain → clamp → **speaking** → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.gate_open` (gate is authoritative — if closed, never
//!   speech), `ctx.rnnoise_vad_prob` (1.0 when denoise is bypassed, so
//!   the gate alone decides)
//! - **Writes**: `ctx.is_speech`
//!
//! Never touches sample values.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use super::{AudioProcessor, FrameContext};

/// `RNNoise` speech probability at or above which a frame is speech.
const VAD_THRESHOLD: f32 = 0.5;

/// After speech stops, keep `is_speech` true for this many frames so the
/// indicator doesn't flicker on syllable boundaries. 8 × 20 ms = 160 ms.
const HOLD_FRAMES: u32 = 8;

/// UI-side smoother: don't drop the "speaking" light for at least this
/// long after speech stops. Purely visual.
const STOP_COOLDOWN: Duration = Duration::from_millis(1500);

pub struct SpeakingIndicator {
    is_speaking: Arc<AtomicBool>,
    hold_remaining: u32,
    last_transition: Instant,
}

impl Default for SpeakingIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeakingIndicator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            is_speaking: Arc::new(AtomicBool::new(false)),
            hold_remaining: 0,
            last_transition: Instant::now(),
        }
    }

    /// Handle to observe speaking state from outside the audio thread.
    #[must_use]
    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        self.is_speaking.clone()
    }

    fn classify(&mut self, gate_open: bool, vad_prob: f32) -> bool {
        if gate_open && vad_prob >= VAD_THRESHOLD {
            self.hold_remaining = HOLD_FRAMES;
            return true;
        }
        // Inside the hold: this frame still counts, then spend one.
        if self.hold_remaining > 0 {
            self.hold_remaining -= 1;
            return true;
        }
        false
    }

    /// Publish the speaking indicator with a slow-fall cooldown for UI.
    fn update_ui_indicator(&mut self, should_speak: bool) {
        let was_speaking = self.is_speaking.load(Ordering::Relaxed);
        if should_speak && !was_speaking {
            self.is_speaking.store(true, Ordering::Relaxed);
            self.last_transition = Instant::now();
        } else if !should_speak && was_speaking && self.last_transition.elapsed() > STOP_COOLDOWN {
            self.is_speaking.store(false, Ordering::Relaxed);
            self.last_transition = Instant::now();
        }
    }
}

impl AudioProcessor for SpeakingIndicator {
    fn name(&self) -> &'static str {
        "speaking"
    }

    fn process(&mut self, _samples: &mut [f32], ctx: &mut FrameContext) {
        let is_speech = self.classify(ctx.gate_open, ctx.rnnoise_vad_prob);
        ctx.is_speech = is_speech;
        self.update_ui_indicator(is_speech);
    }

    fn reset(&mut self) {
        self.hold_remaining = 0;
        self.is_speaking.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_gate_is_never_speech() {
        let mut s = SpeakingIndicator::new();
        let mut ctx = FrameContext {
            gate_open: false,
            rnnoise_vad_prob: 1.0,
            ..FrameContext::default()
        };
        s.process(&mut [], &mut ctx);
        assert!(!ctx.is_speech);
    }

    #[test]
    fn speech_holds_for_hold_frames_after_it_stops() {
        let mut s = SpeakingIndicator::new();
        let mut ctx = FrameContext {
            gate_open: true,
            rnnoise_vad_prob: 0.9,
            ..FrameContext::default()
        };
        s.process(&mut [], &mut ctx);
        assert!(ctx.is_speech);
        ctx.rnnoise_vad_prob = 0.0;
        for _ in 0..HOLD_FRAMES {
            s.process(&mut [], &mut ctx);
            assert!(ctx.is_speech, "should hold");
        }
        s.process(&mut [], &mut ctx);
        assert!(!ctx.is_speech, "hold expired");
    }
}
