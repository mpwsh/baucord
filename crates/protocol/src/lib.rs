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

pub mod features;
pub mod frame;
pub mod lanes;
pub mod messages;
pub mod relayed;
pub mod system_audio;
pub mod username;
pub mod video;
pub mod voice;

// Explicit re-exports: everything a consumer needs, nothing more.
pub use bytes::Bytes;
pub use lanes::{
    ALPN, Carrier, DATAGRAM_PAYLOAD_LIMIT, Lane, OUTDATED_MESSAGE, SYSTEM_AUDIO_DATAGRAM_BUDGET,
    VIDEO_FRAME_BUDGET, VOICE_DATAGRAM_BUDGET,
};
pub use messages::{
    AudioSample, Auth, CLIENT_VERSION, ChannelDenyReason, ChannelInfo, ChatMessage, ClientId,
    ClientInfo, ClientKeyProof, ClientMessage, DenyReason, IMAGE_CHUNK_BYTES, MAX_CHAT_IMAGE_BYTES,
    MAX_CONTROL_MESSAGE_BYTES, MAX_IMAGE_CHUNKS, MAX_IMAGE_MESSAGE_BYTES, MAX_LOG_UPLOAD_BYTES,
    MAX_LOG_UPLOAD_CHUNKS, MEDIA_UPLOAD_PACE, PathSample, RefusedReason, ServerMessage, ShareScope,
    SharingSample, TelemetryEvent, TelemetrySample, UserId, ViewingSample, decode_client_message,
    decode_server_message, encode_client_message, encode_server_message,
};
pub use relayed::{RELAY_SENDER_BYTES, split_sender, wrap_sender};
pub use system_audio::{
    SYSTEM_AUDIO_HEADER_SIZE, SYSTEM_AUDIO_MTU_BUDGET, SystemAudioHeader, pack_system_audio,
    parse_system_audio,
};
pub use username::{MAX_USERNAME_BYTES, sanitize_username, truncate_on_char_boundary};
pub use video::{
    MAX_VIDEO_FRAME_BYTES, ReassembledFrame, VIDEO_CHUNK_PAYLOAD, VIDEO_HEADER_SIZE, VideoHeader,
    VideoReassembler, pack_video_frame, parse_video_header,
};
pub use voice::{
    VOICE_FLAG_RED, VOICE_FLAGS_KNOWN, VOICE_HEADER_SIZE, VOICE_MAX_FRAME_SIZE, VOICE_MTU_BUDGET,
    VoiceHeader, pack_voice, pack_voice_with_flags, parse_voice,
};
