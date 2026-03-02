use super::*;

/// Linux system audio capture placeholder.
///
/// # Implementation plan
///
/// Linux has two viable approaches, both well-supported:
///
/// 1. **PipeWire** (modern distros, 2022+): Use the PipeWire API to
///    create a capture stream connected to the default audio sink's
///    monitor port. PipeWire is the successor to PulseAudio and is
///    default on Fedora, Ubuntu 22.10+, Arch, etc.
///    Pro: Modern, well-documented, supports per-app capture.
///    Con: Not available on older distros.
///
/// 2. **PulseAudio monitor**: Every PulseAudio sink has a `.monitor`
///    source that mirrors its output. cpal can capture from it if we
///    find the monitor device name (e.g., `alsa_output.pci-xxx.monitor`).
///    Pro: Works with existing cpal code, just need the right device name.
///    Con: PulseAudio-specific, may not work on PipeWire's PulseAudio compat layer.
///
/// 3. **cpal + PulseAudio monitor (simplest)**: Enumerate cpal input
///    devices, find one whose name contains ".monitor". This is the
///    PulseAudio loopback source. Open it like any other input device.
///    Pro: Dead simple, uses existing cpal infrastructure.
///    Con: Fragile device name matching.
///
/// Recommendation: Start with #3 (cpal + PulseAudio monitor) since it
/// requires zero new dependencies. Fall back to #1 (PipeWire native)
/// for per-app capture and Wayland compatibility.
pub struct LinuxLoopback;

impl LinuxLoopback {
    pub fn new(
        _ring_producer: Producer<f32>,
        _stats: Arc<AudioStats>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Err("System audio capture not yet implemented on Linux. \
                 PulseAudio monitor / PipeWire integration planned — see module docs."
            .into())
    }
}

impl SystemAudioBackend for LinuxLoopback {
    fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        Err("Not implemented".into())
    }
    fn stop(&mut self) {}
    fn sample_rate(&self) -> u32 {
        48000
    }
    fn device_name(&self) -> String {
        "Linux system audio (not implemented)".into()
    }
}
