//! Input noise gate with smooth open/close transitions.
//!
//! Hard threshold gate that silences anything below a configurable
//! RMS level. Prevents low-level noise from ever reaching the encoder.
//!
//! ## Pipeline position
//!
//! ```text
//! capture → resample → denoise → **gate** → AGC → compressor → gain → clamp → speaking → encode
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
//! - **Smooth transitions** via linear fade over 8 ms (`FADE_SAMPLES`)
//!   eliminate click/pop artifacts.
//! - **Hold time** keeps the gate open after signal drops below
//!   threshold, preventing clipping of natural speech pauses.
//! - **Hold-window expansion**: during the hold the gate is not fully
//!   transparent — a gentle downward expander attenuates the signal
//!   by how far it sits below the close threshold (up to
//!   [`HOLD_EXPANDER_FLOOR`], −15 dB). Trailing syllables live at or above
//!   the threshold and pass untouched, but the residual noise floor
//!   (post-denoise hiss, boosted upstream of nothing but the encoder)
//!   is pushed down immediately instead of playing out as an audible
//!   noise tail for the whole hold period. Attack is near-instant so
//!   resuming speech is never dulled; release is smoothed to avoid
//!   zipper noise.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

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
/// 384 samples at 48 kHz = 8 ms — inaudible, eliminates clicks.
const FADE_SAMPLES: usize = 384;

/// Downward-expansion ratio applied while the envelope is below the
/// close threshold (the hold window). 2.0 means every dB below the
/// threshold costs one extra dB of attenuation — gentle enough that
/// word endings decay naturally, firm enough to sink the noise floor.
const HOLD_EXPANDER_RATIO: f32 = 2.0;

/// Maximum attenuation the hold-window expander applies. The point is
/// to move the noise tail below audibility, not to hard-mute the hold
/// (hard muting would defeat the hold's purpose). Runtime code uses
/// the precomputed linear `HOLD_EXPANDER_FLOOR`; this is what it means.
#[cfg(test)]
const HOLD_EXPANDER_FLOOR_DB: f32 = -15.0;

/// `HOLD_EXPANDER_FLOOR_DB` as linear gain (10^(-15/20)), precomputed
/// rather than `powf`'d on every frame.
const HOLD_EXPANDER_FLOOR: f32 = 0.177_827_94;

/// Per-sample smoothing for expander gain *decreases* (~21 ms time
/// constant at 48 kHz) — slow enough to be zipper-free and to average
/// out frame-to-frame RMS wobble on the noise floor.
const EXPANDER_RELEASE: f32 = 0.001;

/// Per-sample smoothing for expander gain *increases* (~0.4 ms) —
/// effectively instant so a resumed speech onset is never dulled;
/// upward gain steps are masked by the onset itself.
const EXPANDER_ATTACK: f32 = 0.05;

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
    /// Smoothed hold-window expander gain (1.0 while speech is at or
    /// above the close threshold).
    expander_gain: f32,
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
            expander_gain: 1.0,
        }
    }

    /// Whether the gate is currently passing audio.
    #[must_use]
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

        // Update envelope from frame RMS (per-frame, not per-sample;
        // coefficients are for the 20 ms frame cadence).
        let rms = compute_rms(samples);
        if rms > self.envelope {
            // Rising — snap fast to catch speech onset (~1 frame attack).
            self.envelope = self.envelope * 0.2 + rms * 0.8;
        } else {
            // ≈ 0.95 per 10 ms compounded: prevents mid-sentence cutoffs
            // on low-gain mics without lengthening the tail.
            self.envelope = self.envelope * 0.9 + rms * 0.1;
        }

        // Hold-window expander target for this frame: unity at or above
        // the close threshold, then (rms/threshold)^(ratio-1) below it,
        // floored. Keyed off the *instantaneous* frame RMS, not the
        // smoothed envelope — the envelope's slow release exists for
        // open/close hysteresis and would delay the expander past the
        // hold window; the per-sample smoother below handles zipper
        // safety instead.
        let expander_target = if rms >= close_threshold {
            1.0
        } else {
            let rel = (rms / close_threshold.max(1e-6)).max(1e-6);
            rel.powf(HOLD_EXPANDER_RATIO - 1.0)
                .clamp(HOLD_EXPANDER_FLOOR, 1.0)
        };

        // Per-sample state machine for smooth fades and accurate hold timing.
        for s in samples.iter_mut() {
            let coeff = if expander_target > self.expander_gain {
                EXPANDER_ATTACK
            } else {
                EXPANDER_RELEASE
            };
            self.expander_gain += coeff * (expander_target - self.expander_gain);
            match self.state {
                GateState::Closed => {
                    *s = 0.0;
                    if self.envelope >= open_threshold {
                        self.state = GateState::Opening;
                    }
                }
                GateState::Opening => {
                    self.fade_position = (self.fade_position + self.fade_step).min(1.0);
                    *s *= smoothstep(self.fade_position) * self.expander_gain;
                    if self.fade_position >= 1.0 {
                        self.state = GateState::Open;
                    } else if self.envelope < close_threshold {
                        self.state = GateState::Closing;
                    }
                }
                GateState::Open => {
                    // Unity while the envelope holds the threshold; the
                    // expander only bites once the envelope falls below.
                    *s *= self.expander_gain;
                    if self.envelope < close_threshold {
                        self.state = GateState::Holding;
                        self.hold_remaining = self.hold_samples;
                    }
                }
                GateState::Holding => {
                    *s *= self.expander_gain;
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
                    *s *= smoothstep(self.fade_position) * self.expander_gain;
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
    fn process(&mut self, samples: &mut [f32], ctx: &mut FrameContext) {
        ctx.gate_open = self.process_inner(samples);
        ctx.gate_holding = matches!(self.state, GateState::Holding);
    }

    fn reset(&mut self) {
        self.state = GateState::Closed;
        self.envelope = 0.0;
        self.fade_position = 0.0;
        self.hold_remaining = 0;
        self.expander_gain = 1.0;
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
    #[must_use]
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::processor::{AudioProcessor, FrameContext};

    /// One 10 ms frame of a ±`amp` square wave (RMS = `amp` exactly,
    /// which makes envelope math deterministic).
    fn frame(amp: f32) -> Vec<f32> {
        (0..480)
            .map(|i| if i % 2 == 0 { amp } else { -amp })
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Gate with a 0.012 threshold and 200 ms hold, driven to fully
    /// open with loud frames.
    fn opened_gate() -> (NoiseGate, FrameContext) {
        let mut gate = NoiseGate::new(Arc::new(Knobs::default()), 200);
        let mut ctx = FrameContext::default();
        for _ in 0..20 {
            let mut f = frame(0.2);
            gate.process(&mut f, &mut ctx);
        }
        assert!(ctx.gate_open && !ctx.gate_holding, "gate should be open");
        (gate, ctx)
    }

    /// Feed quiet frames until the gate reports `Holding`, then return
    /// how many frames that took. Panics if the hold never starts.
    fn drive_into_hold(gate: &mut NoiseGate, ctx: &mut FrameContext, amp: f32) -> usize {
        for n in 0..200 {
            let mut f = frame(amp);
            gate.process(&mut f, ctx);
            if ctx.gate_holding {
                return n;
            }
        }
        panic!("gate never entered the hold window");
    }

    mod hold_window_expander {
        use super::*;

        #[test]
        fn linear_floor_matches_its_db_definition() {
            let from_db = 10.0_f32.powf(HOLD_EXPANDER_FLOOR_DB / 20.0);
            assert!(
                (from_db - HOLD_EXPANDER_FLOOR).abs() < 1e-6,
                "HOLD_EXPANDER_FLOOR ({HOLD_EXPANDER_FLOOR}) drifted from {HOLD_EXPANDER_FLOOR_DB} dB ({from_db})"
            );
        }

        #[test]
        fn attenuates_the_noise_floor_during_the_hold_window() {
            let (mut gate, mut ctx) = opened_gate();
            // Noise floor: below close threshold (0.0072), above zero.
            drive_into_hold(&mut gate, &mut ctx, 0.003);
            // Let the expander release settle within the hold window.
            let mut out = frame(0.003);
            for _ in 0..5 {
                out = frame(0.003);
                gate.process(&mut out, &mut ctx);
            }
            assert!(ctx.gate_holding, "still inside the hold window");
            let gain = rms(&out) / 0.003;
            assert!(
                gain < 0.6 && gain > 0.1,
                "expected expander attenuation, got gain {gain}"
            );
        }

        #[test]
        fn deep_noise_floor_is_limited_by_the_expander_floor() {
            let (mut gate, mut ctx) = opened_gate();
            drive_into_hold(&mut gate, &mut ctx, 0.0005);
            let mut out = frame(0.0005);
            for _ in 0..8 {
                out = frame(0.0005);
                gate.process(&mut out, &mut ctx);
                if !ctx.gate_holding {
                    break;
                }
            }
            let floor = HOLD_EXPANDER_FLOOR;
            let gain = rms(&out) / 0.0005;
            assert!(
                gain >= floor * 0.8,
                "attenuation should not exceed the floor: gain {gain} vs floor {floor}"
            );
        }

        #[test]
        fn resumed_speech_returns_to_unity_within_a_frame() {
            let (mut gate, mut ctx) = opened_gate();
            drive_into_hold(&mut gate, &mut ctx, 0.003);
            for _ in 0..5 {
                let mut f = frame(0.003);
                gate.process(&mut f, &mut ctx);
            }
            // Speech comes back mid-hold.
            let mut first = frame(0.2);
            gate.process(&mut first, &mut ctx);
            let mut second = frame(0.2);
            gate.process(&mut second, &mut ctx);
            let gain = rms(&second) / 0.2;
            assert!(gain > 0.95, "speech after hold should be unity, got {gain}");
        }

        #[test]
        fn open_state_stays_unity_for_signal_above_threshold() {
            let (mut gate, mut ctx) = opened_gate();
            let mut out = frame(0.05);
            gate.process(&mut out, &mut ctx);
            let gain = rms(&out) / 0.05;
            assert!(
                (gain - 1.0).abs() < 0.01,
                "open gate should be transparent, got gain {gain}"
            );
        }

        #[test]
        fn gate_holding_flag_tracks_the_hold_window() {
            let (mut gate, mut ctx) = opened_gate();
            assert!(!ctx.gate_holding);
            drive_into_hold(&mut gate, &mut ctx, 0.003);
            assert!(ctx.gate_holding && ctx.gate_open);
        }
    }
}
