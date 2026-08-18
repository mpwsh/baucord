//! Channel layout, buffer budgets, and the shared renet [`ConnectionConfig`].
//!
//! Both client and server build their connection from
//! [`connection_config`], so the channel set can never drift between the
//! two ends. Channel IDs are **not** configurable at runtime on purpose —
//! a mismatched channel map is an undebuggable failure mode.
//!
//! ## Channel map
//!
//! | id | name         | semantics            | carries                              |
//! |----|--------------|----------------------|--------------------------------------|
//! | 0  | control      | reliable, ordered    | chat (text+images), presence, share  |
//! | 1  | signal       | reliable, unordered  | keyframe requests, stream health     |
//! | 2  | voice        | unreliable           | Opus voice packets                   |
//! | 3  | video        | unreliable           | H.264 chunks                         |
//! | 4  | system audio | unreliable           | Opus stereo (screen-share audio)     |
//! | 5  | media        | reliable, unordered  | chat images/GIFs (bulk transfers)    |
//!
//! Splitting `signal` from `control` means a burst of chat or presence
//! traffic can never head-of-line-block a keyframe request — the one
//! control message that must be timely for stream recovery. `signal` is
//! reliable-unordered: requests must arrive, but their relative order is
//! irrelevant (they are idempotent and rate-gated server-side).
//!
//! `media` exists for the same reason in the other direction: chat
//! images/GIFs (up to [`crate::MAX_CHAT_IMAGE_BYTES`]) ride a separate
//! reliable-*unordered* channel so a 10 MB transfer never head-of-line
//! blocks the ordered text stream — at 5 Mbit/s that stall would be
//! ~16 s of frozen chat per queued image. Unordered is safe: rows are
//! self-contained and the UI appends by arrival.
//!
//! ## Priority & pacing
//!
//! renet packs outgoing packets by walking the channel Vec **in order**,
//! each channel consuming from `available_bytes_per_tick` before passing
//! the remainder down. The Vec below is therefore ordered
//! control → signal → voice → system audio → video → media, so neither
//! a keyframe burst nor a bulk image can starve voice or control within
//! a tick.
//!
//! `available_bytes_per_tick` doubles as burst pacing: at the server's
//! ~1 ms tick, [`SERVER_BYTES_PER_TICK`] (20 KB) spreads a ~175 KB
//! keyframe over ~9 ms instead of dumping 130+ datagrams onto the wire
//! back-to-back — the burst pattern that overflows home-router queues.
//! The ceiling is 20 KB/ms ≈ 160 Mbit/s per client: far above any real
//! stream, so it never throttles steady-state traffic.

use std::time::Duration;

use renet::{ChannelConfig, ConnectionConfig, SendType};

/// Application-level protocol identifier.
///
/// Both ends must agree or the netcode handshake refuses the connection.
/// **Bump this on every breaking wire change** — that is the whole
/// versioning story: old peers are rejected at the door rather than
/// misparsing packets mid-session.
pub const PROTOCOL_ID: u64 = 32;

// ── Channel IDs ─────────────────────────────────────────────────────────

/// Reliable ordered: chat (text and images), presence, share signaling.
pub const CONTROL_CHANNEL: u8 = 0;
/// Reliable unordered: keyframe requests/pokes, stream health reports.
pub const SIGNAL_CHANNEL: u8 = 1;
/// Unreliable: voice (Opus) packets. Late packets are useless.
pub const VOICE_CHANNEL: u8 = 2;
/// Unreliable: video (H.264) chunks. Loss recovered via keyframe request.
pub const VIDEO_CHANNEL: u8 = 3;
/// Unreliable: system audio (Opus stereo) during screen share.
/// Separate from voice so deafening voice chat doesn't kill stream audio.
pub const SYSTEM_AUDIO_CHANNEL: u8 = 4;
/// Reliable unordered: chat images and GIFs. Bulk transfers live here
/// so they never head-of-line block ordered chat text on `control`.
pub const MEDIA_CHANNEL: u8 = 5;

// ── Per-channel memory budgets ──────────────────────────────────────────
//
// For unreliable channels this is burst absorption, not durable
// buffering: renet drops new messages once the budget is exhausted.

/// Control channel budget. A reliable channel that hits its ceiling
/// disconnects the client (`ReliableChannelMaxMemoryReached`). Images
/// moved to [`MEDIA_CHANNEL`], so this only carries text/presence/share
/// signaling again — 1 MiB is thousands of messages of headroom.
pub const CONTROL_BUFFER_BYTES: usize = 1024 * 1024;

/// Media channel budget. Same disconnect-on-ceiling hazard as every
/// reliable channel, so it must hold several worst-case images
/// ([`crate::MAX_CHAT_IMAGE_BYTES`] = 10 MiB) in flight toward a slow
/// client: 4× the cap. This is per-client worst-case buffering on the
/// server — N clients × 40 MiB transient if everyone is slow at once.
pub const MEDIA_BUFFER_BYTES: usize = 4 * crate::MAX_CHAT_IMAGE_BYTES;

/// Signal channel budget. Messages here are tiny (tens of bytes) and
/// rate-gated; 256 KB is thousands of messages of headroom.
pub const SIGNAL_BUFFER_BYTES: usize = 256 * 1024;

/// Voice channel budget. Voice is ~10 KB/s per speaker; 512 KB absorbs
/// multi-second scheduling hiccups for a full room.
pub const VOICE_BUFFER_BYTES: usize = 512 * 1024;

/// Video channel budget. Must absorb several keyframes in flight toward
/// a slow viewer before the relay's backpressure kicks in.
pub const VIDEO_BUFFER_BYTES: usize = 10 * 1024 * 1024;

/// System audio budget. ~16 KB/s stream; 512 KB is ample.
pub const SYSTEM_AUDIO_BUFFER_BYTES: usize = 512 * 1024;

// ── Pacing budgets ──────────────────────────────────────────────────────

/// Per-tick outbound byte budget, server side.
///
/// ⚠ This is NOT pacing. renet's unreliable channels drain their whole
/// queue every tick and silently DROP any message over the remaining
/// budget (`unreliable.rs`: "Drop message, no available bytes to
/// send"). The budget must therefore exceed the worst burst ever
/// queued between two ticks — a max video frame is ~278 KB of chunks
/// queued in one call — or keyframes are silently beheaded and streams
/// never start. 4 MB is ~14× that worst case; burst absorption is the
/// OS socket buffer's job, congestion response is StreamHealth's.
pub const SERVER_BYTES_PER_TICK: u64 = 4 * 1024 * 1024;

/// Per-tick outbound byte budget, client side. Same hazard as
/// [`SERVER_BYTES_PER_TICK`]: this must never bind, or the tail of
/// every keyframe is dropped at the sharer.
pub const CLIENT_BYTES_PER_TICK: u64 = 4 * 1024 * 1024;

/// Target OS-level UDP socket buffer size (send and receive).
///
/// Pacing (above) smooths most bursts, but the OS buffer is the last
/// line of defense: when it fills, `send_to` fails and keepalives get
/// dropped, causing netcode timeouts. 8 MB is cheap insurance.
pub const UDP_SOCKET_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Resend interval for the ordered control channel.
const CONTROL_RESEND: Duration = Duration::from_millis(200);
/// Resend interval for the media channel — bulk data, no urgency.
const MEDIA_RESEND: Duration = Duration::from_millis(300);
/// Resend interval for the signal channel — faster, because keyframe
/// requests are latency-sensitive and only bytes-tiny.
const SIGNAL_RESEND: Duration = Duration::from_millis(100);

/// Build the [`ConnectionConfig`] shared by client and server.
///
/// `available_bytes_per_tick` differs per side ([`SERVER_BYTES_PER_TICK`]
/// / [`CLIENT_BYTES_PER_TICK`], or a server-config override); the channel
/// set is identical by construction.
#[must_use]
pub fn connection_config(available_bytes_per_tick: u64) -> ConnectionConfig {
    // Vec order = send priority (see module docs). IDs are stable and
    // independent of this ordering.
    let channels = vec![
        ChannelConfig {
            channel_id: CONTROL_CHANNEL,
            max_memory_usage_bytes: CONTROL_BUFFER_BYTES,
            send_type: SendType::ReliableOrdered {
                resend_time: CONTROL_RESEND,
            },
        },
        ChannelConfig {
            channel_id: SIGNAL_CHANNEL,
            max_memory_usage_bytes: SIGNAL_BUFFER_BYTES,
            send_type: SendType::ReliableUnordered {
                resend_time: SIGNAL_RESEND,
            },
        },
        ChannelConfig {
            channel_id: VOICE_CHANNEL,
            max_memory_usage_bytes: VOICE_BUFFER_BYTES,
            send_type: SendType::Unreliable,
        },
        ChannelConfig {
            channel_id: SYSTEM_AUDIO_CHANNEL,
            max_memory_usage_bytes: SYSTEM_AUDIO_BUFFER_BYTES,
            send_type: SendType::Unreliable,
        },
        ChannelConfig {
            channel_id: VIDEO_CHANNEL,
            max_memory_usage_bytes: VIDEO_BUFFER_BYTES,
            send_type: SendType::Unreliable,
        },
        // Last on purpose: bulk media yields the per-tick byte budget
        // to everything realtime above it. With 4 MB/tick of headroom
        // this rarely binds, but when it does, this is the right loser.
        ChannelConfig {
            channel_id: MEDIA_CHANNEL,
            max_memory_usage_bytes: MEDIA_BUFFER_BYTES,
            send_type: SendType::ReliableUnordered {
                resend_time: MEDIA_RESEND,
            },
        },
    ];

    ConnectionConfig {
        available_bytes_per_tick,
        server_channels_config: channels.clone(),
        client_channels_config: channels,
    }
}
