//! Input noise gate with smooth open/close transitions.
//!
//! Hard threshold gate that silences anything below a configurable
//! RMS level. Prevents low-level noise from ever reaching the encoder.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → **gate** → compressor → AGC → gain → clamp → VAD → encode
//! ```
//!
//! ## Context interaction
//!
//! - **Reads**: nothing (threshold comes from its own atomic handle)
//! - **Writes**: `ctx.gate_open` — downstream processors (compressor,
//!   AGC) use this to decide whether to adapt or hold.
//!
//! ## Design
//!
//! - **Threshold** is RMS-based (not peak), so brief transients
//!   don't cause false opens and sustained low noise doesn't leak.
//! - **Hysteresis** uses separate open/close thresholds to prevent
//!   chatter at the boundary (close threshold is slightly below open).
//! - **Smooth transitions** via linear fade over ~1.3ms (64 samples
//!   at 48 kHz) eliminate click/pop artifacts.
//! - **Hold time** keeps the gate open after signal drops below
//!   threshold, preventing clipping of natural speech pauses.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use super::{compute_rms, AudioProcessor, FrameContext};

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

/// Number of samples for fade-in/fade-out transitions.
/// 256 samples at 48 kHz ≈ 5.2ms — inaudible but eliminates clicks.
const FADE_SAMPLES: usize = 256;

/// Real-time noise gate processor.
///
/// Call [`process`](AudioProcessor::process) once per 480-sample frame
/// (10 ms at 48 kHz). The gate threshold is adjustable at runtime via
/// the atomic handle from [`threshold_handle`](Self::threshold_handle).
pub struct NoiseGate {
    /// RMS threshold (linear, 0.0–1.0), stored as f32 bits in an
    /// atomic for lock-free runtime adjustment.
    knobs: Arc<Knobs>,

    /// Hysteresis ratio — close threshold = open threshold × this.
    /// 0.6 means the gate closes at 60% of the open threshold.
    hysteresis_ratio: f32,

    state: GateState,
    envelope: f32,
    fade_position: f32,
    fade_step: f32,
    hold_remaining: usize,
    hold_samples: usize,
}

impl NoiseGate {
    /// Create a noise gate with a shared threshold atomic.
    ///
    /// Use this when the threshold needs runtime adjustment from outside
    /// (e.g., from the `VoiceChat` API). The gate reads the threshold
    /// from this atomic on every frame.
    ///
    /// - `threshold_handle`: shared atomic holding the RMS threshold as f32 bits.
    /// - `hold_ms`: how long the gate stays open after signal drops below
    ///   threshold. 50–150 ms is good for voice chat.
    pub fn new(knobs: Arc<Knobs>, hold_ms: u32) -> Self {
        let hold_samples = (48_000 * hold_ms as usize) / 1000;
        Self {
            knobs,
            hysteresis_ratio: 0.6,
            state: GateState::Closed,
            envelope: 0.0,
            fade_position: 0.0,
            fade_step: 1.0 / FADE_SAMPLES as f32,
            hold_remaining: 0,
            hold_samples,
        }
    }

    /// Whether the gate is currently passing audio.
    pub fn is_open(&self) -> bool {
        matches!(
            self.state,
            GateState::Open | GateState::Opening | GateState::Holding
        )
    }

    /// Run the gate state machine on one frame. Returns whether the
    /// gate is open after processing.
    fn process_inner(&mut self, samples: &mut [f32]) -> bool {
        let open_threshold = f32::from_bits(self.knobs.threshold.load(Ordering::Relaxed));
        let close_threshold = open_threshold * self.hysteresis_ratio;

        // Update envelope from frame RMS (per-frame, not per-sample).
        let rms = compute_rms(samples);
        if rms > self.envelope {
            // Rising — snap fast to catch speech onset (~1 frame attack).
            self.envelope = self.envelope * 0.2 + rms * 0.8;
        } else {
            // Prevents mid-sentence cutoffs on low-gain mics.
            self.envelope = self.envelope * 0.95 + rms * 0.05;
        }

        // Per-sample state machine for smooth fades and accurate hold timing.
        for s in samples.iter_mut() {
            match self.state {
                GateState::Closed => {
                    *s = 0.0;
                    if self.envelope >= open_threshold {
                        self.state = GateState::Opening;
                    }
                }
                GateState::Opening => {
                    self.fade_position = (self.fade_position + self.fade_step).min(1.0);
                    *s *= smoothstep(self.fade_position);
                    if self.fade_position >= 1.0 {
                        self.state = GateState::Open;
                    } else if self.envelope < close_threshold {
                        self.state = GateState::Closing;
                    }
                }
                GateState::Open => {
                    if self.envelope < close_threshold {
                        self.state = GateState::Holding;
                        self.hold_remaining = self.hold_samples;
                    }
                }
                GateState::Holding => {
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
                    *s *= smoothstep(self.fade_position);
                    if self.fade_position <= 0.0 {
                        self.state = GateState::Closed;
                    } else if self.envelope >= open_threshold {
                        self.state = GateState::Opening;
                    }
                }
            }
        }

        self.is_open()
    }
}

impl AudioProcessor for NoiseGate {
    fn name(&self) -> &str {
        "gate"
    }

    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        ctx.gate_open = self.process_inner(samples);
    }

    fn reset(&mut self) {
        self.state = GateState::Closed;
        self.envelope = 0.0;
        self.fade_position = 0.0;
        self.hold_remaining = 0;
    }
}

/// Tuneable parameters for the noise gate.
///
/// Lives inside [`InputConfig`](crate::input_config::InputConfig) and is
/// read by the gate every frame. All fields are atomic for lock-free
/// access from the UI thread.
pub struct Knobs {
    /// RMS threshold (f32 bits, 0.001–0.5) below which audio is gated.
    pub threshold: AtomicU32,
}

impl Knobs {
    /// Create with an explicit threshold.
    pub fn with_threshold(threshold: f32) -> Self {
        Self {
            threshold: AtomicU32::new(threshold.clamp(0.001, 0.5).to_bits()),
        }
    }
}

impl Default for Knobs {
    fn default() -> Self {
        Self::with_threshold(0.012)
    }
}

/// Map linear fade position (0.0–1.0) to smoothstep gain.
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}
