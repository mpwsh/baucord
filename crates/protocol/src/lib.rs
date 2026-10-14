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
pub mod safety;
pub mod system_audio;
pub mod username;
pub mod video;
pub mod voice;

// Explicit re-exports: everything a consumer needs, nothing more.
pub use admin::{
    AdminCommand, AdminError, AdminOutcome, AdminTarget, AuditEntry, BlobSummary, RestrictionKind,
    StatsSummary, TelemetrySummary, MAX_ADMIN_REASON_BYTES,
};
pub use bytes::Bytes;
pub use lanes::{
    Lane, ALPN, DATAGRAM_PAYLOAD_LIMIT, OUTDATED_MESSAGE, SYSTEM_AUDIO_DATAGRAM_BUDGET,
    VIDEO_FRAME_BUDGET, VOICE_DATAGRAM_BUDGET,
};
pub use messages::{
    decode_client_message, decode_server_message, decode_telemetry_event, decode_telemetry_sample,
    encode_client_message, encode_server_message, encode_telemetry_event, encode_telemetry_sample,
    keep_prefix_that_fits, keep_suffix_that_fits, split_to_fit, Attachment, AudioSample, Auth,
    AuthorInfo, BlobId, ChannelDenyReason, ChannelInfo, ChannelSpec, ChannelUiDenyReason,
    ChatMessage, ClientId, ClientInfo, ClientKeyProof, ClientMessage, ConvRef, DenyReason,
    DmFailReason, DmSummary, HistoryMessage, InviteDenyReason, InviteInvalidReason, NameStyle,
    NeedInviteReason, PathSample, Profile, ProfileDenyReason, RefusedReason, RemovedKind,
    RenameDenyReason, RoleInfo, ServerMessage, ShareScope, SharingSample, TelemetryEvent,
    TelemetrySample, UploadRejectReason, UserId, ViewingSample, CLIENT_VERSION, IMAGE_CHUNK_BYTES,
    LIST_ENVELOPE_BYTES, LIST_ITEMS_BUDGET, MAX_BLOB_BYTES, MAX_BLOB_CHUNKS, MAX_CHAT_IMAGE_BYTES,
    MAX_CONTROL_MESSAGE_BYTES, MAX_IMAGE_CHUNKS, MAX_IMAGE_MESSAGE_BYTES, MAX_LOG_UPLOAD_BYTES,
    MAX_LOG_UPLOAD_CHUNKS, MEDIA_UPLOAD_PACE,
};
pub use permissions::{Rank, RoleId};
pub use progress::{cost_of_level, level_for_xp, xp_for_level};
pub use relayed::{split_sender, wrap_sender, RELAY_SENDER_BYTES};
pub use safety::{
    BlockedUser, DeleteRefusal, ReportReason, ReportRefusal, ReportSummary, ReportTarget,
    MAX_REPORT_EXCERPT_BYTES, MAX_REPORT_NOTE_BYTES,
};
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

/// A full 64-hex-digit id (a user id, a key hash, a blob id) as its 32
/// bytes; `None` for anything else. The one parser the server's CLI and
/// the client's commands share. It reads bytes, not `str` slices: the
/// two copies it replaces sliced by byte offset and panicked on a
/// 64-byte input holding a multi-byte character (review P2-15).
#[must_use]
pub fn parse_hex_id(text: &str) -> Option<[u8; 32]> {
    let digits = text.trim().as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *byte = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod hex_id_tests {
    use super::parse_hex_id;

    #[test]
    fn a_hex_id_is_64_hex_digits_and_nothing_else() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let id: [u8; 32] = std::array::from_fn(|i| u8::try_from(i).unwrap());
        assert_eq!(parse_hex_id(hex), Some(id));
        assert_eq!(
            parse_hex_id(&format!("  {}\n", hex.to_uppercase())),
            Some(id)
        );
        assert_eq!(parse_hex_id(&hex[..62]), None);
        assert_eq!(parse_hex_id(&format!("{}zz", &hex[..62])), None);
        // `from_str_radix` took a sign; an id has none.
        assert_eq!(parse_hex_id(&format!("+f{}", &hex[2..])), None);
    }

    #[test]
    fn multi_byte_input_is_refused_not_sliced() {
        // 64 bytes, but not 64 characters: byte-offset slicing landed
        // inside a character and panicked.
        let text = format!("é{}", "a".repeat(62));
        assert_eq!(text.len(), 64);
        assert_eq!(parse_hex_id(&text), None);
        let text = format!("{}é", "a".repeat(62));
        assert_eq!(parse_hex_id(&text), None);
        let text = "\u{3000}".repeat(21) + "a";
        assert_eq!(text.len(), 64);
        assert_eq!(parse_hex_id(&text), None);
    }
}
