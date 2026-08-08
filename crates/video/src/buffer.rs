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
//! There is deliberately **no depth target and no pacing** here: when the
//! next packet in sequence is present it is released immediately, so the
//! happy path adds zero latency. Time is only spent when a packet is
//! actually missing:
//!
//! - If `next_seq` is absent but later packets exist, delivery pauses for
//!   a short *reorder window* (derived from measured arrival jitter,
//!   clamped to 20–150 ms) to give a reordered packet time to arrive.
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

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long the buffer will wait, unprimed, watching undecodable delta
/// frames stream past before asking the sender for a keyframe. Covers
/// the "joined mid-GOP" case.
const UNPRIMED_REQUEST_AFTER: Duration = Duration::from_millis(400);

/// Minimum spacing between keyframe requests. Below the sender's
/// keyframe interval by design: a request, its IDR, and that IDR's loss
/// can all happen within one interval.
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_millis(700);

/// Bounds for the reorder window (how long to hold out for a missing
/// packet before recovering). Derived from measured jitter at runtime.
const REORDER_WAIT_MIN: Duration = Duration::from_millis(20);
const REORDER_WAIT_MAX: Duration = Duration::from_millis(150);

/// An encoded video packet waiting in the jitter buffer.
#[derive(Debug)]
pub struct BufferedPacket {
    pub seq: u32,
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub arrival_time: Instant,
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
    /// Note: BTreeMap ordering is numeric, not wraparound-aware, so all
    /// "which packet is next/first/latest" decisions use the wrapping
    /// comparisons below rather than map iteration order.
    pending: BTreeMap<u32, BufferedPacket>,
    /// Next sequence number to release. `None` until primed.
    next_seq: Option<u32>,
    /// Whether we've seen the first keyframe (nothing is decodable before it).
    primed: bool,
    /// Hard cap on buffered packets (~3 s at the nominal fps). Hitting it
    /// means the consumer stalled; recovery jumps to the freshest keyframe.
    max_depth: usize,
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
    /// Test hook: fixed reorder window override.
    #[cfg(test)]
    reorder_wait_override: Option<Duration>,
    pub stats: JitterBufferStats,
}

impl VideoJitterBuffer {
    /// `fps` seeds the jitter estimator and sizes the overflow cap; the
    /// estimator then learns the sender's real cadence from arrivals, so
    /// the seed only matters for the first second or so of a stream.
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        Self {
            pending: BTreeMap::new(),
            next_seq: None,
            primed: false,
            max_depth: (fps as usize * 3).max(8),
            gap_since: None,
            unprimed_since: None,
            want_keyframe: false,
            last_keyframe_request: None,
            jitter: JitterEstimate::new(fps),
            #[cfg(test)]
            reorder_wait_override: None,
            stats: JitterBufferStats::default(),
        }
    }

    /// Insert a packet that arrived from the network.
    pub fn push(&mut self, packet: BufferedPacket) {
        self.stats.packets_received += 1;
        self.jitter.record_arrival(packet.arrival_time);

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

        if self.pending.len() > self.max_depth {
            self.recover_from_overflow();
        }

        self.stats.current_depth = self.pending.len();
    }

    /// Release the next packet in sequence, if the buffer's rules allow.
    ///
    /// Call repeatedly until it returns `None`. Afterwards, poll
    /// [`Self::take_keyframe_request`] and forward it to the sender.
    pub fn pull_next(&mut self) -> Option<BufferedPacket> {
        if !self.primed {
            return None;
        }
        let next = self.next_seq?;

        // Happy path: next packet is here — release immediately.
        if let Some(packet) = self.pending.remove(&next) {
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
        // reorder window before declaring it lost.
        let now = Instant::now();
        let since = *self.gap_since.get_or_insert(now);
        if now.duration_since(since) < self.reorder_wait() {
            return None;
        }

        // Window expired: the packet is lost. Recover.
        self.gap_since = None;
        self.stats.gaps_detected += 1;

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
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    /// Buffered span in milliseconds: arrival-time spread between the
    /// oldest and newest pending packets.
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
    /// will stall on a missing packet before recovering. (This is the
    /// only latency this buffer ever adds, and only during loss.)
    pub fn adaptive_target_ms(&self) -> f32 {
        self.reorder_wait().as_secs_f32() * 1000.0
    }

    /// Smoothed arrival-jitter estimate in milliseconds.
    pub fn adaptive_jitter_ms(&self) -> f32 {
        self.jitter.jitter_ms()
    }

    /// Whether the first keyframe has been seen.
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Clear all state (e.g. when the peer restarts their share).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.next_seq = None;
        self.primed = false;
        self.gap_since = None;
        self.unprimed_since = None;
        self.want_keyframe = false;
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
        let wait = Duration::from_secs_f32(self.jitter.jitter_ms() * 2.0 / 1000.0);
        wait.clamp(REORDER_WAIT_MIN, REORDER_WAIT_MAX)
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
        let latest_kf = self.next_seq.and_then(|next| {
            self.pending
                .iter()
                .filter(|(&s, p)| p.is_keyframe && (s == next || is_seq_after(s, next)))
                .map(|(&s, _)| s)
                .max_by_key(|&s| s.wrapping_sub(next))
        });

        match latest_kf {
            Some(kf) => {
                let before = self.pending.len();
                self.pending.retain(|&s, _| s == kf || is_seq_after(s, kf));
                self.stats.packets_dropped_overflow += (before - self.pending.len()) as u64;
                self.stats.keyframe_resets += 1;
                self.next_seq = Some(kf);
                self.gap_since = None;
                log::warn!(
                    "jitter buffer overflow: jumped to keyframe seq {kf}, dropped {} packet(s)",
                    before - self.pending.len(),
                );
            }
            None => {
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
                log::warn!(
                    "jitter buffer overflow with no buffered keyframe: froze, requesting IDR"
                );
            }
        }
    }

    // ── Test hooks ──────────────────────────────────────────────────

    #[cfg(test)]
    fn set_reorder_wait(&mut self, d: Duration) {
        self.reorder_wait_override = Some(d);
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
            const ALPHA: f32 = 1.0 / 16.0; // RFC 3550 smoothing factor
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
        BufferedPacket {
            seq,
            is_keyframe,
            width: 1280,
            height: 720,
            data: vec![0u8; 8],
            arrival_time: Instant::now(),
        }
    }

    #[test]
    fn unprimed_until_first_keyframe() {
        let mut buf = VideoJitterBuffer::new(30);
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
        let mut buf = VideoJitterBuffer::new(30);
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
        let mut buf = VideoJitterBuffer::new(30);
        buf.set_reorder_wait(Duration::from_secs(3600)); // effectively forever
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
        let mut buf = VideoJitterBuffer::new(30);
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
        let mut buf = VideoJitterBuffer::new(30);
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
        let mut buf = VideoJitterBuffer::new(30);
        buf.push(pkt(10, true));
        assert_eq!(buf.pull_next().unwrap().seq, 10);
        buf.push(pkt(9, false)); // already superseded
        assert!(buf.pull_next().is_none());
        assert_eq!(buf.stats.packets_dropped_late, 1);
    }

    #[test]
    fn overflow_jumps_to_freshest_keyframe() {
        let mut buf = VideoJitterBuffer::new(3); // max_depth = 9
        buf.set_reorder_wait(Duration::from_secs(3600));
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
        let mut buf = VideoJitterBuffer::new(3); // max_depth = 9
        buf.set_reorder_wait(Duration::from_secs(3600));
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
        let mut buf = VideoJitterBuffer::new(30);
        buf.push(pkt(100, false));
        assert!(!buf.take_keyframe_request()); // inside the grace period

        // Simulate the grace period having elapsed.
        buf.set_unprimed_since(Instant::now() - UNPRIMED_REQUEST_AFTER * 2);
        buf.push(pkt(101, false));
        assert!(buf.take_keyframe_request());
    }

    #[test]
    fn seq_wraparound() {
        assert!(is_seq_after(1, 0));
        assert!(is_seq_after(0, u32::MAX)); // wraps
        assert!(!is_seq_after(u32::MAX, 0));
        assert!(!is_seq_after(0, 0));

        let mut buf = VideoJitterBuffer::new(30);
        buf.push(pkt(u32::MAX, true));
        assert_eq!(buf.pull_next().unwrap().seq, u32::MAX);
        buf.push(pkt(0, false));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        assert_eq!(buf.pull_next().unwrap().seq, 1);
    }

    #[test]
    fn reset_clears_everything() {
        let mut buf = VideoJitterBuffer::new(30);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        buf.reset();
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
        assert!(buf.pull_next().is_none());
    }
}
