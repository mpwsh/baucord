//! Adaptive jitter buffer with per-peer playout priming and mixing.
//!
//! Buffers **encoded packets** per peer, decodes them at playout time
//! (see [`crate::transport::packet_buffer`]), mixes to mono, and adapts its
//! target depth from inter-arrival jitter and buffer-health feedback.
//!
//! ## v0.9 changes from v0.8
//!
//! - **Packets, not PCM**: the per-peer store is a [`PacketBuffer`]. The
//!   decoder thread is gone; the output worker decodes the slot it is
//!   about to play. "Late" now means *behind the playhead*, so the
//!   adaptive target is the one late-tolerance knob (the old decoder had
//!   a fixed 30 ms reorder hold that this target could not reach), in-band
//!   FEC is applied the moment a slot comes due, and reordering costs
//!   nothing.
//! - **No mid-spurt splice**: the "burst-safe drain" used to cut raw
//!   samples out of the middle of speech. Sustained excess now drops one
//!   *quiet* decoded frame per hop instead — Perkins ch. 6: adjust the
//!   playout point where concealment is cheapest.
//! - **PLC instead of a decay tail** when a peer runs dry mid-word: two
//!   frames of Opus extrapolation, then the existing prime/underrun logic.
//! - **Honest stats**: `fec` counts only slots recovered from LBRR that
//!   was actually present (`opus_packet_has_lbrr`).
//!
//! Older history (timestamp-based jitter estimation, playout priming,
//! deferred underrun classification, per-peer clock-drift compensation)
//! is unchanged and lives on in the comments below.

use std::{
    collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    config::PeerAudioSettings,
    processor::limiter::Limiter,
    transport::{
        packet::{AudioPacket, PayloadType},
        packet_buffer::{PacketBuffer, Push},
        red,
    },
    FRAME_SIZE, OPUS_SAMPLE_RATE,
};

/// Deferred underrun classification: a peer that ran dry and then receives
/// audio again within this window was genuinely starved mid-stream (the
/// sender never stopped talking — packets were just late). If nothing
/// arrives for longer than this, the dry spell was a DTX pause, which is
/// normal and must NOT inflate the buffer target. The encoder's gate holds
/// transmission for 200 ms past speech, so real pauses always produce gaps
/// well beyond this window.
///
/// (v0.7.1: the previous heuristic — "dry within 40 ms of the last push" —
/// classified *every DTX utterance end* as an underrun, ratcheting the
/// target up ~10 ms per speech pause until it parked near 45 ms even on a
/// clean LAN. Since utterance-start latency equals the priming target, that
/// tax landed on exactly the moments listeners notice.)
const UNDERRUN_CONFIRM_WINDOW: Duration = Duration::from_millis(120);

/// Samples of linear fade-in applied when a peer resumes after priming
/// (~3 ms at 48 kHz). Softens the resume edge of an underrun/pause.
const RESUME_FADE_SAMPLES: usize = 144;

/// Media-continuity bound for confirming an underrun, in seconds.
///
/// A genuine delivery stall means the sender kept transmitting: every
/// frame exists, so the frame that ends the dry spell is media-adjacent
/// to the last one received — a gap of ONE frame (20 ms). A DTX pause
/// tail looks different: Opus needs ~200 ms of silence before it starts
/// withholding, so by the time frames are actually missing, the media
/// gap at speech resume is many frames wide. Two frames + margin
/// separates the signatures.
const UNDERRUN_MEDIA_CONTIGUITY_S: f32 = 0.045;

/// Peers with no pushed audio for this long are pruned.
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Length of one clock-drift estimation window. Long enough that the
/// slope measurement (samples in vs samples out) resolves tens of ppm
/// above scheduler noise.
const DRIFT_WINDOW: Duration = Duration::from_secs(10);

/// Estimated per-peer skew below this is noise; treated as zero so
/// healthy senders take the unmodified playout path.
const DRIFT_DEADZONE: f64 = 10e-6;

/// Ceiling on the applied per-peer rate correction (±2000 ppm — a
/// ~3.5-cent time-scale change, below audibility; larger skews mean a
/// broken sender, for which the underrun/shrink machinery below stays
/// as the safety net).
const MAX_DRIFT_CORRECTION: f64 = 2_000e-6;

/// Consecutive hops of sustained excess before a peer is asked to shrink.
/// Burst arrivals after a jitter spike legitimately overfill the buffer;
/// only excess that persists past this is clock drift or a stale target.
const SHRINK_AFTER_TICKS: u32 = 10;

/// Self-monitor (loopback) PCM: 200 ms cap. It is local audio with no
/// jitter or loss, so it never goes through a packet buffer.
const LOOPBACK_CAP: usize = OPUS_SAMPLE_RATE as usize / 5;

/// How long after the last observed loss the protection floor stays up.
///
/// FEC (LBRR in packet N+1) and RED (a copy in N+1 or N+2) can only repair
/// a lost slot if playout waits for that later packet: on a clean link the
/// target sits at its 2–10 ms floor and every repair arrives after its
/// slot has played — field logs showed `FEC 4 PLC 125` for exactly this
/// reason. While loss is being observed the target is held at one frame
/// (or the RED span) plus a margin; when the link has been clean for this
/// long the floor drops and latency returns to the minimum.
const LOSS_MEMORY: Duration = Duration::from_secs(5);
/// Margin on top of the repair distance, so the repair packet's own
/// jitter doesn't make it late.
const PROTECT_MARGIN_MS: f32 = 10.0;

/// Comfort noise continues for a paused peer only while its DTX refreshes
/// keep arriving (every 400 ms); a peer silent for longer than this has
/// left, muted, or lost its link, and gets silence.
const CNG_MAX_GAP: Duration = Duration::from_secs(1);

/// Concealment / loss counters accumulated between [`AdaptiveJitterBuffer::drain_stats`] calls.
#[derive(Debug, Default, Clone, Copy)]
pub struct JitterStats {
    /// Slots recovered from in-band FEC that was actually present.
    pub fec: u64,
    /// Slots concealed with PLC (missing packet, or dry-buffer extrapolation).
    pub plc: u64,
    /// Slots that never played: concealed or skipped.
    pub lost: u64,
    /// Packets that arrived behind the playhead.
    pub late: u64,
    /// Packets recovered from a RED copy carried by a later packet.
    pub red: u64,
    /// Clock-drift windows that ran clean to their end and measured a
    /// peer's skew, and how many of those left the correction switched
    /// on. The estimator needs ten seconds of one peer's unbroken speech
    /// per window; the review doubted it ever gets them (P3-22). These
    /// two are in the log for a release to answer that: if `drift_on`
    /// stays 0 in the field, the estimator and the micro rate
    /// adjustment it drives can go.
    pub drift_windows: u64,
    pub drift_on: u64,
}

/// Per-peer state inside the jitter buffer.
struct PeerBuffer {
    /// Encoded packets + decoder for this peer.
    packets: PacketBuffer,
    /// This peer's volume and mute, as of the mix pass under way.
    settings: PeerAudioSettings,
    /// Timestamp of the last accepted packet.
    last_ts: Option<u32>,
    /// Arrival time of the last accepted packet.
    last_arrival: Option<Instant>,
    /// When a packet was last accepted for this peer.
    last_push: Instant,
    /// Smoothed jitter estimate in seconds (exponential moving average).
    /// Uses asymmetric coefficients: fast attack, slower release.
    jitter_estimate: f32,
    /// How many consecutive hops this peer has had excess buffered media.
    excess_ticks: u32,
    /// While priming, the peer contributes silence until its buffer holds
    /// the target depth. Set on creation and again after a mid-stream
    /// underrun so playout rebuilds a protective cushion.
    priming: bool,
    /// Whether the peer contributed audio on the previous pull (used to
    /// distinguish mid-stream underruns from DTX pauses).
    playing: bool,
    /// Set when the buffer runs dry while playing; resolved on the next
    /// push (recent → underrun, stale → pause). See `UNDERRUN_CONFIRM_WINDOW`.
    dry_at: Option<Instant>,
    /// Last sample this peer contributed to the mix — the starting point
    /// for the synthesized decay tail when the PLC budget is exhausted.
    last_sample: f32,
    /// Remaining fade-in samples after resuming from priming.
    fade_in_remaining: usize,
    /// Cumulative media samples accepted for this peer (drift numerator).
    pushed_samples: u64,
    /// Cumulative wall-paced samples consumed while playing (drift
    /// denominator). Their long-run ratio deviates from 1.0 by
    /// exactly the sender's capture-clock skew.
    consumed_samples: u64,
    /// Open estimation window: (start, pushed at start, consumed at
    /// start).
    drift_window: Option<(Instant, u64, u64)>,
    /// Windows containing an underrun, priming, concealment, shrink,
    /// or mute measure playout disruption, not clock rate — poisoned
    /// windows restart instead of updating the estimate.
    drift_window_dirty: bool,
    /// Smoothed sender clock skew (`+` = sender fast, `-` = slow).
    /// 0.0 = no correction.
    drift_estimate: f64,
    /// Fractional sample carry for the micro rate adjustment.
    slip_frac: f64,
    /// Per-peer output limiter — protects listeners from loud transients
    /// from one peer without ducking everyone else in the mix.
    limiter: Limiter,
}

impl PeerBuffer {
    fn new() -> opus::Result<Self> {
        Ok(Self {
            packets: PacketBuffer::new()?,
            settings: PeerAudioSettings::default(),
            last_ts: None,
            last_arrival: None,
            last_push: Instant::now(),
            jitter_estimate: 0.002, // start at 2ms — optimistic, will rise if needed
            excess_ticks: 0,
            priming: true,
            playing: false,
            dry_at: None,
            last_sample: 0.0,
            fade_in_remaining: 0,
            pushed_samples: 0,
            consumed_samples: 0,
            drift_window: None,
            drift_window_dirty: false,
            drift_estimate: 0.0,
            slip_frac: 0.0,
            limiter: Limiter::new(),
        })
    }

    /// Bookkeeping for "ran dry while playing": record the moment so the
    /// next push can classify it, and rebuild the cushion before playing
    /// again.
    fn mark_dry(&mut self, now: Instant) {
        self.dry_at = Some(now);
        self.priming = true;
        self.playing = false;
        self.drift_window_dirty = true;
    }
}

pub struct AdaptiveJitterBuffer {
    sample_rate: usize,
    target_samples: usize,
    min_samples: usize,
    max_samples: usize,

    buffers: HashMap<u64, PeerBuffer>,
    peer_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
    /// Peers removed via the public API; swept in `adapt()`.
    removed_peers: Arc<Mutex<HashSet<u64>>>,

    /// Self-monitor PCM (mic loopback), mixed after the peers.
    loopback: VecDeque<f32>,

    /// Scratch buffer for per-peer volume/limiting, allocated once.
    scratch: Vec<f32>,
    /// Scratch for `adapt`'s per-peer jitter values.
    jitter_scratch: Vec<f32>,
    /// Scratch input for the per-peer micro rate adjustment,
    /// allocated once like `scratch`.
    scratch_in: Vec<f32>,
    /// Smoothed multi-speaker normalization gain (see `pull_mixed`).
    mix_scale: f32,

    // ── Adaptation state ────────────────────────────────────────────
    /// Counts `adapt()` calls since the last concealment/underrun event.
    /// When this is high, conditions are stable and we can drain.
    stable_ticks: u32,
    /// Concealment slots (FEC + PLC at a due slot) since last `adapt()`:
    /// each one is a packet that was lost or arrived too late.
    concealment_count: u32,
    /// Mid-stream underruns observed since last `adapt()` call.
    underrun_count: u32,
    /// Last target value emitted to the debug log (ms).
    last_logged_target_ms: f32,
    /// Decaying memory of underrun-corrected depth. The drain path will
    /// not descend below this, so the buffer stops repeatedly draining
    /// into a depth that just failed. Field log showed a steady cycle of
    /// drain-to-9ms → real micro-stall → +10 ms → drain-to-9ms again,
    /// each puncture an audible gap. Decays ~40% per 10 s, so old trouble
    /// stops mattering on the same timescale users forget it.
    underrun_floor_ms: f32,
    /// Counters for `AudioStats`, drained by the output worker.
    stats: JitterStats,

    // ── Loss protection floor ───────────────────────────────────────
    /// Last time a due slot had to be concealed or skipped.
    last_loss_at: Option<Instant>,
    /// Duration of the most recent accepted packet, for the floor.
    frame_samples_hint: usize,
    /// Largest RED redundancy span seen recently, in samples.
    red_span_samples: usize,
    last_red_at: Option<Instant>,
}

impl AdaptiveJitterBuffer {
    pub fn new(
        sample_rate: usize,
        initial_target_ms: usize,
        peer_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
        removed_peers: Arc<Mutex<HashSet<u64>>>,
    ) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let initial_ms = initial_target_ms as f32;
        Self {
            sample_rate,
            target_samples: (initial_target_ms * sample_rate) / 1000,
            min_samples: (2 * sample_rate) / 1000, // 2ms floor
            max_samples: (200 * sample_rate) / 1000, // 200ms ceiling
            buffers: HashMap::new(),
            peer_settings,
            removed_peers,
            loopback: VecDeque::with_capacity(LOOPBACK_CAP),
            scratch: Vec::new(),
            jitter_scratch: Vec::new(),
            scratch_in: Vec::new(),
            mix_scale: 1.0,
            stable_ticks: 0,
            concealment_count: 0,
            underrun_count: 0,
            last_logged_target_ms: initial_ms,
            underrun_floor_ms: 0.0,
            stats: JitterStats::default(),
            last_loss_at: None,
            frame_samples_hint: FRAME_SIZE,
            red_span_samples: 0,
            last_red_at: None,
        }
    }

    /// Tell the buffer how the output pulls: in bursts of `burst` samples
    /// (one device callback's worth, at this buffer's rate), `hop` at a
    /// time. A 10 ms-callback device pulls one hop per callback and the
    /// 2 ms floor is enough; a device that asks for 40 ms at once (Android
    /// `AAudio` outside low-latency mode) has the worker pull four hops back
    /// to back, and hops two to four need cushion that has already
    /// arrived — so the floor becomes `burst − hop`. Without it a low
    /// target primes, plays one hop, runs dry inside the same burst and
    /// re-primes, every burst.
    pub fn set_pull_burst(&mut self, burst: usize, hop: usize) {
        let base_floor = (2 * self.sample_rate) / 1000;
        self.min_samples = burst
            .saturating_sub(hop)
            .max(base_floor)
            .min(self.max_samples);
        if self.target_samples < self.min_samples {
            self.target_samples = self.min_samples;
        }
    }

    /// Minimum target while repairs are worth waiting for (ms); 0 when the
    /// link has been clean for `LOSS_MEMORY`.
    fn protection_floor_ms(&self) -> f32 {
        let recent_loss = self.last_loss_at.is_some_and(|t| t.elapsed() < LOSS_MEMORY);
        if !recent_loss {
            return 0.0;
        }
        let recent_red = self.last_red_at.is_some_and(|t| t.elapsed() < LOSS_MEMORY);
        let span = if recent_red {
            self.red_span_samples.max(self.frame_samples_hint)
        } else {
            self.frame_samples_hint
        };
        #[allow(clippy::cast_precision_loss)]
        {
            span as f32 * 1000.0 / self.sample_rate as f32 + PROTECT_MARGIN_MS
        }
    }

    /// Ingest one encoded packet, stamped with its network arrival time.
    ///
    /// This is the primary entry point from the output worker. It:
    /// 1. Classifies a pending dry spell (underrun vs DTX pause)
    /// 2. Updates jitter estimation from arrival/timestamp deltas
    /// 3. Hands the packet to the peer's [`PacketBuffer`]
    ///
    /// Packets may arrive in any order; the packet buffer sorts them. Late
    /// packets (behind the playhead) are counted and dropped.
    pub fn push_packet(&mut self, pkt: AudioPacket, arrival: Instant) {
        if pkt.header.payload_type == PayloadType::Red {
            self.push_red_bundle(pkt, arrival);
            return;
        }
        self.push_primary(pkt, arrival);
    }

    /// Split a RED bundle: redundant copies go straight into the peer's
    /// packet buffer (a duplicate of something that already arrived is
    /// ignored; a copy of something that never arrived is a recovery),
    /// then the primary takes the normal path.
    fn push_red_bundle(&mut self, pkt: AudioPacket, arrival: Instant) {
        let Some(bundle) = red::decode(&pkt.opus_data) else {
            log::debug!("malformed RED bundle from peer {}", pkt.header.ssrc);
            return;
        };
        let h = &pkt.header;
        let mut span = 0usize;
        for block in &bundle.redundant {
            span = span.max(block.ts_offset as usize);
            let seq = h.seq.wrapping_sub(block.seq_distance);
            // A copy of a packet that arrived (nearly all are) is
            // not copied out to be dropped.
            if self
                .buffers
                .get(&h.ssrc)
                .is_some_and(|peer| !peer.packets.wants(seq))
            {
                continue;
            }
            let copy = AudioPacket::new(
                seq,
                h.timestamp.wrapping_sub(block.ts_offset),
                h.ssrc,
                block.data.to_vec(),
            );
            self.push_redundant(copy);
        }
        if !bundle.redundant.is_empty() {
            self.red_span_samples = span;
            self.last_red_at = Some(arrival);
        }
        let primary = AudioPacket::new(h.seq, h.timestamp, h.ssrc, bundle.primary.to_vec());
        self.push_primary(primary, arrival);
    }

    /// A redundant copy: no jitter estimation, no underrun classification —
    /// it isn't a fresh arrival, it's a repair.
    fn push_redundant(&mut self, pkt: AudioPacket) {
        let peer_id = pkt.header.ssrc;
        let Some(peer) = self.buffers.get_mut(&peer_id) else {
            return;
        };
        let pushed = opus::packet::get_nb_samples(&pkt.opus_data, OPUS_SAMPLE_RATE)
            .unwrap_or(FRAME_SIZE) as u64;
        if peer.packets.push(pkt) == Push::Accepted {
            peer.pushed_samples += pushed;
            self.stats.red += 1;
        }
    }

    fn push_primary(&mut self, pkt: AudioPacket, arrival: Instant) {
        let peer_id = pkt.header.ssrc;
        let ts = pkt.header.timestamp;

        // Check mute
        {
            let Ok(settings) = self.peer_settings.lock() else {
                return;
            };
            if settings.get(&peer_id).is_some_and(|s| s.is_muted) {
                if let Some(peer) = self.buffers.get_mut(&peer_id) {
                    peer.packets.clear();
                    peer.drift_window_dirty = true;
                }
                return;
            }
        }

        let peer = match self.buffers.entry(peer_id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => match PeerBuffer::new() {
                Ok(p) => {
                    log::info!("Adding decoder for peer {peer_id}");
                    v.insert(p)
                }
                // Opus init failure is near-impossible; skip this packet
                // and retry on the next one rather than panicking on an
                // audio-adjacent thread.
                Err(e) => {
                    log::error!("Failed to create Opus decoder for peer {peer_id}: {e}");
                    return;
                }
            },
        };

        // Media duration of this packet, for the drift numerator.
        let pushed = opus::packet::get_nb_samples(&pkt.opus_data, OPUS_SAMPLE_RATE)
            .unwrap_or(FRAME_SIZE) as u64;

        match peer.packets.push(pkt) {
            Push::Accepted => {}
            Push::Late => {
                self.stats.late += 1;
                return;
            }
            Push::Duplicate => return,
        }

        // ── Deferred underrun classification ────────────────────
        // Runs before last_ts is updated so we can compare against
        // the previous frame's media clock. Two stories end a dry
        // spell, and the timestamp tells them apart:
        //
        //   Delivery stall: the sender never paused — timestamps are
        //   contiguous with what we already played, the packets were
        //   just late. THAT is an underrun; the target must grow.
        //
        //   Sender pause (DTX): media time jumped by ≈ the wall gap
        //   because the clock ran while nothing was sent. Normal.
        //
        // Wall-time alone can't tell these apart: the gate transmits
        // through the first 200 ms of every pause, so a natural
        // 250–320 ms inter-phrase pause yields a 50–120 ms packet gap
        // — which a wall-clock-only classifier scores as starvation
        // on every other sentence (field log: underrun=1 every few
        // seconds of clean-LAN speech, ratcheting the target +10 ms
        // each time).
        if let Some(dry_time) = peer.dry_at.take() {
            #[allow(clippy::cast_precision_loss)]
            let media_gap = peer
                .last_ts
                .map(|lt| ts.wrapping_sub(lt) as f32 / self.sample_rate as f32);
            let delivery_resumed_fast = dry_time.elapsed() < UNDERRUN_CONFIRM_WINDOW;
            if delivery_resumed_fast && media_gap.is_some_and(|g| g <= UNDERRUN_MEDIA_CONTIGUITY_S)
            {
                self.underrun_count += 1;
            }
        }

        // ── Jitter estimation ────────────────────────────────────
        // Compare how much later the packet arrived vs. how much
        // later its media clock says it should be. DTX pauses move
        // both clocks together, so they contribute ~zero jitter.
        if let (Some(last_arrival), Some(last_ts)) = (peer.last_arrival, peer.last_ts) {
            let arrival_delta = arrival.duration_since(last_arrival).as_secs_f32();
            #[allow(clippy::cast_precision_loss)]
            let expected_delta = ts.wrapping_sub(last_ts) as f32 / self.sample_rate as f32;
            // Only near-continuous packets inform the estimator.
            // Inter-arrival jitter is defined on a flowing stream;
            // with DTX the stream pauses (utterance gaps, ~400 ms
            // silence refreshes), and an interval that spans a pause
            // measures the pause, not the network. Those samples are
            // pure noise, and via the EMA + the "jump target up
            // immediately" rule, a few of them could park the target
            // — and therefore the priming delay every utterance pays
            // — far above what the network needs. Media-time
            // continuity (expected_delta) is the right gate: delayed
            // bursts of continuous speech still qualify, pauses of
            // any kind never do. Reordered packets fail it too
            // (negative media delta wraps huge), which is correct.
            if expected_delta > 0.0 && expected_delta <= 0.1 && arrival_delta < 0.5 {
                let jitter_sample = (arrival_delta - expected_delta).abs();

                // Winsorize: one genuine outlier (a sender stall
                // delivering 300 ms of frames in a burst is media-
                // continuous and correctly passes the gate above) must
                // raise the estimate, not replace it. Field log showed
                // a single burst + 0.3 attack slamming the estimate
                // ~20× in one sample, parking the target at its 200 ms
                // ceiling with zero concealments or underruns.
                let capped = jitter_sample.min(peer.jitter_estimate * 3.0 + 0.005);

                // Asymmetric EMA, RFC 3550-strength gain on the way up
                // (1/16 per sample: a real shift still dominates within
                // ~20 packets), gentler on the way down so the estimate
                // outlives short calm stretches.
                let coeff = if capped > peer.jitter_estimate {
                    0.0625
                } else {
                    0.02
                };
                peer.jitter_estimate = peer.jitter_estimate * (1.0 - coeff) + capped * coeff;
            }
        }
        peer.last_arrival = Some(arrival);
        peer.last_ts = Some(ts);
        peer.pushed_samples += pushed;
        peer.last_push = Instant::now();
        #[allow(clippy::cast_possible_truncation)] // ≤ MAX_FRAME
        {
            self.frame_samples_hint = pushed as usize;
        }

        // Hard cap: prevent runaway growth (e.g., frozen output, paused app).
        // This is a safety valve, not the normal shrink path.
        peer.packets.truncate_to(self.max_samples * 3);
    }

    /// Queue self-monitor audio (mic loopback). Local, lossless, unjittered:
    /// it bypasses the packet buffers and is summed straight into the mix.
    pub fn push_loopback(&mut self, frame: &[f32]) {
        let overflow = (self.loopback.len() + frame.len()).saturating_sub(LOOPBACK_CAP);
        if overflow > 0 {
            self.loopback.drain(..overflow);
        }
        self.loopback.extend(frame.iter().copied());
    }

    /// Pull exactly `n` mixed samples, filling with silence if needed.
    ///
    /// Peers that are priming (new, or recovering from an underrun)
    /// contribute silence until their buffer holds the adaptive target
    /// depth — this is what actually converts "target depth" into
    /// protection against late packets. Normalization is based on peers
    /// that contribute meaningful audio energy to this pull, with the
    /// scale smoothed over ~80 ms to avoid level jumps when speakers
    /// join or leave the mix.
    pub fn pull_mixed(&mut self, n: usize, out: &mut [f32]) {
        debug_assert_eq!(out.len(), n);
        out.fill(0.0);

        // Volume and mute, copied out for this pass: the lock was held
        // across every peer's decode, and the UI's slider waited for it
        // (review P3-21).
        {
            let Ok(settings) = self.peer_settings.lock() else {
                return;
            };
            for (peer_id, peer) in &mut self.buffers {
                peer.settings = settings.get(peer_id).cloned().unwrap_or_default();
            }
        }
        let mut contributing_peers = 0u32;
        let mut fec = 0u64;
        let mut plc = 0u64;
        let mut dry_plc = 0u64;
        let mut lost = 0u64;

        if self.scratch.len() < n {
            self.scratch.resize(n, 0.0);
        }
        let now = Instant::now();
        // Sustained-excess threshold: target + 4 hops.
        let headroom = self.target_samples + n * 4;
        // Never below the pull-burst floor (set_pull_burst): priming to
        // less than one burst re-primes inside every burst.
        let prime_cap = ((60 * self.sample_rate) / 1000).max(self.min_samples);
        let prime_target = self.target_samples.min(prime_cap);

        for peer in self.buffers.values_mut() {
            let peer_settings = peer.settings.clone();
            if peer_settings.is_muted {
                peer.packets.clear();
                peer.playing = false;
                peer.drift_window_dirty = true;
                continue;
            }

            // Priming: hold playout until the target cushion exists so a
            // late packet has time to arrive before its slot plays.
            //
            // Capped at 60 ms: priming is paid in start-of-speech latency
            // (packets arrive in real time, so priming T ms delays the
            // first audible sample by T ms). On a network that genuinely
            // needs a deeper buffer than the cap, utterance starts may
            // conceal briefly and re-prime — a better trade than every
            // utterance on every network paying the worst case up front.
            if peer.priming {
                peer.drift_window_dirty = true;
                if peer.packets.buffered_media() >= prime_target {
                    peer.priming = false;
                    // Soften the resume edge to match the softened cut.
                    peer.fade_in_remaining = RESUME_FADE_SAMPLES;
                } else {
                    // DTX pause: keep the last refresh's noise floor going
                    // (decaying) while refreshes keep arriving, instead of
                    // a blip every 400 ms. Not "playing": priming still
                    // decides when speech resumes.
                    if !peer.playing
                        && peer.last_push.elapsed() < CNG_MAX_GAP
                        && peer.packets.comfort_noise(&mut self.scratch[..n])
                    {
                        for slot in &mut self.scratch[..n] {
                            *slot *= peer_settings.volume;
                        }
                        peer.limiter.process_samples(&mut self.scratch[..n]);
                        for (o, s) in out.iter_mut().zip(&self.scratch[..n]) {
                            *o += s;
                        }
                    }
                    continue;
                }
            }

            // ── Sustained excess → shrink during quiet ──────────────
            // After a jitter spike, packets arrive in bursts and legitimately
            // fill the buffer past target; those play out naturally. Only
            // excess that persists for SHRINK_AFTER_TICKS hops is clock
            // drift or a stale target, and then the packet buffer drops at
            // most one quiet decoded frame per hop.
            let depth = peer.packets.buffered_media();
            let shrink = if depth > headroom {
                peer.excess_ticks = peer.excess_ticks.saturating_add(1);
                peer.excess_ticks > SHRINK_AFTER_TICKS
            } else {
                peer.excess_ticks = 0;
                false
            };
            if shrink {
                peer.drift_window_dirty = true;
            }

            // ── Micro rate adjustment (sender clock drift) ──────────
            // drift_estimate < 0: the sender's capture clock is slow —
            // consume fractionally fewer buffered samples per wall
            // hop and time-stretch them onto the full hop, so the
            // buffer stops draining toward an audible underrun.
            // > 0: the opposite, so the buffer stops growing into the
            // shrink path. 0.0 (default, and every healthy sender):
            // plain copy.
            peer.consumed_samples += n as u64;
            let mut desired = n;
            if peer.drift_estimate != 0.0 {
                #[allow(clippy::cast_precision_loss)] // n is one hop (480)
                {
                    peer.slip_frac += n as f64 * peer.drift_estimate;
                }
                // One hop at the ±2000 ppm clamp moves the carry by at
                // most 0.96, so the truncation is always -1, 0, or +1.
                #[allow(clippy::cast_possible_truncation)]
                let adj = peer.slip_frac.trunc() as i64;
                peer.slip_frac -= adj as f64;
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let base = n as i64;
                #[allow(clippy::cast_sign_loss)] // .max(1) keeps it positive
                {
                    desired = (base + adj).max(1) as usize;
                }
            }

            // ── Decode / conceal into scratch ───────────────────────
            let available;
            if desired == n {
                let rep = peer.packets.fill(&mut self.scratch[..n], true, shrink);
                fec += u64::from(rep.fec);
                plc += u64::from(rep.plc);
                dry_plc += u64::from(rep.dry_plc);
                lost += u64::from(rep.fec + rep.plc + rep.skipped);
                if rep.fec + rep.plc + rep.skipped + rep.dropped_quiet > 0 {
                    // Missing or skipped media is not clock skew: a lossy
                    // stretch must restart the window, not teach the
                    // estimator that the sender's clock is slow.
                    peer.drift_window_dirty = true;
                }
                available = rep.filled;
            } else {
                if self.scratch_in.len() < desired {
                    self.scratch_in.resize(desired, 0.0);
                }
                let rep = peer
                    .packets
                    .fill(&mut self.scratch_in[..desired], true, shrink);
                fec += u64::from(rep.fec);
                plc += u64::from(rep.plc);
                dry_plc += u64::from(rep.dry_plc);
                lost += u64::from(rep.fec + rep.plc + rep.skipped);
                if rep.fec + rep.plc + rep.skipped + rep.dropped_quiet > 0 {
                    // Missing or skipped media is not clock skew.
                    peer.drift_window_dirty = true;
                }
                if rep.filled == desired {
                    // Drain `desired` source samples, emit exactly `n`.
                    // The ratio is within ±0.2%, where linear
                    // interpolation is audibly transparent.
                    resample_linear(&self.scratch_in[..desired], &mut self.scratch[..n]);
                    available = n;
                } else {
                    // Ran dry mid-hop: no stretch, play what we have.
                    self.scratch[..rep.filled].copy_from_slice(&self.scratch_in[..rep.filled]);
                    available = rep.filled;
                }
            }

            if available > 0 {
                for slot in &mut self.scratch[..available] {
                    *slot *= peer_settings.volume;
                }

                // Apply per-peer limiter (hearing protection)
                peer.limiter.process_samples(&mut self.scratch[..available]);

                // Linear fade-in over the first ~3 ms after resuming playout.
                if peer.fade_in_remaining > 0 {
                    let n_fade = peer.fade_in_remaining.min(available);
                    let done = RESUME_FADE_SAMPLES - peer.fade_in_remaining;
                    #[allow(clippy::cast_precision_loss)]
                    for (i, s) in self.scratch[..n_fade].iter_mut().enumerate() {
                        *s *= (done + i + 1) as f32 / RESUME_FADE_SAMPLES as f32;
                    }
                    peer.fade_in_remaining -= n_fade;
                }
                peer.last_sample = self.scratch[available - 1];

                // Sum into output mix and track energy
                let mut peer_energy = 0.0f32;
                for (o, s) in out[..available].iter_mut().zip(&self.scratch[..available]) {
                    *o += *s;
                    peer_energy += *s * *s;
                }
                peer.playing = true;

                // Only count peers with meaningful audio (not silence/DTX residue).
                // Threshold: ~-60 dBFS per sample average.
                #[allow(clippy::cast_precision_loss)]
                if (peer_energy / available as f32) > 1e-6 {
                    contributing_peers += 1;
                }
            }

            if available < n && peer.playing {
                // Ran dry (the PLC budget inside the packet buffer is spent).
                // Whether this was a genuine underrun (packets late
                // mid-stream) or a normal DTX pause can only be told by what
                // happens next, so record the moment and let the next push
                // classify it. Either way, rebuild the cushion before
                // playing again.
                //
                // Soften the cut: an instant step from the last sample to
                // silence is a broadband click. Synthesize a ~2 ms
                // exponential decay tail from where the audio stopped.
                let mut tail = peer.last_sample;
                for slot in out[available..].iter_mut().take(96) {
                    tail *= 0.95;
                    *slot += tail;
                }
                peer.last_sample = 0.0;
                peer.mark_dry(now);
            }
        }

        // ── Self-monitor ────────────────────────────────────────────
        for slot in out.iter_mut() {
            let Some(s) = self.loopback.pop_front() else {
                break;
            };
            *slot += s;
        }

        // Soft-knee normalization: no scaling for 1-2 speakers, gentle
        // sqrt scaling above that. The scale is smoothed (~80 ms) and
        // interpolated across the frame so a third speaker starting or
        // stopping doesn't step the whole mix by 3 dB between frames.
        #[allow(clippy::cast_precision_loss)]
        let target_scale = if contributing_peers > 2 {
            1.0 / ((contributing_peers as f32 - 1.0).sqrt())
        } else {
            1.0
        };
        let scale_start = self.mix_scale;
        let scale_end = scale_start + 0.25 * (target_scale - scale_start);
        if (scale_start - 1.0).abs() > 1e-6 || (scale_end - 1.0).abs() > 1e-6 {
            #[allow(clippy::cast_precision_loss)]
            let inv_len = 1.0 / n.max(1) as f32;
            for (i, s) in out.iter_mut().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 * inv_len;
                *s *= scale_start + (scale_end - scale_start) * t;
            }
        }
        self.mix_scale = scale_end;

        if fec + plc + lost > 0 {
            self.last_loss_at = Some(now);
        }

        // Concealment at a *due slot* is the adaptation signal (a packet was
        // lost or late). Dry-buffer extrapolation is deliberately excluded:
        // the deferred underrun classification owns that verdict, and
        // counting it here would charge the target for every DTX pause
        // (the v0.7.1 bug in a new coat).
        #[allow(clippy::cast_possible_truncation)]
        {
            self.concealment_count = self
                .concealment_count
                .saturating_add((fec + plc).min(u64::from(u32::MAX)) as u32);
        }
        self.stats.fec += fec;
        self.stats.plc += plc + dry_plc;
        self.stats.lost += lost;
    }

    /// Adapt target depth based on jitter estimates and buffer health.
    ///
    /// Called every 10ms by the output worker. Uses two signals:
    ///
    /// 1. **Jitter estimates** (from arrival vs. media-clock deltas) — what
    ///    the network looks like right now.
    /// 2. **Health feedback** (concealment slots, mid-stream underruns) —
    ///    whether the current target is actually too low.
    ///
    /// The target moves **up fast** on problems (scaled by severity) and
    /// **down slowly** during stable periods (over 200 ticks / 2s).
    pub fn adapt(&mut self) {
        // Drop decoder state for peers removed via the API, even if no
        // further packet from them ever arrives.
        if let Ok(mut removed) = self.removed_peers.lock() {
            for id in removed.drain() {
                if self.buffers.remove(&id).is_some() {
                    log::info!("Dropped decoder for removed peer {id}");
                }
            }
        }

        // Prune peers that have been silent long enough that keeping their
        // state (and letting it skew the jitter percentile) serves nothing.
        self.buffers
            .retain(|_, p| !p.packets.is_empty() || p.last_push.elapsed() < PEER_IDLE_TIMEOUT);

        // Decay idle jitter estimates. During DTX silence no samples
        // arrive, so without this a spiked estimate freezes at its peak
        // and keeps re-hoisting the target after every drain (the
        // 160↔200 ms sawtooth seen in the field with zero underruns).
        // ~0.99 per 10 ms tick ≈ estimate thirds every second of silence.
        for p in self.buffers.values_mut() {
            if let Some(last) = p.last_arrival
                && last.elapsed() > Duration::from_secs(1)
            {
                p.jitter_estimate = (p.jitter_estimate * 0.99).max(0.001);
            }
        }

        // ── Per-peer clock-drift estimation ─────────────────────────
        // Inflow vs wall-paced consumption over a clean window is the
        // sender's capture-clock skew. Poisoned windows (underrun,
        // priming, concealment, shrink, mute) restart without updating.
        let now = Instant::now();
        let min_window_consumed = (self.sample_rate as u64) * 8;
        for p in self.buffers.values_mut() {
            match p.drift_window {
                None => {
                    p.drift_window = Some((now, p.pushed_samples, p.consumed_samples));
                    p.drift_window_dirty = false;
                }
                Some((t0, pushed0, consumed0)) => {
                    if p.drift_window_dirty {
                        p.drift_window = Some((now, p.pushed_samples, p.consumed_samples));
                        p.drift_window_dirty = false;
                    } else if now.duration_since(t0) >= DRIFT_WINDOW {
                        let dp = p.pushed_samples - pushed0;
                        let dc = p.consumed_samples - consumed0;
                        if dc >= min_window_consumed {
                            #[allow(clippy::cast_precision_loss)] // window sums ≪ 2^53
                            let skew = (dp as f64 - dc as f64) / dc as f64;
                            let blended = 0.5 * p.drift_estimate
                                + 0.5 * skew.clamp(-MAX_DRIFT_CORRECTION, MAX_DRIFT_CORRECTION);
                            p.drift_estimate = if blended.abs() < DRIFT_DEADZONE {
                                0.0
                            } else {
                                blended
                            };
                            self.stats.drift_windows += 1;
                            if p.drift_estimate != 0.0 {
                                self.stats.drift_on += 1;
                            }
                        }
                        p.drift_window = Some((now, p.pushed_samples, p.consumed_samples));
                    }
                }
            }
        }

        if self.buffers.is_empty() {
            return;
        }

        // ── Compute jitter-based target ─────────────────────────────
        // Use 75th percentile across active peers instead of max.
        // This prevents one bad peer from inflating the buffer for everyone.
        // Into a list kept between calls: this runs 100 times a second
        // and allocated one each time (review P3-21).
        let mut jitter_values = std::mem::take(&mut self.jitter_scratch);
        jitter_values.clear();
        jitter_values.extend(
            self.buffers
                .values()
                .filter(|p| p.last_arrival.is_some())
                .map(|p| p.jitter_estimate * 1000.0), // convert to ms
        );

        let jitter_target_ms = if jitter_values.is_empty() {
            0.0
        } else {
            jitter_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let p75_idx = (jitter_values.len() * 3) / 4;
            let p75_jitter = jitter_values[p75_idx.min(jitter_values.len() - 1)];
            // Target = 2× p75 jitter — enough headroom without being excessive
            p75_jitter * 2.0
        };
        self.jitter_scratch = jitter_values;

        // ── Health-based adjustment ─────────────────────────────────
        let had_problems = self.concealment_count > 0 || self.underrun_count > 0;

        // Decay the underrun floor: recent trouble matters, old doesn't.
        self.underrun_floor_ms = (self.underrun_floor_ms * 0.9995).max(0.0);

        if had_problems {
            // Problems detected — increase target, scaled by severity.
            // Underruns are weighted 2× because they mean audio was
            // audibly interrupted, not just recovered by concealment.
            #[allow(clippy::cast_precision_loss)]
            let severity = self.concealment_count as f32 + self.underrun_count as f32 * 2.0;
            let bump_ms = (severity * 5.0).min(40.0); // 5ms per event, cap 40ms per cycle
            let current_ms = self.target_ms();
            let new_target_ms = (current_ms + bump_ms).min(self.max_ms());
            self.target_samples = self.ms_to_samples(new_target_ms);
            self.stable_ticks = 0;
            // Underruns teach the floor: don't drain back below ~3/4 of
            // the depth that an actual starvation event pushed us to.
            if self.underrun_count > 0 {
                self.underrun_floor_ms = self.underrun_floor_ms.max(new_target_ms * 0.75);
            }
        } else {
            // No problems — count stable ticks and slowly drain toward
            // the jitter-based target (which itself is slowly decreasing
            // via the asymmetric EMA).
            self.stable_ticks = self.stable_ticks.saturating_add(1);

            // After 200 stable ticks (~2s with no issues), start draining.
            // Drain rate: 0.2ms per tick = ~2ms/sec of target reduction —
            // slow enough that a single missed drain cycle doesn't cause
            // an underrun on networks with periodic jitter bursts.
            if self.stable_ticks > 200 {
                let current_ms = self.target_ms();
                // Drain toward the jitter-based target, but never below min.
                let floor_ms = jitter_target_ms
                    .max(self.min_ms())
                    .max(self.underrun_floor_ms);
                let new_target_ms = (current_ms - 0.2).max(floor_ms);
                self.target_samples = self.ms_to_samples(new_target_ms);
            }
        }

        // ── Always respect jitter-based minimum ─────────────────────
        // If jitter estimate says we need more than current target,
        // jump up immediately (don't wait for underruns to prove it).
        let jitter_samples = self.ms_to_samples(jitter_target_ms);
        if jitter_samples > self.target_samples {
            self.target_samples = jitter_samples;
        }

        // ── Loss-protection floor ───────────────────────────────────
        // Repairs (LBRR, RED) live in the *next* packet(s); hold playout
        // long enough for them to arrive while loss is being observed.
        let protect = self.ms_to_samples(self.protection_floor_ms());
        if protect > self.target_samples {
            self.target_samples = protect;
        }

        // ── Clamp to [min, max] ─────────────────────────────────────
        self.target_samples = self
            .target_samples
            .clamp(self.min_samples, self.max_samples);

        // Debug-log meaningful target moves — cheap field telemetry for
        // exactly the "why is cold-start latency high" question.
        let target_now = self.target_ms();
        if (target_now - self.last_logged_target_ms).abs() >= 5.0 {
            log::debug!(
                "jitter target {:.1} ms -> {:.1} ms (conceal={}, underrun={}, floor={:.1})",
                self.last_logged_target_ms,
                target_now,
                self.concealment_count,
                self.underrun_count,
                self.underrun_floor_ms
            );
            self.last_logged_target_ms = target_now;
        }

        // Reset event counters for next cycle
        self.concealment_count = 0;
        self.underrun_count = 0;
    }

    /// Take the accumulated loss/concealment counters (for `AudioStats`).
    pub fn drain_stats(&mut self) -> JitterStats {
        std::mem::take(&mut self.stats)
    }

    #[must_use]
    pub fn occupancy_ms(&self) -> f32 {
        if self.buffers.is_empty() {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let avg: f32 = self
            .buffers
            .values()
            .map(|p| p.packets.buffered_media() as f32)
            .sum::<f32>()
            / self.buffers.len() as f32;
        #[allow(clippy::cast_precision_loss)]
        {
            (avg * 1000.0) / self.sample_rate as f32
        }
    }

    /// The floor the target never drains below (ms), for logs.
    #[must_use]
    pub fn min_target_ms(&self) -> f32 {
        self.min_ms()
    }

    /// Current target depth in milliseconds (for stats).
    #[must_use]
    pub fn target_ms(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            (self.target_samples as f32 * 1000.0) / self.sample_rate as f32
        }
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn min_ms(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            (self.min_samples as f32 * 1000.0) / self.sample_rate as f32
        }
    }

    fn max_ms(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            (self.max_samples as f32 * 1000.0) / self.sample_rate as f32
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn ms_to_samples(&self, ms: f32) -> usize {
        ((ms * self.sample_rate as f32) / 1000.0) as usize
    }
}

/// Linearly interpolate `input` onto `out`, preserving endpoints.
/// Used only for length ratios within ±1%, where linear interpolation
/// is audibly transparent for speech.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // hop-sized buffers; positions are non-negative and small
fn resample_linear(input: &[f32], out: &mut [f32]) {
    if input.is_empty() || out.is_empty() {
        return;
    }
    if input.len() == out.len() {
        out.copy_from_slice(input);
        return;
    }
    let step = (input.len() - 1) as f64 / (out.len() - 1).max(1) as f64;
    for (i, o) in out.iter_mut().enumerate() {
        let pos = i as f64 * step;
        let idx = pos as usize;
        let frac = (pos - idx as f64) as f32;
        let a = input[idx];
        let b = input[(idx + 1).min(input.len() - 1)];
        *o = a + (b - a) * frac;
    }
}

#[cfg(test)]
mod tests {
    mod resample_linear {
        use super::super::resample_linear;

        #[test]
        fn copies_input_when_lengths_match() {
            let input = [1.0f32, 2.0, 3.0, 4.0];
            let mut out = [0.0f32; 4];
            resample_linear(&input, &mut out);
            // A 1:1 resample copies samples verbatim.
            #[expect(clippy::float_cmp)]
            {
                assert_eq!(out, input);
            }
        }

        #[test]
        fn preserves_endpoints_when_stretching_by_one_sample() {
            let input: Vec<f32> = (0..479).map(|i| i as f32).collect();
            let mut out = vec![0.0f32; 480];
            resample_linear(&input, &mut out);
            assert!(
                out[0].abs() < 1e-4 && (out[479] - 478.0).abs() < 1e-3,
                "endpoints ({}, {})",
                out[0],
                out[479]
            );
        }

        #[test]
        fn stays_close_to_a_sine_when_compressing_by_one_sample() {
            let omega = 440.0 * std::f32::consts::TAU / 48_000.0;
            let input: Vec<f32> = (0..481).map(|i| (i as f32 * omega).sin()).collect();
            let mut out = vec![0.0f32; 480];
            resample_linear(&input, &mut out);
            let step = 480.0f32 / 479.0;
            let worst = out
                .iter()
                .enumerate()
                .map(|(i, &y)| (y - (i as f32 * step * omega).sin()).abs())
                .fold(0.0f32, f32::max);
            assert!(worst < 0.01, "worst deviation {worst}");
        }
    }

    mod end_to_end {
        use super::super::*;

        fn buffer() -> AdaptiveJitterBuffer {
            AdaptiveJitterBuffer::new(
                48_000,
                10,
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(HashSet::new())),
            )
        }

        fn encoder() -> opus::Encoder {
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)
                .expect("encoder")
        }

        fn packet(enc: &mut opus::Encoder, seq: u32, ssrc: u64) -> AudioPacket {
            let pcm: Vec<f32> = (0..FRAME_SIZE)
                .map(|i| {
                    ((seq as usize * FRAME_SIZE + i) as f32 * 300.0 * std::f32::consts::TAU
                        / 48_000.0)
                        .sin()
                        * 0.4
                })
                .collect();
            let mut buf = vec![0u8; 1024];
            let n = enc.encode_float(&pcm, &mut buf).expect("encode");
            AudioPacket::new(seq, seq * FRAME_SIZE as u32, ssrc, buf[..n].to_vec())
        }

        #[test]
        fn in_order_stream_plays_after_priming_without_concealment() {
            let mut jb = buffer();
            let mut enc = encoder();
            let t0 = Instant::now();
            for seq in 0..5 {
                jb.push_packet(packet(&mut enc, seq, 7), t0);
            }
            let mut out = vec![0.0f32; 480];
            let mut energy = 0.0f32;
            for _ in 0..10 {
                jb.pull_mixed(480, &mut out);
                energy += out.iter().map(|s| s * s).sum::<f32>();
            }
            let stats = jb.drain_stats();
            assert!(energy > 0.0, "no audio reached the mix");
            assert_eq!(
                (stats.fec, stats.plc, stats.lost, stats.late, stats.red),
                (0, 0, 0, 0, 0)
            );
        }

        /// Packet 2 is never delivered; packet 3 carries a RED copy of it.
        /// The copy must land in the buffer as a recovery and slot 2 must
        /// play from it — no concealment, no loss.
        #[test]
        fn red_copy_repairs_a_missing_packet_exactly() {
            let mut jb = buffer();
            let mut enc = encoder();
            let t0 = Instant::now();
            let p0 = packet(&mut enc, 0, 7);
            let p1 = packet(&mut enc, 1, 7);
            let p2 = packet(&mut enc, 2, 7); // lost on the wire
            let p3 = packet(&mut enc, 3, 7);

            jb.push_packet(p0, t0);
            jb.push_packet(p1, t0);
            let mut bundle = Vec::new();
            red::encode(
                &p3.opus_data,
                &[(FRAME_SIZE as u32, &p2.opus_data)],
                &mut bundle,
            );
            let p3_red =
                AudioPacket::new_typed(3, 3 * FRAME_SIZE as u32, 7, PayloadType::Red, bundle);
            jb.push_packet(p3_red, t0);

            let mut out = vec![0.0f32; 480];
            for _ in 0..8 {
                jb.pull_mixed(480, &mut out);
            }
            let stats = jb.drain_stats();
            assert_eq!(stats.red, 1, "the copy of packet 2 is a recovery");
            assert_eq!((stats.fec, stats.plc, stats.lost), (0, 0, 0));
        }

        /// The usual bundle: its redundant block is of a packet that
        /// arrived. Nothing is recovered, nothing concealed, and the
        /// stream plays as without redundancy (review P3-21: the block
        /// is no longer copied out first).
        #[test]
        fn red_copy_of_a_packet_that_arrived_changes_nothing() {
            let mut jb = buffer();
            let mut enc = encoder();
            let t0 = Instant::now();
            let p0 = packet(&mut enc, 0, 7);
            let p1 = packet(&mut enc, 1, 7);
            let p2 = packet(&mut enc, 2, 7);
            let p3 = packet(&mut enc, 3, 7);

            let mut bundle = Vec::new();
            red::encode(
                &p3.opus_data,
                &[
                    (2 * FRAME_SIZE as u32, &p1.opus_data),
                    (FRAME_SIZE as u32, &p2.opus_data),
                ],
                &mut bundle,
            );
            jb.push_packet(p0, t0);
            jb.push_packet(p1, t0);
            jb.push_packet(p2, t0);
            let p3_red =
                AudioPacket::new_typed(3, 3 * FRAME_SIZE as u32, 7, PayloadType::Red, bundle);
            jb.push_packet(p3_red, t0);

            let mut out = vec![0.0f32; 480];
            for _ in 0..8 {
                jb.pull_mixed(480, &mut out);
            }
            let stats = jb.drain_stats();
            assert_eq!(stats.red, 0, "both copies were of packets that arrived");
            assert_eq!((stats.fec, stats.plc, stats.lost), (0, 0, 0));
        }

        /// Android `AAudio`'s normal mode asks for 40 ms per callback, so
        /// the output worker pulls four hops back to back every 40 ms
        /// while packets arrive every 20 ms. Over two seconds of steady
        /// speech, counts the hops that did not play real audio after
        /// playout first started: silent hops plus concealed and
        /// late-dropped frames (PLC sounds as broken as a gap).
        fn damage_with_bursty_pulls(set_burst: bool) -> (usize, u64, u64) {
            let mut jb = buffer();
            if set_burst {
                jb.set_pull_burst(1920, 480);
            }
            let mut enc = encoder();
            let t0 = Instant::now();
            let mut out = vec![0.0f32; 480];
            let mut started = false;
            let mut silent = 0;
            let (mut plc, mut late) = (0, 0);
            let mut seq = 0u32;
            for t_ms in (0..2_000u64).step_by(10) {
                if t_ms % 20 == 0 {
                    jb.push_packet(packet(&mut enc, seq, 7), t0 + Duration::from_millis(t_ms));
                    seq += 1;
                }
                if t_ms % 40 == 0 {
                    for _ in 0..4 {
                        jb.pull_mixed(480, &mut out);
                        let audible = out.iter().any(|s| s.abs() > 1e-3);
                        if audible {
                            started = true;
                        } else if started {
                            silent += 1;
                        }
                    }
                }
                jb.adapt();
                let st = jb.drain_stats();
                plc += st.plc + st.fec;
                late += st.late;
            }
            (silent, plc, late)
        }

        #[test]
        fn bursty_pulls_play_clean_once_the_burst_is_known() {
            let before = damage_with_bursty_pulls(false);
            let after = damage_with_bursty_pulls(true);
            eprintln!("SIM (silent hops, concealed, late) before={before:?} after={after:?}");
            assert_eq!(
                after,
                (0, 0, 0),
                "with the burst floor (without: {before:?})"
            );
        }

        #[test]
        fn pull_burst_floor_is_burst_minus_one_hop() {
            let mut jb = buffer();
            jb.set_pull_burst(480, 480);
            assert!(
                (jb.min_target_ms() - 2.0).abs() < 0.01,
                "desktop keeps 2 ms"
            );
            jb.set_pull_burst(1920, 480);
            assert!((jb.min_target_ms() - 30.0).abs() < 0.01);
            assert!(jb.target_ms() >= 30.0, "target lifted to the floor");
        }

        #[test]
        fn late_packet_is_counted_not_played() {
            let mut jb = buffer();
            let mut enc = encoder();
            let t0 = Instant::now();
            for seq in 0..3 {
                jb.push_packet(packet(&mut enc, seq, 7), t0);
            }
            let mut out = vec![0.0f32; 480];
            for _ in 0..6 {
                jb.pull_mixed(480, &mut out);
            }
            jb.push_packet(packet(&mut enc, 0, 7), t0);
            assert_eq!(jb.drain_stats().late, 1);
        }
    }
}
