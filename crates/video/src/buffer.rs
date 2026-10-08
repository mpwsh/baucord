//! Reorder buffer for incoming video packets.
//!
//! Video frames arrive over an unreliable channel (chunked, reassembled),
//! so they can arrive out of order or not at all, and the decoder drains
//! them from a different loop than the one that receives them. This
//! buffer sits between the two and enforces one invariant: **packets are
//! handed to the decoder in sequence order, and never after a silent
//! gap** — feeding a P-frame whose reference frames were lost produces
//! smearing/ghosting artifacts that persist until the next keyframe.
//!
//! # Latency model
//!
//! Two clocks govern release:
//!
//! **Playout pacing.** Every packet carries the sender's capture
//! timestamp. The buffer maps that clock onto the local one by tracking
//! the fastest observed path (smallest `arrival − timestamp`) and
//! releases each frame at `timestamp + fastest_path + playout_delay` —
//! the sender's cadence, not the network's. Without this, a 200 KB
//! keyframe that takes ~200 ms of wire time is followed by a dozen
//! deltas that arrive bunched behind it, and the viewer sees the stream
//! lurch slow-then-fast every keyframe interval. The delay is adaptive:
//! a decaying peak of how late frames have been arriving relative to the
//! fastest path, clamped to [`PLAYOUT_DELAY_MIN`]..=[`PLAYOUT_DELAY_MAX`].
//! On a clean link it sits at the minimum; a stream with big keyframes
//! settles near their serialization time. Frames that arrive later than
//! their slot are released at once — pacing never adds to lateness.
//!
//! **Reorder window.** Time is also spent when a packet is actually
//! missing:
//!
//! - If `next_seq` is absent but later packets exist, delivery pauses for
//!   a *reorder window* to give the packet time to arrive — from a
//!   reorder, or from the transport's resend (the video channel is
//!   reliable-unordered, so a lost chunk is retransmitted after its
//!   resend interval). The window is the larger of 2× measured arrival
//!   jitter and the longest recent repair, clamped to
//!   [`REORDER_WAIT_MIN`]..=[`REORDER_WAIT_MAX`].
//! - When the window expires, the buffer recovers: it jumps forward to a
//!   buffered **keyframe** if one exists (clean restart). Otherwise it
//!   **freezes**: buffered deltas are discarded, the buffer drops back to
//!   the unprimed state, and a *keyframe request* is raised — every frame
//!   past an unhealed gap references lost data, and decoding them anyway
//!   paints reference-corruption smear (ghosting, dragged blocks) until
//!   the next IDR. For viewers actually watching a stream, a short
//!   freeze on the last clean frame reads far better than motion made of
//!   artifacts. Delivery resumes at the next keyframe.
//!
//! # Keyframe requests
//!
//! The buffer never talks to the network; it just raises a flag when
//! recovery needs sender cooperation. The owner polls
//! [`VideoJitterBuffer::take_keyframe_request`] after draining and relays
//! it to the sender (which calls `VideoEncoder::force_keyframe`). The
//! flag is rate-limited internally so a broken stream can't cause
//! request spam.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// How long the buffer will wait, unprimed, watching undecodable delta
/// frames stream past before asking the sender for a keyframe. Covers
/// the "joined mid-GOP" case.
const UNPRIMED_REQUEST_AFTER: Duration = Duration::from_millis(400);

/// Minimum spacing between keyframe requests. Below the sender's
/// keyframe interval by design: a request, its IDR, and that IDR's loss
/// can all happen within one interval.
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_millis(700);

/// Bounds for the reorder window (how long to hold out for a missing
/// packet before recovering). Derived at runtime from measured jitter
/// and observed repair times, within these bounds.
///
/// The minimum is the transport's repair latency: a lost chunk is
/// resent `VIDEO_RESEND` (150 ms) after it was first sent, plus one-way
/// transit, so it lands ~150–200 ms after the frames that overtook it.
/// A shorter window can never see a repair — the 2026-08-30 viewer log
/// shows exactly that: a 20 ms window turned every single lost chunk
/// into a keyframe request (7 in 30 s, felt as a stutter every ~5 s),
/// and the repair-time estimator never got a sample to learn from. The
/// price is paid only during loss: a frame the sender itself dropped
/// (no repair coming) costs 200 ms instead of 20 before recovery.
///
/// The maximum covers a slow hop's resend plus jitter.
const REORDER_WAIT_MIN: Duration = Duration::from_millis(200);
const REORDER_WAIT_MAX: Duration = Duration::from_millis(350);
/// Headroom added to the longest observed repair when sizing the window.
const REPAIR_MARGIN_MS: f64 = 20.0;

/// Bounds for the playout delay (see the module docs). The minimum is
/// about one frame at 60 fps of slack so ordinary arrival jitter never
/// makes a frame late; the maximum caps the latency spent absorbing
/// keyframe bursts (a VBV-sized keyframe at stream bitrate is ~200 ms
/// of wire time, so this covers it at full rate — lowering the encoder's
/// `VBV_MAX_BITS` is the lever if that ever needs to be smaller).
const PLAYOUT_DELAY_MIN: Duration = Duration::from_millis(20);
const PLAYOUT_DELAY_MAX: Duration = Duration::from_millis(200);
/// Headroom added to the lateness peak.
const PLAYOUT_MARGIN_MS: f64 = 5.0;
/// Decay of the lateness and repair peaks when quiet, ms per second: a
/// 200 ms keyframe peak fades over ~20 s if keyframes stop being late,
/// and is refreshed every keyframe interval while they aren't.
const PEAK_DECAY_MS_PER_S: f64 = 10.0;
/// The fastest-path offset is the minimum over this many recent
/// buckets of [`OFFSET_BUCKET`] each (a 2 s window). A window, not an
/// all-time minimum: a hardware encoder draining its pipeline can emit
/// several frames at once, and those arrive *early* relative to their
/// capture stamps — an all-time minimum would lock onto that burst and
/// mark every normal frame late until clock drift caught up. A window
/// forgets it in 2 s and tracks drift for free.
const OFFSET_BUCKETS: usize = 4;
const OFFSET_BUCKET: Duration = Duration::from_millis(500);
/// The backlog cap, in seconds of stream at the frame rate the buffer
/// has learned from arrivals. Longer than the sender's keyframe interval
/// (2 s), so a backlog that reaches it holds a keyframe to jump to: a
/// cap fixed at 90 frames (3 s at the 30 fps the buffer is created
/// for) was 1.5 s of a 60 fps stream, and about half the overflows
/// found no keyframe and froze (2026-10-08, MOB-18).
const BACKLOG_CAP_SECS: f32 = 3.0;
/// A frame this far behind live when its turn comes: skip to the
/// newest buffered keyframe instead of decoding a backlog the viewer
/// would watch late — well before the cap, so a device that cannot keep
/// up drops video and stays near its sound.
const CATCH_UP_BEHIND: Duration = Duration::from_secs(1);
/// An implied-offset jump larger than this means the sender's clock
/// restarted (new share): re-anchor instead of holding frames for ages
/// or treating them all as hopelessly late.
const CLOCK_JUMP_MS: f64 = 5_000.0;

/// An encoded video packet waiting in the jitter buffer.
#[derive(Debug)]
pub struct BufferedPacket {
    pub seq: u32,
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub arrival_time: Instant,
    /// Sender capture time, ms on the sender's clock (wraps). Drives
    /// playout pacing.
    pub timestamp_ms: u32,
}

/// Counters for observability. All monotonic except `current_depth`.
#[derive(Debug, Default, Clone)]
pub struct JitterBufferStats {
    /// Packets pushed into the buffer.
    pub packets_received: u64,
    /// Packets released to the decoder.
    pub packets_delivered: u64,
    /// Packets discarded because the buffer hit its size cap.
    pub packets_dropped_overflow: u64,
    /// Packets discarded because their sequence was already released.
    pub packets_dropped_late: u64,
    /// Sequence gaps that expired the reorder window (packet truly lost).
    pub gaps_detected: u64,
    /// Packets discarded because an unhealed gap broke their reference
    /// chain (the buffer froze rather than decode them into smear).
    pub packets_dropped_broken_reference: u64,
    /// Recoveries that jumped forward to a buffered keyframe.
    pub keyframe_resets: u64,
    /// Keyframe requests raised toward the sender.
    pub keyframe_requests: u64,
    /// Packets currently buffered.
    pub current_depth: usize,
}

/// Sequence-ordering reorder buffer with keyframe-aware loss recovery.
///
/// See the module docs for the latency model. Not thread-safe by design —
/// owned and driven entirely by the screen thread.
pub struct VideoJitterBuffer {
    /// Buffered packets keyed by sequence number.
    ///
    /// Note: `BTreeMap` ordering is numeric, not wraparound-aware, so all
    /// "which packet is next/first/latest" decisions use the wrapping
    /// comparisons below rather than map iteration order.
    pending: BTreeMap<u32, BufferedPacket>,
    /// Next sequence number to release. `None` until primed.
    next_seq: Option<u32>,
    /// Whether we've seen the first keyframe (nothing is decodable before it).
    primed: bool,
    /// When the current sequence gap was first observed (reorder window start).
    gap_since: Option<Instant>,
    /// When packets started arriving while unprimed (keyframe-wait timer).
    unprimed_since: Option<Instant>,
    /// Set when recovery needs a fresh IDR from the sender.
    want_keyframe: bool,
    /// Last time a keyframe request was handed out (rate limiting).
    last_keyframe_request: Option<Instant>,
    /// Arrival-jitter estimator driving the reorder window.
    jitter: JitterEstimate,
    /// Sender-clock → local-clock mapping and lateness/repair tracking.
    playout: PlayoutClock,
    /// Test hook: fixed reorder window override.
    #[cfg(test)]
    reorder_wait_override: Option<Duration>,
    /// Test hook: fixed playout delay override.
    #[cfg(test)]
    playout_delay_override: Option<Duration>,
    pub stats: JitterBufferStats,
}

impl VideoJitterBuffer {
    /// `fps` seeds the jitter estimator and sizes the overflow cap; the
    /// estimator then learns the sender's real cadence from arrivals, so
    /// the seed only matters for the first second or so of a stream.
    #[must_use]
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        Self {
            pending: BTreeMap::new(),
            next_seq: None,
            primed: false,
            gap_since: None,
            unprimed_since: None,
            want_keyframe: false,
            last_keyframe_request: None,
            jitter: JitterEstimate::new(fps),
            playout: PlayoutClock::new(),
            #[cfg(test)]
            reorder_wait_override: None,
            #[cfg(test)]
            playout_delay_override: None,
            stats: JitterBufferStats::default(),
        }
    }

    /// Insert a packet that arrived from the network.
    pub fn push(&mut self, packet: BufferedPacket) {
        self.stats.packets_received += 1;
        self.jitter.record_arrival(packet.arrival_time);
        self.playout
            .observe(packet.arrival_time, packet.timestamp_ms);

        // Late packet: its slot has already been released or skipped.
        if self.primed {
            if let Some(next) = self.next_seq {
                if packet.seq != next && !is_seq_after(packet.seq, next) {
                    self.stats.packets_dropped_late += 1;
                    return;
                }
            }
        }

        if !self.primed {
            if packet.is_keyframe {
                // Prime on the first keyframe; nothing before it is decodable.
                let kf = packet.seq;
                self.next_seq = Some(kf);
                self.primed = true;
                self.unprimed_since = None;
                self.pending.retain(|&s, _| s == kf || is_seq_after(s, kf));
            } else {
                // Undecodable delta frames are streaming past — if this
                // goes on long enough, ask the sender for an IDR instead
                // of waiting out its keyframe interval.
                let since = *self.unprimed_since.get_or_insert(packet.arrival_time);
                if packet.arrival_time.duration_since(since) >= UNPRIMED_REQUEST_AFTER {
                    self.want_keyframe = true;
                }
            }
        }

        self.pending.insert(packet.seq, packet);

        if self.pending.len() > self.max_depth() {
            self.recover_from_overflow();
        }

        self.stats.current_depth = self.pending.len();
    }

    /// Release the next packet in sequence, if the buffer's rules allow.
    ///
    /// Call repeatedly until it returns `None`. Afterwards, poll
    /// [`Self::take_keyframe_request`] and forward it to the sender.
    pub fn pull_next(&mut self) -> Option<BufferedPacket> {
        self.pull_next_at(Instant::now())
    }

    fn pull_next_at(&mut self, now: Instant) -> Option<BufferedPacket> {
        if !self.primed {
            return None;
        }
        let next = self.next_seq?;

        // Happy path: the next packet is here — release it at its
        // playout time. A packet already past its slot goes at once.
        if let Some(packet) = self.pending.get(&next) {
            if now < self.playout_due(packet) {
                return None;
            }
            let far_behind = self
                .behind_live(packet.timestamp_ms, now)
                .is_some_and(|behind| behind >= CATCH_UP_BEHIND);
            if far_behind {
                if let Some(kf) = self.newest_keyframe_after(next) {
                    self.jump_to(kf);
                    return self.pull_next_at(now);
                }
            }
            if let Some(since) = self.gap_since {
                // It was missing when we looked earlier and has since
                // arrived (reorder or transport resend): remember how
                // long that took so the window keeps covering it.
                self.playout
                    .note_repair(packet.arrival_time.saturating_duration_since(since));
            }
            let packet = self.pending.remove(&next)?;
            self.next_seq = Some(next.wrapping_add(1));
            self.gap_since = None;
            self.stats.packets_delivered += 1;
            self.stats.current_depth = self.pending.len();
            return Some(packet);
        }

        // Is anything buffered *after* the missing slot?
        let has_future = self.pending.keys().any(|&s| is_seq_after(s, next));
        if !has_future {
            // Fully caught up — nothing missing, just nothing new yet.
            self.gap_since = None;
            return None;
        }

        // A gap: later packets exist but `next` is missing. Hold for the
        // reorder window before declaring it lost — and never recover
        // before the earliest later frame's own playout slot, since
        // nothing could be shown sooner anyway.
        let since = *self.gap_since.get_or_insert(now);
        let earliest_later_due = self
            .pending
            .iter()
            .filter(|(&s, _)| is_seq_after(s, next))
            .min_by_key(|(&s, _)| s.wrapping_sub(next))
            .map(|(_, p)| self.playout_due(p));
        let window = self.reorder_wait();
        if now.duration_since(since) < window || earliest_later_due.is_some_and(|due| now < due) {
            return None;
        }

        // Window expired: the packet is lost. Recover.
        self.gap_since = None;
        self.stats.gaps_detected += 1;
        // Counted in the viewer's summary line (`lost`); the when is
        // only for a debug log.
        log::debug!(
            "jitter buffer: seq {next} still missing after {} ms (window {} ms) — recovering",
            now.duration_since(since).as_millis(),
            window.as_millis()
        );

        if let Some(kf) = self.first_keyframe_after(next) {
            // Clean restart from a buffered keyframe: everything before it
            // is unusable (broken reference chains), everything from it on
            // is a fresh GOP.
            self.pending.retain(|&s, _| s == kf || is_seq_after(s, kf));
            self.stats.keyframe_resets += 1;
            let packet = self.pending.remove(&kf).expect("keyframe located above");
            self.next_seq = Some(kf.wrapping_add(1));
            self.stats.packets_delivered += 1;
            self.stats.current_depth = self.pending.len();
            return Some(packet);
        }

        // No keyframe buffered: every buffered and future delta frame
        // references data that is gone. Decoding them would keep motion
        // on screen at the cost of reference-corruption smear until the
        // next IDR — the wrong trade for people *watching* a stream.
        // Freeze instead: discard the orphaned deltas, drop back to
        // unprimed (only a keyframe restarts delivery — the same
        // machinery as stream start), and ask the sender for an IDR.
        // If that IDR is itself lost, the unprimed-grace logic in
        // `push` re-raises the request, so recovery self-heals.
        let dropped = self.pending.len();
        self.pending.clear();
        self.stats.packets_dropped_broken_reference += dropped as u64;
        self.primed = false;
        self.next_seq = None;
        self.unprimed_since = Some(now);
        self.want_keyframe = true;
        self.stats.current_depth = 0;
        log::debug!(
            "gap at seq {next} with no buffered keyframe: froze, dropped {dropped} delta(s), requesting IDR"
        );
        None
    }

    /// True at most once per [`KEYFRAME_REQUEST_COOLDOWN`] when recovery
    /// wants a fresh IDR from the sender. The caller relays this to the
    /// sender's encoder (`force_keyframe`).
    pub fn take_keyframe_request(&mut self) -> bool {
        if !self.want_keyframe {
            return false;
        }
        let now = Instant::now();
        if let Some(last) = self.last_keyframe_request {
            if now.duration_since(last) < KEYFRAME_REQUEST_COOLDOWN {
                return false;
            }
        }
        self.want_keyframe = false;
        self.last_keyframe_request = Some(now);
        self.stats.keyframe_requests += 1;
        true
    }

    /// Number of packets currently buffered.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    /// Buffered span in milliseconds: arrival-time spread between the
    /// oldest and newest pending packets.
    #[must_use]
    pub fn depth_ms(&self) -> f32 {
        let mut oldest: Option<Instant> = None;
        let mut newest: Option<Instant> = None;
        for p in self.pending.values() {
            oldest = Some(oldest.map_or(p.arrival_time, |o| o.min(p.arrival_time)));
            newest = Some(newest.map_or(p.arrival_time, |n| n.max(p.arrival_time)));
        }
        match (oldest, newest) {
            (Some(o), Some(n)) => n.duration_since(o).as_secs_f32() * 1000.0,
            _ => 0.0,
        }
    }

    /// Current reorder window in milliseconds — the longest the buffer
    /// will stall on a missing packet before recovering.
    #[must_use]
    pub fn adaptive_target_ms(&self) -> f32 {
        self.reorder_wait().as_secs_f32() * 1000.0
    }

    /// Current playout delay in milliseconds — the steady latency the
    /// pacer spends to release frames at the sender's cadence.
    #[must_use]
    pub fn playout_delay_ms(&self) -> f32 {
        self.playout_delay().as_secs_f32() * 1000.0
    }

    /// Smoothed arrival-jitter estimate in milliseconds.
    #[must_use]
    pub fn adaptive_jitter_ms(&self) -> f32 {
        self.jitter.jitter_ms()
    }

    /// Whether the first keyframe has been seen.
    #[must_use]
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// How far behind live a frame with this sender timestamp is at
    /// `at`: how much later than the fastest delivery seen it is being
    /// handled. The playout delay and network wobble are part of it
    /// (tens of ms); a decoder slower than the stream makes it grow
    /// without bound. `None` until the clock has a mapping.
    #[must_use]
    pub fn behind_live(&self, timestamp_ms: u32, at: Instant) -> Option<Duration> {
        self.playout
            .local_time_of(timestamp_ms)
            .map(|live| at.saturating_duration_since(live))
    }

    /// Clear all state (e.g. when the peer restarts their share).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.next_seq = None;
        self.primed = false;
        self.gap_since = None;
        self.unprimed_since = None;
        self.want_keyframe = false;
        self.playout = PlayoutClock::new();
        self.stats.current_depth = 0;
    }

    // ── Internals ───────────────────────────────────────────────────

    fn reorder_wait(&self) -> Duration {
        #[cfg(test)]
        if let Some(d) = self.reorder_wait_override {
            return d;
        }
        // Hold out ~2× the measured jitter: a packet reordered by one
        // "typical wobble" usually lands within that. The arrival-based
        // estimator over-reads on keyframe bursts (big frames take longer
        // to serialize), which errs toward waiting slightly longer during
        // loss — acceptable, and the clamp bounds the damage.
        let from_jitter = f64::from(self.jitter.jitter_ms()) * 2.0;
        let from_repairs = self.playout.repair_peak_ms() + REPAIR_MARGIN_MS;
        Duration::from_secs_f64(from_jitter.max(from_repairs) / 1000.0)
            .clamp(REORDER_WAIT_MIN, REORDER_WAIT_MAX)
    }

    /// Current playout delay: how far behind the fastest observed path
    /// frames are scheduled (see module docs).
    fn playout_delay(&self) -> Duration {
        #[cfg(test)]
        if let Some(d) = self.playout_delay_override {
            return d;
        }
        Duration::from_secs_f64((self.playout.late_peak_ms() + PLAYOUT_MARGIN_MS) / 1000.0)
            .clamp(PLAYOUT_DELAY_MIN, PLAYOUT_DELAY_MAX)
    }

    /// Local time at which `packet` should be handed to the decoder.
    fn playout_due(&self, packet: &BufferedPacket) -> Instant {
        self.playout
            .local_time_of(packet.timestamp_ms)
            .map_or(packet.arrival_time, |t| t + self.playout_delay())
    }

    /// The backlog cap: [`BACKLOG_CAP_SECS`] at the learned frame rate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a frame count
    fn max_depth(&self) -> usize {
        let fps = 1000.0 / self.jitter.expected_interval_ms.max(1.0);
        ((fps * BACKLOG_CAP_SECS).round() as usize).max(8)
    }

    /// The newest buffered keyframe after `after` (wraparound-aware).
    fn newest_keyframe_after(&self, after: u32) -> Option<u32> {
        self.pending
            .iter()
            .filter(|(&s, p)| p.is_keyframe && is_seq_after(s, after))
            .map(|(&s, _)| s)
            .max_by_key(|&s| s.wrapping_sub(after))
    }

    /// Catch up: drop everything before keyframe `kf` and go on from it.
    fn jump_to(&mut self, kf: u32) {
        let before = self.pending.len();
        self.pending.retain(|&s, _| s == kf || is_seq_after(s, kf));
        let dropped = before - self.pending.len();
        self.stats.packets_dropped_overflow += dropped as u64;
        self.stats.keyframe_resets += 1;
        self.stats.current_depth = self.pending.len();
        self.next_seq = Some(kf);
        self.gap_since = None;
        // Counted in the viewer's log (`dropped`, `jumps`).
        log::debug!("video over {CATCH_UP_BEHIND:?} behind live: jumped to keyframe seq {kf}, dropped {dropped}");
    }

    /// The buffered keyframe closest after `after` (wraparound-aware).
    fn first_keyframe_after(&self, after: u32) -> Option<u32> {
        self.pending
            .iter()
            .filter(|(&s, p)| p.is_keyframe && is_seq_after(s, after))
            .map(|(&s, _)| s)
            .min_by_key(|&s| s.wrapping_sub(after))
    }

    /// The buffer hit its size cap: the consumer stalled for seconds.
    /// Jump to the *freshest* buffered keyframe (dropping everything
    /// older — it's stale by definition), or clear entirely and request
    /// an IDR if no keyframe is buffered.
    fn recover_from_overflow(&mut self) {
        log::debug!(
            "jitter buffer: overflow at {} pending (consumer slower than stream) — recovering",
            self.pending.len()
        );
        let latest_kf = self.next_seq.and_then(|next| {
            self.pending
                .iter()
                .filter(|(&s, p)| p.is_keyframe && (s == next || is_seq_after(s, next)))
                .map(|(&s, _)| s)
                .max_by_key(|&s| s.wrapping_sub(next))
        });

        if let Some(kf) = latest_kf {
            let before = self.pending.len();
            self.pending.retain(|&s, _| s == kf || is_seq_after(s, kf));
            self.stats.packets_dropped_overflow += (before - self.pending.len()) as u64;
            self.stats.keyframe_resets += 1;
            self.next_seq = Some(kf);
            self.gap_since = None;
            // Counted in the viewer's log (`dropped`, `jumps`).
            log::debug!(
                "video fell {} frames behind (the decoder is slower than the stream): \
                 jumped to keyframe seq {kf}, dropped {} frame(s)",
                before,
                before - self.pending.len(),
            );
        } else {
            self.stats.packets_dropped_overflow += self.pending.len() as u64;
            self.pending.clear();
            self.gap_since = None;
            // Same freeze semantics as an unhealed gap: nothing
            // buffered is decodable without a keyframe, so drop to
            // unprimed and resume at the next IDR.
            self.primed = false;
            self.next_seq = None;
            self.unprimed_since = Some(Instant::now());
            self.want_keyframe = true;
            // The viewer's log names the freeze and counts the frames.
            log::debug!(
                "video fell {} frames behind with no keyframe to jump to: froze, asking for one",
                self.max_depth()
            );
        }
    }

    // ── Test hooks ──────────────────────────────────────────────────

    #[cfg(test)]
    fn set_reorder_wait(&mut self, d: Duration) {
        self.reorder_wait_override = Some(d);
    }

    #[cfg(test)]
    fn set_playout_delay(&mut self, d: Duration) {
        self.playout_delay_override = Some(d);
    }

    #[cfg(test)]
    fn set_unprimed_since(&mut self, t: Instant) {
        self.unprimed_since = Some(t);
    }
}

/// Wraparound-aware "is `a` after `b`" for u32 sequence numbers.
///
/// Uses the RTP convention: `a` is after `b` if the wrapped distance is in
/// the forward half of the sequence space.
fn is_seq_after(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

// ---------------------------------------------------------------------------
// Playout clock
// ---------------------------------------------------------------------------

/// Maps sender capture timestamps onto the local clock and tracks how
/// late frames arrive relative to the fastest path seen.
///
/// The mapping is `local = timestamp + offset`, where `offset` is the
/// smallest `arrival − timestamp` observed (the fastest path), creeping
/// later at [`OFFSET_CREEP_MS_PER_S`] so it follows clock drift. A
/// frame's *lateness* is how much its own `arrival − timestamp` exceeds
/// that; the decaying peak of lateness sizes the playout delay. Sender
/// timestamps are unwrapped into a running `f64` so the u32 wrap is
/// invisible here.
struct PlayoutClock {
    /// Local reference for the ms arithmetic: the first arrival seen,
    /// so no arrival can predate it.
    epoch: Option<Instant>,
    /// Last raw sender timestamp and its unwrapped value, ms.
    last_ts: Option<(u32, f64)>,
    /// Windowed minimum of `local_ms − sender_ms` (the fastest path).
    offset: MinWindow,
    /// When the peaks were last decayed.
    last_observed: Option<Instant>,
    late_peak: PeakHold,
    repair_peak: PeakHold,
}

impl PlayoutClock {
    fn new() -> Self {
        Self {
            epoch: None,
            last_ts: None,
            offset: MinWindow::default(),
            last_observed: None,
            late_peak: PeakHold::default(),
            repair_peak: PeakHold::default(),
        }
    }

    /// Milliseconds since the epoch, at microsecond resolution so a
    /// round trip through [`Self::instant_at`] reproduces the instant.
    #[allow(clippy::cast_precision_loss)] // µs since a process-local epoch: exact for centuries
    fn local_ms(epoch: Instant, at: Instant) -> f64 {
        at.saturating_duration_since(epoch).as_micros() as f64 / 1000.0
    }

    /// Inverse of [`Self::local_ms`].
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped ≥ 0, µs fits
    fn instant_at(epoch: Instant, local_ms: f64) -> Instant {
        epoch + Duration::from_micros((local_ms.max(0.0) * 1000.0).round() as u64)
    }

    /// Unwrap a sender timestamp against the last one seen.
    fn unwrap_ts(&mut self, ts: u32) -> f64 {
        let unwrapped = match self.last_ts {
            // Wrap-aware signed delta: a u32 timestamp that wrapped
            // still yields a small positive step.
            Some((last_raw, last_unwrapped)) => {
                #[allow(clippy::cast_possible_wrap)] // reinterpreting the wrapped delta as signed
                let delta = ts.wrapping_sub(last_raw) as i32;
                last_unwrapped + f64::from(delta)
            }
            None => f64::from(ts),
        };
        self.last_ts = Some((ts, unwrapped));
        unwrapped
    }

    /// Record one arrival.
    fn observe(&mut self, arrival: Instant, ts: u32) {
        let elapsed_s = self
            .last_observed
            .map_or(0.0, |t| arrival.saturating_duration_since(t).as_secs_f64());
        self.last_observed = Some(arrival);
        self.late_peak.decay(elapsed_s);
        self.repair_peak.decay(elapsed_s);

        let epoch = *self.epoch.get_or_insert(arrival);
        let sender_ms = self.unwrap_ts(ts);
        let offset = Self::local_ms(epoch, arrival) - sender_ms;
        if let Some(current) = self.offset.min() {
            if (offset - current).abs() > CLOCK_JUMP_MS {
                // Sender clock restarted: start over rather than
                // schedule against a dead reference.
                self.offset = MinWindow::default();
                self.late_peak = PeakHold::default();
            }
        }
        self.offset.observe(offset, arrival);
        let fastest = self.offset.min().unwrap_or(offset);
        self.late_peak.raise(offset - fastest);
    }

    /// Local instant the fastest path would have delivered a frame with
    /// this timestamp. `None` before the first observation.
    fn local_time_of(&self, ts: u32) -> Option<Instant> {
        let epoch = self.epoch?;
        let (last_raw, last_unwrapped) = self.last_ts?;
        let offset = self.offset.min()?;
        #[allow(clippy::cast_possible_wrap)] // see unwrap_ts
        let delta = ts.wrapping_sub(last_raw) as i32;
        let sender_ms = last_unwrapped + f64::from(delta);
        Some(Self::instant_at(epoch, sender_ms + offset))
    }

    fn note_repair(&mut self, took: Duration) {
        self.repair_peak.raise(took.as_secs_f64() * 1000.0);
    }

    fn late_peak_ms(&self) -> f64 {
        self.late_peak.value
    }

    fn repair_peak_ms(&self) -> f64 {
        self.repair_peak.value
    }
}

/// Minimum over a sliding window of [`OFFSET_BUCKETS`] × [`OFFSET_BUCKET`].
#[derive(Default)]
struct MinWindow {
    buckets: [Option<f64>; OFFSET_BUCKETS],
    /// Index of the bucket being filled and when it opened.
    current: usize,
    opened: Option<Instant>,
}

impl MinWindow {
    fn observe(&mut self, sample: f64, at: Instant) {
        match self.opened {
            None => self.opened = Some(at),
            Some(opened) => {
                // Advance one bucket per elapsed interval, clearing what
                // it overwrites; a long silence clears the whole window.
                let elapsed = at.saturating_duration_since(opened);
                let steps = (elapsed.as_secs_f64() / OFFSET_BUCKET.as_secs_f64()).floor();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // floor ≥ 0
                let steps = (steps as usize).min(OFFSET_BUCKETS);
                for _ in 0..steps {
                    self.current = (self.current + 1) % OFFSET_BUCKETS;
                    self.buckets[self.current] = None;
                }
                if steps > 0 {
                    self.opened = Some(at);
                }
            }
        }
        let slot = &mut self.buckets[self.current];
        *slot = Some(slot.map_or(sample, |v| v.min(sample)));
    }

    fn min(&self) -> Option<f64> {
        self.buckets.iter().flatten().copied().reduce(f64::min)
    }
}

/// A peak detector that forgets slowly: holds the largest sample seen,
/// decaying linearly at [`PEAK_DECAY_MS_PER_S`] while samples stay low.
#[derive(Default)]
struct PeakHold {
    value: f64,
}

impl PeakHold {
    fn decay(&mut self, elapsed_s: f64) {
        self.value = (self.value - PEAK_DECAY_MS_PER_S * elapsed_s).max(0.0);
    }

    fn raise(&mut self, sample_ms: f64) {
        self.value = self.value.max(sample_ms);
    }
}

// ---------------------------------------------------------------------------
// Arrival jitter estimation
// ---------------------------------------------------------------------------

/// EMA of the deviation between consecutive packet arrival gaps and the
/// nominal frame interval.
///
/// **Known bias:** keyframes are ~10× larger than delta frames, so their
/// serialization/chunking delay inflates arrival gaps once per keyframe
/// interval. That inflates this estimate. It is used only to size the
/// reorder window (clamped) and for UI stats — it does not gate delivery,
/// so the bias costs at most a slightly longer hold during actual loss.
struct JitterEstimate {
    expected_interval_ms: f32,
    jitter_ms: f32,
    last_arrival: Option<Instant>,
}

impl JitterEstimate {
    fn new(fps: u32) -> Self {
        Self {
            expected_interval_ms: 1000.0 / fps.max(1) as f32,
            // Seed at half a frame interval — a sane starting reorder
            // window before real measurements accumulate.
            jitter_ms: 1000.0 / fps.max(1) as f32 / 2.0,
            last_arrival: None,
        }
    }

    fn record_arrival(&mut self, at: Instant) {
        const ALPHA: f32 = 1.0 / 16.0; // RFC 3550 smoothing factor
        if let Some(last) = self.last_arrival {
            let gap_ms = at.duration_since(last).as_secs_f32() * 1000.0;

            // Learn the sender's actual frame cadence instead of trusting
            // the constructor's nominal fps: sharers pick 30 or 60, and
            // sender-side fps adaptation changes the true rate mid-stream,
            // so any static number is eventually wrong. Only plausible
            // frame-cadence gaps train the estimate — chunk-completion
            // bursts (~0 ms) and keyframe-assembly pauses (150 ms+) are
            // arrival artifacts, not cadence.
            if (8.0..=120.0).contains(&gap_ms) {
                const RATE_ALPHA: f32 = 1.0 / 32.0;
                self.expected_interval_ms += RATE_ALPHA * (gap_ms - self.expected_interval_ms);
            }

            // Clamp the deviation so a stream pause (alt-tab, network
            // hiccup measured in seconds) doesn't poison the estimate.
            let deviation = (gap_ms - self.expected_interval_ms).abs().min(250.0);
            self.jitter_ms += ALPHA * (deviation - self.jitter_ms);
        }
        self.last_arrival = Some(at);
    }

    fn jitter_ms(&self) -> f32 {
        self.jitter_ms
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32, is_keyframe: bool) -> BufferedPacket {
        pkt_at(seq, is_keyframe, Instant::now(), 0)
    }

    fn pkt_at(
        seq: u32,
        is_keyframe: bool,
        arrival_time: Instant,
        timestamp_ms: u32,
    ) -> BufferedPacket {
        BufferedPacket {
            seq,
            is_keyframe,
            width: 1280,
            height: 720,
            data: vec![0u8; 8],
            arrival_time,
            timestamp_ms,
        }
    }

    /// A buffer with playout pacing disabled, for tests about ordering
    /// and loss recovery where "available == released" is the premise.
    fn unpaced(fps: u32) -> VideoJitterBuffer {
        let mut buf = VideoJitterBuffer::new(fps);
        buf.set_playout_delay(Duration::ZERO);
        buf
    }

    #[test]
    fn unprimed_until_first_keyframe() {
        let mut buf = unpaced(30);
        buf.push(pkt(5, false));
        buf.push(pkt(6, false));
        assert!(!buf.is_primed());
        assert!(buf.pull_next().is_none());

        buf.push(pkt(7, true));
        assert!(buf.is_primed());
        // Deltas before the keyframe were discarded on priming.
        let p = buf.pull_next().expect("keyframe releases");
        assert_eq!(p.seq, 7);
        assert!(p.is_keyframe);
    }

    #[test]
    fn releases_immediately_and_in_order() {
        let mut buf = unpaced(30);
        buf.push(pkt(10, true));
        // No depth gating: available == released.
        assert_eq!(buf.pull_next().unwrap().seq, 10);
        assert!(buf.pull_next().is_none());

        // Out-of-order arrival still releases in order.
        buf.push(pkt(12, false));
        buf.push(pkt(11, false));
        assert_eq!(buf.pull_next().unwrap().seq, 11);
        assert_eq!(buf.pull_next().unwrap().seq, 12);
        assert!(buf.pull_next().is_none());
    }

    #[test]
    fn gap_waits_for_reorder_window() {
        let mut buf = unpaced(30);
        buf.set_reorder_wait(Duration::from_hours(1)); // effectively forever
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        buf.push(pkt(2, false)); // 1 is missing
                                 // Window hasn't expired — hold.
        assert!(buf.pull_next().is_none());
        assert!(buf.pull_next().is_none());

        // The missing packet arrives late-but-in-time: normal delivery.
        buf.push(pkt(1, false));
        assert_eq!(buf.pull_next().unwrap().seq, 1);
        assert_eq!(buf.pull_next().unwrap().seq, 2);
    }

    #[test]
    fn expired_gap_without_keyframe_freezes_and_requests() {
        let mut buf = unpaced(30);
        buf.set_reorder_wait(Duration::ZERO);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        buf.push(pkt(3, false)); // 1, 2 lost; no keyframe ahead
        buf.push(pkt(4, false));
        // Zero-length window: the pull that observes the gap also finds it
        // already expired. Freeze: nothing is delivered (decoding these
        // deltas would smear), the orphans are dropped, and an IDR is
        // requested.
        assert!(buf.pull_next().is_none());
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
        assert_eq!(buf.stats.packets_dropped_broken_reference, 2);
        assert!(buf.take_keyframe_request());
        // Rate-limited: not twice in a row.
        assert!(!buf.take_keyframe_request());
        assert_eq!(buf.stats.gaps_detected, 1);

        // More deltas from the broken GOP arrive: still frozen.
        buf.push(pkt(5, false));
        assert!(buf.pull_next().is_none());

        // The requested IDR arrives: delivery resumes cleanly from it.
        buf.push(pkt(6, true));
        let p = buf.pull_next().expect("resumes at the keyframe");
        assert_eq!(p.seq, 6);
        assert!(p.is_keyframe);
        buf.push(pkt(7, false));
        assert_eq!(buf.pull_next().unwrap().seq, 7);
    }

    #[test]
    fn expired_gap_prefers_buffered_keyframe() {
        let mut buf = unpaced(30);
        buf.set_reorder_wait(Duration::ZERO);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        buf.push(pkt(4, false));
        buf.push(pkt(6, true)); // fresh GOP starts at 6
        buf.push(pkt(7, false));

        let p = buf.pull_next().expect("jumps to keyframe");
        assert_eq!(p.seq, 6);
        assert!(p.is_keyframe);
        // The orphaned delta before the keyframe was discarded.
        assert_eq!(buf.pull_next().unwrap().seq, 7);
        // Clean keyframe restart → no request needed.
        assert!(!buf.take_keyframe_request());
        assert_eq!(buf.stats.keyframe_resets, 1);
    }

    #[test]
    fn late_packets_dropped() {
        let mut buf = unpaced(30);
        buf.push(pkt(10, true));
        assert_eq!(buf.pull_next().unwrap().seq, 10);
        buf.push(pkt(9, false)); // already superseded
        assert!(buf.pull_next().is_none());
        assert_eq!(buf.stats.packets_dropped_late, 1);
    }

    #[test]
    fn overflow_jumps_to_freshest_keyframe() {
        let mut buf = unpaced(3); // max_depth = 9
        buf.set_reorder_wait(Duration::from_hours(1));
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        // Stalled consumer: 1 never arrives, later packets pile up.
        for seq in [2, 3, 4, 5, 6] {
            buf.push(pkt(seq, false));
        }
        buf.push(pkt(7, true)); // a fresh GOP is in there
        for seq in [8, 9, 10, 11] {
            buf.push(pkt(seq, false)); // final push exceeds max_depth
        }

        assert!(buf.stats.packets_dropped_overflow > 0);
        // Recovery landed exactly on the keyframe.
        assert_eq!(buf.pull_next().unwrap().seq, 7);
        assert_eq!(buf.pull_next().unwrap().seq, 8);
    }

    #[test]
    fn overflow_without_keyframe_freezes_and_requests() {
        let mut buf = unpaced(3); // max_depth = 9
        buf.set_reorder_wait(Duration::from_hours(1));
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        for seq in 2..=11 {
            buf.push(pkt(seq, false)); // all deltas, overflow on the 10th
        }
        assert_eq!(buf.depth(), 0);
        assert!(!buf.is_primed());
        assert!(buf.take_keyframe_request());

        // Resumes at the next keyframe, exactly like stream start.
        buf.push(pkt(12, true));
        assert!(buf.is_primed());
        assert_eq!(buf.pull_next().unwrap().seq, 12);
    }

    #[test]
    fn unprimed_stream_requests_keyframe_after_grace() {
        let mut buf = unpaced(30);
        buf.push(pkt(100, false));
        assert!(!buf.take_keyframe_request()); // inside the grace period

        // Simulate the grace period having elapsed.
        buf.set_unprimed_since(
            Instant::now()
                .checked_sub(UNPRIMED_REQUEST_AFTER * 2)
                .unwrap(),
        );
        buf.push(pkt(101, false));
        assert!(buf.take_keyframe_request());
    }

    #[test]
    fn seq_wraparound() {
        assert!(is_seq_after(1, 0));
        assert!(is_seq_after(0, u32::MAX)); // wraps
        assert!(!is_seq_after(u32::MAX, 0));
        assert!(!is_seq_after(0, 0));

        let mut buf = unpaced(30);
        buf.push(pkt(u32::MAX, true));
        assert_eq!(buf.pull_next().unwrap().seq, u32::MAX);
        buf.push(pkt(0, false));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        assert_eq!(buf.pull_next().unwrap().seq, 1);
    }

    #[test]
    fn reset_clears_everything() {
        let mut buf = unpaced(30);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        buf.reset();
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
        assert!(buf.pull_next().is_none());
    }

    mod playout_pacing {
        use super::*;

        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }

        #[test]
        fn behind_live_is_the_lateness_against_the_fastest_path() {
            // Two frames 33 ms apart that arrived on the fastest path;
            // the second handled 400 ms after it could have been.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            assert_eq!(buf.behind_live(0, t0), None);
            buf.push(pkt_at(1, true, t0, 1_000));
            buf.push(pkt_at(2, false, t0 + ms(33), 1_033));
            assert_eq!(buf.behind_live(1_033, t0 + ms(33)), Some(Duration::ZERO));
            assert_eq!(buf.behind_live(1_033, t0 + ms(433)), Some(ms(400)));
        }

        #[test]
        fn a_frame_far_behind_live_skips_to_the_newest_keyframe() {
            // A 30 fps stream that arrived on time, handled 1.5 s late:
            // the backlog holds keyframes at 10 and 40.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            for seq in 1..=50u32 {
                let ts = (seq - 1) * 33;
                let kf = seq == 1 || seq == 10 || seq == 40;
                buf.push(pkt_at(seq, kf, t0 + ms(u64::from(ts)), ts));
            }
            let at = t0 + ms(1_500);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(40));
            assert_eq!(buf.stats.keyframe_resets, 1);
            assert_eq!(buf.stats.packets_dropped_overflow, 39);
        }

        #[test]
        fn a_frame_a_little_late_is_not_skipped() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            for seq in 1..=20u32 {
                let ts = (seq - 1) * 33;
                buf.push(pkt_at(
                    seq,
                    seq == 1 || seq == 10,
                    t0 + ms(u64::from(ts)),
                    ts,
                ));
            }
            assert_eq!(buf.pull_next_at(t0 + ms(500)).map(|p| p.seq), Some(1));
            assert_eq!(buf.stats.keyframe_resets, 0);
        }

        #[test]
        fn the_backlog_cap_follows_the_learned_frame_rate() {
            // Created for 30 fps, fed 60: the cap grows to 3 s of 60.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            assert_eq!(buf.max_depth(), 90);
            for seq in 1..=300u32 {
                let ts = (seq - 1) * 1000 / 60;
                buf.push(pkt_at(seq, seq == 1, t0 + ms(u64::from(ts)), ts));
                let _ = buf.pull_next_at(t0 + ms(u64::from(ts)) + PLAYOUT_DELAY_MAX);
            }
            assert!(buf.max_depth() >= 170, "{}", buf.max_depth());
        }

        #[test]
        fn a_frame_is_held_until_its_playout_slot() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            assert!(buf.pull_next_at(t0).is_none());
        }

        #[test]
        fn a_frame_is_released_once_the_minimum_delay_has_passed() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            let at = t0 + PLAYOUT_DELAY_MIN + ms(1);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(1));
        }

        /// Steady 60 fps for a while (establishes the fastest path), then
        /// a keyframe-sized stall: frames 5..8 all land together, 150 ms
        /// late. Returns the buffer and the bunch's arrival instant.
        fn after_a_keyframe_bunch() -> (VideoJitterBuffer, Instant) {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            for i in 0..4u32 {
                buf.push(pkt_at(i + 1, i == 0, t0 + ms(16 * u64::from(i)), 16 * i));
            }
            for _ in 0..4 {
                buf.pull_next_at(t0 + ms(100));
            }
            let bunch_at = t0 + ms(64) + ms(150);
            for i in 4..8u32 {
                buf.push(pkt_at(i + 1, false, bunch_at, 16 * i));
            }
            (buf, bunch_at)
        }

        #[test]
        fn first_frame_of_a_bunch_is_released_within_the_margin() {
            let (mut buf, bunch_at) = after_a_keyframe_bunch();
            let at = bunch_at + Duration::from_secs_f64(PLAYOUT_MARGIN_MS / 1000.0) + ms(1);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(5));
        }

        #[test]
        fn rest_of_a_bunch_is_paced_at_sender_cadence() {
            let (mut buf, bunch_at) = after_a_keyframe_bunch();
            let at = bunch_at + Duration::from_secs_f64(PLAYOUT_MARGIN_MS / 1000.0) + ms(1);
            buf.pull_next_at(at);
            assert!(
                buf.pull_next_at(at).is_none(),
                "seq 6 must wait its 16 ms slot"
            );
        }

        #[test]
        fn second_frame_of_a_bunch_lands_one_interval_later() {
            let (mut buf, bunch_at) = after_a_keyframe_bunch();
            let at = bunch_at + Duration::from_secs_f64(PLAYOUT_MARGIN_MS / 1000.0) + ms(1);
            buf.pull_next_at(at);
            assert_eq!(buf.pull_next_at(at + ms(16)).map(|p| p.seq), Some(6));
        }

        #[test]
        fn an_early_burst_does_not_anchor_the_clock_for_long() {
            // Encoder pipeline drain: seq 2 arrives together with seq 1
            // despite a 16 ms newer stamp. After the window rolls, a
            // steady frame is on schedule again rather than "late".
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.push(pkt_at(2, false, t0, 16));
            let later = t0 + OFFSET_BUCKET * (OFFSET_BUCKETS as u32 + 1);
            buf.push(pkt_at(3, false, later, 3000));
            // Exactly zero by construction: nothing was late.
            #[expect(clippy::float_cmp)]
            {
                assert_eq!(buf.playout.late_peak_ms(), 0.0);
            }
        }

        #[test]
        fn lateness_grows_the_playout_delay() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.push(pkt_at(2, false, t0 + ms(16) + ms(150), 16));
            assert!(
                buf.playout_delay_ms() >= 150.0,
                "delay {}",
                buf.playout_delay_ms()
            );
        }

        #[test]
        fn playout_delay_never_exceeds_the_cap() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.push(pkt_at(2, false, t0 + ms(16) + ms(900), 16));
            assert_eq!(buf.playout_delay(), PLAYOUT_DELAY_MAX);
        }

        #[test]
        fn a_sender_clock_restart_reanchors_instead_of_waiting() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 60_000));
            buf.pull_next_at(t0 + ms(30));
            // New share: timestamps start over from 0.
            buf.push(pkt_at(2, true, t0 + ms(100), 0));
            let at = t0 + ms(100) + PLAYOUT_DELAY_MIN + ms(1);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(2));
        }

        #[test]
        fn timestamps_unwrap_across_u32_max() {
            let mut clock = PlayoutClock::new();
            let before = clock.unwrap_ts(u32::MAX - 5);
            let after = clock.unwrap_ts(10);
            // Sixteen ticks apart across the wrap: exact, no rounding.
            #[expect(clippy::float_cmp)]
            {
                assert_eq!(after - before, 16.0);
            }
        }

        #[test]
        fn gap_recovery_waits_for_the_later_frames_playout_slot() {
            // Even with a zero reorder window, seq 2 is not given up on
            // before seq 3's own slot: recovering earlier shows nothing.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.set_reorder_wait(Duration::ZERO);
            buf.push(pkt_at(1, true, t0, 0));
            buf.pull_next_at(t0 + ms(30));
            buf.push(pkt_at(3, true, t0 + ms(32), 32));
            assert!(buf.pull_next_at(t0 + ms(33)).is_none());
        }

        #[test]
        fn gap_recovery_proceeds_once_the_later_frames_slot_has_passed() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.set_reorder_wait(Duration::ZERO);
            buf.push(pkt_at(1, true, t0, 0));
            buf.pull_next_at(t0 + ms(30));
            buf.push(pkt_at(3, true, t0 + ms(32), 32));
            let after_slot = t0 + ms(32) + PLAYOUT_DELAY_MIN + ms(1);
            assert_eq!(buf.pull_next_at(after_slot).map(|p| p.seq), Some(3));
        }

        #[test]
        fn a_repair_widens_the_reorder_window() {
            let t0 = Instant::now();
            let mut buf = unpaced(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.pull_next_at(t0);
            buf.push(pkt_at(3, false, t0 + ms(32), 32));
            // Gap at seq 2 noticed now; it arrives 180 ms later (resend).
            assert!(buf.pull_next_at(t0 + ms(32)).is_none());
            buf.push(pkt_at(2, false, t0 + ms(32) + ms(180), 16));
            buf.pull_next_at(t0 + ms(32) + ms(180));
            assert!(
                buf.adaptive_target_ms() >= 180.0,
                "window {}",
                buf.adaptive_target_ms()
            );
        }
    }
}
