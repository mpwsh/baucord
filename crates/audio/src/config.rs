//! Configuration types for the voice chat engine.
//!
//! Simple, flat configuration. Everything has sane defaults.
//! Users pick devices by name, we handle sample rates and channels automatically.

/// Top-level configuration for [`VoiceChat`](crate::VoiceChat).
///
/// Only expose what users actually need to decide. Sample rates, channel
/// mapping, buffer sizes, and codec settings are all handled internally.
#[derive(Debug, Clone)]
pub struct VoiceChatConfig {
    /// Input device name, or `None` for system default.
    pub input_device: Option<String>,
    /// Output device name, or `None` for system default.
    pub output_device: Option<String>,

    /// Enable RNNoise-based noise suppression. Default: `true`.
    ///
    /// When enabled, background noise (fans, HVAC, keyboard, etc.) is
    /// removed before the gate and encoding. This also reduces false
    /// gate activations.
    /// Toggle at runtime via [`VoiceChat::set_noise_suppression`](crate::VoiceChat::set_noise_suppression).
    pub noise_suppression: bool,

    /// Master input gain multiplier (0.0–2.0). Default: `1.0`.
    ///
    /// Applied after the gate, before encoding.
    /// Use this for mic boost or attenuation.
    /// Adjustable at runtime via [`VoiceChat::set_input_gain`](crate::VoiceChat::set_input_gain).
    pub input_gain: f32,

    /// Master output gain multiplier (0.0–2.0). Default: `1.0`.
    ///
    /// Applied after jitter buffer mixing, before playback.
    /// This is the "master volume" control.
    /// Adjustable at runtime via [`VoiceChat::set_output_gain`](crate::VoiceChat::set_output_gain).
    pub output_gain: f32,

    /// Noise gate / activation threshold (RMS, 0.001–0.5). Default: `0.012`.
    ///
    /// This is the **primary voice activation control**. Audio below this
    /// level is silenced and never transmitted. When the gate is closed,
    /// no packets are sent regardless of what the speech classifier thinks.
    ///
    /// The threshold operates on the post-denoise, pre-gain signal, so it
    /// responds to the user's actual voice level at the mic, unaffected by
    /// the input gain setting.
    ///
    /// Good starting points:
    /// - `0.005` (~-46 dBFS): Very sensitive — picks up whispers
    /// - `0.010` (~-40 dBFS): High sensitivity — quiet environments
    /// - `0.020` (~-34 dBFS): Default — balanced for most situations
    /// - `0.040` (~-28 dBFS): Medium — requires clear speech
    /// - `0.080` (~-22 dBFS): Low sensitivity — only loud speech passes
    ///
    /// Adjustable at runtime via [`VoiceChat::set_gate_threshold`](crate::VoiceChat::set_gate_threshold).
    pub gate_threshold: f32,
    /// Enable dynamic range compression. Default: `true`.
    ///
    /// Tames loud peaks (shouts, mic bumps) so they don't sound harsh,
    /// especially when multiple people talk at once.
    pub compressor_enabled: bool,
    /// Enable automatic gain control. Default: `true`.
    /// When enabled, mic input is automatically normalized to a consistent
    /// level regardless of mic sensitivity or distance.
    pub agc_enabled: bool,

    /// AGC target loudness (RMS, 0.01–0.5). Default: `0.10`.
    /// Higher = louder output. 0.10 ≈ -20 dBFS, good for voice chat.
    pub agc_target_rms: f32,

    /// Echo cancellation (`AEC-SPEC.md`): when to open the streams on
    /// the platform's voice-call path. Default: [`AecMode::Auto`].
    /// Changed at runtime via
    /// [`VoiceChat::set_echo_cancellation`](crate::VoiceChat::set_echo_cancellation).
    pub echo_cancellation: crate::aec::AecMode,
}

impl Default for VoiceChatConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            noise_suppression: true,
            input_gain: 1.0,
            output_gain: 0.4,
            gate_threshold: 0.012,
            compressor_enabled: true,
            agc_enabled: true,
            agc_target_rms: 0.10,
            echo_cancellation: crate::aec::AecMode::Auto,
        }
    }
}

/// Per-peer audio settings (volume, mute).
///
/// Used by the jitter buffer / mixer on the output side.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PeerAudioSettings {
    /// Playback volume multiplier (0.0–2.0). Default 1.0.
    pub volume: f32,
    /// Whether this peer is muted locally.
    pub is_muted: bool,
}

impl Default for PeerAudioSettings {
    fn default() -> Self {
        Self {
            volume: 1.0,
            is_muted: false,
        }
    }
}
