//! Channel IDs and shared network constants.
//!
//! Renet multiplexes multiple logical channels over one UDP connection.
//! Each channel has its own reliability and ordering semantics.

/// Application-level protocol identifier.
///
/// Both client and server must agree on this value or the netcode
/// handshake will reject the connection.
pub const PROTOCOL_ID: u64 = 27;

// ── Channel IDs ─────────────────────────────────────────────────────────

/// Reliable ordered channel for chat, signaling, presence, and control
/// messages. Guaranteed delivery, in-order, with automatic retransmit.
pub const RELIABLE_CHANNEL: u8 = 2;

/// Unreliable channel for voice (Opus) packets.
/// Low latency, no retransmit — late packets are useless for real-time audio.
pub const VOICE_CHANNEL: u8 = 3;

/// Unreliable channel for video (H.264) chunks.
/// All video — keyframes and P-frames — goes here as application-layer
/// chunks. Lost keyframes are recovered via PLI on the reliable channel.
pub const VIDEO_CHANNEL: u8 = 4;

/// Unreliable channel for system audio (Opus stereo) during screen share.
/// Separate from voice so subscriber routing can mirror the video stream,
/// and so deafening voice chat doesn't kill stream audio.
pub const SYSTEM_AUDIO_CHANNEL: u8 = 6;

// ── Buffer sizes ────────────────────────────────────────────────────────

/// Default memory budget for the reliable channel (bytes).
pub const RELIABLE_BUFFER_BYTES: usize = 5 * 1024 * 1024; // 5 MB


/// Default memory budget for the voice channel (bytes).
pub const VOICE_BUFFER_BYTES: usize = 5 * 1024 * 1024; // 5 MB

/// Default memory budget for the video channel (bytes).
pub const VIDEO_BUFFER_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Default memory budget for the system audio channel (bytes).
///
/// System audio is ~16 KB/s (320-byte Opus frames × 50 packets/sec).
/// 512 KB gives ~32 seconds of buffering — more than enough for renet's
/// internal bookkeeping on an unreliable channel.
pub const SYSTEM_AUDIO_BUFFER_BYTES: usize = 512 * 1024; // 512 KB

/// Target OS-level UDP socket buffer size (send and receive).
///
/// The Linux default is 208 KB, which is too small for relaying video
/// keyframes (~136 chunks × ~1300 bytes = ~177 KB per burst). When the
/// OS send buffer fills, `send_to` fails silently and keepalive packets
/// get dropped, causing netcode timeouts.
///
/// 8 MB gives comfortable headroom for multiple keyframes in flight.
pub const UDP_SOCKET_BUFFER_BYTES: usize = 8 * 1024 * 1024; // 8 MB