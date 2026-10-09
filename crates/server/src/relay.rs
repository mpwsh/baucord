//! Media relay — voice and video fan-out.
//!
//! Both relays share the same trust model: media arriving from a client
//! carries **no sender identity**. The relay validates the packet with
//! the shared `protocol` parsers (the only parsers that exist — P2-1),
//! then prepends the sender's authenticated transport id via
//! [`protocol::wrap_sender`] before fan-out. A client cannot impersonate
//! another client's voice or video, full stop.
//!
//! Video rides *groups* (spec §6.2): the sharer opens one stream per
//! keyframe, and the relay mirrors each group onto every viewer's
//! connection as a group of its own, keyed by the sharer's id (the
//! *track*). A viewer that falls behind loses whole groups — the
//! transport resets the group when its bytes wait longer than the
//! deadline — and forwarding resumes at the next keyframe. Nothing
//! older than one GOP ever queues, and the relay needs no watermarks or
//! keyframe pokes of its own; the transport's counters say how often
//! each viewer fell behind, and that is what `StreamHealth` reports.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use log::{debug, info};
use protocol::{
    parse_system_audio, parse_video_header, parse_voice, wrap_sender, ClientId, Lane, ShareScope,
};
use transport::{Driver, LinkStats, VideoItem};

use crate::link_stats::VoiceSeqTracker;

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
    /// Per-recipient sends skipped because the recipient blocked the
    /// speaker (`protocol::safety`).
    pub sends_skipped_blocked: u64,
    /// Packets dropped because the sender is muted by the server
    /// (spec §4.2): a `voice` restriction, or no `SPEAK`.
    pub packets_dropped_muted: u64,
    /// Per-recipient sends skipped because the recipient is in a
    /// different voice channel than the speaker.
    pub sends_skipped_other_channel: u64,
    pub bytes_relayed: u64,
}

#[derive(Default)]
pub struct VoiceRelay {
    pub stats: VoiceStats,
    /// Per-sender sequence tracking for the link report — see
    /// [`crate::link_stats`].
    pub seq: VoiceSeqTracker,
}

/// What one relay tick needs to know about the room: who is admitted,
/// who hears nothing, who says nothing, and where everyone is. Built
/// by the server each tick from its own maps; borrowed, never kept.
#[derive(Clone, Copy)]
pub struct Room<'a> {
    pub clients: &'a [ClientId],
    pub deafened: &'a HashSet<ClientId>,
    pub muted: &'a HashSet<ClientId>,
    /// (sender, receiver) pairs whose voice is not relayed: the
    /// receiver blocked the sender (`protocol::safety`).
    pub blocked: &'a HashSet<(ClientId, ClientId)>,
    /// Voice in the AFK channel goes nowhere (spec §5).
    pub afk_channel: Option<u16>,
    /// Voice from the lobby goes nowhere (spec §12, 1.1.5).
    pub lobby: u16,
    pub channel_of: &'a HashMap<ClientId, u16>,
}

impl VoiceRelay {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stats: VoiceStats::default(),
            seq: VoiceSeqTracker::default(),
        }
    }

    /// Forget a client's sequence position once it is gone.
    pub fn handle_disconnect(&mut self, client_id: ClientId) {
        self.seq.remove(client_id);
    }

    /// Read voice packets from all clients and forward to the *speaker's
    /// voice channel*, skipping deafened recipients — they discard the
    /// audio anyway, so relaying it is pure wasted egress (a deafened
    /// client in a busy room was previously still downloading every
    /// speaker's stream).
    ///
    /// `channel_of` is the membership map owned by the server. Rooms
    /// are routing policy, not transport: everything still rides the
    /// one Voice lane; this filter is what makes a room a room. A
    /// client somehow missing from the map (should be impossible —
    /// admission runs before the relay each tick) hears and reaches
    /// nobody rather than everybody: fail closed.
    ///
    /// `room.clients` are the *admitted* peers; a connection still in
    /// its `Hello` is not among them and its voice, if any, is ignored.
    pub fn process(&mut self, driver: &mut Driver, now: Instant, room: &Room<'_>) {
        let Room {
            clients,
            deafened,
            muted,
            blocked,
            afk_channel,
            lobby,
            channel_of,
        } = *room;
        for sender in clients {
            let sender_channel = channel_of.get(sender).copied();
            // A muted sender is told and stops; this is the backstop.
            // One set lookup per sender, before its packets. Voice in
            // the AFK channel goes nowhere either (spec §5), nor out
            // of the lobby (spec §12, 1.1.5).
            let sender_muted = muted.contains(sender)
                || (afk_channel.is_some() && sender_channel == afk_channel)
                || sender_channel == Some(lobby);
            while let Some(message) = driver.recv(*sender, Lane::Voice) {
                self.stats.packets_received += 1;
                if sender_muted {
                    self.stats.packets_dropped_muted += 1;
                    continue;
                }

                // Single source of validation: the shared parser.
                let Some((hdr, _)) = parse_voice(&message) else {
                    self.stats.packets_dropped_malformed += 1;
                    continue;
                };
                self.seq.note(*sender, hdr.seq);

                // Stamp the authenticated sender once; fan out.
                let envelope = wrap_sender(*sender, &message);
                let mut sent = 0u64;
                for recipient in clients {
                    if recipient == sender {
                        continue;
                    }
                    if sender_channel.is_none()
                        || channel_of.get(recipient).copied() != sender_channel
                    {
                        self.stats.sends_skipped_other_channel += 1;
                        continue;
                    }
                    if deafened.contains(recipient) {
                        self.stats.sends_skipped_deafened += 1;
                        continue;
                    }
                    if !blocked.is_empty() && blocked.contains(&(*sender, *recipient)) {
                        self.stats.sends_skipped_blocked += 1;
                        continue;
                    }
                    if driver.send(now, *recipient, Lane::Voice, &envelope).is_ok() {
                        sent += 1;
                    }
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
        if self.stats.packets_dropped_malformed > 0
            || self.stats.packets_dropped_muted > 0
            || self.stats.sends_skipped_deafened > 0
            || self.stats.sends_skipped_other_channel > 0
            || self.stats.sends_skipped_blocked > 0
        {
            info!(
                "Voice — malformed drops: {}, muted drops: {}, deafened skips: {}, \
                 other-channel skips: {}, blocked skips: {}",
                self.stats.packets_dropped_malformed,
                self.stats.packets_dropped_muted,
                self.stats.sends_skipped_deafened,
                self.stats.sends_skipped_other_channel,
                self.stats.sends_skipped_blocked,
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
    pub chunks_dropped_malformed: u64,
    pub chunks_dropped_not_sharing: u64,
    pub chunks_dropped_no_subscribers: u64,
    /// Groups (keyframe-led streams) received from sharers.
    pub groups_received: u64,
    /// Groups the sharer abandoned mid-way (its own uplink fell behind).
    pub groups_reset_by_sharer: u64,
    /// Groups opened toward viewers (one per viewer per sharer group).
    pub groups_relayed: u64,
    pub keyframe_requests_forwarded: u64,
    pub keyframe_requests_suppressed: u64,
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
    /// Worst "behind" ratio among connected viewers, 0.0–1.0: video
    /// groups the relay had to abandon for that viewer over groups it
    /// started, since the last report. Measured on the video itself,
    /// never on transport ack accounting (spec §6.2).
    pub worst_loss: f32,
    /// Worst path RTT among connected viewers, in milliseconds.
    pub worst_rtt_ms: f32,
    pub viewers: u16,
}

/// Minimum spacing between keyframe requests forwarded to one sharer.
/// The **only** authoritative keyframe rate limit in the system: viewer
/// requests, new-subscriber kicks, and backpressure pokes all funnel
/// through it, and an IDR produced for one passes serves every viewer.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(300);

/// Everything the relay knows about one active share.
#[derive(Debug, Clone, Copy)]
pub struct ShareInfo {
    pub width: u16,
    pub height: u16,
    /// Who may see this stream — enforced by the server at subscribe
    /// time and re-validated on channel moves.
    pub scope: ShareScope,
}

pub struct VideoRelay {
    /// Currently sharing clients and their announced share parameters.
    sharers: HashMap<ClientId, ShareInfo>,
    /// `streamer_id` → set of viewer `client_id`s.
    subscribers: HashMap<ClientId, HashSet<ClientId>>,
    /// Last time a keyframe request was forwarded to each sharer.
    keyframe_request_gate: HashMap<ClientId, Instant>,
    /// Sharers whose current group is open on their viewers' tracks.
    group_open: HashSet<ClientId>,
    /// Per-viewer transport counters at the last health report, so the
    /// report covers one interval.
    health_baseline: HashMap<ClientId, LinkStats>,
    pub stats: VideoStats,
}

impl Default for VideoRelay {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoRelay {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sharers: HashMap::new(),
            subscribers: HashMap::new(),
            keyframe_request_gate: HashMap::new(),
            group_open: HashSet::new(),
            health_baseline: HashMap::new(),
            stats: VideoStats::default(),
        }
    }

    // ── Subscription management (called from the control handler) ───

    /// A client started (or re-announced) sharing their screen.
    pub fn start_sharing(&mut self, client_id: ClientId, info: ShareInfo) {
        info!(
            "Screen share started: {client_id:#x} ({}x{}, {:?} scope)",
            info.width, info.height, info.scope
        );
        self.sharers.insert(client_id, info);
        self.subscribers.entry(client_id).or_default();
    }

    /// Share parameters for `client_id`, if they are sharing.
    #[must_use]
    pub fn share_info(&self, client_id: ClientId) -> Option<ShareInfo> {
        self.sharers.get(&client_id).copied()
    }

    /// Snapshot of the viewers subscribed to `streamer`.
    #[must_use]
    pub fn subscribers_of(&self, streamer: ClientId) -> Vec<ClientId> {
        self.subscribers
            .get(&streamer)
            .map(|subs| subs.iter().copied().collect())
            .unwrap_or_default()
    }

    /// A client stopped sharing their screen. Any group still open on
    /// its viewers is finished.
    pub fn stop_sharing(&mut self, driver: &mut Driver, now: Instant, client_id: ClientId) {
        info!("Screen share stopped: {client_id:#x}");
        self.sharers.remove(&client_id);
        if let Some(viewers) = self.subscribers.remove(&client_id) {
            for viewer in viewers {
                driver.video_finish_group(now, viewer, client_id);
            }
        }
        self.group_open.remove(&client_id);
        self.keyframe_request_gate.remove(&client_id);
    }

    /// A viewer wants to watch a streamer. Caller enforces caps first.
    pub fn subscribe(&mut self, viewer: ClientId, streamer: ClientId) {
        if self.sharers.contains_key(&streamer) {
            self.subscribers.entry(streamer).or_default().insert(viewer);
            info!("{viewer:#x} subscribed to {streamer:#x}'s screen");
        } else {
            debug!("{viewer:#x} tried to subscribe to {streamer:#x} who is not sharing");
        }
    }

    /// A viewer stops watching a streamer; their open group on that
    /// track is abandoned.
    pub fn unsubscribe(
        &mut self,
        driver: &mut Driver,
        now: Instant,
        viewer: ClientId,
        streamer: ClientId,
    ) {
        if let Some(subs) = self.subscribers.get_mut(&streamer)
            && subs.remove(&viewer)
        {
            driver.video_reset_group(now, viewer, streamer);
        }
    }

    /// Clean up when a client disconnects: as a sharer, its viewers'
    /// groups are abandoned; as a viewer, it is dropped from every
    /// subscriber set.
    pub fn handle_disconnect(&mut self, driver: &mut Driver, now: Instant, client_id: ClientId) {
        if self.sharers.remove(&client_id).is_some() {
            if let Some(viewers) = self.subscribers.remove(&client_id) {
                for viewer in viewers {
                    driver.video_reset_group(now, viewer, client_id);
                }
            }
            self.group_open.remove(&client_id);
            self.keyframe_request_gate.remove(&client_id);
        }
        for subs in self.subscribers.values_mut() {
            subs.remove(&client_id);
        }
        self.health_baseline.remove(&client_id);
    }

    #[must_use]
    pub fn is_sharing(&self, client_id: ClientId) -> bool {
        self.sharers.contains_key(&client_id)
    }

    #[must_use]
    pub fn is_subscribed(&self, viewer: ClientId, streamer: ClientId) -> bool {
        self.subscribers
            .get(&streamer)
            .is_some_and(|subs| subs.contains(&viewer))
    }

    /// Number of active screen shares.
    #[must_use]
    pub fn sharer_count(&self) -> usize {
        self.sharers.len()
    }

    /// Number of viewers subscribed to `streamer`.
    #[must_use]
    pub fn viewer_count(&self, streamer: ClientId) -> usize {
        self.subscribers.get(&streamer).map_or(0, HashSet::len)
    }

    /// Rate-gate keyframe requests toward `streamer`. Returns true (and
    /// arms the gate) if the request should be forwarded.
    pub fn allow_keyframe_request(&mut self, now: Instant, streamer: ClientId) -> bool {
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
    #[must_use]
    pub fn sharers(&self) -> &HashMap<ClientId, ShareInfo> {
        &self.sharers
    }

    /// Per-stream viewer health for the periodic `StreamHealth` report,
    /// over the interval since the previous call.
    pub fn stream_health(&mut self, driver: &Driver) -> Vec<StreamHealthReport> {
        let mut reports = Vec::new();
        for (sharer, subs) in &self.subscribers {
            if !self.sharers.contains_key(sharer) || subs.is_empty() {
                continue;
            }
            let mut worst_loss = 0f32;
            let mut worst_rtt_ms = 0f32;
            let mut viewers = 0u16;
            for viewer in subs {
                let Some(stats) = driver.stats(*viewer) else {
                    continue;
                };
                let base = self
                    .health_baseline
                    .get(viewer)
                    .copied()
                    .unwrap_or_default();
                let started = stats.groups_started.saturating_sub(base.groups_started);
                let reset = stats.groups_reset.saturating_sub(base.groups_reset);
                #[allow(clippy::cast_precision_loss)]
                let behind = if started == 0 {
                    0.0
                } else {
                    (reset as f32 / started as f32).min(1.0)
                };
                worst_loss = worst_loss.max(behind);
                #[allow(clippy::cast_possible_truncation)]
                {
                    worst_rtt_ms = worst_rtt_ms.max(stats.rtt.as_secs_f32() * 1000.0);
                }
                viewers = viewers.saturating_add(1);
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
        // Next interval starts now, for every viewer of anyone.
        for subs in self.subscribers.values() {
            for viewer in subs {
                if let Some(stats) = driver.stats(*viewer) {
                    self.health_baseline.insert(*viewer, stats);
                }
            }
        }
        reports
    }

    // ── Group relay (called every tick) ─────────────────────────────

    /// Read every sharer's incoming groups and mirror them onto their
    /// viewers' tracks.
    pub fn process(&mut self, driver: &mut Driver, now: Instant, clients: &[ClientId]) {
        for sender in clients {
            while let Some((_, item)) = driver.recv_video(*sender) {
                self.relay_video_item(driver, now, *sender, item);
            }
        }
    }

    fn relay_video_item(
        &mut self,
        driver: &mut Driver,
        now: Instant,
        sender: ClientId,
        item: VideoItem,
    ) {
        if !self.sharers.contains_key(&sender) {
            if matches!(item, VideoItem::Chunk(_)) {
                self.stats.chunks_dropped_not_sharing += 1;
            }
            return;
        }
        let viewers: Vec<ClientId> = self
            .subscribers
            .get(&sender)
            .map(|subs| subs.iter().copied().collect())
            .unwrap_or_default();

        match item {
            VideoItem::GroupStart => {
                self.stats.groups_received += 1;
                self.group_open.insert(sender);
                for viewer in &viewers {
                    driver.video_new_group(now, *viewer, sender);
                    self.stats.groups_relayed += 1;
                }
            }
            VideoItem::Chunk(message) => {
                self.stats.chunks_received += 1;
                // Single source of validation (header, framing
                // invariant, dims).
                if parse_video_header(&message).is_none() {
                    self.stats.chunks_dropped_malformed += 1;
                    return;
                }
                if viewers.is_empty() {
                    self.stats.chunks_dropped_no_subscribers += 1;
                    return;
                }
                let envelope = wrap_sender(sender, &message);
                let mut sent = 0u64;
                for viewer in &viewers {
                    // A viewer whose group was reset simply misses the
                    // rest of it; the next GroupStart reopens.
                    if driver.video_group_open(*viewer, sender)
                        && driver.send_video(now, *viewer, sender, &envelope).is_ok()
                    {
                        sent += 1;
                    }
                }
                if sent > 0 {
                    self.stats.chunks_forwarded += 1;
                    self.stats.bytes_relayed += envelope.len() as u64 * sent;
                }
            }
            VideoItem::GroupEnd => {
                self.group_open.remove(&sender);
                for viewer in &viewers {
                    driver.video_finish_group(now, *viewer, sender);
                }
            }
            VideoItem::GroupReset => {
                self.stats.groups_reset_by_sharer += 1;
                self.group_open.remove(&sender);
                for viewer in &viewers {
                    driver.video_reset_group(now, *viewer, sender);
                }
            }
        }
    }

    // ── System audio relay ──────────────────────────────────────────

    /// Forward system audio to the stream's subscribers. Deliberately
    /// not gated by voice-chat deafen state: deafening the room's
    /// voices shouldn't mute the movie.
    pub fn process_system_audio(
        &mut self,
        driver: &mut Driver,
        now: Instant,
        clients: &[ClientId],
    ) {
        for client_id in clients {
            while let Some(message) = driver.recv(*client_id, Lane::SystemAudio) {
                self.relay_system_audio(driver, now, *client_id, &message);
            }
        }
    }

    fn relay_system_audio(
        &mut self,
        driver: &mut Driver,
        now: Instant,
        sender: ClientId,
        message: &[u8],
    ) {
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

        let envelope = wrap_sender(sender, message);
        let mut sent = 0u64;
        for viewer in viewers {
            if driver
                .send(now, viewer, Lane::SystemAudio, &envelope)
                .is_ok()
            {
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
            "Video — chunks RX: {}, FWD: {}, groups RX: {} (sharer resets {}), groups relayed: {}, relayed: {:.2} MB",
            self.stats.chunks_received,
            self.stats.chunks_forwarded,
            self.stats.groups_received,
            self.stats.groups_reset_by_sharer,
            self.stats.groups_relayed,
            self.stats.bytes_relayed as f64 / 1_048_576.0,
        );
        info!(
            "Video drops — no subs: {}, not sharing: {}, malformed: {}",
            self.stats.chunks_dropped_no_subscribers,
            self.stats.chunks_dropped_not_sharing,
            self.stats.chunks_dropped_malformed,
        );
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
