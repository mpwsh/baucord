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
// `audio::stream::decoder` — screen.rs never touches Opus directly.
//
// Sender-side adaptation: uplink link samples (from network.rs) drive a
// bitrate ladder on the encoder; viewer keyframe-request distress drives
// a small fps pacer. See the "Sender-side adaptation" section below.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{
    stats::AudioStats,
    stream::{
        decoder::{self as stream_decoder, StreamAudioPacket, StreamDecoder},
        input::SystemAudioCapture,
    },
};
use crossbeam_channel::{bounded, Receiver, Sender};
use log::{info, warn};
use video::{
    buffer::{BufferedPacket, VideoJitterBuffer},
    capture::{CaptureStatus, Capturer},
    codec::{FfmpegDecoder, FfmpegEncoder},
    CodecError, EncodedFrame, Quality, VideoDecoder, VideoEncoder,
};

use crate::{network::NetworkControl, WakeFn};

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
        /// Worst viewer loss, 0.0–1.0 (fraction, not percent).
        worst_loss: f32,
        worst_rtt_ms: f32,
        viewers: u16,
    },
    /// Our own uplink as renet measures it, sampled by the network
    /// thread every ~2 s (`NETWORK_STATS_INTERVAL`). Drives the encoder
    /// bitrate ladder while sharing; ignored otherwise.
    UplinkStats {
        /// Sent packets the server never acked, in percent (0–100).
        loss_pct: f32,
        rtt_ms: f32,
        /// renet payload throughput, kbit/s. Video dominates while
        /// sharing, so this is effectively the encoder's output rate.
        kbps_up: f32,
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
        /// When the jitter buffer released the frame for decoding (after
        /// its playout hold), so `decoded_at - enqueued_at` is decode
        /// latency alone.
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
    /// (100 / 75 / 50) — stepped down under viewer keyframe-request
    /// distress, see `PACE_SCALES`.
    pub fps_scale_pct: Arc<AtomicU32>,
    /// Current encoder bitrate as a percentage of the preset (100 at
    /// full rate) — stepped down under uplink loss, see
    /// `RATE_LADDER_PCT`.
    pub bitrate_scale_pct: Arc<AtomicU32>,
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
            bitrate_scale_pct: Arc::new(AtomicU32::new(100)),
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
    pub fn spawn(network_tx: Sender<NetworkControl>, wake: WakeFn) -> Self {
        // 256 slots: at 30fps inbound + outbound control messages, 64 was
        // too tight and caused try_send drops during active streaming.
        let (tx, control_rx) = bounded::<ScreenControl>(256);
        let (event_tx, event_rx) = bounded::<ScreenEvent>(256);
        let shared = SharedScreenState::new();

        let shared_clone = shared.clone();
        let join_handle = thread::Builder::new()
            .name("screen-share".into())
            .spawn(move || {
                run_screen_loop(control_rx, event_tx, network_tx, shared_clone, wake);
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

/// Emit the rolling share telemetry line every 10 s: fresh capture
/// rate, skipped (unchanged) tick rate, and encoded output rate. On a
/// healthy 60 fps share of moving content, fresh and encode both read
/// ~60/s; fresh well below the target while encode tracks it means the
/// *source* isn't changing that fast (30 fps video content, display
/// refresh cap) rather than a pipeline problem.
fn report_share_telemetry(state: &mut CaptureState) {
    const TELEMETRY_INTERVAL: Duration = Duration::from_secs(10);
    let (start, fresh, skipped, encoded, single, multi) = state.telemetry;
    let elapsed = start.elapsed();
    if elapsed < TELEMETRY_INTERVAL {
        return;
    }
    let secs = elapsed.as_secs_f32();
    info!(
        "share telemetry: {:.1} fresh/s, {:.1} skipped/s, {:.1} encoded/s over {:.0}s (spacing: {:.1} single/s, {:.1} bursty/s)",
        fresh as f32 / secs,
        skipped as f32 / secs,
        encoded as f32 / secs,
        secs,
        single as f32 / secs,
        multi as f32 / secs
    );
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
    /// Encoder bitrate ladder (see "Sender-side adaptation").
    ladder: BitrateLadder,
    /// Most recent uplink loss sample, percent. Gates the fps pacer:
    /// keyframe requests during our own loss are not viewer distress.
    uplink_loss_pct: f32,
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
// * The **bitrate ladder** protects our uplink. network.rs samples
//   renet's uplink loss, RTT and send rate every ~2 s; sustained loss
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
/// Minimum time at a rung before stepping again. Must exceed renet's
/// loss window (6 s, excluding the newest 0.9 s): a step's effect isn't
/// in the loss figure until the window has rolled over, and stepping
/// again before that acts on stale data. Not applied to a step down
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
const RATE_EXERCISED_STEP_DOWN: f32 = 0.25;
const RATE_EXERCISED_STEP_UP: f32 = 0.5;

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

    /// Apply one uplink sample (loss in percent, RTT in ms, send rate in
    /// kbit/s) and decide whether the rung changes.
    #[allow(clippy::cast_precision_loss)] // kbit/s comparisons, exactness irrelevant
    fn sample(
        &mut self,
        now: Instant,
        loss_pct: f32,
        rtt_ms: f32,
        kbps_up: f32,
    ) -> Option<RungChange> {
        let rung_kbps = self.quality.scaled_bitrate(self.pct()) as f32 / 1000.0;
        let exercised = |fraction: f32| kbps_up >= rung_kbps * fraction;
        let dwelled = self
            .changed
            .is_none_or(|t| now.duration_since(t) >= RATE_DWELL);
        let last_change_was_climb = self.changed.is_some() && self.changed == self.last_step_up;

        if loss_pct >= RATE_LOSS_STEP_DOWN {
            self.good_samples = 0;
            if !exercised(RATE_EXERCISED_STEP_DOWN) {
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
                "uplink loss {loss_pct:.0}%, rtt {rtt_ms:.0} ms, up {kbps_up:.0} kbit/s{note}"
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
fn adapt_bitrate(
    state: &mut CaptureState,
    now: Instant,
    loss_pct: f32,
    rtt_ms: f32,
    kbps_up: f32,
    shared: &SharedScreenState,
) {
    state.uplink_loss_pct = loss_pct;
    let from = state.ladder.pct();
    if let Some(change) = state.ladder.sample(now, loss_pct, rtt_ms, kbps_up) {
        state.encoder.set_bitrate_scale_pct(change.pct);
        shared
            .bitrate_scale_pct
            .store(change.pct, Ordering::Relaxed);
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
/// keep a picture. This many requests within PACE_DISTRESS_WINDOW steps
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
/// near-zero bytes and the encoder's 2 s IDR cadence rides on top, so a
/// static share costs a tiny fraction of full-rate encoding while
/// joining viewers still get a picture within the keyframe interval.
/// A pending forced keyframe (viewer waiting on an IDR) bypasses this
/// and encodes on the next tick.
const STATIC_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Upper bound on a keyframe "burst" (see CaptureState::keyframe_burst_until):
/// when the encoder can't force IDRs (AMF), a viewer's keyframe request
/// makes us encode every tick — static screen or not — until the
/// encoder's own IDR period elapses. At the 2 s AMF IDR period a burst
/// resolves within ~2 s of full-rate frames; this cap just guarantees a
/// misbehaving encoder can't pin us at full rate forever.
const KEYFRAME_BURST_MAX: Duration = Duration::from_secs(5);

/// The capture interval at the current pace.
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
    /// Rolling 10 s viewer telemetry: (window start, decoded frames).
    /// Splits "viewer fps is low" into what actually *arrived and
    /// decoded* vs what the sharer sent (compare against the sharer's
    /// `share telemetry` line for the same wall-clock window).
    rx_telemetry: (Instant, u32),
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
                Ok(ctrl) => handle_control(ctrl, &mut st, &event_tx, &network_tx, &shared, &wake),
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
                Ok(ctrl) => handle_control(ctrl, &mut st, &event_tx, &network_tx, &shared, &wake),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        } else {
            // ── Idle mode ───────────────────────────────────────────
            // Nothing to capture and no jitter buffers to drain —
            // block until a command arrives instead of waking 10×/s.
            match control_rx.recv() {
                Ok(ctrl) => handle_control(ctrl, &mut st, &event_tx, &network_tx, &shared, &wake),
                Err(_) => break,
            }
        }

        // Drain ALL pending control messages. This is critical: each
        // IncomingVideoFrame now just pushes into the jitter buffer
        // (nanoseconds), so we can drain hundreds of pending packets
        // without blocking.
        while let Ok(ctrl) = control_rx.try_recv() {
            handle_control(ctrl, &mut st, &event_tx, &network_tx, &shared, &wake);
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
                "ENCODE seq={} {cap_ms:.0}ms (budget {:.1}ms)",
                state.seq,
                effective_frame_interval(state).as_secs_f32() * 1000.0
            );
        }
    }

    shared.is_sharing.store(false, Ordering::Relaxed);
    info!("Screen thread stopped");
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
            // Stamp the moment the buffer *released* the frame, not its
            // network arrival: the buffer now holds frames on purpose
            // (playout pacing, up to ~200 ms), and the app's per-frame
            // latency diagnostic should measure the decode/present path
            // after that hold, not the hold itself.
            let enqueued_at = Instant::now();
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
                    "Keyframe #{seq} from peer {client_id}: {frame_bytes} bytes \
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
                    stream.rx_telemetry.1 += 1;
                    let rx_elapsed = stream.rx_telemetry.0.elapsed();
                    if rx_elapsed.as_secs() >= 10 {
                        info!(
                            "viewer telemetry peer {client_id}: {:.1} fps decoded over {:.0}s \
                             (playout delay {:.0} ms, reorder window {:.0} ms)",
                            stream.rx_telemetry.1 as f32 / rx_elapsed.as_secs_f32(),
                            rx_elapsed.as_secs_f32(),
                            stream.jitter_buffer.playout_delay_ms(),
                            stream.jitter_buffer.adaptive_target_ms(),
                        );
                        stream.rx_telemetry = (decoded_at, 0);
                    }
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
            info!("Requesting keyframe from peer {client_id}");
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
                    wake();
                    return;
                }
            };

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

            info!(
                "Screen share started: {quality}, monitor {monitor_index}, capture backend: {}",
                capturer.backend_name()
            );

            // Start system audio capture (best-effort — non-fatal if unsupported)
            let system_audio = match SystemAudioCapture::new(Arc::new(AudioStats::new())) {
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
                telemetry: (Instant::now(), 0, 0, 0, 0, 0),
                keyframes_logged: 0,
                system_audio,
                ladder: BitrateLadder::new(quality),
                uplink_loss_pct: 0.0,
                viewer_loss_pct: 0.0,
                health_logged: None,
                pace_idx: 0,
                pace_changed: Instant::now(),
                kf_request_times: Vec::new(),
                last_distress: None,
            });

            shared.is_sharing.store(true, Ordering::Relaxed);
            shared.frames_encoded.store(0, Ordering::Relaxed);
            shared.fps_scale_pct.store(100, Ordering::Relaxed);
            shared.bitrate_scale_pct.store(100, Ordering::Relaxed);
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
                scope,
            });

            event_tx.send(ScreenEvent::SharingStarted { quality }).ok();
            wake();
        }

        ScreenControl::StopSharing => {
            if st.capture.take().is_some() {
                shared.is_sharing.store(false, Ordering::Relaxed);
                shared.fps_scale_pct.store(100, Ordering::Relaxed);
                shared.bitrate_scale_pct.store(100, Ordering::Relaxed);
                shared.stream_loss_pct.store(0, Ordering::Relaxed);
                shared.stream_rtt_ms.store(0, Ordering::Relaxed);
                shared.stream_viewers.store(0, Ordering::Relaxed);
                let _ = network_tx.send(NetworkControl::ScreenShareStop);
                *shared.current_quality.lock().unwrap() = None;
                event_tx.send(ScreenEvent::SharingStopped).ok();
                wake();
                info!("Screen share stopped");
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
            timestamp_ms,
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
                        info!("Created decoder for peer {client_id} ({width}x{height})");
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
                                rx_telemetry: (Instant::now(), 0),
                            },
                        );
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
            decode_incoming_system_audio(client_id, seq, data, &mut st.peer_streams);
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
                    info!("System audio capture stopped for device rebuild");
                }

                // Rebuild — best-effort, non-fatal
                match SystemAudioCapture::new(Arc::new(AudioStats::new())) {
                    Ok(mut cap) => match cap.start() {
                        Ok(()) => {
                            info!("System audio capture restarted after device change");
                            state.system_audio = Some(cap);
                        }
                        Err(e) => {
                            warn!("System audio capture restart failed: {e}");
                        }
                    },
                    Err(e) => {
                        warn!("System audio capture unavailable after device change: {e}");
                    }
                }
            }
        }

        ScreenControl::ForceKeyframe => {
            if let Some(state) = &mut st.capture {
                info!("Viewer requested keyframe — forcing IDR");
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
                .stream_rtt_ms
                .store(worst_rtt_ms.to_bits(), Ordering::Relaxed);
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
        } => {
            if let Some(state) = &mut st.capture {
                let now = Instant::now();
                adapt_bitrate(state, now, loss_pct, rtt_ms, kbps_up, shared);
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
    report_share_telemetry(state);
    let repeat = match status {
        CaptureStatus::Fresh => false,
        CaptureStatus::Unchanged => {
            // Nothing changed on screen. Encode anyway only if a viewer
            // is waiting on a forced IDR or the periodic refresh is due
            // — otherwise skip entirely: no swscale, no encoder work,
            // no bytes on the wire, and no seq consumed.
            let bursting = state.keyframe_burst_until.is_some_and(|until| now < until);
            if !state.encoder.keyframe_pending() && !bursting && now < state.next_refresh {
                state.telemetry.2 += 1;
                shared.last_capture_us.store(capture_us, Ordering::Relaxed);
                return;
            }
            true
        }
        // Nothing captured yet (streaming backend rebuilding with no
        // held frame) — nothing to encode this tick.
        CaptureStatus::Empty => {
            shared.last_capture_us.store(capture_us, Ordering::Relaxed);
            return;
        }
    };

    // A repeated frame gets its timestamp advanced to "now" so PTS,
    // keyframe cadence, and rate control keep tracking real time.
    let Some(frame) = state.capturer.frame(repeat) else {
        return;
    };

    // Encode. Returns zero or more access units: zero while a hardware
    // encoder's pipeline fills (first few frames on NVENC/QSV/AMF), and
    // occasionally more than one when it flushes pending output. Each AU
    // gets its own sequence number — the receiver's jitter buffer treats
    // seq gaps as loss, so skipped/empty encodes must not consume seqs.
    let t1 = Instant::now();
    let encoded_frames = match state.encoder.encode(frame) {
        Ok(e) => e,
        Err(e) => {
            warn!("Encode failed: {e}");
            return;
        }
    };
    let encode_us = t1.elapsed().as_micros() as u32;
    // Output-spacing accounting (see the telemetry field docs): one AU
    // per tick is healthy; 2+ means bursts that viewers will coalesce.
    match encoded_frames.len() {
        0 => {}
        1 => state.telemetry.4 += 1,
        _ => state.telemetry.5 += 1,
    }
    state.next_refresh = now + STATIC_REFRESH_INTERVAL;

    shared.last_capture_us.store(capture_us, Ordering::Relaxed);
    shared.last_encode_us.store(encode_us, Ordering::Relaxed);

    for encoded in encoded_frames {
        if encoded.data.is_empty() {
            continue;
        }

        let seq = state.seq;
        state.seq = state.seq.wrapping_add(1);
        state.telemetry.3 += 1;

        if encoded.is_keyframe {
            log::debug!("Sent keyframe #{seq}: {} bytes", encoded.data.len());
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
                "Encoded frame seq={seq} too large to send ({} bytes, max {}) — dropping and forcing a fresh keyframe",
                encoded.data.len(),
                protocol::MAX_VIDEO_FRAME_BYTES,
            );
            state.encoder.force_keyframe();
            if !state.encoder.supports_forced_keyframes() {
                state.keyframe_burst_until = Some(Instant::now() + KEYFRAME_BURST_MAX);
            }
            continue;
        }

        if encoded.is_keyframe {
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
        let _ = network_tx.try_send(NetworkControl::SendVideoFrame {
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
) {
    let Some(stream) = peer_streams.get_mut(&client_id) else {
        // No PeerStream yet — waiting for first video keyframe.
        // System audio packets that arrive before the first keyframe
        // are silently dropped.
        return;
    };

    // Lazy-init the stream decoder + playback on first system audio packet.
    if stream.stream_audio.is_none() {
        match stream_decoder::spawn(None, 1.0) {
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

/// Create the video encoder (FFmpeg is currently the only backend).
fn create_encoder(quality: Quality) -> Result<Box<dyn VideoEncoder + Send>, CodecError> {
    match FfmpegEncoder::new(quality) {
        Ok(enc) => {
            info!("Using FFmpeg encoder: {}", enc.encoder_name());
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
#[cfg(test)]
mod tests {
    use super::*;

    mod bitrate_ladder {
        use super::*;

        /// Link samples at `loss` percent while the link is carrying the
        /// current rung's full rate (so the exercised gates pass).
        fn carrying(ladder: &mut BitrateLadder, now: Instant, loss: f32) -> Option<u32> {
            #[allow(clippy::cast_precision_loss)]
            let kbps = ladder.quality.scaled_bitrate(ladder.pct()) as f32 / 1000.0;
            ladder.sample(now, loss, 220.0, kbps).map(|c| c.pct)
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
            let t0 = Instant::now();
            let mut l = BitrateLadder::new(Quality::FHD60);
            let steps: Vec<_> = (0..6)
                .filter_map(|i| l.sample(at(t0, 2 * i), 60.0, 220.0, 500.0))
                .collect();
            assert!(steps.is_empty());
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
                .filter_map(|i| l.sample(at(t0, 12 + 2 * i), 0.0, 60.0, 500.0))
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
}
