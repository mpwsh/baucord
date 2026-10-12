//! System audio capture (loopback) for screen sharing.
//!
//! Captures the system's audio output (what the user hears) and delivers
//! it as interleaved stereo f32 frames at 48 kHz, ready for Opus encoding.
//!
//! ## Platform support
//!
//! Windows only: WASAPI process loopback (`stream::process_loopback`),
//! Windows 10 2004 and later. Everywhere else [`SystemAudioCapture::start`]
//! fails and the share goes out without sound — there is no backend
//! for macOS (it would be `ScreenCaptureKit`), Linux (`PipeWire`) or the
//! phones.
//!
//! ## Own-audio exclusion (echo prevention)
//!
//! The stream must not contain what *we* play — above all the decoded
//! peer voices — or every viewer hears themselves echoed back roughly
//! one stream-latency later. That is solved at capture time: the
//! backend asks WASAPI for the system mix *minus our own process tree*.
//! (An endpoint-loopback fallback for older Windows 10 builds captured
//! everything, peers' voices included, and nothing downstream removed
//! them; it is gone.) A backend for another platform follows the same
//! rule: exclude at capture time
//! (`SCStreamConfiguration.excludesCurrentProcessAudio`, `PipeWire`
//! routing), never filter after the fact.
//!
//! ## Footprint (silence)
//!
//! Silence costs ~nothing end to end: the process-loopback backend
//! contributes no samples for silent capture (SILENT-flagged buffers are
//! dropped), the worker parks on a wake latch instead of polling the
//! ring (the 2026-08 audit measured ~500 wakes/s from the 2 ms polling
//! sleep this replaced), and the silence gate stops Opus encoding and
//! packet output when nothing audible is playing.
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
//! no VAD, no denoise — system audio is clean. The one gate is a cheap
//! *silence* gate that stops encoding and packet output entirely when
//! nothing audible is playing.
//!
//! ## Encoding strategy
//!
//! System audio uses a separate Opus encoder from voice chat, with
//! different settings optimized for music/game audio:
//! - 128 kbps stereo (vs 48 kbps mono for voice)
//! - `Application::Audio` (vs `Application::Voip`)
//! - No DTX — the worker-level silence gate replaces it, stopping
//!   packets entirely during silence (seq only advances on sent
//!   packets, so receivers see silence, not loss)
//!
//! The encoded packets are tagged with a sentinel SSRC so the receiver
//! knows to decode them with music-optimized settings and route them
//! to the screen share viewer's audio output rather than mixing with
//! voice chat.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use log::{debug, error, info, warn};
use rtrb::Consumer;
#[cfg(target_os = "windows")]
use rtrb::{Producer, RingBuffer};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

#[cfg(target_os = "windows")]
use crate::latch::Ring;
use crate::{latch::Wait, stats::AudioStats, OPUS_SAMPLE_RATE};

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

/// Peak amplitude below which a frame counts as silent (≈ −80 dBFS,
/// under half a 16-bit LSB).
const SILENCE_THRESHOLD: f32 = 1e-4;

/// Consecutive silent frames before the encoder gate closes:
/// 25 × 20 ms = 500 ms of hangover, so reverb tails and short pauses
/// aren't clipped and the gate doesn't chatter at track boundaries.
const SILENCE_HANGOVER_FRAMES: u32 = 25;

/// Cap on the worker's block while waiting for capture data. Wakes come
/// from the backend's wake latch the moment samples land; this timeout
/// only bounds `stop()` latency. This is an
/// OS wait — zero CPU while blocked, ≤10 wakes/s during total silence.
const WORKER_WAIT_CAP: Duration = Duration::from_millis(100);

/// Where encoded packets go: called on the encoder worker as each one
/// is made; `false` means its consumer is gone, and the worker stops.
/// A full consumer drops the packet — a 20 ms gap in background sound —
/// rather than hold the worker.
pub type PacketSink = Arc<dyn Fn(SystemAudioPacket) -> bool + Send + Sync>;

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
/// `ScreenCaptureKit`, `PipeWire`, etc.
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
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) trait SystemAudioBackend {
    /// Start capturing. Spawns platform callbacks that push into the ring buffer.
    fn start(&mut self) -> Result<(), Box<dyn std::error::Error>>;

    /// Stop capturing and release platform resources.
    fn stop(&mut self);

    /// The sample rate of captured audio (for resampler configuration).
    fn sample_rate(&self) -> u32;

    /// Human-readable name of the capture source.
    fn device_name(&self) -> String;

    /// The capture stopped by itself (its device went, the audio engine
    /// kept failing) and will deliver nothing more.
    fn failed(&self) -> bool;
}

/// A started backend paired with the ring consumer the worker drains.
#[cfg(target_os = "windows")]
type StartedBackend = (Box<dyn SystemAudioBackend>, Consumer<f32>);

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
/// ```rust,ignore
/// use audio::stream::input::SystemAudioCapture;
/// use audio::stats::AudioStats;
/// use std::sync::Arc;
///
/// let stats = Arc::new(AudioStats::new());
/// // Each packet goes out as it is encoded.
/// let sink: PacketSink = Arc::new(move |packet| network.send_system_audio(packet));
/// let mut capture = SystemAudioCapture::new(stats, sink)?;
/// capture.start()?;
///
/// // Stop when screen sharing ends
/// capture.stop();
/// ```
pub struct SystemAudioCapture {
    /// Worker thread that reads from ring buffer, resamples, encodes.
    worker_thread: Option<thread::JoinHandle<()>>,

    /// Where encoded packets go, as each is made. They used to wait in
    /// a channel the screen thread drained on its capture tick, so a
    /// viewer got them in bursts of a frame interval and had to buffer
    /// a burst more (PRODUCTION-PLAN.md LAT-2).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    sink: PacketSink,

    /// Signal to stop worker thread.
    should_stop: Arc<AtomicBool>,

    /// Whether capture is currently active.
    is_active: Arc<AtomicBool>,

    /// Stats (shared with voice chat engine for unified reporting).
    /// Only read by the Windows backend — dead weight elsewhere, kept
    /// so the struct layout is identical everywhere.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    stats: Arc<AudioStats>,

    /// Platform backend handle — kept alive to maintain the capture
    /// stream. `SystemAudioCapture` lives and dies on the screen thread.
    #[cfg(target_os = "windows")]
    backend: Option<Box<dyn SystemAudioBackend>>,
}

impl SystemAudioCapture {
    /// Create a new system audio capturer.
    ///
    /// Does not start capturing — call `start()` when screen sharing begins.
    pub fn new(
        stats: Arc<AudioStats>,
        sink: PacketSink,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            worker_thread: None,
            sink,
            should_stop: Arc::new(AtomicBool::new(false)),
            is_active: Arc::new(AtomicBool::new(false)),
            stats,
            #[cfg(target_os = "windows")]
            backend: None,
        })
    }

    /// Start capturing system audio.
    ///
    /// Unsupported on this platform — the one backend is Windows' (see
    /// the module docs), so this always fails cleanly. Callers already
    /// treat a failed start as "share without stream audio".
    #[cfg(not(target_os = "windows"))]
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        Err("system audio capture is not supported on this platform".into())
    }

    /// Start capturing system audio.
    ///
    /// Creates the platform backend, ring buffer, and worker thread.
    /// Idempotent — calling twice is a no-op.
    #[cfg(target_os = "windows")]
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.is_active.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Wake latch: capture backends ping it after pushing samples so
        // the worker blocks instead of polling an empty ring. Pings
        // the worker has not seen yet collapse into one (`latch`).
        let (wake_tx, wake_rx) = crate::latch::latch();

        let (backend, ring_consumer) = Self::create_started_backend(&self.stats, &wake_tx)?;

        let device_sample_rate = backend.sample_rate();
        let device_name = backend.device_name();

        // Spawn the encoder worker thread
        let should_stop = self.should_stop.clone();

        self.should_stop.store(false, Ordering::Relaxed);

        let worker = Self::spawn_worker(
            ring_consumer,
            self.sink.clone(),
            should_stop,
            device_sample_rate,
            wake_rx,
        );

        self.worker_thread = Some(worker);
        self.backend = Some(backend);
        self.is_active.store(true, Ordering::Relaxed);

        info!("System audio capture ready: {device_name} @ {device_sample_rate}Hz");

        Ok(())
    }

    /// Ring buffer sizing: 200 ms at the 48 kHz capture rate, stereo
    /// interleaved (2× the mono sample count). Sized generously because
    /// WASAPI can deliver in large chunks.
    #[cfg(target_os = "windows")]
    fn new_ring() -> (Producer<f32>, Consumer<f32>) {
        RingBuffer::new(48000 * 2 * 200 / 1000)
    }

    /// Create and start the capture backend, returning it together
    /// with the ring consumer the worker reads from: process loopback
    /// (`stream::process_loopback`) — the system mix *excluding our own
    /// process tree*, so the peer voices we play never enter the stream.
    /// Windows 10 2004+; on an older build the share has no sound.
    #[cfg(target_os = "windows")]
    fn create_started_backend(
        stats: &Arc<AudioStats>,
        wake_tx: &Ring,
    ) -> Result<StartedBackend, Box<dyn std::error::Error>> {
        let (ring_producer, ring_consumer) = Self::new_ring();
        let mut backend = super::process_loopback::ProcessLoopback::new(
            ring_producer,
            wake_tx.clone(),
            stats.clone(),
        );
        backend.start().map_err(|e| {
            format!("process loopback unavailable ({e}); it needs Windows 10 version 2004 or later")
        })?;
        info!("System audio backend: process loopback (own audio excluded)");
        let backend: Box<dyn SystemAudioBackend> = Box::new(backend);
        Ok((backend, ring_consumer))
    }

    /// Stop capturing system audio and release all resources.
    /// The capture stopped by itself and will deliver nothing more: the
    /// share has no sound until it is started again. A process loopback
    /// that gave up used to say so in the log and nowhere else (review
    /// P2-22).
    #[must_use]
    pub fn failed(&self) -> bool {
        #[cfg(target_os = "windows")]
        {
            self.backend
                .as_ref()
                .is_some_and(|backend| backend.failed())
        }
        // No backend elsewhere: nothing started, nothing to fail.
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    pub fn stop(&mut self) {
        self.should_stop.store(true, Ordering::Release);

        // Stop the platform backend first (stops callbacks pushing to ring buffer)
        #[cfg(target_os = "windows")]
        if let Some(mut backend) = self.backend.take() {
            backend.stop();
        }

        // Join the worker thread
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }

        self.is_active.store(false, Ordering::Relaxed);
        info!("System audio capture stopped");
    }

    // ── Worker thread ────────────────────────────────────────────────────

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn spawn_worker(
        ring_consumer: Consumer<f32>,
        sink: PacketSink,
        should_stop: Arc<AtomicBool>,
        device_sample_rate: u32,
        wake_rx: Wait,
    ) -> thread::JoinHandle<()> {
        crate::platform::spawn_named("sysaudio-encode", move || {
            // Audio-class scheduling (MMCSS "Pro Audio" on Windows).
            // The capture side lives in platform backends; the worker's
            // timed waits benefit from the process-wide 1 ms timer held
            // by VoiceChat's TimerResolutionGuard.
            let _rt = crate::platform::promote_current_thread(960, crate::OPUS_SAMPLE_RATE);
            Self::worker_loop(
                ring_consumer,
                &*sink,
                &should_stop,
                device_sample_rate,
                wake_rx,
            );
        })
    }

    fn worker_loop(
        mut ring_consumer: Consumer<f32>,
        sink: &(dyn Fn(SystemAudioPacket) -> bool + Send + Sync),
        should_stop: &AtomicBool,
        device_sample_rate: u32,
        wake_rx: Wait,
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
        // screen sharing is active (unlike voice, which is gated and DTX-encoded).
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
            let ratio = f64::from(OPUS_SAMPLE_RATE) / f64::from(device_sample_rate);
            let params = SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: Some(0.95),
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
                                              // One interleaved stereo frame, reused for every encode.
        let mut frame = vec![0.0f32; SYSTEM_AUDIO_FRAME_SIZE * SYSTEM_AUDIO_CHANNELS];
        let mut seq: u32 = 0;
        // Silence-gate state: frames since the last audible one. Starts
        // at 0 so a share always primes the receiver with a few frames
        // before the gate can close.
        let mut silent_frames: u32 = 0;

        // Interleaved frame size (what Opus expects)
        let interleaved_frame_size = SYSTEM_AUDIO_FRAME_SIZE * SYSTEM_AUDIO_CHANNELS;

        info!(
            "System audio worker started (stereo, device {device_sample_rate}Hz, resample={needs_resample})"
        );

        while !should_stop.load(Ordering::Relaxed) {
            // ── Read interleaved stereo from ring buffer ────────────────
            let target_interleaved = if let Some(ref r) = resampler {
                r.input_frames_next() * SYSTEM_AUDIO_CHANNELS
            } else {
                interleaved_frame_size
            };

            // Read whole stereo pairs only. Capping by the ring's
            // readable count (rounded down to even) means a producer
            // caught mid-pair leaves its lone half in the ring for the
            // next pass — popping and discarding it (the old behavior)
            // permanently swapped L/R for everything that followed.
            let available_even = ring_consumer.slots() & !1_usize;
            let to_read = target_interleaved.min(read_buf.len()).min(available_even) & !1_usize;
            // One chunked read (to_read ≤ readable, so nothing is left
            // unfilled) instead of a pop per sample.
            let (popped, _) = ring_consumer.pop_partial_slice(&mut read_buf[..to_read]);
            let n_read = popped.len() & !1_usize;

            if n_read == 0 {
                // Empty ring: block until the capture backend pings the
                // wake latch (or the cap elapses for a stop check). This
                // replaces a 2 ms polling sleep that the 2026-08 audit
                // predicted at ~500 wakes/s during engine-idle silence.
                if !wake_rx.wait(WORKER_WAIT_CAP) {
                    break;
                }
                continue;
            }

            // ── Resample to 48 kHz stereo if needed ─────────────────────
            // Output lands straight in `accumulator` (interleaved, 48 kHz);
            // no intermediate Vec per pass.
            let before = accumulator.len();
            if let Some(ref mut r) = resampler {
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
                    Ok((_in_len, out_len)) => {
                        // Re-interleave: L0 R0 L1 R1 ...
                        for (left, right) in resampler_output[0][..out_len]
                            .iter()
                            .zip(&resampler_output[1][..out_len])
                        {
                            accumulator.push(*left);
                            accumulator.push(*right);
                        }
                    }
                    Err(e) => {
                        error!("System audio resampler error: {e}");
                    }
                }
            } else {
                accumulator.extend_from_slice(&read_buf[..n_read]);
            }

            if accumulator.len() == before {
                continue;
            }

            // ── Frame-sized chunks (interleaved_frame_size = 960 × 2) ──
            while accumulator.len() >= interleaved_frame_size {
                frame.copy_from_slice(&accumulator[..interleaved_frame_size]);
                accumulator.drain(..interleaved_frame_size);

                // ── Measure the peak ────────────────────────────────────
                let peak = frame.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));

                // ── Silence gate ────────────────────────────────────────
                // Measured 2026-08: without this, a silent share kept the
                // whole encode+network path busy shipping Opus-encoded
                // zeros (~50 pkts/s per viewer). Seq only advances
                // on sent packets, so the receiver sees no loss — its
                // playback ring simply underruns to silence by design.
                // Debug, not info: a game or a video pauses its sound
                // every few seconds (29 lines in 9 minutes of a
                // 2026-10-08 share), and nothing is wrong when it does.
                if peak < SILENCE_THRESHOLD {
                    silent_frames = silent_frames.saturating_add(1);
                    if silent_frames == SILENCE_HANGOVER_FRAMES {
                        debug!("System audio gate closed (silence)");
                    }
                    if silent_frames >= SILENCE_HANGOVER_FRAMES {
                        continue; // hangover elapsed — skip encode + send
                    }
                } else {
                    if silent_frames >= SILENCE_HANGOVER_FRAMES {
                        debug!("System audio gate opened (audio resumed)");
                    }
                    silent_frames = 0;
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

                        if !sink(packet) {
                            info!("System audio packets have no consumer, stopping worker");
                            return;
                        }
                    }
                    Err(e) => {
                        error!("System audio Opus encode error: {e}");
                    }
                }
            }
        }

        info!("System audio worker stopped");
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
