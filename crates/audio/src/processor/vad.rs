//! Voice Activity Detection stage with hold timer and crossfade.
//!
//! Wraps the earshot speech classifier and adds the state machine that
//! was previously inline in `input.rs`: hold timer (keep transmitting
//! briefly after speech ends) and crossfade (smooth transitions to
//! prevent clicks at VAD on/off edges).
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → compressor → AGC → gain → clamp → **VAD** → encode
//! ```
//!
//! This must be the **last** processor in the chain because it modifies
//! the signal (fade in/out) based on the speech decision.
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.gate_open` (gate is authoritative — if closed,
//!   no speech), `ctx.rnnoise_vad_prob` (pre-filter: skip expensive
//!   earshot on frames RNNoise already classifies as non-speech)
//! - **Writes**: `ctx.is_speech` (final speech determination),
//!   `ctx.fade_out_active` (one extra frame should be sent, ramped to zero)

use earshot::{Detector, QuantizedPredictor};
use log::error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AudioProcessor, FrameContext};

/// earshot expects 256 samples at 16 kHz (16 ms).
const EARSHOT_FRAME_16K: usize = 256;

/// Corresponding number of 48 kHz samples: 256 × 3 = 768.
const SAMPLES_48K_PER_FRAME: usize = EARSHOT_FRAME_16K * 3;

/// Number of samples for fade-in/fade-out transitions.
/// 64 samples at 48 kHz ≈ 1.3 ms — inaudible but eliminates the step
/// discontinuity that causes digital cracking.
const FADE_SAMPLES: usize = 64;

/// After VAD says "not speech", keep sending for this many frames
/// before starting the fade-out. Prevents aggressive tail cutting
/// on word endings and quiet trailing syllables.
/// 15 frames × 10 ms = 150 ms.
const VAD_HOLD_FRAMES: usize = 15;

/// Combined VAD + hold + crossfade processor.
///
/// The gate is authoritative for the noise floor: if the gate is
/// closed, we never detect speech. The VAD distinguishes speech from
/// non-speech noise above the gate threshold.
pub struct VadStage {
    // ── earshot classifier ───────────────────────────────────────────
    detector: Detector<QuantizedPredictor<'static>>,
    accum_48k: Vec<f32>,
    threshold: f32,

    // ── Cooldown (hysteresis) ────────────────────────────────────────
    is_speaking: Arc<AtomicBool>,
    last_transition: Instant,
    start_cooldown: Duration,
    stop_cooldown: Duration,

    // ── Hold timer ───────────────────────────────────────────────────
    vad_hold_remaining: usize,
    was_speech: bool,

    // ── Crossfade state ──────────────────────────────────────────────
    fade_in_remaining: usize,
    fade_out_remaining: usize,
}
impl Default for VadStage {
    fn default() -> Self {
        Self::new()
    }
}
impl VadStage {
    pub fn new() -> Self {
        Self {
            detector: Detector::<QuantizedPredictor>::default(),
            accum_48k: Vec::with_capacity(SAMPLES_48K_PER_FRAME * 2),
            threshold: 0.5,
            is_speaking: Arc::new(AtomicBool::new(false)),
            last_transition: Instant::now(),
            start_cooldown: Duration::from_millis(0),
            stop_cooldown: Duration::from_millis(1500),
            vad_hold_remaining: 0,
            was_speech: false,
            fade_in_remaining: 0,
            fade_out_remaining: 0,
        }
    }

    /// Handle to observe speaking state from outside the audio thread.
    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        self.is_speaking.clone()
    }

    // ── earshot speech classification ────────────────────────────────

    /// Accumulate 48 kHz samples, downsample 3:1 to 16 kHz, run earshot.
    fn run_earshot(&mut self, samples_48k: &[f32]) -> bool {
        self.accum_48k.extend_from_slice(samples_48k);

        let mut detected_any = false;
        let mut ran_any = false;

        while self.accum_48k.len() >= SAMPLES_48K_PER_FRAME {
            let mut frame_16k = [0.0f32; EARSHOT_FRAME_16K];
            for (sample, chunk) in frame_16k.iter_mut().zip(self.accum_48k.chunks(3)) {
                let avg = (chunk[0] + chunk[1] + chunk[2]) / 3.0;
                *sample = avg.clamp(-1.0, 1.0);
            }
            self.accum_48k.drain(..SAMPLES_48K_PER_FRAME);

            let detector = &mut self.detector;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                detector.predict_f32(&frame_16k)
            }));

            match result {
                Ok(score) => {
                    if score >= self.threshold {
                        detected_any = true;
                    }
                }
                Err(_) => {
                    error!("earshot panic caught — resetting detector");
                    self.detector = Detector::<QuantizedPredictor>::default();
                }
            }
            ran_any = true;
        }

        if ran_any {
            detected_any
        } else {
            self.is_speaking.load(Ordering::Relaxed)
        }
    }

    /// Apply cooldown hysteresis to prevent rapid on/off toggling.
    fn update_cooldown(&mut self, should_speak: bool) -> bool {
        let was_speaking = self.is_speaking.load(Ordering::Relaxed);
        let elapsed = self.last_transition.elapsed();

        let cooldown = if was_speaking && !should_speak {
            self.stop_cooldown
        } else if !was_speaking && should_speak {
            self.start_cooldown
        } else {
            Duration::ZERO
        };

        if should_speak != was_speaking && elapsed > cooldown {
            self.is_speaking.store(should_speak, Ordering::Relaxed);
            self.last_transition = Instant::now();
        }

        self.is_speaking.load(Ordering::Relaxed)
    }

    // ── Hold + crossfade logic (extracted from input.rs) ─────────────

    /// Run the full VAD pipeline: classify → cooldown → hold → crossfade.
    fn classify_and_fade(
        &mut self,
        samples: &mut [f32],
        gate_open: bool,
        rnnoise_vad_prob: f32,
    ) -> (bool, bool) {
        // Gate is authoritative: if closed, never detect speech.
        // RNNoise pre-filter: skip expensive earshot on clear non-speech.
       let vad_says_speech = if !gate_open || rnnoise_vad_prob < 0.1 {
    false
} else {
    let detected = self.run_earshot(samples);
    self.update_cooldown(detected)
}; 

        // Hold timer: keep "speaking" for VAD_HOLD_FRAMES after speech ends.
        if vad_says_speech {
            self.vad_hold_remaining = VAD_HOLD_FRAMES;
        }
        let is_speech = vad_says_speech || self.vad_hold_remaining > 0;
        if !vad_says_speech && self.vad_hold_remaining > 0 {
            self.vad_hold_remaining -= 1;
        }

        self.is_speaking.store(is_speech, Ordering::Relaxed);

        // ── Crossfade at VAD transitions ────────────────────────────
        if is_speech && !self.was_speech {
            // Only fade in when the gate is already fully open.
            // If the gate is still in its Opening fade, skip the
            // VAD fade-in to avoid double-fading the onset.
            if gate_open {
                self.fade_in_remaining = FADE_SAMPLES;
            }
        }
        if !is_speech && self.was_speech {
            self.fade_out_remaining = FADE_SAMPLES;
        }
        self.was_speech = is_speech;

        // Apply fade-in
        if is_speech && self.fade_in_remaining > 0 {
            for s in samples.iter_mut() {
                if self.fade_in_remaining > 0 {
                    let t = 1.0 - (self.fade_in_remaining as f32 / FADE_SAMPLES as f32);
                    *s *= t;
                    self.fade_in_remaining -= 1;
                }
            }
        }

        let fade_out_active = !is_speech && self.fade_out_remaining > 0;

        // Apply fade-out
        if fade_out_active {
            for s in samples.iter_mut() {
                if self.fade_out_remaining > 0 {
                    let t = self.fade_out_remaining as f32 / FADE_SAMPLES as f32;
                    *s *= t;
                    self.fade_out_remaining -= 1;
                }
            }
        }

        (is_speech, fade_out_active)
    }
}

impl AudioProcessor for VadStage {
    fn name(&self) -> &str {
        "vad"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        let (is_speech, fade_out_active) =
            self.classify_and_fade(samples, ctx.gate_open, ctx.rnnoise_vad_prob);
        ctx.is_speech = is_speech;
        ctx.fade_out_active = fade_out_active;
    }

    fn reset(&mut self) {
        self.detector = Detector::<QuantizedPredictor>::default();
        self.accum_48k.clear();
        self.vad_hold_remaining = 0;
        self.was_speech = false;
        self.fade_in_remaining = 0;
        self.fade_out_remaining = 0;
        self.is_speaking.store(false, Ordering::Relaxed);
    }
}