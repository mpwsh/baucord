//! Lane table: the application-level channels and their QUIC carriers.
//!
//! Both ends program against `send(peer, lane, bytes)`; this module is
//! the single description of what each lane rides on, so client and
//! server can never disagree about it.
//!
//! | lane          | carrier                       | reliability                 | framing           |
//! |---------------|-------------------------------|-----------------------------|-------------------|
//! | `Control`     | one bidi stream, client-opened| reliable, ordered           | u32 length prefix |
//! | `Signal`      | one bidi stream, client-opened| reliable, ordered           | u32 length prefix |
//! | `Voice`       | DATAGRAM frames               | unreliable                  | one packet each   |
//! | `SystemAudio` | DATAGRAM frames               | unreliable                  | one packet each   |
//! | `Video`       | one uni stream per *group*    | reliable within a group;    | u32 length prefix |
//! |               | (opened at each keyframe)     | a group is droppable        |                   |
//! | `Media`       | one uni stream per upload     | reliable                    | u32 length prefix |
//!
//! Every stream begins with one **lane tag** byte ([`Lane::tag`]) so the
//! receiver can route it without relying on stream-id ordering, and every
//! datagram is prefixed with the same tag so Voice and `SystemAudio` can
//! share the DATAGRAM carrier (their headers both start with a version
//! byte and are otherwise indistinguishable). One byte per packet is the
//! whole cost.
//!
//! Signal is a separate stream from Control so a subscribe or keyframe
//! request never queues behind chat; Media is a separate stream *per
//! upload* so a 10 MB image never head-of-line blocks text; Video is a
//! stream per group so a viewer that falls behind loses one GOP (the
//! group is `reset`) instead of accumulating retransmissions.
//!
//! Versioning: [`ALPN`] plays the role the old `PROTOCOL_ID` did — a peer
//! offering another ALPN is refused inside the TLS handshake. Bump it on
//! every breaking wire change; additive changes ride on
//! [`crate::features`] bits instead.

/// ALPN token negotiated in the TLS handshake. `baucord/<wire version>`.
pub const ALPN: &[u8] = b"baucord/1";

/// User-facing text when the peer's ALPN does not match ours.
pub const OUTDATED_MESSAGE: &str = "Client outdated — download the latest version from baucord.com";

/// Application lanes. The discriminant is the on-wire tag byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lane {
    /// Chat text, presence, share signaling, Hello/Welcome, reports.
    Control = 0,
    /// Subscribes, keyframe requests, channel joins.
    Signal = 1,
    /// Opus voice packets.
    Voice = 2,
    /// Opus stereo screen-share audio.
    SystemAudio = 3,
    /// H.264 chunks, grouped per keyframe.
    Video = 4,
    /// Chat images / GIFs and log uploads, one stream per upload.
    Media = 5,
}

impl Lane {
    /// Every lane, in send-priority order (first = most urgent).
    #[cfg(test)]
    pub const ALL: [Lane; 6] = [
        Lane::Voice,
        Lane::SystemAudio,
        Lane::Signal,
        Lane::Control,
        Lane::Video,
        Lane::Media,
    ];

    /// The tag byte that prefixes every stream and datagram on this lane.
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// Parse a tag byte.
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Lane> {
        Some(match tag {
            0 => Lane::Control,
            1 => Lane::Signal,
            2 => Lane::Voice,
            3 => Lane::SystemAudio,
            4 => Lane::Video,
            5 => Lane::Media,
            _ => return None,
        })
    }

    /// Stream priority handed to the transport (`set_priority`; higher
    /// is scheduled first). Datagram lanes have no stream and are
    /// written ahead of all stream data by the transport anyway.
    #[must_use]
    pub const fn priority(self) -> i32 {
        match self {
            Lane::Signal => STREAM_PRIORITY_SIGNAL,
            Lane::Control => STREAM_PRIORITY_CONTROL,
            Lane::Video => STREAM_PRIORITY_VIDEO,
            Lane::Media => STREAM_PRIORITY_MEDIA,
            Lane::Voice | Lane::SystemAudio => 0,
        }
    }

    /// Largest frame the receiver accepts on this lane (length-prefixed
    /// lanes) or largest datagram payload (datagram lanes), *excluding*
    /// the tag byte. A frame claiming more is a protocol violation and
    /// the stream is stopped.
    #[must_use]
    pub const fn max_frame_bytes(self) -> usize {
        match self {
            Lane::Control | Lane::Signal => crate::MAX_CONTROL_MESSAGE_BYTES,
            Lane::Media => crate::MAX_IMAGE_MESSAGE_BYTES,
            Lane::Video => VIDEO_FRAME_BUDGET,
            Lane::Voice => VOICE_DATAGRAM_BUDGET,
            Lane::SystemAudio => SYSTEM_AUDIO_DATAGRAM_BUDGET,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Lane::Control => "control",
            Lane::Signal => "signal",
            Lane::Voice => "voice",
            Lane::SystemAudio => "system_audio",
            Lane::Video => "video",
            Lane::Media => "media",
        }
    }
}

impl std::fmt::Display for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ── Stream priorities ───────────────────────────────────────────────────

/// Signal outranks everything: a keyframe request is a handful of bytes
/// that unblocks a whole stream.
pub const STREAM_PRIORITY_SIGNAL: i32 = 20;
/// Control (chat, presence) next.
pub const STREAM_PRIORITY_CONTROL: i32 = 10;
/// Video groups.
pub const STREAM_PRIORITY_VIDEO: i32 = 0;
/// Bulk media last — the right loser when bytes are scarce.
pub const STREAM_PRIORITY_MEDIA: i32 = -10;

// ── Per-lane byte budgets ───────────────────────────────────────────────

/// Bytes of relay envelope the server prepends when forwarding media
/// (`[sender_id: u64]`, see [`crate::relayed`]).
const RELAY_ENVELOPE: usize = crate::RELAY_SENDER_BYTES;

/// Largest voice datagram payload, either direction: the relay envelope
/// plus a full voice packet ([`crate::VOICE_MTU_BUDGET`]).
pub const VOICE_DATAGRAM_BUDGET: usize = RELAY_ENVELOPE + crate::VOICE_MTU_BUDGET;

/// Largest system-audio datagram payload.
pub const SYSTEM_AUDIO_DATAGRAM_BUDGET: usize = RELAY_ENVELOPE + crate::SYSTEM_AUDIO_MTU_BUDGET;

/// Largest video frame on a group stream: one chunk with its relay
/// envelope. Chunks are ≤ 1096 B, so this is a hard cap on hostile input
/// rather than a working size.
pub const VIDEO_FRAME_BUDGET: usize =
    RELAY_ENVELOPE + crate::VIDEO_HEADER_SIZE + crate::VIDEO_CHUNK_PAYLOAD;

/// Every datagram lane must fit a 1200-byte QUIC datagram with room for
/// the packet header, ACK/other frames, and the DATAGRAM frame's own
/// length field. `quinn-proto` reports the exact limit at runtime; this
/// is the compile-time sanity bound on the *application* side and
/// already accounts for the one-byte lane tag.
pub const DATAGRAM_PAYLOAD_LIMIT: usize = 1100;

const _: () = assert!(VOICE_DATAGRAM_BUDGET < DATAGRAM_PAYLOAD_LIMIT);
const _: () = assert!(SYSTEM_AUDIO_DATAGRAM_BUDGET < DATAGRAM_PAYLOAD_LIMIT);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_roundtrip_for_every_lane() {
        for lane in Lane::ALL {
            assert_eq!(Lane::from_tag(lane.tag()), Some(lane));
        }
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert_eq!(Lane::from_tag(6), None);
        assert_eq!(Lane::from_tag(255), None);
    }

    #[test]
    fn all_lists_each_lane_once() {
        let mut tags: Vec<u8> = Lane::ALL.iter().map(|l| l.tag()).collect();
        tags.sort_unstable();
        assert_eq!(tags, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn datagram_lanes_fit_the_datagram_budget() {
        assert!(Lane::Voice.max_frame_bytes() < DATAGRAM_PAYLOAD_LIMIT);
        assert!(Lane::SystemAudio.max_frame_bytes() < DATAGRAM_PAYLOAD_LIMIT);
    }

    #[test]
    fn signal_outranks_control_which_outranks_bulk() {
        assert!(Lane::Signal.priority() > Lane::Control.priority());
        assert!(Lane::Control.priority() > Lane::Video.priority());
        assert!(Lane::Video.priority() > Lane::Media.priority());
    }
}
