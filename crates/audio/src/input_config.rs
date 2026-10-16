//! Shared input pipeline configuration — the "macro rack".
//!
//! One `Arc<InputConfig>` is shared between the `AudioInput` control
//! surface (UI thread) and the worker thread. All fields are atomic
//! for lock-free access.
//!
//! The gate's threshold — the one processor setting the user turns —
//! lives in [`gate::Knobs`] behind an `Arc` the gate holds and reads
//! every frame. The others are fixed when the chain is built: the
//! compressor's [`compressor::Params`], the AGC's
//! [`agc::TARGET_RMS`] (review 2026-10, P2-50).

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use crate::processor::gate;
#[cfg(doc)]
use crate::processor::{agc, compressor};

/// Shared, lock-free configuration for the entire input pipeline.
///
/// Created once in [`AudioInput::new`](crate::input::AudioInput::new),
/// wrapped in `Arc`, and shared with the worker thread. The worker
/// reads from this every frame to populate [`FrameContext`](crate::processor::FrameContext).
/// The UI thread writes to it via `AudioInput`'s setter methods.
///
/// The gate's knobs are behind their own `Arc` so it can hold a clone
/// without borrowing `InputConfig` itself.
pub struct InputConfig {
    // ── Pipeline toggles ─────────────────────────────────────────────
    /// Master mute — when true, the cpal callback discards samples
    /// and the worker drains the ring buffer.
    pub muted: AtomicBool,

    /// In a voice room. Out of one (the lobby, the AFK room) the relay
    /// drops our voice, so the desktop treats it like mute: capture
    /// stays open (PRODUCTION-PLAN.md §6, item 5) and its samples are
    /// dropped before anything denoises, encodes or sends them (review
    /// P1-6). The phones close the microphone instead
    /// (`VoiceChat::set_presence`).
    pub in_room: AtomicBool,

    /// Someone can hear what is sent: off for a microphone test, the
    /// Settings meter and a room with nobody else in it. The chain
    /// still runs — the speaking light, the meters, the test's own
    /// playback — but no frame reaches the encoder: it encoded 50 a
    /// second for nobody (review 2026-10, P2-37).
    pub transmit: AtomicBool,

    /// Enable loopback monitoring (hear yourself through the output).
    pub loopback_enabled: AtomicBool,

    // ── Pipeline-level knobs ─────────────────────────────────────────
    /// Master input gain multiplier (f32 bits, 0.0–2.0).
    pub input_gain: AtomicU32,

    // ── Per-processor knob panels ────────────────────────────────────
    /// Noise gate parameters.
    pub gate: Arc<gate::Knobs>,
}

impl InputConfig {
    /// Create with explicit initial values (from [`VoiceChatConfig`]).
    #[must_use]
    pub fn new(input_gain: f32, gate_threshold: f32) -> Self {
        Self {
            muted: AtomicBool::new(false),
            in_room: AtomicBool::new(true),
            transmit: AtomicBool::new(true),
            loopback_enabled: AtomicBool::new(false),
            input_gain: AtomicU32::new(input_gain.clamp(0.0, 2.0).to_bits()),
            gate: Arc::new(gate::Knobs::with_threshold(gate_threshold)),
        }
    }

    /// The samples are dropped: muted, or out of a voice room. Read by
    /// the capture callbacks and the capture worker.
    #[inline]
    pub fn silenced(&self) -> bool {
        self.muted.load(Ordering::Relaxed) || !self.in_room.load(Ordering::Relaxed)
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self::new(1.0, 0.012)
    }
}
