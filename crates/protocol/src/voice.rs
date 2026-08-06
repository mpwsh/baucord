//! Voice wire format (version 2).
//!
//! **One Opus frame per packet.** The v1 format supported bundling
//! multiple frames per packet behind a batching threshold that shipped
//! set to 1 — so every packet carried exactly one frame through a
//! multi-frame container (frame counts, per-frame length prefixes, and
//! three separate parsers of the container). Bundling frames would add
//! its own latency (frame N waits for frame N+1), which is the wrong
//! trade for this application: voice latency is the product's mission,
//! and the bandwidth saved (~4 kbit/s of headers per speaker) is noise
//! next to a single video stream. v2 deletes the container entirely.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! offset  size  field
//! 0       1     version   (= 2)
//! 1       1     flags     (reserved, must be 0)
//! 2       4     sequence number
//! 6       4     timestamp (48 kHz sample clock)
//! 10      —     Opus frame (1..=VOICE_MAX_FRAME_SIZE bytes)
//! ```
//!
//! There is no sender id (see [`crate::relayed`]) and no length field —
//! the frame is delimited by the datagram itself.

/// Current voice wire format version.
pub const VOICE_PROTOCOL_VERSION: u8 = 2;

/// Serialized header size in bytes.
pub const VOICE_HEADER_SIZE: usize = 10;

/// Maximum size of a single Opus frame we will send or relay. A 10 ms
/// frame at 128 kbit/s is ~160 bytes; 512 leaves ample headroom for any
/// sane encoder setting while bounding hostile input.
pub const VOICE_MAX_FRAME_SIZE: usize = 512;

/// Maximum total voice packet size accepted by the relay
/// (header + frame, before the relay envelope).
pub const VOICE_MTU_BUDGET: usize = VOICE_HEADER_SIZE + VOICE_MAX_FRAME_SIZE;

/// Parsed voice packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceHeader {
    pub version: u8,
    pub flags: u8,
    /// Monotonic per-sender packet sequence number.
    pub seq: u32,
    /// Timestamp of the first sample, in 48 kHz units.
    pub timestamp: u32,
}

/// Pack one Opus frame into a voice packet.
///
/// Returns `None` if the frame is empty or exceeds
/// [`VOICE_MAX_FRAME_SIZE`] — callers should drop such frames (and log),
/// never truncate audio.
#[must_use]
pub fn pack_voice(seq: u32, timestamp: u32, opus_frame: &[u8]) -> Option<Vec<u8>> {
    if opus_frame.is_empty() || opus_frame.len() > VOICE_MAX_FRAME_SIZE {
        return None;
    }
    let mut out = Vec::with_capacity(VOICE_HEADER_SIZE + opus_frame.len());
    out.push(VOICE_PROTOCOL_VERSION);
    out.push(0); // flags
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(opus_frame);
    Some(out)
}

/// Parse a voice packet from untrusted bytes.
///
/// Validates version, size bounds, and frame length. Returns the header
/// and a slice borrowing the Opus frame.
#[must_use]
pub fn parse_voice(data: &[u8]) -> Option<(VoiceHeader, &[u8])> {
    if data.len() <= VOICE_HEADER_SIZE || data.len() > VOICE_MTU_BUDGET {
        return None;
    }
    if data[0] != VOICE_PROTOCOL_VERSION {
        return None;
    }
    let header = VoiceHeader {
        version: data[0],
        flags: data[1],
        seq: u32::from_le_bytes([data[2], data[3], data[4], data[5]]),
        timestamp: u32::from_le_bytes([data[6], data[7], data[8], data[9]]),
    };
    Some((header, &data[VOICE_HEADER_SIZE..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let frame = vec![0xAB; 160];
        let packet = pack_voice(42, 48_000, &frame).unwrap();
        assert_eq!(packet.len(), VOICE_HEADER_SIZE + 160);

        let (header, parsed_frame) = parse_voice(&packet).unwrap();
        assert_eq!(header.version, VOICE_PROTOCOL_VERSION);
        assert_eq!(header.seq, 42);
        assert_eq!(header.timestamp, 48_000);
        assert_eq!(parsed_frame, &frame[..]);
    }

    #[test]
    fn empty_frame_refused() {
        assert!(pack_voice(0, 0, &[]).is_none());
    }

    #[test]
    fn oversize_frame_refused() {
        let frame = vec![0u8; VOICE_MAX_FRAME_SIZE + 1];
        assert!(pack_voice(0, 0, &frame).is_none());
    }

    #[test]
    fn max_frame_accepted() {
        let frame = vec![0u8; VOICE_MAX_FRAME_SIZE];
        let packet = pack_voice(1, 2, &frame).unwrap();
        let (_, parsed) = parse_voice(&packet).unwrap();
        assert_eq!(parsed.len(), VOICE_MAX_FRAME_SIZE);
    }

    #[test]
    fn wrong_version_rejected() {
        let mut packet = pack_voice(1, 2, &[0u8; 10]).unwrap();
        packet[0] = 1; // v1
        assert!(parse_voice(&packet).is_none());
    }

    #[test]
    fn truncated_rejected() {
        assert!(parse_voice(&[VOICE_PROTOCOL_VERSION, 0, 0, 0]).is_none());
        // Header with no frame at all is also invalid.
        let packet = pack_voice(1, 2, &[9]).unwrap();
        assert!(parse_voice(&packet[..VOICE_HEADER_SIZE]).is_none());
    }

    #[test]
    fn oversize_packet_rejected() {
        let mut packet = pack_voice(1, 2, &[0u8; VOICE_MAX_FRAME_SIZE]).unwrap();
        packet.push(0); // one byte over budget
        assert!(parse_voice(&packet).is_none());
    }
}
