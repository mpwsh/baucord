//! Shared wire protocol for baucord client ↔ server communication.
//!
//! This crate is the **single source of truth** for everything both ends
//! must agree on: channel layout, renet [`ConnectionConfig`], control
//! message enums and their serialization, and the binary formats of the
//! media hot paths (voice / video / system audio).
//!
//! Design rules:
//! - No I/O, no threads, no behavior beyond pure pack/parse.
//! - Every parse of network input returns `Option`/`Result` — a malformed
//!   packet must never be able to panic a relay or client thread.
//! - Control messages use `postcard` (compact varints); media packets use
//!   hand-framed fixed headers defined **only** here.
//! - Breaking the wire format ⇒ bump [`PROTOCOL_ID`] so old peers are
//!   refused at the netcode handshake instead of misbehaving mid-session.
//!
//! [`ConnectionConfig`]: renet::ConnectionConfig

pub mod channels;
pub mod messages;
pub mod relayed;
pub mod system_audio;
pub mod user_data;
pub mod video;
pub mod voice;

// Explicit re-exports: everything a consumer needs, nothing more.
pub use channels::{
    CLIENT_BYTES_PER_TICK, CONTROL_CHANNEL, MEDIA_CHANNEL, PROTOCOL_ID, SERVER_BYTES_PER_TICK, SIGNAL_CHANNEL,
    SYSTEM_AUDIO_CHANNEL, UDP_SOCKET_BUFFER_BYTES, VIDEO_BUFFER_BYTES, VIDEO_CHANNEL,
    VOICE_CHANNEL, connection_config,
};
pub use messages::{
    ChannelDenyReason, ChannelInfo, ChatMessage, ClientMessage, DenyReason, IMAGE_CHUNK_BYTES,
    MAX_CHAT_IMAGE_BYTES, MAX_CONTROL_MESSAGE_BYTES, MAX_IMAGE_CHUNKS, MAX_IMAGE_MESSAGE_BYTES,
    MEDIA_UPLOAD_PACE, ServerMessage, ShareScope, decode_client_message, decode_server_message,
    encode_client_message, encode_server_message,
};
pub use relayed::{RELAY_SENDER_BYTES, split_sender, wrap_sender};
pub use system_audio::{
    SYSTEM_AUDIO_HEADER_SIZE, SYSTEM_AUDIO_MTU_BUDGET, SystemAudioHeader, pack_system_audio,
    parse_system_audio,
};
pub use user_data::{
    MAX_USERNAME_BYTES, USER_DATA_BYTES, decode_username, encode_username, sanitize_username,
};
pub use video::{
    MAX_VIDEO_FRAME_BYTES, ReassembledFrame, VIDEO_CHUNK_PAYLOAD, VIDEO_HEADER_SIZE, VideoHeader,
    VideoReassembler, pack_video_frame, parse_video_header,
};
pub use voice::{
    VOICE_HEADER_SIZE, VOICE_MAX_FRAME_SIZE, VOICE_MTU_BUDGET, VoiceHeader, pack_voice, parse_voice,
};
