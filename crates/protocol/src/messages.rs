//! Control-plane message types and their serialization.
//!
//! Messages are serialized with `postcard` (compact varint encoding) and
//! travel on the Control, Signal or Media lane — each variant knows its
//! own lane via [`ClientMessage::lane`] / [`ServerMessage::lane`], so
//! call sites can't misroute.
//!
//! **Compatibility policy** (see [`crate::features`]): a breaking change
//! bumps [`crate::ALPN`] and old peers are refused in the handshake. An
//! additive change is a *new message variant*, appended at the end of
//! its enum, sent only when the peer announced the matching feature bit.
//! Never add a field to an existing struct — postcard is positional.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::lanes::Lane;

/// Transport-assigned identity of a connection, valid for its lifetime.
/// The server assigns a random non-zero id at admission and every peer
/// learns it from [`ServerMessage::Welcome`] / [`ServerMessage::Init`].
pub type ClientId = u64;

/// Stable identity behind a display name: `SHA-256(client public key)`.
/// Not honored by a 1.0 server (Part C of the spec); always `None` in
/// [`ServerMessage::Welcome`] until the `IDENTITY` feature ships.
pub type UserId = [u8; 32];

/// Display string for the wire version, sent in `Hello` for the
/// server's logs (the ALPN already gates compatibility).
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound for a serialized control message. Anything larger is
/// dropped before deserialization — a cheap guard against hostile length
/// games, independent of any transport-level buffer bound.
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
pub const MAX_IMAGE_CHUNKS: u32 = (MAX_CHAT_IMAGE_BYTES / IMAGE_CHUNK_BYTES + 1) as u32;

/// Size cap for a "send my log" upload (§9.1) **[default]**.
pub const MAX_LOG_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

/// Most chunks a log upload may span.
#[allow(clippy::cast_possible_truncation)] // 129 with current constants
pub const MAX_LOG_UPLOAD_CHUNKS: u32 = (MAX_LOG_UPLOAD_BYTES / IMAGE_CHUNK_BYTES + 1) as u32;

/// Client upload pacing for image chunks, bytes per second. Chosen to
/// sit under typical home uplinks so the router queue stays shallow
/// and voice never waits behind bulk: a 1 MiB image takes ~2 s, the
/// 10 MiB worst case ~20 s (with live progress in the UI).
pub const MEDIA_UPLOAD_PACE: usize = 512 * 1024;

// ── Shared payload types ────────────────────────────────────────────────

/// A text message attributed to a specific client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Server-assigned, unique for the server's lifetime (history,
    /// replies and reactions get a stable handle from day one).
    pub id: u64,
    /// Server wall clock, milliseconds since the Unix epoch.
    pub ts: u64,
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
// Not to be confused with transport lanes (`crate::lanes`):
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

// ── Admission ───────────────────────────────────────────────────────────

/// How the client asks to be let in. Only the shared server password
/// exists in 1.0; `Account` is reserved so that adding it later is an
/// appended variant, not a wire break.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Auth {
    /// The room secret from `server.yaml` (`authentication.password`);
    /// `None` on servers that don't require one.
    Shared { password: Option<String> },
}

/// Proof that the client holds a stable Ed25519 identity key: the public
/// key and its signature over this TLS session's exporter value
/// (`export_keying_material("baucord-client-auth")`), so a replayed
/// `Hello` proves nothing on another session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientKeyProof {
    pub public_key: [u8; 32],
    /// 64-byte Ed25519 signature.
    pub signature: Vec<u8>,
}

/// Why admission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusedReason {
    /// The shared password is missing or wrong.
    WrongPassword,
    /// `network.max_clients` reached.
    ServerFull,
    /// Too many failed admissions from this address recently.
    RateLimited,
    /// The `Hello` was malformed or arrived too late.
    BadHello,
}

impl RefusedReason {
    /// Human-readable form for UI display.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            RefusedReason::WrongPassword => "wrong server password",
            RefusedReason::ServerFull => "the server is full",
            RefusedReason::RateLimited => "too many attempts — try again in a minute",
            RefusedReason::BadHello => "the server did not understand the handshake",
        }
    }
}

// ── Telemetry (§9.1) ────────────────────────────────────────────────────
//
// The client's own view of its session, sent as data so the operator
// can line it up with the server's `link:` line for the same
// connection without asking anyone for a log file.

/// Client build and platform.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    pub version: String,
    pub os: String,
    pub arch: String,
    /// Settings live next to the executable (portable install).
    pub portable: bool,
}

/// Transport path statistics. `loss_pct` is *transport-level* (packets
/// the QUIC stack declared lost) and informational: it never steers a
/// media decision (spec §6.1).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PathSample {
    pub rtt_ms: u32,
    pub loss_pct: f32,
    pub cwnd: u64,
    pub bytes_in_flight: u64,
    pub kbps_up: f32,
    pub kbps_down: f32,
    /// Datagrams dropped at send time (buffer full or too large).
    pub datagram_send_drops: u64,
    /// Times the transport reported `DatagramsBlocked`.
    pub datagrams_blocked: u64,
}

/// Audio encode / decode counters, all already computed for the local
/// `audio stats:` line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioSample {
    // ── encode ──
    pub bitrate_bps: u32,
    /// Opus band as the ladder names it (`"wide"`, `"narrow"`, …).
    pub band: String,
    pub frame_ms: u32,
    pub red: bool,
    pub fec_hint_pct: u32,
    pub smoothed_loss_pct: f32,
    /// The encoder transmitted enough packets in the last window for
    /// its receiver report to count (spec §6.1).
    pub exercised: bool,
    pub packets_sent: u64,
    pub encode_us: u64,
    pub input_overruns: u64,
    pub output_underruns: u64,
    pub input_device: String,
    pub output_device: String,
    // ── decode ──
    pub packets_received: u64,
    pub packets_lost: u64,
    pub fec_recoveries: u64,
    pub red_recoveries: u64,
    pub plc_frames: u64,
    pub jitter_ms: u32,
    pub jitter_target_ms: u32,
    pub decode_us: u64,
}

/// Screen-share sender counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SharingSample {
    pub preset: String,
    pub rung_pct: u32,
    pub fps_encoded: f32,
    pub fps_scale_pct: u32,
    pub forced_keyframes: u64,
    pub keyframe_requests: u64,
    pub encoder: String,
    /// Groups the sharer reset because they fell behind the deadline.
    pub groups_reset: u64,
}

/// Screen-share viewer counters, one per watched stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViewingSample {
    pub sharer: ClientId,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub keyframe_requests_sent: u64,
    pub decode_us: u64,
}

/// One periodic telemetry sample (every 10 s while connected).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetrySample {
    pub client: ClientInfo,
    pub path: PathSample,
    pub audio: Option<AudioSample>,
    pub sharing: Option<SharingSample>,
    pub viewing: Vec<ViewingSample>,
}

/// A notable moment, sent immediately (a handful per session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TelemetryEvent {
    // (Variants carry a `String` or two; the size spread is small.)
    /// The audio rate ladder moved.
    RateRungChanged {
        level: u32,
        bitrate_bps: u32,
        red: bool,
    },
    /// The server-identity prompt was shown to the user.
    IdentityPromptShown,
    /// An audio device changed or was restarted.
    DeviceChanged {
        description: String,
    },
    /// A burst of capture discontinuities.
    InputDiscontinuityBurst {
        count: u64,
    },
    /// The transport reported send-buffer pressure.
    SendBufferPressure,
    ShareStarted {
        preset: String,
        encoder: String,
    },
    ShareStopped,
    /// The connection ended, as the client understood it.
    Disconnected {
        reason: String,
    },
}

// ── Client → Server ─────────────────────────────────────────────────────

/// Messages sent from a client to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message on the Control stream. The server answers with
    /// [`ServerMessage::Welcome`] followed by [`ServerMessage::Init`],
    /// or [`ServerMessage::Refused`] and closes.
    Hello {
        username: String,
        auth: Auth,
        /// Stable identity proof (Part C); a 1.0 server only logs it.
        client_key: Option<ClientKeyProof>,
        /// Capability bits ([`crate::features`]).
        features: u64,
        client_version: String,
    },

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

    // ── Telemetry (feature bit `TELEMETRY`) ─────────────────────────
    /// Periodic sample of the client's own view (§9.1). Boxed: the
    /// sample is a few hundred bytes of counters and would otherwise
    /// size every `ClientMessage` on the hot path.
    Telemetry(Box<TelemetrySample>),
    /// A notable moment (§9.1).
    TelemetryEvent(TelemetryEvent),
    /// One slice of a "send my log to this server" upload; same
    /// chunking as images, size-capped by [`MAX_LOG_UPLOAD_BYTES`].
    LogUploadChunk {
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    },
}

impl ClientMessage {
    /// The lane this message must be sent on.
    #[must_use]
    pub fn lane(&self) -> Lane {
        match self {
            // Timely, tiny, must never queue behind chat.
            ClientMessage::RequestKeyframe { .. }
            | ClientMessage::SubscribeScreen { .. }
            | ClientMessage::UnsubscribeScreen { .. }
            | ClientMessage::JoinChannel { .. } => Lane::Signal,
            // Bulk rides its own stream per upload.
            ClientMessage::ChatImageChunk { .. } | ClientMessage::LogUploadChunk { .. } => {
                Lane::Media
            }
            _ => Lane::Control,
        }
    }

    /// Whether this message is a media-upload chunk (rides a Media
    /// stream that the upload opens and finishes).
    #[must_use]
    pub fn is_upload_chunk(&self) -> bool {
        matches!(
            self,
            ClientMessage::ChatImageChunk { .. } | ClientMessage::LogUploadChunk { .. }
        )
    }
}

// ── Server → Client ─────────────────────────────────────────────────────

/// Messages sent from the server to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Admission granted. Always followed by [`ServerMessage::Init`].
    Welcome {
        client_id: ClientId,
        /// Always `None` on a 1.0 server (Part C).
        user_id: Option<UserId>,
        /// The server's capability bits; the client uses the
        /// intersection with its own.
        features: u64,
    },

    /// Admission refused; the server closes the connection after this.
    Refused { reason: RefusedReason },

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
        /// Server-assigned message id / timestamp, same as
        /// [`ChatMessage`] (repeated per chunk; postcard has no header).
        id: u64,
        ts: u64,
        /// SHA-256 of the complete image, so receivers can cache by
        /// content and a later blob store can dedup against them.
        hash: [u8; 32],
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

    // ── Receiver report (spec §6.1) ─────────────────────────────────
    /// The relay's view of *this client's* voice over the last
    /// reporting interval: what its listeners actually miss, counted
    /// from the voice sequence numbers where the packets arrive. The
    /// only loss figure that may drive the audio rate ladder.
    VoiceReceiverReport {
        received: u32,
        expected: u32,
        lost: u32,
        late: u32,
        /// The server's path RTT to this client, ms.
        rtt_ms: u32,
    },
}

impl ServerMessage {
    /// The lane this message must be sent on.
    #[must_use]
    pub fn lane(&self) -> Lane {
        match self {
            ServerMessage::KeyframeRequested { .. } | ServerMessage::StreamHealth { .. } => {
                Lane::Signal
            }
            ServerMessage::ChatImageChunk { .. } => Lane::Media,
            _ => Lane::Control,
        }
    }

    /// Whether this message is a media-upload chunk (rides a Media
    /// stream that the relay opens and finishes per image).
    #[must_use]
    pub fn is_upload_chunk(&self) -> bool {
        matches!(self, ServerMessage::ChatImageChunk { .. })
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
        ClientMessage::LogUploadChunk { data, count, .. } => {
            data.len() <= IMAGE_CHUNK_BYTES && *count <= MAX_LOG_UPLOAD_CHUNKS
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
            id: 1,
            ts: 2,
            hash: [3; 32],
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
    fn image_chunks_use_media_lane() {
        let c = ClientMessage::ChatImageChunk {
            upload_id: 0,
            index: 0,
            count: 1,
            data: Vec::new(),
        };
        assert_eq!(c.lane(), Lane::Media);
        let s = ServerMessage::ChatImageChunk {
            client_id: 0,
            upload_id: 0,
            index: 0,
            count: 1,
            id: 0,
            ts: 0,
            hash: [0; 32],
            data: Vec::new(),
        };
        assert_eq!(s.lane(), Lane::Media);
    }

    #[test]
    fn join_channel_roundtrips_on_signal() {
        let msg = ClientMessage::JoinChannel {
            channel_id: 3,
            password: Some("hunter2".to_string()),
        };
        assert_eq!(msg.lane(), Lane::Signal);
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
        let ServerMessage::Init {
            channels,
            memberships,
            ..
        } = back
        else {
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
            ServerMessage::UserMovedChannel {
                client_id: 9,
                channel_id: 2
            }
        ));

        let bytes = encode_server_message(&ServerMessage::JoinChannelDenied {
            channel_id: 2,
            reason: ChannelDenyReason::WrongPassword,
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes).unwrap(),
            ServerMessage::JoinChannelDenied {
                reason: ChannelDenyReason::WrongPassword,
                ..
            }
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
        assert_eq!(msg.lane(), Lane::Control);
        let bytes = encode_server_message(&msg).unwrap();
        let ServerMessage::ChannelListUpdated {
            channels,
            memberships,
        } = decode_server_message(&bytes).unwrap()
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
            ClientMessage::ScreenShareStart {
                scope: ShareScope::Channel,
                ..
            }
        ));
    }

    #[test]
    fn keyframe_messages_use_signal_lane() {
        assert_eq!(
            ClientMessage::RequestKeyframe { target_id: 1 }.lane(),
            Lane::Signal
        );
        assert_eq!(
            ServerMessage::KeyframeRequested { viewer_id: 1 }.lane(),
            Lane::Signal
        );
        assert_eq!(ClientMessage::Chat(String::new()).lane(), Lane::Control);
    }

    #[test]
    fn hello_and_welcome_roundtrip() {
        let hello = ClientMessage::Hello {
            username: "chicho".to_string(),
            auth: Auth::Shared {
                password: Some("conos".to_string()),
            },
            client_key: Some(ClientKeyProof {
                public_key: [7; 32],
                signature: vec![9; 64],
            }),
            features: crate::features::CLIENT_1_0,
            client_version: "1.0.0".to_string(),
        };
        assert_eq!(hello.lane(), Lane::Control);
        let bytes = encode_client_message(&hello).unwrap();
        let ClientMessage::Hello {
            username,
            auth: Auth::Shared { password },
            client_key,
            features,
            ..
        } = decode_client_message(&bytes).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(username, "chicho");
        assert_eq!(password.as_deref(), Some("conos"));
        assert_eq!(client_key.map(|k| k.public_key), Some([7; 32]));
        assert_eq!(features, crate::features::CLIENT_1_0);

        let welcome = ServerMessage::Welcome {
            client_id: 42,
            user_id: None,
            features: crate::features::SERVER_1_0,
        };
        let bytes = encode_server_message(&welcome).unwrap();
        assert!(matches!(
            decode_server_message(&bytes).unwrap(),
            ServerMessage::Welcome {
                client_id: 42,
                user_id: None,
                ..
            }
        ));
    }

    #[test]
    fn refused_and_receiver_report_roundtrip() {
        let bytes = encode_server_message(&ServerMessage::Refused {
            reason: RefusedReason::WrongPassword,
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes).unwrap(),
            ServerMessage::Refused {
                reason: RefusedReason::WrongPassword
            }
        ));

        let report = ServerMessage::VoiceReceiverReport {
            received: 90,
            expected: 100,
            lost: 10,
            late: 2,
            rtt_ms: 43,
        };
        assert_eq!(report.lane(), Lane::Control);
        let bytes = encode_server_message(&report).unwrap();
        assert!(matches!(
            decode_server_message(&bytes).unwrap(),
            ServerMessage::VoiceReceiverReport {
                expected: 100,
                lost: 10,
                ..
            }
        ));
    }

    #[test]
    fn chat_message_carries_server_id_and_timestamp() {
        let msg = ServerMessage::Chat(ChatMessage {
            id: 5,
            ts: 1_700_000_000_000,
            client_id: 1,
            text: "hi".to_string(),
        });
        let bytes = encode_server_message(&msg).unwrap();
        let ServerMessage::Chat(back) = decode_server_message(&bytes).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!((back.id, back.ts), (5, 1_700_000_000_000));
    }

    #[test]
    fn telemetry_sample_roundtrips_on_control() {
        let msg = ClientMessage::Telemetry(Box::new(TelemetrySample {
            client: ClientInfo {
                version: "1.0.0".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                portable: true,
            },
            path: PathSample {
                rtt_ms: 40,
                ..PathSample::default()
            },
            audio: Some(AudioSample::default()),
            sharing: None,
            viewing: vec![ViewingSample {
                sharer: 9,
                ..ViewingSample::default()
            }],
        }));
        assert_eq!(msg.lane(), Lane::Control);
        let bytes = encode_client_message(&msg).unwrap();
        assert!(bytes.len() < 600, "sample is {} bytes", bytes.len());
        let ClientMessage::Telemetry(back) = decode_client_message(&bytes).unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(back.path.rtt_ms, 40);
        assert_eq!(back.viewing[0].sharer, 9);
    }

    #[test]
    fn log_upload_chunk_is_bounded_like_images() {
        let ok = ClientMessage::LogUploadChunk {
            upload_id: 1,
            index: 0,
            count: 2,
            data: vec![0; IMAGE_CHUNK_BYTES],
        };
        assert_eq!(ok.lane(), Lane::Media);
        assert!(encode_client_message(&ok).is_some());
        let overcount = ClientMessage::LogUploadChunk {
            upload_id: 1,
            index: 0,
            count: MAX_LOG_UPLOAD_CHUNKS + 1,
            data: Vec::new(),
        };
        assert!(encode_client_message(&overcount).is_none());
    }
}
