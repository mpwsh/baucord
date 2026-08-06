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

use crate::channels::{CONTROL_CHANNEL, SIGNAL_CHANNEL};

/// Upper bound for a serialized control message. Anything larger is
/// dropped before deserialization — a cheap guard against hostile length
/// games, independent of renet's channel memory cap.
pub const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;

// ── Shared payload types ────────────────────────────────────────────────

/// A text message attributed to a specific client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub client_id: ClientId,
    pub text: String,
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
}

impl ClientMessage {
    /// The channel this message must be sent on.
    #[must_use]
    pub fn channel(&self) -> u8 {
        match self {
            ClientMessage::RequestKeyframe { .. } => SIGNAL_CHANNEL,
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
}

impl ServerMessage {
    /// The channel this message must be sent on.
    #[must_use]
    pub fn channel(&self) -> u8 {
        match self {
            ServerMessage::KeyframeRequested { .. } | ServerMessage::StreamHealth { .. } => {
                SIGNAL_CHANNEL
            }
            _ => CONTROL_CHANNEL,
        }
    }
}

// ── Codec ───────────────────────────────────────────────────────────────
//
// One encode/decode pair per direction, used by both binaries. Errors
// are logged via the caller (these return Option so hot loops can just
// skip malformed input).

/// Serialize a [`ClientMessage`]. `None` on serializer failure (should
/// never happen for these types; treated as a droppable event).
#[must_use]
pub fn encode_client_message(msg: &ClientMessage) -> Option<Vec<u8>> {
    postcard::to_allocvec(msg).ok()
}

/// Deserialize a [`ClientMessage`] from untrusted bytes.
#[must_use]
pub fn decode_client_message(bytes: &[u8]) -> Option<ClientMessage> {
    if bytes.is_empty() || bytes.len() > MAX_CONTROL_MESSAGE_BYTES {
        return None;
    }
    postcard::from_bytes(bytes).ok()
}

/// Serialize a [`ServerMessage`].
#[must_use]
pub fn encode_server_message(msg: &ServerMessage) -> Option<Vec<u8>> {
    postcard::to_allocvec(msg).ok()
}

/// Deserialize a [`ServerMessage`] from untrusted bytes.
#[must_use]
pub fn decode_server_message(bytes: &[u8]) -> Option<ServerMessage> {
    if bytes.is_empty() || bytes.len() > MAX_CONTROL_MESSAGE_BYTES {
        return None;
    }
    postcard::from_bytes(bytes).ok()
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
        assert!(matches!(back, ServerMessage::StreamHealth { viewers: 3, .. }));
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
    fn keyframe_messages_use_signal_channel() {
        assert_eq!(
            ClientMessage::RequestKeyframe { target_id: 1 }.channel(),
            SIGNAL_CHANNEL
        );
        assert_eq!(
            ServerMessage::KeyframeRequested { viewer_id: 1 }.channel(),
            SIGNAL_CHANNEL
        );
        assert_eq!(ClientMessage::Chat(String::new()).channel(), CONTROL_CHANNEL);
    }
}
