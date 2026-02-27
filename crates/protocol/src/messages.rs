//! Reliable-channel message types.
//!
//! These enums are serialized with bincode and sent on [`RELIABLE_CHANNEL`].
//! Both client and server must use the **exact same** enum definition,
//! which is why they live here rather than being duplicated.
//!
//! **Backwards compatibility:** new variants must be appended at the end.
//! Inserting in the middle shifts bincode discriminants and breaks older
//! peers.

use renet::ClientId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Chat message wrapper ────────────────────────────────────────────────

/// A text message attributed to a specific client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub client_id: ClientId,
    pub text: String,
}

// ── Client → Server ─────────────────────────────────────────────────────

/// Messages sent from a client to the server on the reliable channel.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Send a text chat message to the room.
    Text(String),

    /// Legacy: voice data on reliable channel (unused, kept for compat).
    VoiceData(Vec<u8>),

    /// Our local speaking state changed.
    Speaking(bool),

    /// Broadcast our mute/deafen state to other users.
    AudioState { muted: bool, deafened: bool },

    // ── Screen share signaling ──────────────────────────────────────
    /// We started sharing our screen at the given resolution.
    ScreenShareStart { width: u32, height: u32 },

    /// We stopped sharing our screen.
    ScreenShareStop,

    /// Subscribe to another user's screen share stream.
    /// The server will start relaying their video frames to us.
    SubscribeScreen { target_id: u64 },

    /// Unsubscribe from another user's screen share stream.
    /// The server will stop relaying their video frames to us.
    UnsubscribeScreen { target_id: u64 },

    /// Request a keyframe from a specific sharer (viewer → server → sharer).
    RequestKeyframe { target_id: u64 },
}

// ── Server → Client ─────────────────────────────────────────────────────

/// Messages sent from the server to clients on the reliable channel.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// A new client connected to the server.
    ClientConnected {
        client_id: ClientId,
        username: String,
    },

    /// A client disconnected from the server.
    ClientDisconnected { client_id: ClientId },

    /// A chat message from another client.
    ClientMessage(ChatMessage),

    /// Initialization payload sent to a newly connected client.
    /// Contains the current user list, server name, and greeting.
    InitClient {
        usernames: HashMap<ClientId, String>,
        server_name: String,
        greeting: String,
    },

    /// Legacy: voice data on reliable channel (unused, kept for compat).
    VoiceData { client_id: ClientId, data: Vec<u8> },

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
    /// Sent to all clients in the room so they can show a "Watch" button.
    ScreenShareStarted {
        client_id: ClientId,
        width: u32,
        height: u32,
    },

    /// A client stopped sharing their screen.
    /// Sent to all clients in the room.
    ScreenShareStopped { client_id: ClientId },
    /// A viewer needs a keyframe from you.
    KeyframeRequested { viewer_id: ClientId },
}
