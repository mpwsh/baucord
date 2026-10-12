// screen.rs — Screen share capture + encode thread.
//
// Same architecture as voice.rs: dedicated thread, crossbeam channels,
// control/event enums, shared state for UI.
//
// Sender side: capture → encode → MediaOut::VideoFrame
// Receiver side: incoming H.264 frames → jitter buffer → per-peer VideoDecoder → RGBA for display
//
// The jitter buffer decouples packet receipt from decode. Without it,
// keyframe decode stalls (10-16ms) block the channel drain loop, causing
// `try_send` drops on the network thread and consistent sequence gaps
// after every keyframe. With the buffer, packets are appended in O(1)
// and the decoder pulls at its own pace.
//
// System audio: incoming Opus stereo packets are delegated to
// `audio::stream::decoder` — screen.rs never touches Opus directly.
//
// Sender-side adaptation: uplink link samples (from network.rs) drive a
// bitrate ladder on the encoder; viewer keyframe-request distress drives
// a small fps pacer. See the "Sender-side adaptation" section below.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{
    stats::AudioStats,
    stream::{
        decoder::{self as stream_decoder, StreamAudioPacket, StreamDecoder},
        input::{PacketSink, SystemAudioCapture},
    },
};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, SendError, Sender, TrySendError};
use log::{info, warn};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use video::codec::FfmpegDecoder;
use video::{
    buffer::{BufferedPacket, VideoJitterBuffer},
    capture::{CaptureStatus, Capturer},
    codec::FfmpegEncoder,
    CodecError, EncodedFrame, Quality, VideoDecoder, VideoEncoder,
};

pub use crate::viewlog::PresentTally;
use crate::{
    network::{MediaOut, NetworkControl},
    viewlog::{ShareState, Sharing, Viewing},
};

/// What the UI did with each watched stream's frames since its last
/// summary line, by sharer (see [`PresentTally`]).
pub type PresentTallies = Arc<Mutex<HashMap<u64, PresentTally>>>;

// ── Channel messages ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ScreenControl {
    /// Start sharing our screen.
    StartSharing {
        quality: Quality,
        monitor_index: usize,
        /// Who may see the stream (channel-only or the whole server).
        scope: protocol::ShareScope,
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
        /// Sender capture time, ms on the sender's clock (wraps); the
        /// jitter buffer paces playout with it.
        timestamp_ms: u32,
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
    /// We opened a viewer on a peer's stream: its frames are wanted
    /// (again, after [`ScreenControl::StopWatching`]).
    Watch { client_id: u64 },
    /// We closed the viewer: drop the stream's decoder, buffer and
    /// audio now, and ignore frames still in flight for it until the
    /// next [`ScreenControl::Watch`]. Kept, a closed stream went on
    /// writing an empty summary every 5 s, and its thread polling at
    /// 250 Hz, until the sharer stopped (2026-10-08, both phones).
    StopWatching { client_id: u64 },
    /// Whether anyone can see a stream's picture: `client_id: None` is
    /// the whole app (a phone going to the background and back), a peer
    /// is that viewer's window (minimized and restored on the desktop).
    /// Unseen, a stream's frames are dropped before the jitter buffer —
    /// nothing is decoded — while its audio plays on; seen again, it
    /// waits for a keyframe and asks for one (review P1-7).
    SetVisible {
        client_id: Option<u64>,
        visible: bool,
    },
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
    /// Aggregated viewer health for our stream (the server→viewer leg),
    /// relayed by the network thread ~1/s while sharing. Shown in the
    /// sharer's stats block; it does not drive adaptation — see
    /// `UplinkStats` for the leg the sharer can actually do something
    /// about.
    StreamHealth {
        /// Worst viewer "behind" share, 0.0–1.0 (fraction, not
        /// percent): video groups the relay abandoned for that viewer
        /// over groups it started, last second.
        worst_loss: f32,
        worst_rtt_ms: f32,
        viewers: u16,
    },
    /// Our own video uplink as the transport measures it, sampled by
    /// the network thread every ~2 s (`NETWORK_STATS_INTERVAL`). Drives
    /// the encoder bitrate ladder while sharing; ignored otherwise.
    /// Measured on the video itself (spec §6.2), never on transport
    /// ack accounting.
    UplinkStats {
        /// Video groups we had to abandon because their bytes waited
        /// past the deadline, over groups started, in percent (0–100).
        loss_pct: f32,
        rtt_ms: f32,
        /// Bytes on the wire, kbit/s. Video dominates while sharing,
        /// so this is effectively the encoder's output rate.
        kbps_up: f32,
        /// Groups started and abandoned in the window, and the
        /// transport's packet loss (the sharer's summary line).
        groups_started: u32,
        groups_reset: u32,
        path_loss_pct: f32,
    },
    /// Clean shutdown.
    Shutdown,
}

/// What travels with a decoded frame from the screen thread to the
/// window that shows it: whose it is, its size, and the figures the
/// viewer's stats line and the stream's summary report. One struct all
/// the way (it was listed field by field four times; review P3-36): a
/// new figure is added here and where it is read.
#[derive(Debug, Clone, Copy)]
pub struct FrameMeta {
    pub client_id: u64,
    pub width: u32,
    pub height: u32,
    /// The encoded frame's size.
    pub frame_bytes: usize,
    pub seq: u32,
    /// When the jitter buffer released the frame for decoding (after
    /// its playout hold), so `decoded_at - enqueued_at` is decode
    /// latency alone.
    pub enqueued_at: Instant,
    pub decoded_at: Instant,
    pub buffer_depth_ms: f32,
    pub buffer_target_ms: f32,
    pub buffer_jitter_ms: f32,
}

#[derive(Debug, Clone)]
pub enum ScreenEvent {
    /// Our screen share started successfully.
    SharingStarted { quality: Quality },
    /// Our screen share stopped.
    SharingStopped,
    /// Error during capture or encode.
    Error(String),
    /// A decoded frame is ready for display. `pixels` are laid out as
    /// `format` says: RGBA from the desktop decoders, the codec's YUV
    /// planes on Android, converted only if the frame is the one that
    /// gets shown. They come from `video::pool`: give them back with
    /// `video::pool::recycle` when done.
    FrameReady {
        meta: FrameMeta,
        pixels: Vec<u8>,
        format: video::PictureFormat,
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
    /// Size of the last encoded frame in bytes.
    pub last_frame_bytes: Arc<AtomicU32>,
    /// Encode time of the last frame in microseconds.
    pub last_encode_us: Arc<AtomicU32>,
    /// Selected quality by streamer
    pub current_quality: Arc<Mutex<Option<Quality>>>,
    /// Worst packet loss among our viewers, in percent, stored as f32
    /// bits — read with `f32::from_bits`. Updated ~1/s while sharing.
    pub stream_loss_pct: Arc<AtomicU32>,
    /// Number of viewers currently subscribed to our stream.
    pub stream_viewers: Arc<AtomicU32>,
    /// Current capture pace as a percentage of the preset fps
    /// (100 / 75 / 50) — stepped down under viewer keyframe-request
    /// distress, see `PACE_SCALES`.
    pub fps_scale_pct: Arc<AtomicU32>,
    /// Filled by the frontend as it shows watched streams; read by the
    /// viewer's summary line.
    pub present: PresentTallies,
    /// The voice output picked in Settings, set by the frontend: stream
    /// audio plays there too, and moves when it changes (review P1-19).
    pub stream_output: audio::stream::OutputSelection,
}

impl Default for SharedScreenState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedScreenState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            is_sharing: Arc::new(AtomicBool::new(false)),
            last_frame_bytes: Arc::new(AtomicU32::new(0)),
            last_encode_us: Arc::new(AtomicU32::new(0)),
            current_quality: Arc::new(Mutex::new(None)),
            stream_loss_pct: Arc::new(AtomicU32::new(0)),
            stream_viewers: Arc::new(AtomicU32::new(0)),
            fps_scale_pct: Arc::new(AtomicU32::new(100)),
            present: Arc::new(Mutex::new(HashMap::new())),
            stream_output: audio::stream::OutputSelection::default(),
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────

/// The way in to the screen threads: one sender to callers, two
/// threads behind it. Our own share (capture, encode) and the streams
/// we watch (reassembly, decode, their audio) each run the same loop on
/// a thread of their own, and a command goes to the one it is for. On
/// one thread a sharer who also watched lost capture ticks to decoding:
/// a decode pass may take 24 ms, a 60 fps tick comes every 16.7 (review
/// P2-44).
#[derive(Clone)]
pub struct ScreenTx {
    share: Sender<ScreenControl>,
    watch: Sender<ScreenControl>,
}

impl ScreenTx {
    /// The thread `ctrl` is for; `None` for the one command both get.
    fn lane(&self, ctrl: &ScreenControl) -> Option<&Sender<ScreenControl>> {
        match ctrl {
            ScreenControl::StartSharing { .. }
            | ScreenControl::StopSharing
            | ScreenControl::RestartSystemAudio
            | ScreenControl::ForceKeyframe
            | ScreenControl::StreamHealth { .. }
            | ScreenControl::UplinkStats { .. } => Some(&self.share),
            ScreenControl::IncomingVideoFrame { .. }
            | ScreenControl::PeerStartedSharing { .. }
            | ScreenControl::PeerStoppedSharing { .. }
            | ScreenControl::Watch { .. }
            | ScreenControl::StopWatching { .. }
            | ScreenControl::SetVisible { .. }
            | ScreenControl::IncomingSystemAudio { .. }
            | ScreenControl::SetStreamVolume { .. }
            | ScreenControl::MuteStream { .. } => Some(&self.watch),
            ScreenControl::Shutdown => None,
        }
    }

    /// Send, waiting for room.
    ///
    /// # Errors
    /// The thread is gone; the command comes back.
    pub fn send(&self, ctrl: ScreenControl) -> Result<(), SendError<ScreenControl>> {
        if let Some(lane) = self.lane(&ctrl) {
            return lane.send(ctrl);
        }
        let share = self.share.send(ScreenControl::Shutdown);
        self.watch.send(ctrl).and(share)
    }

    /// Send if there is room.
    ///
    /// # Errors
    /// The thread is behind (`Full`) or gone; the command comes back.
    pub fn try_send(&self, ctrl: ScreenControl) -> Result<(), TrySendError<ScreenControl>> {
        if let Some(lane) = self.lane(&ctrl) {
            return lane.try_send(ctrl);
        }
        let share = self.share.try_send(ScreenControl::Shutdown);
        self.watch.try_send(ctrl).and(share)
    }
}

/// One channel for everything: a test's stand-in for the threads.
impl From<Sender<ScreenControl>> for ScreenTx {
    fn from(tx: Sender<ScreenControl>) -> Self {
        Self {
            share: tx.clone(),
            watch: tx,
        }
    }
}

pub struct ScreenThreadHandle {
    pub tx: ScreenTx,
    pub event_rx: Receiver<ScreenEvent>,
    pub shared: SharedScreenState,
    /// Joined by the frontend on shutdown (with a timeout).
    pub join_handle: Option<thread::JoinHandle<()>>,
}

impl ScreenThreadHandle {
    /// Start the screen threads ([`ScreenTx`]): `screen-share` for our
    /// own share, `screen-watch` for the streams we watch. Neither
    /// wakes the UI: whoever reads `event_rx` does — the frontend's
    /// present thread, which wakes the UI once it has forwarded an
    /// event or prepared a frame. A wake here as well was an empty
    /// drain of the UI per decoded frame, with a phone in the
    /// background too (review P1-8). A thread with nothing to do blocks
    /// on its channel and costs nothing. Dropping the handle stops
    /// both.
    #[must_use]
    pub fn spawn(network_tx: Sender<NetworkControl>, media_tx: Sender<MediaOut>) -> Self {
        // 256 slots: at 30fps inbound + outbound control messages, 64 was
        // too tight and caused try_send drops during active streaming.
        let (share_tx, share_rx) = bounded::<ScreenControl>(256);
        let (watch_tx, watch_rx) = bounded::<ScreenControl>(256);
        let (event_tx, event_rx) = bounded::<ScreenEvent>(256);
        let shared = SharedScreenState::new();

        let share = {
            let (event_tx, network_tx, media_tx, shared) = (
                event_tx.clone(),
                network_tx.clone(),
                media_tx.clone(),
                shared.clone(),
            );
            thread::Builder::new()
                .name("screen-share".into())
                .spawn(move || run_screen_loop(share_rx, event_tx, network_tx, media_tx, shared))
                .expect("failed to spawn screen thread")
        };
        let shared_clone = shared.clone();
        // The one handle callers join: it ends when both have.
        let join_handle = thread::Builder::new()
            .name("screen-watch".into())
            .spawn(move || {
                run_screen_loop(watch_rx, event_tx, network_tx, media_tx, shared_clone);
                let _ = share.join();
            })
            .expect("failed to spawn screen thread");

        Self {
            tx: ScreenTx {
                share: share_tx,
                watch: watch_tx,
            },
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

/// The sharer's summary line every 5 s (`viewlog::Sharing`), and the
/// rolling share telemetry for the server every 10 s: fresh capture
/// rate, skipped (unchanged) tick rate, and encoded output rate. On a
/// healthy 60 fps share of moving content, fresh and encode both read
/// ~60/s; fresh well below the target while encode tracks it means the
/// *source* isn't changing that fast (30 fps video content, display
/// refresh cap) rather than a pipeline problem.
fn report_share_telemetry(
    state: &mut CaptureState,
    network_tx: &Sender<NetworkControl>,
    shared: &SharedScreenState,
) {
    const TELEMETRY_INTERVAL: Duration = Duration::from_secs(10);
    // Called on every capture tick: a name, not a `String` made for it
    // each time (review P3-16).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a percentage
    let share = ShareState {
        preset: state.quality.label(),
        bitrate_pct: state.ladder.pct(),
        fps_pct: (PACE_SCALES[state.pace_idx] * 100.0).round() as u32,
        viewers: shared.stream_viewers.load(Ordering::Relaxed),
        qp_floor: state.encoder.qp_floor(),
    };
    if let Some((level, line)) = state.log.poll(Instant::now(), &share) {
        log::log!(level, "{line}");
    }
    let (start, fresh, skipped, encoded, single, multi) = state.telemetry;
    let elapsed = start.elapsed();
    if elapsed < TELEMETRY_INTERVAL {
        return;
    }
    let secs = elapsed.as_secs_f32();
    log::debug!(
        "share telemetry: {:.1} fresh/s, {:.1} skipped/s, {:.1} encoded/s over {:.0}s (spacing: {:.1} single/s, {:.1} bursty/s)",
        fresh as f32 / secs,
        skipped as f32 / secs,
        encoded as f32 / secs,
        secs,
        single as f32 / secs,
        multi as f32 / secs
    );
    // The same numbers ride to the server (spec §9.1); the network
    // thread fills in the transport's group counters.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let _ = network_tx.try_send(NetworkControl::TelemetrySharing(Some(
        protocol::SharingSample {
            preset: state.quality.label().to_string(),
            rung_pct: state.ladder.pct(),
            fps_encoded: encoded as f32 / secs,
            fps_scale_pct: (PACE_SCALES[state.pace_idx] * 100.0).round() as u32,
            forced_keyframes: state.forced_keyframes,
            keyframe_requests: state.keyframe_requests,
            encoder: state.encoder_name.to_string(),
            groups_reset: 0,
        },
    )));
    state.telemetry = (Instant::now(), 0, 0, 0, 0, 0);
}

struct CaptureState {
    capturer: Capturer,
    encoder: Box<dyn VideoEncoder + Send>,
    quality: Quality,
    seq: u32,
    /// Next time an *unchanged* frame gets encoded anyway. Streaming
    /// capture only delivers frames when the screen changes, so this
    /// low-rate refresh keeps the stream primed (periodic IDRs for
    /// joiners, live-looking stats) while the screen is static. With
    /// the screenshot backend every poll is Fresh and this never fires.
    next_refresh: Instant,
    /// While `Some` and not yet expired, the static-screen encode skip
    /// is suspended: a viewer asked for a keyframe and this encoder
    /// (AMF) can only emit IDRs after enough *submitted* frames, so
    /// frames must keep flowing until one ships. Cleared when a
    /// keyframe AU is sent.
    keyframe_burst_until: Option<Instant>,
    /// The last change went into an encoder that hands out an access
    /// unit only when given another frame (`output_needs_input`, AMF):
    /// the next still tick repeats the held frame once to bring it out.
    flush_owed: bool,
    /// Rolling 10 s telemetry: (window start, fresh captures, skipped
    /// unchanged ticks, encoded AUs, single-AU encodes, multi-AU
    /// encodes). The last two measure output *spacing*: multi-AU
    /// encodes mean the encoder is emitting in bursts (pairs on the
    /// wire), which viewers' UI coalescing collapses — the halved-fps
    /// signature that rate counters alone can't see.
    telemetry: (Instant, u32, u32, u32, u32, u32),
    /// Keyframe AUs whose NAL layout has been logged this session (the
    /// first few are logged at INFO as a bitstream self-check — makes
    /// hardware-encoder quirks visible from the sender's log alone).
    keyframes_logged: u8,
    /// System audio capture — started alongside video, sends Opus stereo
    /// packets to the network. `None` if platform doesn't support it.
    system_audio: Option<SystemAudioCapture>,
    /// Not before this is a capture that stopped by itself restarted
    /// again (`revive_system_audio`).
    system_audio_retry: Option<Instant>,
    /// Encoder bitrate ladder (see "Sender-side adaptation").
    ladder: BitrateLadder,
    /// The sharer's summary line (`viewlog`).
    log: Sharing,
    /// Encoded bytes handed to the network since `offered_since`: what
    /// the share asked the uplink to carry, abandoned groups included.
    /// Read and reset with each uplink sample (`kbps_offered`).
    offered_bytes: u64,
    offered_since: Instant,
    /// Most recent uplink sample (groups abandoned, percent). Gates the
    /// fps pacer: keyframe requests while our own uplink is behind are
    /// not viewer distress.
    uplink_loss_pct: f32,
    /// Encoder backend name, for telemetry.
    encoder_name: &'static str,
    /// IDRs we forced (viewer requests, oversized-frame restarts).
    forced_keyframes: u64,
    /// Keyframe requests relayed to us by viewers.
    keyframe_requests: u64,
    /// Worst viewer loss from the last `StreamHealth`, percent. Same
    /// gate, other leg.
    viewer_loss_pct: f32,
    /// Last time viewer health was logged (rate limit).
    health_logged: Option<Instant>,
    /// Index into `PACE_SCALES` — current fps pace step.
    pace_idx: usize,
    /// When the pace last changed (cooldown).
    pace_changed: Instant,
    /// Recent viewer keyframe-request arrivals (distress detector).
    kf_request_times: Vec<Instant>,
    /// Last time viewer distress stepped the pace down.
    last_distress: Option<Instant>,
}

// ── Sender-side adaptation ──────────────────────────────────────────────
//
// Two loops, two jobs:
//
// * The **bitrate ladder** protects our uplink. network.rs samples the
//   share of video groups the transport abandoned (spec §6.2), RTT and
//   send rate every ~2 s; sustained loss
//   steps the encoder's bitrate scale down a rung, a long clean stretch
//   steps it back up. Resolution and frame rate never change: every
//   frame still goes out, each with fewer bits — a softer picture that
//   stays smooth, rather than a sharp one that freezes.
//
// * The **fps pacer** handles the one case bitrate can't: a viewer that
//   keeps asking for keyframes while our uplink is clean is failing on
//   *its* side (a decoder that can't keep up, downlink loss we can't
//   see). Fewer frames per second is the remedy there.
//
// Why the ladder watches the uplink and not viewer reports: in the
// 2026-08-28 incident a 1080p60 share cycled 60→30→60 fps for eight
// minutes. The pacer only learned of the problem from viewer keyframe
// requests, 24 s after the link monitor first logged loss, and stepped
// back up on a timer keyed to the server→viewer leg — which was clean
// throughout, because packets the uplink dropped never reached the
// server to be lost again. So it re-saturated the uplink every ~40 s.
// This loop reacts to the leg that failed, in ~4 s, and only climbs
// once the uplink has carried the current rung cleanly for a while.

/// Rungs, percent of the preset bitrate. Descending; index 0 is full
/// rate.
const RATE_LADDER_PCT: [u32; 5] = [100, 70, 50, 35, 25];
/// No rung runs below this many bits/s whatever the preset. A rung the
/// floor lifts onto the one above it is unreachable (HD30's 25% would be
/// 875 kbit/s; its ladder ends at 35% ≈ 1.2 Mbit).
const RATE_FLOOR_BPS: u32 = 1_200_000;
/// Uplink loss (percent) at or above which a sample counts against the
/// current rung.
const RATE_LOSS_STEP_DOWN: f32 = 10.0;
/// Loss at or above this steps two rungs at once. Every incident episode
/// crossed 50% within 4 s of onset, and the rung one down demonstrably
/// didn't fit either — dwelling there is 10 s of frozen stream.
const RATE_LOSS_SEVERE: f32 = 30.0;
/// Loss below which a sample counts toward climbing.
const RATE_LOSS_RECOVERED: f32 = 2.0;
/// Consecutive lossy samples before stepping down (~4 s). One sample is
/// a blip; two is a trend.
const RATE_STEP_DOWN_SAMPLES: u32 = 2;
/// Consecutive clean samples before stepping up (~30 s), multiplied by
/// the probe backoff.
const RATE_STEP_UP_SAMPLES: u32 = 15;
/// Minimum time at a rung before stepping again. Groups are up to 2 s
/// long and a reset shows up in the *next* sample, so a step's effect
/// takes a few samples to appear in the figure; stepping again before
/// that acts on stale data. Not applied to a step down
/// that follows a climb — the link was clean for the whole hold before
/// the climb, so loss right after it is fresh evidence, and the probe
/// is undone as soon as it's detected.
const RATE_DWELL: Duration = Duration::from_secs(10);
/// A step down this soon after a step up is a failed probe: the next
/// climb waits twice as long, up to `RATE_PROBE_BACKOFF_MAX` times the
/// base hold. This is what stops a link that fits at 50% but not 70%
/// from being re-probed every 30 s.
const RATE_PROBE_FAIL_WINDOW: Duration = Duration::from_secs(20);
const RATE_PROBE_BACKOFF_MAX: u32 = 4;
/// The link only proves or disproves a rung while it is carrying it.
/// Samples with `up` below this fraction of the rung's bitrate (static
/// screen, nothing to send) count neither way: loss then isn't ours to
/// shed, and a clean reading says nothing about whether the rung fits.
///
/// For a step down, what the share *offered* counts as well as what
/// went out: an uplink that collapsed under the rung sends little
/// because it is overloaded, not because the screen is idle. On
/// 2026-10-08 a 1080p60 share (8 Mbit/s) sat at full rate for two
/// minutes with nearly every group abandoned: QUIC's window had shrunk
/// to 2 KB, `up` read ~200 kbit/s — under a quarter of the rung — and
/// every sample was taken for an idle screen.
const RATE_EXERCISED_STEP_DOWN: f32 = 0.25;
const RATE_EXERCISED_STEP_UP: f32 = 0.5;
/// Overloaded: the encoder offered at least this many times what the
/// uplink carried, and at least `RATE_OVERLOAD_MIN_KBPS` — a lossy
/// sample then counts against the rung whatever its size. (The floor
/// keeps a static screen's trickle of refresh frames out of it.)
const RATE_OVERLOAD_RATIO: f32 = 2.0;
const RATE_OVERLOAD_MIN_KBPS: f32 = 250.0;

/// One uplink sample, as the ladder reads it.
#[derive(Clone, Copy, Debug)]
struct LinkSample {
    /// Video groups abandoned over groups started, percent.
    loss_pct: f32,
    rtt_ms: f32,
    /// What went out on the wire, kbit/s (all traffic).
    kbps_up: f32,
    /// What the encoder handed to the network over about the same
    /// window, kbit/s — abandoned groups included.
    kbps_offered: f32,
}

/// Percent of the preset a rung actually runs at: the ladder value,
/// lifted to `RATE_FLOOR_BPS` when the preset would put it lower.
fn rung_pct(quality: Quality, idx: usize) -> u32 {
    let floor_pct = RATE_FLOOR_BPS.div_ceil(quality.bitrate() / 100);
    RATE_LADDER_PCT[idx].max(floor_pct).min(100)
}

/// Lowest rung index reachable for this preset — the last one that is
/// still strictly below the rung above it after the floor is applied.
fn lowest_rung(quality: Quality) -> usize {
    (1..RATE_LADDER_PCT.len())
        .take_while(|&i| rung_pct(quality, i) < rung_pct(quality, i - 1))
        .last()
        .unwrap_or(0)
}

/// The ladder's state machine, kept free of I/O so it can be unit
/// tested: [`BitrateLadder::sample`] takes one link sample and returns
/// the rung change to apply, if any. `adapt_bitrate` owns the side
/// effects (encoder, shared state, log).
struct BitrateLadder {
    quality: Quality,
    /// Index into `RATE_LADDER_PCT`.
    idx: usize,
    /// When the rung last changed; `None` until the first change so a
    /// share that starts on a saturated uplink doesn't sit out a dwell.
    changed: Option<Instant>,
    /// Consecutive lossy, link-exercised samples (spike filter).
    bad_samples: u32,
    /// Consecutive clean, link-exercised samples (climb hold).
    good_samples: u32,
    /// Multiplier on the climb hold: 1, 2, 4 — doubled by each failed
    /// probe, reset by a step down that wasn't one.
    probe_backoff: u32,
    /// When the ladder last stepped up; a step down soon after is a
    /// failed probe.
    last_step_up: Option<Instant>,
}

/// A rung transition decided by [`BitrateLadder::sample`].
struct RungChange {
    /// New bitrate scale, percent of the preset.
    pct: u32,
    /// Human-readable trigger, for the log line.
    why: String,
}

impl BitrateLadder {
    fn new(quality: Quality) -> Self {
        Self {
            quality,
            idx: 0,
            changed: None,
            bad_samples: 0,
            good_samples: 0,
            probe_backoff: 1,
            last_step_up: None,
        }
    }

    /// Current bitrate scale, percent of the preset.
    fn pct(&self) -> u32 {
        rung_pct(self.quality, self.idx)
    }

    /// Whether the ladder is below full rate.
    fn engaged(&self) -> bool {
        self.idx > 0
    }

    /// Apply one uplink sample and decide whether the rung changes.
    #[allow(clippy::cast_precision_loss)] // kbit/s comparisons, exactness irrelevant
    fn sample(&mut self, now: Instant, link: LinkSample) -> Option<RungChange> {
        let LinkSample {
            loss_pct,
            rtt_ms,
            kbps_up,
            kbps_offered,
        } = link;
        let rung_kbps = self.quality.scaled_bitrate(self.pct()) as f32 / 1000.0;
        let exercised = |fraction: f32| kbps_up >= rung_kbps * fraction;
        // See `RATE_EXERCISED_STEP_DOWN`.
        let loaded = exercised(RATE_EXERCISED_STEP_DOWN)
            || kbps_offered >= rung_kbps * RATE_EXERCISED_STEP_DOWN
            || (kbps_offered >= RATE_OVERLOAD_MIN_KBPS
                && kbps_offered >= kbps_up * RATE_OVERLOAD_RATIO);
        let dwelled = self
            .changed
            .is_none_or(|t| now.duration_since(t) >= RATE_DWELL);
        let last_change_was_climb = self.changed.is_some() && self.changed == self.last_step_up;

        if loss_pct >= RATE_LOSS_STEP_DOWN {
            self.good_samples = 0;
            if !loaded {
                // Lossy but idle: not our traffic, nothing to shed.
                self.bad_samples = 0;
                return None;
            }
            self.bad_samples += 1;
            let lowest = lowest_rung(self.quality);
            if self.bad_samples < RATE_STEP_DOWN_SAMPLES
                || !(dwelled || last_change_was_climb)
                || self.idx >= lowest
            {
                return None;
            }
            let failed_probe = self
                .last_step_up
                .take()
                .is_some_and(|t| now.duration_since(t) < RATE_PROBE_FAIL_WINDOW);
            // A failed probe goes back exactly where it came from — that
            // rung was known good; overshooting it would waste quality.
            let rungs = if !failed_probe && loss_pct >= RATE_LOSS_SEVERE {
                2
            } else {
                1
            };
            let next = (self.idx + rungs).min(lowest);
            self.probe_backoff = if failed_probe {
                (self.probe_backoff * 2).min(RATE_PROBE_BACKOFF_MAX)
            } else {
                1
            };
            let note = if failed_probe {
                format!(
                    "; probe failed, next climb needs {} clean samples",
                    RATE_STEP_UP_SAMPLES * self.probe_backoff
                )
            } else {
                String::new()
            };
            let why = format!(
                "uplink loss {loss_pct:.0}%, rtt {rtt_ms:.0} ms, up {kbps_up:.0} kbit/s, \
                 offered {kbps_offered:.0} kbit/s{note}"
            );
            Some(self.move_to(next, now, why))
        } else if loss_pct < RATE_LOSS_RECOVERED {
            self.bad_samples = 0;
            if !self.engaged() || !exercised(RATE_EXERCISED_STEP_UP) {
                return None;
            }
            self.good_samples += 1;
            let needed = RATE_STEP_UP_SAMPLES * self.probe_backoff;
            if self.good_samples < needed || !dwelled {
                return None;
            }
            self.last_step_up = Some(now);
            let why = format!(
                "uplink clean for {needed} samples, rtt {rtt_ms:.0} ms, up {kbps_up:.0} kbit/s"
            );
            Some(self.move_to(self.idx - 1, now, why))
        } else {
            // Middle band: hold the rung, restart both clocks.
            self.bad_samples = 0;
            self.good_samples = 0;
            None
        }
    }

    fn move_to(&mut self, idx: usize, now: Instant, why: String) -> RungChange {
        self.idx = idx;
        self.changed = Some(now);
        self.bad_samples = 0;
        self.good_samples = 0;
        RungChange {
            pct: self.pct(),
            why,
        }
    }
}

/// Feed one uplink sample to the ladder and apply its decision.
fn adapt_bitrate(state: &mut CaptureState, now: Instant, link: LinkSample) {
    state.uplink_loss_pct = link.loss_pct;
    let from = state.ladder.pct();
    if let Some(change) = state.ladder.sample(now, link) {
        state.encoder.set_bitrate_scale_pct(change.pct);
        info!(
            "bitrate ladder: {from}% → {}% ({} kbit/s; {})",
            change.pct,
            state.quality.scaled_bitrate(change.pct) / 1000,
            change.why
        );
    }
}

// ── Viewer-distress fps pacer ───────────────────────────────────────────

const PACE_SCALES: [f32; 3] = [1.0, 0.75, 0.5];
/// Minimum time between pace changes.
const PACE_STEP_COOLDOWN: Duration = Duration::from_secs(2);
/// Repeated viewer keyframe requests mean a viewer keeps failing to
/// keep a picture. This many requests within `PACE_DISTRESS_WINDOW` steps
/// the pace down — unless loss on either leg explains them (see
/// `PACE_LOSS_GATE_PCT`), in which case fewer frames per second fixes
/// nothing and the transport's resends / the bitrate ladder are the
/// remedy.
const PACE_DISTRESS_REQUESTS: usize = 4;
/// Loss (percent) on our uplink or at the worst viewer at or above
/// which keyframe requests are attributed to loss, not to a viewer that
/// can't keep up. Low on purpose: ~16 chunks per frame means even 2%
/// packet loss is a stream in constant recovery — the 2026-08-29 logs
/// show the pacer firing 25 times in one share on exactly that, cutting
/// the sharer to 30 fps for nothing.
const PACE_LOSS_GATE_PCT: f32 = 2.0;
const PACE_DISTRESS_WINDOW: Duration = Duration::from_secs(5);
/// Don't step back up for this long after viewer distress — stepping up
/// the moment the viewer goes quiet just re-chokes them in a slow cycle.
const PACE_DISTRESS_HOLD: Duration = Duration::from_secs(30);
/// Rate limit on the `viewers:` health log line.
const HEALTH_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// While the screen is static (streaming backend reports Unchanged),
/// re-encode the held frame this often. Identical-content P-frames are
/// near-zero bytes, and the encoder leaves them out of its keyframe
/// cadence (`encode_repeat`; a still share sent a full IDR every 2 s
/// before — review P2-41), so a static share costs next to nothing. A
/// joining viewer asks for its keyframe: a pending forced keyframe
/// (viewer waiting on an IDR) bypasses this and encodes on the next
/// tick.
const STATIC_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Upper bound on a keyframe "burst" (see `CaptureState::keyframe_burst_until)`:
/// when the encoder can't force IDRs (AMF), a viewer's keyframe request
/// makes us encode every tick — static screen or not — until the
/// encoder's own IDR period elapses. At the 2 s AMF IDR period a burst
/// resolves within ~2 s of full-rate frames; this cap just guarantees a
/// misbehaving encoder can't pin us at full rate forever.
const KEYFRAME_BURST_MAX: Duration = Duration::from_secs(5);

/// How long a system-audio capture that stopped by itself waits before
/// it is started again (and again, if it keeps stopping).
const SYSTEM_AUDIO_RETRY: Duration = Duration::from_secs(10);

/// Stop the share's system-audio capture, if any, and start a new one —
/// best-effort: a share without sound is still a share.
fn restart_system_audio(state: &mut CaptureState, media_tx: &Sender<MediaOut>, why: &str) {
    // Stop the old capture (drops the dead WASAPI loopback stream).
    if let Some(mut old) = state.system_audio.take() {
        old.stop();
        info!("System audio capture stopped ({why})");
    }
    match SystemAudioCapture::new(Arc::new(AudioStats::new()), system_audio_sink(media_tx)) {
        Ok(mut cap) => match cap.start() {
            Ok(()) => {
                info!("System audio capture restarted ({why})");
                state.system_audio = Some(cap);
            }
            Err(e) => warn!("System audio capture restart failed ({why}): {e}"),
        },
        Err(e) => warn!("System audio capture unavailable ({why}): {e}"),
    }
}

/// A system-audio capture that gave up (its device went, the audio
/// engine kept failing) is started again. It used to log that the share
/// went on without sound and leave it so until the share ended (review
/// P2-22). Called on every capture tick: two loads when all is well.
fn revive_system_audio(state: &mut CaptureState, media_tx: &Sender<MediaOut>) {
    if !state
        .system_audio
        .as_ref()
        .is_some_and(SystemAudioCapture::failed)
    {
        return;
    }
    let now = Instant::now();
    if state.system_audio_retry.is_some_and(|at| now < at) {
        return;
    }
    state.system_audio_retry = Some(now + SYSTEM_AUDIO_RETRY);
    restart_system_audio(state, media_tx, "it stopped by itself");
}

/// The capture interval at the current pace.
/// Our share's sound goes to the network thread as each 20 ms packet is
/// encoded, from the encoder's own thread. It used to wait for this
/// thread's capture tick and leave in bursts of a frame interval (33 ms
/// at 30 fps, 67 at the slowest pace), which every viewer had to buffer
/// on top (PRODUCTION-PLAN.md LAT-2). A full channel drops the packet.
fn system_audio_sink(media_tx: &Sender<MediaOut>) -> PacketSink {
    let media_tx = media_tx.clone();
    Arc::new(move |packet| {
        let sent = media_tx.try_send(MediaOut::SystemAudio {
            seq: packet.seq,
            channels: packet.channels,
            duration_ms: u16::from(packet.duration_ms),
            data: packet.data,
        });
        !matches!(sent, Err(TrySendError::Disconnected(_)))
    })
}

fn effective_frame_interval(state: &CaptureState) -> Duration {
    state
        .quality
        .frame_duration()
        .mul_f32(1.0 / PACE_SCALES[state.pace_idx])
}

/// Record a viewer keyframe request and step the pace down if requests
/// have become a pattern — but only when loss on neither leg explains
/// them (see `PACE_DISTRESS_REQUESTS`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn note_keyframe_request(state: &mut CaptureState, now: Instant, shared: &SharedScreenState) {
    state.kf_request_times.push(now);
    state
        .kf_request_times
        .retain(|&t| now.duration_since(t) < PACE_DISTRESS_WINDOW);
    let loss_explains_it = state.ladder.engaged()
        || state.uplink_loss_pct >= PACE_LOSS_GATE_PCT
        || state.viewer_loss_pct >= PACE_LOSS_GATE_PCT;
    if loss_explains_it
        || state.kf_request_times.len() < PACE_DISTRESS_REQUESTS
        || state.pace_idx + 1 >= PACE_SCALES.len()
        || now.duration_since(state.pace_changed) < PACE_STEP_COOLDOWN
    {
        return;
    }
    state.pace_idx += 1;
    state.pace_changed = now;
    state.last_distress = Some(now);
    state.kf_request_times.clear();
    info!(
        "Viewer repeatedly requesting keyframes — reducing capture to {:.0}% fps",
        PACE_SCALES[state.pace_idx] * 100.0
    );
    shared.fps_scale_pct.store(
        (PACE_SCALES[state.pace_idx] * 100.0) as u32,
        Ordering::Relaxed,
    );
}

/// Step the pace back up once viewers have been quiet for the distress
/// hold. Runs on the uplink sample tick (~2 s).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn recover_pace(state: &mut CaptureState, now: Instant, shared: &SharedScreenState) {
    if state.pace_idx == 0 {
        return;
    }
    state
        .kf_request_times
        .retain(|&t| now.duration_since(t) < PACE_DISTRESS_WINDOW);
    let held = state
        .last_distress
        .is_none_or(|t| now.duration_since(t) >= PACE_DISTRESS_HOLD);
    if !state.kf_request_times.is_empty()
        || !held
        || now.duration_since(state.pace_changed) < PACE_STEP_COOLDOWN
    {
        return;
    }
    state.pace_idx -= 1;
    state.pace_changed = now;
    info!(
        "Viewers quiet — restoring capture to {:.0}% fps",
        PACE_SCALES[state.pace_idx] * 100.0
    );
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
    /// Frames dropped by the jitter buffer / decoder since the stream
    /// opened, and cumulative decode time — for the telemetry sample.
    frames_decoded_total: u64,
    frames_dropped_total: u64,
    decode_us_total: u64,
    /// When the server last got this stream's viewing counters (every
    /// 10 s, spec §9.1).
    reported_at: Instant,
    /// The log: a summary every 5 s and a line when something changes
    /// (`viewlog`).
    log: Viewing,
    /// Jitter buffer for this peer's video stream. Holds encoded packets
    /// and releases them in sequence order at a controlled rate.
    jitter_buffer: VideoJitterBuffer,
    /// Stream audio decoder + playback pipeline for this peer.
    /// Lazy-initialized on the first system audio packet.
    /// Owned by the core library — screen.rs just feeds it raw Opus bytes.
    stream_audio: Option<StreamDecoder>,
    /// The frame last fed to the decoder: what a picture handed out by
    /// `VideoDecoder::poll` (between frames) is reported as.
    last_fed: Option<Fed>,
}

/// One frame as fed to a decoder, for the picture that comes of it.
#[derive(Clone, Copy)]
struct Fed {
    seq: u32,
    frame_bytes: usize,
    /// The sender's capture timestamp (for "behind live").
    sent_at: u32,
    /// When the jitter buffer released it.
    enqueued_at: Instant,
}

/// The server's viewing counters (spec §9.1).
const VIEWING_REPORT_EVERY: Duration = Duration::from_secs(10);

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
    /// Peers whose viewer we closed: their late frames are dropped.
    unwatched: HashSet<u64>,
    /// Nobody sees any stream: the phone app is in the background.
    host_hidden: bool,
    /// Peers whose viewer window is minimized.
    minimized: HashSet<u64>,
    running: bool,
}

impl ScreenState {
    /// Whether anyone can see `client_id`'s picture right now.
    fn sees(&self, client_id: u64) -> bool {
        !self.host_hidden && !self.minimized.contains(&client_id)
    }

    /// [`ScreenControl::SetVisible`].
    fn set_visible(&mut self, client_id: Option<u64>, visible: bool) {
        let unseen: Vec<u64> = self
            .peer_streams
            .keys()
            .copied()
            .filter(|&id| !self.sees(id))
            .collect();
        match client_id {
            None => self.host_hidden = !visible,
            Some(id) if visible => {
                self.minimized.remove(&id);
            }
            Some(id) => {
                self.minimized.insert(id);
            }
        }
        // A stream seen again starts over from a keyframe: the frames in
        // between were never decoded. The request goes out with the next
        // drain, once (the buffer rate-limits it).
        let now = Instant::now();
        for id in unseen {
            if self.sees(id)
                && let Some(stream) = self.peer_streams.get_mut(&id)
            {
                stream.jitter_buffer.wait_for_keyframe(now);
                log::debug!("video from {id} on screen again: asking for a keyframe");
            }
        }
    }
}

/// When the loop next has work of its own: the next capture tick while
/// sharing, and per watched stream the jitter buffer's next release, a
/// picture its decoder may still hand out, its next log line. `None`
/// when only a command can bring work.
fn next_wake(st: &ScreenState, next_capture: Instant, now: Instant) -> Option<Instant> {
    let capture = st.capture.is_some().then_some(next_capture);
    st.peer_streams
        .values()
        .flat_map(|s| {
            [
                s.jitter_buffer.next_due(now),
                s.decoder.next_poll(now),
                Some(s.log.next_due()),
            ]
        })
        .flatten()
        .chain(capture)
        .min()
}

// ── Thread loop ─────────────────────────────────────────────────────────

fn run_screen_loop(
    control_rx: Receiver<ScreenControl>,
    event_tx: Sender<ScreenEvent>,
    network_tx: Sender<NetworkControl>,
    media_tx: Sender<MediaOut>,
    shared: SharedScreenState,
) {
    let mut st = ScreenState {
        capture: None,
        peer_streams: HashMap::new(),
        decoder_failed: HashSet::new(),
        unwatched: HashSet::new(),
        host_hidden: false,
        minimized: HashSet::new(),
        running: true,
    };
    let mut next_capture = Instant::now();
    // Whether this thread holds `video::pool` for its pictures, and the
    // 1 ms timer it holds with it.
    let mut pooling = false;
    // (Held for what dropping it does; never read.)
    let mut _fine_timer: Option<audio::TimerResolutionGuard> = None;

    while st.running {
        // Sleep until the earliest thing with a time of its own (review
        // P1-7): a capture tick, a frame's playout slot, a picture the
        // decoder may still hand out, a stream's log line. Frames and
        // commands wake the loop through the channel. It used to poll
        // every 4 ms while any stream was watched — 250 wakes a second
        // for a still slide, or a phone in the background.
        let now = Instant::now();
        let received = match next_wake(&st, next_capture, now) {
            Some(at) => control_rx.recv_timeout(at.saturating_duration_since(now)),
            // Nothing to capture or drain: block until a command comes.
            None => control_rx
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(ctrl) => handle_control(ctrl, &mut st, &event_tx, &network_tx, &media_tx, &shared),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Drain ALL pending control messages. This is critical: each
        // IncomingVideoFrame now just pushes into the jitter buffer
        // (nanoseconds), so we can drain hundreds of pending packets
        // without blocking.
        while let Ok(ctrl) = control_rx.try_recv() {
            handle_control(ctrl, &mut st, &event_tx, &network_tx, &media_tx, &shared);
        }

        // Frame buffers go round only while a picture is made or
        // shown; with no share and nothing watched they are freed
        // (review P2-43).
        let busy = st.capture.is_some() || !st.peer_streams.is_empty();
        if busy != pooling {
            pooling = busy;
            if busy {
                video::pool::retain();
            } else {
                video::pool::release();
            }
            // Capture ticks and playout slots are short timed waits:
            // on Windows they need the 1 ms timer, which the voice
            // engine now holds only in a room (review P2-35).
            _fine_timer = busy.then(audio::TimerResolutionGuard::acquire);
        }

        // ── Drain jitter buffers → decode → emit ────────────────────
        // Pull packets from each peer's jitter buffer and decode them.
        // This runs AFTER the channel drain, so all newly arrived packets
        // are in the buffer before we start decoding.
        drain_and_decode(
            &mut st.peer_streams,
            &event_tx,
            &network_tx,
            &shared.present,
        );

        if let Some(state) = &mut st.capture
            && Instant::now() >= next_capture
        {
            capture_and_encode(state, &network_tx, &media_tx, &shared);

            // Advance from the previous target (not from now) so the
            // capture cadence stays exact instead of drifting. The
            // interval reflects the current pace (fps adaptation).
            next_capture += effective_frame_interval(state);

            // If we fell behind, skip to now (don't burst)
            if next_capture < Instant::now() {
                next_capture = Instant::now();
            }
        }
    }

    if pooling {
        video::pool::release();
    }
    if st.capture.is_some() {
        shared.is_sharing.store(false, Ordering::Relaxed);
    }
    info!("Screen thread stopped");
}

/// Hand one decoded picture to the frontend, reported as the frame it
/// came from (`fed`). `took`: the decode call that produced it; `None`
/// for one handed out between frames by `poll`. True when the stream's
/// viewing counters are due at the server.
fn emit_picture(
    client_id: u64,
    stream: &mut PeerStream,
    decoded: video::DecodedFrame,
    fed: Fed,
    took: Option<Duration>,
    event_tx: &Sender<ScreenEvent>,
) -> bool {
    let decoded_at = Instant::now();
    stream.frames_decoded_total += 1;
    if let Some(took) = took {
        stream.decode_us_total += u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
    }
    let report = decoded_at.duration_since(stream.reported_at) >= VIEWING_REPORT_EVERY;
    if report {
        stream.reported_at = decoded_at;
    }
    let behind = stream.jitter_buffer.behind_live(fed.sent_at, decoded_at);
    let size = (decoded.width, decoded.height);
    let line = match took {
        Some(took) => {
            stream
                .log
                .on_picture(decoded_at, stream.decoder.last_timing(), took, behind, size)
        }
        None => stream.log.on_held_picture(decoded_at, behind, size),
    };
    if let Some((level, line)) = line {
        log::log!(level, "{line}");
    }
    event_tx
        .send(ScreenEvent::FrameReady {
            meta: FrameMeta {
                client_id,
                width: decoded.width,
                height: decoded.height,
                frame_bytes: fed.frame_bytes,
                seq: fed.seq,
                enqueued_at: fed.enqueued_at,
                decoded_at,
                buffer_depth_ms: stream.jitter_buffer.depth_ms(),
                // The buffer's steady hold; the reorder window this
                // showed is gone (review P1-12).
                buffer_target_ms: stream.jitter_buffer.playout_delay_ms(),
                buffer_jitter_ms: stream.jitter_buffer.adaptive_jitter_ms(),
            },
            pixels: decoded.data,
            format: decoded.format,
        })
        .ok();
    report
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
    present: &PresentTallies,
) {
    // Cap decode work per pass. An unbounded pass on a backlog spends
    // seconds decoding stale frames inside one loop iteration; bounded,
    // the loop stays live and a real backlog resolves via the buffer's
    // overflow jump-to-freshest-keyframe — skipping stale video instead
    // of replaying it, which is what a live viewer wants.
    const DECODE_BUDGET: Duration = Duration::from_millis(24);
    let mut report_viewing = false;
    for (&client_id, stream) in peer_streams.iter_mut() {
        let pass_start = Instant::now();
        let mut fed_any = false;
        while let Some(packet) = stream.jitter_buffer.pull_next() {
            fed_any = true;
            let seq = packet.seq;
            let is_keyframe = packet.is_keyframe;
            // Stamp the moment the buffer *released* the frame, not its
            // network arrival: the buffer now holds frames on purpose
            // (playout pacing, up to ~200 ms), and the app's per-frame
            // latency diagnostic should measure the decode/present path
            // after that hold, not the hold itself.
            let fed = Fed {
                seq,
                frame_bytes: packet.data.len(),
                sent_at: packet.timestamp_ms,
                enqueued_at: Instant::now(),
            };
            stream.last_fed = Some(fed);

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
                    "Keyframe #{seq} from peer {client_id}: {} bytes \
                     (buffer depth: {:.0}ms)",
                    fed.frame_bytes,
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
            let result = stream.decoder.decode(&encoded);
            let took = t0.elapsed();
            stream.log.on_decode_call(took);
            match result {
                Ok(Some(decoded)) => {
                    report_viewing |=
                        emit_picture(client_id, stream, decoded, fed, Some(took), event_tx);
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!("Decode error for peer {client_id} seq {seq}: {e}");
                    stream.log.on_decode_error(&e);
                }
            }
            if pass_start.elapsed() >= DECODE_BUDGET {
                break;
            }
        }
        // No frame this pass: a picture the decoder still holds goes out
        // once the stream has paused (`VideoDecoder::poll`).
        if !fed_any && let Some(fed) = stream.last_fed {
            match stream.decoder.poll() {
                Ok(Some(decoded)) => {
                    report_viewing |= emit_picture(client_id, stream, decoded, fed, None, event_tx);
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!("Decode error for peer {client_id} (between frames): {e}");
                    stream.log.on_decode_error(&e);
                }
            }
        }

        // The buffer raises this when a frame never came and no keyframe
        // was buffered to go on from (it froze), or when it has been
        // watching undecodable deltas while unprimed. Rate-limited
        // internally, so relaying every signal is safe.
        if stream.jitter_buffer.take_keyframe_request() {
            // Counted (and a freeze named) in the stream's log lines.
            log::debug!("Requesting keyframe from peer {client_id}");
            let _ = network_tx.try_send(NetworkControl::RequestKeyframe {
                target_id: client_id,
            });
        }

        let audio = stream.stream_audio.as_ref().map(StreamDecoder::stats);
        let lines = stream
            .log
            .poll(Instant::now(), &stream.jitter_buffer, audio, || {
                present.lock().ok()?.remove(&client_id)
            });
        for (level, line) in lines {
            log::log!(level, "{line}");
        }
    }
    if report_viewing {
        // Every stream's counters ride to the server (spec §9.1); the
        // network thread adds the keyframe requests it sent for us.
        let samples: Vec<protocol::ViewingSample> = peer_streams
            .iter()
            .map(|(sharer, ps)| protocol::ViewingSample {
                sharer: *sharer,
                frames_decoded: ps.frames_decoded_total,
                frames_dropped: ps.frames_dropped_total,
                keyframe_requests_sent: 0,
                decode_us: ps
                    .decode_us_total
                    .checked_div(ps.frames_decoded_total)
                    .unwrap_or(0),
            })
            .collect();
        let _ = network_tx.try_send(NetworkControl::TelemetryViewing(samples));
    }
}

fn handle_control(
    ctrl: ScreenControl,
    st: &mut ScreenState,
    event_tx: &Sender<ScreenEvent>,
    network_tx: &Sender<NetworkControl>,
    media_tx: &Sender<MediaOut>,
    shared: &SharedScreenState,
) {
    match ctrl {
        ScreenControl::StartSharing {
            quality,
            monitor_index,
            scope,
        } => {
            // Restart-while-sharing replaces the pipeline. The vendored
            // DXGI backend shuts down cleanly on drop (thread joined,
            // duplication released), so no park/reuse dance is needed.
            drop(st.capture.take());
            // Create capturer — picks the best backend for the platform
            // (streaming on Windows, screenshots elsewhere / as
            // fallback) and logs the choice.
            let capturer = match Capturer::new(monitor_index, quality) {
                Ok(c) => c,
                Err(e) => {
                    event_tx
                        .send(ScreenEvent::Error(format!("Capture init: {e}")))
                        .ok();
                    return;
                }
            };

            let (encoder, encoder_name): (Box<dyn VideoEncoder + Send>, &'static str) =
                match create_encoder(quality) {
                    Ok(e) => e,
                    Err(e) => {
                        event_tx
                            .send(ScreenEvent::Error(format!("Encoder init: {e}")))
                            .ok();
                        return;
                    }
                };

            info!(
                "Screen share started: {quality}, monitor {monitor_index}, capture backend: {}",
                capturer.backend_name()
            );

            // Start system audio capture (best-effort — non-fatal if unsupported)
            let system_audio = match SystemAudioCapture::new(
                Arc::new(AudioStats::new()),
                system_audio_sink(media_tx),
            ) {
                Ok(mut cap) => match cap.start() {
                    Ok(()) => {
                        info!("System audio capture started for screen share");
                        Some(cap)
                    }
                    Err(e) => {
                        warn!("System audio capture failed to start: {e}");
                        None
                    }
                },
                Err(e) => {
                    warn!("System audio capture not available: {e}");
                    None
                }
            };

            st.capture = Some(CaptureState {
                capturer,
                encoder,
                quality,
                seq: 0,
                next_refresh: Instant::now(),
                keyframe_burst_until: None,
                flush_owed: false,
                telemetry: (Instant::now(), 0, 0, 0, 0, 0),
                keyframes_logged: 0,
                system_audio,
                system_audio_retry: None,
                ladder: BitrateLadder::new(quality),
                log: Sharing::new(Instant::now()),
                offered_bytes: 0,
                offered_since: Instant::now(),
                uplink_loss_pct: 0.0,
                encoder_name,
                forced_keyframes: 0,
                keyframe_requests: 0,
                viewer_loss_pct: 0.0,
                health_logged: None,
                pace_idx: 0,
                pace_changed: Instant::now(),
                kf_request_times: Vec::new(),
                last_distress: None,
            });

            let _ = network_tx.try_send(NetworkControl::TelemetryEvent(
                protocol::TelemetryEvent::ShareStarted {
                    preset: quality.to_string(),
                    encoder: encoder_name.to_string(),
                },
            ));
            shared.is_sharing.store(true, Ordering::Relaxed);
            shared.fps_scale_pct.store(100, Ordering::Relaxed);
            shared.stream_loss_pct.store(0, Ordering::Relaxed);
            shared.stream_viewers.store(0, Ordering::Relaxed);
            *shared.current_quality.lock().unwrap() = Some(quality);

            // Tell network to signal other clients. These are the
            // quality *bounds* — the actual encoded dimensions travel
            // with every video frame (aspect-preserving fit).
            let _ = network_tx.send(NetworkControl::ScreenShareStart {
                width: quality.width(),
                height: quality.height(),
                scope,
            });

            event_tx.send(ScreenEvent::SharingStarted { quality }).ok();
        }

        ScreenControl::StopSharing => {
            if st.capture.take().is_some() {
                shared.is_sharing.store(false, Ordering::Relaxed);
                shared.fps_scale_pct.store(100, Ordering::Relaxed);
                shared.stream_loss_pct.store(0, Ordering::Relaxed);
                shared.stream_viewers.store(0, Ordering::Relaxed);
                let _ = network_tx.send(NetworkControl::ScreenShareStop);
                let _ = network_tx.try_send(NetworkControl::TelemetrySharing(None));
                let _ = network_tx.try_send(NetworkControl::TelemetryEvent(
                    protocol::TelemetryEvent::ShareStopped,
                ));
                *shared.current_quality.lock().unwrap() = None;
                event_tx.send(ScreenEvent::SharingStopped).ok();
                info!("Screen share stopped");
            }
        }

        // ── Jitter buffer push ──────────────────────────────────────
        // Decoding happens in `drain_and_decode`; this only appends to
        // the buffer, so a burst of frames never blocks the channel.
        ScreenControl::IncomingVideoFrame {
            client_id,
            seq,
            is_keyframe,
            width,
            height,
            timestamp_ms,
            data,
        } => {
            // Decoder init already failed for this peer — drop their
            // frames until they start a new share session. Or we closed
            // its viewer and these are the last ones on their way.
            if st.decoder_failed.contains(&client_id) || st.unwatched.contains(&client_id) {
                return;
            }

            if let std::collections::hash_map::Entry::Vacant(slot) =
                st.peer_streams.entry(client_id)
            {
                match create_decoder() {
                    Ok(decoder) => {
                        info!("Created decoder for peer {client_id} ({width}x{height})");
                        // Assume 30fps initially; will be refined when
                        // Quality is known. The buffer adapts well even
                        // if the FPS guess is off.
                        slot.insert(PeerStream {
                            decoder,
                            width,
                            height,
                            jitter_buffer: VideoJitterBuffer::new(30),
                            stream_audio: None,
                            frames_decoded_total: 0,
                            frames_dropped_total: 0,
                            decode_us_total: 0,
                            reported_at: Instant::now(),
                            log: Viewing::new(client_id, Instant::now()),
                            last_fed: None,
                        });
                    }
                    Err(e) => {
                        // Do NOT panic here — this used to be an .expect
                        // that killed the entire screen thread (and with
                        // it, our own share) on a single failed init.
                        warn!("Decoder init failed for peer {client_id}: {e}");
                        st.decoder_failed.insert(client_id);
                        event_tx
                            .send(ScreenEvent::Error(format!(
                                "Video decoder init failed: {e}"
                            )))
                            .ok();
                        return;
                    }
                }
            }

            let seen = st.sees(client_id);
            if let Some(stream) = st.peer_streams.get_mut(&client_id) {
                // Nobody can see the picture: a phone in the background
                // or a minimized viewer. Decoding on (IOS-SPEC D42,
                // ANDROID-SPEC D43) kept the first picture back current,
                // at the price of every frame decoded — and on Android
                // ~3 MB of planes copied out per 1080p frame — for as
                // long as the app was away. Dropping them costs one
                // keyframe on return, asked for at once (`SetVisible`),
                // and the sharer sends one every 2 s anyway. The stream
                // stays, so its audio (packets of its own) plays on.
                if !seen {
                    stream.log.on_unseen();
                    return;
                }
                stream.jitter_buffer.push(BufferedPacket {
                    seq,
                    is_keyframe,
                    width,
                    height,
                    data,
                    arrival_time: Instant::now(),
                    timestamp_ms,
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
        }

        ScreenControl::Watch { client_id } => {
            st.unwatched.remove(&client_id);
            st.minimized.remove(&client_id);
        }

        ScreenControl::StopWatching { client_id } => {
            // As below: the decoder, the buffer and the stream's audio
            // go; a new viewer starts from the next keyframe.
            st.unwatched.insert(client_id);
            st.minimized.remove(&client_id);
            if st.peer_streams.remove(&client_id).is_some() {
                log::debug!("stopped decoding peer {client_id}: viewer closed");
            }
        }

        ScreenControl::PeerStoppedSharing { client_id } => {
            // Dropping the PeerStream also drops StreamDecoder,
            // which stops the decode thread + playback pipeline.
            st.peer_streams.remove(&client_id);
            st.decoder_failed.remove(&client_id);
            st.unwatched.remove(&client_id);
            st.minimized.remove(&client_id);
            event_tx
                .send(ScreenEvent::PeerStoppedSharing { client_id })
                .ok();
        }

        ScreenControl::SetVisible { client_id, visible } => st.set_visible(client_id, visible),

        ScreenControl::IncomingSystemAudio {
            client_id,
            seq,
            data,
        } => {
            if !st.unwatched.contains(&client_id) {
                let output = &shared.stream_output;
                decode_incoming_system_audio(client_id, seq, data, &mut st.peer_streams, output);
            }
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
                restart_system_audio(state, media_tx, "device change");
            }
        }

        ScreenControl::ForceKeyframe => {
            if let Some(state) = &mut st.capture {
                // Counted into the sharer's summary line.
                state.log.on_keyframe_asked();
                state.keyframe_requests += 1;
                state.forced_keyframes += 1;
                state.encoder.force_keyframe();

                let now = Instant::now();
                if !state.encoder.supports_forced_keyframes() {
                    // This encoder only emits IDRs on its own frame-counted
                    // period — keep frames flowing so it actually elapses.
                    state.keyframe_burst_until = Some(now + KEYFRAME_BURST_MAX);
                }
                note_keyframe_request(state, now, shared);
            }
        }

        ScreenControl::StreamHealth {
            worst_loss,
            worst_rtt_ms,
            viewers,
        } => {
            let worst_loss_pct = worst_loss * 100.0;
            shared
                .stream_loss_pct
                .store(worst_loss_pct.to_bits(), Ordering::Relaxed);
            shared
                .stream_viewers
                .store(u32::from(viewers), Ordering::Relaxed);
            if let Some(state) = &mut st.capture {
                state.viewer_loss_pct = worst_loss_pct;
                // The server→viewer leg is otherwise invisible in the
                // sharer's log; one line per HEALTH_LOG_INTERVAL while
                // any viewer is losing packets.
                let now = Instant::now();
                let due = state
                    .health_logged
                    .is_none_or(|t| now.duration_since(t) >= HEALTH_LOG_INTERVAL);
                if worst_loss_pct >= PACE_LOSS_GATE_PCT && due {
                    state.health_logged = Some(now);
                    info!(
                        "viewers: worst loss {worst_loss_pct:.1}%, worst rtt {worst_rtt_ms:.0} ms, \
                         {viewers} watching"
                    );
                }
            }
        }

        ScreenControl::UplinkStats {
            loss_pct,
            rtt_ms,
            kbps_up,
            groups_started,
            groups_reset,
            path_loss_pct,
        } => {
            if let Some(state) = &mut st.capture {
                let now = Instant::now();
                state
                    .log
                    .on_uplink(groups_started, groups_reset, path_loss_pct, rtt_ms, kbps_up);
                let secs = now.duration_since(state.offered_since).as_secs_f32();
                #[allow(clippy::cast_precision_loss)] // kbit/s, exactness irrelevant
                let kbps_offered = if secs > 0.0 {
                    state.offered_bytes as f32 * 8.0 / 1000.0 / secs
                } else {
                    0.0
                };
                state.offered_bytes = 0;
                state.offered_since = now;
                let link = LinkSample {
                    loss_pct,
                    rtt_ms,
                    kbps_up,
                    kbps_offered,
                };
                adapt_bitrate(state, now, link);
                recover_pace(state, now, shared);
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
    media_tx: &Sender<MediaOut>,
    shared: &SharedScreenState,
) {
    // Capture. For the streaming backend this is a non-blocking channel
    // drain (the OS captures on its own thread); for the screenshot
    // backend it performs the actual capture here.
    let t0 = Instant::now();
    let status = match state.capturer.poll() {
        Ok(s) => s,
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

    let now = Instant::now();
    if status == CaptureStatus::Fresh {
        state.telemetry.1 += 1;
    }
    match status {
        CaptureStatus::Fresh => state.log.on_capture(true),
        CaptureStatus::Unchanged => state.log.on_capture(false),
        CaptureStatus::Empty => {}
    }
    report_share_telemetry(state, network_tx, shared);
    revive_system_audio(state, media_tx);
    let repeat = match status {
        CaptureStatus::Fresh => false,
        CaptureStatus::Unchanged => {
            // Nothing changed on screen. Encode anyway only if a viewer
            // is waiting on a forced IDR, the periodic refresh is due, or
            // the last change is stuck inside an encoder that hands out
            // output only when given a frame (AMF: one repeat of the held
            // frame brings it out, and the next tick's drain sends it).
            // Otherwise skip: no swscale, no encoder work, no seq
            // consumed — only what the encoder has finished goes out.
            let bursting = state.keyframe_burst_until.is_some_and(|until| now < until);
            let flush = std::mem::take(&mut state.flush_owed);
            if !state.encoder.keyframe_pending() && !bursting && !flush && now < state.next_refresh
            {
                state.telemetry.2 += 1;
                // A hardware encoder may still hold the last frames of
                // the change: they leave now, not with the next change or
                // the 1 s refresh (review P1-13).
                if state.encoder.frames_in_flight() > 0 {
                    match state.encoder.drain() {
                        Ok(aus) => send_encoded(state, aus, media_tx, shared),
                        // A broken encoder says so on the next encode.
                        Err(e) => log::debug!("Encoder drain failed: {e}"),
                    }
                }
                return;
            }
            true
        }
        // Nothing captured yet (streaming backend rebuilding with no
        // held frame) — nothing to encode this tick.
        CaptureStatus::Empty => {
            return;
        }
    };

    // A repeated frame gets its timestamp advanced to "now" so PTS and
    // rate control keep tracking real time.
    let Some(frame) = state.capturer.frame(repeat) else {
        return;
    };

    // Encode. Returns zero or more access units: zero while a hardware
    // encoder's pipeline fills (first few frames on NVENC/QSV/AMF), and
    // occasionally more than one when it flushes pending output. Each AU
    // gets its own sequence number — the receiver's jitter buffer treats
    // seq gaps as loss, so skipped/empty encodes must not consume seqs.
    let t1 = Instant::now();
    let encoded = if repeat {
        state.encoder.encode_repeat(frame)
    } else {
        state.encoder.encode(frame)
    };
    let encoded_frames = match encoded {
        Ok(e) => e,
        Err(e) => {
            warn!("Encode failed: {e}");
            return;
        }
    };
    let encode_us = t1.elapsed().as_micros() as u32;
    if !encoded_frames.is_empty() {
        state.log.on_encode(t1.elapsed());
    }
    // Output-spacing accounting (see the telemetry field docs): one AU
    // per tick is healthy; 2+ means bursts that viewers will coalesce.
    match encoded_frames.len() {
        0 => {}
        1 => state.telemetry.4 += 1,
        _ => state.telemetry.5 += 1,
    }
    state.next_refresh = now + STATIC_REFRESH_INTERVAL;
    // A change went in: if the encoder keeps its output until the next
    // submission, the first still tick repeats the frame once.
    state.flush_owed = !repeat && state.encoder.output_needs_input();
    shared.last_encode_us.store(encode_us, Ordering::Relaxed);
    send_encoded(state, encoded_frames, media_tx, shared);
}

/// Hand the encoder's access units to the network, one sequence number
/// each — except frames too large to carry, which are dropped before
/// they take one.
fn send_encoded(
    state: &mut CaptureState,
    encoded_frames: Vec<EncodedFrame>,
    media_tx: &Sender<MediaOut>,
    shared: &SharedScreenState,
) {
    for encoded in encoded_frames {
        if encoded.data.is_empty() {
            continue;
        }

        state.telemetry.3 += 1;

        shared
            .last_frame_bytes
            .store(encoded.data.len() as u32, Ordering::Relaxed);

        // A frame the chunker can't carry is dropped before it takes a
        // sequence number: a number that never arrives is a gap, and
        // every viewer would freeze until a keyframe (review P1-12; the
        // number used to be taken first). The GOP restarts on the next
        // frame instead — a one-frame hiccup. The retried keyframe is
        // not smaller by itself: on AMF it is a fresh session's first
        // frame, which ignores rate control (2026-10-08: 287–522 KB in
        // a row, 425 KB from a session at 2.1 Mbit/s). So the encoder
        // raises its quantizer floor first, and the replacement opens
        // with it.
        if encoded.data.len() > protocol::MAX_VIDEO_FRAME_BYTES {
            state.log.on_too_large(encoded.data.len());
            match state.encoder.frame_too_large() {
                Some(qp) => warn!(
                    "encoded frame too large to send ({} KB, max {} KB): \
                     keyframes now encode at QP ≥ {qp}",
                    encoded.data.len() / 1000,
                    protocol::MAX_VIDEO_FRAME_BYTES / 1000,
                ),
                None => log::debug!(
                    "encoded frame too large to send ({} bytes) — dropped",
                    encoded.data.len()
                ),
            }
            state.forced_keyframes += 1;
            state.encoder.force_keyframe();
            if !state.encoder.supports_forced_keyframes() {
                state.keyframe_burst_until = Some(Instant::now() + KEYFRAME_BURST_MAX);
            }
            continue;
        }

        let seq = state.seq;
        state.seq = state.seq.wrapping_add(1);
        if encoded.is_keyframe {
            log::debug!("Sent keyframe #{seq}: {} bytes", encoded.data.len());
            // A real IDR is going out — any burst has done its job.
            state.keyframe_burst_until = None;
            // Bitstream self-check: prove (or disprove) that keyframe
            // AUs carry what viewers need to prime — parameter sets and
            // an actual IDR slice.
            if state.keyframes_logged < 3 {
                state.keyframes_logged += 1;
                info!(
                    "keyframe AU check #{}: seq={seq}, {} bytes, NALs: {}",
                    state.keyframes_logged,
                    encoded.data.len(),
                    video::describe_annexb_nals(&encoded.data)
                );
            }
        }
        state.offered_bytes += encoded.data.len() as u64;
        state.log.on_sent(encoded.data.len(), encoded.is_keyframe);
        let _ = media_tx.try_send(MediaOut::VideoFrame {
            seq,
            is_keyframe: encoded.is_keyframe,
            width: encoded.width as u16,
            height: encoded.height as u16,
            // Wraps at ~49 days of share; viewers unwrap it.
            timestamp_ms: encoded.timestamp_ms as u32,
            data: encoded.data,
        });
    }
}

/// Decode an incoming system audio (Opus stereo) packet from a remote peer.
///
/// Lazy-initializes the stream decoder on the first packet. The Opus
/// decoder, PLC, and playback pipeline all live in `audio::stream::decoder`
/// — this function just creates the handle and feeds it raw bytes.
fn decode_incoming_system_audio(
    client_id: u64,
    seq: u32,
    data: Vec<u8>,
    peer_streams: &mut HashMap<u64, PeerStream>,
    output: &audio::stream::OutputSelection,
) {
    let Some(stream) = peer_streams.get_mut(&client_id) else {
        // No PeerStream yet — waiting for first video keyframe.
        // System audio packets that arrive before the first keyframe
        // are silently dropped.
        return;
    };

    // Lazy-init the stream decoder + playback on first system audio packet.
    if stream.stream_audio.is_none() {
        match stream_decoder::spawn(output.clone(), 1.0) {
            Ok(decoder) => {
                info!("Started stream audio decoder for peer {client_id}");
                stream.stream_audio = Some(decoder);
            }
            Err(e) => {
                warn!("Failed to create stream audio decoder for peer {client_id}: {e}");
                return;
            }
        }
    }

    // Feed the raw Opus packet — the audio crate handles decode + PLC + playback.
    if let Some(ref audio) = stream.stream_audio {
        audio.send(StreamAudioPacket { data, seq });
    }
}

// ── Encoder/decoder factory ─────────────────────────────────────────────

/// Create the video encoder (`FFmpeg` is currently the only backend).
fn create_encoder(
    quality: Quality,
) -> Result<(Box<dyn VideoEncoder + Send>, &'static str), CodecError> {
    match FfmpegEncoder::new(quality) {
        Ok(enc) => {
            let name = enc.encoder_name();
            info!("Using FFmpeg encoder: {name}");
            Ok((Box::new(enc), name))
        }
        Err(e) => {
            warn!("FFmpeg encoder unavailable: {e}");
            Err(CodecError::NoEncoder(e.to_string()))
        }
    }
}

/// Create the video decoder: `FFmpeg` on the desktop.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn create_decoder() -> Result<Box<dyn VideoDecoder + Send>, CodecError> {
    match FfmpegDecoder::new() {
        Ok(dec) => Ok(Box::new(dec)),
        Err(e) => {
            warn!("FFmpeg decoder unavailable: {e}");
            Err(CodecError::DecoderInit(e.to_string()))
        }
    }
}

/// Create the video decoder: the platform's `MediaCodec` on Android
/// (ANDROID-SPEC.md D23). It configures itself on the first keyframe.
#[cfg(target_os = "android")]
fn create_decoder() -> Result<Box<dyn VideoDecoder + Send>, CodecError> {
    match video::mediacodec::MediaCodecDecoder::new() {
        Ok(dec) => Ok(Box::new(dec)),
        Err(e) => {
            warn!("MediaCodec decoder unavailable: {e}");
            Err(CodecError::DecoderInit(e.to_string()))
        }
    }
}

/// Create the video decoder: `VideoToolbox` on iOS (IOS-SPEC.md D9). It
/// makes its session on the first keyframe.
#[cfg(target_os = "ios")]
fn create_decoder() -> Result<Box<dyn VideoDecoder + Send>, CodecError> {
    match video::videotoolbox::VideoToolboxDecoder::new() {
        Ok(dec) => Ok(Box::new(dec)),
        Err(e) => {
            warn!("VideoToolbox decoder unavailable: {e}");
            Err(CodecError::DecoderInit(e.to_string()))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A command reaches the thread it is for, and only that one;
    /// `Shutdown` reaches both (review P2-44).
    #[test]
    fn commands_go_to_the_thread_they_are_for() {
        let (share, share_rx) = bounded(8);
        let (watch, watch_rx) = bounded(8);
        let tx = ScreenTx { share, watch };

        tx.send(ScreenControl::ForceKeyframe).unwrap();
        tx.try_send(ScreenControl::StopSharing).unwrap();
        tx.send(ScreenControl::RestartSystemAudio).unwrap();
        assert!(watch_rx.try_recv().is_err(), "nothing of the share's");
        assert!(matches!(
            share_rx.try_recv(),
            Ok(ScreenControl::ForceKeyframe)
        ));
        assert!(matches!(
            share_rx.try_recv(),
            Ok(ScreenControl::StopSharing)
        ));
        assert!(matches!(
            share_rx.try_recv(),
            Ok(ScreenControl::RestartSystemAudio)
        ));

        tx.send(ScreenControl::Watch { client_id: 7 }).unwrap();
        tx.try_send(ScreenControl::PeerStoppedSharing { client_id: 7 })
            .unwrap();
        tx.send(ScreenControl::SetVisible {
            client_id: None,
            visible: false,
        })
        .unwrap();
        assert!(share_rx.try_recv().is_err(), "nothing of the viewers'");
        assert!(matches!(
            watch_rx.try_recv(),
            Ok(ScreenControl::Watch { client_id: 7 })
        ));
        assert!(matches!(
            watch_rx.try_recv(),
            Ok(ScreenControl::PeerStoppedSharing { client_id: 7 })
        ));
        assert!(matches!(
            watch_rx.try_recv(),
            Ok(ScreenControl::SetVisible { .. })
        ));

        tx.send(ScreenControl::Shutdown).unwrap();
        assert!(matches!(share_rx.try_recv(), Ok(ScreenControl::Shutdown)));
        assert!(matches!(watch_rx.try_recv(), Ok(ScreenControl::Shutdown)));

        // A viewer that is behind does not hold up the share's commands.
        for _ in 0..8 {
            tx.try_send(ScreenControl::Watch { client_id: 1 }).unwrap();
        }
        assert!(matches!(
            tx.try_send(ScreenControl::Watch { client_id: 1 }),
            Err(TrySendError::Full(_))
        ));
        tx.try_send(ScreenControl::ForceKeyframe).unwrap();
    }

    /// Both threads start idle, take their commands and stop on
    /// `Shutdown`; the one handle joins both.
    #[test]
    fn both_threads_stop_on_shutdown() {
        let (network_tx, _network_rx) = bounded(8);
        let (media_tx, _media_rx) = bounded(8);
        let mut handle = ScreenThreadHandle::spawn(network_tx, media_tx);
        handle.tx.send(ScreenControl::StopSharing).unwrap();
        handle
            .tx
            .send(ScreenControl::StopWatching { client_id: 3 })
            .unwrap();
        let started = Instant::now();
        handle.shutdown();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(handle.join_handle.is_none());
        // Stopped: nobody takes commands any more.
        assert!(handle.tx.send(ScreenControl::ForceKeyframe).is_err());
        assert!(handle
            .tx
            .send(ScreenControl::Watch { client_id: 3 })
            .is_err());
    }

    mod bitrate_ladder {
        use super::*;

        /// Link samples at `loss` percent while the link is carrying the
        /// current rung's full rate (so the exercised gates pass).
        fn carrying(ladder: &mut BitrateLadder, now: Instant, loss: f32) -> Option<u32> {
            #[allow(clippy::cast_precision_loss)]
            let kbps = ladder.quality.scaled_bitrate(ladder.pct()) as f32 / 1000.0;
            ladder
                .sample(now, link(loss, 220.0, kbps, kbps))
                .map(|c| c.pct)
        }

        fn link(loss_pct: f32, rtt_ms: f32, kbps_up: f32, kbps_offered: f32) -> LinkSample {
            LinkSample {
                loss_pct,
                rtt_ms,
                kbps_up,
                kbps_offered,
            }
        }

        fn at(t0: Instant, secs: u64) -> Instant {
            t0 + Duration::from_secs(secs)
        }

        #[test]
        fn starts_at_full_rate() {
            assert_eq!(BitrateLadder::new(Quality::FHD60).pct(), 100);
        }

        #[test]
        fn holds_on_a_single_lossy_sample() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            assert_eq!(carrying(&mut l, at(t0, 0), 30.0), None);
        }

        #[test]
        fn drops_two_rungs_on_severe_loss() {
            // 2026-08-28 18:31:20–22: 29.7% then 53.4% at 8.3 Mbit.
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 29.7);
            assert_eq!(carrying(&mut l, at(t0, 2), 53.4), Some(50));
        }

        #[test]
        fn drops_one_rung_on_moderate_loss() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 15.0);
            assert_eq!(carrying(&mut l, at(t0, 2), 15.0), Some(70));
        }

        #[test]
        fn ignores_loss_while_the_link_is_idle() {
            // A static screen: little offered, and what went out was
            // about that much.
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            let steps: Vec<_> = (0..6)
                .filter_map(|i| l.sample(at(t0, 2 * i), link(60.0, 220.0, 500.0, 400.0)))
                .collect();
            assert!(steps.is_empty());
        }

        #[test]
        fn a_static_screens_refresh_trickle_is_not_overload() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            let steps: Vec<_> = (0..6)
                .filter_map(|i| l.sample(at(t0, 2 * i), link(100.0, 45.0, 40.0, 200.0)))
                .collect();
            assert!(steps.is_empty());
        }

        #[test]
        fn steps_down_when_the_uplink_collapsed_under_the_rung() {
            // 2026-10-08: 1080p60 at 8 Mbit/s, every group abandoned,
            // QUIC's window at 2 KB so ~200 kbit/s went out — the old
            // gate read that as an idle screen and never stepped.
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            assert!(l
                .sample(at(t0, 0), link(100.0, 45.0, 209.0, 1500.0))
                .is_none());
            let first = l.sample(at(t0, 2), link(100.0, 45.0, 223.0, 1500.0));
            assert_eq!(first.map(|c| c.pct), Some(50));
            for s in [4, 6, 8, 10] {
                l.sample(at(t0, s), link(100.0, 45.0, 200.0, 1500.0));
            }
            let second = l.sample(at(t0, 12), link(100.0, 45.0, 200.0, 1500.0));
            assert_eq!(second.map(|c| c.pct), Some(25));
        }

        #[test]
        fn offered_load_alone_counts_for_a_step_down() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            l.sample(at(t0, 0), link(15.0, 60.0, 1800.0, 3000.0));
            let step = l.sample(at(t0, 2), link(15.0, 60.0, 1800.0, 3000.0));
            assert_eq!(step.map(|c| c.pct), Some(70));
        }

        #[test]
        fn waits_out_the_dwell_before_stepping_again() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0); // → 50% at t=2
            let inside_dwell: Vec<_> = [4, 6, 8, 10]
                .into_iter()
                .filter_map(|s| carrying(&mut l, at(t0, s), 60.0))
                .collect();
            assert!(inside_dwell.is_empty());
        }

        #[test]
        fn steps_again_once_the_dwell_has_passed() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0); // → 50% at t=2
            for s in [4, 6, 8, 10] {
                carrying(&mut l, at(t0, s), 60.0);
            }
            assert_eq!(carrying(&mut l, at(t0, 12), 60.0), Some(25));
        }

        #[test]
        fn climbs_one_rung_after_the_clean_hold() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0); // → 50%
            let mut result = None;
            for i in 1..=u64::from(RATE_STEP_UP_SAMPLES) {
                result = carrying(&mut l, at(t0, 12 + 2 * i), 0.5);
            }
            assert_eq!(result, Some(70));
        }

        #[test]
        fn does_not_climb_one_sample_early() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0); // → 50%
            let mut result = None;
            for i in 1..u64::from(RATE_STEP_UP_SAMPLES) {
                result = carrying(&mut l, at(t0, 12 + 2 * i), 0.5);
            }
            assert_eq!(result, None);
        }

        #[test]
        fn does_not_climb_while_the_link_is_idle() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0); // → 50%
            let steps: Vec<_> = (1..=40)
                .filter_map(|i| l.sample(at(t0, 12 + 2 * i), link(0.0, 60.0, 500.0, 500.0)))
                .collect();
            assert!(steps.is_empty());
        }

        /// Drive a fresh FHD60 ladder to 50%, then through a full clean
        /// hold to a climb to 70%. Returns the ladder and the time (s)
        /// of the climb.
        fn after_a_climb() -> (BitrateLadder, Instant, u64) {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0); // → 50%
            for i in 1..=u64::from(RATE_STEP_UP_SAMPLES) {
                carrying(&mut l, at(t0, 12 + 2 * i), 0.5); // → 70% on the last
            }
            let t_up = 12 + 2 * u64::from(RATE_STEP_UP_SAMPLES);
            assert_eq!(l.pct(), 70, "setup: climb did not happen");
            (l, t0, t_up)
        }

        #[test]
        fn failed_probe_is_undone_without_waiting_for_the_dwell() {
            let (mut l, t0, t_up) = after_a_climb();
            carrying(&mut l, at(t0, t_up + 2), 60.0);
            assert_eq!(carrying(&mut l, at(t0, t_up + 4), 60.0), Some(50));
        }

        #[test]
        fn failed_probe_returns_to_the_previous_rung_even_under_severe_loss() {
            let (mut l, t0, t_up) = after_a_climb();
            carrying(&mut l, at(t0, t_up + 2), 65.0);
            carrying(&mut l, at(t0, t_up + 4), 65.0);
            assert_eq!(l.pct(), 50);
        }

        #[test]
        fn failed_probe_doubles_the_next_hold() {
            let (mut l, t0, t_up) = after_a_climb();
            carrying(&mut l, at(t0, t_up + 2), 60.0);
            carrying(&mut l, at(t0, t_up + 4), 60.0);
            assert_eq!(l.probe_backoff, 2);
        }

        #[test]
        fn backoff_is_capped() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            l.probe_backoff = RATE_PROBE_BACKOFF_MAX;
            l.idx = 1;
            l.changed = Some(at(t0, 100));
            l.last_step_up = Some(at(t0, 100));
            carrying(&mut l, at(t0, 102), 60.0);
            carrying(&mut l, at(t0, 104), 60.0);
            assert_eq!(l.probe_backoff, RATE_PROBE_BACKOFF_MAX);
        }

        #[test]
        fn fresh_congestion_resets_the_backoff() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            l.probe_backoff = 4;
            carrying(&mut l, at(t0, 0), 60.0);
            carrying(&mut l, at(t0, 2), 60.0);
            assert_eq!(l.probe_backoff, 1);
        }

        #[test]
        fn middle_band_resets_the_lossy_count() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            carrying(&mut l, at(t0, 0), 15.0);
            carrying(&mut l, at(t0, 2), 5.0);
            assert_eq!(carrying(&mut l, at(t0, 4), 15.0), None);
        }

        #[test]
        fn floor_shortens_the_hd30_ladder() {
            assert_eq!(lowest_rung(Quality::HD30), 3);
        }

        #[test]
        fn floor_lifts_hd30_bottom_rung_to_35_percent() {
            assert_eq!(rung_pct(Quality::HD30, 3), 35);
        }

        #[test]
        fn fhd60_reaches_every_rung() {
            assert_eq!(lowest_rung(Quality::FHD60), RATE_LADDER_PCT.len() - 1);
        }

        #[test]
        fn never_steps_below_the_lowest_rung() {
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::HD30);
            let mut last = None;
            for i in 0..40 {
                if let Some(p) = carrying(&mut l, at(t0, 2 * i), 60.0) {
                    last = Some(p);
                }
            }
            assert_eq!(last, Some(35));
        }
    }

    /// Who can see a stream, and when the loop wakes (review P1-7).
    mod visibility {
        use super::*;

        const PEER: u64 = 7;

        struct Harness {
            st: ScreenState,
            event_tx: Sender<ScreenEvent>,
            _event_rx: Receiver<ScreenEvent>,
            network_tx: Sender<NetworkControl>,
            network_rx: Receiver<NetworkControl>,
            media_tx: Sender<MediaOut>,
            shared: SharedScreenState,
        }

        impl Harness {
            fn new() -> Self {
                let (event_tx, event_rx) = bounded(64);
                let (network_tx, network_rx) = bounded(64);
                let (media_tx, _media_rx) = bounded(64);
                Self {
                    st: ScreenState {
                        capture: None,
                        peer_streams: HashMap::new(),
                        decoder_failed: HashSet::new(),
                        unwatched: HashSet::new(),
                        host_hidden: false,
                        minimized: HashSet::new(),
                        running: true,
                    },
                    event_tx,
                    _event_rx: event_rx,
                    network_tx,
                    network_rx,
                    media_tx,
                    shared: SharedScreenState::new(),
                }
            }

            fn control(&mut self, ctrl: ScreenControl) {
                handle_control(
                    ctrl,
                    &mut self.st,
                    &self.event_tx,
                    &self.network_tx,
                    &self.media_tx,
                    &self.shared,
                );
            }

            fn frame(&mut self, seq: u32, is_keyframe: bool) {
                self.control(ScreenControl::IncomingVideoFrame {
                    client_id: PEER,
                    seq,
                    is_keyframe,
                    width: 64,
                    height: 64,
                    timestamp_ms: seq * 16,
                    data: vec![0; 16],
                });
            }

            fn depth(&self) -> usize {
                self.st.peer_streams[&PEER].jitter_buffer.depth()
            }

            /// Keyframe requests the network thread was handed by one
            /// drain pass.
            fn requests_after_a_drain(&mut self) -> usize {
                drain_and_decode(
                    &mut self.st.peer_streams,
                    &self.event_tx,
                    &self.network_tx,
                    &self.shared.present,
                );
                self.network_rx
                    .try_iter()
                    .filter(|c| {
                        matches!(c, NetworkControl::RequestKeyframe { target_id } if *target_id == PEER)
                    })
                    .count()
            }
        }

        #[test]
        fn a_hidden_app_buffers_nothing_and_asks_for_a_keyframe_once_back() {
            let mut h = Harness::new();
            h.frame(0, true);
            assert_eq!(h.depth(), 1);
            h.control(ScreenControl::SetVisible {
                client_id: None,
                visible: false,
            });
            h.frame(1, false);
            h.frame(2, false);
            assert_eq!(h.depth(), 1, "unseen frames never reach the buffer");
            assert_eq!(h.requests_after_a_drain(), 0);

            h.control(ScreenControl::SetVisible {
                client_id: None,
                visible: true,
            });
            let buffer = &h.st.peer_streams[&PEER].jitter_buffer;
            assert!(!buffer.is_primed(), "starts over from a keyframe");
            assert_eq!(h.requests_after_a_drain(), 1);
            assert_eq!(h.requests_after_a_drain(), 0, "asked once");
            h.frame(3, true);
            assert_eq!(h.depth(), 1);
        }

        #[test]
        fn a_minimized_viewer_is_unseen_until_restored_or_closed() {
            let mut h = Harness::new();
            h.frame(0, true);
            h.control(ScreenControl::SetVisible {
                client_id: Some(PEER),
                visible: false,
            });
            assert!(!h.st.sees(PEER));
            assert!(h.st.sees(PEER + 1), "only that viewer");
            h.control(ScreenControl::SetVisible {
                client_id: Some(PEER),
                visible: true,
            });
            assert!(h.st.sees(PEER));

            h.control(ScreenControl::SetVisible {
                client_id: Some(PEER),
                visible: false,
            });
            h.control(ScreenControl::StopWatching { client_id: PEER });
            assert!(h.st.sees(PEER), "a new viewer starts seen");
        }

        #[test]
        fn a_stream_that_stays_seen_is_not_restarted() {
            let mut h = Harness::new();
            h.frame(0, true);
            h.control(ScreenControl::SetVisible {
                client_id: None,
                visible: true,
            });
            assert!(h.st.peer_streams[&PEER].jitter_buffer.is_primed());
        }

        #[test]
        fn nothing_to_do_blocks() {
            let h = Harness::new();
            assert_eq!(next_wake(&h.st, Instant::now(), Instant::now()), None);
        }

        #[test]
        fn a_watched_stream_wakes_for_its_work_else_for_its_log() {
            let mut h = Harness::new();
            h.control(ScreenControl::SetVisible {
                client_id: None,
                visible: false,
            });
            h.frame(0, true); // the stream exists; the frame was dropped
            let now = Instant::now();
            let log_due = h.st.peer_streams[&PEER].log.next_due();
            assert!(log_due > now);
            assert_eq!(next_wake(&h.st, now, now), Some(log_due));

            // Back on screen: the keyframe request is due at once.
            h.control(ScreenControl::SetVisible {
                client_id: None,
                visible: true,
            });
            assert_eq!(next_wake(&h.st, now, now), Some(now));
            assert_eq!(h.requests_after_a_drain(), 1);

            // A frame: due at its playout slot, before the log line.
            h.frame(1, true);
            let frame_due = h.st.peer_streams[&PEER]
                .jitter_buffer
                .next_due(now)
                .expect("buffered");
            assert!(frame_due < log_due);
            assert_eq!(next_wake(&h.st, now, now), Some(frame_due));
        }
    }
}
