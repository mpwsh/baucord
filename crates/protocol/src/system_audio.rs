//! System audio wire format (version 2) — screen-share audio.
//!
//! Carries Opus-encoded system audio (usually stereo) alongside a video
//! stream. Deliberately **not** gated by voice-chat deafen state: muting
//! the people in a call shouldn't silence the movie they're watching.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! offset  size  field
//! 0       1     version      (= 2)
//! 1       1     channel count (1 or 2)
//! 2       2     frame duration in ms
//! 4       4     sequence number
//! 8       —     Opus payload (delimited by the datagram)
//! ```
//!
//! v1 carried a sender id (now the relay envelope's job, see
//! [`crate::relayed`]) and an explicit payload length that duplicated
//! what the datagram boundary already provides. Both are gone: 20 bytes
//! of header down to 8.

/// Current system audio wire format version.
pub const SYSTEM_AUDIO_PROTOCOL_VERSION: u8 = 2;

/// Serialized header size in bytes.
pub const SYSTEM_AUDIO_HEADER_SIZE: usize = 8;

/// Maximum total packet size (header + payload, before the relay
/// envelope). Stereo Opus at high quality stays well under this.
pub const SYSTEM_AUDIO_MTU_BUDGET: usize = 1024;

/// Parsed system audio packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemAudioHeader {
    pub version: u8,
    /// Number of audio channels (1 = mono, 2 = stereo).
    pub channels: u8,
    /// Duration of this frame in milliseconds.
    pub duration_ms: u16,
    /// Monotonic per-sender packet sequence number.
    pub seq: u32,
}

/// Pack one Opus system-audio frame.
///
/// Returns `None` if the payload is empty or the packet would exceed
/// [`SYSTEM_AUDIO_MTU_BUDGET`].
#[must_use]
pub fn pack_system_audio(seq: u32, channels: u8, duration_ms: u16, opus: &[u8]) -> Option<Vec<u8>> {
    if opus.is_empty() || SYSTEM_AUDIO_HEADER_SIZE + opus.len() > SYSTEM_AUDIO_MTU_BUDGET {
        return None;
    }
    let mut out = Vec::with_capacity(SYSTEM_AUDIO_HEADER_SIZE + opus.len());
    out.push(SYSTEM_AUDIO_PROTOCOL_VERSION);
    out.push(channels);
    out.extend_from_slice(&duration_ms.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(opus);
    Some(out)
}

/// Parse a system audio packet from untrusted bytes.
#[must_use]
pub fn parse_system_audio(data: &[u8]) -> Option<(SystemAudioHeader, &[u8])> {
    if data.len() <= SYSTEM_AUDIO_HEADER_SIZE || data.len() > SYSTEM_AUDIO_MTU_BUDGET {
        return None;
    }
    if data[0] != SYSTEM_AUDIO_PROTOCOL_VERSION {
        return None;
    }
    let channels = data[1];
    if channels == 0 || channels > 2 {
        return None;
    }
    let header = SystemAudioHeader {
        version: data[0],
        channels,
        duration_ms: u16::from_le_bytes([data[2], data[3]]),
        seq: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
    };
    Some((header, &data[SYSTEM_AUDIO_HEADER_SIZE..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let payload = vec![0xCD; 240];
        let packet = pack_system_audio(7, 2, 20, &payload).unwrap();
        assert_eq!(packet.len(), SYSTEM_AUDIO_HEADER_SIZE + 240);

        let (header, parsed) = parse_system_audio(&packet).unwrap();
        assert_eq!(header.version, SYSTEM_AUDIO_PROTOCOL_VERSION);
        assert_eq!(header.channels, 2);
        assert_eq!(header.duration_ms, 20);
        assert_eq!(header.seq, 7);
        assert_eq!(parsed, &payload[..]);
    }

    #[test]
    fn empty_payload_refused() {
        assert!(pack_system_audio(0, 2, 20, &[]).is_none());
    }

    #[test]
    fn oversize_refused() {
        let payload = vec![0u8; SYSTEM_AUDIO_MTU_BUDGET];
        assert!(pack_system_audio(0, 2, 20, &payload).is_none());
    }

    #[test]
    fn bad_channel_count_rejected() {
        let mut packet = pack_system_audio(1, 2, 20, &[1, 2, 3]).unwrap();
        packet[1] = 0;
        assert!(parse_system_audio(&packet).is_none());
        packet[1] = 3;
        assert!(parse_system_audio(&packet).is_none());
    }

    #[test]
    fn wrong_version_rejected() {
        let mut packet = pack_system_audio(1, 2, 20, &[1, 2, 3]).unwrap();
        packet[0] = 1;
        assert!(parse_system_audio(&packet).is_none());
    }
}
