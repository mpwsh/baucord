//! Standardized audio packet with 48 kHz timestamps.

/// What the payload bytes are.
///
/// Serialised on the wire as one byte by the application (protocol
/// bump in 0.8.0); unknown values must be treated as [`PayloadType::Opus`]
/// by old receivers only if the sender never enables RED, so a receiver
/// that can't parse the type should be told not to receive RED at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PayloadType {
    /// One Opus packet (any frame size Opus allows).
    #[default]
    Opus = 0,
    /// Redundant audio (RFC 2198-shaped, see [`crate::transport::red`]):
    /// a primary Opus packet plus full copies of one or two preceding
    /// packets.
    Red = 1,
}

impl PayloadType {
    /// Decode the wire byte. `None` for values this version doesn't know.
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Opus),
            1 => Some(Self::Red),
            _ => None,
        }
    }
}

/// Header for an audio packet.
#[derive(Debug, Clone)]
pub struct PacketHeader {
    /// Monotonically increasing sequence number (wraps).
    pub seq: u32,
    /// 48 kHz sample-clock timestamp.
    pub timestamp: u32,
    /// Synchronization source — identifies the sender peer.
    pub ssrc: u64,
    /// What `opus_data` holds.
    pub payload_type: PayloadType,
}

/// An encoded audio packet ready for network transport.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub header: PacketHeader,
    /// Payload bytes: an Opus packet, or a RED bundle (see `payload_type`).
    pub opus_data: Vec<u8>,
}

impl AudioPacket {
    /// Create a plain Opus packet.
    #[must_use]
    pub fn new(seq: u32, ts_48k: u32, ssrc: u64, opus_data: Vec<u8>) -> Self {
        Self::new_typed(seq, ts_48k, ssrc, PayloadType::Opus, opus_data)
    }

    /// Create a packet with an explicit payload type (what a receiver
    /// should call after reading the type byte off the wire).
    #[must_use]
    pub fn new_typed(
        seq: u32,
        ts_48k: u32,
        ssrc: u64,
        payload_type: PayloadType,
        opus_data: Vec<u8>,
    ) -> Self {
        Self {
            header: PacketHeader {
                seq,
                timestamp: ts_48k,
                ssrc,
                payload_type,
            },
            opus_data,
        }
    }
}
