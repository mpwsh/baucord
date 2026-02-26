//! Application-layer chunking for video frames.
//!
//! renet's unreliable channel fragments large messages into ~1200-byte UDP
//! packets internally. If **any** fragment is lost, the **entire** message
//! is dropped — and with 60 fragmented messages per second, the reassembly
//! buffer overflows and silently discards incomplete messages.
//!
//! This module fixes the problem by chunking frames at the application layer
//! so each chunk fits in a single UDP packet (no renet fragmentation). Lost
//! chunks only affect one piece, not the whole frame, and the reassembly
//! buffer pressure is eliminated.
//!
//! ## Wire format
//!
//! Each chunk is a self-contained message sent over renet's unreliable channel:
//!
//! ```text
//! Bytes  Field
//! ─────  ──────────────────────────────────
//! 0-3    frame_seq: u32      (frame sequence number)
//! 4      chunk_idx: u8       (this chunk's index, 0-based)
//! 5      chunk_count: u8     (total chunks in this frame)
//! 6      flags: u8           (bit 0 = is_keyframe)
//! 7-8    width: u16
//! 9-10   height: u16
//! 11..   payload: [u8]       (chunk of encoded H.264 data)
//! ```
//!
//! Header is 11 bytes. With a 1,100-byte chunk payload, total message size
//! is 1,111 bytes — well under the ~1,200-byte MTU, leaving room for renet's
//! own framing and UDP/IP headers.

use std::collections::HashMap;
use std::time::Instant;

/// Maximum payload bytes per chunk. Chosen so header + payload fits in a
/// single UDP packet without triggering renet fragmentation.
///
/// UDP MTU is typically 1,200 bytes for internet traffic (conservative).
/// Minus 11 bytes header = 1,189 bytes payload. We round down to 1,100
/// for safety margin (renet adds its own framing).
pub const CHUNK_PAYLOAD: usize = 1_100;

/// Header size in bytes.
const HEADER_SIZE: usize = 11;

/// Maximum number of chunks per frame. At 1,100 bytes per chunk, this
/// supports frames up to 255 × 1,100 = 280KB — more than enough for
/// even the largest keyframes.
const MAX_CHUNKS: usize = 255;

/// Maximum age before an incomplete frame is discarded (milliseconds).
const FRAME_TIMEOUT_MS: u128 = 500;

/// Maximum number of in-flight frames in the reassembly buffer.
/// Prevents unbounded memory growth from lost frames.
const MAX_PENDING_FRAMES: usize = 10;

// ── Flags ───────────────────────────────────────────────────────────────

const FLAG_KEYFRAME: u8 = 0x01;

// ── Chunker (sender side) ───────────────────────────────────────────────

/// Split an encoded video frame into chunks that each fit in a single
/// UDP packet.
///
/// Returns an iterator of byte vectors, each ready to send as an
/// independent renet unreliable message. No chunk will trigger renet's
/// internal fragmentation.
///
/// # Panics
///
/// Panics if the frame exceeds [`MAX_CHUNKS`] × [`CHUNK_PAYLOAD`] bytes
/// (~280KB). This should never happen with properly configured encoder
/// bitrate limits.
pub fn chunk_frame(
    seq: u32,
    is_keyframe: bool,
    width: u16,
    height: u16,
    data: &[u8],
) -> Vec<Vec<u8>> {
    let chunk_count = (data.len() + CHUNK_PAYLOAD - 1) / CHUNK_PAYLOAD;
    assert!(
        chunk_count <= MAX_CHUNKS,
        "frame too large: {} bytes = {} chunks (max {})",
        data.len(),
        chunk_count,
        MAX_CHUNKS,
    );
    // Handle empty frames (shouldn't happen, but be safe)
    let chunk_count = chunk_count.max(1);

    let flags = if is_keyframe { FLAG_KEYFRAME } else { 0 };

    let mut chunks = Vec::with_capacity(chunk_count);

    for i in 0..chunk_count {
        let start = i * CHUNK_PAYLOAD;
        let end = (start + CHUNK_PAYLOAD).min(data.len());
        let payload = &data[start..end];

        let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
        buf.extend_from_slice(&seq.to_le_bytes()); // 0..4
        buf.push(i as u8); // 4
        buf.push(chunk_count as u8); // 5
        buf.push(flags); // 6
        buf.extend_from_slice(&width.to_le_bytes()); // 7..9
        buf.extend_from_slice(&height.to_le_bytes()); // 9..11
        buf.extend_from_slice(payload); // 11..

        chunks.push(buf);
    }

    chunks
}

// ── Reassembler (receiver side) ─────────────────────────────────────────

/// A fully reassembled video frame, ready to push into the jitter buffer.
#[derive(Debug)]
pub struct ReassembledFrame {
    pub seq: u32,
    pub is_keyframe: bool,
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

/// Tracks reassembly state for a single frame.
struct PendingFrame {
    chunk_count: u8,
    received: u8,
    is_keyframe: bool,
    width: u16,
    height: u16,
    /// Chunks indexed by chunk_idx. `None` = not yet received.
    chunks: Vec<Option<Vec<u8>>>,
    first_seen: Instant,
}

/// Reassembles chunked video frames from individual chunk messages.
///
/// Each chunk is an independent renet message. The reassembler collects
/// chunks by frame sequence number and emits complete frames once all
/// chunks have arrived.
///
/// Incomplete frames are discarded after [`FRAME_TIMEOUT_MS`] or when
/// the pending buffer exceeds [`MAX_PENDING_FRAMES`].
pub struct ChunkReassembler {
    pending: HashMap<u32, PendingFrame>,
    /// Stats for diagnostics.
    pub frames_completed: u64,
    pub frames_dropped_timeout: u64,
    pub frames_dropped_overflow: u64,
    pub chunks_received: u64,
    pub chunks_duplicate: u64,
}

impl ChunkReassembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            frames_completed: 0,
            frames_dropped_timeout: 0,
            frames_dropped_overflow: 0,
            chunks_received: 0,
            chunks_duplicate: 0,
        }
    }

    /// Process a raw chunk message received from the network.
    ///
    /// Returns `Some(ReassembledFrame)` if this chunk completed a frame,
    /// `None` otherwise. This is the hot path — called for every incoming
    /// chunk message.
    ///
    /// Invalid or malformed chunks are silently dropped.
    pub fn push_chunk(&mut self, raw: &[u8]) -> Option<ReassembledFrame> {
        if raw.len() < HEADER_SIZE {
            return None; // malformed
        }

        let seq = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let chunk_idx = raw[4] as usize;
        let chunk_count = raw[5];
        let flags = raw[6];
        let width = u16::from_le_bytes([raw[7], raw[8]]);
        let height = u16::from_le_bytes([raw[9], raw[10]]);
        let payload = &raw[HEADER_SIZE..];

        if chunk_count == 0 || chunk_idx >= chunk_count as usize {
            return None; // malformed
        }

        self.chunks_received += 1;

        // Evict timed-out frames before inserting
        self.evict_stale();

        let is_keyframe = (flags & FLAG_KEYFRAME) != 0;

        let frame = self.pending.entry(seq).or_insert_with(|| {
            // Overflow protection: if too many frames are pending, evict
            // the oldest non-keyframe frame.
            if self.pending.len() >= MAX_PENDING_FRAMES {
                self.evict_oldest();
            }
            PendingFrame {
                chunk_count,
                received: 0,
                is_keyframe,
                width,
                height,
                chunks: vec![None; chunk_count as usize],
                first_seen: Instant::now(),
            }
        });

        // Validate chunk_count consistency
        if frame.chunk_count != chunk_count {
            return None; // mismatched metadata — ignore
        }

        // Insert chunk if not already received
        if frame.chunks[chunk_idx].is_some() {
            self.chunks_duplicate += 1;
            return None;
        }

        frame.chunks[chunk_idx] = Some(payload.to_vec());
        frame.received += 1;

        // Check if frame is complete
        if frame.received == frame.chunk_count {
            let frame = self.pending.remove(&seq)?;
            self.frames_completed += 1;

            // Concatenate chunks in order
            let total_size: usize = frame.chunks.iter().map(|c| c.as_ref().map_or(0, |v| v.len())).sum();
            let mut data = Vec::with_capacity(total_size);
            for chunk in &frame.chunks {
                if let Some(payload) = chunk {
                    data.extend_from_slice(payload);
                }
            }

            Some(ReassembledFrame {
                seq,
                is_keyframe: frame.is_keyframe,
                width: frame.width,
                height: frame.height,
                data,
            })
        } else {
            None
        }
    }

    /// Remove frames that have been pending longer than the timeout.
    fn evict_stale(&mut self) {
        let now = Instant::now();
        self.pending.retain(|_seq, frame| {
            let age = now.duration_since(frame.first_seen).as_millis();
            if age > FRAME_TIMEOUT_MS {
                self.frames_dropped_timeout += 1;
                false
            } else {
                true
            }
        });
    }

    /// Evict the oldest non-keyframe pending frame to make room.
    fn evict_oldest(&mut self) {
        // Prefer evicting non-keyframes — keyframes are recovery points
        let oldest = self
            .pending
            .iter()
            .filter(|(_, f)| !f.is_keyframe)
            .min_by_key(|(_, f)| f.first_seen)
            .map(|(&seq, _)| seq);

        let to_remove = oldest.or_else(|| {
            self.pending
                .iter()
                .min_by_key(|(_, f)| f.first_seen)
                .map(|(&seq, _)| seq)
        });

        if let Some(seq) = to_remove {
            self.pending.remove(&seq);
            self.frames_dropped_overflow += 1;
        }
    }

    /// Number of frames currently being reassembled.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_frame_roundtrips() {
        let data = vec![0xAB; 500]; // small frame, fits in one chunk
        let chunks = chunk_frame(42, true, 1280, 720, &data);

        assert_eq!(chunks.len(), 1);

        let mut reassembler = ChunkReassembler::new();
        let frame = reassembler.push_chunk(&chunks[0]).unwrap();

        assert_eq!(frame.seq, 42);
        assert!(frame.is_keyframe);
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        assert_eq!(frame.data, data);
    }

    #[test]
    fn multi_chunk_frame_roundtrips() {
        let data = vec![0xCD; 5000]; // ~5 chunks
        let chunks = chunk_frame(99, false, 1920, 1080, &data);

        assert_eq!(chunks.len(), 5);

        let mut reassembler = ChunkReassembler::new();

        // Feed chunks in order — only the last should complete
        for (i, chunk) in chunks.iter().enumerate() {
            let result = reassembler.push_chunk(chunk);
            if i < chunks.len() - 1 {
                assert!(result.is_none(), "chunk {i} should not complete frame");
            } else {
                let frame = result.unwrap();
                assert_eq!(frame.seq, 99);
                assert!(!frame.is_keyframe);
                assert_eq!(frame.data, data);
            }
        }
    }

    #[test]
    fn out_of_order_chunks_reassemble() {
        let data = vec![0xEF; 3500]; // 4 chunks
        let chunks = chunk_frame(7, true, 1280, 720, &data);
        assert_eq!(chunks.len(), 4);

        let mut reassembler = ChunkReassembler::new();

        // Feed in reverse order
        assert!(reassembler.push_chunk(&chunks[3]).is_none());
        assert!(reassembler.push_chunk(&chunks[1]).is_none());
        assert!(reassembler.push_chunk(&chunks[0]).is_none());

        let frame = reassembler.push_chunk(&chunks[2]).unwrap();
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.data, data);
    }

    #[test]
    fn duplicate_chunks_ignored() {
        let data = vec![0x11; 2500];
        let chunks = chunk_frame(1, false, 1280, 720, &data);

        let mut reassembler = ChunkReassembler::new();
        reassembler.push_chunk(&chunks[0]);
        reassembler.push_chunk(&chunks[0]); // duplicate
        assert_eq!(reassembler.chunks_duplicate, 1);

        // Should still complete with remaining chunks
        reassembler.push_chunk(&chunks[1]);
        let frame = reassembler.push_chunk(&chunks[2]).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn lost_chunk_means_lost_frame() {
        let data = vec![0x22; 3500]; // 4 chunks
        let chunks = chunk_frame(5, false, 1280, 720, &data);

        let mut reassembler = ChunkReassembler::new();

        // Only send 3 of 4 chunks — frame never completes
        reassembler.push_chunk(&chunks[0]);
        reassembler.push_chunk(&chunks[1]);
        reassembler.push_chunk(&chunks[3]);
        // chunk[2] is "lost"

        assert_eq!(reassembler.pending_count(), 1);
        assert_eq!(reassembler.frames_completed, 0);
    }

    #[test]
    fn large_keyframe_chunks_correctly() {
        // Simulate a 120KB keyframe
        let data = vec![0xFF; 120_000];
        let chunks = chunk_frame(1000, true, 1280, 720, &data);

        // 120_000 / 1_100 = 110 chunks (rounded up)
        assert_eq!(chunks.len(), 110);

        // Verify each chunk is under the fragmentation threshold
        for chunk in &chunks {
            assert!(
                chunk.len() <= CHUNK_PAYLOAD + HEADER_SIZE,
                "chunk too large: {} bytes",
                chunk.len()
            );
        }

        // Full reassembly
        let mut reassembler = ChunkReassembler::new();
        let mut result = None;
        for chunk in &chunks {
            result = reassembler.push_chunk(chunk);
        }
        let frame = result.unwrap();
        assert_eq!(frame.data, data);
        assert!(frame.is_keyframe);
    }

    #[test]
    fn malformed_chunks_rejected() {
        let mut reassembler = ChunkReassembler::new();

        // Too short
        assert!(reassembler.push_chunk(&[0; 5]).is_none());

        // chunk_count = 0
        let mut bad = vec![0u8; HEADER_SIZE + 10];
        bad[5] = 0; // chunk_count = 0
        assert!(reassembler.push_chunk(&bad).is_none());

        // chunk_idx >= chunk_count
        bad[4] = 5; // chunk_idx = 5
        bad[5] = 3; // chunk_count = 3
        assert!(reassembler.push_chunk(&bad).is_none());
    }

    #[test]
    fn chunk_payload_size_fits_mtu() {
        // Verify our constants are consistent
        assert!(CHUNK_PAYLOAD + HEADER_SIZE <= 1200,
            "chunk total {} exceeds MTU safe zone",
            CHUNK_PAYLOAD + HEADER_SIZE
        );
    }
}