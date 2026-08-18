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
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use protocol::{
    ChannelDenyReason, ChannelInfo, ChatImageMessage, ChatMessage, ClientMessage, DenyReason,
    ServerMessage, ShareScope,
};

use crate::auth;
use crate::config::{ChannelSettings, Config, validate_channel_list};
use crate::relay::{ShareInfo, VideoRelay, VoiceRelay};

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

/// How often to stat the config file for hot reload. A stat every few
/// seconds is free; anything file-watcher-fancier buys nothing here.
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(3);

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
    /// Voice-channel membership: client → channel id (index into
    /// `channel_infos`). Every connected client has an entry.
    channel_of: HashMap<ClientId, u16>,
    /// The channel list announced to clients, built once from config.
    channel_infos: Vec<ChannelInfo>,
    /// Where new clients land (config `default: true`).
    default_channel: u16,
    /// Config file to watch for hot reload (None = watching disabled,
    /// e.g. in tests that drive `apply_channel_config` directly).
    config_path: Option<PathBuf>,
    /// Last observed mtime of `config_path`.
    config_mtime: Option<SystemTime>,
    last_config_check: Instant,
    /// Recent chat-image send times per client, pruned to the last
    /// minute — the sliding window behind `limits.max_images_per_minute`.
    image_stamps: HashMap<ClientId, Vec<Instant>>,
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

        // ── Voice channels ──────────────────────────────────────────
        let default_channel = config
            .validate_channels()
            .map_err(|e| format!("invalid server.yaml: {e}"))?;
        let channel_infos = config.channel_infos();
        info!(
            "Voice channels: {} (default: '{}')",
            channel_infos
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            channel_infos[default_channel as usize].name,
        );

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
            channel_of: HashMap::new(),
            channel_infos,
            default_channel,
            config_path: None,
            config_mtime: None,
            last_config_check: now,
            image_stamps: HashMap::new(),
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
        self.voice.process(&mut self.server, &self.deafened, &self.channel_of);
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

        if now.duration_since(self.last_config_check) >= CONFIG_CHECK_INTERVAL {
            self.check_config_reload();
            self.last_config_check = now;
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

    /// Sliding-window rate check for chat images: record `now`, prune
    /// stamps older than a minute, and refuse once the window holds
    /// `limits.max_images_per_minute` sends (0 = images disabled).
    fn allow_image(&mut self, client_id: ClientId) -> bool {
        let cap = self.config.limits.max_images_per_minute;
        if cap == 0 {
            return false;
        }
        let now = Instant::now();
        let stamps = self.image_stamps.entry(client_id).or_default();
        stamps.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if stamps.len() >= cap {
            warn!(
                "Rate-limiting chat images from {} ({} in the last minute)",
                self.usernames
                    .get(&client_id)
                    .map_or("<unknown>", String::as_str),
                stamps.len(),
            );
            return false;
        }
        stamps.push(now);
        true
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
        // Everyone lands in the default channel (config guarantees it
        // is open and unlimited).
        self.channel_of.insert(client_id, self.default_channel);

        // Full state snapshot for the new client: user list, channel
        // roster and memberships, active shares, and current audio
        // states — so a client joining an in-progress session renders
        // it correctly instead of assuming everyone is unmuted, nobody
        // is streaming, and the world is one big room.
        let init = ServerMessage::Init {
            usernames: self.usernames.clone(),
            client_id,
            server_name: self.config.server.name.clone(),
            greeting: self.config.server.greeting_message.clone(),
            channels: self.channel_infos.clone(),
            memberships: self.channel_of.clone(),
        };
        self.send_msg(client_id, &init);

        // Only announce streams the new client may actually watch:
        // channel-scoped shares outside the default channel are
        // invisible from where they land.
        let sharers: Vec<(ClientId, ShareInfo)> = self
            .video
            .sharers()
            .iter()
            .map(|(id, info)| (*id, *info))
            .collect();
        for (sharer_id, info) in sharers {
            if self.share_visible_from(info, sharer_id, self.default_channel) {
                self.send_msg(
                    client_id,
                    &ServerMessage::ScreenShareStarted {
                        client_id: sharer_id,
                        width: info.width,
                        height: info.height,
                        scope: info.scope,
                    },
                );
            }
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
                channel_id: self.default_channel,
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
        self.channel_of.remove(&client_id);
        self.image_stamps.remove(&client_id);

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
            for channel in [
                protocol::CONTROL_CHANNEL,
                protocol::SIGNAL_CHANNEL,
                protocol::MEDIA_CHANNEL,
            ] {
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
            ClientMessage::ChatImage(data) => {
                // Size is already enforced: decode_client_message refuses
                // payloads over MAX_CHAT_IMAGE_BYTES, so `data` is bounded
                // before this arm can run. This arm owns policy only.
                if !self.allow_image(client_id) {
                    return;
                }
                // Relay only what claims to be a PNG or GIF — the
                // desktop encodes exactly these two, so anything else is
                // a modified client wasting everyone's bandwidth.
                // Receivers sniff the same magic to pick a decoder.
                const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
                let is_png = data.starts_with(&PNG_MAGIC);
                let is_gif = data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a");
                if !is_png && !is_gif {
                    warn!(
                        "Dropping non-PNG/GIF chat image ({} bytes) from {}",
                        data.len(),
                        self.get_username(client_id),
                    );
                    return;
                }
                info!(
                    "[{}] sent an image ({} KB)",
                    self.get_username(client_id),
                    data.len() / 1024,
                );
                self.stats.messages_count += 1;
                self.broadcast_msg(&ServerMessage::ChatImage(ChatImageMessage {
                    client_id,
                    data,
                }));
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
            ClientMessage::ScreenShareStart {
                width,
                height,
                scope,
            } => {
                self.handle_share_start(client_id, width, height, scope);
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

            // ── Voice channels ──────────────────────────────────────
            ClientMessage::JoinChannel {
                channel_id,
                password,
            } => {
                self.handle_join_channel(client_id, channel_id, password.as_deref());
            }
        }
    }

    fn handle_share_start(&mut self, client_id: ClientId, width: u16, height: u16, scope: ShareScope) {
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

        // A re-announce that narrows scope Everyone → Channel must
        // evict out-of-channel viewers, exactly like a channel move.
        let previous_scope = self.video.share_info(client_id).map(|info| info.scope);

        let username = self.get_username(client_id);
        info!("{username} started screen share ({width}x{height}, {scope:?})");
        self.video.start_sharing(
            client_id,
            ShareInfo {
                width,
                height,
                scope,
            },
        );

        let started = ServerMessage::ScreenShareStarted {
            client_id,
            width,
            height,
            scope,
        };
        match scope {
            ShareScope::Everyone => self.broadcast_msg(&started),
            ShareScope::Channel => {
                // Only the sharer's channel learns about the stream —
                // for everyone else it doesn't exist. The sharer is a
                // member of their own channel, so this covers them too.
                let channel = self.channel_of.get(&client_id).copied();
                for peer in self.peers_in_channel(channel) {
                    self.send_msg(peer, &started);
                }
                if previous_scope == Some(ShareScope::Everyone) {
                    self.evict_out_of_channel_viewers(client_id, channel);
                    // Other channels saw this stream while it was
                    // public; tell them it's gone from their view.
                    let stopped = ServerMessage::ScreenShareStopped { client_id };
                    let outside: Vec<ClientId> = self
                        .server
                        .clients_id()
                        .into_iter()
                        .filter(|peer| {
                            *peer != client_id && self.channel_of.get(peer).copied() != channel
                        })
                        .collect();
                    for peer in outside {
                        self.send_msg(peer, &stopped);
                    }
                }
            }
        }
    }

    // ── Voice channels ──────────────────────────────────────────────

    /// Watch `path` for changes and hot-apply the `channels` section.
    /// Called once from main after startup; everything else in the
    /// file requires a restart (and says so in the log).
    pub fn watch_config_file(&mut self, path: PathBuf) {
        self.config_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        info!(
            "Watching {} for channel changes (checked every {}s; only the \
             [channels] section hot-applies)",
            path.display(),
            CONFIG_CHECK_INTERVAL.as_secs(),
        );
        self.config_path = Some(path);
    }

    /// If the watched config file's mtime moved, re-read it and apply
    /// the channel list. A file that fails to parse or validate is
    /// logged and ignored — a live server never dies or degrades from
    /// a bad edit.
    fn check_config_reload(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;

        let fresh = match Config::from_file(&path) {
            Ok(fresh) => fresh,
            Err(e) => {
                warn!("Config reload: {} is not valid YAML — keeping current config: {e}", path.display());
                return;
            }
        };
        if fresh.channels == self.config.channels {
            info!(
                "Config reload: {} changed but the [channels] section did not — \
                 other sections require a restart to apply",
                path.display(),
            );
            return;
        }
        match self.apply_channel_config(fresh.channels) {
            Ok(()) => info!("Config reload: channel list applied from {}", path.display()),
            Err(e) => warn!("Config reload: invalid [channels] — keeping current list: {e}"),
        }
    }

    /// Swap in a new channel list on a live server.
    ///
    /// Channels are matched **by name**: a channel that keeps its name
    /// keeps its members (even if it moved position, or its password /
    /// user limit changed — nobody is ever kicked by a reload). Members
    /// of a removed or renamed channel are moved to the new default.
    /// Clients get one authoritative [`ServerMessage::ChannelListUpdated`]
    /// (ids are list indexes, so a reload renumbers them and clients
    /// must resync wholesale), and channel-scoped share visibility is
    /// re-derived with exactly-once corrective Started/Stopped messages.
    pub fn apply_channel_config(&mut self, channels: Vec<ChannelSettings>) -> Result<(), String> {
        let new_default = validate_channel_list(&channels)?;

        #[allow(clippy::cast_possible_truncation)] // validated ≤ MAX_CHANNELS
        let new_id_of: HashMap<String, u16> = channels
            .iter()
            .enumerate()
            .map(|(i, ch)| (ch.name.trim().to_string(), i as u16))
            .collect();

        // Snapshot who-was-where by *name* before renumbering.
        let old_name_of: HashMap<ClientId, String> = self
            .channel_of
            .iter()
            .map(|(client, id)| {
                let name = self
                    .channel_infos
                    .get(usize::from(*id))
                    .map(|info| info.name.clone())
                    .unwrap_or_default();
                (*client, name)
            })
            .collect();

        let mut moved_to_default = 0usize;
        let new_channel_of: HashMap<ClientId, u16> = old_name_of
            .iter()
            .map(|(client, name)| {
                let new_id = new_id_of.get(name).copied().unwrap_or_else(|| {
                    moved_to_default += 1;
                    new_default
                });
                (*client, new_id)
            })
            .collect();

        self.config.channels = channels;
        self.channel_infos = self.config.channel_infos();
        self.default_channel = new_default;
        self.channel_of = new_channel_of;

        info!(
            "Channel list updated: {} (default: '{}'); {} member(s) moved to the default",
            self.channel_infos
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            self.channel_infos[usize::from(new_default)].name,
            moved_to_default,
        );

        self.broadcast_msg(&ServerMessage::ChannelListUpdated {
            channels: self.channel_infos.clone(),
            memberships: self.channel_of.clone(),
        });

        // ── Re-derive channel-scoped share visibility ───────────────
        // For each channel-scoped stream, compare each client's view
        // before (same channel *name* as the sharer) and after (same
        // new channel id): gained → Started, lost → Stopped, exactly
        // once. Out-of-channel subscriptions are dropped silently here;
        // the delta loop delivers their Stopped.
        let sharers: Vec<(ClientId, ShareInfo)> = self
            .video
            .sharers()
            .iter()
            .filter(|(_, info)| info.scope == ShareScope::Channel)
            .map(|(id, info)| (*id, *info))
            .collect();
        for (sharer, info) in sharers {
            let Some(sharer_channel) = self.channel_of.get(&sharer).copied() else {
                continue;
            };
            for viewer in self.video.subscribers_of(sharer) {
                if self.channel_of.get(&viewer).copied() != Some(sharer_channel) {
                    self.video.unsubscribe(viewer, sharer);
                }
            }

            let sharer_old_name = old_name_of.get(&sharer).cloned().unwrap_or_default();
            let started = ServerMessage::ScreenShareStarted {
                client_id: sharer,
                width: info.width,
                height: info.height,
                scope: info.scope,
            };
            let stopped = ServerMessage::ScreenShareStopped { client_id: sharer };
            for peer in self.server.clients_id() {
                if peer == sharer {
                    continue;
                }
                let saw_before = old_name_of.get(&peer) == Some(&sharer_old_name);
                let sees_now = self.channel_of.get(&peer).copied() == Some(sharer_channel);
                if sees_now && !saw_before {
                    self.send_msg(peer, &started);
                } else if saw_before && !sees_now {
                    self.send_msg(peer, &stopped);
                }
            }
        }
        Ok(())
    }

    /// Validate and execute a channel move. Password and capacity
    /// checks run here; on success the move is broadcast (including to
    /// the mover) and channel-scoped share visibility is re-validated.
    fn handle_join_channel(&mut self, client_id: ClientId, channel_id: u16, password: Option<&str>) {
        let Some(target) = self.config.channels.get(usize::from(channel_id)) else {
            self.deny_join(client_id, channel_id, ChannelDenyReason::NoSuchChannel);
            return;
        };
        let old_channel = self.channel_of.get(&client_id).copied();
        if old_channel == Some(channel_id) {
            return; // already there — nothing to do
        }
        if let Some(expected) = target.password.as_deref()
            && password != Some(expected)
        {
            info!(
                "{} denied joining '{}': bad or missing password",
                self.get_username(client_id),
                target.name,
            );
            self.deny_join(client_id, channel_id, ChannelDenyReason::WrongPassword);
            return;
        }
        if let Some(limit) = target.user_limit
            && self.members_in(channel_id) >= usize::from(limit)
        {
            self.deny_join(client_id, channel_id, ChannelDenyReason::ChannelFull);
            return;
        }

        info!(
            "{} moved to channel '{}'",
            self.get_username(client_id),
            target.name,
        );
        self.channel_of.insert(client_id, channel_id);
        self.broadcast_msg(&ServerMessage::UserMovedChannel {
            client_id,
            channel_id,
        });
        self.revalidate_shares_after_move(client_id, old_channel, channel_id);
    }

    fn deny_join(&mut self, client_id: ClientId, channel_id: u16, reason: ChannelDenyReason) {
        self.send_msg(
            client_id,
            &ServerMessage::JoinChannelDenied { channel_id, reason },
        );
    }

    /// Members currently in `channel_id`.
    fn members_in(&self, channel_id: u16) -> usize {
        self.channel_of.values().filter(|c| **c == channel_id).count()
    }

    /// Connected clients in `channel` (None never matches anyone).
    fn peers_in_channel(&self, channel: Option<u16>) -> Vec<ClientId> {
        self.server
            .clients_id()
            .into_iter()
            .filter(|peer| channel.is_some() && self.channel_of.get(peer).copied() == channel)
            .collect()
    }

    /// Can a client in `viewer_channel` see this stream?
    fn share_visible_from(&self, info: ShareInfo, sharer: ClientId, viewer_channel: u16) -> bool {
        match info.scope {
            ShareScope::Everyone => true,
            ShareScope::Channel => self.channel_of.get(&sharer).copied() == Some(viewer_channel),
        }
    }

    /// Unsubscribe every viewer of `sharer` who is not in `channel`,
    /// telling each their view of the stream ended.
    fn evict_out_of_channel_viewers(&mut self, sharer: ClientId, channel: Option<u16>) {
        let stopped = ServerMessage::ScreenShareStopped { client_id: sharer };
        for viewer in self.video.subscribers_of(sharer) {
            if self.channel_of.get(&viewer).copied() != channel {
                self.video.unsubscribe(viewer, sharer);
                self.send_msg(viewer, &stopped);
            }
        }
    }

    /// Channel-scoped share visibility follows membership. After
    /// `mover` goes `old` → `new`:
    /// - if the mover is sharing channel-scoped: the stream leaves the
    ///   old channel (badge cleared, viewers evicted) and appears in
    ///   the new one;
    /// - any channel-scoped stream the mover was watching or could see
    ///   in the old channel disappears for them, and streams in the new
    ///   channel are announced to them.
    ///
    /// Everyone-scoped streams are unaffected throughout.
    fn revalidate_shares_after_move(&mut self, mover: ClientId, old: Option<u16>, new: u16) {
        // ── The mover as a sharer ───────────────────────────────────
        if let Some(info) = self.video.share_info(mover)
            && info.scope == ShareScope::Channel
        {
            // Old channel: stream gone (this also reaches evicted
            // viewers, closing their windows).
            let stopped = ServerMessage::ScreenShareStopped { client_id: mover };
            for peer in self.peers_in_channel(old) {
                if peer != mover {
                    self.send_msg(peer, &stopped);
                }
            }
            self.evict_out_of_channel_viewers(mover, Some(new));
            // New channel: stream appears.
            let started = ServerMessage::ScreenShareStarted {
                client_id: mover,
                width: info.width,
                height: info.height,
                scope: info.scope,
            };
            for peer in self.peers_in_channel(Some(new)) {
                if peer != mover {
                    self.send_msg(peer, &started);
                }
            }
        }

        // ── The mover as a viewer / potential viewer ────────────────
        let sharers: Vec<(ClientId, ShareInfo)> = self
            .video
            .sharers()
            .iter()
            .map(|(id, info)| (*id, *info))
            .collect();
        for (sharer, info) in sharers {
            if sharer == mover || info.scope != ShareScope::Channel {
                continue;
            }
            let sharer_channel = self.channel_of.get(&sharer).copied();
            if sharer_channel == Some(new) {
                // Snapshot semantics, like connecting: a stream lives
                // in the channel the mover just entered.
                self.send_msg(
                    mover,
                    &ServerMessage::ScreenShareStarted {
                        client_id: sharer,
                        width: info.width,
                        height: info.height,
                        scope: info.scope,
                    },
                );
            } else if sharer_channel == old {
                // The mover left this stream behind.
                if self.video.is_subscribed(mover, sharer) {
                    self.video.unsubscribe(mover, sharer);
                }
                self.send_msg(mover, &ServerMessage::ScreenShareStopped { client_id: sharer });
            }
        }
    }

    fn handle_subscribe(&mut self, client_id: ClientId, target_id: ClientId) {
        let Some(info) = self.video.share_info(target_id) else {
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::NotSharing,
                },
            );
            return;
        };

        // Channel-scoped streams are watchable only from the sharer's
        // channel. (The stream is normally invisible from elsewhere —
        // this closes the modified-client path.)
        if info.scope == ShareScope::Channel
            && self.channel_of.get(&client_id) != self.channel_of.get(&target_id)
        {
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::NotInChannel,
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
        // Both reliable channels disconnect on ceiling; media carries
        // the big payloads now, so it's the one that usually matters.
        let watched = [
            ("control", protocol::CONTROL_CHANNEL, protocol::channels::CONTROL_BUFFER_BYTES),
            ("media", protocol::MEDIA_CHANNEL, protocol::channels::MEDIA_BUFFER_BYTES),
        ];
        for client_id in self.server.clients_id() {
            for (name, channel, max_bytes) in watched {
                let threshold = max_bytes / 4; // warn at 75% used
                let available = self.server.channel_available_memory(client_id, channel);
                if available < threshold {
                    let username = self.get_username(client_id);
                    let used_pct = (1.0 - available as f64 / max_bytes as f64) * 100.0;
                    warn!(
                        "CHANNEL PRESSURE: {username} ({client_id}) {name} channel at \
                         {used_pct:.0}% ({available} bytes remaining of {max_bytes})",
                    );
                }
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
        protocol::MEDIA_CHANNEL => "media",
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
