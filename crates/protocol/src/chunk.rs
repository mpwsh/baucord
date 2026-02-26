//! Video chunk wire format and reassembly.
//!
//! renet's unreliable channel fragments large messages into ~1200-byte UDP
//! packets internally. If **any** fragment is lost, the **entire** message
//! is dropped — and with 60 fragmented messages per second, the reassembly
//! buffer overflows and silently discards incomplete messages.
//!
//! This module fixes the problem by chunking frames at the application layer
//! so each chunk fits in a single UDP packet (no renet fragmentation). Lost
//! chunks only affect one frame, not the whole GOP.
//!
//! ## Wire format
//!
//! Each chunk is a self-contained message sent over renet's unreliable channel:
//!
//! ```text
//! offset  size  field
//!   0       1   version (VIDEO_CHUNK_VERSION)
//!   1       1   flags (bit 0 = keyframe)
//!   2       1   chunk_idx (0-based)
//!   3       1   chunk_count (total chunks in this frame)
//!   4       8   client_id (u64 LE) — sender
//!  12       4   seq (u32 LE) — frame sequence number
//!  16       2   width (u16 LE)
//!  18       2   height (u16 LE)
//!  20..     payload: [u8] — chunk of encoded H.264 data
//! ```
//!
//! Header is 20 bytes. With a 1,080-byte chunk payload, total message size
//! is 1,100 bytes — well under the ~1,200-byte MTU, leaving room for renet's
//! own framing and UDP/IP headers.
//!
//! The header deliberately mirrors [`super::VideoHeader`] conventions:
//! same `client_id` field for server-side routing, same keyframe flag
//! semantics, same endianness.

use std::collections::HashMap;
use std::time::Instant;

/// Video chunk sub-protocol version. Bump when the wire format changes.
pub const VIDEO_CHUNK_VERSION: u8 = 1;

/// Header size in bytes.
pub const CHUNK_HEADER_SIZE: usize = 20;

/// Maximum payload bytes per chunk.
///
/// Chosen so header + payload fits in a single UDP packet without
/// triggering renet fragmentation. Conservative MTU budget is ~1,200
/// bytes for internet traffic; minus 20 bytes header = 1,180 available.
/// We use 1,080 for safety margin (renet adds its own framing).
pub const CHUNK_PAYLOAD: usize = 1_080;

/// Maximum number of chunks per frame. At 1,080 bytes per chunk, this
/// supports frames up to 255 × 1,080 ≈ 276 KB.
const MAX_CHUNKS: usize = 255;

/// Maximum age before an incomplete frame is discarded (milliseconds).
const FRAME_TIMEOUT_MS: u128 = 500;

/// Maximum number of in-flight frames in the reassembly buffer per peer.
const MAX_PENDING_FRAMES: usize = 10;

// ── Flags ───────────────────────────────────────────────────────────────

/// Chunk flag: this frame is a keyframe (IDR).
pub const CHUNK_FLAG_KEYFRAME: u8 = 0x01;

// ── Parsed header ───────────────────────────────────────────────────────

/// Parsed chunk header (not stored on the wire as a struct — we serialize
/// field-by-field like the other protocol headers).
#[derive(Copy, Clone, Debug)]
pub struct VideoChunkHeader {
    pub version: u8,
    pub flags: u8,
    pub chunk_idx: u8,
    pub chunk_count: u8,
    pub client_id: u64,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
}

impl VideoChunkHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & CHUNK_FLAG_KEYFRAME != 0
    }
}

// ── Parse / pack helpers ────────────────────────────────────────────────

/// Parse a chunk header from raw bytes.
///
/// Returns `None` if the buffer is too small or the version doesn't match.
pub fn parse_chunk_header(data: &[u8]) -> Option<VideoChunkHeader> {
    if data.len() < CHUNK_HEADER_SIZE {
        return None;
    }
    if data[0] != VIDEO_CHUNK_VERSION {
        return None;
    }
    Some(VideoChunkHeader {
        version: data[0],
        flags: data[1],
        chunk_idx: data[2],
        chunk_count: data[3],
        client_id: u64::from_le_bytes(data[4..12].try_into().ok()?),
        seq: u32::from_le_bytes(data[12..16].try_into().ok()?),
        width: u16::from_le_bytes(data[16..18].try_into().ok()?),
        height: u16::from_le_bytes(data[18..20].try_into().ok()?),
    })
}

/// Extract the chunk payload from a raw chunk message.
pub fn chunk_payload(data: &[u8]) -> Option<&[u8]> {
    if data.len() < CHUNK_HEADER_SIZE {
        return None;
    }
    Some(&data[CHUNK_HEADER_SIZE..])
}

// ── Chunker (sender side) ───────────────────────────────────────────────

/// Split an encoded video frame into chunks that each fit in a single
/// UDP packet.
///
/// Returns a Vec of byte vectors, each ready to send as an independent
/// renet unreliable message. No chunk will trigger renet's internal
/// fragmentation.
///
/// # Panics
///
/// Panics if the frame exceeds [`MAX_CHUNKS`] × [`CHUNK_PAYLOAD`] bytes
/// (~276 KB). This should never happen with properly configured encoder
/// bitrate limits.
pub fn chunk_frame(
    client_id: u64,
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

    let flags = if is_keyframe { CHUNK_FLAG_KEYFRAME } else { 0 };

    let mut chunks = Vec::with_capacity(chunk_count);

    for i in 0..chunk_count {
        let start = i * CHUNK_PAYLOAD;
        let end = (start + CHUNK_PAYLOAD).min(data.len());
        let payload = &data[start..end];

        let mut buf = Vec::with_capacity(CHUNK_HEADER_SIZE + payload.len());
        buf.push(VIDEO_CHUNK_VERSION);                   // 0
        buf.push(flags);                                 // 1
        buf.push(i as u8);                               // 2: chunk_idx
        buf.push(chunk_count as u8);                     // 3: chunk_count
        buf.extend_from_slice(&client_id.to_le_bytes()); // 4..12
        buf.extend_from_slice(&seq.to_le_bytes());       // 12..16
        buf.extend_from_slice(&width.to_le_bytes());     // 16..18
        buf.extend_from_slice(&height.to_le_bytes());    // 18..20
        buf.extend_from_slice(payload);                  // 20..

        chunks.push(buf);
    }

    chunks
}

// ── Reassembler (receiver side) ─────────────────────────────────────────

/// A fully reassembled video frame, ready to push into the jitter buffer.
#[derive(Debug)]
pub struct ReassembledFrame {
    pub client_id: u64,
    pub seq: u32,
    pub is_keyframe: bool,
    pub width: u16,
    pub height: u16,
    pub data: Vec<u8>,
}

/// Tracks reassembly state for a single frame.
struct PendingFrame {
    client_id: u64,
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
/// Keyed by `(client_id, seq)` so multiple peers can stream simultaneously.
/// Incomplete frames are discarded after [`FRAME_TIMEOUT_MS`] or when
/// the pending buffer exceeds [`MAX_PENDING_FRAMES`].
pub struct ChunkReassembler {
    pending: HashMap<(u64, u32), PendingFrame>,
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
    /// `None` otherwise.
    pub fn push_chunk(&mut self, raw: &[u8]) -> Option<ReassembledFrame> {
        let hdr = parse_chunk_header(raw)?;
        let payload = chunk_payload(raw)?;

        if hdr.chunk_count == 0 || hdr.chunk_idx >= hdr.chunk_count {
            return None;
        }

        self.chunks_received += 1;

        let key = (hdr.client_id, hdr.seq);

        // Evict stale/overflow before touching the entry to avoid
        // borrow conflicts with &mut self inside entry().
        self.evict_stale();
        if self.pending.len() >= MAX_PENDING_FRAMES && !self.pending.contains_key(&key) {
            self.evict_oldest();
        }

        let frame = self.pending.entry(key).or_insert_with(|| PendingFrame {
            client_id: hdr.client_id,
            chunk_count: hdr.chunk_count,
            received: 0,
            is_keyframe: hdr.is_keyframe(),
            width: hdr.width,
            height: hdr.height,
            chunks: vec![None; hdr.chunk_count as usize],
            first_seen: Instant::now(),
        });

        // Validate chunk_count consistency
        if frame.chunk_count != hdr.chunk_count {
            return None;
        }

        let idx = hdr.chunk_idx as usize;

        // Duplicate chunk?
        if frame.chunks[idx].is_some() {
            self.chunks_duplicate += 1;
            return None;
        }

        frame.chunks[idx] = Some(payload.to_vec());
        frame.received += 1;

        // Complete?
        if frame.received == frame.chunk_count {
            let frame = self.pending.remove(&key)?;
            self.frames_completed += 1;

            let total_size: usize = frame
                .chunks
                .iter()
                .map(|c| c.as_ref().map_or(0, |v| v.len()))
                .sum();
            let mut data = Vec::with_capacity(total_size);
            for chunk in &frame.chunks {
                if let Some(p) = chunk {
                    data.extend_from_slice(p);
                }
            }

            Some(ReassembledFrame {
                client_id: frame.client_id,
                seq: hdr.seq,
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
        let before = self.pending.len();
        self.pending.retain(|_key, frame| {
            now.duration_since(frame.first_seen).as_millis() <= FRAME_TIMEOUT_MS
        });
        let evicted = before - self.pending.len();
        self.frames_dropped_timeout += evicted as u64;
    }

    /// Evict the oldest non-keyframe pending frame to make room.
    fn evict_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .filter(|(_, f)| !f.is_keyframe)
            .min_by_key(|(_, f)| f.first_seen)
            .map(|(&key, _)| key);

        let to_remove = oldest.or_else(|| {
            self.pending
                .iter()
                .min_by_key(|(_, f)| f.first_seen)
                .map(|(&key, _)| key)
        });

        if let Some(key) = to_remove {
            self.pending.remove(&key);
            self.frames_dropped_overflow += 1;
        }
    }

    /// Number of frames currently being reassembled.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Reset all state (e.g. when a peer stops sharing).
    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CID: u64 = 12345;

    #[test]
    fn single_chunk_frame_roundtrips() {
        let data = vec![0xAB; 500];
        let chunks = chunk_frame(TEST_CID, 42, true, 1280, 720, &data);
        assert_eq!(chunks.len(), 1);

        let mut r = ChunkReassembler::new();
        let frame = r.push_chunk(&chunks[0]).unwrap();

        assert_eq!(frame.client_id, TEST_CID);
        assert_eq!(frame.seq, 42);
        assert!(frame.is_keyframe);
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
        assert_eq!(frame.data, data);
    }

    #[test]
    fn multi_chunk_frame_roundtrips() {
        let data = vec![0xCD; 5000]; // ~5 chunks
        let chunks = chunk_frame(TEST_CID, 99, false, 1920, 1080, &data);
        assert_eq!(chunks.len(), 5);

        let mut r = ChunkReassembler::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let result = r.push_chunk(chunk);
            if i < chunks.len() - 1 {
                assert!(result.is_none());
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
        let chunks = chunk_frame(TEST_CID, 7, true, 1280, 720, &data);
        assert_eq!(chunks.len(), 4);

        let mut r = ChunkReassembler::new();
        assert!(r.push_chunk(&chunks[3]).is_none());
        assert!(r.push_chunk(&chunks[1]).is_none());
        assert!(r.push_chunk(&chunks[0]).is_none());
        let frame = r.push_chunk(&chunks[2]).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn duplicate_chunks_ignored() {
        let data = vec![0x11; 2500];
        let chunks = chunk_frame(TEST_CID, 1, false, 1280, 720, &data);

        let mut r = ChunkReassembler::new();
        r.push_chunk(&chunks[0]);
        r.push_chunk(&chunks[0]); // duplicate
        assert_eq!(r.chunks_duplicate, 1);

        r.push_chunk(&chunks[1]);
        let frame = r.push_chunk(&chunks[2]).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn lost_chunk_means_lost_frame() {
        let data = vec![0x22; 3500];
        let chunks = chunk_frame(TEST_CID, 5, false, 1280, 720, &data);

        let mut r = ChunkReassembler::new();
        r.push_chunk(&chunks[0]);
        r.push_chunk(&chunks[1]);
        r.push_chunk(&chunks[3]);

        assert_eq!(r.pending_count(), 1);
        assert_eq!(r.frames_completed, 0);
    }

    #[test]
    fn large_keyframe_chunks_correctly() {
        let data = vec![0xFF; 120_000];
        let chunks = chunk_frame(TEST_CID, 1000, true, 1280, 720, &data);
        // ceil(120_000 / 1_080) = 112
        assert_eq!(chunks.len(), 112);

        for chunk in &chunks {
            assert!(chunk.len() <= CHUNK_PAYLOAD + CHUNK_HEADER_SIZE);
        }

        let mut r = ChunkReassembler::new();
        let mut result = None;
        for chunk in &chunks {
            result = r.push_chunk(chunk);
        }
        let frame = result.unwrap();
        assert_eq!(frame.data, data);
        assert!(frame.is_keyframe);
    }

    #[test]
    fn malformed_chunks_rejected() {
        let mut r = ChunkReassembler::new();

        // Too short
        assert!(r.push_chunk(&[0; 5]).is_none());

        // chunk_count = 0
        let mut bad = vec![0u8; CHUNK_HEADER_SIZE + 10];
        bad[0] = VIDEO_CHUNK_VERSION;
        bad[3] = 0;
        assert!(r.push_chunk(&bad).is_none());

        // chunk_idx >= chunk_count
        bad[2] = 5;
        bad[3] = 3;
        assert!(r.push_chunk(&bad).is_none());
    }

    #[test]
    fn chunk_size_fits_mtu() {
        assert!(
            CHUNK_PAYLOAD + CHUNK_HEADER_SIZE <= 1200,
            "chunk total {} exceeds MTU safe zone",
            CHUNK_PAYLOAD + CHUNK_HEADER_SIZE
        );
    }

    #[test]
    fn multiple_peers_interleaved() {
        let data_a = vec![0xAA; 2200];
        let data_b = vec![0xBB; 2200];
        let chunks_a = chunk_frame(111, 1, false, 1280, 720, &data_a);
        let chunks_b = chunk_frame(222, 1, true, 1920, 1080, &data_b);

        let mut r = ChunkReassembler::new();
        r.push_chunk(&chunks_a[0]);
        r.push_chunk(&chunks_b[0]);
        r.push_chunk(&chunks_a[1]);
        r.push_chunk(&chunks_b[1]);

        let frame_b = r.push_chunk(&chunks_b[2]).unwrap();
        assert_eq!(frame_b.client_id, 222);
        assert_eq!(frame_b.data, data_b);

        let frame_a = r.push_chunk(&chunks_a[2]).unwrap();
        assert_eq!(frame_a.client_id, 111);
        assert_eq!(frame_a.data, data_a);
    }

    #[test]
    fn header_parse_roundtrip() {
        let chunks = chunk_frame(99999, 42, true, 1920, 1080, &[0xDE; 100]);
        let hdr = parse_chunk_header(&chunks[0]).unwrap();
        assert_eq!(hdr.version, VIDEO_CHUNK_VERSION);
        assert_eq!(hdr.client_id, 99999);
        assert_eq!(hdr.seq, 42);
        assert!(hdr.is_keyframe());
        assert_eq!(hdr.width, 1920);
        assert_eq!(hdr.height, 1080);
        assert_eq!(hdr.chunk_idx, 0);
        assert_eq!(hdr.chunk_count, 1);
    }
}