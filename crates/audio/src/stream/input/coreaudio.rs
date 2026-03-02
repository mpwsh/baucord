use super::*;

/// macOS system audio capture placeholder.
///
/// # Implementation plan
///
/// macOS doesn't support loopback capture through CoreAudio directly.
/// Options, in order of preference:
///
/// 1. **ScreenCaptureKit** (macOS 12.3+): `SCStreamConfiguration` with
///    `capturesAudio = true` captures system audio alongside screen.
///    This is the blessed Apple API and requires no virtual devices.
///    Audio arrives as `CMSampleBuffer` in the stream delegate.
///    Pro: No user setup, no permissions beyond screen recording.
///    Con: Requires macOS 12.3+, Objective-C/Swift bridge.
///
/// 2. **Virtual audio device**: BlackHole, Soundflower, or similar.
///    User installs a virtual loopback device, we capture from it via cpal.
///    Pro: Works with existing cpal code.
///    Con: Requires user to install and configure third-party software.
///
/// 3. **Aggregate device**: Create a multi-output device programmatically
///    via CoreAudio's AudioHardware API that mirrors the default output.
///    Pro: No third-party software.
///    Con: Complex CoreAudio HAL code, may require user permission.
///
/// Recommendation: ScreenCaptureKit (#1). It's the same API we'd want
/// for screen capture anyway (replacing xcap), and Apple explicitly
/// designed it for this use case. The audio and video are already
/// synchronized, which solves A/V sync for free.
pub struct CoreAudioLoopback;

impl CoreAudioLoopback {
    pub fn new(
        _ring_producer: Producer<f32>,
        _stats: Arc<AudioStats>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Err("System audio capture not yet implemented on macOS. \
                 ScreenCaptureKit integration planned — see module docs."
            .into())
    }
}

impl SystemAudioBackend for CoreAudioLoopback {
    fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        Err("Not implemented".into())
    }
    fn stop(&mut self) {}
    fn sample_rate(&self) -> u32 {
        48000
    }
    fn device_name(&self) -> String {
        "macOS system audio (not implemented)".into()
    }
}
