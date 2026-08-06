//! Video wire format (version 2): frame chunking and reassembly.
//!
//! Encoded H.264 frames exceed a single safe datagram, so frames are
//! split into chunks of at most [`VIDEO_CHUNK_PAYLOAD`] bytes. Chunk
//! layout (all integers little-endian):
//!
//! ```text
//! offset  size  field
//! 0       1     version      (= 2)
//! 1       1     flags        (bit 0: keyframe)
//! 2       1     chunk index
//! 3       1     chunk count
//! 4       4     frame sequence number
//! 8       2     frame width
//! 10      2     frame height
//! 12      —     payload (≤ VIDEO_CHUNK_PAYLOAD bytes)
//! ```
//!
//! Framing invariant: every chunk except the last carries **exactly**
//! [`VIDEO_CHUNK_PAYLOAD`] bytes; the last carries the remainder
//! (1..=[`VIDEO_CHUNK_PAYLOAD`]). Parsers reject anything else, which
//! lets the reassembler write chunks straight into one pre-sized buffer
//! at `idx * VIDEO_CHUNK_PAYLOAD` — no per-chunk allocations, no
//! concatenation pass (the v1 reassembler allocated a `Vec` per chunk
//! plus a final copy; at 30 fps that was ~10k transient allocations/s
//! per stream).
//!
//! Sender identity comes from the relay envelope ([`crate::relayed`]),
//! not the chunk header; the reassembler keys pending frames by
//! `(sender, seq)` so interleaved streams can't collide.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use renet::Bytes;

/// Current video wire format version.
pub const VIDEO_PROTOCOL_VERSION: u8 = 2;

/// Serialized chunk header size in bytes.
pub const VIDEO_HEADER_SIZE: usize = 12;

/// Maximum payload bytes per chunk. Header + payload + relay envelope
/// (8) totals 1100 bytes — comfortably inside a 1200-byte conservative
/// MTU with renet/netcode overhead on top.
pub const VIDEO_CHUNK_PAYLOAD: usize = 1080;

/// Maximum encoded frame size (chunk count is a `u8`).
pub const MAX_VIDEO_FRAME_BYTES: usize = 255 * VIDEO_CHUNK_PAYLOAD;

/// Sanity bounds for frame dimensions, enforced on parse.
pub const MIN_VIDEO_DIMENSION: u16 = 16;
/// See [`MIN_VIDEO_DIMENSION`].
pub const MAX_VIDEO_DIMENSION: u16 = 4096;

const FLAG_KEYFRAME: u8 = 0b0000_0001;

/// Discard incomplete frames older than this. At 30 fps a frame missing
/// chunks for 500 ms is 15 frames stale — unrecoverable, waiting for a
/// keyframe anyway.
const FRAME_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum in-flight partial frames across all senders. Bounds hostile
/// or badly-reordered input; normal operation holds 1–2 per stream.
const MAX_PENDING_FRAMES: usize = 16;

/// Parsed chunk header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    pub version: u8,
    pub flags: u8,
    pub chunk_idx: u8,
    pub chunk_count: u8,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
}

impl VideoHeader {
    #[must_use]
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
}

/// A fully reassembled video frame.
#[derive(Debug, Clone)]
pub struct ReassembledFrame {
    /// Authenticated sender (from the relay envelope).
    pub sender_id: u64,
    pub seq: u32,
    pub is_keyframe: bool,
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

/// Split an encoded frame into wire chunks.
///
/// All chunks share one backing allocation; the returned [`Bytes`] are
/// zero-copy slices of it, ready to hand to renet.
///
/// Returns `None` for an empty frame or one larger than
/// [`MAX_VIDEO_FRAME_BYTES`] — the caller should drop the frame (and,
/// for a keyframe, request a fresh one) rather than send a torn frame.
#[must_use]
pub fn pack_video_frame(
    seq: u32,
    is_keyframe: bool,
    width: u16,
    height: u16,
    data: &[u8],
) -> Option<Vec<Bytes>> {
    if data.is_empty() || data.len() > MAX_VIDEO_FRAME_BYTES {
        return None;
    }
    let chunk_count = data.len().div_ceil(VIDEO_CHUNK_PAYLOAD);
    let flags = if is_keyframe { FLAG_KEYFRAME } else { 0 };

    let mut buf = Vec::with_capacity(data.len() + chunk_count * VIDEO_HEADER_SIZE);
    let mut boundaries = Vec::with_capacity(chunk_count);

    for (idx, payload) in data.chunks(VIDEO_CHUNK_PAYLOAD).enumerate() {
        let start = buf.len();
        buf.push(VIDEO_PROTOCOL_VERSION);
        buf.push(flags);
        // chunk_count ≤ 255 is guaranteed by the MAX_VIDEO_FRAME_BYTES
        // bound above, so these casts cannot truncate.
        buf.push(idx as u8);
        buf.push(chunk_count as u8);
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.extend_from_slice(payload);
        boundaries.push((start, buf.len()));
    }

    let backing = Bytes::from(buf);
    Some(
        boundaries
            .into_iter()
            .map(|(start, end)| backing.slice(start..end))
            .collect(),
    )
}

/// Parse a chunk header from untrusted bytes, returning the header and
/// the payload slice. Enforces the framing invariant and dimension
/// sanity bounds.
#[must_use]
pub fn parse_video_header(data: &[u8]) -> Option<(VideoHeader, &[u8])> {
    if data.len() <= VIDEO_HEADER_SIZE {
        return None;
    }
    if data[0] != VIDEO_PROTOCOL_VERSION {
        return None;
    }
    let header = VideoHeader {
        version: data[0],
        flags: data[1],
        chunk_idx: data[2],
        chunk_count: data[3],
        seq: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        width: u16::from_le_bytes([data[8], data[9]]),
        height: u16::from_le_bytes([data[10], data[11]]),
    };
    if header.chunk_count == 0 || header.chunk_idx >= header.chunk_count {
        return None;
    }
    if header.width < MIN_VIDEO_DIMENSION
        || header.width > MAX_VIDEO_DIMENSION
        || header.height < MIN_VIDEO_DIMENSION
        || header.height > MAX_VIDEO_DIMENSION
    {
        return None;
    }
    let payload = &data[VIDEO_HEADER_SIZE..];
    let is_last = header.chunk_idx == header.chunk_count - 1;
    let valid_len = if is_last {
        (1..=VIDEO_CHUNK_PAYLOAD).contains(&payload.len())
    } else {
        payload.len() == VIDEO_CHUNK_PAYLOAD
    };
    if !valid_len {
        return None;
    }
    Some((header, payload))
}

// ── Reassembly ──────────────────────────────────────────────────────────

struct PendingFrame {
    chunk_count: u8,
    received: u8,
    /// Bit `i` set ⇒ chunk `i` received. 4 × 64 covers the u8 range.
    bitmap: [u64; 4],
    is_keyframe: bool,
    width: u16,
    height: u16,
    /// Single pre-sized buffer; chunk `i` lands at `i * VIDEO_CHUNK_PAYLOAD`.
    data: Vec<u8>,
    /// Payload length of the final chunk, recorded when it arrives.
    last_len: usize,
    first_seen: Instant,
}

impl PendingFrame {
    fn mark(&mut self, idx: u8) -> bool {
        let word = (idx / 64) as usize;
        let bit = 1u64 << (idx % 64);
        if self.bitmap[word] & bit != 0 {
            return false; // duplicate
        }
        self.bitmap[word] |= bit;
        self.received += 1;
        true
    }
}

/// Reassembly statistics (diagnostics only).
#[derive(Debug, Default, Clone, Copy)]
pub struct ReassemblyStats {
    pub chunks_received: u64,
    pub chunks_rejected: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
}

/// Reassembles video frames from chunks, across multiple senders.
pub struct VideoReassembler {
    pending: HashMap<(u64, u32), PendingFrame>,
    pub stats: ReassemblyStats,
}

impl Default for VideoReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoReassembler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            stats: ReassemblyStats::default(),
        }
    }

    /// Feed one chunk (relay envelope already stripped). Returns a frame
    /// when its final missing chunk arrives.
    pub fn push_chunk(&mut self, sender_id: u64, data: &[u8]) -> Option<ReassembledFrame> {
        let Some((header, payload)) = parse_video_header(data) else {
            self.stats.chunks_rejected += 1;
            return None;
        };
        self.stats.chunks_received += 1;

        self.evict_stale();

        let key = (sender_id, header.seq);
        if !self.pending.contains_key(&key) {
            if self.pending.len() >= MAX_PENDING_FRAMES {
                self.evict_oldest();
            }
            self.pending.insert(
                key,
                PendingFrame {
                    chunk_count: header.chunk_count,
                    received: 0,
                    bitmap: [0; 4],
                    is_keyframe: header.is_keyframe(),
                    width: header.width,
                    height: header.height,
                    data: vec![0; header.chunk_count as usize * VIDEO_CHUNK_PAYLOAD],
                    last_len: 0,
                    first_seen: Instant::now(),
                },
            );
        }

        // Borrow scope: mutate the entry, deciding completeness.
        let complete = {
            let Some(frame) = self.pending.get_mut(&key) else {
                return None; // unreachable: inserted above
            };
            // A chunk contradicting the frame's parameters is hostile or
            // corrupt; reject the chunk, keep the frame.
            if frame.chunk_count != header.chunk_count
                || frame.width != header.width
                || frame.height != header.height
                || frame.is_keyframe != header.is_keyframe()
            {
                self.stats.chunks_rejected += 1;
                return None;
            }
            if !frame.mark(header.chunk_idx) {
                self.stats.chunks_rejected += 1; // duplicate
                return None;
            }
            let offset = header.chunk_idx as usize * VIDEO_CHUNK_PAYLOAD;
            frame.data[offset..offset + payload.len()].copy_from_slice(payload);
            if header.chunk_idx == header.chunk_count - 1 {
                frame.last_len = payload.len();
            }
            frame.received == frame.chunk_count
        };

        if !complete {
            return None;
        }

        let Some(frame) = self.pending.remove(&key) else {
            return None; // unreachable: checked above
        };
        let total = (frame.chunk_count as usize - 1) * VIDEO_CHUNK_PAYLOAD + frame.last_len;
        let mut data = frame.data;
        data.truncate(total);
        self.stats.frames_completed += 1;
        Some(ReassembledFrame {
            sender_id,
            seq: header.seq,
            is_keyframe: frame.is_keyframe,
            width: frame.width,
            height: frame.height,
            data,
        })
    }

    /// Drop all partial frames from one sender (stream stopped, user left).
    pub fn forget_sender(&mut self, sender_id: u64) {
        self.pending.retain(|(sid, _), _| *sid != sender_id);
    }

    /// Number of frames currently awaiting chunks.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Drop all partial frames.
    pub fn reset(&mut self) {
        self.pending.clear();
    }

    fn evict_stale(&mut self) {
        let before = self.pending.len();
        self.pending
            .retain(|_, frame| frame.first_seen.elapsed() < FRAME_TIMEOUT);
        self.stats.frames_dropped += (before - self.pending.len()) as u64;
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self
            .pending
            .iter()
            .min_by_key(|(_, frame)| frame.first_seen)
            .map(|(key, _)| *key)
        {
            self.pending.remove(&oldest);
            self.stats.frames_dropped += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENDER: u64 = 42;

    fn make_frame(len: usize, byte: u8) -> Vec<u8> {
        vec![byte; len]
    }

    #[test]
    fn single_chunk_roundtrip() {
        let data = make_frame(500, 0xAA);
        let chunks = pack_video_frame(1, true, 1920, 1080, &data).unwrap();
        assert_eq!(chunks.len(), 1);

        let mut reassembler = VideoReassembler::new();
        let frame = reassembler.push_chunk(SENDER, &chunks[0]).unwrap();
        assert_eq!(frame.data, data);
        assert_eq!(frame.sender_id, SENDER);
        assert!(frame.is_keyframe);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
    }

    #[test]
    fn multi_chunk_roundtrip() {
        let data = make_frame(VIDEO_CHUNK_PAYLOAD * 3 + 123, 0xBB);
        let chunks = pack_video_frame(7, false, 1280, 720, &data).unwrap();
        assert_eq!(chunks.len(), 4);

        let mut reassembler = VideoReassembler::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let result = reassembler.push_chunk(SENDER, chunk);
            if i < 3 {
                assert!(result.is_none());
            } else {
                let frame = result.unwrap();
                assert_eq!(frame.data, data);
                assert_eq!(frame.seq, 7);
                assert!(!frame.is_keyframe);
            }
        }
        assert_eq!(reassembler.pending_count(), 0);
    }

    #[test]
    fn out_of_order_chunks() {
        let data = make_frame(VIDEO_CHUNK_PAYLOAD * 2 + 50, 0xCC);
        let chunks = pack_video_frame(9, true, 640, 480, &data).unwrap();
        assert_eq!(chunks.len(), 3);

        let mut reassembler = VideoReassembler::new();
        assert!(reassembler.push_chunk(SENDER, &chunks[2]).is_none());
        assert!(reassembler.push_chunk(SENDER, &chunks[0]).is_none());
        let frame = reassembler.push_chunk(SENDER, &chunks[1]).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn duplicate_chunks_ignored() {
        let data = make_frame(VIDEO_CHUNK_PAYLOAD + 10, 0xDD);
        let chunks = pack_video_frame(3, false, 800, 600, &data).unwrap();

        let mut reassembler = VideoReassembler::new();
        assert!(reassembler.push_chunk(SENDER, &chunks[0]).is_none());
        assert!(reassembler.push_chunk(SENDER, &chunks[0]).is_none()); // dup
        let frame = reassembler.push_chunk(SENDER, &chunks[1]).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn interleaved_senders_do_not_collide() {
        // Same seq from two senders must reassemble independently.
        let data_a = make_frame(VIDEO_CHUNK_PAYLOAD + 5, 0x01);
        let data_b = make_frame(VIDEO_CHUNK_PAYLOAD + 5, 0x02);
        let chunks_a = pack_video_frame(1, false, 800, 600, &data_a).unwrap();
        let chunks_b = pack_video_frame(1, false, 800, 600, &data_b).unwrap();

        let mut reassembler = VideoReassembler::new();
        assert!(reassembler.push_chunk(1, &chunks_a[0]).is_none());
        assert!(reassembler.push_chunk(2, &chunks_b[0]).is_none());
        let frame_b = reassembler.push_chunk(2, &chunks_b[1]).unwrap();
        let frame_a = reassembler.push_chunk(1, &chunks_a[1]).unwrap();
        assert_eq!(frame_a.data, data_a);
        assert_eq!(frame_b.data, data_b);
    }

    #[test]
    fn empty_frame_refused() {
        assert!(pack_video_frame(1, false, 800, 600, &[]).is_none());
    }

    #[test]
    fn oversize_frame_refused() {
        let data = make_frame(MAX_VIDEO_FRAME_BYTES + 1, 0);
        assert!(pack_video_frame(1, false, 800, 600, &data).is_none());
    }

    #[test]
    fn max_size_frame_roundtrips() {
        let data = make_frame(MAX_VIDEO_FRAME_BYTES, 0xEE);
        let chunks = pack_video_frame(1, true, 3840, 2160, &data).unwrap();
        assert_eq!(chunks.len(), 255);

        let mut reassembler = VideoReassembler::new();
        let mut result = None;
        for chunk in &chunks {
            result = reassembler.push_chunk(SENDER, chunk);
        }
        assert_eq!(result.unwrap().data, data);
    }

    #[test]
    fn malformed_chunks_rejected() {
        let mut reassembler = VideoReassembler::new();
        // Too short.
        assert!(reassembler.push_chunk(SENDER, &[2, 0, 0, 1]).is_none());
        // Wrong version.
        let chunks = pack_video_frame(1, false, 800, 600, &[1, 2, 3]).unwrap();
        let mut bad = chunks[0].to_vec();
        bad[0] = 1;
        assert!(reassembler.push_chunk(SENDER, &bad).is_none());
        // chunk_idx ≥ chunk_count.
        let mut bad = chunks[0].to_vec();
        bad[2] = 5;
        bad[3] = 2;
        assert!(reassembler.push_chunk(SENDER, &bad).is_none());
        // Silly dimensions.
        let mut bad = chunks[0].to_vec();
        bad[8] = 0;
        bad[9] = 0;
        assert!(reassembler.push_chunk(SENDER, &bad).is_none());
        assert_eq!(reassembler.stats.chunks_rejected, 4);
    }

    #[test]
    fn non_last_chunk_with_short_payload_rejected() {
        // Hand-build a 2-chunk frame where chunk 0 is undersized.
        let mut bad = Vec::new();
        bad.push(VIDEO_PROTOCOL_VERSION);
        bad.push(0);
        bad.push(0); // idx 0
        bad.push(2); // count 2 ⇒ chunk 0 must be exactly VIDEO_CHUNK_PAYLOAD
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.extend_from_slice(&800u16.to_le_bytes());
        bad.extend_from_slice(&600u16.to_le_bytes());
        bad.extend_from_slice(&[0u8; 100]);
        assert!(parse_video_header(&bad).is_none());
    }

    #[test]
    fn contradictory_chunk_rejected_without_poisoning_frame() {
        let data = make_frame(VIDEO_CHUNK_PAYLOAD + 10, 0x11);
        let chunks = pack_video_frame(5, false, 800, 600, &data).unwrap();

        let mut reassembler = VideoReassembler::new();
        assert!(reassembler.push_chunk(SENDER, &chunks[0]).is_none());
        // Same (sender, seq) but different dims: reject chunk only.
        let mut liar = chunks[1].to_vec();
        liar[8..10].copy_from_slice(&1024u16.to_le_bytes());
        assert!(reassembler.push_chunk(SENDER, &liar).is_none());
        // Original frame still completes.
        let frame = reassembler.push_chunk(SENDER, &chunks[1]).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn pending_overflow_evicts_oldest() {
        let mut reassembler = VideoReassembler::new();
        let data = make_frame(VIDEO_CHUNK_PAYLOAD + 1, 0);
        // 17 incomplete frames; the first should be evicted.
        for seq in 0..17u32 {
            let chunks = pack_video_frame(seq, false, 800, 600, &data).unwrap();
            assert!(reassembler.push_chunk(SENDER, &chunks[0]).is_none());
        }
        assert_eq!(reassembler.pending_count(), 16);
        assert_eq!(reassembler.stats.frames_dropped, 1);
    }

    #[test]
    fn forget_sender_clears_only_that_sender() {
        let data = make_frame(VIDEO_CHUNK_PAYLOAD + 1, 0);
        let chunks = pack_video_frame(1, false, 800, 600, &data).unwrap();
        let mut reassembler = VideoReassembler::new();
        assert!(reassembler.push_chunk(1, &chunks[0]).is_none());
        assert!(reassembler.push_chunk(2, &chunks[0]).is_none());
        reassembler.forget_sender(1);
        assert_eq!(reassembler.pending_count(), 1);
        // Sender 2 can still finish.
        assert!(reassembler.push_chunk(2, &chunks[1]).is_some());
    }
}
