//! The relay sender envelope.
//!
//! Media packets (voice / video / system audio) carry **no sender id**
//! from client to server — the server already knows who each datagram
//! came from via the authenticated QUIC connection. When relaying, the
//! server prepends the sender's transport id:
//!
//! ```text
//! client → server:   [media packet]
//! server → client:   [sender_id: u64 LE][media packet]
//! ```
//!
//! Consequences, all deliberate:
//! - **Spoofing is impossible.** Receivers attribute media to the id the
//!   server stamped, never to a value the sending client chose.
//! - Upstream packets are 8 bytes smaller — on the busiest per-packet
//!   path (voice at 100 packets/s), that's 6.4 kbit/s saved per speaker.
//! - Exactly one attribution mechanism exists, instead of a per-format
//!   id field that each parser validated differently.

/// Size of the sender id prefix on relayed (server → client) media.
pub const RELAY_SENDER_BYTES: usize = 8;

/// Prepend `sender_id` to a media packet for relay to receivers.
#[must_use]
pub fn wrap_sender(sender_id: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RELAY_SENDER_BYTES + payload.len());
    out.extend_from_slice(&sender_id.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Split a relayed packet into `(sender_id, media packet)`.
///
/// Returns `None` if the packet is too short to contain the prefix.
#[must_use]
pub fn split_sender(data: &[u8]) -> Option<(u64, &[u8])> {
    if data.len() < RELAY_SENDER_BYTES {
        return None;
    }
    let (id_bytes, rest) = data.split_at(RELAY_SENDER_BYTES);
    let mut id = [0u8; RELAY_SENDER_BYTES];
    id.copy_from_slice(id_bytes);
    Some((u64::from_le_bytes(id), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let wrapped = wrap_sender(0xDEAD_BEEF_CAFE_F00D, b"payload");
        let (id, payload) = split_sender(&wrapped).unwrap();
        assert_eq!(id, 0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(payload, b"payload");
    }

    #[test]
    fn empty_payload_roundtrips() {
        let wrapped = wrap_sender(7, b"");
        let (id, payload) = split_sender(&wrapped).unwrap();
        assert_eq!(id, 7);
        assert!(payload.is_empty());
    }

    #[test]
    fn short_input_rejected() {
        assert!(split_sender(&[1, 2, 3]).is_none());
        assert!(split_sender(&[]).is_none());
    }
}
