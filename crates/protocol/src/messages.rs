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

/// Maximum payload of a chat image (PNG, JPEG, or animated GIF),
/// enforced on both ends: the sending client downscales/rejects until
/// it fits, and both reassembly paths refuse anything larger — so a
/// modified client can't spray relay memory. Sized for memes: static
/// images rarely approach this, animated GIFs do.
///
/// Images never travel as one wire message: they are sliced into
/// [`IMAGE_CHUNK_BYTES`] chunks and *paced* onto the wire (see
/// [`MEDIA_UPLOAD_PACE`] / server relay pacing), so bulk media never
/// floods the queues that voice shares — that flooding, not raw size,
/// is what breaks calls during uploads.
pub const MAX_CHAT_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Upper bound for a serialized chat-image message: the payload cap
/// plus generous headroom for the envelope (postcard varints + ids are
/// a handful of bytes). This is the pre-deserialization length guard
/// for the image path, and the worst transient allocation `decode_*`
/// can be tricked into.
pub const MAX_IMAGE_MESSAGE_BYTES: usize = IMAGE_CHUNK_BYTES + 1024;

/// Payload of one image chunk. Small on purpose: a voice packet that
/// lands mid-chunk waits at most `chunk / uplink_rate` behind it
/// (16 KiB at 5 Mbit/s ≈ 25 ms — inside what the jitter buffer
/// absorbs), where a whole 10 MiB message would park voice behind
/// seconds of queue.
pub const IMAGE_CHUNK_BYTES: usize = 16 * 1024;

/// Most chunks a single image may span (ceil of the size cap, plus one
/// for the remainder). Reassemblers refuse anything claiming more.
#[allow(clippy::cast_possible_truncation)] // 641 with current constants
pub const MAX_IMAGE_CHUNKS: u32 =
    (MAX_CHAT_IMAGE_BYTES / IMAGE_CHUNK_BYTES + 1) as u32;

/// Client upload pacing for image chunks, bytes per second. Chosen to
/// sit under typical home uplinks so the router queue stays shallow
/// and voice never waits behind bulk: a 1 MiB image takes ~2 s, the
/// 10 MiB worst case ~20 s (with live progress in the UI).
pub const MEDIA_UPLOAD_PACE: usize = 512 * 1024;

// ── Shared payload types ────────────────────────────────────────────────

/// A text message attributed to a specific client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub client_id: ClientId,
    pub text: String,
}

/// An image chat message attributed to a specific client.
///
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
    /// The stream is channel-scoped and the viewer is in a different
    /// voice channel.
    NotInChannel,
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
            DenyReason::NotInChannel => "this stream is only visible in its voice channel",
        }
    }
}

// ── Voice channels (rooms) ──────────────────────────────────────────────
//
// Not to be confused with renet transport channels (`crate::channels`):
// a voice channel is a server-defined *room* that scopes voice routing
// and (optionally) screen-share visibility. Rooms are static per server
// run, defined in server.yaml, and identified on the wire by their
// index in that list.

/// One voice channel (room) as announced to clients in
/// [`ServerMessage::Init`]. The password itself never crosses the wire
/// server→client — only the fact that one is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Stable id for this server run: the index in the config list.
    pub id: u16,
    pub name: String,
    /// Whether [`ClientMessage::JoinChannel`] must carry a password.
    pub has_password: bool,
    /// Maximum members, `None` = unlimited. The default channel never
    /// has a limit (enforced by server config validation).
    pub user_limit: Option<u16>,
}

/// Who may see (and subscribe to) a screen share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareScope {
    /// Only members of the sharer's current voice channel. The stream
    /// follows the sharer across channel moves.
    Channel,
    /// Every user on the server, regardless of voice channel.
    Everyone,
}

/// Why the server refused a [`ClientMessage::JoinChannel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelDenyReason {
    /// The channel id does not exist on this server.
    NoSuchChannel,
    /// The channel requires a password and the supplied one was missing
    /// or wrong.
    WrongPassword,
    /// The channel is at its user limit.
    ChannelFull,
}

impl ChannelDenyReason {
    /// Human-readable form for UI display.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            ChannelDenyReason::NoSuchChannel => "that channel does not exist",
            ChannelDenyReason::WrongPassword => "wrong channel password",
            ChannelDenyReason::ChannelFull => "the channel is full",
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
    /// `scope` decides who may see and subscribe to the stream.
    ScreenShareStart {
        width: u16,
        height: u16,
        scope: ShareScope,
    },

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
    /// One slice of a chat image upload (PNG/JPEG/GIF, total ≤
    /// [`MAX_CHAT_IMAGE_BYTES`]). The client paces these onto the wire
    /// at [`MEDIA_UPLOAD_PACE`]; the server reassembles, validates,
    /// and relays the image re-chunked and paced per receiver. A new
    /// `upload_id` from the same client abandons any incomplete one.
    ChatImageChunk {
        /// Sender-scoped upload identifier (wrapping counter).
        upload_id: u32,
        /// 0-based chunk index.
        index: u32,
        /// Total chunks in this upload; constant across its chunks and
        /// bounded by [`MAX_IMAGE_CHUNKS`].
        count: u32,
        data: Vec<u8>,
    },

    // ── Voice channels ──────────────────────────────────────────────
    /// Move to another voice channel. `password` is required when the
    /// channel's [`ChannelInfo::has_password`] is set. The server
    /// answers with [`ServerMessage::UserMovedChannel`] (broadcast,
    /// including to us) or [`ServerMessage::JoinChannelDenied`].
    JoinChannel {
        channel_id: u16,
        password: Option<String>,
    },
}

impl ClientMessage {
    /// The channel this message must be sent on.
    #[must_use]
    pub fn channel(&self) -> u8 {
        match self {
            ClientMessage::RequestKeyframe { .. } => SIGNAL_CHANNEL,
            // Bulk media rides its own reliable-unordered channel so it
            // can never head-of-line block ordered chat text.
            ClientMessage::ChatImageChunk { .. } => MEDIA_CHANNEL,
            _ => CONTROL_CHANNEL,
        }
    }
}

// ── Server → Client ─────────────────────────────────────────────────────

/// Messages sent from the server to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Initialization payload for a newly connected client: the current
    /// user list, our authoritative client id, server name and greeting,
    /// plus the voice-channel roster: every channel this server defines
    /// and which channel each connected user (including us) is in.
    Init {
        usernames: HashMap<ClientId, String>,
        client_id: ClientId,
        server_name: String,
        greeting: String,
        channels: Vec<ChannelInfo>,
        memberships: HashMap<ClientId, u16>,
    },

    /// A new client connected (always into the default channel).
    UserJoined {
        client_id: ClientId,
        username: String,
        channel_id: u16,
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
    /// A client started sharing their screen. Channel-scoped shares are
    /// only announced to the sharer's channel; receiving this means the
    /// stream is watchable from where you are.
    ScreenShareStarted {
        client_id: ClientId,
        width: u16,
        height: u16,
        scope: ShareScope,
    },

    /// A client stopped sharing their screen — or, for channel-scoped
    /// streams, the stream became invisible to *you* (the sharer or you
    /// moved channels). Either way: clear the LIVE badge and close any
    /// viewer window for this stream.
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
    /// One slice of a relayed chat image. The server paces these per
    /// receiver so a big image never floods anyone's downlink queue —
    /// the receiving side of the same voice-protection scheme as
    /// [`ClientMessage::ChatImageChunk`].
    ChatImageChunk {
        /// The client whose image this is.
        client_id: ClientId,
        /// The sender's upload id — `(client_id, upload_id)` keys the
        /// receiver's reassembly.
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    },

    // ── Voice channels ──────────────────────────────────────────────
    /// A client moved to another voice channel. Broadcast to everyone
    /// including the mover — the UI treats the echo as authoritative
    /// for its own position (same contract as `Speaking`).
    UserMovedChannel {
        client_id: ClientId,
        channel_id: u16,
    },

    /// The server refused our `JoinChannel`.
    JoinChannelDenied {
        channel_id: u16,
        reason: ChannelDenyReason,
    },

    /// The server's channel list changed (hot-reloaded server.yaml).
    /// Carries the full new list *and* authoritative memberships —
    /// channel ids are list indexes, so a reload renumbers them and
    /// clients must resync wholesale rather than patch incrementally.
    ChannelListUpdated {
        channels: Vec<ChannelInfo>,
        memberships: HashMap<ClientId, u16>,
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
            ServerMessage::ChatImageChunk { .. } => MEDIA_CHANNEL,
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
        ClientMessage::ChatImageChunk { data, count, .. } => {
            data.len() <= IMAGE_CHUNK_BYTES && *count <= MAX_IMAGE_CHUNKS
        }
        _ => wire_len <= MAX_CONTROL_MESSAGE_BYTES,
    }
}

/// Does this server message respect the length budget for its variant?
fn server_message_len_ok(msg: &ServerMessage, wire_len: usize) -> bool {
    match msg {
        ServerMessage::ChatImageChunk { data, count, .. } => {
            data.len() <= IMAGE_CHUNK_BYTES && *count <= MAX_IMAGE_CHUNKS
        }
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
    fn image_chunk_roundtrips_both_directions() {
        let data = vec![0xA5u8; IMAGE_CHUNK_BYTES];
        let msg = ClientMessage::ChatImageChunk {
            upload_id: 3,
            index: 7,
            count: 40,
            data: data.clone(),
        };
        let bytes = encode_client_message(&msg).unwrap();
        let back = decode_client_message(&bytes).unwrap();
        assert!(matches!(
            back,
            ClientMessage::ChatImageChunk { upload_id: 3, index: 7, count: 40, data: d } if d == data
        ));

        let msg = ServerMessage::ChatImageChunk {
            client_id: 9,
            upload_id: 3,
            index: 7,
            count: 40,
            data: data.clone(),
        };
        let bytes = encode_server_message(&msg).unwrap();
        let back = decode_server_message(&bytes).unwrap();
        assert!(matches!(
            back,
            ServerMessage::ChatImageChunk { client_id: 9, upload_id: 3, data: d, .. } if d == data
        ));
    }

    #[test]
    fn oversized_or_overcounted_chunks_refused_both_ways() {
        // Payload over the chunk size: encode refuses locally…
        let big = ClientMessage::ChatImageChunk {
            upload_id: 0,
            index: 0,
            count: 1,
            data: vec![0u8; IMAGE_CHUNK_BYTES + 1],
        };
        assert!(encode_client_message(&big).is_none());
        // …and a hostile peer serializing it anyway is dropped at decode.
        let raw = postcard::to_allocvec(&big).unwrap();
        assert!(decode_client_message(&raw).is_none());

        // A chunk *claiming* an absurd total is refused outright, so
        // reassemblers never even see it.
        let overcount = ClientMessage::ChatImageChunk {
            upload_id: 0,
            index: 0,
            count: MAX_IMAGE_CHUNKS + 1,
            data: Vec::new(),
        };
        let raw = postcard::to_allocvec(&overcount).unwrap();
        assert!(decode_client_message(&raw).is_none());
    }

    #[test]
    fn huge_non_image_message_still_rejected() {
        // The pre-decode bound must not let other variants balloon:
        // a 500 KB text chat is refused post-decode.
        let msg = ClientMessage::Chat("x".repeat(500 * 1024));
        assert!(encode_client_message(&msg).is_none());
        let raw = postcard::to_allocvec(&msg).unwrap();
        assert!(decode_client_message(&raw).is_none());
    }

    #[test]
    fn image_chunks_use_media_channel() {
        let c = ClientMessage::ChatImageChunk {
            upload_id: 0,
            index: 0,
            count: 1,
            data: Vec::new(),
        };
        assert_eq!(c.channel(), MEDIA_CHANNEL);
        let s = ServerMessage::ChatImageChunk {
            client_id: 0,
            upload_id: 0,
            index: 0,
            count: 1,
            data: Vec::new(),
        };
        assert_eq!(s.channel(), MEDIA_CHANNEL);
    }

    #[test]
    fn join_channel_roundtrips_on_control() {
        let msg = ClientMessage::JoinChannel {
            channel_id: 3,
            password: Some("hunter2".to_string()),
        };
        assert_eq!(msg.channel(), CONTROL_CHANNEL);
        let bytes = encode_client_message(&msg).unwrap();
        let back = decode_client_message(&bytes).unwrap();
        assert!(matches!(
            back,
            ClientMessage::JoinChannel { channel_id: 3, password: Some(p) } if p == "hunter2"
        ));
    }

    #[test]
    fn init_with_channels_roundtrips() {
        let msg = ServerMessage::Init {
            usernames: HashMap::from([(1, "empe".to_string())]),
            client_id: 1,
            server_name: "baucord".to_string(),
            greeting: "hi".to_string(),
            channels: vec![
                ChannelInfo {
                    id: 0,
                    name: "Lobby".to_string(),
                    has_password: false,
                    user_limit: None,
                },
                ChannelInfo {
                    id: 1,
                    name: "Gaming".to_string(),
                    has_password: true,
                    user_limit: Some(8),
                },
            ],
            memberships: HashMap::from([(1, 0)]),
        };
        let bytes = encode_server_message(&msg).unwrap();
        let back = decode_server_message(&bytes).unwrap();
        let ServerMessage::Init { channels, memberships, .. } = back else {
            panic!("wrong variant");
        };
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[1].user_limit, Some(8));
        assert!(channels[1].has_password);
        assert_eq!(memberships.get(&1), Some(&0));
    }

    #[test]
    fn moved_and_denied_roundtrip() {
        let bytes = encode_server_message(&ServerMessage::UserMovedChannel {
            client_id: 9,
            channel_id: 2,
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes).unwrap(),
            ServerMessage::UserMovedChannel { client_id: 9, channel_id: 2 }
        ));

        let bytes = encode_server_message(&ServerMessage::JoinChannelDenied {
            channel_id: 2,
            reason: ChannelDenyReason::WrongPassword,
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes).unwrap(),
            ServerMessage::JoinChannelDenied { reason: ChannelDenyReason::WrongPassword, .. }
        ));
    }

    #[test]
    fn channel_list_update_roundtrips() {
        let msg = ServerMessage::ChannelListUpdated {
            channels: vec![ChannelInfo {
                id: 0,
                name: "Lobby".to_string(),
                has_password: false,
                user_limit: None,
            }],
            memberships: HashMap::from([(7, 0)]),
        };
        assert_eq!(msg.channel(), CONTROL_CHANNEL);
        let bytes = encode_server_message(&msg).unwrap();
        let ServerMessage::ChannelListUpdated { channels, memberships } =
            decode_server_message(&bytes).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(channels[0].name, "Lobby");
        assert_eq!(memberships.get(&7), Some(&0));
    }

    #[test]
    fn share_start_carries_scope() {
        let bytes = encode_client_message(&ClientMessage::ScreenShareStart {
            width: 1920,
            height: 1080,
            scope: ShareScope::Channel,
        })
        .unwrap();
        assert!(matches!(
            decode_client_message(&bytes).unwrap(),
            ClientMessage::ScreenShareStart { scope: ShareScope::Channel, .. }
        ));
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
