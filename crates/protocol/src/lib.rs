//! Shared wire protocol for baucord client ↔ server communication.
//!
//! This crate is the **single source of truth** for everything both ends
//! must agree on: the lane table and its QUIC carriers, stream framing,
//! control message enums and their serialization, the capability bits,
//! and the binary formats of the media hot paths (voice / video / system
//! audio).
//!
//! Design rules:
//! - No I/O, no threads, no behavior beyond pure pack/parse.
//! - Every parse of network input returns `Option`/`Result` — a malformed
//!   packet must never be able to panic a relay or client thread.
//! - Control messages use `postcard` (compact varints); media packets use
//!   hand-framed fixed headers defined **only** here.
//! - Breaking the wire format ⇒ bump [`ALPN`] so old peers are refused in
//!   the TLS handshake instead of misbehaving mid-session. Additive
//!   changes ride on [`features`] bits (new messages, never new fields).

pub mod admin;
pub mod features;
pub mod frame;
pub mod invite;
pub mod lanes;
pub mod messages;
pub mod permissions;
pub mod progress;
pub mod relayed;
pub mod system_audio;
pub mod username;
pub mod video;
pub mod voice;

// Explicit re-exports: everything a consumer needs, nothing more.
pub use admin::{
    AdminCommand, AdminError, AdminOutcome, AdminTarget, AuditEntry, BlobSummary, RestrictionKind,
    StatsSummary, TelemetrySummary,
};
pub use bytes::Bytes;
pub use lanes::{
    Carrier, Lane, ALPN, DATAGRAM_PAYLOAD_LIMIT, OUTDATED_MESSAGE, SYSTEM_AUDIO_DATAGRAM_BUDGET,
    VIDEO_FRAME_BUDGET, VOICE_DATAGRAM_BUDGET,
};
pub use messages::{
    decode_client_message, decode_server_message, decode_telemetry_event, decode_telemetry_sample,
    encode_client_message, encode_server_message, encode_telemetry_event, encode_telemetry_sample,
    Attachment, AudioSample, Auth, BlobId, ChannelDenyReason, ChannelInfo, ChannelSpec,
    ChannelUiDenyReason, ChatMessage, ClientId, ClientInfo, ClientKeyProof, ClientMessage, ConvRef,
    DenyReason, DmFailReason, DmSummary, HistoryMessage, InviteDenyReason, InviteInvalidReason,
    NameStyle, NeedInviteReason, PathSample, Profile, ProfileDenyReason, RefusedReason,
    RemovedKind, RenameDenyReason, RoleInfo, ServerMessage, ShareScope, SharingSample,
    TelemetryEvent, TelemetrySample, UploadRejectReason, UserId, ViewingSample, CLIENT_VERSION,
    IMAGE_CHUNK_BYTES, MAX_BLOB_BYTES, MAX_BLOB_CHUNKS, MAX_CHAT_IMAGE_BYTES,
    MAX_CONTROL_MESSAGE_BYTES, MAX_IMAGE_CHUNKS, MAX_IMAGE_MESSAGE_BYTES, MAX_LOG_UPLOAD_BYTES,
    MAX_LOG_UPLOAD_CHUNKS, MEDIA_UPLOAD_PACE,
};
pub use permissions::{Rank, RoleId};
pub use progress::{cost_of_level, level_for_xp, xp_for_level};
pub use relayed::{split_sender, wrap_sender, RELAY_SENDER_BYTES};
pub use system_audio::{
    pack_system_audio, parse_system_audio, SystemAudioHeader, SYSTEM_AUDIO_HEADER_SIZE,
    SYSTEM_AUDIO_MTU_BUDGET,
};
pub use username::{sanitize_username, truncate_on_char_boundary, MAX_USERNAME_BYTES};
pub use video::{
    pack_video_frame, parse_video_header, ReassembledFrame, VideoHeader, VideoReassembler,
    MAX_VIDEO_FRAME_BYTES, VIDEO_CHUNK_PAYLOAD, VIDEO_HEADER_SIZE,
};
pub use voice::{
    pack_voice, pack_voice_with_flags, parse_voice, VoiceHeader, VOICE_FLAGS_KNOWN, VOICE_FLAG_RED,
    VOICE_HEADER_SIZE, VOICE_MAX_FRAME_SIZE, VOICE_MTU_BUDGET,
};
