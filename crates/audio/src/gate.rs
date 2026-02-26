//! Input noise gate with smooth open/close transitions.
//!
//! Hard threshold gate that silences anything below a configurable
//! RMS level. Sits after denoise and before gain/VAD in the input
//! pipeline, preventing low-level noise from ever reaching the
//! encoder.
//!
//! Unlike AGC (which adjusts gain to normalize levels), the gate
//! is binary: signal passes through untouched above the threshold,
//! and is silenced below it. This gives users full control over
//! their own input volume while keeping the noise floor clean.
//!
//! ## Design
//!
//! - **Threshold** is RMS-based (not peak), so brief transients
//!   don't cause false opens and sustained low noise doesn't leak.
//! - **Hysteresis** uses separate open/close thresholds to prevent
//!   chatter at the boundary (close threshold is slightly below open).
//! - **Smooth transitions** via linear fade over ~1.3ms (64 samples
//!   at 48 kHz) eliminate the click/pop artifacts that a hard cut
//!   would produce.
//! - **Hold time** keeps the gate open for a short period after
//!   signal drops below threshold, preventing clipping of natural
//!   speech pauses and word endings.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Gate state machine states.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GateState {
    /// Gate is closed — output silence.
    Closed,
    /// Gate is opening — fading in from silence to full signal.
    Opening,
    /// Gate is open — signal passes through untouched.
    Open,
    /// Signal dropped below threshold but we're in the hold period.
    Holding,
    /// Gate is closing — fading out from full signal to silence.
    Closing,
}

/// Real-time noise gate processor.
///
/// Call [`process_frame`] once per 480-sample frame (10ms at 48 kHz).
/// The gate threshold is adjustable at runtime via the atomic handle
/// returned by [`threshold_handle`].
pub struct NoiseGate {
    /// RMS threshold to open the gate (linear, 0.0–1.0).
    /// Stored as f32 bits in an atomic for lock-free runtime adjustment.
    threshold: Arc<AtomicU32>,

    /// Hysteresis ratio — close threshold = open threshold × this value.
    /// Prevents chatter when signal hovers near the threshold.
    /// 0.6 means the gate closes at 60% of the open threshold.
    hysteresis_ratio: f32,

    /// Current gate state.
    state: GateState,

    /// Smoothed RMS envelope for stable threshold comparison.
    envelope: f32,

    /// Fade progress: 0.0 = fully closed, 1.0 = fully open.
    /// Used during Opening and Closing states for smooth transitions.
    fade_position: f32,

    /// Fade increment per sample. At 48 kHz with 64-sample fade:
    /// 1.0 / 64 ≈ 0.015625 per sample ≈ 1.3ms transition.
    fade_step: f32,

    /// Hold counter: samples remaining before transitioning from
    /// Holding → Closing. Prevents clipping word tails.
    hold_remaining: usize,

    /// Hold duration in samples. 50ms at 48 kHz = 2400 samples.
    /// Long enough to bridge natural speech pauses within a word,
    /// short enough to close promptly during actual silence.
    hold_samples: usize,
}

/// Number of samples for fade-in/fade-out transitions.
/// 64 samples at 48 kHz ≈ 1.3ms — inaudible but eliminates clicks.
const FADE_SAMPLES: usize = 64;

impl NoiseGate {
    /// Create a new noise gate with a shared threshold atomic.
    ///
    /// Use this when the threshold needs to be adjustable from outside
    /// (e.g., from the AudioInput's runtime controls). The gate reads
    /// the threshold from this atomic on every `process_frame` call.
    ///
    /// - `threshold_handle`: Shared atomic holding the RMS threshold as f32 bits.
    /// - `hold_ms`: How long the gate stays open after signal drops
    ///   below threshold. Default 50ms bridges natural speech pauses.
    pub fn with_threshold_handle(threshold_handle: Arc<AtomicU32>, hold_ms: u32) -> Self {
        let hold_samples = (48000 * hold_ms as usize) / 1000;

        Self {
            threshold: threshold_handle,
            hysteresis_ratio: 0.6,
            state: GateState::Closed,
            envelope: 0.0,
            fade_position: 0.0,
            fade_step: 1.0 / FADE_SAMPLES as f32,
            hold_remaining: 0,
            hold_samples,
        }
    }

    /// Create a new noise gate.
    ///
    /// - `threshold`: RMS level (0.0–1.0) below which audio is gated.
    ///   Good starting points:
    ///   - `0.005` (~-46 dBFS): Very low — only gates near-silence
    ///   - `0.010` (~-40 dBFS): Low — gates quiet background noise
    ///   - `0.020` (~-34 dBFS): Medium — gates moderate noise (default)
    ///   - `0.040` (~-28 dBFS): High — requires clear speech to open
    ///   - `0.080` (~-22 dBFS): Aggressive — only loud speech passes
    ///
    /// - `hold_ms`: How long the gate stays open after signal drops
    ///   below threshold. Default 50ms bridges natural speech pauses.
    pub fn new(threshold: f32, hold_ms: u32) -> Self {
        let threshold_clamped = threshold.clamp(0.001, 0.5);
        // 48 kHz assumed — this module only sees post-resample audio
        let hold_samples = (48000 * hold_ms as usize) / 1000;

        Self {
            threshold: Arc::new(AtomicU32::new(threshold_clamped.to_bits())),
            hysteresis_ratio: 0.6,
            state: GateState::Closed,
            envelope: 0.0,
            fade_position: 0.0,
            fade_step: 1.0 / FADE_SAMPLES as f32,
            hold_remaining: 0,
            hold_samples,
        }
    }

    /// Get a handle to the threshold atomic for runtime adjustment.
    pub fn threshold_handle(&self) -> Arc<AtomicU32> {
        self.threshold.clone()
    }

    /// Process a frame of audio in-place.
    ///
    /// Samples below the gate threshold are faded to silence.
    /// Samples above pass through untouched (no gain modification).
    ///
    /// Returns `true` if the gate is currently open (signal is passing).
    pub fn process_frame(&mut self, samples: &mut [f32]) -> bool {
        let open_threshold = f32::from_bits(self.threshold.load(Ordering::Relaxed));
        let close_threshold = open_threshold * self.hysteresis_ratio;

        // Update envelope from frame RMS.
        // NOTE: This is called once per frame (every 10ms / 480 samples),
        // so coefficients are per-frame, not per-sample.
        let rms = Self::compute_rms(samples);
        if rms > self.envelope {
            // Rising — snap fast to catch speech onset.
            // ~1 frame attack: envelope reaches ~80% of target in one frame.
            self.envelope = self.envelope * 0.2 + rms * 0.8;
        } else {
            // Falling — release smoothly over ~5 frames (~50ms).
            self.envelope = self.envelope * 0.8 + rms * 0.2;
        }

        // Apply gate to samples, running state transitions per-sample
        // so fades and hold timers advance correctly within the frame.
        for s in samples.iter_mut() {
            match self.state {
                GateState::Closed => {
                    *s = 0.0;
                    if self.envelope >= open_threshold {
                        self.state = GateState::Opening;
                        // Don't zero this sample — start the fade immediately
                        self.fade_position = (self.fade_position + self.fade_step).min(1.0);
                        *s *= self.fade_position;
                    }
                }
                GateState::Opening => {
                    self.fade_position = (self.fade_position + self.fade_step).min(1.0);
                    *s *= self.fade_position;
                    if self.fade_position >= 1.0 {
                        self.state = GateState::Open;
                    } else if self.envelope < close_threshold {
                        // Signal dropped during fade-in — reverse
                        self.state = GateState::Closing;
                    }
                }
                GateState::Open => {
                    // Pass through untouched
                    if self.envelope < close_threshold {
                        self.state = GateState::Holding;
                        self.hold_remaining = self.hold_samples;
                    }
                }
                GateState::Holding => {
                    // Pass through untouched during hold
                    if self.envelope >= close_threshold {
                        self.state = GateState::Open;
                        self.hold_remaining = 0;
                    } else if self.hold_remaining == 0 {
                        self.state = GateState::Closing;
                    } else {
                        self.hold_remaining -= 1;
                    }
                }
                GateState::Closing => {
                    self.fade_position = (self.fade_position - self.fade_step).max(0.0);
                    *s *= self.fade_position;
                    if self.fade_position <= 0.0 {
                        self.state = GateState::Closed;
                    } else if self.envelope >= open_threshold {
                        // Signal came back during fade-out — reverse
                        self.state = GateState::Opening;
                    }
                }
            }
        }

        self.is_open()
    }

    /// Whether the gate is currently passing audio.
    pub fn is_open(&self) -> bool {
        matches!(
            self.state,
            GateState::Open | GateState::Opening | GateState::Holding
        )
    }

    /// Reset gate state (e.g., after device switch or unmute).
    pub fn reset(&mut self) {
        self.state = GateState::Closed;
        self.envelope = 0.0;
        self.fade_position = 0.0;
        self.hold_remaining = 0;
    }

    fn compute_rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }
}