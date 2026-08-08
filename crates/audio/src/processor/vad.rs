//! Voice Activity Detection — a pure classifier.
//!
//! Wraps the earshot speech classifier with a hold timer. Does *not*
//! modify audio samples and — since v0.5 — does not gate transmission
//! either: every frame reaches the encoder and Opus DTX decides what
//! goes on the wire. This stage exists for the `is_speaking` UI
//! indicator and the `vad_active` stat. (Gating transmission here used
//! to clip the first word of every utterance, because earshot needs
//! 16 ms of buffered audio before its first speech decision.)
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → gate → compressor → AGC → gain → clamp → **VAD** → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: `ctx.gate_open` (gate is authoritative — if closed,
//!   never detect speech), `ctx.rnnoise_vad_prob` (pre-filter: skip
//!   expensive earshot on frames RNNoise already classifies as
//!   non-speech)
//! - **Writes**: `ctx.is_speech` (final speech determination),
//!   `ctx.fade_out_active` (one extra frame should be sent after speech
//!   ends — the samples themselves are not attenuated, this is just the
//!   "send one more" flag for the transmission logic)
//!
//! ## Why no sample-level fades
//!
//! Earlier versions of this stage applied a 1.3 ms fade-in when
//! `is_speech` transitioned false→true, and a 1.3 ms fade-out on the
//! opposite transition. That created audible clicks: by the time the
//! VAD said "speech", the gate had *already* been passing full-volume
//! audio for a couple of frames (gate open + earshot's 16 ms buffering
//! delay). Slapping a fade on that already-playing audio introduced a
//! step from ~1.0 amplitude down to ~0 and back — the exact
//! discontinuity fades exist to prevent. Symmetric problem on tail.
//!
//! The gate's own smoothstep fade on open/close is smooth enough on
//! its own. VAD stays out of the signal path.

use earshot::Detector;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AudioProcessor, FrameContext};

/// earshot expects 256 samples at 16 kHz (16 ms).
const EARSHOT_FRAME_16K: usize = 256;

/// Corresponding number of 48 kHz samples: 256 × 3 = 768.
const SAMPLES_48K_PER_FRAME: usize = EARSHOT_FRAME_16K * 3;

/// After VAD says "not speech", keep transmitting for this many frames
/// so trailing consonants and quiet syllable ends make it through the
/// encoder. 15 frames × 10 ms = 150 ms.
const VAD_HOLD_FRAMES: usize = 15;

/// Classifier stage. Owns the earshot detector and the speech-hold state
/// machine. Never touches sample values.
pub struct VadStage {
    // ── earshot classifier ───────────────────────────────────────────
    // Boxed: the detector carries FFT ring buffers + RNN state and
    // upstream only offers boxed constructors (stack-size safety).
    detector: Box<Detector>,
    accum_48k: Vec<f32>,
    /// Anti-alias lowpass applied at 48 kHz before the 3:1 decimation.
    aa_filter: Biquad,
    threshold: f32,

    // ── Speaking state (published for UI observers) ──────────────────
    is_speaking: Arc<AtomicBool>,
    last_transition: Instant,
    /// UI-side smoother: don't drop the "speaking" indicator for at
    /// least this long after speech stops. Purely visual — has no
    /// effect on the sample path or transmission.
    stop_cooldown: Duration,

    // ── Transmission hold timer ──────────────────────────────────────
    vad_hold_remaining: usize,
    was_speech: bool,
}

impl Default for VadStage {
    fn default() -> Self {
        Self::new()
    }
}

impl VadStage {
    pub fn new() -> Self {
        Self {
            detector: Detector::default_boxed(),
            accum_48k: Vec::with_capacity(SAMPLES_48K_PER_FRAME * 2),
            aa_filter: Biquad::lowpass(7000.0, 48000.0),
            threshold: 0.5,
            is_speaking: Arc::new(AtomicBool::new(false)),
            last_transition: Instant::now(),
            stop_cooldown: Duration::from_millis(1500),
            vad_hold_remaining: 0,
            was_speech: false,
        }
    }

    /// Build the stage around an externally owned speaking flag, so the
    /// indicator survives input-device swaps (the flag lives in
    /// `VoiceChat`, not in this stage).
    pub fn with_handle(is_speaking: Arc<AtomicBool>) -> Self {
        let mut s = Self::new();
        s.is_speaking = is_speaking;
        s
    }

    /// Handle to observe speaking state from outside the audio thread.
    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        self.is_speaking.clone()
    }

    // ── earshot speech classification ────────────────────────────────

    /// Accumulate 48 kHz samples, downsample 3:1 to 16 kHz, run earshot.
    ///
    /// The signal is lowpassed at ~7 kHz *before* decimation. The old
    /// 3-sample boxcar average only nulls at exact multiples of 16 kHz,
    /// so sibilant energy in the 8–16 kHz band aliased back into the
    /// 16 kHz frame and degraded the classifier on the exact sounds
    /// (s/sh/t) it most often mis-scores.
    fn run_earshot(&mut self, samples_48k: &[f32]) -> bool {
        let filt = &mut self.aa_filter;
        self.accum_48k
            .extend(samples_48k.iter().map(|&s| filt.process(s)));

        let mut detected_any = false;
        let mut ran_any = false;

        while self.accum_48k.len() >= SAMPLES_48K_PER_FRAME {
            let mut frame_16k = [0.0f32; EARSHOT_FRAME_16K];
            for (sample, chunk) in frame_16k.iter_mut().zip(self.accum_48k.chunks(3)) {
                // Post-filter, plain decimation is correct: keep every 3rd.
                *sample = chunk[0].clamp(-1.0, 1.0);
            }
            self.accum_48k.drain(..SAMPLES_48K_PER_FRAME);

            // Deliberate: earshot is pulled from an unpinned git ref, so
            // we treat it as untrusted and keep the input worker alive if
            // it panics. If the dependency gets pinned to an audited rev,
            // this guard can go. VAD is indicator-only, so a swallowed
            // panic costs a blip on the speaking light, nothing more.
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
                    log::error!("earshot panic caught — resetting detector");
                    self.detector.reset();
                }
            }
            ran_any = true;
        }

        if ran_any {
            detected_any
        } else {
            // Not enough buffered samples yet — hold the last decision.
            self.is_speaking.load(Ordering::Relaxed)
        }
    }

    /// Publish the speaking indicator with a slow-fall cooldown for UI.
    /// This affects only the observable `is_speaking` atomic, not the
    /// return value or `ctx.is_speech`.
    fn update_ui_indicator(&mut self, should_speak: bool) {
        let was_speaking = self.is_speaking.load(Ordering::Relaxed);

        if should_speak && !was_speaking {
            self.is_speaking.store(true, Ordering::Relaxed);
            self.last_transition = Instant::now();
        } else if !should_speak && was_speaking {
            if self.last_transition.elapsed() > self.stop_cooldown {
                self.is_speaking.store(false, Ordering::Relaxed);
                self.last_transition = Instant::now();
            }
        }
    }

    // ── Classification ───────────────────────────────────────────────

    /// Run VAD on the current frame. Returns `(is_speech, fade_out_active)`.
    /// Neither branch modifies the audio samples.
    fn classify(
        &mut self,
        samples: &[f32],
        gate_open: bool,
        rnnoise_vad_prob: f32,
    ) -> (bool, bool) {
        // Gate is authoritative: if closed, never detect speech.
        // RNNoise pre-filter: skip expensive earshot on clear non-speech.
        let vad_says_speech = if !gate_open || rnnoise_vad_prob < 0.1 {
            false
        } else {
            self.run_earshot(samples)
        };

        // Hold timer: keep transmission alive for VAD_HOLD_FRAMES after
        // earshot stops detecting speech, so trailing consonants and
        // quiet syllable ends aren't clipped from the encoder stream.
        if vad_says_speech {
            self.vad_hold_remaining = VAD_HOLD_FRAMES;
        }
        let is_speech = vad_says_speech || self.vad_hold_remaining > 0;
        if !vad_says_speech && self.vad_hold_remaining > 0 {
            self.vad_hold_remaining -= 1;
        }

        // fade_out_active is set on the first frame *after* speech ends,
        // so the transmission logic sends one final frame beyond the
        // hold window. Naming is historical: this flag no longer implies
        // any sample-level fade, just "one more frame please".
        let fade_out_active = !is_speech && self.was_speech;
        self.was_speech = is_speech;

        self.update_ui_indicator(is_speech);

        (is_speech, fade_out_active)
    }
}

impl AudioProcessor for VadStage {
    fn name(&self) -> &str {
        "vad"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        // Deliberately ignore the &mut on samples — this stage classifies,
        // it does not modify. Kept &mut only to match the trait signature.
        let (is_speech, fade_out_active) =
            self.classify(samples, ctx.gate_open, ctx.rnnoise_vad_prob);
        ctx.is_speech = is_speech;
        ctx.fade_out_active = fade_out_active;
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.accum_48k.clear();
        self.vad_hold_remaining = 0;
        self.was_speech = false;
        self.is_speaking.store(false, Ordering::Relaxed);
    }
}

// ── Anti-alias filter ───────────────────────────────────────────────────

/// Minimal RBJ-cookbook lowpass biquad (Direct Form 1).
///
/// Used only as the anti-alias filter in front of the 3:1 decimation for
/// earshot; not part of the audible signal path.
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Butterworth-Q lowpass at `fc` Hz for sample rate `fs`.
    fn lowpass(fc: f32, fs: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * fc / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let alpha = sin_w0 / (2.0 * q);

        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos_w0) / 2.0) / a0,
            b1: (1.0 - cos_w0) / a0,
            b2: ((1.0 - cos_w0) / 2.0) / a0,
            a1: (-2.0 * cos_w0) / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}
