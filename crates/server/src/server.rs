//! Chat server — connection management, message routing, media relay.
//!
//! Wire types, channel layout, and serialization all come from the
//! shared `protocol` crate; this file owns policy: who may connect
//! (auth), who may share and watch (limits), and when to tell sharers
//! about their viewers' network health.

use log::{debug, info, warn};
use renet::{ClientId, RenetServer, ServerEvent};
use renet_netcode::{
    NETCODE_USER_DATA_BYTES, NetcodeServerTransport, ServerAuthentication, ServerConfig,
};
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

use protocol::{ChatMessage, ClientMessage, DenyReason, ServerMessage};

use crate::auth;
use crate::config::Config;
use crate::relay::{VideoRelay, VoiceRelay};

// The protocol crate defines the user-data layout against its own
// constant so it doesn't depend on the transport crate; make sure the
// two can never drift apart.
const _: () = assert!(protocol::USER_DATA_BYTES == NETCODE_USER_DATA_BYTES);

/// Announced share dimensions must fall in this range (per-chunk
/// dimensions are additionally validated by the shared header parser).
const MIN_SHARE_DIMENSION: u16 = 16;
const MAX_SHARE_DIMENSION: u16 = 4096;

/// How often to sample viewer network health and report to sharers.
const STREAM_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// How often to check reliable-channel pressure. (The check itself is
/// cheap; the log volume of doing it every 1 ms tick was not — a client
/// sitting at 76% used to generate a thousand warnings per second.)
const PRESSURE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// A tick is considered "media-active" for sleep scheduling if any
/// voice/video/system-audio packet arrived within this window.
const MEDIA_ACTIVE_WINDOW: Duration = Duration::from_millis(500);

// ── Stats ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct ServerStats {
    messages_count: u64,
    bytes_sent: u64,
    bytes_received: u64,
}

// ── ChatServer ──────────────────────────────────────────────────────────

pub struct ChatServer {
    config: Config,
    server: RenetServer,
    transport: NetcodeServerTransport,
    usernames: HashMap<ClientId, String>,
    /// Latest self-reported audio state: client → (muted, deafened).
    audio_states: HashMap<ClientId, (bool, bool)>,
    /// Clients currently deafened (voice relay skips them).
    deafened: HashSet<ClientId>,
    last_updated: Instant,
    last_stats: Instant,
    last_pressure_check: Instant,
    last_health: Instant,
    /// Last instant any media packet arrived (drives adaptive sleep).
    last_media: Instant,
    stats: ServerStats,
    voice: VoiceRelay,
    video: VideoRelay,
}

impl ChatServer {
    pub fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        let addr = config.socket_addr()?;
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;

        // Increase OS-level UDP buffers: pacing smooths most bursts, but
        // the OS buffer is the last line of defense before send_to fails
        // and keepalives get dropped.
        let sock_ref = socket2::SockRef::from(&socket);
        sock_ref
            .set_send_buffer_size(protocol::UDP_SOCKET_BUFFER_BYTES)
            .ok();
        sock_ref
            .set_recv_buffer_size(protocol::UDP_SOCKET_BUFFER_BYTES)
            .ok();
        info!(
            "UDP buffers — send: {} KB, recv: {} KB",
            sock_ref.send_buffer_size().unwrap_or(0) / 1024,
            sock_ref.recv_buffer_size().unwrap_or(0) / 1024,
        );
        let actual_send = sock_ref.send_buffer_size().unwrap_or(0);
        if actual_send < protocol::UDP_SOCKET_BUFFER_BYTES {
            warn!(
                "UDP send buffer is {} KB, wanted {} KB — apply sysctl tuning on the host",
                actual_send / 1024,
                protocol::UDP_SOCKET_BUFFER_BYTES / 1024,
            );
        }

        let local_addr = socket.local_addr()?;
        info!("{} starting on {local_addr}", config.server.name);

        // ── Authentication ──────────────────────────────────────────
        let (authentication, public_addr) = if config.authentication.secure {
            let key = auth::load_or_create_private_key(&config.authentication.private_key_file)?;
            let public_addr = resolve_public_addr(&config, local_addr)?;
            auth::spawn_issuer(
                &config.authentication.issuer.bind,
                config.authentication.issuer.password.clone(),
                key,
                public_addr,
            )?;
            info!(
                "Secure mode: clients must present a connect token (public address {public_addr})"
            );
            (
                ServerAuthentication::Secure { private_key: key },
                public_addr,
            )
        } else {
            warn!("╔══════════════════════════════════════════════════════════════╗");
            warn!("║ AUTHENTICATION DISABLED — running in open (unsecure) mode.   ║");
            warn!("║ Anyone who can reach this UDP port can join with any name.   ║");
            warn!("║ Set `authentication.secure: true` for real deployments.      ║");
            warn!("╚══════════════════════════════════════════════════════════════╝");
            (ServerAuthentication::Unsecure, local_addr)
        };

        let server_config = ServerConfig {
            current_time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?,
            max_clients: config.network.max_clients,
            protocol_id: protocol::PROTOCOL_ID,
            public_addresses: vec![public_addr],
            authentication,
        };

        let transport = NetcodeServerTransport::new(server_config, socket)?;
        let mut bytes_per_tick = config.network.bytes_per_tick;
        if bytes_per_tick < 1_048_576 {
            warn!(
                "network.bytes_per_tick = {bytes_per_tick} is unsafe: renet DROPS unreliable \
                 messages over the per-tick budget, beheading video keyframes. Clamping to {}.",
                protocol::SERVER_BYTES_PER_TICK
            );
            bytes_per_tick = protocol::SERVER_BYTES_PER_TICK;
        }
        let server = RenetServer::new(protocol::connection_config(bytes_per_tick));

        let now = Instant::now();
        Ok(Self {
            config,
            server,
            transport,
            usernames: HashMap::new(),
            audio_states: HashMap::new(),
            deafened: HashSet::new(),
            last_updated: now,
            last_stats: now,
            last_pressure_check: now,
            last_health: now,
            last_media: now,
            stats: ServerStats::default(),
            voice: VoiceRelay::new(),
            video: VideoRelay::new(),
        })
    }

    /// Drive one tick of the server: receive, process, send.
    pub fn update(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let now = Instant::now();
        let duration = now - self.last_updated;
        self.last_updated = now;

        self.server.update(duration);
        self.transport.update(duration, &mut self.server)?;

        self.process_events();
        self.process_control_messages();

        let media_before = self.media_packet_count();
        self.voice.process(&mut self.server, &self.deafened);
        self.video.process(&mut self.server);
        self.video.process_system_audio(&mut self.server);
        if self.media_packet_count() != media_before {
            self.last_media = now;
        }

        self.forward_backpressure_pokes();

        if now.duration_since(self.last_health) >= STREAM_HEALTH_INTERVAL {
            self.send_stream_health();
            self.last_health = now;
        }

        if now.duration_since(self.last_pressure_check) >= PRESSURE_CHECK_INTERVAL {
            self.monitor_channel_pressure();
            self.last_pressure_check = now;
        }

        self.transport.send_packets(&mut self.server);

        if now.duration_since(self.last_stats)
            >= Duration::from_secs(self.config.logging.stats_interval_secs)
        {
            self.print_stats();
            self.last_stats = now;
        }

        Ok(())
    }

    /// Sleep the main loop should take before the next tick.
    ///
    /// - Empty server: 20 ms (nothing to relay to anyone).
    /// - Media in the last 500 ms: 1 ms (sub-millisecond relay matters).
    /// - Clients connected but quiet: 5 ms — adds at most 5 ms to the
    ///   *first* packet of a talk burst, then the loop runs hot again;
    ///   inaudible next to the jitter buffer, and it cuts idle CPU ~5×.
    pub fn suggested_sleep(&self) -> Duration {
        if self.server.connected_clients() == 0 {
            Duration::from_millis(20)
        } else if self.last_media.elapsed() < MEDIA_ACTIVE_WINDOW {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(5)
        }
    }

    fn media_packet_count(&self) -> u64 {
        self.voice.stats.packets_received
            + self.video.stats.chunks_received
            + self.video.stats.sys_audio_received
    }

    // ── Message send helpers ────────────────────────────────────────

    fn send_msg(&mut self, client_id: ClientId, msg: &ServerMessage) {
        if let Some(encoded) = protocol::encode_server_message(msg) {
            self.stats.bytes_sent += encoded.len() as u64;
            self.server.send_message(client_id, msg.channel(), encoded);
        }
    }

    fn broadcast_msg(&mut self, msg: &ServerMessage) {
        if let Some(encoded) = protocol::encode_server_message(msg) {
            self.stats.bytes_sent += encoded.len() as u64 * self.server.connected_clients() as u64;
            self.server.broadcast_message(msg.channel(), encoded);
        }
    }

    fn broadcast_msg_except(&mut self, except: ClientId, msg: &ServerMessage) {
        if let Some(encoded) = protocol::encode_server_message(msg) {
            self.stats.bytes_sent +=
                encoded.len() as u64 * self.server.connected_clients().saturating_sub(1) as u64;
            self.server
                .broadcast_message_except(except, msg.channel(), encoded);
        }
    }

    // ── Connection events ───────────────────────────────────────────

    fn process_events(&mut self) {
        while let Some(event) = self.server.get_event() {
            match event {
                ServerEvent::ClientConnected { client_id } => {
                    self.handle_client_connected(client_id);
                }
                ServerEvent::ClientDisconnected { client_id, reason } => {
                    self.handle_client_disconnected(client_id, reason);
                }
            }
        }
    }

    fn handle_client_connected(&mut self, client_id: ClientId) {
        // In secure mode the username comes from inside the connect
        // token (issuer-controlled); in open mode from client user data.
        // Sanitize either way.
        let mut username = self
            .transport
            .user_data(client_id)
            .map(|data| protocol::sanitize_username(&protocol::decode_username(&data)))
            .unwrap_or_default();
        if username.is_empty() {
            username = format!("User{client_id}");
        }

        info!("{username} ({client_id}) connected");
        self.usernames.insert(client_id, username.clone());

        // Full state snapshot for the new client: user list, active
        // shares, and current audio states — so a client joining an
        // in-progress session renders it correctly instead of assuming
        // everyone is unmuted and nobody is streaming.
        let init = ServerMessage::Init {
            usernames: self.usernames.clone(),
            client_id,
            server_name: self.config.server.name.clone(),
            greeting: self.config.server.greeting_message.clone(),
        };
        self.send_msg(client_id, &init);

        let sharers: Vec<(ClientId, (u16, u16))> = self
            .video
            .sharers()
            .iter()
            .map(|(id, dims)| (*id, *dims))
            .collect();
        for (sharer_id, (width, height)) in sharers {
            self.send_msg(
                client_id,
                &ServerMessage::ScreenShareStarted {
                    client_id: sharer_id,
                    width,
                    height,
                },
            );
        }

        let states: Vec<(ClientId, (bool, bool))> =
            self.audio_states.iter().map(|(id, s)| (*id, *s)).collect();
        for (peer_id, (muted, deafened)) in states {
            self.send_msg(
                client_id,
                &ServerMessage::AudioState {
                    client_id: peer_id,
                    muted,
                    deafened,
                },
            );
        }

        self.broadcast_msg_except(
            client_id,
            &ServerMessage::UserJoined {
                client_id,
                username,
            },
        );
    }

    fn handle_client_disconnected(&mut self, client_id: ClientId, reason: renet::DisconnectReason) {
        let username = self
            .usernames
            .remove(&client_id)
            .unwrap_or_else(|| format!("User{client_id}"));
        self.audio_states.remove(&client_id);
        self.deafened.remove(&client_id);

        warn!(
            "CLIENT DISCONNECTED: {username} ({client_id}) reason={reason:?} display=\"{reason}\""
        );
        match reason {
            renet::DisconnectReason::SendChannelError { channel_id, error } => {
                let ch_name = channel_name(channel_id);
                warn!("  → Send channel error on {ch_name} (id={channel_id}): {error:?}");
                warn!("  → This usually means the channel's max_memory_usage_bytes was exceeded.");
            }
            renet::DisconnectReason::ReceiveChannelError { channel_id, error } => {
                let ch_name = channel_name(channel_id);
                warn!("  → Receive channel error on {ch_name} (id={channel_id}): {error:?}");
            }
            renet::DisconnectReason::Transport => {
                warn!("  → Transport-level disconnect (netcode timeout or socket error)");
            }
            renet::DisconnectReason::DisconnectedByClient => {
                info!("  → Clean disconnect initiated by the client");
            }
            renet::DisconnectReason::DisconnectedByServer => {
                info!("  → Server kicked this client");
            }
            _ => {
                warn!("  → Reason details: {reason:?}");
            }
        }

        let was_sharing = self.video.is_sharing(client_id);
        self.video.handle_disconnect(client_id);
        if was_sharing {
            self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
        }

        self.broadcast_msg(&ServerMessage::UserLeft { client_id });
    }

    // ── Control-plane messages ──────────────────────────────────────

    fn process_control_messages(&mut self) {
        for client_id in self.server.clients_id() {
            for channel in [protocol::CONTROL_CHANNEL, protocol::SIGNAL_CHANNEL] {
                while let Some(message) = self.server.receive_message(client_id, channel) {
                    self.stats.bytes_received += message.len() as u64;
                    match protocol::decode_client_message(&message) {
                        Some(msg) => self.handle_client_message(client_id, msg),
                        None => debug!(
                            "Undecodable control message ({} bytes) from {client_id}",
                            message.len()
                        ),
                    }
                }
            }
        }
    }

    fn handle_client_message(&mut self, client_id: ClientId, message: ClientMessage) {
        match message {
            ClientMessage::Chat(text) => {
                let text = protocol::user_data::truncate_on_char_boundary(
                    &text,
                    self.config.limits.max_chat_bytes,
                );
                let username = self.get_username(client_id);
                info!("[{username}]: {text}");
                self.stats.messages_count += 1;
                self.broadcast_msg(&ServerMessage::Chat(ChatMessage { client_id, text }));
            }
            ClientMessage::Speaking(is_speaking) => {
                // Broadcast to everyone including the sender: the UI
                // treats the echo as authoritative for its own tile.
                self.broadcast_msg(&ServerMessage::Speaking {
                    client_id,
                    is_speaking,
                });
            }
            ClientMessage::AudioState { muted, deafened } => {
                self.audio_states.insert(client_id, (muted, deafened));
                if deafened {
                    self.deafened.insert(client_id);
                } else {
                    self.deafened.remove(&client_id);
                }
                self.broadcast_msg_except(
                    client_id,
                    &ServerMessage::AudioState {
                        client_id,
                        muted,
                        deafened,
                    },
                );
            }

            // ── Screen share signaling ──────────────────────────────
            ClientMessage::ScreenShareStart { width, height } => {
                self.handle_share_start(client_id, width, height);
            }
            ClientMessage::ScreenShareStop => {
                let username = self.get_username(client_id);
                info!("{username} stopped screen share");
                self.video.stop_sharing(client_id);
                self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
            }
            ClientMessage::SubscribeScreen { target_id } => {
                self.handle_subscribe(client_id, target_id);
            }
            ClientMessage::UnsubscribeScreen { target_id } => {
                self.video.unsubscribe(client_id, target_id);
            }
            ClientMessage::RequestKeyframe { target_id } => {
                // Only honored from subscribed viewers, so a misbehaving
                // client can't force arbitrary encoders to spam IDRs.
                // The per-sharer gate is the single authoritative rate
                // limit for all keyframe request paths.
                if self.video.is_subscribed(client_id, target_id)
                    && self.video.allow_keyframe_request(target_id)
                {
                    self.send_keyframe_request(target_id, client_id);
                }
            }
        }
    }

    fn handle_share_start(&mut self, client_id: ClientId, width: u16, height: u16) {
        let dims_ok = (MIN_SHARE_DIMENSION..=MAX_SHARE_DIMENSION).contains(&width)
            && (MIN_SHARE_DIMENSION..=MAX_SHARE_DIMENSION).contains(&height);
        if !dims_ok {
            warn!("{client_id} announced share with invalid dimensions {width}x{height}");
            self.send_msg(
                client_id,
                &ServerMessage::ShareDenied {
                    reason: DenyReason::InvalidDimensions,
                },
            );
            return;
        }

        // Admission control: cap concurrent shares. Re-announcing an
        // existing share (e.g. quality change) is always allowed.
        if !self.video.is_sharing(client_id)
            && self.video.sharer_count() >= self.config.limits.max_sharers
        {
            info!(
                "Share denied for {client_id}: {} of {} share slots in use",
                self.video.sharer_count(),
                self.config.limits.max_sharers
            );
            self.send_msg(
                client_id,
                &ServerMessage::ShareDenied {
                    reason: DenyReason::ShareLimitReached,
                },
            );
            return;
        }

        let username = self.get_username(client_id);
        info!("{username} started screen share ({width}x{height})");
        self.video.start_sharing(client_id, width, height);
        self.broadcast_msg(&ServerMessage::ScreenShareStarted {
            client_id,
            width,
            height,
        });
    }

    fn handle_subscribe(&mut self, client_id: ClientId, target_id: ClientId) {
        if !self.video.is_sharing(target_id) {
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::NotSharing,
                },
            );
            return;
        }

        // Admission control: cap viewers per stream. Worst-case egress
        // is max_sharers × max_viewers_per_stream × bitrate — the knob
        // that actually protects the uplink.
        if !self.video.is_subscribed(client_id, target_id)
            && self.video.viewer_count(target_id) >= self.config.limits.max_viewers_per_stream
        {
            info!(
                "Subscribe denied for {client_id} → {target_id}: viewer limit {} reached",
                self.config.limits.max_viewers_per_stream
            );
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::ViewerLimitReached,
                },
            );
            return;
        }

        self.video.subscribe(client_id, target_id);

        // Kick the sharer for an IDR so the new viewer gets a first
        // picture in one RTT instead of waiting out the keyframe
        // interval. Same gate as explicit requests.
        if self.video.allow_keyframe_request(target_id) {
            self.send_keyframe_request(target_id, client_id);
        }
    }

    /// Forward backpressure pokes from the video relay: viewers whose
    /// delta stream was suspended need a keyframe to resume cleanly.
    fn forward_backpressure_pokes(&mut self) {
        let pokes = self.video.take_pokes();
        for (sharer, viewer) in pokes {
            if self.video.is_sharing(sharer) && self.video.allow_keyframe_request(sharer) {
                self.send_keyframe_request(sharer, viewer);
            }
        }
    }

    /// Sample viewer network health per stream and report to sharers.
    fn send_stream_health(&mut self) {
        let reports = self.video.stream_health(&self.server);
        for report in reports {
            self.send_msg(
                report.sharer,
                &ServerMessage::StreamHealth {
                    worst_loss: report.worst_loss,
                    worst_rtt_ms: report.worst_rtt_ms,
                    viewers: report.viewers,
                },
            );
        }
    }

    /// Forward a keyframe request to `sharer` on behalf of `viewer`.
    fn send_keyframe_request(&mut self, sharer: ClientId, viewer: ClientId) {
        debug!("Forwarding keyframe request: viewer {viewer} → sharer {sharer}");
        self.send_msg(
            sharer,
            &ServerMessage::KeyframeRequested { viewer_id: viewer },
        );
    }

    // ── Utilities ───────────────────────────────────────────────────

    fn get_username(&self, client_id: ClientId) -> String {
        self.usernames
            .get(&client_id)
            .cloned()
            .unwrap_or_else(|| format!("User{client_id}"))
    }

    /// Warn when any client's control channel is filling up. A reliable
    /// channel hitting its ceiling disconnects the client, so pressure
    /// here deserves attention. Rate-limited to once a second.
    #[allow(clippy::cast_precision_loss)]
    fn monitor_channel_pressure(&self) {
        let max_bytes = protocol::channels::CONTROL_BUFFER_BYTES;
        let threshold = max_bytes / 4; // warn at 75% used

        for client_id in self.server.clients_id() {
            let available = self
                .server
                .channel_available_memory(client_id, protocol::CONTROL_CHANNEL);
            if available < threshold {
                let username = self.get_username(client_id);
                let used_pct = (1.0 - available as f64 / max_bytes as f64) * 100.0;
                warn!(
                    "CHANNEL PRESSURE: {username} ({client_id}) control channel at {used_pct:.0}% \
                     ({available} bytes remaining of {max_bytes})",
                );
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn print_stats(&self) {
        info!("=== {} Statistics ===", self.config.server.name);
        info!("Connected clients: {}", self.server.connected_clients());
        info!("Text messages: {}", self.stats.messages_count);

        self.voice.print_stats();
        self.video.print_stats();

        info!(
            "Control data sent: {:.2} MB, received: {:.2} MB",
            self.stats.bytes_sent as f64 / 1_048_576.0,
            self.stats.bytes_received as f64 / 1_048_576.0
        );

        // Per-client link health from renet's connection stats.
        for client_id in self.server.clients_id() {
            if let Ok(net) = self.server.network_info(client_id) {
                let username = self.get_username(client_id);
                info!(
                    "  {username} ({client_id}): rtt {:.0} ms, loss {:.1}%, tx {:.0} kbit/s, rx {:.0} kbit/s",
                    net.rtt * 1000.0,
                    net.packet_loss * 100.0,
                    net.bytes_sent_per_second * 8.0 / 1000.0,
                    net.bytes_received_per_second * 8.0 / 1000.0,
                );
            }
        }
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.transport.addresses()
    }
}

/// Human-readable name for a channel ID, for diagnostics.
fn channel_name(channel_id: u8) -> &'static str {
    match channel_id {
        protocol::CONTROL_CHANNEL => "control",
        protocol::SIGNAL_CHANNEL => "signal",
        protocol::VOICE_CHANNEL => "voice",
        protocol::VIDEO_CHANNEL => "video",
        protocol::SYSTEM_AUDIO_CHANNEL => "system_audio",
        _ => "unknown",
    }
}

/// Resolve the address to embed in connect tokens.
fn resolve_public_addr(
    config: &Config,
    local_addr: SocketAddr,
) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    if let Some(public) = &config.server.public_address {
        let resolved = public
            .to_socket_addrs()
            .map_err(|e| format!("could not resolve server.public_address '{public}': {e}"))?
            .next()
            .ok_or_else(|| format!("server.public_address '{public}' resolved to no addresses"))?;
        return Ok(resolved);
    }
    if local_addr.ip().is_unspecified() {
        return Err(
            "secure mode requires server.public_address when binding a wildcard host: \
             connect tokens embed the concrete address clients must dial, and a token \
             pointing at 0.0.0.0 is unusable. Set e.g. public_address: \"203.0.113.5:5000\""
                .into(),
        );
    }
    Ok(local_addr)
}
