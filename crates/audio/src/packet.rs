//! Standardized audio packet with 48 kHz timestamps.

/// Header for an audio packet.
#[derive(Debug, Clone)]
pub struct PacketHeader {
    /// Monotonically increasing sequence number (wraps).
    pub seq: u32,
    /// 48 kHz sample-clock timestamp.
    pub timestamp: u32,
    /// Synchronization source — identifies the sender peer.
    pub ssrc: u64,
}

/// An encoded Opus audio packet ready for network transport.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub header: PacketHeader,
    pub opus_data: Vec<u8>,
}

impl AudioPacket {
    /// Create a new packet for local encoding.
    pub fn new(seq: u32, ts_48k: u32, ssrc: u64, opus_data: Vec<u8>) -> Self {
        Self {
            header: PacketHeader {
                seq,
                timestamp: ts_48k,
                ssrc,
            },
            opus_data,
        }
    }
}