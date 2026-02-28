// screen.rs — Screen share capture + encode thread.
//
// Same architecture as voice.rs: dedicated thread, crossbeam channels,
// control/event enums, shared state for UI.
//
// Sender side: capture → encode → NetworkControl::SendVideoFrame
// Receiver side: incoming H.264 frames → jitter buffer → per-peer VideoDecoder → RGBA for display
//
// The jitter buffer decouples packet receipt from decode. Without it,
// keyframe decode stalls (10-16ms) block the channel drain loop, causing
// `try_send` drops on the network thread and consistent sequence gaps
// after every keyframe. With the buffer, packets are pushed in O(log n)
// and the decoder pulls at its own pace.
//
// System audio: incoming Opus stereo packets are delegated to
// `core::stream::decoder` — screen.rs never touches Opus directly.

use crossbeam_channel::{Receiver, Sender, bounded};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::thread;
use std::time::{Duration, Instant};

use log::{info, warn};

use video::{
    CodecError, EncodedFrame, Quality, VideoDecoder, VideoEncoder,
    buffer::{BufferedPacket, VideoJitterBuffer},
    capture::ScreenCapturer,
    codec::{FfmpegDecoder, FfmpegEncoder},
};

use crate::{network::NetworkControl, voice::WakeFn};

use audio::{
    stats::AudioStats,
    stream::{
        decoder::{self as stream_decoder, StreamAudioPacket, StreamDecoder},
        input::SystemAudioCapture,
    },
};

// ── Channel messages ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ScreenControl {
    /// Start sharing our screen.
    StartSharing {
        quality: Quality,
        monitor_index: usize,
    },
    /// Stop sharing our screen.
    StopSharing,
    /// An encoded video frame arrived from the network for a remote peer.
    IncomingVideoFrame {
        client_id: u64,
        seq: u32,
        is_keyframe: bool,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// A remote peer started sharing — prepare a decoder.
    PeerStartedSharing {
        client_id: u64,
        width: u32,
        height: u32,
    },
    /// A remote peer stopped sharing — drop their decoder.
    PeerStoppedSharing { client_id: u64 },
    /// An encoded system audio packet arrived from the network for a remote peer.
    IncomingSystemAudio {
        client_id: u64,
        seq: u32,
        data: Vec<u8>,
    },
    /// Set the stream audio volume for a peer's viewer.
    SetStreamVolume { client_id: u64, volume: f32 },
    /// Mute/unmute the stream audio for a peer's viewer.
    MuteStream { client_id: u64, muted: bool },
    /// Restart the system audio capture (e.g. after an output device change).
    /// WASAPI loopback is bound to the default output device, so when that
    /// device changes the capture stream dies and must be rebuilt.
    RestartSystemAudio,
    /// Clean shutdown.
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum ScreenEvent {
    /// Our screen share started successfully.
    SharingStarted { quality: Quality },
    /// Our screen share stopped.
    SharingStopped,
    /// Error during capture or encode.
    Error(String),
    /// A decoded frame is ready for display.
    FrameReady {
        client_id: u64,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        frame_bytes: usize,
        seq: u32,
        is_keyframe: bool,
        enqueued_at: Instant,
        decoded_at: Instant,
        buffer_depth_ms: f32,
        buffer_target_ms: f32,
        buffer_jitter_ms: f32,
    },
    /// A remote peer started sharing (so UI can show "Watch" button).
    PeerStartedSharing {
        client_id: u64,
        width: u32,
        height: u32,
    },
    /// A remote peer stopped sharing.
    PeerStoppedSharing { client_id: u64 },
}

// ── Shared state for UI ─────────────────────────────────────────────────

#[derive(Clone)]
pub struct SharedScreenState {
    /// Whether we are currently sharing.
    pub is_sharing: Arc<AtomicBool>,
    /// Number of frames encoded so far.
    pub frames_encoded: Arc<AtomicU64>,
    /// Size of the last encoded frame in bytes.
    pub last_frame_bytes: Arc<AtomicU32>,
    /// Encode time of the last frame in microseconds.
    pub last_encode_us: Arc<AtomicU32>,
    /// Capture time of the last frame in microseconds.
    pub last_capture_us: Arc<AtomicU32>,
    /// Selected quality by streamer
    pub current_quality: Arc<Mutex<Option<Quality>>>,
}

impl SharedScreenState {
    pub fn new() -> Self {
        Self {
            is_sharing: Arc::new(AtomicBool::new(false)),
            frames_encoded: Arc::new(AtomicU64::new(0)),
            last_frame_bytes: Arc::new(AtomicU32::new(0)),
            last_encode_us: Arc::new(AtomicU32::new(0)),
            last_capture_us: Arc::new(AtomicU32::new(0)),
            current_quality: Arc::new(Mutex::new(None)),
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────

pub struct ScreenThreadHandle {
    pub tx: Sender<ScreenControl>,
    pub event_rx: Receiver<ScreenEvent>,
    pub shared: SharedScreenState,
    pub(crate) join_handle: Option<thread::JoinHandle<()>>,
}

impl ScreenThreadHandle {
    pub fn spawn(
        network_tx: Sender<NetworkControl>,
        wake: WakeFn,
        voice_energy: Arc<AtomicU32>,
    ) -> Self {
        // 256 slots: at 30fps inbound + outbound control messages, 64 was
        // too tight and caused try_send drops during active streaming.
        let (tx, control_rx) = bounded::<ScreenControl>(256);
        let (event_tx, event_rx) = bounded::<ScreenEvent>(256);
        let shared = SharedScreenState::new();

        let shared_clone = shared.clone();
        let voice_energy_clone = voice_energy.clone();
        let join_handle = thread::spawn(move || {
            run_screen_loop(
                control_rx,
                event_tx,
                network_tx,
                shared_clone,
                wake,
                voice_energy_clone,
            );
        });

        Self {
            tx,
            event_rx,
            shared,
            join_handle: Some(join_handle),
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(ScreenControl::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ScreenThreadHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Capture + encode state ──────────────────────────────────────────────

struct CaptureState {
    capturer: ScreenCapturer,
    encoder: Box<dyn VideoEncoder + Send>,
    quality: Quality,
    seq: u32,
    /// System audio capture — started alongside video, sends Opus stereo
    /// packets to the network. `None` if platform doesn't support it.
    system_audio: Option<SystemAudioCapture>,
}

// ── Decoder state for incoming streams ──────────────────────────────────

struct PeerStream {
    decoder: Box<dyn VideoDecoder + Send>,
    width: u32,
    height: u32,
    /// Jitter buffer for this peer's video stream. Holds encoded packets
    /// and releases them in sequence order at a controlled rate.
    jitter_buffer: VideoJitterBuffer,
    /// Stream audio decoder + playback pipeline for this peer.
    /// Lazy-initialized on the first system audio packet.
    /// Owned by the core library — screen.rs just feeds it raw Opus bytes.
    stream_audio: Option<StreamDecoder>,
}

// ── Thread loop ─────────────────────────────────────────────────────────

fn run_screen_loop(
    control_rx: Receiver<ScreenControl>,
    event_tx: Sender<ScreenEvent>,
    network_tx: Sender<NetworkControl>,
    shared: SharedScreenState,
    wake: WakeFn,
    voice_energy: Arc<AtomicU32>,
) {
    let mut capture: Option<CaptureState> = None;
    let mut peer_streams: HashMap<u64, PeerStream> = HashMap::new();
    let mut running = true;
    let mut next_capture = Instant::now();

    while running {
        let now = Instant::now();

        let is_viewing = !peer_streams.is_empty();
        let is_capturing = capture.is_some();

        if is_capturing {
            // ── Sharing mode ────────────────────────────────────────
            let time_to_capture = next_capture.saturating_duration_since(now);

            match control_rx.recv_timeout(time_to_capture) {
                Ok(ctrl) => handle_control(
                    ctrl,
                    &mut capture,
                    &mut peer_streams,
                    &event_tx,
                    &network_tx,
                    &shared,
                    &wake,
                    &mut running,
                    &voice_energy,
                ),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        } else if is_viewing {
            // ── Viewing mode ────────────────────────────────────────
            // Use a short timeout instead of blocking recv() so we can
            // drain the jitter buffer even when no new packets arrive.
            // 4ms gives ~250Hz drain rate — fast enough for 60fps with
            // headroom, while keeping CPU usage negligible.
            match control_rx.recv_timeout(Duration::from_millis(4)) {
                Ok(ctrl) => handle_control(
                    ctrl,
                    &mut capture,
                    &mut peer_streams,
                    &event_tx,
                    &network_tx,
                    &shared,
                    &wake,
                    &mut running,
                    &voice_energy,
                ),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        } else {
            // ── Idle mode ───────────────────────────────────────────
            match control_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ctrl) => handle_control(
                    ctrl,
                    &mut capture,
                    &mut peer_streams,
                    &event_tx,
                    &network_tx,
                    &shared,
                    &wake,
                    &mut running,
                    &voice_energy,
                ),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }

        // Drain ALL pending control messages. This is critical: each
        // IncomingVideoFrame now just pushes into the jitter buffer
        // (nanoseconds), so we can drain hundreds of pending packets
        // without blocking.
        while let Ok(ctrl) = control_rx.try_recv() {
            handle_control(
                ctrl,
                &mut capture,
                &mut peer_streams,
                &event_tx,
                &network_tx,
                &shared,
                &wake,
                &mut running,
                &voice_energy,
            );
        }

        // ── Drain jitter buffers → decode → emit ────────────────────
        // Pull packets from each peer's jitter buffer and decode them.
        // This runs AFTER the channel drain, so all newly arrived packets
        // are in the buffer before we start decoding.
        drain_and_decode(&mut peer_streams, &event_tx, &wake);

        if let Some(state) = &mut capture
            && Instant::now() >= next_capture
        {
            let cap_start = Instant::now();
            capture_and_encode(state, &network_tx, &shared);

            // Drain system audio packets and send to network
            if let Some(ref sys_audio) = state.system_audio {
                for pkt in sys_audio.drain_packets() {
                    let _ = network_tx.try_send(NetworkControl::SendSystemAudio {
                        seq: pkt.seq,
                        channels: pkt.channels,
                        duration_ms: pkt.duration_ms as u16,
                        data: pkt.data,
                    });
                }
            }

            // FIX: advance from previous target, not from now
            next_capture += state.quality.frame_duration();

            // If we fell behind, skip to now (don't burst)
            if next_capture < Instant::now() {
                next_capture = Instant::now();
            }

            let cap_ms = cap_start.elapsed().as_secs_f32() * 1000.0;
            log::debug!(
                "📊 ENCODE seq={} {cap_ms:.1}ms (budget {:.1}ms)",
                state.seq,
                state.quality.frame_duration().as_secs_f32() * 1000.0
            );
        }
    }

    shared.is_sharing.store(false, Ordering::Relaxed);
    info!("📺 Screen thread stopped");
}

/// Pull packets from each peer's jitter buffer and decode them.
///
/// This is the key change from the old architecture: decoding is now
/// driven by the jitter buffer's readiness, not by packet arrival.
/// The buffer absorbs decode-time jitter so keyframe stalls don't
/// cause packet loss.
fn drain_and_decode(
    peer_streams: &mut HashMap<u64, PeerStream>,
    event_tx: &Sender<ScreenEvent>,
    wake: &WakeFn,
) {
    for (&client_id, stream) in peer_streams.iter_mut() {
        while let Some(packet) = stream.jitter_buffer.pull_next() {
            let seq = packet.seq;
            let is_keyframe = packet.is_keyframe;
            let enqueued_at = packet.arrival_time;
            let frame_bytes = packet.data.len();

            if is_keyframe {
                // Flush decoder before every keyframe. Since we're only
                // receiving keyframes right now (delta frames lost in
                // transit), every keyframe is effectively a recovery point.
                // Even when deltas start arriving, flushing on keyframes
                // is cheap and guarantees no corruption carryover.
                stream.decoder.flush();

                stream.width = packet.width;
                stream.height = packet.height;
                info!(
                    "📺 Keyframe #{seq} from peer {client_id}: {frame_bytes} bytes \
                     (buffer depth: {:.0}ms)",
                    stream.jitter_buffer.depth_ms()
                );
            }

            let encoded = EncodedFrame {
                data: packet.data,
                timestamp_ms: 0,
                is_keyframe,
            };

            let t0 = Instant::now();
            match stream.decoder.decode(&encoded) {
                Ok(Some(decoded)) => {
                    let decode_ms = t0.elapsed().as_secs_f32() * 1000.0;
                    let decoded_at = Instant::now();
                    if decode_ms > 10.0 {
                        log::warn!("Slow decode: {decode_ms:.1}ms for seq {seq}");
                    }
                    event_tx
                        .send(ScreenEvent::FrameReady {
                            client_id,
                            width: decoded.width,
                            height: decoded.height,
                            rgba: decoded.data,
                            frame_bytes,
                            seq,
                            is_keyframe,
                            enqueued_at,
                            decoded_at,
                            buffer_depth_ms: stream.jitter_buffer.depth_ms(),
                            buffer_target_ms: stream.jitter_buffer.adaptive_target_ms(),
                            buffer_jitter_ms: stream.jitter_buffer.adaptive_jitter_ms(),
                        })
                        .ok();
                    wake();
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("Decode error for peer {client_id} seq {seq}: {e}");
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_control(
    ctrl: ScreenControl,
    capture: &mut Option<CaptureState>,
    peer_streams: &mut HashMap<u64, PeerStream>,
    event_tx: &Sender<ScreenEvent>,
    network_tx: &Sender<NetworkControl>,
    shared: &SharedScreenState,
    wake: &WakeFn,
    running: &mut bool,
    voice_energy: &Arc<AtomicU32>,
) {
    match ctrl {
        ScreenControl::StartSharing {
            quality,
            monitor_index,
        } => {
            // Create capturer
            let capturer = match ScreenCapturer::new(monitor_index, quality) {
                Ok(c) => c,
                Err(e) => {
                    event_tx
                        .send(ScreenEvent::Error(format!("Capture init: {e}")))
                        .ok();
                    wake();
                    return;
                }
            };

            // Create encoder — use ffmpeg if available, fall back to openh264
            let encoder: Box<dyn VideoEncoder + Send> = match create_encoder(quality) {
                Ok(e) => e,
                Err(e) => {
                    event_tx
                        .send(ScreenEvent::Error(format!("Encoder init: {e}")))
                        .ok();
                    wake();
                    return;
                }
            };

            info!("📺 Screen share started: {quality}, monitor {monitor_index}");

            // Start system audio capture (best-effort — non-fatal if unsupported)
            let system_audio = match SystemAudioCapture::new(Arc::new(AudioStats::new())) {
                Ok(mut cap) => match cap.start() {
                    Ok(()) => {
                        info!("🔊🎤 System audio capture started for screen share");
                        Some(cap)
                    }
                    Err(e) => {
                        warn!("🔊 System audio capture failed to start: {e}");
                        None
                    }
                },
                Err(e) => {
                    warn!("🔊 System audio capture not available: {e}");
                    None
                }
            };

            *capture = Some(CaptureState {
                capturer,
                encoder,
                quality,
                seq: 0,
                system_audio,
            });

            shared.is_sharing.store(true, Ordering::Relaxed);
            shared.frames_encoded.store(0, Ordering::Relaxed);

            // Tell network to signal other clients
            let _ = network_tx.send(NetworkControl::ScreenShareStart {
                width: quality.width(),
                height: quality.height(),
            });

            event_tx.send(ScreenEvent::SharingStarted { quality }).ok();
            wake();
        }

        ScreenControl::StopSharing => {
            if capture.take().is_some() {
                shared.is_sharing.store(false, Ordering::Relaxed);
                let _ = network_tx.send(NetworkControl::ScreenShareStop);
                *shared.current_quality.lock().unwrap() = None;
                event_tx.send(ScreenEvent::SharingStopped).ok();
                wake();
                info!("📺 Screen share stopped");
            }
        }

        // ── Jitter buffer push (the critical change) ────────────────
        // Previously this called decode_incoming_frame() which blocked
        // on FFmpeg decode. Now it just pushes into the BTreeMap —
        // nanoseconds instead of milliseconds.
        ScreenControl::IncomingVideoFrame {
            client_id,
            seq,
            is_keyframe,
            width,
            height,
            data,
        } => {
            let stream = peer_streams.entry(client_id).or_insert_with(|| {
                info!("📺 Created decoder for peer {client_id} ({width}x{height})");
                let decoder = create_decoder().expect("decoder creation failed");
                // Assume 30fps initially; will be refined when Quality is known.
                // The buffer adapts well even if the FPS guess is off.
                let jitter_buffer = VideoJitterBuffer::new(30);
                PeerStream {
                    decoder,
                    width,
                    height,
                    jitter_buffer,
                    stream_audio: None,
                }
            });

            stream.jitter_buffer.push(BufferedPacket {
                seq,
                is_keyframe,
                width,
                height,
                data,
                arrival_time: Instant::now(),
            });
        }

        ScreenControl::PeerStartedSharing {
            client_id,
            width,
            height,
        } => {
            event_tx
                .send(ScreenEvent::PeerStartedSharing {
                    client_id,
                    width,
                    height,
                })
                .ok();
            wake();
        }

        ScreenControl::PeerStoppedSharing { client_id } => {
            // Dropping the PeerStream also drops StreamDecoder,
            // which stops the decode thread + playback pipeline.
            peer_streams.remove(&client_id);
            event_tx
                .send(ScreenEvent::PeerStoppedSharing { client_id })
                .ok();
            wake();
        }

        ScreenControl::IncomingSystemAudio {
            client_id,
            seq,
            data,
        } => {
            decode_incoming_system_audio(client_id, seq, data, peer_streams, voice_energy);
        }

        ScreenControl::SetStreamVolume { client_id, volume } => {
            if let Some(stream) = peer_streams.get(&client_id)
                && let Some(ref audio) = stream.stream_audio
            {
                audio.set_volume(volume);
            }
        }

        ScreenControl::MuteStream { client_id, muted } => {
            if let Some(stream) = peer_streams.get(&client_id)
                && let Some(ref audio) = stream.stream_audio
            {
                audio.set_muted(muted);
            }
        }

        ScreenControl::RestartSystemAudio => {
            if let Some(state) = capture {
                // Stop the old capture (drops the dead WASAPI loopback stream)
                if let Some(mut old) = state.system_audio.take() {
                    old.stop();
                    info!("🔊🎤 System audio capture stopped for device rebuild");
                }

                // Rebuild — best-effort, non-fatal
                match SystemAudioCapture::new(Arc::new(AudioStats::new())) {
                    Ok(mut cap) => match cap.start() {
                        Ok(()) => {
                            info!("🔊🎤 System audio capture restarted after device change");
                            state.system_audio = Some(cap);
                        }
                        Err(e) => {
                            warn!("🔊 System audio capture restart failed: {e}");
                        }
                    },
                    Err(e) => {
                        warn!("🔊 System audio capture unavailable after device change: {e}");
                    }
                }
            }
        }

        ScreenControl::Shutdown => {
            *running = false;
        }
    }
}

fn capture_and_encode(
    state: &mut CaptureState,
    network_tx: &Sender<NetworkControl>,
    shared: &SharedScreenState,
) {
    // Capture
    let t0 = Instant::now();
    let frame = match state.capturer.capture() {
        Ok(f) => f,
        Err(e) => {
            warn!("Capture failed: {e}");
            return;
        }
    };
    let capture_us = t0.elapsed().as_micros() as u32;
    let capture_ms = capture_us as f32 / 1000.0;
    if capture_ms > 20.0 {
        log::debug!("Slow capture: {capture_ms:.1}ms");
    }

    // Encode
    let t1 = Instant::now();
    let encoded = match state.encoder.encode(&frame) {
        Ok(e) => e,
        Err(e) => {
            warn!("Encode failed: {e}");
            // Return capture buffer for reuse
            state.capturer.return_buffer(frame.data);
            return;
        }
    };
    if encoded.is_keyframe {
        log::debug!(
            "📺 Sent keyframe #{}: {} bytes",
            state.seq,
            encoded.data.len()
        );
    }
    let encode_us = t1.elapsed().as_micros() as u32;

    // Update shared stats
    shared.last_capture_us.store(capture_us, Ordering::Relaxed);
    shared.last_encode_us.store(encode_us, Ordering::Relaxed);
    shared
        .last_frame_bytes
        .store(encoded.data.len() as u32, Ordering::Relaxed);
    shared.frames_encoded.fetch_add(1, Ordering::Relaxed);

    // Send to network
    let seq = state.seq;
    state.seq = state.seq.wrapping_add(1);

    let _ = network_tx.try_send(NetworkControl::SendVideoFrame {
        seq,
        is_keyframe: encoded.is_keyframe,
        width: state.quality.width() as u16,
        height: state.quality.height() as u16,
        data: encoded.data,
    });

    // Return capture buffer for reuse
    state.capturer.return_buffer(frame.data);
}

/// Decode an incoming system audio (Opus stereo) packet from a remote peer.
///
/// Lazy-initializes the stream decoder on the first packet. The Opus
/// decoder, PLC, and playback pipeline all live in `core::stream::decoder`
/// — this function just creates the handle and feeds it raw bytes.
fn decode_incoming_system_audio(
    client_id: u64,
    seq: u32,
    data: Vec<u8>,
    peer_streams: &mut HashMap<u64, PeerStream>,
    voice_energy: &Arc<AtomicU32>,
) {
    let Some(stream) = peer_streams.get_mut(&client_id) else {
        // No PeerStream yet — waiting for first video keyframe.
        // System audio packets that arrive before the first keyframe
        // are silently dropped.
        return;
    };

    // Lazy-init the stream decoder + playback on first system audio packet.
    if stream.stream_audio.is_none() {
        match stream_decoder::spawn(None, 1.0, Some(voice_energy.clone())) {
            Ok(decoder) => {
                info!("🔊🎬 Started stream audio decoder for peer {client_id}");
                stream.stream_audio = Some(decoder);
            }
            Err(e) => {
                warn!("Failed to create stream audio decoder for peer {client_id}: {e}");
                return;
            }
        }
    }

    // Feed the raw Opus packet — core library handles decode + PLC + playback.
    if let Some(ref audio) = stream.stream_audio {
        audio.send(StreamAudioPacket { data, seq });
    }
}

// ── Encoder/decoder factory ─────────────────────────────────────────────

/// Create the best available video encoder.
fn create_encoder(quality: Quality) -> Result<Box<dyn VideoEncoder + Send>, CodecError> {
    {
        match FfmpegEncoder::new(quality) {
            Ok(enc) => {
                info!("📺 Using FFmpeg encoder: {}", enc.encoder_name());
                return Ok(Box::new(enc));
            }
            Err(e) => {
                warn!("FFmpeg encoder unavailable: {e}, trying openh264");
            }
        }
    }

    #[allow(unreachable_code)]
    Err(CodecError::NoEncoder(
        "no codec features enabled".to_string(),
    ))
}

/// Create the best available video decoder.
fn create_decoder() -> Result<Box<dyn VideoDecoder + Send>, CodecError> {
    match FfmpegDecoder::new() {
        Ok(dec) => return Ok(Box::new(dec)),
        Err(e) => {
            warn!("FFmpeg decoder unavailable: {e}, trying openh264");
        }
    }

    #[allow(unreachable_code)]
    Err(CodecError::DecoderInit(
        "no codec features enabled".to_string(),
    ))
}