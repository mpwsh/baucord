//! Video (H.264) packet wire format.
//!
//! Video packets are sent as raw binary on [`VIDEO_CHANNEL`] — **not**
//! bincode-serialized — same approach as voice for minimal overhead.
//!
//! Wire layout:
//! ```text
//! [VideoHeader: 24 bytes][H.264 NAL data: data_len bytes]
//! ```
//!
//! Unlike voice, video is one frame per packet (no bundling). Renet
//! handles fragmentation internally for frames that exceed MTU.

/// Video sub-protocol version. Bump when the wire format changes.
pub const VIDEO_PROTOCOL_VERSION: u8 = 1;

/// Maximum video packet size (header + H.264 data).
/// Renet fragments internally, but we cap total size to prevent abuse.
/// 128 KB accommodates even large keyframes at 1080p.
pub const VIDEO_MTU_BUDGET: usize = 300 * 1024;

/// Fixed-layout header for video packets.
///
/// Total size: 24 bytes. All multi-byte fields are little-endian.
///
/// ```text
/// offset  size  field
///   0       1   version (VIDEO_PROTOCOL_VERSION)
///   1       1   flags (bit 0 = keyframe)
///   2       2   reserved (zero)
///   4       8   client_id (u64 LE) — sender
///  12       4   seq (u32 LE) — frame sequence number
///  16       2   width (u16 LE) — frame width in pixels
///  18       2   height (u16 LE) — frame height in pixels
///  20       4   data_len (u32 LE) — H.264 payload length
/// ```
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct VideoHeader {
    pub version: u8,
    pub flags: u8,
    pub _reserved: [u8; 2],
    pub client_id: u64,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub data_len: u32,
}

impl VideoHeader {
    /// Total byte size of the header on the wire.
    pub const SIZE: usize = 24;

    /// Flag bit: this frame is a keyframe (IDR).
    pub const FLAG_KEYFRAME: u8 = 0x01;
}

// ── Serialization helpers ───────────────────────────────────────────────

/// Write a video header + H.264 payload into a Vec.
///
/// Returns the complete packet ready to send on `VIDEO_CHANNEL`.
pub fn pack_video_frame(
    client_id: u64,
    seq: u32,
    is_keyframe: bool,
    width: u16,
    height: u16,
    h264_data: &[u8],
) -> Vec<u8> {
    let total = VideoHeader::SIZE + h264_data.len();
    let mut buf = Vec::with_capacity(total);

    buf.push(VIDEO_PROTOCOL_VERSION);
    buf.push(if is_keyframe {
        VideoHeader::FLAG_KEYFRAME
    } else {
        0
    });
    buf.extend_from_slice(&[0, 0]); // reserved
    buf.extend_from_slice(&client_id.to_le_bytes());
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&(h264_data.len() as u32).to_le_bytes());
    buf.extend_from_slice(h264_data);

    buf
}

/// Parse a video packet header. Returns `None` if the buffer is too small
/// or the version doesn't match.
pub fn parse_video_header(data: &[u8]) -> Option<VideoHeader> {
    if data.len() < VideoHeader::SIZE {
        return None;
    }
    if data[0] != VIDEO_PROTOCOL_VERSION {
        return None;
    }

    Some(VideoHeader {
        version: data[0],
        flags: data[1],
        _reserved: [data[2], data[3]],
        client_id: u64::from_le_bytes(data[4..12].try_into().ok()?),
        seq: u32::from_le_bytes(data[12..16].try_into().ok()?),
        width: u16::from_le_bytes(data[16..18].try_into().ok()?),
        height: u16::from_le_bytes(data[18..20].try_into().ok()?),
        data_len: u32::from_le_bytes(data[20..24].try_into().ok()?),
    })
}

/// Extract the H.264 payload from a video packet.
///
/// Returns `None` if the packet is malformed (header says more data than
/// buffer contains).
pub fn video_payload(data: &[u8]) -> Option<&[u8]> {
    let hdr = parse_video_header(data)?;
    let start = VideoHeader::SIZE;
    let end = start + hdr.data_len as usize;
    if end > data.len() {
        return None;
    }
    Some(&data[start..end])
}
