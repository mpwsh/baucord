// screen.rs — Screen share capture + encode thread.
//
// Same architecture as voice.rs: dedicated thread, crossbeam channels,
// control/event enums, shared state for UI.
//
// Sender side: capture → encode → NetworkControl::SendVideoFrame
// Receiver side: incoming H.264 frames → per-peer VideoDecoder → RGBA for display
//
// System audio: incoming Opus stereo packets are delegated to
// `core::stream::decoder` — screen.rs never touches Opus directly.

use crossbeam_channel::{Receiver, Sender, bounded};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use std::thread;
use std::time::{Duration, Instant};

use log::{info, warn};

use screencap::capture::{MonitorInfo, ScreenCapturer};
pub use screencap::{
    CodecError, EncodedFrame, Quality, VideoDecoder, VideoEncoder,
    codec::{FfmpegDecoder, FfmpegEncoder},
};

use crate::network::NetworkControl;
use crate::voice::WakeFn;

use core::stats::AudioStats;
use core::stream::decoder::{self as stream_decoder, StreamAudioPacket, StreamDecoder};
use core::stream::input::SystemAudioCapture;

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
        enqueued_at: Instant,
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
        /// RGBA pixel data. Callers should process this promptly — the
        /// screen thread will keep producing frames.
        rgba: Vec<u8>,
        /// Size of the encoded H.264 frame in bytes (before decode).
        frame_bytes: usize,
        seq: u32,
        is_keyframe: bool,
        enqueued_at: Instant,
        decoded_at: Instant,
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
    /// Current quality preset (stored as u8, mapped from Quality enum).
    pub quality: Arc<AtomicU8>,
}

impl SharedScreenState {
    pub fn new() -> Self {
        Self {
            is_sharing: Arc::new(AtomicBool::new(false)),
            frames_encoded: Arc::new(AtomicU64::new(0)),
            last_frame_bytes: Arc::new(AtomicU32::new(0)),
            last_encode_us: Arc::new(AtomicU32::new(0)),
            last_capture_us: Arc::new(AtomicU32::new(0)),
            quality: Arc::new(AtomicU8::new(0)),
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
    /// Whether we've received at least one keyframe for this stream.
    /// P-frames are silently dropped until the first keyframe arrives,
    /// preventing the "non-existing PPS 0 referenced" FFmpeg errors.
    got_keyframe: bool,
    /// Last successfully processed sequence number for gap detection.
    /// P-frames with seq gaps > 1 are dropped (unreliable channel may
    /// lose or reorder packets, and H.264 decoders produce corruption
    /// when reference frames are missing).
    last_seq: Option<u32>,
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
            // Block on channel — wake INSTANTLY when a frame arrives.
            // No timeout = no scheduler delay = full framerate.
            match control_rx.recv() {
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

        // Drain ALL pending messages
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

        if let Some(state) = &mut capture
            && Instant::now() >= next_capture
        {
            let cap_start = Instant::now();
            capture_and_encode(state, &event_tx, &network_tx, &shared);
            let cap_ms = cap_start.elapsed().as_secs_f32() * 1000.0;

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

            log::info!(
                "📊 ENCODE seq={} {cap_ms:.1}ms (budget {:.1}ms)",
                state.seq,
                state.quality.frame_duration().as_secs_f32() * 1000.0
            );
        }
    }

    shared.is_sharing.store(false, Ordering::Relaxed);
    info!("📺 Screen thread stopped");
}

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
                event_tx.send(ScreenEvent::SharingStopped).ok();
                wake();
                info!("📺 Screen share stopped");
            }
        }

        ScreenControl::IncomingVideoFrame {
            client_id,
            seq,
            is_keyframe,
            width,
            height,
            data,
            enqueued_at,
        } => {
            decode_incoming_frame(
                client_id,
                seq,
                is_keyframe,
                width,
                height,
                data,
                enqueued_at,
                peer_streams,
                event_tx,
                wake,
            );
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
            if let Some(stream) = peer_streams.get(&client_id) {
                if let Some(ref audio) = stream.stream_audio {
                    audio.set_volume(volume);
                }
            }
        }

        ScreenControl::MuteStream { client_id, muted } => {
            if let Some(stream) = peer_streams.get(&client_id) {
                if let Some(ref audio) = stream.stream_audio {
                    audio.set_muted(muted);
                }
            }
        }

        ScreenControl::Shutdown => {
            *running = false;
        }
    }
}

/// Decode an incoming video frame from a remote peer.
///
/// Key invariants:
/// - P-frames are dropped until the first keyframe (decoder needs SPS/PPS).
/// - On each keyframe, the decoder is **reset** (fresh instance) to prevent
///   corruption from compounding across GOP boundaries. This is critical
///   because the unreliable channel may lose P-frames, leaving the decoder
///   in an inconsistent reference state.
/// - P-frames with sequence gaps > 1 are dropped — the missing reference
///   frame(s) would cause cascading visual corruption.
fn decode_incoming_frame(
    client_id: u64,
    seq: u32,
    is_keyframe: bool,
    width: u32,
    height: u32,
    data: Vec<u8>,
    enqueued_at: Instant,
    peer_streams: &mut HashMap<u64, PeerStream>,
    event_tx: &Sender<ScreenEvent>,
    wake: &WakeFn,
) {
    if is_keyframe {
        let stream = peer_streams.entry(client_id).or_insert_with(|| {
            // First keyframe from this peer — create decoder
            info!("📺 Created decoder for peer {client_id} ({width}x{height})");
            let decoder = create_decoder().expect("decoder creation failed");
            PeerStream {
                decoder,
                width,
                height,
                got_keyframe: false,
                last_seq: None,
                stream_audio: None,
            }
        });

        stream.width = width;
        stream.height = height;
        stream.got_keyframe = true;
        stream.last_seq = Some(seq);

        info!(
            "📺 Keyframe #{seq} from peer {client_id}: {} bytes",
            data.len()
        );

        let encoded = EncodedFrame {
            data,
            timestamp_ms: 0,
            is_keyframe: true,
        };

        let t0 = std::time::Instant::now();
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
                        frame_bytes: encoded.data.len(),
                        seq,
                        is_keyframe,
                        enqueued_at,
                        decoded_at,
                    })
                    .ok();
                wake();
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Decode error for keyframe from peer {client_id} seq {seq}: {e}");
                stream.got_keyframe = false;
            }
        }
        return;
    }

    // P-frame path
    let Some(stream) = peer_streams.get_mut(&client_id) else {
        // No decoder yet — waiting for first keyframe
        return;
    };

    if !stream.got_keyframe {
        // Still waiting for a keyframe — silently drop
        return;
    }

    // Sequence gap detection: small gaps (1-3 frames) are tolerated —
    // the decoder may produce minor artifacts but recovers at next
    // keyframe. Large gaps indicate serious packet loss; reset to
    // avoid compounding corruption.
    if let Some(last) = stream.last_seq {
        let gap = seq.wrapping_sub(last);
        if gap > 1 && gap < 1000 {
            // Gap detected
            if gap > 5 {
                // Large gap — decoder state is too stale, wait for keyframe
                warn!(
                    "Large seq gap for peer {client_id}: expected {}, got {seq} (gap={gap}) — \
                 dropping until next keyframe",
                    last.wrapping_add(1)
                );
                stream.got_keyframe = false;
                return;
            }
            // Small gap (2-5 frames) — log but let the decoder try.
            // Worst case: minor artifacts until next keyframe.
            warn!(
                "Seq gap for peer {client_id}: expected {}, got {seq} (gap={gap}) — \
             continuing (minor artifacts possible)",
                last.wrapping_add(1)
            );
        }
    }

    stream.last_seq = Some(seq);

    let encoded = EncodedFrame {
        data,
        timestamp_ms: 0,
        is_keyframe: false,
    };

    match stream.decoder.decode(&encoded) {
        Ok(Some(decoded)) => {
            let decoded_at = Instant::now();
            event_tx
                .send(ScreenEvent::FrameReady {
                    client_id,
                    width: decoded.width,
                    height: decoded.height,
                    rgba: decoded.data,
                    frame_bytes: encoded.data.len(),
                    seq,
                    is_keyframe: false,
                    enqueued_at,
                    decoded_at,
                })
                .ok();
            wake();
        }
        Ok(None) => {}
        Err(e) => {
            warn!("Decode error for peer {client_id} seq {seq}: {e}");
            // Decode error likely means corrupted reference state —
            // wait for the next keyframe to recover.
            stream.got_keyframe = false;
        }
    }
}

fn capture_and_encode(
    state: &mut CaptureState,
    event_tx: &Sender<ScreenEvent>,
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
        log::warn!("Slow capture: {capture_ms:.1}ms");
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
        info!(
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

/// Map a resolution back to the closest Quality preset.
fn quality_from_resolution(width: u32, height: u32) -> Quality {
    match (width, height) {
        (1920, 1080) => Quality::FHD30,
        (1280, 720) => Quality::HD30,
        _ => {
            if width > 1280 {
                Quality::FHD30
            } else {
                Quality::HD30
            }
        }
    }
}

// ── Public helpers ──────────────────────────────────────────────────────

/// List available monitors for the UI's monitor picker.
pub fn list_monitors() -> Result<Vec<MonitorInfo>, screencap::CaptureError> {
    screencap::capture::list_monitors()
}
