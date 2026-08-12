//! Control-plane message types and their serialization.
//!
//! Messages are serialized with `postcard` (compact varint encoding) and
//! travel on [`CONTROL_CHANNEL`] or [`SIGNAL_CHANNEL`] — each variant
//! knows its own channel via [`ClientMessage::channel`] /
//! [`ServerMessage::channel`], so call sites can't misroute.
//!
//! **Compatibility policy:** there is none within a wire version — this
//! is a v1 protocol with no deployed peers, and [`crate::PROTOCOL_ID`]
//! hard-gates mismatched versions at the handshake. Change these enums
//! freely, but bump `PROTOCOL_ID` when you do.

use std::collections::HashMap;

use renet::ClientId;
use serde::{Deserialize, Serialize};

use crate::channels::{CONTROL_CHANNEL, MEDIA_CHANNEL, SIGNAL_CHANNEL};

/// Upper bound for a serialized control message. Anything larger is
/// dropped before deserialization — a cheap guard against hostile length
/// games, independent of renet's channel memory cap.
///
/// Chat images are the one deliberate exception: their wire size is
/// governed by [`MAX_IMAGE_MESSAGE_BYTES`] instead, and the codec
/// enforces per-variant that *only* image messages may exceed this
/// bound (see [`decode_client_message`] / [`decode_server_message`]).
pub const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;

/// Maximum payload of a chat image (PNG) or animated GIF, enforced on
/// both ends: the sending client downscales/rejects until it fits, and
/// the codec refuses to encode or decode anything larger — so a
/// modified client can't spray relay memory. Sized for memes: static
/// images rarely approach this, animated GIFs do.
pub const MAX_CHAT_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Upper bound for a serialized chat-image message: the payload cap
/// plus generous headroom for the envelope (postcard varints + ids are
/// a handful of bytes). This is the pre-deserialization length guard
/// for the image path, and the worst transient allocation `decode_*`
/// can be tricked into.
pub const MAX_IMAGE_MESSAGE_BYTES: usize = MAX_CHAT_IMAGE_BYTES + 1024;

// ── Shared payload types ────────────────────────────────────────────────

/// A text message attributed to a specific client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub client_id: ClientId,
    pub text: String,
}

/// An image chat message attributed to a specific client.
///
/// `data` is an encoded PNG or animated GIF (receivers sniff the magic
/// bytes) no larger than [`MAX_CHAT_IMAGE_BYTES`]; the codec refuses
/// oversized payloads in both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatImageMessage {
    /// Authoritative sender id, stamped by the server on relay.
    pub client_id: ClientId,
    /// Encoded PNG bytes.
    pub data: Vec<u8>,
}

/// Why the server refused a screen-share request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DenyReason {
    /// The server's concurrent-sharer cap is reached.
    ShareLimitReached,
    /// The stream already has the maximum number of viewers.
    ViewerLimitReached,
    /// The subscribe target is not currently sharing.
    NotSharing,
    /// The announced share dimensions were out of range.
    InvalidDimensions,
}

impl DenyReason {
    /// Human-readable form for UI display.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            DenyReason::ShareLimitReached => "the server's screen share limit is reached",
            DenyReason::ViewerLimitReached => "this stream is at its viewer limit",
            DenyReason::NotSharing => "that user is not sharing their screen",
            DenyReason::InvalidDimensions => "the share dimensions were rejected",
        }
    }
}

// ── Client → Server ─────────────────────────────────────────────────────

/// Messages sent from a client to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Send a text chat message to the room.
    Chat(String),

    /// Our local speaking state changed (edge-triggered).
    Speaking(bool),

    /// Broadcast our mute/deafen state to other users. The server also
    /// uses `deafened` to stop relaying voice to us entirely.
    AudioState { muted: bool, deafened: bool },

    // ── Screen share signaling ──────────────────────────────────────
    /// We started sharing our screen. Dimensions are the quality preset
    /// bounds; per-frame actual dimensions travel in the video headers.
    ScreenShareStart { width: u16, height: u16 },

    /// We stopped sharing our screen.
    ScreenShareStop,

    /// Subscribe to another user's screen share stream.
    SubscribeScreen { target_id: ClientId },

    /// Unsubscribe from another user's screen share stream.
    UnsubscribeScreen { target_id: ClientId },

    /// Request a fresh keyframe from a sharer we are subscribed to
    /// (viewer → server → sharer). Travels on the signal channel so it
    /// can never queue behind chat.
    RequestKeyframe { target_id: ClientId },

    // ── Chat, continued ─────────────────────────────────────────────
    // (Appended after the original variants so existing postcard
    // discriminants are untouched.)
    /// Send an image chat message (encoded PNG, ≤
    /// [`MAX_CHAT_IMAGE_BYTES`]) to the room.
    ChatImage(Vec<u8>),
}

impl ClientMessage {
    /// The channel this message must be sent on.
    #[must_use]
    pub fn channel(&self) -> u8 {
        match self {
            ClientMessage::RequestKeyframe { .. } => SIGNAL_CHANNEL,
            // Bulk media rides its own reliable-unordered channel so it
            // can never head-of-line block ordered chat text.
            ClientMessage::ChatImage(_) => MEDIA_CHANNEL,
            _ => CONTROL_CHANNEL,
        }
    }
}

// ── Server → Client ─────────────────────────────────────────────────────

/// Messages sent from the server to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Initialization payload for a newly connected client: the current
    /// user list, our authoritative client id, server name and greeting.
    Init {
        usernames: HashMap<ClientId, String>,
        client_id: ClientId,
        server_name: String,
        greeting: String,
    },

    /// A new client connected.
    UserJoined {
        client_id: ClientId,
        username: String,
    },

    /// A client disconnected.
    UserLeft { client_id: ClientId },

    /// A chat message from a client.
    Chat(ChatMessage),

    /// A client's speaking state changed.
    Speaking {
        client_id: ClientId,
        is_speaking: bool,
    },

    /// A client's mute/deafen state changed.
    AudioState {
        client_id: ClientId,
        muted: bool,
        deafened: bool,
    },

    // ── Screen share signaling ──────────────────────────────────────
    /// A client started sharing their screen.
    ScreenShareStarted {
        client_id: ClientId,
        width: u16,
        height: u16,
    },

    /// A client stopped sharing their screen.
    ScreenShareStopped { client_id: ClientId },

    /// The server refused our `ScreenShareStart`.
    ShareDenied { reason: DenyReason },

    /// The server refused our `SubscribeScreen`.
    SubscribeDenied {
        target_id: ClientId,
        reason: DenyReason,
    },

    /// A viewer needs a keyframe from us — force an IDR.
    /// Signal channel: must not queue behind chat.
    KeyframeRequested { viewer_id: ClientId },

    /// Periodic health report for a stream we are sharing, aggregated
    /// across our viewers. Drives sender-side rate adaptation.
    /// Signal channel; stale reports are harmless (newer overwrite).
    StreamHealth {
        /// Worst observed packet loss among viewers, 0.0–1.0.
        worst_loss: f32,
        /// Worst observed round-trip time among viewers, in ms.
        worst_rtt_ms: f32,
        /// Number of connected viewers.
        viewers: u16,
    },

    // ── Chat, continued ─────────────────────────────────────────────
    // (Appended after the original variants so existing postcard
    // discriminants are untouched.)
    /// An image chat message from a client.
    ChatImage(ChatImageMessage),
}

impl ServerMessage {
    /// The channel this message must be sent on.
    #[must_use]
    pub fn channel(&self) -> u8 {
        match self {
            ServerMessage::KeyframeRequested { .. } | ServerMessage::StreamHealth { .. } => {
                SIGNAL_CHANNEL
            }
            ServerMessage::ChatImage(_) => MEDIA_CHANNEL,
            _ => CONTROL_CHANNEL,
        }
    }
}

// ── Codec ───────────────────────────────────────────────────────────────
//
// One encode/decode pair per direction, used by both binaries. Errors
// are logged via the caller (these return Option so hot loops can just
// skip malformed input).
//
// Length discipline lives here so client and server can't drift:
// pre-decode, anything over MAX_IMAGE_MESSAGE_BYTES is dropped unread
// (bounding the worst transient allocation); post-decode, only image
// variants may exceed MAX_CONTROL_MESSAGE_BYTES, and their payload must
// fit MAX_CHAT_IMAGE_BYTES. Encode applies the same caps so a buggy
// caller fails locally instead of poisoning the wire.

/// Does this client message respect the length budget for its variant?
fn client_message_len_ok(msg: &ClientMessage, wire_len: usize) -> bool {
    match msg {
        ClientMessage::ChatImage(data) => data.len() <= MAX_CHAT_IMAGE_BYTES,
        _ => wire_len <= MAX_CONTROL_MESSAGE_BYTES,
    }
}

/// Does this server message respect the length budget for its variant?
fn server_message_len_ok(msg: &ServerMessage, wire_len: usize) -> bool {
    match msg {
        ServerMessage::ChatImage(img) => img.data.len() <= MAX_CHAT_IMAGE_BYTES,
        _ => wire_len <= MAX_CONTROL_MESSAGE_BYTES,
    }
}

/// Serialize a [`ClientMessage`]. `None` on serializer failure (should
/// never happen for these types; treated as a droppable event) or if the
/// message exceeds its variant's length budget.
#[must_use]
pub fn encode_client_message(msg: &ClientMessage) -> Option<Vec<u8>> {
    let bytes = postcard::to_allocvec(msg).ok()?;
    client_message_len_ok(msg, bytes.len()).then_some(bytes)
}

/// Deserialize a [`ClientMessage`] from untrusted bytes.
#[must_use]
pub fn decode_client_message(bytes: &[u8]) -> Option<ClientMessage> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_MESSAGE_BYTES {
        return None;
    }
    let msg: ClientMessage = postcard::from_bytes(bytes).ok()?;
    client_message_len_ok(&msg, bytes.len()).then_some(msg)
}

/// Serialize a [`ServerMessage`]. `None` on serializer failure or if the
/// message exceeds its variant's length budget.
#[must_use]
pub fn encode_server_message(msg: &ServerMessage) -> Option<Vec<u8>> {
    let bytes = postcard::to_allocvec(msg).ok()?;
    server_message_len_ok(msg, bytes.len()).then_some(bytes)
}

/// Deserialize a [`ServerMessage`] from untrusted bytes.
#[must_use]
pub fn decode_server_message(bytes: &[u8]) -> Option<ServerMessage> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_MESSAGE_BYTES {
        return None;
    }
    let msg: ServerMessage = postcard::from_bytes(bytes).ok()?;
    server_message_len_ok(&msg, bytes.len()).then_some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_roundtrips() {
        let msg = ClientMessage::Chat("hello".to_string());
        let bytes = encode_client_message(&msg).unwrap();
        let back = decode_client_message(&bytes).unwrap();
        assert!(matches!(back, ClientMessage::Chat(t) if t == "hello"));
    }

    #[test]
    fn server_message_roundtrips() {
        let msg = ServerMessage::StreamHealth {
            worst_loss: 0.05,
            worst_rtt_ms: 42.0,
            viewers: 3,
        };
        let bytes = encode_server_message(&msg).unwrap();
        let back = decode_server_message(&bytes).unwrap();
        assert!(matches!(
            back,
            ServerMessage::StreamHealth { viewers: 3, .. }
        ));
    }

    #[test]
    fn oversize_control_message_rejected() {
        let bytes = vec![0u8; MAX_CONTROL_MESSAGE_BYTES + 1];
        assert!(decode_client_message(&bytes).is_none());
        assert!(decode_server_message(&bytes).is_none());
    }

    #[test]
    fn garbage_rejected() {
        assert!(decode_server_message(&[0xFF, 0xFF, 0xFF, 0xFF]).is_none());
        assert!(decode_client_message(&[]).is_none());
    }

    #[test]
    fn chat_image_roundtrips_past_the_control_cap() {
        // 200 KB: comfortably over MAX_CONTROL_MESSAGE_BYTES, under the
        // image budget — the whole point of the split.
        let payload = vec![0xA5u8; 200 * 1024];
        let msg = ClientMessage::ChatImage(payload.clone());
        let bytes = encode_client_message(&msg).unwrap();
        assert!(bytes.len() > MAX_CONTROL_MESSAGE_BYTES);
        let back = decode_client_message(&bytes).unwrap();
        assert!(matches!(back, ClientMessage::ChatImage(d) if d == payload));

        let msg = ServerMessage::ChatImage(ChatImageMessage {
            client_id: 7,
            data: payload.clone(),
        });
        let bytes = encode_server_message(&msg).unwrap();
        let back = decode_server_message(&bytes).unwrap();
        assert!(matches!(
            back,
            ServerMessage::ChatImage(img) if img.client_id == 7 && img.data == payload
        ));
    }

    #[test]
    fn oversized_image_refused_both_ways() {
        let too_big = vec![0u8; MAX_CHAT_IMAGE_BYTES + 1];

        // Encode refuses locally…
        assert!(encode_client_message(&ClientMessage::ChatImage(too_big.clone())).is_none());
        assert!(
            encode_server_message(&ServerMessage::ChatImage(ChatImageMessage {
                client_id: 1,
                data: too_big.clone(),
            }))
            .is_none()
        );

        // …and a hostile peer that serializes it anyway is dropped at
        // decode (post-decode payload check).
        let raw = postcard::to_allocvec(&ClientMessage::ChatImage(too_big)).unwrap();
        assert!(decode_client_message(&raw).is_none());
    }

    #[test]
    fn huge_non_image_message_still_rejected() {
        // The relaxed pre-decode bound must not open the door for other
        // variants to balloon: a 500 KB text chat is refused post-decode.
        let msg = ClientMessage::Chat("x".repeat(500 * 1024));
        assert!(encode_client_message(&msg).is_none());
        let raw = postcard::to_allocvec(&msg).unwrap();
        assert!(decode_client_message(&raw).is_none());
    }

    #[test]
    fn chat_image_uses_control_channel() {
        assert_eq!(ClientMessage::ChatImage(Vec::new()).channel(), CONTROL_CHANNEL);
        assert_eq!(
            ServerMessage::ChatImage(ChatImageMessage {
                client_id: 0,
                data: Vec::new(),
            })
            .channel(),
            CONTROL_CHANNEL
        );
    }

    #[test]
    fn keyframe_messages_use_signal_channel() {
        assert_eq!(
            ClientMessage::RequestKeyframe { target_id: 1 }.channel(),
            SIGNAL_CHANNEL
        );
        assert_eq!(
            ServerMessage::KeyframeRequested { viewer_id: 1 }.channel(),
            SIGNAL_CHANNEL
        );
        assert_eq!(
            ClientMessage::Chat(String::new()).channel(),
            CONTROL_CHANNEL
        );
    }
}
