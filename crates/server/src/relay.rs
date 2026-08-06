//! Media relay — voice and video fan-out.
//!
//! Both relays share the same trust model: media arriving from a client
//! carries **no sender identity**. The relay validates the packet with
//! the shared `protocol` parsers (the only parsers that exist — P2-1),
//! then prepends the sender's authenticated transport id via
//! [`protocol::wrap_sender`] before fan-out. A client cannot impersonate
//! another client's voice or video, full stop.
//!
//! The video relay additionally implements per-viewer backpressure: a
//! viewer whose outbound video queue grows past a watermark stops
//! receiving delta frames (which would be useless without their
//! predecessors anyway) and is resumed at the next keyframe boundary
//! once the queue drains — the classic SFU pattern. Without this, renet
//! tail-drops arbitrary chunks once the channel budget fills, corrupting
//! every subsequent frame while still burning bandwidth on them.

use log::{debug, info, warn};
use renet::{Bytes, ClientId, RenetServer};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use protocol::{
    SYSTEM_AUDIO_CHANNEL, VIDEO_BUFFER_BYTES, VIDEO_CHANNEL, VOICE_CHANNEL, parse_system_audio,
    parse_video_header, parse_voice, wrap_sender,
};

// ════════════════════════════════════════════════════════════════════════
//  Voice relay — broadcast to all other (non-deafened) clients
// ════════════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct VoiceStats {
    pub packets_received: u64,
    pub packets_forwarded: u64,
    pub packets_dropped_malformed: u64,
    /// Per-recipient sends skipped because the recipient is deafened.
    pub sends_skipped_deafened: u64,
    pub bytes_relayed: u64,
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

    /// Read voice packets from all clients and forward to everyone else,
    /// skipping deafened recipients — they discard the audio anyway, so
    /// relaying it is pure wasted egress (a deafened client in a busy
    /// room was previously still downloading every speaker's stream).
    pub fn process(&mut self, server: &mut RenetServer, deafened: &HashSet<ClientId>) {
        let clients: Vec<ClientId> = server.clients_id();

        for sender in &clients {
            while let Some(message) = server.receive_message(*sender, VOICE_CHANNEL) {
                self.stats.packets_received += 1;

                // Single source of validation: the shared parser.
                if parse_voice(&message).is_none() {
                    self.stats.packets_dropped_malformed += 1;
                    continue;
                }

                // Stamp the authenticated sender once; fan out refcounted.
                let envelope = Bytes::from(wrap_sender(*sender, &message));
                let mut sent = 0u64;
                for recipient in &clients {
                    if recipient == sender {
                        continue;
                    }
                    if deafened.contains(recipient) {
                        self.stats.sends_skipped_deafened += 1;
                        continue;
                    }
                    server.send_message(*recipient, VOICE_CHANNEL, envelope.clone());
                    sent += 1;
                }

                if sent > 0 {
                    self.stats.packets_forwarded += 1;
                    self.stats.bytes_relayed += envelope.len() as u64 * sent;
                }
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn print_stats(&self) {
        if self.stats.packets_received == 0 {
            return;
        }
        info!(
            "Voice — RX: {}, FWD: {}, relayed: {:.2} MB",
            self.stats.packets_received,
            self.stats.packets_forwarded,
            self.stats.bytes_relayed as f64 / 1_048_576.0,
        );
        if self.stats.packets_dropped_malformed > 0 || self.stats.sends_skipped_deafened > 0 {
            info!(
                "Voice — malformed drops: {}, deafened skips: {}",
                self.stats.packets_dropped_malformed, self.stats.sends_skipped_deafened,
            );
        }
    }
}

// ════════════════════════════════════════════════════════════════════════
//  Video relay — subscription-based forwarding with backpressure
// ════════════════════════════════════════════════════════════════════════

#[derive(Default)]
pub struct VideoStats {
    pub chunks_received: u64,
    pub chunks_forwarded: u64,
    /// Per-viewer chunk sends suppressed by backpressure.
    pub chunks_suppressed_backpressure: u64,
    pub chunks_dropped_malformed: u64,
    pub chunks_dropped_not_sharing: u64,
    pub chunks_dropped_no_subscribers: u64,
    /// Keyframe frame-starts forwarded to at least one viewer.
    pub keyframes_relayed: u64,
    pub keyframe_requests_forwarded: u64,
    pub keyframe_requests_suppressed: u64,
    /// Times a viewer entered the backpressure (waiting-for-keyframe) state.
    pub backpressure_marks: u64,
    pub bytes_relayed: u64,
    // System audio
    pub sys_audio_received: u64,
    pub sys_audio_forwarded: u64,
    pub sys_audio_dropped_malformed: u64,
    pub sys_audio_dropped_not_sharing: u64,
    pub sys_audio_dropped_no_subscribers: u64,
    pub sys_audio_bytes_relayed: u64,
}

/// Aggregated per-stream viewer health, produced once a second and sent
/// to the sharer as `ServerMessage::StreamHealth`.
pub struct StreamHealthReport {
    pub sharer: ClientId,
    /// Worst packet loss among connected viewers, 0.0–1.0.
    pub worst_loss: f32,
    /// Worst RTT among connected viewers, in milliseconds.
    pub worst_rtt_ms: f32,
    pub viewers: u16,
}

/// Minimum spacing between keyframe requests forwarded to one sharer.
/// The **only** authoritative keyframe rate limit in the system: viewer
/// requests, new-subscriber kicks, and backpressure pokes all funnel
/// through it, and an IDR produced for one passes serves every viewer.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(300);

/// Suspend delta forwarding to a viewer once their queued (unsent)
/// video exceeds this. 1 MB ≈ 2–3 s of a 3 Mbit/s stream or ~6 queued
/// keyframes — the stream is already stalled for that viewer; feeding
/// the queue further only adds latency to their eventual recovery.
const VIDEO_QUEUE_SUSPEND_BYTES: usize = 1_000_000;

/// Resume a suspended viewer only at a keyframe boundary once their
/// queue has drained below this.
const VIDEO_QUEUE_RESUME_BYTES: usize = 256 * 1024;

pub struct VideoRelay {
    /// Currently sharing clients: client_id → announced (width, height).
    sharers: HashMap<ClientId, (u16, u16)>,
    /// streamer_id → set of viewer client_ids.
    subscribers: HashMap<ClientId, HashSet<ClientId>>,
    /// Last time a keyframe request was forwarded to each sharer.
    keyframe_request_gate: HashMap<ClientId, Instant>,
    /// Viewers in backpressure: `(sharer, viewer)` pairs receiving no
    /// deltas until a keyframe boundary + drained queue.
    waiting_keyframe: HashSet<(ClientId, ClientId)>,
    /// Keyframe pokes to send to sharers, drained by the server each
    /// tick (subject to the gate): `(sharer, viewer)`.
    pending_pokes: Vec<(ClientId, ClientId)>,
    pub stats: VideoStats,
}

impl VideoRelay {
    pub fn new() -> Self {
        Self {
            sharers: HashMap::new(),
            subscribers: HashMap::new(),
            keyframe_request_gate: HashMap::new(),
            waiting_keyframe: HashSet::new(),
            pending_pokes: Vec::new(),
            stats: VideoStats::default(),
        }
    }

    // ── Subscription management (called from the control handler) ───

    /// A client started (or re-announced) sharing their screen.
    pub fn start_sharing(&mut self, client_id: ClientId, width: u16, height: u16) {
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
        self.waiting_keyframe.retain(|(sharer, _)| *sharer != client_id);
        self.pending_pokes.retain(|(sharer, _)| *sharer != client_id);
    }

    /// A viewer wants to watch a streamer. Caller enforces caps first.
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
        self.waiting_keyframe.remove(&(streamer, viewer));
        self.pending_pokes
            .retain(|(s, v)| !(*s == streamer && *v == viewer));
    }

    /// Clean up when a client disconnects.
    pub fn handle_disconnect(&mut self, client_id: ClientId) {
        if self.sharers.remove(&client_id).is_some() {
            self.subscribers.remove(&client_id);
            self.keyframe_request_gate.remove(&client_id);
        }
        for subs in self.subscribers.values_mut() {
            subs.remove(&client_id);
        }
        self.waiting_keyframe
            .retain(|(sharer, viewer)| *sharer != client_id && *viewer != client_id);
        self.pending_pokes
            .retain(|(sharer, viewer)| *sharer != client_id && *viewer != client_id);
    }

    pub fn is_sharing(&self, client_id: ClientId) -> bool {
        self.sharers.contains_key(&client_id)
    }

    pub fn is_subscribed(&self, viewer: ClientId, streamer: ClientId) -> bool {
        self.subscribers
            .get(&streamer)
            .is_some_and(|subs| subs.contains(&viewer))
    }

    /// Number of active screen shares.
    pub fn sharer_count(&self) -> usize {
        self.sharers.len()
    }

    /// Number of viewers subscribed to `streamer`.
    pub fn viewer_count(&self, streamer: ClientId) -> usize {
        self.subscribers.get(&streamer).map_or(0, HashSet::len)
    }

    /// Rate-gate keyframe requests toward `streamer`. Returns true (and
    /// arms the gate) if the request should be forwarded.
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

    /// Active sharers (for the join snapshot sent to new clients).
    pub fn sharers(&self) -> &HashMap<ClientId, (u16, u16)> {
        &self.sharers
    }

    /// Drain pending backpressure keyframe pokes: `(sharer, viewer)`.
    pub fn take_pokes(&mut self) -> Vec<(ClientId, ClientId)> {
        std::mem::take(&mut self.pending_pokes)
    }

    /// Per-stream viewer health for the periodic `StreamHealth` report.
    pub fn stream_health(&self, server: &RenetServer) -> Vec<StreamHealthReport> {
        let mut reports = Vec::new();
        for (sharer, subs) in &self.subscribers {
            if !self.sharers.contains_key(sharer) || subs.is_empty() {
                continue;
            }
            let mut worst_loss = 0f32;
            let mut worst_rtt_ms = 0f32;
            let mut viewers = 0u16;
            for viewer in subs {
                if let Ok(info) = server.network_info(*viewer) {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        worst_loss = worst_loss.max(info.packet_loss as f32);
                        worst_rtt_ms = worst_rtt_ms.max((info.rtt * 1000.0) as f32);
                    }
                    viewers = viewers.saturating_add(1);
                }
            }
            if viewers > 0 {
                reports.push(StreamHealthReport {
                    sharer: *sharer,
                    worst_loss,
                    worst_rtt_ms,
                    viewers,
                });
            }
        }
        reports
    }

    // ── Packet relay (called every tick) ────────────────────────────

    pub fn process(&mut self, server: &mut RenetServer) {
        let clients: Vec<ClientId> = server.clients_id();
        for client_id in &clients {
            while let Some(message) = server.receive_message(*client_id, VIDEO_CHANNEL) {
                self.relay_video_chunk(server, *client_id, &message);
            }
        }
    }

    /// Validate and fan out one video chunk with per-viewer backpressure.
    fn relay_video_chunk(&mut self, server: &mut RenetServer, sender: ClientId, message: &Bytes) {
        self.stats.chunks_received += 1;

        // Single source of validation (header, framing invariant, dims).
        let Some((hdr, _payload)) = parse_video_header(message) else {
            self.stats.chunks_dropped_malformed += 1;
            return;
        };

        if !self.sharers.contains_key(&sender) {
            self.stats.chunks_dropped_not_sharing += 1;
            return;
        }

        // Snapshot viewers so we can mutate backpressure state below.
        let viewers: Vec<ClientId> = match self.subscribers.get(&sender) {
            Some(subs) if !subs.is_empty() => subs.iter().copied().collect(),
            _ => {
                self.stats.chunks_dropped_no_subscribers += 1;
                return;
            }
        };

        let keyframe_start = hdr.is_keyframe() && hdr.chunk_idx == 0;
        let envelope = Bytes::from(wrap_sender(sender, message));
        let mut sent = 0u64;

        for viewer in viewers {
            if !server.is_connected(viewer) {
                continue;
            }

            let available = server.channel_available_memory(viewer, VIDEO_CHANNEL);
            let queued = VIDEO_BUFFER_BYTES.saturating_sub(available);
            let key = (sender, viewer);

            if self.waiting_keyframe.contains(&key) {
                if keyframe_start && queued < VIDEO_QUEUE_RESUME_BYTES {
                    // Clean resume: the viewer's decoder restarts from
                    // this keyframe with no torn frames in between.
                    self.waiting_keyframe.remove(&key);
                    debug!("Backpressure cleared for viewer {viewer} of {sender}");
                    server.send_message(viewer, VIDEO_CHANNEL, envelope.clone());
                    sent += 1;
                } else {
                    // Still suspended. If we just skipped the start of a
                    // keyframe (queue not drained yet), poke the sharer
                    // so the next one arrives promptly; the per-sharer
                    // gate absorbs duplicates.
                    if keyframe_start {
                        self.pending_pokes.push(key);
                    }
                    self.stats.chunks_suppressed_backpressure += 1;
                }
            } else if queued > VIDEO_QUEUE_SUSPEND_BYTES {
                self.waiting_keyframe.insert(key);
                self.stats.backpressure_marks += 1;
                self.stats.chunks_suppressed_backpressure += 1;
                self.pending_pokes.push(key);
                warn!(
                    "Viewer {viewer} can't keep up with {sender}'s stream \
                     ({queued} bytes queued) — suspending deltas until next keyframe"
                );
            } else {
                server.send_message(viewer, VIDEO_CHANNEL, envelope.clone());
                sent += 1;
            }
        }

        if sent > 0 {
            self.stats.chunks_forwarded += 1;
            self.stats.bytes_relayed += envelope.len() as u64 * sent;
            if keyframe_start {
                self.stats.keyframes_relayed += 1;
            }
        }
    }

    // ── System audio relay ──────────────────────────────────────────

    /// Forward system audio to the stream's subscribers. Deliberately
    /// not gated by voice-chat deafen state: deafening the room's
    /// voices shouldn't mute the movie.
    pub fn process_system_audio(&mut self, server: &mut RenetServer) {
        let clients: Vec<ClientId> = server.clients_id();
        for client_id in &clients {
            while let Some(message) = server.receive_message(*client_id, SYSTEM_AUDIO_CHANNEL) {
                self.relay_system_audio(server, *client_id, &message);
            }
        }
    }

    fn relay_system_audio(&mut self, server: &mut RenetServer, sender: ClientId, message: &Bytes) {
        self.stats.sys_audio_received += 1;

        if parse_system_audio(message).is_none() {
            self.stats.sys_audio_dropped_malformed += 1;
            return;
        }

        if !self.sharers.contains_key(&sender) {
            self.stats.sys_audio_dropped_not_sharing += 1;
            return;
        }

        let viewers: Vec<ClientId> = match self.subscribers.get(&sender) {
            Some(subs) if !subs.is_empty() => subs.iter().copied().collect(),
            _ => {
                self.stats.sys_audio_dropped_no_subscribers += 1;
                return;
            }
        };

        let envelope = Bytes::from(wrap_sender(sender, message));
        let mut sent = 0u64;
        for viewer in viewers {
            if server.is_connected(viewer) {
                server.send_message(viewer, SYSTEM_AUDIO_CHANNEL, envelope.clone());
                sent += 1;
            }
        }

        if sent > 0 {
            self.stats.sys_audio_forwarded += 1;
            self.stats.sys_audio_bytes_relayed += envelope.len() as u64 * sent;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn print_stats(&self) {
        if self.stats.chunks_received == 0
            && self.stats.sys_audio_received == 0
            && self.sharers.is_empty()
        {
            return;
        }

        info!(
            "Video — chunks RX: {}, FWD: {}, keyframes: {}, relayed: {:.2} MB",
            self.stats.chunks_received,
            self.stats.chunks_forwarded,
            self.stats.keyframes_relayed,
            self.stats.bytes_relayed as f64 / 1_048_576.0,
        );
        info!(
            "Video drops — no subs: {}, not sharing: {}, malformed: {}",
            self.stats.chunks_dropped_no_subscribers,
            self.stats.chunks_dropped_not_sharing,
            self.stats.chunks_dropped_malformed,
        );
        if self.stats.backpressure_marks > 0 {
            info!(
                "Video backpressure — viewers suspended: {}, chunks suppressed: {}",
                self.stats.backpressure_marks, self.stats.chunks_suppressed_backpressure,
            );
        }
        info!(
            "Streams active: {}, total subscribers: {}",
            self.sharers.len(),
            self.subscribers.values().map(HashSet::len).sum::<usize>(),
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
        }
    }
}
