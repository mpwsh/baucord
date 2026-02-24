//! Chat server — connection management, message routing, media relay.
//!
//! Uses the shared `protocol` crate for all wire types and constants.
//! Delegates voice and video relay to dedicated structs in `relay.rs`.

use bincode::Options;
use log::{info, warn};
use renet::{ChannelConfig, ClientId, ConnectionConfig, RenetServer, SendType, ServerEvent};
use renet_netcode::{
    NETCODE_USER_DATA_BYTES, NetcodeServerTransport, ServerAuthentication, ServerConfig,
};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

use protocol::{ChatMessage, ClientMessage, RELIABLE_BUFFER_BYTES, ServerMessage};

use crate::config::{ChannelSettings, Config};
use crate::relay::{VideoRelay, VoiceRelay};

// ── Helpers ─────────────────────────────────────────────────────────────

/// Extract a UTF-8 username from renet's fixed-size user data buffer.
fn username_from_user_data(user_data: &[u8; NETCODE_USER_DATA_BYTES]) -> String {
    let len = (user_data[0] as usize).min(NETCODE_USER_DATA_BYTES - 1);
    String::from_utf8_lossy(&user_data[1..=len]).into_owned()
}

/// Serialize a `ServerMessage` via bincode, logging on failure.
fn encode_message(msg: &ServerMessage) -> Option<Vec<u8>> {
    bincode::options()
        .serialize(msg)
        .map_err(|e| {
            warn!("Failed to serialize message: {e}");
        })
        .ok()
}

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
    last_updated: Instant,
    last_stats: Instant,
    stats: ServerStats,
    voice: VoiceRelay,
    video: VideoRelay,
}

impl ChatServer {
    pub fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        let addr = config.socket_addr()?;
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        let public_addr = socket.local_addr()?;

        info!("{} starting on {public_addr}", config.server.name);

        let server_config = ServerConfig {
            current_time: SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?,
            max_clients: config.network.max_clients,
            protocol_id: config.network.protocol_id,
            public_addresses: vec![public_addr],
            authentication: if config.authentication.enabled {
                ServerAuthentication::Secure {
                    private_key: [0; 32],
                }
            } else {
                ServerAuthentication::Unsecure
            },
        };

        let transport = NetcodeServerTransport::new(server_config, socket)?;
        let connection_config = Self::create_connection_config(&config.channels);
        let server = RenetServer::new(connection_config);

        Ok(Self {
            config,
            server,
            transport,
            usernames: HashMap::new(),
            last_updated: Instant::now(),
            last_stats: Instant::now(),
            stats: ServerStats::default(),
            voice: VoiceRelay::new(),
            video: VideoRelay::new(),
        })
    }

    fn create_connection_config(channels: &ChannelSettings) -> ConnectionConfig {
        let mut config = ConnectionConfig::default();

        config.client_channels_config.clear();
        config.server_channels_config.clear();

        let reliable = ChannelConfig {
            channel_id: channels.reliable_channel_id,
            max_memory_usage_bytes: RELIABLE_BUFFER_BYTES,
            send_type: SendType::ReliableOrdered {
                resend_time: Duration::from_millis(200),
            },
        };
        config.client_channels_config.push(reliable.clone());
        config.server_channels_config.push(reliable);

        let voice = ChannelConfig {
            channel_id: channels.voice_channel_id,
            max_memory_usage_bytes: channels.voice_buffer_size_mb * 1024 * 1024,
            send_type: SendType::Unreliable,
        };
        config.client_channels_config.push(voice.clone());
        config.server_channels_config.push(voice);

        let video = ChannelConfig {
            channel_id: channels.video_channel_id,
            max_memory_usage_bytes: channels.video_buffer_size_mb * 1024 * 1024,
            send_type: SendType::Unreliable,
        };
        config.client_channels_config.push(video.clone());
        config.server_channels_config.push(video);

        let reliable_video = ChannelConfig {
            channel_id: channels.reliable_video_channel_id,
            max_memory_usage_bytes: channels.reliable_video_buffer_size_mb * 1024 * 1024,
            send_type: SendType::ReliableOrdered {
                resend_time: Duration::from_millis(200),
            },
        };
        config.client_channels_config.push(reliable_video.clone());
        config.server_channels_config.push(reliable_video);

        config
    }

    /// Drive one tick of the server: receive, process, send.
    ///
    /// The caller controls the loop cadence. For lowest voice latency,
    /// call this as fast as possible (≤1 ms sleeps or spin-poll).
    pub fn update(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let now = Instant::now();
        let duration = now - self.last_updated;
        self.last_updated = now;

        self.server.update(duration);
        self.transport.update(duration, &mut self.server)?;

        self.process_events();
        self.process_reliable_messages();
        self.voice
            .process(&mut self.server, self.config.channels.voice_channel_id);
        self.video.process(
            &mut self.server,
            self.config.channels.video_channel_id,
            self.config.channels.reliable_video_channel_id,
        );

        self.transport.send_packets(&mut self.server);

        if now.duration_since(self.last_stats)
            >= Duration::from_secs(self.config.logging.stats_interval_secs)
        {
            self.print_stats();
            self.last_stats = now;
        }

        Ok(())
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
        let username = self.transport.user_data(client_id).map_or_else(
            || format!("User{client_id}"),
            |data| username_from_user_data(&data),
        );

        info!("{username} ({client_id}) connected");
        self.usernames.insert(client_id, username.clone());

        let reliable = self.config.channels.reliable_channel_id;

        // Send init with the full user list to the new client
        let init_msg = ServerMessage::InitClient {
            usernames: self.usernames.clone(),
            server_name: self.config.server.name.clone(),
            greeting: self.config.server.greeting_message.clone(),
        };
        if let Some(encoded) = encode_message(&init_msg) {
            self.server.send_message(client_id, reliable, encoded);
        }

        // Notify the new client about any active screen shares
        for (sharer_id, &(width, height)) in self.video.sharers() {
            let msg = ServerMessage::ScreenShareStarted {
                client_id: *sharer_id,
                width,
                height,
            };
            if let Some(encoded) = encode_message(&msg) {
                self.server.send_message(client_id, reliable, encoded);
            }
        }

        // Notify everyone else about the new client
        let connected_msg = ServerMessage::ClientConnected {
            client_id,
            username,
        };
        if let Some(encoded) = encode_message(&connected_msg) {
            self.server
                .broadcast_message_except(client_id, reliable, encoded);
        }
    }

    fn handle_client_disconnected(&mut self, client_id: ClientId, reason: renet::DisconnectReason) {
        let reliable = self.config.channels.reliable_channel_id;

        if let Some(username) = self.usernames.remove(&client_id) {
            info!("{username} ({client_id}) disconnected: {reason}");
        }

        // Clean up screen share state — if they were sharing, notify everyone
        let was_sharing = self.video.is_sharing(client_id);
        self.video.handle_disconnect(client_id);

        if was_sharing {
            let msg = ServerMessage::ScreenShareStopped { client_id };
            if let Some(encoded) = encode_message(&msg) {
                self.server.broadcast_message(reliable, encoded);
            }
        }

        // Notify everyone about the disconnect
        let msg = ServerMessage::ClientDisconnected { client_id };
        if let Some(encoded) = encode_message(&msg) {
            self.server.broadcast_message(reliable, encoded);
        }
    }

    // ── Reliable messages ───────────────────────────────────────────

    fn process_reliable_messages(&mut self) {
        let channel_id = self.config.channels.reliable_channel_id;

        for client_id in self.server.clients_id() {
            while let Some(message) = self.server.receive_message(client_id, channel_id) {
                self.stats.bytes_received += message.len() as u64;

                if let Ok(msg) = bincode::options().deserialize::<ClientMessage>(&message) {
                    self.handle_client_message(client_id, msg);
                }
            }
        }
    }

    fn handle_client_message(&mut self, client_id: ClientId, message: ClientMessage) {
        let reliable = self.config.channels.reliable_channel_id;

        match message {
            ClientMessage::Text(text) => {
                let username = self.get_username(client_id);
                info!("[{username}]: {text}");
                self.stats.messages_count += 1;

                let msg = ServerMessage::ClientMessage(ChatMessage { client_id, text });
                if let Some(encoded) = encode_message(&msg) {
                    self.stats.bytes_sent +=
                        encoded.len() as u64 * self.server.connected_clients() as u64;
                    self.server.broadcast_message(reliable, encoded);
                }
            }
            ClientMessage::Speaking(is_speaking) => {
                let msg = ServerMessage::Speaking {
                    client_id,
                    is_speaking,
                };
                if let Some(encoded) = encode_message(&msg) {
                    self.server.broadcast_message(reliable, encoded);
                }
            }
            ClientMessage::AudioState { muted, deafened } => {
                let msg = ServerMessage::AudioState {
                    client_id,
                    muted,
                    deafened,
                };
                if let Some(encoded) = encode_message(&msg) {
                    self.server
                        .broadcast_message_except(client_id, reliable, encoded);
                }
            }
            ClientMessage::VoiceData(_) => {
                warn!("Voice data received on reliable channel from {client_id}");
            }

            // ── Screen share signaling ──────────────────────────────
            ClientMessage::ScreenShareStart { width, height } => {
                let username = self.get_username(client_id);
                info!("{username} started screen share ({width}x{height})");

                self.video.start_sharing(client_id, width, height);

                let msg = ServerMessage::ScreenShareStarted {
                    client_id,
                    width,
                    height,
                };
                if let Some(encoded) = encode_message(&msg) {
                    self.server.broadcast_message(reliable, encoded);
                }
            }
            ClientMessage::ScreenShareStop => {
                let username = self.get_username(client_id);
                info!("{username} stopped screen share");

                self.video.stop_sharing(client_id);

                let msg = ServerMessage::ScreenShareStopped { client_id };
                if let Some(encoded) = encode_message(&msg) {
                    self.server.broadcast_message(reliable, encoded);
                }
            }
            ClientMessage::SubscribeScreen { target_id } => {
                self.video.subscribe(client_id, target_id);
            }
            ClientMessage::UnsubscribeScreen { target_id } => {
                self.video.unsubscribe(client_id, target_id);
            }
        }
    }

    // ── Utilities ───────────────────────────────────────────────────

    fn get_username(&self, client_id: ClientId) -> String {
        self.usernames
            .get(&client_id)
            .cloned()
            .unwrap_or_else(|| format!("User{client_id}"))
    }

    #[allow(clippy::cast_precision_loss)]
    fn print_stats(&self) {
        info!("=== {} Statistics ===", self.config.server.name);
        info!("Connected clients: {}", self.server.connected_clients());
        info!("Text messages: {}", self.stats.messages_count);

        self.voice.print_stats();
        self.video.print_stats();

        info!(
            "Data sent: {:.2} MB, received: {:.2} MB",
            self.stats.bytes_sent as f64 / 1_048_576.0,
            self.stats.bytes_received as f64 / 1_048_576.0
        );

        info!(
            "Active users: {:?}",
            self.usernames.values().collect::<Vec<_>>()
        );
    }

    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.transport.addresses()
    }
}
