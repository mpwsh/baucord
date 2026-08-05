//! Media relay modules — voice and video.
//!
//! Each relay struct owns its validation logic, stats, and fan-out behavior.
//! The server calls `process()` once per tick; the relay reads from clients,
//! validates, and forwards to the appropriate recipients.

use log::{debug, info, warn};
use std::time::{Duration, Instant};
use renet::{ClientId, RenetServer};
use std::collections::{HashMap, HashSet};

use protocol::{
    SYSTEM_AUDIO_MTU_BUDGET, SystemAudioHeader, VIDEO_HEADER_SIZE, VOICE_MAX_FRAME_SIZE,
    VOICE_MAX_FRAMES_PER_PACKET, VOICE_MTU_BUDGET, VOICE_PROTOCOL_VERSION, VoiceHeader,
    parse_system_audio_header, parse_video_header,
};

// ════════════════════════════════════════════════════════════════════════
//  Voice relay — broadcast to all other clients in the room
// ════════════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct VoiceStats {
    pub packets_received: u64,
    pub packets_forwarded: u64,
    pub packets_dropped_malformed: u64,
    pub packets_dropped_oversize: u64,
    pub packets_dropped_version: u64,
    pub total_frames: u64,
    pub avg_frames_per_packet: f32,
}

pub struct VoiceRelay {
    pub stats: VoiceStats,
}

impl VoiceRelay {
    pub fn new() -> Self {
        Self {
            stats: VoiceStats::default(),
        }
    }

    /// Read voice packets from all clients and broadcast to everyone else.
    pub fn process(&mut self, server: &mut RenetServer, channel_id: u8) {
        let clients: Vec<ClientId> = server.clients_id();

        for client_id in &clients {
            while let Some(message) = server.receive_message(*client_id, channel_id) {
                self.stats.packets_received += 1;

                if !self.validate(&message, *client_id) {
                    continue;
                }

                // Parse frame_count for stats
                let frame_count = message[20] as usize;
                self.stats.total_frames += frame_count as u64;
                #[allow(clippy::cast_precision_loss)]
                {
                    self.stats.avg_frames_per_packet =
                        self.stats.avg_frames_per_packet * 0.95 + frame_count as f32 * 0.05;
                }

                // Forward raw bytes to all other clients.
                // Capture len before move — broadcast_message_except takes ownership.
                let msg_len = message.len();
                let recipients = clients.len().saturating_sub(1);

                server.broadcast_message_except(*client_id, channel_id, message);

                self.stats.packets_forwarded += 1;
                let _ = (msg_len, recipients); // available for byte stats if needed
            }
        }
    }

    /// Validate a voice packet. Returns true if valid, false if dropped.
    fn validate(&mut self, message: &[u8], client_id: ClientId) -> bool {
        // Size check
        if message.len() < VoiceHeader::SIZE + 3 {
            self.stats.packets_dropped_malformed += 1;
            return false;
        }

        if message.len() > VOICE_MTU_BUDGET {
            self.stats.packets_dropped_oversize += 1;
            warn!(
                "Dropping oversize voice packet: {} bytes from {client_id}",
                message.len(),
            );
            return false;
        }

        // Version check
        if message[0] != VOICE_PROTOCOL_VERSION {
            self.stats.packets_dropped_version += 1;
            warn!(
                "Invalid voice protocol version {} from {client_id}",
                message[0]
            );
            return false;
        }

        // Parse header fields for validation
        let mut off = 4; // skip version + reserved

        let hdr_client_id = u64::from_le_bytes(message[off..off + 8].try_into().unwrap());
        off += 8;
        let _seq = u32::from_le_bytes(message[off..off + 4].try_into().unwrap());
        off += 4;
        let _ts_48k = u32::from_le_bytes(message[off..off + 4].try_into().unwrap());
        off += 4;
        let frame_count = message[off] as usize;
        off += 4; // frame_count + padding

        // Frame count
        if frame_count == 0 || frame_count > VOICE_MAX_FRAMES_PER_PACKET {
            self.stats.packets_dropped_malformed += 1;
            warn!("Invalid frame count {frame_count} from {client_id}");
            return false;
        }

        // Client ID mismatch is informational only — transport guarantees sender
        if hdr_client_id != client_id {
            debug!("Header client_id {hdr_client_id} != sender {client_id} (stale session?)",);
        }

        // Validate all frame lengths
        let mut validation_offset = off;
        for i in 0..frame_count {
            if validation_offset + 2 > message.len() {
                self.stats.packets_dropped_malformed += 1;
                return false;
            }

            let len = u16::from_le_bytes(
                message[validation_offset..validation_offset + 2]
                    .try_into()
                    .unwrap(),
            ) as usize;
            validation_offset += 2;

            if len == 0 || len > VOICE_MAX_FRAME_SIZE {
                debug!("Invalid frame {i} size: {len} bytes from {client_id}");
                self.stats.packets_dropped_malformed += 1;
                return false;
            }

            if validation_offset + len > message.len() {
                self.stats.packets_dropped_malformed += 1;
                return false;
            }

            validation_offset += len;
        }

        if validation_offset != message.len() {
            self.stats.packets_dropped_malformed += 1;
            debug!(
                "Trailing bytes: expected {validation_offset}, got {} from {client_id}",
                message.len(),
            );
            return false;
        }

        true
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn print_stats(&self) {
        info!(
            "Voice packets — RX: {}, FWD: {}",
            self.stats.packets_received, self.stats.packets_forwarded
        );

        if self.stats.packets_received > 0 {
            let total_dropped = self.stats.packets_dropped_malformed
                + self.stats.packets_dropped_oversize
                + self.stats.packets_dropped_version;
            let drop_rate = total_dropped as f32 / self.stats.packets_received as f32 * 100.0;

            info!(
                "Voice drops: {} malformed, {} oversize, {} bad version ({drop_rate:.1}% total)",
                self.stats.packets_dropped_malformed,
                self.stats.packets_dropped_oversize,
                self.stats.packets_dropped_version,
            );
        }

        info!(
            "Avg frames/packet: {:.2}, Total frames: {}",
            self.stats.avg_frames_per_packet, self.stats.total_frames
        );
    }
}

// ════════════════════════════════════════════════════════════════════════
//  Video relay — subscription-based forwarding
// ════════════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct VideoStats {
    pub packets_received: u64,
    pub packets_forwarded: u64,
    pub packets_dropped_no_subscribers: u64,
    pub packets_dropped_malformed: u64,
    pub packets_dropped_oversize: u64,
    pub packets_dropped_not_sharing: u64,
    pub keyframes_relayed: u64,
    pub keyframe_requests_forwarded: u64,
    pub keyframe_requests_suppressed: u64,
    pub bytes_relayed: u64,
    // Chunk stats
    pub chunks_received: u64,
    pub chunks_forwarded: u64,
    // System audio stats
    pub sys_audio_received: u64,
    pub sys_audio_forwarded: u64,
    pub sys_audio_dropped_malformed: u64,
    pub sys_audio_dropped_oversize: u64,
    pub sys_audio_dropped_not_sharing: u64,
    pub sys_audio_dropped_no_subscribers: u64,
    pub sys_audio_bytes_relayed: u64,
}

pub struct VideoRelay {
    /// Which clients are currently screen sharing: client_id → (width, height).
    sharers: HashMap<ClientId, (u32, u32)>,
    /// Subscription map: streamer_id → set of viewer client_ids.
    subscribers: HashMap<ClientId, HashSet<ClientId>>,
    /// Last time a keyframe request was forwarded to each sharer.
    ///
    /// Per-sharer server-side rate limit: viewers self-limit to one
    /// request per 700 ms, but N viewers could still multiply that by N.
    /// Requests suppressed here are harmless — the IDR produced for the
    /// request that *did* pass serves every viewer of that stream.
    keyframe_request_gate: HashMap<ClientId, Instant>,
    pub stats: VideoStats,
}

/// Minimum spacing between keyframe requests forwarded to one sharer.
/// Kept below the client-side cooldown (700 ms) so a single viewer is
/// never throttled by the server, only stampedes are.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(300);

impl VideoRelay {
    pub fn new() -> Self {
        Self {
            sharers: HashMap::new(),
            subscribers: HashMap::new(),
            keyframe_request_gate: HashMap::new(),
            stats: VideoStats::default(),
        }
    }

    // ── Subscription management (called from reliable message handler) ──

    /// A client started sharing their screen.
    pub fn start_sharing(&mut self, client_id: ClientId, width: u32, height: u32) {
        info!("Screen share started: {client_id} ({width}x{height})");
        self.sharers.insert(client_id, (width, height));
        self.subscribers.entry(client_id).or_default();
    }

    /// A client stopped sharing their screen.
    pub fn stop_sharing(&mut self, client_id: ClientId) {
        info!("Screen share stopped: {client_id}");
        self.sharers.remove(&client_id);
        self.subscribers.remove(&client_id);
        self.keyframe_request_gate.remove(&client_id);
    }

    /// A viewer wants to watch a streamer.
    pub fn subscribe(&mut self, viewer: ClientId, streamer: ClientId) {
        if self.sharers.contains_key(&streamer) {
            self.subscribers.entry(streamer).or_default().insert(viewer);
            info!("{viewer} subscribed to {streamer}'s screen");
        } else {
            debug!("{viewer} tried to subscribe to {streamer} who is not sharing");
        }
    }

    /// A viewer stops watching a streamer.
    pub fn unsubscribe(&mut self, viewer: ClientId, streamer: ClientId) {
        if let Some(subs) = self.subscribers.get_mut(&streamer) {
            subs.remove(&viewer);
        }
    }

    /// Clean up when a client disconnects — stop their share and remove
    /// them from all subscription lists.
    pub fn handle_disconnect(&mut self, client_id: ClientId) {
        // If they were sharing, stop it
        let was_sharing = self.sharers.remove(&client_id).is_some();
        if was_sharing {
            self.subscribers.remove(&client_id);
            self.keyframe_request_gate.remove(&client_id);
        }

        // Remove them as a viewer from everyone else
        for subs in self.subscribers.values_mut() {
            subs.remove(&client_id);
        }
    }

    /// Returns true if the client is currently sharing.
    pub fn is_sharing(&self, client_id: ClientId) -> bool {
        self.sharers.contains_key(&client_id)
    }

    /// Returns true if `viewer` is subscribed to `streamer`'s stream.
    pub fn is_subscribed(&self, viewer: ClientId, streamer: ClientId) -> bool {
        self.subscribers
            .get(&streamer)
            .is_some_and(|subs| subs.contains(&viewer))
    }

    /// Rate-gate keyframe requests toward `streamer`.
    ///
    /// Returns true (and arms the gate) if enough time has passed since
    /// the last forwarded request; the caller then forwards it. Counted
    /// either way in stats.
    pub fn allow_keyframe_request(&mut self, streamer: ClientId) -> bool {
        let now = Instant::now();
        let allowed = self
            .keyframe_request_gate
            .get(&streamer)
            .is_none_or(|last| now.duration_since(*last) >= KEYFRAME_REQUEST_MIN_INTERVAL);
        if allowed {
            self.keyframe_request_gate.insert(streamer, now);
            self.stats.keyframe_requests_forwarded += 1;
        } else {
            self.stats.keyframe_requests_suppressed += 1;
        }
        allowed
    }

    /// Returns the set of active sharers (for sending state to newly connected clients).
    pub fn sharers(&self) -> &HashMap<ClientId, (u32, u32)> {
        &self.sharers
    }

    // ── Packet relay (called every tick) ────────────────────────────

    pub fn process(&mut self, server: &mut RenetServer, video_channel_id: u8) {
        let clients: Vec<ClientId> = server.clients_id();

        // All video arrives as chunks on the unreliable channel
        for client_id in &clients {
            while let Some(message) = server.receive_message(*client_id, video_channel_id) {
                self.relay_video_chunk(server, *client_id, &message, video_channel_id);
            }
        }
    }

    /// Relay a video chunk (VideoChunkHeader format) on the unreliable channel.
    ///
    /// Each chunk is a small (<1.2 KB) self-contained message — no renet
    /// fragmentation. The server doesn't reassemble; it just validates the
    /// header and forwards to subscribers. The client reassembles.
    fn relay_video_chunk(
        &mut self,
        server: &mut RenetServer,
        client_id: ClientId,
        message: &bytes::Bytes,
        outbound_channel: u8,
    ) {
        self.stats.chunks_received += 1;

        // Validate chunk header
        if message.len() < VIDEO_HEADER_SIZE {
            self.stats.packets_dropped_malformed += 1;
            return;
        }

        let Some(hdr) = parse_video_header(&message) else {
            self.stats.packets_dropped_malformed += 1;
            return;
        };

        // Basic sanity: chunk_count > 0, chunk_idx < chunk_count
        if hdr.chunk_count == 0 || hdr.chunk_idx >= hdr.chunk_count {
            self.stats.packets_dropped_malformed += 1;
            return;
        }

        // Only relay if sender is actually sharing
        if !self.sharers.contains_key(&client_id) {
            self.stats.packets_dropped_not_sharing += 1;
            return;
        }

        // Get subscriber list
        let Some(subs) = self.subscribers.get(&client_id) else {
            self.stats.packets_dropped_no_subscribers += 1;
            return;
        };

        if subs.is_empty() {
            self.stats.packets_dropped_no_subscribers += 1;
            return;
        }

        // Forward to each subscriber
        let msg_len = message.len();
        let mut sent_count = 0u64;

        for viewer_id in subs {
            if server.is_connected(*viewer_id) {
                server.send_message(*viewer_id, outbound_channel, message.clone());
                sent_count += 1;
            }
        }

        self.stats.chunks_forwarded += 1;
        self.stats.bytes_relayed += msg_len as u64 * sent_count;
    }

    // ── System audio relay (called every tick) ──────────────────────

    /// Read system audio packets from all clients on the system audio
    /// channel and forward to the same subscribers as video.
    pub fn process_system_audio(&mut self, server: &mut RenetServer, system_audio_channel_id: u8) {
        let clients: Vec<ClientId> = server.clients_id();

        for client_id in &clients {
            while let Some(message) = server.receive_message(*client_id, system_audio_channel_id) {
                self.relay_system_audio_packet(
                    server,
                    *client_id,
                    &message,
                    system_audio_channel_id,
                );
            }
        }
    }

    fn relay_system_audio_packet(
        &mut self,
        server: &mut RenetServer,
        client_id: ClientId,
        message: &bytes::Bytes,
        system_audio_channel_id: u8,
    ) {
        self.stats.sys_audio_received += 1;

        // Validate header size
        if message.len() < SystemAudioHeader::SIZE {
            self.stats.sys_audio_dropped_malformed += 1;
            return;
        }

        if message.len() > SYSTEM_AUDIO_MTU_BUDGET {
            self.stats.sys_audio_dropped_oversize += 1;
            warn!(
                "Dropping oversize system audio packet: {} bytes from {client_id}",
                message.len(),
            );
            return;
        }

        let Some(hdr) = parse_system_audio_header(&message) else {
            self.stats.sys_audio_dropped_malformed += 1;
            return;
        };

        // Verify payload length matches header
        if SystemAudioHeader::SIZE + hdr.data_len as usize > message.len() {
            self.stats.sys_audio_dropped_malformed += 1;
            return;
        }

        // Only relay if sender is actually sharing
        if !self.sharers.contains_key(&client_id) {
            self.stats.sys_audio_dropped_not_sharing += 1;
            return;
        }

        // Get subscriber list (same as video)
        let Some(subs) = self.subscribers.get(&client_id) else {
            self.stats.sys_audio_dropped_no_subscribers += 1;
            return;
        };

        if subs.is_empty() {
            self.stats.sys_audio_dropped_no_subscribers += 1;
            return;
        }

        // Forward to each subscriber on the same unreliable channel
        let msg_len = message.len();
        let mut sent_count = 0u64;

        for viewer_id in subs {
            if server.is_connected(*viewer_id) {
                server.send_message(*viewer_id, system_audio_channel_id, message.clone());
                sent_count += 1;
            }
        }

        self.stats.sys_audio_forwarded += 1;
        self.stats.sys_audio_bytes_relayed += msg_len as u64 * sent_count;
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn print_stats(&self) {
        if self.stats.packets_received == 0
            && self.stats.chunks_received == 0
            && self.stats.sys_audio_received == 0
            && self.sharers.is_empty()
        {
            return; // don't print stats if never used
        }

        info!(
            "Video — keyframe packets RX: {}, FWD: {}, keyframes: {}",
            self.stats.packets_received, self.stats.packets_forwarded, self.stats.keyframes_relayed,
        );
        info!(
            "Video — chunks RX: {}, FWD: {}",
            self.stats.chunks_received, self.stats.chunks_forwarded,
        );
        info!(
            "Video drops — no subs: {}, not sharing: {}, malformed: {}, oversize: {}",
            self.stats.packets_dropped_no_subscribers,
            self.stats.packets_dropped_not_sharing,
            self.stats.packets_dropped_malformed,
            self.stats.packets_dropped_oversize,
        );
        info!(
            "Video relayed: {:.2} MB, Active streams: {}, Total subscribers: {}",
            self.stats.bytes_relayed as f64 / 1_048_576.0,
            self.sharers.len(),
            self.subscribers.values().map(|s| s.len()).sum::<usize>(),
        );
        if self.stats.keyframe_requests_forwarded + self.stats.keyframe_requests_suppressed > 0 {
            info!(
                "Keyframe requests — forwarded: {}, rate-limited: {}",
                self.stats.keyframe_requests_forwarded, self.stats.keyframe_requests_suppressed,
            );
        }

        if self.stats.sys_audio_received > 0 {
            info!(
                "System audio — RX: {}, FWD: {}, relayed: {:.2} MB",
                self.stats.sys_audio_received,
                self.stats.sys_audio_forwarded,
                self.stats.sys_audio_bytes_relayed as f64 / 1_048_576.0,
            );
            let dropped = self.stats.sys_audio_dropped_malformed
                + self.stats.sys_audio_dropped_oversize
                + self.stats.sys_audio_dropped_not_sharing
                + self.stats.sys_audio_dropped_no_subscribers;
            if dropped > 0 {
                info!(
                    "System audio drops — no subs: {}, not sharing: {}, malformed: {}, oversize: {}",
                    self.stats.sys_audio_dropped_no_subscribers,
                    self.stats.sys_audio_dropped_not_sharing,
                    self.stats.sys_audio_dropped_malformed,
                    self.stats.sys_audio_dropped_oversize,
                );
            }
        }
    }
}
