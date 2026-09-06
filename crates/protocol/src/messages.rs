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

use crate::{
    admin::{AdminCommand, AdminError, AdminOutcome, RestrictionKind},
    lanes::Lane,
    permissions::RoleId,
};

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
    // ── 1.1 ─────────────────────────────────────────────────────────
    /// The user's roles do not allow it, or a restriction removed it.
    NotPermitted,
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
            DenyReason::NotPermitted => "you are not allowed to share here",
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
    // ── 1.1 ─────────────────────────────────────────────────────────
    /// The user's roles do not allow joining channels.
    NotPermitted,
}

impl ChannelDenyReason {
    /// Human-readable form for UI display.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            ChannelDenyReason::NoSuchChannel => "that channel does not exist",
            ChannelDenyReason::WrongPassword => "wrong channel password",
            ChannelDenyReason::ChannelFull => "the channel is full",
            ChannelDenyReason::NotPermitted => "you are not allowed to join channels",
        }
    }
}

// ── Admission ───────────────────────────────────────────────────────────

/// How the client asks to be let in (spec §3.2).
///
/// 1.0 had only the shared room password. 1.1 admits by device key: a
/// known key logs in with `Key`, a first visit presents an invite code
/// with `Invite`. `Shared` stays so a 1.0 client's `Hello` still
/// decodes — and is refused with [`RefusedReason::BadHello`], which
/// that client already renders as "update baucord".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Auth {
    /// The 1.0 room secret. Refused by a 1.1 server.
    Shared { password: Option<String> },
    /// "You know this device": `Hello.client_key` is the login. Every
    /// visit after the first.
    Key,
    /// A first visit: `Hello.client_key` plus an invite code. The
    /// server creates the user, links the key and grants the code's
    /// role. A known key sending a code is logged in as `Key` and no
    /// use is burned.
    Invite { code: String },
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

/// Why a `Hello` without a valid invite needs one (spec §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NeedInviteReason {
    /// The server has never seen this key.
    UnknownKey,
    /// This key was unlinked from its user by an admin; it cannot
    /// found a second user. An admin has to re-link it.
    KeyRetired,
}

/// Why an invite code was not accepted (spec §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InviteInvalidReason {
    Expired,
    Revoked,
    /// Its use cap is reached.
    Exhausted,
    /// No such code.
    Unknown,
}

/// Why admission was refused. Not `Copy`: a ban carries its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusedReason {
    /// The shared password is missing or wrong (1.0 servers only).
    WrongPassword,
    /// `network.max_clients` reached.
    ServerFull,
    /// Too many failed admissions from this address recently.
    RateLimited,
    /// The `Hello` was malformed, arrived too late, or used the 1.0
    /// password variant against a 1.1 server.
    BadHello,
    // ── 1.1 (feature bit `INVITES`) ─────────────────────────────────
    /// Unknown (or retired) key and no invite code: the client shows
    /// the invite field.
    NeedInvite { why: NeedInviteReason },
    /// The code did not admit: the client says which and keeps the
    /// field.
    InviteInvalid { why: InviteInvalidReason },
    /// The chosen name belongs to someone else: ask for another.
    NameInUse,
    /// A `server` restriction on the key, user or address. `until` is
    /// milliseconds since the Unix epoch; `None` is indefinite.
    Banned { until: Option<u64>, reason: String },
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
            RefusedReason::NeedInvite {
                why: NeedInviteReason::UnknownKey,
            } => "this server needs an invite code",
            RefusedReason::NeedInvite {
                why: NeedInviteReason::KeyRetired,
            } => "this device was removed from your user; ask an admin to re-link it",
            RefusedReason::InviteInvalid {
                why: InviteInvalidReason::Expired,
            } => "that invite code has expired",
            RefusedReason::InviteInvalid {
                why: InviteInvalidReason::Revoked,
            } => "that invite code was revoked",
            RefusedReason::InviteInvalid {
                why: InviteInvalidReason::Exhausted,
            } => "that invite code has been used up",
            RefusedReason::InviteInvalid {
                why: InviteInvalidReason::Unknown,
            } => "that invite code is not valid",
            RefusedReason::NameInUse => "that name is already in use",
            RefusedReason::Banned { .. } => "you are banned from this server",
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

/// Which conversation a history request is about (spec §6). DMs are
/// 1.1.1; the variant exists so the shape does not change then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConvRef {
    General,
    Dm(UserId),
}

/// One stored message (spec §6): authored by a user who may well be
/// offline, so it names the author rather than a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub id: u64,
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    pub user_id: UserId,
    /// The author's name of record at the time of sending.
    pub name: String,
    pub text: String,
    pub edited: bool,
}

/// Why a channel could not be created, edited or deleted (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelUiDenyReason {
    NameTaken,
    NoPermission,
    /// Config channels belong to the file: rename yes, delete no.
    ConfigOwned,
    /// The creator already owns `user_channels.max_per_user`.
    Limit,
    NoSuchChannel,
    /// A malformed request: empty name, absurd limit.
    Invalid,
}

impl ChannelUiDenyReason {
    /// Human-readable form for UI display.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::NameTaken => "a channel with that name exists",
            Self::NoPermission => "you may not do that to this channel",
            Self::ConfigOwned => "that channel comes from the server's config file",
            Self::Limit => "you already own as many channels as allowed",
            Self::NoSuchChannel => "that channel does not exist",
            Self::Invalid => "that channel request was not valid",
        }
    }
}

/// A user-created channel's settings, sent whole on create and edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSpec {
    pub name: String,
    pub password: Option<String>,
    pub user_limit: Option<u16>,
    /// Joinable from this role's rank up; `None` = everyone.
    pub min_role: Option<RoleId>,
    /// Kept when empty; otherwise deleted after
    /// `user_channels.temporary_after_min` empty.
    pub permanent: bool,
    /// Idle minutes before a member is moved to the AFK channel; `None`
    /// = the server default.
    pub afk_after_min: Option<u16>,
}

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

    // ── 1.1: invites (feature bit `INVITES`) ────────────────────────
    /// Mint an invite code (spec §3.1). Needs `CREATE_INVITES`. A
    /// member gets guest codes; `grants_role` other than guest needs
    /// `MANAGE_INVITES`, and admin codes only come from the CLI.
    /// Expiry longer than the default (24 h) also needs
    /// `MANAGE_INVITES`; the server clamps and reports what it minted.
    /// Answered with `InviteCreated` or `InviteDenied`.
    CreateInvite {
        /// `None` = guest.
        grants_role: Option<RoleId>,
        /// Seconds from now; `None` = the default.
        expires_secs: Option<u64>,
        /// `None` = unlimited.
        max_uses: Option<u32>,
    },

    // ── 1.1: administration (feature bit `ROLES`) ───────────────────
    /// An administrative action (spec §8), checked like any other
    /// message and audited by the server. Answered with `AdminResult`
    /// carrying the same `request_id`.
    Admin {
        request_id: u32,
        command: AdminCommand,
    },

    // ── 1.1: history (feature bit `HISTORY`) ────────────────────────
    /// Older messages: those with an id below `before` (`None` = the
    /// newest), at most `limit`. Answered with `History`, or
    /// `HistoryUnavailable` when the store cannot answer in time.
    FetchHistory {
        conv: ConvRef,
        before: Option<u64>,
        limit: u16,
    },
    /// Replace the text of your own message.
    EditMessage { msg_id: u64, text: String },
    /// Remove your own message.
    DeleteMessage { msg_id: u64 },

    // ── 1.1: channels from the UI (feature bit `CHANNELS_UI`) ───────
    /// Create a channel (needs `CREATE_CHANNELS`; spec §5).
    CreateChannel(ChannelSpec),
    /// Edit a channel you own (`MANAGE_OWN_CHANNELS`) or any
    /// (`MANAGE_CHANNELS`). A config channel accepts only a rename.
    EditChannel { channel_id: u16, spec: ChannelSpec },
    /// Delete a channel you own, or any with `MANAGE_CHANNELS`.
    DeleteChannel { channel_id: u16 },
    /// Send a member of your channel back to the default channel.
    ChannelKick { channel_id: u16, user_id: UserId },

    // ── 1.1.2: profiles (feature bit `PROFILES`, spec §7.1) ─────────
    /// How this user's name looks; sent right after `Session` on every
    /// connect and on every change. The server validates (contrast,
    /// the gold band, the avatar's size) and answers by broadcasting
    /// [`ServerMessage::ProfileUpdated`] or with
    /// [`ServerMessage::ProfileDenied`].
    SetProfile(Profile),
    /// A new name while connected. Checked like the name at `Hello`;
    /// answered by [`ServerMessage::UserRenamed`] to everyone, or
    /// [`ServerMessage::RenameDenied`].
    SetNickname { name: String },

    // ── 1.1.2: blobs (feature bit `BLOBS`, spec §7) ─────────────────
    /// Announce an upload. `sha256` lets a known blob be acknowledged
    /// without transfer. Until files land, only avatar-sized images
    /// (`avatars.max_kb`) are accepted.
    UploadBegin {
        upload_id: u32,
        name: String,
        mime: String,
        size: u32,
        sha256: BlobId,
    },
    /// One slice of the announced upload; same pacing and bounds as
    /// [`ClientMessage::ChatImageChunk`].
    UploadChunk {
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    },
    /// Ask for a blob's bytes; they arrive as
    /// [`ServerMessage::BlobChunk`] on a Media stream, or
    /// [`ServerMessage::BlobMissing`].
    FetchBlob { blob_id: BlobId },

    // ── 1.1.1: direct messages (feature bit `DM`, spec §6) ──────────
    /// A message to one user. `client_ref` is the sender's own handle
    /// for the reply: [`ServerMessage::MessageAck`] once stored (and
    /// delivered if they are online) or [`ServerMessage::MessageFailed`].
    DirectMessage {
        to: UserId,
        text: String,
        client_ref: u32,
    },
    /// Everything up to `msg_id` in `conv` has been seen; the unread
    /// count in [`ServerMessage::Conversations`] starts after it.
    MarkRead { conv: ConvRef, msg_id: u64 },
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
            ClientMessage::ChatImageChunk { .. }
            | ClientMessage::LogUploadChunk { .. }
            | ClientMessage::UploadChunk { .. } => Lane::Media,
            ClientMessage::UploadBegin { .. } | ClientMessage::FetchBlob { .. } => Lane::Signal,
            _ => Lane::Control,
        }
    }

    /// Whether this message is a media-upload chunk (rides a Media
    /// stream that the upload opens and finishes).
    #[must_use]
    pub fn is_upload_chunk(&self) -> bool {
        matches!(
            self,
            ClientMessage::ChatImageChunk { .. }
                | ClientMessage::LogUploadChunk { .. }
                | ClientMessage::UploadChunk { .. }
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
    Refused {
        reason: RefusedReason,
    },

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
    UserLeft {
        client_id: ClientId,
    },

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
    ScreenShareStopped {
        client_id: ClientId,
    },

    /// The server refused our `ScreenShareStart`.
    ShareDenied {
        reason: DenyReason,
    },

    /// The server refused our `SubscribeScreen`.
    SubscribeDenied {
        target_id: ClientId,
        reason: DenyReason,
    },

    /// A viewer needs a keyframe from us — force an IDR.
    /// Signal channel: must not queue behind chat.
    KeyframeRequested {
        viewer_id: ClientId,
    },

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

    // ── 1.1: sessions and invites (feature bit `INVITES`) ───────────
    /// Who this session is, right after `Welcome` (spec §10). The
    /// client renders what `permissions` allows; it never decides.
    Session {
        user_id: UserId,
        /// Bitset from [`crate::permissions`]: this user's roles
        /// unioned, minus active restrictions.
        permissions: u64,
        /// `false`: this server forgets roles, bans and invites when it
        /// restarts (spec §1.1). The client shows a standing notice.
        persistent: bool,
    },
    /// This session is over by the server's decision (spec §4.1). The
    /// connection closes 300 ms later; the client renders a final
    /// disconnect with the reason and does not auto-reconnect — except
    /// after `Replaced`, which is its own newer connection winning.
    Removed {
        kind: RemovedKind,
        reason: String,
        /// Display name of who did it; `server` for automatic ones.
        by: String,
    },
    /// Answer to `CreateInvite`.
    InviteCreated {
        code: String,
        grants_role: RoleId,
        /// Milliseconds since the Unix epoch; `None` = never.
        expires_at: Option<u64>,
        max_uses: Option<u32>,
    },
    /// `CreateInvite` was refused.
    InviteDenied {
        reason: InviteDenyReason,
    },

    // ── 1.1: roles and moderation (feature bit `ROLES`) ─────────────
    /// Who a session is, for the roster: after each `UserJoined`, and
    /// once per user after `Init`. Roles come from the server's tables;
    /// no client asserts its own (spec §8).
    UserDetails {
        client_id: ClientId,
        user_id: UserId,
        roles: Vec<RoleId>,
        /// From xp (spec §9.1), when the server tracks progress.
        level: u16,
    },
    /// A restriction now applies to this session (spec §4.2): the UI
    /// shows it instead of letting the action fail silently. `until`
    /// in milliseconds since the Unix epoch; `None` = until lifted.
    Restricted {
        kind: RestrictionKind,
        until: Option<u64>,
        reason: String,
    },
    /// A restriction on this session ended (lifted, or ran out).
    RestrictionLifted {
        kind: RestrictionKind,
    },
    /// The answer to `ClientMessage::Admin`.
    AdminResult {
        request_id: u32,
        result: Result<AdminOutcome, AdminError>,
    },

    // ── 1.1: history (feature bit `HISTORY`) ────────────────────────
    /// A page of messages, oldest first. Right after `Init` the last
    /// `history.replay` of the general chat arrive this way; `more`
    /// says an older page exists (`FetchHistory` with `before` = the
    /// first id here). Pages are at most 64 KiB.
    History {
        conv: ConvRef,
        messages: Vec<HistoryMessage>,
        more: bool,
    },
    /// The store could not answer in time (a stalled disk): the client
    /// keeps what it has and may retry after `retry_secs`.
    HistoryUnavailable {
        conv: ConvRef,
        retry_secs: u16,
    },
    MessageEdited {
        msg_id: u64,
        text: String,
    },
    MessageDeleted {
        msg_id: u64,
    },
    /// Chat is rate limited (`limits.chat`); the message was dropped.
    ChatRateLimited {
        retry_ms: u32,
    },

    // ── 1.1: progress (feature bit `PROGRESS`) ──────────────────────
    /// Your xp crossed a level (spec §9.1).
    ProgressUpdate {
        xp: u64,
        level: u16,
    },

    // ── 1.1: channels from the UI (feature bit `CHANNELS_UI`) ───────
    /// What `ChannelInfo` cannot say, one per channel after `Init` and
    /// after every `ChannelListUpdated`. `stable_id` survives the
    /// renumbering a list change causes (0 for config channels).
    ChannelDetails {
        channel_id: u16,
        stable_id: u64,
        owner: Option<UserId>,
        min_role: Option<RoleId>,
        permanent: bool,
        afk_after_min: Option<u16>,
        is_afk: bool,
    },
    /// A create/edit/delete/kick was refused.
    ChannelDenied {
        reason: ChannelUiDenyReason,
    },
    /// The server moved this session to the AFK channel after
    /// `after_min` idle minutes (spec §5).
    MovedIdle {
        after_min: u16,
    },

    // ── 1.1.2: profiles (feature bit `PROFILES`, spec §7.1) ─────────
    /// A session's profile, sent right after each `UserDetails` and as
    /// a batch after `Init` to sessions that negotiated `PROFILES`.
    UserProfile {
        client_id: ClientId,
        profile: Profile,
    },
    /// A profile changed (or was reset by an admin); everyone renders
    /// from it.
    ProfileUpdated {
        user_id: UserId,
        profile: Profile,
    },
    ProfileDenied {
        reason: ProfileDenyReason,
    },
    /// A session changed its name; every roster renames it.
    UserRenamed {
        client_id: ClientId,
        name: String,
    },
    RenameDenied {
        reason: RenameDenyReason,
    },

    // ── 1.1.2: blobs (feature bit `BLOBS`, spec §7) ─────────────────
    /// `needed: false` = the server already has this hash; reference
    /// it, send nothing.
    UploadAccept {
        upload_id: u32,
        needed: bool,
    },
    UploadDone {
        upload_id: u32,
        blob_id: BlobId,
    },
    UploadRejected {
        upload_id: u32,
        reason: UploadRejectReason,
    },
    /// One slice of a fetched blob, on a Media stream paced like an
    /// image relay; the client verifies the hash once whole.
    BlobChunk {
        blob_id: BlobId,
        index: u32,
        count: u32,
        data: Vec<u8>,
    },
    BlobMissing {
        blob_id: BlobId,
    },

    // ── 1.1.1: direct messages (feature bit `DM`, spec §6) ──────────
    /// A live DM from `from`; `msg.client_id` is their session.
    Direct {
        from: UserId,
        msg: ChatMessage,
    },
    /// The sender's DM has its id: stored (with persistence) and
    /// delivered to every session of the recipient that is online.
    MessageAck {
        client_ref: u32,
        msg_id: u64,
    },
    MessageFailed {
        client_ref: u32,
        reason: DmFailReason,
    },
    /// The DM conversations this user has, with unread counts; sent
    /// after the general replay on join and after a change (a DM
    /// arrived while away, a mark moved).
    Conversations {
        dms: Vec<DmSummary>,
    },

    // ── 1.1: progress, everyone's (feature bit `PROGRESS`) ──────────
    /// How far a session's user is (spec §9.1), for the profile card's
    /// bar: the whole roster to a newcomer after `Init`, the newcomer
    /// to everyone, and once a minute for whoever gained xp.
    /// `UserDetails` carries the level alone; this is the xp behind
    /// it, and `progress::xp_for_level` / `cost_of_level` turn the
    /// pair into a fraction.
    UserProgress {
        client_id: ClientId,
        xp: u64,
        level: u16,
    },
}

/// Why a DM was not sent (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DmFailReason {
    /// No store, and the recipient is not online to take it live.
    Offline,
    /// A guest on either end, or a `dm` restriction on the sender.
    NoPermission,
    /// No such user.
    NoSuchUser,
    /// The store did not confirm within the deadline; nothing was
    /// delivered.
    Store,
}

impl DmFailReason {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Offline => "they are offline and this server does not keep messages",
            Self::NoPermission => "direct messages are for members",
            Self::NoSuchUser => "no such user",
            Self::Store => "the server could not store the message; try again",
        }
    }
}

/// One DM conversation as the list shows it (spec §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmSummary {
    pub user_id: UserId,
    /// Their name of record.
    pub name: String,
    pub unread: u32,
    /// The newest message's `ts`, milliseconds since the Unix epoch.
    pub last_ts: u64,
    /// The newest message's id (the client's paging cursor).
    pub last_msg_id: u64,
}

/// The SHA-256 of a blob's bytes: its identity everywhere (spec §7).
pub type BlobId = [u8; 32];

/// How a name is drawn (spec §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NameStyle {
    #[default]
    Normal,
    Bold,
    Italic,
}

/// How a user's name looks, and their picture (spec §7.1). Kept on
/// the user row, so it follows the key across visits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Profile {
    /// `0xRRGGBB`; `None` = the client's default name color.
    pub name_color: Option<u32>,
    pub name_style: NameStyle,
    /// A 128×128 PNG/JPEG/WebP blob the client uploaded.
    pub avatar: Option<BlobId>,
}

/// Why `SetProfile` was refused (spec §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileDenyReason {
    /// The gold band (hue 38°–65°, saturation ≥ 0.35) without `ADMIN`.
    ReservedColor,
    /// Under 3:1 against the chat background.
    LowContrast,
    /// One change per 30 s.
    RateLimited,
    /// An admin reset it with `lock`; a `profile` restriction stands.
    Locked,
    /// The avatar is not a blob the server holds, or not an image of
    /// the allowed size.
    BadAvatar,
}

impl ProfileDenyReason {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::ReservedColor => "gold is reserved for admins",
            Self::LowContrast => "that color does not read on the chat background",
            Self::RateLimited => "one profile change every 30 seconds",
            Self::Locked => "an admin locked your profile",
            Self::BadAvatar => "the server does not have that picture",
        }
    }
}

/// Why `SetNickname` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenameDenyReason {
    /// Someone online has it, or a guest asked for an offline
    /// member's name.
    Taken,
    /// Empty after sanitizing.
    Invalid,
    /// One rename per 30 s.
    RateLimited,
}

impl RenameDenyReason {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Taken => "that name is in use",
            Self::Invalid => "that name is empty",
            Self::RateLimited => "one rename every 30 seconds",
        }
    }
}

/// Why `UploadBegin` or the finished upload was refused (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadRejectReason {
    TooLarge,
    /// Not PNG/JPEG/WebP (declared or sniffed).
    BadType,
    /// The bytes do not hash to what `UploadBegin` said.
    HashMismatch,
    /// A second upload before the first finished, or chunks for an
    /// upload never announced.
    NotAnnounced,
    /// The server is not storing blobs at the moment (its blob store
    /// is full, or the upload kind is not enabled yet).
    Refused,
}

impl UploadRejectReason {
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::TooLarge => "the file is larger than this server allows",
            Self::BadType => "only PNG, JPEG and WebP images are accepted",
            Self::HashMismatch => "the upload did not match its announced hash",
            Self::NotAnnounced => "the upload was not announced first",
            Self::Refused => "the server refused the upload",
        }
    }
}

/// Why a session was ended (spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemovedKind {
    /// Access unchanged; rejoining is one click.
    Disconnected,
    /// Access revoked until an admin lifts it.
    Kicked,
    /// Access revoked and the key and address blocked; `until` in
    /// milliseconds since the Unix epoch, `None` = indefinite.
    Banned { until: Option<u64> },
    /// The same user signed in from a newer connection (auto-reconnect
    /// after a blackout); this older session yields.
    Replaced,
}

/// Why `CreateInvite` was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InviteDenyReason {
    /// No `CREATE_INVITES`, or no `MANAGE_INVITES` for that role.
    NoPermission,
    /// No such role, or a role that codes may not grant (admin).
    RoleNotAllowed,
}

impl ServerMessage {
    /// The lane this message must be sent on.
    #[must_use]
    pub fn lane(&self) -> Lane {
        match self {
            ServerMessage::KeyframeRequested { .. } | ServerMessage::StreamHealth { .. } => {
                Lane::Signal
            }
            ServerMessage::ChatImageChunk { .. } | ServerMessage::BlobChunk { .. } => Lane::Media,
            ServerMessage::UploadAccept { .. }
            | ServerMessage::UploadDone { .. }
            | ServerMessage::UploadRejected { .. }
            | ServerMessage::BlobMissing { .. } => Lane::Signal,
            _ => Lane::Control,
        }
    }

    /// Whether this message is a media-upload chunk (rides a Media
    /// stream that the relay opens and finishes per image).
    #[must_use]
    pub fn is_upload_chunk(&self) -> bool {
        matches!(
            self,
            ServerMessage::ChatImageChunk { .. } | ServerMessage::BlobChunk { .. }
        )
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

// ── Telemetry at rest ───────────────────────────────────────────────
//
// The server keeps each sample and event as an opaque blob next to the
// columns worth querying (spec §9). Same codec as the wire so the
// `server telemetry` CLI and a future viewer decode them with this
// crate and nothing else.

/// Serialize a telemetry sample for storage.
#[must_use]
pub fn encode_telemetry_sample(sample: &TelemetrySample) -> Option<Vec<u8>> {
    postcard::to_allocvec(sample).ok()
}

/// Deserialize a stored telemetry sample.
#[must_use]
pub fn decode_telemetry_sample(bytes: &[u8]) -> Option<TelemetrySample> {
    postcard::from_bytes(bytes).ok()
}

/// Serialize a telemetry event for storage.
#[must_use]
pub fn encode_telemetry_event(event: &TelemetryEvent) -> Option<Vec<u8>> {
    postcard::to_allocvec(event).ok()
}

/// Deserialize a stored telemetry event.
#[must_use]
pub fn decode_telemetry_event(bytes: &[u8]) -> Option<TelemetryEvent> {
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
    fn key_and_invite_auth_roundtrip_in_hello() {
        for auth in [
            Auth::Key,
            Auth::Invite {
                code: "K7FQ2MX9ABCD".to_string(),
            },
        ] {
            let msg = ClientMessage::Hello {
                username: "alice".to_string(),
                auth: auth.clone(),
                client_key: None,
                features: features_for_test(),
                client_version: CLIENT_VERSION.to_string(),
            };
            let bytes = encode_client_message(&msg).unwrap();
            let Some(ClientMessage::Hello { auth: back, .. }) = decode_client_message(&bytes)
            else {
                panic!("not a Hello");
            };
            match (auth, back) {
                (Auth::Key, Auth::Key) => {}
                (Auth::Invite { code: a }, Auth::Invite { code: b }) => assert_eq!(a, b),
                (a, b) => panic!("auth changed on the wire: {a:?} → {b:?}"),
            }
        }
    }

    fn features_for_test() -> u64 {
        crate::features::CLIENT_1_0 | crate::features::INVITES
    }

    #[test]
    fn banned_refusal_carries_until_and_reason() {
        let bytes = encode_server_message(&ServerMessage::Refused {
            reason: RefusedReason::Banned {
                until: Some(1_800_000_000_000),
                reason: "spam".to_string(),
            },
        })
        .unwrap();
        let Some(ServerMessage::Refused { reason }) = decode_server_message(&bytes) else {
            panic!("not a Refused");
        };
        assert_eq!(
            reason,
            RefusedReason::Banned {
                until: Some(1_800_000_000_000),
                reason: "spam".to_string(),
            }
        );
    }

    #[test]
    fn need_invite_reasons_describe_themselves_distinctly() {
        let unknown = RefusedReason::NeedInvite {
            why: NeedInviteReason::UnknownKey,
        };
        let retired = RefusedReason::NeedInvite {
            why: NeedInviteReason::KeyRetired,
        };
        assert_ne!(unknown.describe(), retired.describe());
    }

    #[test]
    fn telemetry_sample_survives_storage_codec() {
        let sample = TelemetrySample {
            path: PathSample {
                rtt_ms: 42,
                ..PathSample::default()
            },
            ..TelemetrySample::default()
        };
        let bytes = encode_telemetry_sample(&sample).unwrap();
        assert_eq!(decode_telemetry_sample(&bytes).unwrap().path.rtt_ms, 42);
        let event = TelemetryEvent::ShareStopped;
        let bytes = encode_telemetry_event(&event).unwrap();
        assert!(matches!(
            decode_telemetry_event(&bytes).unwrap(),
            TelemetryEvent::ShareStopped
        ));
    }

    #[test]
    fn session_and_removed_roundtrip_on_control() {
        let session = ServerMessage::Session {
            user_id: [9u8; 32],
            permissions: crate::permissions::MEMBER_PERMISSIONS,
            persistent: false,
        };
        assert_eq!(session.lane(), Lane::Control);
        let bytes = encode_server_message(&session).unwrap();
        let Some(ServerMessage::Session {
            user_id,
            permissions,
            persistent,
        }) = decode_server_message(&bytes)
        else {
            panic!("not a Session");
        };
        assert_eq!(user_id, [9u8; 32]);
        assert_eq!(permissions, crate::permissions::MEMBER_PERMISSIONS);
        assert!(!persistent);

        let removed = ServerMessage::Removed {
            kind: RemovedKind::Banned { until: Some(42) },
            reason: "spam".to_string(),
            by: "mpw".to_string(),
        };
        assert_eq!(removed.lane(), Lane::Control);
        let bytes = encode_server_message(&removed).unwrap();
        assert!(matches!(
            decode_server_message(&bytes),
            Some(ServerMessage::Removed {
                kind: RemovedKind::Banned { until: Some(42) },
                ..
            })
        ));
    }

    #[test]
    fn invite_messages_roundtrip() {
        let create = ClientMessage::CreateInvite {
            grants_role: Some(crate::permissions::ROLE_MEMBER),
            expires_secs: Some(3600),
            max_uses: None,
        };
        assert_eq!(create.lane(), Lane::Control);
        let bytes = encode_client_message(&create).unwrap();
        assert!(matches!(
            decode_client_message(&bytes),
            Some(ClientMessage::CreateInvite {
                grants_role: Some(2),
                expires_secs: Some(3600),
                max_uses: None,
            })
        ));

        let created = ServerMessage::InviteCreated {
            code: "K7FQ2MX9ABCD".to_string(),
            grants_role: crate::permissions::ROLE_GUEST,
            expires_at: Some(1_800_000_000_000),
            max_uses: Some(1),
        };
        let bytes = encode_server_message(&created).unwrap();
        let Some(ServerMessage::InviteCreated { code, max_uses, .. }) =
            decode_server_message(&bytes)
        else {
            panic!("not InviteCreated");
        };
        assert_eq!(code, "K7FQ2MX9ABCD");
        assert_eq!(max_uses, Some(1));

        let bytes = encode_server_message(&ServerMessage::InviteDenied {
            reason: InviteDenyReason::RoleNotAllowed,
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes),
            Some(ServerMessage::InviteDenied {
                reason: InviteDenyReason::RoleNotAllowed
            })
        ));
    }

    #[test]
    fn admin_messages_roundtrip() {
        use crate::admin::{AdminTarget, KeySummary, UserSummary};
        // The message enums carry floats and skip `PartialEq`; a round
        // trip that re-encodes to the same bytes is the equality here.
        let client_roundtrips = |msg: &ClientMessage| {
            let bytes = encode_client_message(msg).unwrap();
            let again = decode_client_message(&bytes).expect("decodes");
            encode_client_message(&again).unwrap() == bytes
        };
        let server_roundtrips = |msg: &ServerMessage| {
            let bytes = encode_server_message(msg).unwrap();
            let again = decode_server_message(&bytes).expect("decodes");
            encode_server_message(&again).unwrap() == bytes
        };

        let cmd = ClientMessage::Admin {
            request_id: 7,
            command: AdminCommand::Restrict {
                target: AdminTarget::User([1u8; 32]),
                kind: RestrictionKind::Voice,
                duration_secs: Some(1800),
                reason: "spam".to_string(),
            },
        };
        assert_eq!(cmd.lane(), Lane::Control);
        assert!(client_roundtrips(&cmd));

        assert!(server_roundtrips(&ServerMessage::AdminResult {
            request_id: 7,
            result: Ok(AdminOutcome::Users(vec![UserSummary {
                user_id: [1u8; 32],
                name: "alice".to_string(),
                roles: vec![crate::permissions::ROLE_MEMBER],
                client_id: Some(9),
                last_seen: 5,
                keys: vec![KeySummary {
                    key_hash: [1u8; 32],
                    retired: false,
                    last_seen: 5,
                }],
            }])),
        }));
        let bytes = encode_server_message(&ServerMessage::AdminResult {
            request_id: 8,
            result: Err(AdminError::Outranked),
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes),
            Some(ServerMessage::AdminResult {
                request_id: 8,
                result: Err(AdminError::Outranked)
            })
        ));
        assert!(server_roundtrips(&ServerMessage::UserDetails {
            client_id: 9,
            user_id: [1u8; 32],
            roles: vec![1, 2],
            level: 3,
        }));
        assert!(server_roundtrips(&ServerMessage::UserProgress {
            client_id: 9,
            xp: 4_242,
            level: 7,
        }));
        let bytes = encode_server_message(&ServerMessage::Restricted {
            kind: RestrictionKind::GeneralText,
            until: None,
            reason: "be nice".to_string(),
        })
        .unwrap();
        assert!(matches!(
            decode_server_message(&bytes),
            Some(ServerMessage::Restricted {
                kind: RestrictionKind::GeneralText,
                until: None,
                ..
            })
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
