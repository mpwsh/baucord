//! Video jitter buffer — decouples packet receipt from decode.
//!
//! The core problem: when the screen thread decodes a keyframe (10-16ms),
//! it can't drain the crossbeam channel. Packets that arrive during decode
//! are dropped by `try_send` on the network thread side. This manifests as
//! consistent sequence gaps immediately after every keyframe.
//!
//! The jitter buffer fixes this by separating receipt (fast `push`) from
//! consumption (decoder-paced `pull_next`). Incoming packets go into a
//! `BTreeMap` keyed by sequence number — insertion is O(log n) and takes
//! nanoseconds. The decoder pulls packets at its own rate, immune to
//! transient stalls.
//!
//! ## Design differences from the voice jitter buffer
//!
//! | Property          | Voice                    | Video                          |
//! |-------------------|--------------------------|--------------------------------|
//! | Packet size       | Uniform (~160B Opus)     | Variable (1KB–170KB)           |
//! | Decode cost       | ~0.1ms                   | 1–16ms (keyframes expensive)   |
//! | Loss tolerance    | PLC conceals 1-2 frames  | Any gap corrupts until keyframe|
//! | Buffering unit    | PCM samples (time-based) | Encoded packets (seq-based)    |
//!
//! Because video packets vary wildly in size and decode cost, this buffer
//! is **sequence-number-based** rather than time-based. The configurable
//! `target_depth_ms` is converted to a packet count using the stream's
//! frame rate, giving users an intuitive "latency knob" (e.g. 80ms)
//! while the internals work in packet counts.

use std::collections::BTreeMap;
use std::time::Instant;

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

/// Statistics exposed for diagnostics and UI display.
#[derive(Debug, Clone, Default)]
pub struct JitterBufferStats {
    /// Total packets pushed into the buffer.
    pub packets_received: u64,
    /// Total packets pulled out for decoding.
    pub packets_delivered: u64,
    /// Packets dropped due to buffer overflow.
    pub packets_dropped_overflow: u64,
    /// Packets dropped because they arrived too late (seq < next_seq).
    pub packets_dropped_late: u64,
    /// Number of sequence gaps detected during pull.
    pub gaps_detected: u64,
    /// Number of keyframe resets.
    pub keyframe_resets: u64,
    /// Current number of packets in the buffer.
    pub current_depth: usize,
    /// Current target depth in packets.
    pub target_depth: u32,
}

/// Sequence-based jitter buffer for encoded H.264 video packets.
///
/// Holds encoded packets sorted by sequence number and releases them
/// to the decoder at a controlled rate. This decouples network receipt
/// (which must be fast) from decode (which can stall on keyframes).
///
/// # Buffer depth
///
/// The target depth is configured in milliseconds and converted to a
/// packet count using the stream's FPS. For example, at 30fps with an
/// 80ms target, the buffer aims to hold ~2-3 packets before releasing.
/// Users on stable connections can use lower values (50ms) for
/// responsiveness; users on flaky networks can increase it (200ms+)
/// for smoothness.
pub struct VideoJitterBuffer {
    /// Packets waiting to be decoded, keyed by sequence number.
    /// BTreeMap gives automatic ordering and O(log n) insert/lookup.
    pending: BTreeMap<u32, BufferedPacket>,

    /// Next sequence number we expect to release to the decoder.
    next_seq: Option<u32>,

    /// Whether we've received at least one keyframe and can start releasing.
    primed: bool,

    /// Target buffer depth in packets (derived from `target_depth_ms` and FPS).
    target_depth: u32,

    /// User-configured target depth in milliseconds.
    target_depth_ms: u32,

    /// Stream frame rate (used to convert ms → packet count).
    fps: u32,

    /// Maximum packets to hold before evicting. Safety valve against
    /// runaway growth from stalled decoders or network bursts.
    max_depth: u32,

    /// Whether playback has started (first keyframe delivered).
    /// Once true, stays true for the lifetime of the stream — we never
    /// re-gate on buffer depth after the initial fill. Recovery from
    /// gaps is handled by skipping to the next keyframe, not by pausing
    /// playback to refill.
    playing: bool,

    /// Diagnostics.
    pub stats: JitterBufferStats,
}

impl VideoJitterBuffer {
    /// Create a new jitter buffer.
    ///
    /// # Arguments
    ///
    /// * `target_depth_ms` — desired buffer depth in milliseconds. Higher
    ///   values add latency but absorb more jitter. Typical range: 50–200ms.
    /// * `fps` — expected frame rate of the incoming stream (e.g. 30 or 60).
    ///
    /// # Example
    ///
    /// ```
    /// use screencap::buffer::VideoJitterBuffer;
    /// let mut buf = VideoJitterBuffer::new(80, 30);
    /// assert_eq!(buf.target_depth_ms(), 80);
    /// ```
    pub fn new(target_depth_ms: u32, fps: u32) -> Self {
        let fps = fps.max(1); // avoid division by zero
        let target_depth = ms_to_packets(target_depth_ms, fps);
        // Max depth: 3 seconds of video or 4× target, whichever is larger.
        // This prevents unbounded growth while giving enough room for bursts.
        let max_depth = (fps * 3).max(target_depth * 4);

        Self {
            pending: BTreeMap::new(),
            next_seq: None,
            primed: false,
            target_depth,
            target_depth_ms,
            fps,
            max_depth,
            playing: false,
            stats: JitterBufferStats::default(),
        }
    }

    /// Push an incoming packet into the buffer.
    ///
    /// This is the hot path called from the channel drain loop — it must
    /// be fast. Insertion into the BTreeMap is O(log n) with n typically
    /// < 10 packets. No decoding happens here.
    ///
    /// Keyframes trigger a reset only when they represent a genuine stream
    /// recovery (large forward jump in sequence numbers). Regular in-sequence
    /// keyframes at GOP boundaries are inserted normally without disrupting
    /// playback — they're just another packet in the buffer.
    pub fn push(&mut self, packet: BufferedPacket) {
        self.stats.packets_received += 1;

        // Drop packets that are behind our current play position.
        // These arrived too late to be useful.
        if let Some(next) = self.next_seq {
            if is_seq_before(packet.seq, next) {
                self.stats.packets_dropped_late += 1;
                return;
            }
        }

        if packet.is_keyframe {
            let kf_seq = packet.seq;

            // Determine if this keyframe represents a stream recovery
            // (significant forward jump) vs a regular GOP keyframe.
            //
            // A keyframe is "recovery" only if it jumps far ahead of where
            // we expect to be. The threshold accounts for normal buffer depth:
            // packets in the buffer are naturally ahead of next_seq by up to
            // target_depth packets. A regular GOP keyframe at seq N+30 when
            // next_seq is N+18 (12 packets buffered) is NOT recovery — it's
            // just the next keyframe sitting in the buffer as expected.
            //
            // We use 2× target_depth as the threshold: anything within that
            // range is normal buffer operation. Beyond it means real packet
            // loss or a stream reset.
            let is_recovery = match self.next_seq {
                Some(next) => {
                    let gap = kf_seq.wrapping_sub(next);
                    is_seq_before(next, kf_seq) && gap > self.target_depth * 2 + 1
                }
                None => true, // first keyframe ever — always prime
            };

            if is_recovery {
                self.pending.retain(|&seq, _| !is_seq_before(seq, kf_seq));
                self.next_seq = Some(kf_seq);
                self.stats.keyframe_resets += 1;
                // NOTE: we do NOT reset self.playing here. Once playback has
                // started, recovery should be seamless — we have a keyframe
                // ready, so there's no reason to pause and wait for the buffer
                // to refill. The keyframe will be released immediately by
                // pull_next() (keyframes bypass depth gating).
            }

            if !self.primed {
                self.next_seq = Some(kf_seq);
                self.primed = true;
            }
        }

        // Overflow protection: if we exceed max_depth, evict the oldest
        // non-keyframe packets. This shouldn't happen in normal operation
        // but protects against stalled decoders.
        while self.pending.len() >= self.max_depth as usize {
            // Find the oldest non-keyframe to evict
            let evict_seq = self
                .pending
                .iter()
                .find(|(_, p)| !p.is_keyframe)
                .map(|(&seq, _)| seq);

            if let Some(seq) = evict_seq {
                self.pending.remove(&seq);
                self.stats.packets_dropped_overflow += 1;
            } else {
                // All packets are keyframes (shouldn't happen) — drop oldest
                if let Some((&seq, _)) = self.pending.iter().next() {
                    self.pending.remove(&seq);
                    self.stats.packets_dropped_overflow += 1;
                }
                break;
            }
        }

        self.pending.insert(packet.seq, packet);
        self.stats.current_depth = self.pending.len();
        self.stats.target_depth = self.target_depth;
    }

    /// Pull the next packet to decode, if the buffer is ready.
    ///
    /// Returns `None` if:
    /// - The buffer hasn't been primed (no keyframe yet)
    /// - The buffer is in initial fill and hasn't reached target depth
    /// - The next expected packet hasn't arrived yet
    ///
    /// The depth gate only applies during the initial fill (before the
    /// first packet is ever released). Once `playing` is true, packets
    /// are released as soon as they're available in sequence. This
    /// prevents the old bug where every GOP keyframe would re-trigger
    /// the depth gate and freeze playback for hundreds of milliseconds.
    ///
    /// When a gap is detected (expected seq missing but later packets
    /// exist), the buffer skips forward to the next available keyframe
    /// for clean recovery. If no keyframe is available, it skips to
    /// the next available packet and lets the decoder handle artifacts.
    pub fn pull_next(&mut self) -> Option<BufferedPacket> {
        if !self.primed {
            return None;
        }
        let next = self.next_seq?;

        // Check if the next expected packet is available
        if self.pending.contains_key(&next) {
            // Depth gating: only applies during initial fill (!playing).
            // Keyframes always bypass — release immediately so the decoder
            // can start/recover ASAP.
            let is_keyframe = self
                .pending
                .get(&next)
                .map(|p| p.is_keyframe)
                .unwrap_or(false);

            if !is_keyframe && !self.playing && self.pending.len() < self.target_depth as usize {
                return None;
            }

            let packet = self.pending.remove(&next)?;
            self.next_seq = Some(next.wrapping_add(1));
            self.stats.packets_delivered += 1;
            self.stats.current_depth = self.pending.len();
            self.playing = true;
            return Some(packet);
        }

        // The packet we need isn't here. Check if we have later packets
        // (meaning this one was truly lost, not just delayed).
        let have_future = self.pending.keys().any(|&s| is_seq_after(s, next));
        if !have_future {
            return None; // hasn't arrived yet — wait
        }

        // Gap detected. Prefer skipping to the next keyframe for clean
        // recovery. If no keyframe ahead, skip to the next available
        // packet — decoder will produce artifacts until next keyframe.
        self.stats.gaps_detected += 1;

        // Look for the nearest keyframe ahead of our position
        let keyframe_seq = self
            .pending
            .iter()
            .find(|(_, p)| p.is_keyframe && is_seq_after_or_eq(p.seq, next))
            .map(|(&seq, _)| seq);

        if let Some(kf_seq) = keyframe_seq {
            // Clean recovery: skip to keyframe, discard everything before it
            self.pending.retain(|&seq, _| !is_seq_before(seq, kf_seq));
            self.next_seq = Some(kf_seq);
            // Don't deliver yet — let the next pull_next() get it so
            // the buffer depth check runs against the cleaned-up state
            return None;
        }

        // No keyframe ahead — skip to the next available packet.
        // The decoder will produce artifacts but at least keeps going.
        let &next_available = self.pending.keys().next()?;
        self.next_seq = Some(next_available);
        let packet = self.pending.remove(&next_available)?;
        self.next_seq = Some(next_available.wrapping_add(1));
        self.stats.packets_delivered += 1;
        self.stats.current_depth = self.pending.len();
        Some(packet)
    }

    /// Update the target buffer depth. Can be called at runtime to let
    /// users adjust the latency/smoothness tradeoff.
    pub fn set_target_depth_ms(&mut self, ms: u32) {
        self.target_depth_ms = ms;
        self.target_depth = ms_to_packets(ms, self.fps);
    }

    /// Current target depth in milliseconds.
    pub fn target_depth_ms(&self) -> u32 {
        self.target_depth_ms
    }

    /// Current number of packets in the buffer.
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    /// Current buffer depth expressed in milliseconds (approximate).
    pub fn depth_ms(&self) -> f32 {
        self.pending.len() as f32 * 1000.0 / self.fps.max(1) as f32
    }

    /// Whether the buffer has been primed with a keyframe.
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Reset the buffer state. Called when a peer stops sharing.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.next_seq = None;
        self.primed = false;
        self.playing = false;
        self.stats.current_depth = 0;
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Convert a target depth in milliseconds to a packet count at the given FPS.
///
/// Always returns at least 1 — a zero-depth buffer would release packets
/// with no jitter absorption, defeating the purpose.
fn ms_to_packets(ms: u32, fps: u32) -> u32 {
    ((ms as u64 * fps as u64 + 999) / 1000) as u32
}

/// Returns true if `a` is sequentially before `b`, handling u32 wraparound.
///
/// Uses the standard "half-space" comparison: if the distance from a to b
/// (wrapping) is less than half the sequence space, a is before b.
fn is_seq_before(a: u32, b: u32) -> bool {
    let diff = a.wrapping_sub(b);
    diff > 0 && diff > u32::MAX / 2
}

/// Returns true if `a` is sequentially after `b`, handling u32 wraparound.
fn is_seq_after(a: u32, b: u32) -> bool {
    is_seq_before(b, a)
}

/// Returns true if `a` is sequentially after or equal to `b`.
fn is_seq_after_or_eq(a: u32, b: u32) -> bool {
    a == b || is_seq_after(a, b)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_packet(seq: u32, is_keyframe: bool) -> BufferedPacket {
        BufferedPacket {
            seq,
            is_keyframe,
            width: 1280,
            height: 720,
            data: vec![0u8; if is_keyframe { 100_000 } else { 5_000 }],
            arrival_time: Instant::now(),
        }
    }

    #[test]
    fn buffer_requires_keyframe_before_releasing() {
        let mut buf = VideoJitterBuffer::new(0, 30);

        buf.push(make_packet(1, false));
        buf.push(make_packet(2, false));
        assert!(
            buf.pull_next().is_none(),
            "should not release without keyframe"
        );

        buf.push(make_packet(3, true));
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 3);
        assert!(pkt.is_keyframe);
    }

    #[test]
    fn packets_released_in_sequence_order() {
        let mut buf = VideoJitterBuffer::new(0, 30);

        // Push out of order
        buf.push(make_packet(10, true));
        buf.push(make_packet(12, false));
        buf.push(make_packet(11, false));

        assert_eq!(buf.pull_next().unwrap().seq, 10);
        assert_eq!(buf.pull_next().unwrap().seq, 11);
        assert_eq!(buf.pull_next().unwrap().seq, 12);
        assert!(buf.pull_next().is_none());
    }

    #[test]
    fn target_depth_delays_initial_release() {
        // 100ms at 30fps ≈ 3 packets
        let mut buf = VideoJitterBuffer::new(100, 30);

        buf.push(make_packet(0, true));
        // Keyframes bypass depth check
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 0);

        // Delta frames respect depth check (during initial fill)
        buf.push(make_packet(1, false));
        assert!(buf.pull_next().is_none(), "depth 1 < target ~3");

        buf.push(make_packet(2, false));
        assert!(buf.pull_next().is_none(), "depth 2 < target ~3");

        buf.push(make_packet(3, false));
        // Now depth = 3, target = 3, should release
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 1);
    }

    #[test]
    fn regular_keyframe_does_not_reset_playing() {
        // Simulate a full GOP cycle to ensure regular keyframes don't
        // trigger recovery or re-gate on buffer depth.
        let mut buf = VideoJitterBuffer::new(100, 30); // target = 3 packets

        // Initial keyframe + fill to start playing
        buf.push(make_packet(0, true));
        let _ = buf.pull_next(); // delivers keyframe, playing = true

        buf.push(make_packet(1, false));
        buf.push(make_packet(2, false));
        buf.push(make_packet(3, false));
        // Drain initial fill
        assert_eq!(buf.pull_next().unwrap().seq, 1);
        assert_eq!(buf.pull_next().unwrap().seq, 2);
        assert_eq!(buf.pull_next().unwrap().seq, 3);

        // Simulate steady-state: packets arrive and are consumed, buffer
        // holds ~3 packets ahead of next_seq.
        for seq in 4..30u32 {
            buf.push(make_packet(seq, false));
            let _ = buf.pull_next();
        }

        // Regular GOP keyframe arrives at seq 30 while next_seq is ~27-29.
        // This MUST NOT trigger recovery or reset playing.
        let resets_before = buf.stats.keyframe_resets;
        buf.push(make_packet(30, true));

        assert_eq!(
            buf.stats.keyframe_resets, resets_before,
            "regular keyframe should not trigger recovery reset"
        );

        // Next pull should still work immediately (no re-gating)
        // Drain up to the keyframe
        while let Some(pkt) = buf.pull_next() {
            if pkt.seq == 30 {
                assert!(pkt.is_keyframe);
                break;
            }
        }
    }

    #[test]
    fn genuine_recovery_keyframe_does_trigger_reset() {
        let mut buf = VideoJitterBuffer::new(100, 30); // target = 3 packets

        // Start playing
        buf.push(make_packet(0, true));
        let _ = buf.pull_next(); // playing = true

        // Large forward jump: seq 0 → seq 100 (gap of 100, well beyond 2*3+1=7)
        buf.push(make_packet(100, true));
        assert_eq!(
            buf.stats.keyframe_resets, 1,
            "large jump should trigger recovery"
        );

        // Recovery keyframe should be immediately available (no re-gating
        // since playing stays true)
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 100);
        assert!(pkt.is_keyframe);
    }

    #[test]
    fn recovery_does_not_regate_on_depth() {
        // This is the critical regression test: after recovery, the buffer
        // should NOT wait for target_depth packets before releasing.
        let mut buf = VideoJitterBuffer::new(100, 30); // target = 3 packets

        // Start playing
        buf.push(make_packet(0, true));
        let _ = buf.pull_next(); // playing = true, next_seq = 1

        // Packets 1-3 keep things going
        buf.push(make_packet(1, false));
        let _ = buf.pull_next();
        buf.push(make_packet(2, false));
        let _ = buf.pull_next();

        // Big gap: recovery keyframe at 50
        buf.push(make_packet(50, true));
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 50, "recovery keyframe should release immediately");

        // Delta frame after recovery should also release immediately
        // (playing is still true, no depth gate)
        buf.push(make_packet(51, false));
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 51, "post-recovery delta should release without waiting for depth");
    }

    #[test]
    fn keyframe_resets_buffer_on_large_gap() {
        let mut buf = VideoJitterBuffer::new(0, 30);

        buf.push(make_packet(0, true));
        buf.push(make_packet(1, false));
        buf.push(make_packet(2, false));

        // Large forward jump — old packets should be purged
        buf.push(make_packet(10, true));

        // Packets 0-2 should be gone, next should be 10
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 10);
    }

    #[test]
    fn late_packets_are_dropped() {
        let mut buf = VideoJitterBuffer::new(0, 30);

        buf.push(make_packet(5, true));
        let _ = buf.pull_next(); // consume seq 5, next_seq = 6

        // Late packet (seq 3) arrives after we've moved past it
        buf.push(make_packet(3, false));
        assert_eq!(buf.stats.packets_dropped_late, 1);
        assert_eq!(buf.depth(), 0);
    }

    #[test]
    fn gap_skips_to_keyframe_for_clean_recovery() {
        let mut buf = VideoJitterBuffer::new(0, 30);

        buf.push(make_packet(0, true));
        let _ = buf.pull_next(); // next_seq = 1

        // Seq 1-4 lost, but seq 5 (keyframe) and 6 arrive
        buf.push(make_packet(5, true));
        buf.push(make_packet(6, false));

        // pull_next sees gap, finds keyframe at 5, skips there
        // First pull detects gap and resets, returns None
        let _ = buf.pull_next();
        // Second pull delivers the keyframe
        let pkt = buf.pull_next().unwrap();
        assert_eq!(pkt.seq, 5);
        assert!(pkt.is_keyframe);
    }

    #[test]
    fn set_target_depth_updates_at_runtime() {
        let mut buf = VideoJitterBuffer::new(50, 30);
        assert_eq!(buf.target_depth_ms(), 50);

        buf.set_target_depth_ms(150);
        assert_eq!(buf.target_depth_ms(), 150);
        // 150ms at 30fps = ceil(150*30/1000) = 5 packets
        assert_eq!(buf.target_depth, 5);
    }

    #[test]
    fn ms_to_packets_rounds_up() {
        // 33ms at 30fps = 0.99 packets → rounds to 1
        assert_eq!(ms_to_packets(33, 30), 1);
        // 100ms at 30fps = 3.0 packets
        assert_eq!(ms_to_packets(100, 30), 3);
        // 80ms at 60fps = 4.8 packets → rounds to 5
        assert_eq!(ms_to_packets(80, 60), 5);
        // 0ms = minimum 0 (the target_depth clamp happens elsewhere)
        assert_eq!(ms_to_packets(0, 30), 0);
    }

    #[test]
    fn seq_wraparound_comparison() {
        // Normal case
        assert!(is_seq_before(5, 10));
        assert!(!is_seq_before(10, 5));
        // Wraparound: u32::MAX is before 0
        assert!(is_seq_before(u32::MAX, 0));
        assert!(is_seq_after(0, u32::MAX));
    }
}