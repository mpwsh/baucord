//! Voice Activity Detection using earshot (WebRTC VAD port).
//!
//! This module provides the **speech classifier** stage of VAD.
//! The cheap energy gate lives in `input.rs` and runs first to skip
//! all expensive processing on silent frames. earshot runs second
//! to distinguish speech from non-speech noise (keyboard, fan bursts).
//!
//! Hysteresis (cooldowns) prevents rapid on/off toggling.
//!
//! ## Resampling
//!
//! earshot operates at **16 kHz, 256-sample frames (16ms)**.
//! Our pipeline runs at 48 kHz with 480-sample frames (10ms).
//! We do a simple 3:1 decimation with averaging — this is fine for
//! a binary speech/no-speech classifier (no need for a full sinc
//! resampler just for VAD).

use earshot::{Detector, QuantizedPredictor};
use log::error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Real-time speech classifier wrapping earshot.
///
/// earshot returns a 0.0–1.0 voice probability per frame. We apply a
/// threshold + cooldown timers to produce a stable binary state.
/// The energy-based gate happens upstream in the input pipeline, not here.
pub struct VADProcessor {
    detector: Detector<QuantizedPredictor<'static>>,
    /// Accumulator for 48 kHz samples waiting to be downsampled.
    accum_48k: Vec<f32>,
    is_speaking: Arc<AtomicBool>,
    last_transition: Instant,

    /// Score threshold above which we consider speech detected.
    threshold: f32,
    start_cooldown: Duration,
    stop_cooldown: Duration,
}

/// earshot expects 256 samples at 16 kHz (16ms).
const EARSHOT_FRAME_16K: usize = 256;

/// Corresponding number of 48 kHz samples: 256 * 3 = 768.
const SAMPLES_48K_PER_FRAME: usize = EARSHOT_FRAME_16K * 3;

impl VADProcessor {
    pub fn new() -> Self {
        Self {
            detector: Detector::<QuantizedPredictor>::default(),
            accum_48k: Vec::with_capacity(SAMPLES_48K_PER_FRAME * 2),
            is_speaking: Arc::new(AtomicBool::new(false)),
            last_transition: Instant::now(),
            threshold: 0.5,
            start_cooldown: Duration::from_millis(0),
            stop_cooldown: Duration::from_millis(1500),
        }
    }

    /// Classify whether a frame contains speech. Returns `true` if speech detected.
    ///
    /// Only call this on frames that already passed the energy gate —
    /// earshot is more expensive than a simple RMS check.
    pub fn process(&mut self, samples_48k: &[f32]) -> bool {
        let detected = self.run_earshot(samples_48k);
        self.update_state(detected)
    }

    /// Accumulate 48 kHz samples, downsample 3:1 to 16 kHz, run earshot.
    fn run_earshot(&mut self, samples_48k: &[f32]) -> bool {
        self.accum_48k.extend_from_slice(samples_48k);

        let mut detected_any = false;
        let mut ran_any = false;

        while self.accum_48k.len() >= SAMPLES_48K_PER_FRAME {
            // Downsample 48k → 16k by averaging groups of 3
            let mut frame_16k = [0.0f32; EARSHOT_FRAME_16K];
            for (sample, chunk) in frame_16k.iter_mut().zip(self.accum_48k.chunks(3)) {
                let avg = (chunk[0] + chunk[1] + chunk[2]) / 3.0;
                *sample = avg.clamp(-1.0, 1.0);
            }
            self.accum_48k.drain(..SAMPLES_48K_PER_FRAME);

            // Safety: catch any panics from earshot (e.g., unexpected
            // internal state from extreme input values)
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
            // Not enough data yet — hold previous state
            self.is_speaking.load(Ordering::Relaxed)
        }
    }

    fn update_state(&mut self, should_speak: bool) -> bool {
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
}
