//! System audio (Opus stereo) packet wire format.
//!
//! System audio packets are sent as raw binary on [`SYSTEM_AUDIO_CHANNEL`]
//! — **not** bincode-serialized — same approach as voice and video for
//! minimal overhead on the hot path.
//!
//! Wire layout:
//! ```text
//! [SystemAudioHeader: 20 bytes][Opus stereo data: data_len bytes]
//! ```
//!
//! Unlike voice, system audio is **one Opus frame per packet** (no
//! bundling). Each frame is a 20ms stereo Opus packet at 128 kbps,
//! typically ~320 bytes — well under MTU, no fragmentation needed.
//!
//! System audio packets are routed using the same subscriber model as
//! video: only peers subscribed to a screen share receive the sharer's
//! system audio.

/// System audio sub-protocol version. Bump when the wire format changes.
pub const SYSTEM_AUDIO_PROTOCOL_VERSION: u8 = 1;

/// Maximum system audio packet size (header + Opus data).
///
/// Stereo Opus at 128 kbps produces ~320 bytes per 20ms frame.
/// 1 KB is generous headroom — if a packet exceeds this, something
/// is wrong and we should drop it.
pub const SYSTEM_AUDIO_MTU_BUDGET: usize = 1024;

/// Fixed-layout header for system audio packets.
///
/// Total size: 20 bytes. All multi-byte fields are little-endian.
///
/// ```text
/// offset  size  field
///   0       1   version (SYSTEM_AUDIO_PROTOCOL_VERSION)
///   1       1   channels (2 = stereo)
///   2       2   duration_ms (u16 LE) — frame duration in ms (20)
///   4       8   client_id (u64 LE) — who is sharing
///  12       4   seq (u32 LE) — frame sequence number
///  16       4   data_len (u32 LE) — Opus payload length in bytes
/// ```
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SystemAudioHeader {
    pub version: u8,
    pub channels: u8,
    pub duration_ms: u16,
    pub client_id: u64,
    pub seq: u32,
    pub data_len: u32,
}

impl SystemAudioHeader {
    /// Total byte size of the header on the wire.
    pub const SIZE: usize = 20;
}

// ── Serialization helpers ───────────────────────────────────────────────

/// Write a system audio header + Opus payload into a Vec.
///
/// Returns the complete packet ready to send on `SYSTEM_AUDIO_CHANNEL`.
pub fn pack_system_audio(
    client_id: u64,
    seq: u32,
    channels: u8,
    duration_ms: u16,
    opus_data: &[u8],
) -> Vec<u8> {
    let total = SystemAudioHeader::SIZE + opus_data.len();
    let mut buf = Vec::with_capacity(total);

    buf.push(SYSTEM_AUDIO_PROTOCOL_VERSION);
    buf.push(channels);
    buf.extend_from_slice(&duration_ms.to_le_bytes());
    buf.extend_from_slice(&client_id.to_le_bytes());
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&(opus_data.len() as u32).to_le_bytes());
    buf.extend_from_slice(opus_data);

    buf
}

/// Parse a system audio packet header.
///
/// Returns `None` if the buffer is too small or the version doesn't match.
pub fn parse_system_audio_header(data: &[u8]) -> Option<SystemAudioHeader> {
    if data.len() < SystemAudioHeader::SIZE {
        return None;
    }
    if data[0] != SYSTEM_AUDIO_PROTOCOL_VERSION {
        return None;
    }

    Some(SystemAudioHeader {
        version: data[0],
        channels: data[1],
        duration_ms: u16::from_le_bytes(data[2..4].try_into().ok()?),
        client_id: u64::from_le_bytes(data[4..12].try_into().ok()?),
        seq: u32::from_le_bytes(data[12..16].try_into().ok()?),
        data_len: u32::from_le_bytes(data[16..20].try_into().ok()?),
    })
}

/// Extract the Opus payload from a system audio packet.
///
/// Returns `None` if the packet is malformed (header says more data than
/// buffer contains).
pub fn system_audio_payload(data: &[u8]) -> Option<&[u8]> {
    let hdr = parse_system_audio_header(data)?;
    let start = SystemAudioHeader::SIZE;
    let end = start + hdr.data_len as usize;
    if end > data.len() {
        return None;
    }
    Some(&data[start..end])
}
