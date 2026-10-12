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

use crossbeam_channel::{bounded, Receiver, Sender};

use super::{AudioProcessor, FrameContext};

/// `RNNoise` speech probability at or above which a frame is speech.
const VAD_THRESHOLD: f32 = 0.5;

/// After speech stops, keep `is_speech` true for this many frames so the
/// indicator doesn't flicker on syllable boundaries. 8 × 20 ms = 160 ms.
const HOLD_FRAMES: u32 = 8;

/// UI-side smoother: don't drop the "speaking" light for at least this
/// long after speech stops. Purely visual.
const STOP_COOLDOWN: Duration = Duration::from_millis(1500);

/// The speaking light, shared by all that set or read it: the capture
/// worker's [`SpeakingIndicator`], the engine (mute, presence and a
/// closed microphone put it out) and the engine's owner. Each change
/// rings [`Self::new`]'s receiver, so the owner waits for an edge
/// instead of polling the flag every 20 ms (review P1-5).
pub struct SpeakingFlag {
    on: AtomicBool,
    edges: Sender<()>,
}

impl SpeakingFlag {
    /// A flag, out, and the ring its changes sound on. The ring holds
    /// one token: any number of changes since the last look read as one
    /// ring, and the flag says where they ended.
    #[must_use]
    pub fn new() -> (Arc<Self>, Receiver<()>) {
        let (edges, ring) = bounded(1);
        let flag = Self {
            on: AtomicBool::new(false),
            edges,
        };
        (Arc::new(flag), ring)
    }

    #[must_use]
    pub fn get(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Set the light; a change rings. Never blocks.
    pub fn set(&self, on: bool) {
        if self.on.swap(on, Ordering::Relaxed) != on {
            let _ = self.edges.try_send(());
        }
    }
}

pub struct SpeakingIndicator {
    is_speaking: Arc<SpeakingFlag>,
    hold_remaining: u32,
    last_transition: Instant,
}

impl Default for SpeakingIndicator {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeakingIndicator {
    /// An indicator with a flag of its own, which nobody hears ring.
    #[must_use]
    pub fn new() -> Self {
        Self::with_flag(SpeakingFlag::new().0)
    }

    /// An indicator that drives `flag`.
    #[must_use]
    pub fn with_flag(flag: Arc<SpeakingFlag>) -> Self {
        Self {
            is_speaking: flag,
            hold_remaining: 0,
            last_transition: Instant::now(),
        }
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
        let was_speaking = self.is_speaking.get();
        if should_speak && !was_speaking {
            self.is_speaking.set(true);
            self.last_transition = Instant::now();
        } else if !should_speak && was_speaking && self.last_transition.elapsed() > STOP_COOLDOWN {
            self.is_speaking.set(false);
            self.last_transition = Instant::now();
        }
    }
}

impl AudioProcessor for SpeakingIndicator {
    fn process(&mut self, _samples: &mut [f32], ctx: &mut FrameContext) {
        let is_speech = self.classify(ctx.gate_open, ctx.rnnoise_vad_prob);
        ctx.is_speech = is_speech;
        self.update_ui_indicator(is_speech);
    }

    fn reset(&mut self) {
        self.hold_remaining = 0;
        self.is_speaking.set(false);
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

    /// The owner hears each change once, never a set that changed
    /// nothing, and changes it has not looked at yet read as one ring.
    #[test]
    fn the_flag_rings_on_a_change_only() {
        let (flag, ring) = SpeakingFlag::new();
        flag.set(false);
        assert!(ring.try_recv().is_err(), "nothing changed");
        flag.set(true);
        assert!(ring.try_recv().is_ok() && flag.get());
        flag.set(true);
        assert!(ring.try_recv().is_err(), "still on: no ring");
        flag.set(false);
        flag.set(true);
        flag.set(false);
        assert!(ring.try_recv().is_ok(), "three changes, one ring");
        assert!(ring.try_recv().is_err());
        assert!(!flag.get(), "the flag says where they ended");

        // The indicator drives the shared flag.
        let mut s = SpeakingIndicator::with_flag(flag.clone());
        let mut ctx = FrameContext {
            gate_open: true,
            rnnoise_vad_prob: 0.9,
            ..FrameContext::default()
        };
        s.process(&mut [], &mut ctx);
        assert!(flag.get() && ring.try_recv().is_ok());
    }
}
