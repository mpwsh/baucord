//! Shared input pipeline configuration — the "macro rack".
//!
//! One `Arc<InputConfig>` is shared between the `AudioInput` control
//! surface (UI thread) and the worker thread. All fields are atomic
//! for lock-free access.
//!
//! Each processor's tuneable parameters live in its own `Knobs` struct
//! (e.g., [`gate::Knobs`], [`compressor::Knobs`]). `InputConfig` holds
//! `Arc`s to each, so processors can clone the `Arc` to their own knobs
//! when they're constructed and then read from them every frame —
//! same as an Ableton macro rack mapping knobs from individual devices
//! onto a single control panel.

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use crate::processor::{agc, compressor, gate};

/// Shared, lock-free configuration for the entire input pipeline.
///
/// Created once in [`AudioInput::new`](crate::input::AudioInput::new),
/// wrapped in `Arc`, and shared with the worker thread. The worker
/// reads from this every frame to populate [`FrameContext`](crate::processor::FrameContext).
/// The UI thread writes to it via `AudioInput`'s setter methods.
///
/// Per-processor knob panels are behind their own `Arc`s so processors
/// can hold a clone without borrowing `InputConfig` itself.
pub struct InputConfig {
    // ── Pipeline toggles ─────────────────────────────────────────────
    /// Master mute — when true, the cpal callback discards samples
    /// and the worker drains the ring buffer.
    pub muted: AtomicBool,

    /// Enable RNNoise-based noise suppression.
    pub denoise_enabled: AtomicBool,

    /// Enable dynamic range compressor.
    pub compressor_enabled: AtomicBool,

    /// Enable automatic gain control.
    pub agc_enabled: AtomicBool,

    /// Enable loopback monitoring (hear yourself through the output).
    pub loopback_enabled: AtomicBool,

    // ── Pipeline-level knobs ─────────────────────────────────────────
    /// Master input gain multiplier (f32 bits, 0.0–2.0).
    pub input_gain: AtomicU32,

    // ── Per-processor knob panels ────────────────────────────────────
    /// Noise gate parameters.
    pub gate: Arc<gate::Knobs>,

    /// Compressor parameters.
    pub compressor: Arc<compressor::Knobs>,

    /// AGC parameters.
    pub agc: Arc<agc::Knobs>,
}

impl InputConfig {
    /// Create with explicit initial values (from [`VoiceChatConfig`]).
    pub fn new(
        noise_suppression: bool,
        input_gain: f32,
        gate_threshold: f32,
        compressor_enabled: bool,
        agc_enabled: bool,
        agc_target_rms: f32,
    ) -> Self {
        Self {
            muted: AtomicBool::new(false),
            denoise_enabled: AtomicBool::new(noise_suppression),
            compressor_enabled: AtomicBool::new(compressor_enabled),
            agc_enabled: AtomicBool::new(agc_enabled),
            loopback_enabled: AtomicBool::new(false),
            input_gain: AtomicU32::new(input_gain.clamp(0.0, 2.0).to_bits()),
            gate: Arc::new(gate::Knobs::with_threshold(gate_threshold)),
            compressor: Arc::new(compressor::Knobs::default()),
            agc: Arc::new(agc::Knobs::with_target(agc_target_rms)),
        }
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self::new(true, 1.0, 0.012, true, true, 0.10)
    }
}