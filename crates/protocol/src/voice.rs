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
//! 1       1     flags     (bit 0 = VOICE_FLAG_RED; other bits reserved, 0)
//! 2       4     sequence number
//! 6       4     timestamp (48 kHz sample clock)
//! 10      —     payload   (1..=VOICE_MAX_FRAME_SIZE bytes)
//! ```
//!
//! There is no sender id (see [`crate::relayed`]) and no length field —
//! the payload is delimited by the datagram itself.
//!
//! ## Payload types (2.1)
//!
//! With `flags == 0` the payload is one Opus packet, as in every v2
//! packet ever sent. With [`VOICE_FLAG_RED`] set it is a *redundant audio
//! bundle* (`audio::transport::red`): the sender's current Opus packet
//! plus full copies of its preceding one or two packets, which a receiver
//! uses to repair losses exactly. Senders only produce bundles when their
//! rate controller sees loss, so a receiver that predates this flag keeps
//! working on a clean link and conceals the bundles it cannot parse on a
//! lossy one — the same outcome it had before. That is why this is a flag
//! inside v2 rather than v3: a version bump would make old and new
//! clients mutually silent, a flag only degrades the old client under
//! the conditions it was already degrading in.
//!
//! The relay only checks version and size (`parse_voice`), so bundles
//! pass through unchanged.

/// Current voice wire format version.
pub const VOICE_PROTOCOL_VERSION: u8 = 2;

/// `flags` bit: the payload is a redundant audio bundle, not a bare Opus
/// packet.
pub const VOICE_FLAG_RED: u8 = 0x01;

/// All flag bits this version understands. Unknown bits are ignored by
/// receivers so future flags can be added without a version bump, as this
/// one was.
pub const VOICE_FLAGS_KNOWN: u8 = VOICE_FLAG_RED;

/// Serialized header size in bytes.
pub const VOICE_HEADER_SIZE: usize = 10;

/// Maximum payload we will send or relay. A 20 ms voice frame at 48 kbit/s
/// is ~120 bytes; the largest RED bundle the audio crate produces (32 kbit/s,
/// two copies, headers) is ~250 bytes. 512 leaves headroom while bounding
/// hostile input.
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

impl VoiceHeader {
    /// The payload is a redundant audio bundle (see [`VOICE_FLAG_RED`]).
    #[must_use]
    pub fn is_red(&self) -> bool {
        self.flags & VOICE_FLAG_RED != 0
    }
}

/// Pack one Opus frame into a voice packet (flags = 0).
///
/// Returns `None` if the frame is empty or exceeds
/// [`VOICE_MAX_FRAME_SIZE`] — callers should drop such frames (and log),
/// never truncate audio.
#[must_use]
pub fn pack_voice(seq: u32, timestamp: u32, opus_frame: &[u8]) -> Option<Vec<u8>> {
    pack_voice_with_flags(seq, timestamp, 0, opus_frame)
}

/// Pack a payload with explicit flags — [`VOICE_FLAG_RED`] for a
/// redundant audio bundle. Same size rules as [`pack_voice`].
#[must_use]
pub fn pack_voice_with_flags(
    seq: u32,
    timestamp: u32,
    flags: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    if payload.is_empty() || payload.len() > VOICE_MAX_FRAME_SIZE {
        return None;
    }
    let mut out = Vec::with_capacity(VOICE_HEADER_SIZE + payload.len());
    out.push(VOICE_PROTOCOL_VERSION);
    out.push(flags & VOICE_FLAGS_KNOWN);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(payload);
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
    fn red_flag_roundtrips_and_plain_packets_are_not_red() {
        let plain = pack_voice(1, 2, &[0xAB; 8]).unwrap();
        assert!(!parse_voice(&plain).unwrap().0.is_red());

        let red = pack_voice_with_flags(1, 2, VOICE_FLAG_RED, &[0xCD; 8]).unwrap();
        let (header, payload) = parse_voice(&red).unwrap();
        assert!(header.is_red());
        assert_eq!(payload, &[0xCD; 8]);
    }

    #[test]
    fn unknown_flag_bits_are_not_sent() {
        let packet = pack_voice_with_flags(1, 2, 0xFF, &[1]).unwrap();
        assert_eq!(packet[1], VOICE_FLAGS_KNOWN);
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
