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

use std::collections::{HashMap, HashSet};
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

use crate::{WakeFn, network::NetworkControl};

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
    /// A viewer (relayed via the server) asked for a fresh keyframe —
    /// their decoder hit an unrecoverable gap, or they joined mid-GOP.
    ForceKeyframe,
    /// Aggregated viewer health for our stream, relayed by the network
    /// thread ~1/s while sharing. Drives sender-side fps adaptation.
    StreamHealth {
        worst_loss: f32,
        worst_rtt_ms: f32,
        viewers: u16,
    },
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
    /// Worst packet loss among our viewers, in percent, stored as f32
    /// bits — read with `f32::from_bits`. Updated ~1/s while sharing.
    pub stream_loss_pct: Arc<AtomicU32>,
    /// Worst viewer RTT in milliseconds, stored as f32 bits.
    pub stream_rtt_ms: Arc<AtomicU32>,
    /// Number of viewers currently subscribed to our stream.
    pub stream_viewers: Arc<AtomicU32>,
    /// Current capture pace as a percentage of the preset fps
    /// (100 / 75 / 50) — sender-side adaptation under viewer loss.
    pub fps_scale_pct: Arc<AtomicU32>,
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
            stream_loss_pct: Arc::new(AtomicU32::new(0)),
            stream_rtt_ms: Arc::new(AtomicU32::new(0)),
            stream_viewers: Arc::new(AtomicU32::new(0)),
            fps_scale_pct: Arc::new(AtomicU32::new(100)),
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────

pub struct ScreenThreadHandle {
    pub tx: Sender<ScreenControl>,
    pub event_rx: Receiver<ScreenEvent>,
    pub shared: SharedScreenState,
    /// Joined by the frontend on shutdown (with a timeout).
    pub join_handle: Option<thread::JoinHandle<()>>,
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
        let join_handle = thread::Builder::new()
            .name("screen-share".into())
            .spawn(move || {
                run_screen_loop(
                    control_rx,
                    event_tx,
                    network_tx,
                    shared_clone,
                    wake,
                    voice_energy_clone,
                );
            })
            .expect("failed to spawn screen thread");

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
    /// Index into `PACE_SCALES` — current fps adaptation step.
    pace_idx: usize,
    /// When the pace last changed (hysteresis cooldown).
    pace_changed: Instant,
    /// Since when viewer loss has been continuously low (recovery hold).
    pace_good_since: Option<Instant>,
    /// Consecutive over-threshold StreamHealth samples (spike filter).
    pace_bad_samples: u32,
    /// Recent viewer keyframe-request arrivals (distress detector).
    kf_request_times: Vec<Instant>,
    /// Last time viewer distress stepped the pace down.
    last_distress: Option<Instant>,
}

// ── Sender-side fps adaptation ──────────────────────────────────────────
//
// When viewers report sustained packet loss (via the server's
// StreamHealth aggregation), reduce capture fps instead of per-frame
// quality: bitrate falls roughly proportionally while every frame keeps
// its full clarity — the right trade for screen share, where viewers
// are *reading* the stream, not judging motion smoothness. The
// encoder's bitrate/quality settings are untouched; in fact each frame
// gets a slightly larger share of the rate budget.

const PACE_SCALES: [f32; 3] = [1.0, 0.75, 0.5];
/// Step the fps down when worst viewer loss exceeds this.
const PACE_LOSS_STEP_DOWN: f32 = 0.08;
/// Consider the link recovered below this loss.
const PACE_LOSS_RECOVERED: f32 = 0.02;
/// Minimum time between pace changes.
const PACE_STEP_COOLDOWN: Duration = Duration::from_secs(2);
/// How long loss must stay low before stepping back up.
const PACE_RECOVER_HOLD: Duration = Duration::from_secs(5);
/// Ignore StreamHealth samples with implausible loss. renet computes
/// loss over a 6 s window that excludes the newest 900 ms; on a quiet
/// or freshly-subscribed connection that window holds a handful of
/// packets (idle exchanges even end with a terminal ack packet that is
/// never acked), so a perfect link can read 50–100% "loss". A link
/// that genuinely loses half its packets can't watch a stream at all —
/// adaptation has nothing useful to do with such a sample either way.
const PACE_LOSS_IMPLAUSIBLE: f32 = 0.5;
/// Consecutive over-threshold samples (1/s each) required to step down —
/// a single spike is measurement noise, three seconds of it is weather.
const PACE_STEP_DOWN_SAMPLES: u32 = 3;
/// Repeated viewer keyframe requests mean a viewer keeps failing to
/// keep a picture — from loss we can't see, or a decoder that can't
/// keep up with the frame rate. Either way, fewer frames per second is
/// the remedy. This many requests within PACE_DISTRESS_WINDOW steps
/// the pace down.
const PACE_DISTRESS_REQUESTS: usize = 4;
const PACE_DISTRESS_WINDOW: Duration = Duration::from_secs(5);
/// Don't step back up for this long after viewer distress — stepping up
/// the moment the viewer recovers just re-chokes them in a slow cycle.
const PACE_DISTRESS_HOLD: Duration = Duration::from_secs(30);

/// The capture interval at the current pace.
fn effective_frame_interval(state: &CaptureState) -> Duration {
    state
        .quality
        .frame_duration()
        .mul_f32(1.0 / PACE_SCALES[state.pace_idx])
}

/// Apply one StreamHealth sample to the pace state machine.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn adapt_pace(state: &mut CaptureState, worst_loss: f32, shared: &SharedScreenState) {
    let now = Instant::now();
    if worst_loss >= PACE_LOSS_IMPLAUSIBLE {
        // Measurement artifact (or a dead link, where fps changes help
        // nobody): display it, adapt on nothing, hold all timers.
        state.pace_bad_samples = 0;
    } else if worst_loss >= PACE_LOSS_STEP_DOWN {
        state.pace_good_since = None;
        state.pace_bad_samples += 1;
        if state.pace_bad_samples >= PACE_STEP_DOWN_SAMPLES
            && state.pace_idx + 1 < PACE_SCALES.len()
            && now.duration_since(state.pace_changed) >= PACE_STEP_COOLDOWN
        {
            state.pace_idx += 1;
            state.pace_changed = now;
            state.pace_bad_samples = 0;
            info!(
                "Viewer loss {:.1}% sustained — reducing capture to {:.0}% fps (clarity preserved)",
                worst_loss * 100.0,
                PACE_SCALES[state.pace_idx] * 100.0
            );
        }
    } else if worst_loss <= PACE_LOSS_RECOVERED {
        state.pace_bad_samples = 0;
        if state.pace_idx > 0 {
            let since = *state.pace_good_since.get_or_insert(now);
            if now.duration_since(since) >= PACE_RECOVER_HOLD
                && now.duration_since(state.pace_changed) >= PACE_STEP_COOLDOWN
                && state
                    .last_distress
                    .is_none_or(|t| now.duration_since(t) >= PACE_DISTRESS_HOLD)
            {
                state.pace_idx -= 1;
                state.pace_changed = now;
                state.pace_good_since = Some(now);
                info!(
                    "Viewer link recovered — restoring capture to {:.0}% fps",
                    PACE_SCALES[state.pace_idx] * 100.0
                );
            }
        }
    } else {
        // Middle band: hold the current pace, restart both clocks.
        state.pace_bad_samples = 0;
        state.pace_good_since = None;
    }
    shared.fps_scale_pct.store(
        (PACE_SCALES[state.pace_idx] * 100.0) as u32,
        Ordering::Relaxed,
    );
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

// ── Loop state ──────────────────────────────────────────────────────────

/// Mutable state owned by the screen thread, grouped so it can be handed
/// to `handle_control` as a single argument.
struct ScreenState {
    /// Our own capture + encode pipeline while sharing.
    capture: Option<CaptureState>,
    /// Decode pipelines for remote streams we're watching.
    peer_streams: HashMap<u64, PeerStream>,
    /// Peers whose decoder failed to initialize. Their frames are dropped
    /// (and the error reported once) instead of panicking per frame.
    decoder_failed: HashSet<u64>,
    running: bool,
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
    let mut st = ScreenState {
        capture: None,
        peer_streams: HashMap::new(),
        decoder_failed: HashSet::new(),
        running: true,
    };
    let mut next_capture = Instant::now();

    while st.running {
        let now = Instant::now();

        let is_viewing = !st.peer_streams.is_empty();
        let is_capturing = st.capture.is_some();

        if is_capturing {
            // ── Sharing mode ────────────────────────────────────────
            let time_to_capture = next_capture.saturating_duration_since(now);

            match control_rx.recv_timeout(time_to_capture) {
                Ok(ctrl) => handle_control(
                    ctrl,
                    &mut st,
                    &event_tx,
                    &network_tx,
                    &shared,
                    &wake,
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
                    &mut st,
                    &event_tx,
                    &network_tx,
                    &shared,
                    &wake,
                    &voice_energy,
                ),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        } else {
            // ── Idle mode ───────────────────────────────────────────
            // Nothing to capture and no jitter buffers to drain —
            // block until a command arrives instead of waking 10×/s.
            match control_rx.recv() {
                Ok(ctrl) => handle_control(
                    ctrl,
                    &mut st,
                    &event_tx,
                    &network_tx,
                    &shared,
                    &wake,
                    &voice_energy,
                ),
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
                &mut st,
                &event_tx,
                &network_tx,
                &shared,
                &wake,
                &voice_energy,
            );
        }

        // ── Drain jitter buffers → decode → emit ────────────────────
        // Pull packets from each peer's jitter buffer and decode them.
        // This runs AFTER the channel drain, so all newly arrived packets
        // are in the buffer before we start decoding.
        drain_and_decode(&mut st.peer_streams, &event_tx, &network_tx, &wake);

        if let Some(state) = &mut st.capture
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

            // Advance from the previous target (not from now) so the
            // capture cadence stays exact instead of drifting. The
            // interval reflects the current pace (fps adaptation).
            next_capture += effective_frame_interval(state);

            // If we fell behind, skip to now (don't burst)
            if next_capture < Instant::now() {
                next_capture = Instant::now();
            }

            let cap_ms = cap_start.elapsed().as_secs_f32() * 1000.0;
            log::debug!(
                "📊 ENCODE seq={} {cap_ms:.1}ms (budget {:.1}ms)",
                state.seq,
                effective_frame_interval(state).as_secs_f32() * 1000.0
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
    network_tx: &Sender<NetworkControl>,
    wake: &WakeFn,
) {
    // Cap decode work per pass. An unbounded pass on a backlog spends
    // seconds decoding stale frames inside one loop iteration; bounded,
    // the loop stays live and a real backlog resolves via the buffer's
    // overflow jump-to-freshest-keyframe — skipping stale video instead
    // of replaying it, which is what a live viewer wants.
    const DECODE_BUDGET: Duration = Duration::from_millis(24);
    for (&client_id, stream) in peer_streams.iter_mut() {
        let pass_start = Instant::now();
        while let Some(packet) = stream.jitter_buffer.pull_next() {
            let seq = packet.seq;
            let is_keyframe = packet.is_keyframe;
            let enqueued_at = packet.arrival_time;
            let frame_bytes = packet.data.len();

            if is_keyframe {
                // Flush decoder state before every keyframe. With the
                // encoder forcing true IDRs this is belt-and-braces (an
                // IDR resets reference state anyway), but it is cheap and
                // guarantees no corruption carryover after packet loss.
                // Resolution changes need no special handling — the
                // decoder rebuilds its scaler automatically.
                stream.decoder.flush();

                stream.width = packet.width;
                stream.height = packet.height;
                log::debug!(
                    "📺 Keyframe #{seq} from peer {client_id}: {frame_bytes} bytes \
                     (buffer depth: {:.0}ms)",
                    stream.jitter_buffer.depth_ms()
                );
            }

            let encoded = EncodedFrame {
                data: packet.data,
                timestamp_ms: 0,
                is_keyframe,
                width: packet.width,
                height: packet.height,
            };

            let t0 = Instant::now();
            match stream.decoder.decode(&encoded) {
                Ok(Some(decoded)) => {
                    let decode_ms = t0.elapsed().as_secs_f32() * 1000.0;
                    let decoded_at = Instant::now();
                    // 1080p keyframes cost 12–15 ms in software decode —
                    // that's normal and the jitter buffer absorbs it.
                    // Only flag decodes that actually threaten cadence.
                    let slow_threshold = if is_keyframe { 33.0 } else { 15.0 };
                    if decode_ms > slow_threshold {
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
            if pass_start.elapsed() >= DECODE_BUDGET {
                break;
            }
        }

        // The buffer raises this when it recovered by skipping past lost
        // packets without a keyframe (stream is showing artifacts) or
        // when it's been watching undecodable deltas while unprimed.
        // Rate-limited internally, so relaying every signal is safe.
        if stream.jitter_buffer.take_keyframe_request() {
            info!("📺 Requesting keyframe from peer {client_id}");
            let _ = network_tx.try_send(NetworkControl::RequestKeyframe {
                target_id: client_id,
            });
        }
    }
}

fn handle_control(
    ctrl: ScreenControl,
    st: &mut ScreenState,
    event_tx: &Sender<ScreenEvent>,
    network_tx: &Sender<NetworkControl>,
    shared: &SharedScreenState,
    wake: &WakeFn,
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

            st.capture = Some(CaptureState {
                capturer,
                encoder,
                quality,
                seq: 0,
                system_audio,
                pace_idx: 0,
                pace_changed: Instant::now(),
                pace_good_since: None,
                pace_bad_samples: 0,
                kf_request_times: Vec::new(),
                last_distress: None,
            });

            shared.is_sharing.store(true, Ordering::Relaxed);
            shared.frames_encoded.store(0, Ordering::Relaxed);
            shared.fps_scale_pct.store(100, Ordering::Relaxed);
            shared.stream_loss_pct.store(0, Ordering::Relaxed);
            shared.stream_rtt_ms.store(0, Ordering::Relaxed);
            shared.stream_viewers.store(0, Ordering::Relaxed);
            *shared.current_quality.lock().unwrap() = Some(quality);

            // Tell network to signal other clients. These are the
            // quality *bounds* — the actual encoded dimensions travel
            // with every video frame (aspect-preserving fit).
            let _ = network_tx.send(NetworkControl::ScreenShareStart {
                width: quality.width(),
                height: quality.height(),
            });

            event_tx.send(ScreenEvent::SharingStarted { quality }).ok();
            wake();
        }

        ScreenControl::StopSharing => {
            if st.capture.take().is_some() {
                shared.is_sharing.store(false, Ordering::Relaxed);
                shared.fps_scale_pct.store(100, Ordering::Relaxed);
                shared.stream_loss_pct.store(0, Ordering::Relaxed);
                shared.stream_rtt_ms.store(0, Ordering::Relaxed);
                shared.stream_viewers.store(0, Ordering::Relaxed);
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
            // Decoder init already failed for this peer — drop their
            // frames until they start a new share session.
            if st.decoder_failed.contains(&client_id) {
                return;
            }

            if !st.peer_streams.contains_key(&client_id) {
                match create_decoder() {
                    Ok(decoder) => {
                        info!("📺 Created decoder for peer {client_id} ({width}x{height})");
                        // Assume 30fps initially; will be refined when
                        // Quality is known. The buffer adapts well even
                        // if the FPS guess is off.
                        st.peer_streams.insert(
                            client_id,
                            PeerStream {
                                decoder,
                                width,
                                height,
                                jitter_buffer: VideoJitterBuffer::new(30),
                                stream_audio: None,
                            },
                        );
                    }
                    Err(e) => {
                        // Do NOT panic here — this used to be an .expect
                        // that killed the entire screen thread (and with
                        // it, our own share) on a single failed init.
                        warn!("📺 Decoder init failed for peer {client_id}: {e}");
                        st.decoder_failed.insert(client_id);
                        event_tx
                            .send(ScreenEvent::Error(format!(
                                "Video decoder init failed: {e}"
                            )))
                            .ok();
                        wake();
                        return;
                    }
                }
            }

            if let Some(stream) = st.peer_streams.get_mut(&client_id) {
                stream.jitter_buffer.push(BufferedPacket {
                    seq,
                    is_keyframe,
                    width,
                    height,
                    data,
                    arrival_time: Instant::now(),
                });
            }
        }

        ScreenControl::PeerStartedSharing {
            client_id,
            width,
            height,
        } => {
            // New share session — allow a fresh decoder attempt even if
            // a previous one failed.
            st.decoder_failed.remove(&client_id);
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
            st.peer_streams.remove(&client_id);
            st.decoder_failed.remove(&client_id);
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
            decode_incoming_system_audio(client_id, seq, data, &mut st.peer_streams, voice_energy);
        }

        ScreenControl::SetStreamVolume { client_id, volume } => {
            if let Some(stream) = st.peer_streams.get(&client_id)
                && let Some(ref audio) = stream.stream_audio
            {
                audio.set_volume(volume);
            }
        }

        ScreenControl::MuteStream { client_id, muted } => {
            if let Some(stream) = st.peer_streams.get(&client_id)
                && let Some(ref audio) = stream.stream_audio
            {
                audio.set_muted(muted);
            }
        }

        ScreenControl::RestartSystemAudio => {
            if let Some(state) = &mut st.capture {
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

        ScreenControl::ForceKeyframe => {
            if let Some(state) = &mut st.capture {
                info!("📺 Viewer requested keyframe — forcing IDR");
                state.encoder.force_keyframe();

                let now = Instant::now();
                state.kf_request_times.push(now);
                state
                    .kf_request_times
                    .retain(|&t| now.duration_since(t) < PACE_DISTRESS_WINDOW);
                if state.kf_request_times.len() >= PACE_DISTRESS_REQUESTS
                    && state.pace_idx + 1 < PACE_SCALES.len()
                    && now.duration_since(state.pace_changed) >= PACE_STEP_COOLDOWN
                {
                    state.pace_idx += 1;
                    state.pace_changed = now;
                    state.last_distress = Some(now);
                    state.kf_request_times.clear();
                    info!(
                        "📺 Viewer repeatedly requesting keyframes — reducing capture to {:.0}% fps",
                        PACE_SCALES[state.pace_idx] * 100.0
                    );
                    shared.fps_scale_pct.store(
                        (PACE_SCALES[state.pace_idx] * 100.0) as u32,
                        Ordering::Relaxed,
                    );
                }
            }
        }

        ScreenControl::StreamHealth {
            worst_loss,
            worst_rtt_ms,
            viewers,
        } => {
            shared
                .stream_loss_pct
                .store((worst_loss * 100.0).to_bits(), Ordering::Relaxed);
            shared
                .stream_rtt_ms
                .store(worst_rtt_ms.to_bits(), Ordering::Relaxed);
            shared
                .stream_viewers
                .store(u32::from(viewers), Ordering::Relaxed);
            if let Some(state) = &mut st.capture {
                adapt_pace(state, worst_loss, shared);
            }
        }

        ScreenControl::Shutdown => {
            st.running = false;
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

    // Encode. Returns zero or more access units: zero while a hardware
    // encoder's pipeline fills (first few frames on NVENC/QSV/AMF), and
    // occasionally more than one when it flushes pending output. Each AU
    // gets its own sequence number — the receiver's jitter buffer treats
    // seq gaps as loss, so skipped/empty encodes must not consume seqs.
    let t1 = Instant::now();
    let encoded_frames = match state.encoder.encode(&frame) {
        Ok(e) => e,
        Err(e) => {
            warn!("Encode failed: {e}");
            return;
        }
    };
    let encode_us = t1.elapsed().as_micros() as u32;

    shared.last_capture_us.store(capture_us, Ordering::Relaxed);
    shared.last_encode_us.store(encode_us, Ordering::Relaxed);

    for encoded in encoded_frames {
        if encoded.data.is_empty() {
            continue;
        }

        let seq = state.seq;
        state.seq = state.seq.wrapping_add(1);

        if encoded.is_keyframe {
            log::debug!("📺 Sent keyframe #{seq}: {} bytes", encoded.data.len());
        }

        shared
            .last_frame_bytes
            .store(encoded.data.len() as u32, Ordering::Relaxed);
        shared.frames_encoded.fetch_add(1, Ordering::Relaxed);

        // A frame the chunker can't carry must not become a silent seq
        // gap: every viewer would freeze and burn a PLI round trip
        // (~700 ms) recovering. Drop it AND restart the GOP on the next
        // frame — a one-frame hiccup instead. (Rate control also reacts
        // to the oversized frame, so the retried IDR comes out smaller.)
        if encoded.data.len() > protocol::MAX_VIDEO_FRAME_BYTES {
            warn!(
                "📺 Encoded frame seq={seq} too large to send ({} bytes, max {}) — dropping and forcing a fresh keyframe",
                encoded.data.len(),
                protocol::MAX_VIDEO_FRAME_BYTES,
            );
            state.encoder.force_keyframe();
            continue;
        }

        let _ = network_tx.try_send(NetworkControl::SendVideoFrame {
            seq,
            is_keyframe: encoded.is_keyframe,
            width: encoded.width as u16,
            height: encoded.height as u16,
            data: encoded.data,
        });
    }
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

/// Create the video encoder (FFmpeg is currently the only backend).
fn create_encoder(quality: Quality) -> Result<Box<dyn VideoEncoder + Send>, CodecError> {
    match FfmpegEncoder::new(quality) {
        Ok(enc) => {
            info!("📺 Using FFmpeg encoder: {}", enc.encoder_name());
            Ok(Box::new(enc))
        }
        Err(e) => {
            warn!("FFmpeg encoder unavailable: {e}");
            Err(CodecError::NoEncoder(e.to_string()))
        }
    }
}

/// Create the video decoder (FFmpeg is currently the only backend).
fn create_decoder() -> Result<Box<dyn VideoDecoder + Send>, CodecError> {
    match FfmpegDecoder::new() {
        Ok(dec) => Ok(Box::new(dec)),
        Err(e) => {
            warn!("FFmpeg decoder unavailable: {e}");
            Err(CodecError::DecoderInit(e.to_string()))
        }
    }
}
