//! System audio capture (loopback) for screen sharing.
//!
//! Captures the system's audio output (what the user hears) and delivers
//! it as interleaved stereo f32 frames at 48 kHz, ready for Opus encoding.
//!
//! ## Platform support
//!
//! | Platform | Backend            | Status      |
//! |----------|--------------------|-------------|
//! | Windows  | WASAPI loopback    | ✅ Working  |
//! | macOS    | ScreenCaptureKit   | 🚧 Stub     |
//! | Linux    | PipeWire / PulseAudio | 🚧 Stub  |
//!
//! ## Architecture
//!
//! The capture runs on a dedicated thread, separate from mic input.
//! Audio flows through the same Opus encoder pipeline as voice:
//!
//! ```text
//! System audio → loopback capture → ring buffer → worker thread
//!   → interleaved stereo → resample 48kHz → Opus encode (stereo) → network
//! ```
//!
//! The worker thread is modeled after `input.rs` but simplified:
//! no noise gate, no VAD, no denoise — system audio is clean.
//!
//! ## Encoding strategy
//!
//! System audio uses a separate Opus encoder from voice chat, with
//! different settings optimized for music/game audio:
//! - 128 kbps stereo (vs 24 kbps mono for voice)
//! - `Application::Audio` (vs `Application::Voip`)
//! - No DTX (continuous transmission while sharing)
//!
//! The encoded packets are tagged with a sentinel SSRC so the receiver
//! knows to decode them with music-optimized settings and route them
//! to the screen share viewer's audio output rather than mixing with
//! voice chat.

use crate::stats::AudioStats;
use crate::OPUS_SAMPLE_RATE;

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use log::{error, info, warn};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Sentinel SSRC for system audio packets.
/// Distinct from `input::LOOPBACK_SSRC` (which is for mic monitoring).
/// Receivers use this to route system audio to the screen share viewer.
pub const SYSTEM_AUDIO_SSRC: u64 = u64::MAX - 1;

/// Opus bitrate for system audio (bits/s).
/// 128 kbps stereo gives excellent quality for music/game audio.
const SYSTEM_AUDIO_BITRATE: i32 = 128_000;

/// Number of channels for system audio capture.
const SYSTEM_AUDIO_CHANNELS: usize = 2;

/// Opus frame size for system audio (samples per channel).
/// 20ms frames (960 samples/ch at 48kHz) for better quality than
/// the 10ms voice frames — system audio doesn't need ultra-low latency.
/// The interleaved buffer size is `SYSTEM_AUDIO_FRAME_SIZE * SYSTEM_AUDIO_CHANNELS`.
const SYSTEM_AUDIO_FRAME_SIZE: usize = 960;

/// Encoded system audio packet, ready for network transport.
///
/// Mirrors `AudioPacket` but is kept separate to avoid coupling
/// the system audio pipeline to voice chat's packet format.
/// The network layer wraps this into whatever wire format it uses.
#[derive(Debug, Clone)]
pub struct SystemAudioPacket {
    /// Opus-encoded audio data (stereo).
    pub data: Vec<u8>,
    /// Sequence number for ordering and loss detection.
    pub seq: u32,
    /// Frame duration in milliseconds (always 20 for system audio).
    pub duration_ms: u8,
    /// Number of channels (always 2 for system audio).
    pub channels: u8,
}

// ─────────────────────────────────────────────────────────────────────────────
// Platform-specific capture backends
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for platform-specific system audio capture.
///
/// Each platform implements this to provide raw audio samples from
/// the system's audio output. The trait abstracts away WASAPI,
/// ScreenCaptureKit, PipeWire, etc.
///
/// Implementations push captured samples into the provided ring buffer
/// producer. The format contract:
/// - Interleaved stereo f32 samples (L, R, L, R, ...)
/// - If the device has more than 2 channels, the backend takes the first two
/// - If the device is mono, the backend duplicates to both channels
/// - At whatever sample rate the system gives us (we resample later)
///
/// The `sample_rate()` method tells us what rate to expect so we
/// can configure the resampler.
trait SystemAudioBackend {
    /// Start capturing. Spawns platform callbacks that push into the ring buffer.
    fn start(&mut self) -> Result<(), Box<dyn std::error::Error>>;

    /// Stop capturing and release platform resources.
    fn stop(&mut self);

    /// The sample rate of captured audio (for resampler configuration).
    fn sample_rate(&self) -> u32;

    /// Human-readable name of the capture source.
    fn device_name(&self) -> String;
}

// ── Windows: WASAPI loopback ─────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod wasapi_loopback {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
    use dasp_sample::{FromSample, Sample as DaspSample, ToSample};

    pub struct WasapiLoopback {
        device: cpal::Device,
        device_config: SupportedStreamConfig,
        stream: Option<Stream>,
        ring_producer: Option<Producer<f32>>,
        stats: Arc<AudioStats>,
    }

    impl WasapiLoopback {
        /// Create a WASAPI loopback capture for the default output device.
        ///
        /// On Windows, cpal exposes loopback capture through the WASAPI host.
        /// We find the default output device and open it as an input stream
        /// with the special WASAPI loopback mode.
        ///
        /// # How WASAPI loopback works
        ///
        /// WASAPI has a special "loopback" mode where you can open an output
        /// device as an input device. The audio subsystem mirrors whatever is
        /// being played to that output into your capture stream. This is:
        /// - Zero latency (same buffer the audio engine uses)
        /// - No virtual device needed (unlike macOS)
        /// - System-level (captures all apps mixed together)
        pub fn new(
            ring_producer: Producer<f32>,
            stats: Arc<AudioStats>,
        ) -> Result<Self, Box<dyn std::error::Error>> {
            // Use the WASAPI host specifically (not the default host which
            // might be ASIO or another backend that doesn't support loopback).
            let host = cpal::host_from_id(cpal::HostId::Wasapi)
                .map_err(|e| format!("WASAPI host not available: {e}"))?;

            // Get the default output device — this is what we'll capture from.
            let device = host
                .default_output_device()
                .ok_or("No default output device for loopback capture")?;

            let device_config = device
                .default_output_config()
                .map_err(|e| format!("Failed to get output config for loopback: {e}"))?;

            info!(
                "🔊🎤 System audio capture: {} @ {}Hz, {}ch, {:?}",
                device.name().unwrap_or_else(|_| "Unknown".into()),
                device_config.sample_rate().0,
                device_config.channels(),
                device_config.sample_format(),
            );

            Ok(Self {
                device,
                device_config,
                stream: None,
                ring_producer: Some(ring_producer),
                stats,
            })
        }

        fn build_stream_typed<T>(
            device: &cpal::Device,
            config: &StreamConfig,
            mut ring_producer: Producer<f32>,
            stats: Arc<AudioStats>,
            channels: usize,
        ) -> Result<Stream, Box<dyn std::error::Error>>
        where
            T: DaspSample + FromSample<f32> + ToSample<f32> + Send + 'static + cpal::SizedSample,
        {
            let stats_err = stats.clone();

            // WASAPI loopback: open the output device as a *loopback* input.
            // cpal supports this via `build_input_stream` on an output device
            // when using the WASAPI host.
            let stream = device.build_input_stream(
                config,
                move |data: &[T], _: &_| {
                    // Output interleaved stereo (L, R, L, R, ...).
                    // Handle any source channel count:
                    //   1ch → duplicate to both channels
                    //   2ch → pass through
                    //   3+ch → take first two channels
                    let ch = channels;
                    for frame in data.chunks(ch) {
                        let left: f32 = frame[0].to_sample::<f32>();
                        let right: f32 = if ch >= 2 {
                            frame[1].to_sample::<f32>()
                        } else {
                            left // mono → duplicate
                        };

                        if ring_producer.push(left).is_err() {
                            stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                            return; // ring full, bail out of this callback
                        }
                        if ring_producer.push(right).is_err() {
                            stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    }
                },
                move |err| {
                    error!("System audio capture error: {}", err);
                    stats_err.input_overruns.fetch_add(1, Ordering::Relaxed);
                },
                None,
            )?;

            Ok(stream)
        }
    }

    impl SystemAudioBackend for WasapiLoopback {
        fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            let sample_rate = self.device_config.sample_rate().0;
            let channels = self.device_config.channels();
            let format = self.device_config.sample_format();

            let config = StreamConfig {
                channels,
                sample_rate: cpal::SampleRate(sample_rate),
                buffer_size: cpal::BufferSize::Default,
            };

            let ring_producer = self
                .ring_producer
                .take()
                .ok_or("Ring producer already taken")?;

            let stream = match format {
                SampleFormat::I16 => Self::build_stream_typed::<i16>(
                    &self.device,
                    &config,
                    ring_producer,
                    self.stats.clone(),
                    channels as usize,
                )?,
                SampleFormat::U16 => Self::build_stream_typed::<u16>(
                    &self.device,
                    &config,
                    ring_producer,
                    self.stats.clone(),
                    channels as usize,
                )?,
                SampleFormat::F32 => Self::build_stream_typed::<f32>(
                    &self.device,
                    &config,
                    ring_producer,
                    self.stats.clone(),
                    channels as usize,
                )?,
                _ => {
                    return Err(format!("Unsupported sample format: {:?}", format).into());
                }
            };

            stream.play()?;
            self.stream = Some(stream);
            info!("🔊🎤 System audio capture started");
            Ok(())
        }

        fn stop(&mut self) {
            self.stream.take();
            info!("🔊🎤 System audio capture stopped");
        }

        fn sample_rate(&self) -> u32 {
            self.device_config.sample_rate().0
        }

        fn device_name(&self) -> String {
            self.device
                .name()
                .unwrap_or_else(|_| "Unknown WASAPI device".into())
        }
    }
}

// ── macOS: stub (ScreenCaptureKit needed) ────────────────────────────────────

#[cfg(target_os = "macos")]
mod coreaudio_loopback {
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
}

// ── Linux: stub (PipeWire/PulseAudio needed) ─────────────────────────────────

#[cfg(target_os = "linux")]
mod linux_loopback {
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// System audio capturer for screen sharing.
///
/// Captures the system's audio output, resamples to 48 kHz stereo,
/// and Opus-encodes it into packets for network transport.
///
/// # Usage
///
/// ```rust,no_run
/// use core::stream::input::SystemAudioCapture;
/// use core::stats::AudioStats;
/// use std::sync::Arc;
///
/// let stats = Arc::new(AudioStats::new());
/// let mut capture = SystemAudioCapture::new(stats)?;
/// capture.start()?;
///
/// // Poll for encoded packets (non-blocking)
/// while let Some(packet) = capture.try_recv() {
///     // Send packet over network tagged with SYSTEM_AUDIO_SSRC
///     network.send_system_audio(packet);
/// }
///
/// // Stop when screen sharing ends
/// capture.stop();
/// ```
pub struct SystemAudioCapture {
    /// Worker thread that reads from ring buffer, resamples, encodes.
    worker_thread: Option<thread::JoinHandle<()>>,

    /// Encoded packets ready for network transport.
    packet_rx: Receiver<SystemAudioPacket>,

    /// Signal to stop worker thread.
    should_stop: Arc<AtomicBool>,

    /// Whether capture is currently active.
    is_active: Arc<AtomicBool>,

    /// Mute system audio capture without stopping the pipeline.
    /// When muted, the worker thread drains the ring buffer but doesn't encode.
    is_muted: Arc<AtomicBool>,

    /// Capture volume (f32 bits, 0.0–2.0). Applied before encoding.
    volume: Arc<AtomicU32>,

    /// Stats (shared with voice chat engine for unified reporting).
    stats: Arc<AudioStats>,

    /// Platform backend handle — kept alive to maintain capture stream.
    #[cfg(target_os = "windows")]
    backend: Option<wasapi_loopback::WasapiLoopback>,
    #[cfg(target_os = "macos")]
    backend: Option<coreaudio_loopback::CoreAudioLoopback>,
    #[cfg(target_os = "linux")]
    backend: Option<linux_loopback::LinuxLoopback>,
}

impl SystemAudioCapture {
    /// Create a new system audio capturer.
    ///
    /// Does not start capturing — call `start()` when screen sharing begins.
    pub fn new(stats: Arc<AudioStats>) -> Result<Self, Box<dyn std::error::Error>> {
        // packet_rx is created as a placeholder — start() replaces it
        // with a real channel connected to the worker thread.
        let (_dead_tx, packet_rx) = bounded(1);

        Ok(Self {
            worker_thread: None,
            packet_rx,
            should_stop: Arc::new(AtomicBool::new(false)),
            is_active: Arc::new(AtomicBool::new(false)),
            is_muted: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            stats,
            backend: None,
        })
    }

    /// Start capturing system audio.
    ///
    /// Creates the platform backend, ring buffer, and worker thread.
    /// Idempotent — calling twice is a no-op.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.is_active.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Ring buffer: 200ms at device rate, stereo interleaved (2x mono size).
        // Sized generously because WASAPI can deliver in large chunks.
        let (ring_producer, ring_consumer) = RingBuffer::new(48000 * 2 * 200 / 1000);

        // Create platform backend
        #[cfg(target_os = "windows")]
        let mut backend = wasapi_loopback::WasapiLoopback::new(ring_producer, self.stats.clone())?;
        #[cfg(target_os = "macos")]
        let mut backend =
            coreaudio_loopback::CoreAudioLoopback::new(ring_producer, self.stats.clone())?;
        #[cfg(target_os = "linux")]
        let mut backend = linux_loopback::LinuxLoopback::new(ring_producer, self.stats.clone())?;

        let device_sample_rate = backend.sample_rate();
        let device_name = backend.device_name();

        // Start the platform capture stream
        backend.start()?;

        // Create the encoded packet channel
        let (packet_tx, packet_rx) = bounded(8);
        self.packet_rx = packet_rx;

        // Spawn the encoder worker thread
        let should_stop = self.should_stop.clone();
        let is_muted = self.is_muted.clone();
        let volume = self.volume.clone();
        let stats = self.stats.clone();

        self.should_stop.store(false, Ordering::Relaxed);

        let worker = Self::spawn_worker(
            ring_consumer,
            packet_tx,
            should_stop,
            is_muted,
            volume,
            stats,
            device_sample_rate,
        );

        self.worker_thread = Some(worker);
        self.backend = Some(backend);
        self.is_active.store(true, Ordering::Relaxed);

        info!(
            "🔊🎤 System audio capture ready: {} @ {}Hz",
            device_name, device_sample_rate
        );

        Ok(())
    }

    /// Stop capturing system audio and release all resources.
    pub fn stop(&mut self) {
        self.should_stop.store(true, Ordering::Release);

        // Stop the platform backend first (stops callbacks pushing to ring buffer)
        if let Some(mut backend) = self.backend.take() {
            backend.stop();
        }

        // Join the worker thread
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }

        self.is_active.store(false, Ordering::Relaxed);
        info!("🔊🎤 System audio capture stopped");
    }

    /// Try to receive an encoded system audio packet (non-blocking).
    ///
    /// Returns `None` if no packet is ready. The caller should poll
    /// this in the same loop that handles screen share video frames.
    pub fn try_recv(&self) -> Option<SystemAudioPacket> {
        self.packet_rx.try_recv().ok()
    }

    /// Drain all available encoded packets into a vec.
    ///
    /// Useful when the caller wants to batch packets for network send.
    pub fn drain_packets(&self) -> Vec<SystemAudioPacket> {
        let mut packets = Vec::new();
        while let Ok(pkt) = self.packet_rx.try_recv() {
            packets.push(pkt);
        }
        packets
    }

    /// Whether system audio capture is currently active.
    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Relaxed)
    }

    /// Mute/unmute system audio capture.
    ///
    /// When muted, the capture stream continues running but audio is
    /// discarded before encoding. This avoids the startup latency of
    /// stopping and restarting WASAPI.
    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.is_muted.load(Ordering::Relaxed)
    }

    /// Set the capture volume (0.0–2.0).
    ///
    /// Applied before Opus encoding. 1.0 is passthrough.
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
    }

    // ── Worker thread ────────────────────────────────────────────────────

    fn spawn_worker(
        ring_consumer: Consumer<f32>,
        packet_tx: Sender<SystemAudioPacket>,
        should_stop: Arc<AtomicBool>,
        is_muted: Arc<AtomicBool>,
        volume: Arc<AtomicU32>,
        stats: Arc<AudioStats>,
        device_sample_rate: u32,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            Self::worker_loop(
                ring_consumer,
                packet_tx,
                should_stop,
                is_muted,
                volume,
                stats,
                device_sample_rate,
            );
        })
    }

    fn worker_loop(
        mut ring_consumer: Consumer<f32>,
        packet_tx: Sender<SystemAudioPacket>,
        should_stop: Arc<AtomicBool>,
        is_muted: Arc<AtomicBool>,
        volume: Arc<AtomicU32>,
        _stats: Arc<AudioStats>,
        device_sample_rate: u32,
    ) {
        // ── Opus encoder (stereo, music-optimized) ──────────────────────
        let mut encoder = match opus::Encoder::new(
            OPUS_SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio, // not Voip — optimized for music/game audio
        ) {
            Ok(enc) => enc,
            Err(e) => {
                error!("Failed to create system audio Opus encoder: {e}");
                return;
            }
        };

        if let Err(e) = encoder.set_bitrate(opus::Bitrate::Bits(SYSTEM_AUDIO_BITRATE)) {
            warn!("Failed to set system audio bitrate: {e}");
        }

        // Disable DTX — system audio should transmit continuously while
        // screen sharing is active (unlike voice which uses VAD).
        if let Err(e) = encoder.set_dtx(false) {
            warn!("Failed to disable DTX for system audio: {e}");
        }

        // ── Resampler (stereo, if device rate ≠ 48 kHz) ────────────────
        // rubato operates on deinterleaved channels: Vec<Vec<f32>> where
        // [0] = left, [1] = right. We deinterleave before resampling,
        // then re-interleave after for Opus.
        let needs_resample = device_sample_rate != OPUS_SAMPLE_RATE;
        let mut resampler: Option<Async<f32>> = None;
        let mut resampler_input: Vec<Vec<f32>> = Vec::new();
        let mut resampler_output: Vec<Vec<f32>> = Vec::new();
        let mut resampler_accum_l: Vec<f32> = Vec::with_capacity(4096);
        let mut resampler_accum_r: Vec<f32> = Vec::with_capacity(4096);

        if needs_resample {
            let chunk = crate::resampler::frames_for_ms(device_sample_rate, 20);
            let ratio = OPUS_SAMPLE_RATE as f64 / device_sample_rate as f64;
            let params = SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            match Async::<f32>::new_sinc(
                ratio,
                1.0, // max_resample_ratio_relative
                &params,
                chunk,
                SYSTEM_AUDIO_CHANNELS, // 2 channels
                FixedAsync::Input,
            ) {
                Ok(r) => {
                    let in_frames = r.input_frames_next();
                    let out_max = r.output_frames_max();
                    resampler_input = vec![
                        Vec::with_capacity(in_frames + 64),
                        Vec::with_capacity(in_frames + 64),
                    ];
                    resampler_output = vec![vec![0.0f32; out_max], vec![0.0f32; out_max]];
                    resampler = Some(r);
                }
                Err(e) => {
                    error!("Failed to create system audio stereo resampler: {e}");
                    return;
                }
            }
        }

        // ── Processing state ────────────────────────────────────────────
        // Ring buffer contains interleaved stereo: L R L R ...
        let mut read_buf = vec![0.0f32; 8192];
        // Accumulator holds interleaved stereo at 48kHz
        let mut accumulator: Vec<f32> =
            Vec::with_capacity(SYSTEM_AUDIO_FRAME_SIZE * SYSTEM_AUDIO_CHANNELS * 4);
        let mut encode_buf = vec![0u8; 4096]; // Opus output buffer
        let mut seq: u32 = 0;

        // Interleaved frame size (what Opus expects)
        let interleaved_frame_size = SYSTEM_AUDIO_FRAME_SIZE * SYSTEM_AUDIO_CHANNELS;

        info!(
            "🔊🎤 System audio worker started (stereo, device {}Hz, resample={})",
            device_sample_rate, needs_resample
        );

        while !should_stop.load(Ordering::Relaxed) {
            // ── Mute: drain ring buffer but don't encode ────────────────
            if is_muted.load(Ordering::Relaxed) {
                while ring_consumer.pop().is_ok() {}
                thread::sleep(Duration::from_millis(10));
                accumulator.clear();
                resampler_accum_l.clear();
                resampler_accum_r.clear();
                continue;
            }

            // ── Read interleaved stereo from ring buffer ────────────────
            let target_interleaved = if let Some(ref r) = resampler {
                r.input_frames_next() * SYSTEM_AUDIO_CHANNELS
            } else {
                interleaved_frame_size
            };

            let mut n_read = 0;
            for i in 0..target_interleaved.min(read_buf.len()) {
                match ring_consumer.pop() {
                    Ok(s) => {
                        read_buf[i] = s;
                        n_read += 1;
                    }
                    Err(_) => break,
                }
            }

            if n_read == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }

            // Ensure we have complete stereo pairs
            let n_read = n_read - (n_read % SYSTEM_AUDIO_CHANNELS);
            if n_read == 0 {
                continue;
            }

            // ── Resample to 48 kHz stereo if needed ─────────────────────
            let at_48k_interleaved: Vec<f32> = if let Some(ref mut r) = resampler {
                // Deinterleave into per-channel accumulators
                for pair in read_buf[..n_read].chunks_exact(2) {
                    resampler_accum_l.push(pair[0]);
                    resampler_accum_r.push(pair[1]);
                }

                let needed = r.input_frames_next();
                if resampler_accum_l.len() < needed {
                    continue; // not enough samples yet
                }

                // Feed the resampler
                resampler_input[0].clear();
                resampler_input[0].extend_from_slice(&resampler_accum_l[..needed]);
                resampler_input[1].clear();
                resampler_input[1].extend_from_slice(&resampler_accum_r[..needed]);
                resampler_accum_l.drain(..needed);
                resampler_accum_r.drain(..needed);

                let expected = r.output_frames_next();
                resampler_output[0].resize(expected, 0.0);
                resampler_output[1].resize(expected, 0.0);

                let in_adapter = match SequentialSliceOfVecs::new(&resampler_input, 2, needed) {
                    Ok(a) => a,
                    Err(e) => {
                        error!("System audio resampler input adapter error: {e}");
                        continue;
                    }
                };
                let mut out_adapter =
                    match SequentialSliceOfVecs::new_mut(&mut resampler_output, 2, expected) {
                        Ok(a) => a,
                        Err(e) => {
                            error!("System audio resampler output adapter error: {e}");
                            continue;
                        }
                    };

                match r.process_into_buffer(&in_adapter, &mut out_adapter, None) {
                    Ok((_in_len, out_len)) if out_len > 0 => {
                        // Re-interleave: L0 R0 L1 R1 ...
                        let mut interleaved = Vec::with_capacity(out_len * 2);
                        for i in 0..out_len {
                            interleaved.push(resampler_output[0][i]);
                            interleaved.push(resampler_output[1][i]);
                        }
                        interleaved
                    }
                    Ok(_) => Vec::new(),
                    Err(e) => {
                        error!("System audio resampler error: {e}");
                        Vec::new()
                    }
                }
            } else {
                read_buf[..n_read].to_vec()
            };

            if at_48k_interleaved.is_empty() {
                continue;
            }

            // ── Accumulate into frame-sized chunks ──────────────────────
            // interleaved_frame_size = 960 * 2 = 1920 interleaved samples
            accumulator.extend_from_slice(&at_48k_interleaved);

            while accumulator.len() >= interleaved_frame_size {
                let mut frame: Vec<f32> = accumulator.drain(..interleaved_frame_size).collect();

                // ── Apply volume ────────────────────────────────────────
                let vol = f32::from_bits(volume.load(Ordering::Relaxed));
                if (vol - 1.0).abs() > f32::EPSILON {
                    for s in frame.iter_mut() {
                        *s = (*s * vol).clamp(-0.99, 0.99);
                    }
                }

                // ── Opus encode (stereo interleaved) ────────────────────
                // opus::Encoder::encode_float expects interleaved stereo
                // with `frame_size` = samples per channel (960).
                match encoder.encode_float(&frame, &mut encode_buf) {
                    Ok(len) => {
                        let packet = SystemAudioPacket {
                            data: encode_buf[..len].to_vec(),
                            seq,
                            duration_ms: 20,
                            channels: SYSTEM_AUDIO_CHANNELS as u8,
                        };
                        seq = seq.wrapping_add(1);

                        match packet_tx.try_send(packet) {
                            Ok(_) => {}
                            Err(TrySendError::Full(_)) => {
                                // Consumer is behind — drop this frame.
                                // System audio is less critical than voice;
                                // dropping a frame causes a barely audible
                                // 20ms gap in background music.
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                info!("System audio packet channel disconnected, stopping worker");
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        error!("System audio Opus encode error: {e}");
                    }
                }
            }
        }

        info!("🔊🎤 System audio worker stopped");
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Check if system audio capture is supported on this platform.
///
/// Returns a human-readable reason if not supported.
pub fn is_supported() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Err("System audio capture on macOS requires ScreenCaptureKit integration (not yet implemented). \
             See core::stream::input::coreaudio_loopback module docs for the implementation plan.".into())
    }
    #[cfg(target_os = "linux")]
    {
        Err("System audio capture on Linux requires PipeWire or PulseAudio monitor integration (not yet implemented). \
             See core::stream::input::linux_loopback module docs for the implementation plan.".into())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err("System audio capture is not supported on this platform.".into())
    }
}
