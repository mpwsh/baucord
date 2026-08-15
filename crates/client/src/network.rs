// network.rs — Renet client + network thread.
//
// NetworkClient handles protocol and serialization; NetworkThreadHandle
// owns the thread that polls the client and routes voice packets to the
// voice thread and video packets to the screen thread.
//
// All wire protocol types come from the shared `protocol` crate.
//
// Authentication: when `token_url` is configured, the client fetches a
// netcode ConnectToken over HTTPS (the server's in-process issuer,
// behind the deployment's TLS proxy) and connects in secure mode — the
// server assigns our client id and username via the token. Without a
// token URL it falls back to open (unsecure) mode for LAN/dev servers.
//
// Media attribution: relayed media arrives wrapped in the server's
// sender envelope ([sender_id: u64][packet]) — the sender id is stamped
// by the server from the authenticated transport, so it cannot be
// spoofed by other clients. See `protocol::relayed`.

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use log::warn;
use renet::{ClientId, RenetClient};
use renet_netcode::{
    ClientAuthentication, ConnectToken, NetcodeClientTransport, NetcodeTransportError,
    generate_random_bytes,
};
use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use audio::AudioPacket;

use protocol::{
    CLIENT_BYTES_PER_TICK, CONTROL_CHANNEL, ClientMessage, DenyReason, MEDIA_CHANNEL, PROTOCOL_ID,
    SIGNAL_CHANNEL,
    SYSTEM_AUDIO_CHANNEL, ServerMessage, VIDEO_CHANNEL, VOICE_CHANNEL, VideoReassembler,
    pack_system_audio, pack_video_frame, pack_voice, parse_system_audio, parse_voice, split_sender,
};

use crate::WakeFn;
use crate::screen::ScreenControl;
use crate::voice::VoiceControl;

/// How often to sample link statistics for the UI.
const NETWORK_STATS_INTERVAL: Duration = Duration::from_secs(2);

/// Timeout for the connect-token HTTP fetch.
const TOKEN_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest token response we will read (a netcode token is 2048 bytes).
const MAX_TOKEN_RESPONSE_BYTES: u64 = 8192;

// ── Messages sent to the UI / App layer ─────────────────────────────────

#[derive(Debug, Clone)]
pub enum NetworkMessage {
    /// Outbound image upload progress, 0–100. 100 means the reliable
    /// channel has drained (every byte acked) — the UI hides its
    /// indicator on it.
    UploadProgress { pct: u8 },
    Connected,
    Disconnected(String),
    InitClient {
        users: HashMap<ClientId, String>,
        client_id: u64,
        server_name: String,
        greeting: String,
    },
    UserJoined {
        client_id: ClientId,
        username: String,
    },
    UserLeft {
        client_id: ClientId,
        username: String,
    },
    TextMessage {
        username: String,
        text: String,
    },
    /// A chat image from a user (encoded PNG bytes, ≤
    /// `protocol::MAX_CHAT_IMAGE_BYTES`).
    ImageMessage {
        username: String,
        data: Vec<u8>,
    },
    VoiceData {
        client_id: ClientId,
        seq: u32,
        timestamp: u32,
        data: Vec<u8>,
    },
    UserSpeaking {
        client_id: ClientId,
        is_speaking: bool,
    },
    /// Another user changed their mute/deafen state.
    UserAudioState {
        client_id: ClientId,
        muted: bool,
        deafened: bool,
    },
    /// Periodic link statistics (replaces the old `LatencyUpdate`,
    /// which carried only RTT while renet was already measuring loss
    /// and throughput).
    NetworkStats {
        rtt_ms: f64,
        /// Packet loss, 0.0–100.0 (percent).
        loss_pct: f64,
        /// Outbound throughput in kbit/s.
        kbps_up: f64,
        /// Inbound throughput in kbit/s.
        kbps_down: f64,
    },

    // ── Screen share events ─────────────────────────────────────────
    /// A remote user started sharing their screen.
    ScreenShareStarted {
        client_id: ClientId,
        width: u32,
        height: u32,
    },
    /// A remote user stopped sharing their screen.
    ScreenShareStopped {
        client_id: ClientId,
    },
    /// The server refused our screen share request (limits/validation).
    ShareDenied {
        reason: DenyReason,
    },
    /// The server refused our subscribe request (limits/not sharing).
    SubscribeDenied {
        target_id: ClientId,
        reason: DenyReason,
    },
    /// An inbound video frame (routed to screen thread, not app).
    VideoFrame {
        client_id: u64,
        seq: u32,
        is_keyframe: bool,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// An inbound system audio packet (routed to screen thread, not app).
    SystemAudioData {
        client_id: u64,
        seq: u32,
        data: Vec<u8>,
    },
    /// A viewer of our screen share requested a fresh keyframe
    /// (routed to screen thread, not app).
    KeyframeRequested {
        viewer_id: ClientId,
    },
    /// Aggregated viewer health for the stream we are sharing
    /// (routed to screen thread, not app — drives fps adaptation).
    StreamHealth {
        worst_loss: f32,
        worst_rtt_ms: f32,
        viewers: u16,
    },
}

// ── Token fetch ─────────────────────────────────────────────────────────

/// Fetch a netcode ConnectToken from the issuer.
///
/// Blocking (runs on the network thread during connect). The URL should
/// be HTTPS in any real deployment — the token contains the session keys.
fn fetch_connect_token(
    url: &str,
    username: &str,
    password: Option<&str>,
) -> Result<ConnectToken, String> {
    let mut request = ureq::post(url).timeout(TOKEN_FETCH_TIMEOUT);
    if let Some(pass) = password {
        request = request.set("x-baucord-pass", pass);
    }

    let response = match request.send_string(username) {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(format!("token server returned {code}: {}", body.trim()));
        }
        Err(e) => return Err(format!("token request failed: {e}")),
    };

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_TOKEN_RESPONSE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read token response: {e}"))?;

    ConnectToken::read(&mut bytes.as_slice()).map_err(|e| format!("invalid connect token: {e}"))
}

// ════════════════════════════════════════════════════════════════════════
//  NetworkClient — protocol + state
// ════════════════════════════════════════════════════════════════════════

pub(crate) struct NetworkClient {
    client: RenetClient,
    transport: NetcodeClientTransport,
    usernames: HashMap<ClientId, String>,
    message_queue: Vec<NetworkMessage>,
    last_updated: Instant,
    reassembler: VideoReassembler,

    // Speaking status dedupe (only send on change)
    last_speaking_sent: bool,
    /// Total bytes of an in-flight chat image upload, if any. Progress
    /// is inferred from the media channel's buffered (unacked) bytes
    /// draining back toward zero — renet exposes no per-message acks,
    /// but with one upload at a time the channel's memory usage IS the
    /// unacked remainder.
    upload_total: Option<usize>,
    /// Last percentage reported to the UI (255 = force first emit).
    upload_last_pct: u8,
    last_stats_check: Instant,
    /// Whether we've announced `NetworkMessage::Connected` to the app.
    /// Set once the netcode handshake actually completes.
    announced_connected: bool,
    /// Warn about OS send-buffer pressure once per connection, not per
    /// tick — bursts can repeat every frame during a stream.
    send_pressure_warned: bool,
}

impl NetworkClient {
    pub fn connect(
        server_addr: SocketAddr,
        username: &str,
        token_url: Option<&str>,
        token_password: Option<&str>,
    ) -> Result<Self, String> {
        let client = RenetClient::new(protocol::connection_config(CLIENT_BYTES_PER_TICK));

        let socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Failed to bind socket: {e}"))?;
        let sock_ref = socket2::SockRef::from(&socket);
        sock_ref
            .set_send_buffer_size(protocol::UDP_SOCKET_BUFFER_BYTES)
            .ok();
        sock_ref
            .set_recv_buffer_size(protocol::UDP_SOCKET_BUFFER_BYTES)
            .ok();
        // The sets above are best-effort; log what the OS actually
        // granted so a clamped buffer (firewall LSPs, policy) is
        // visible in baucord.log when diagnosing ENOBUFS reports.
        if let (Ok(snd), Ok(rcv)) = (sock_ref.send_buffer_size(), sock_ref.recv_buffer_size()) {
            log::info!("UDP socket buffers: send {} KB, recv {} KB", snd / 1024, rcv / 1024);
        }

        let current_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| format!("System time error: {e}"))?;

        let authentication = match token_url {
            Some(url) => {
                // Secure mode: the issuer assigns our client id and
                // embeds our username inside the token; the connection
                // is keyed by material derived from the server's
                // private key.
                log::info!("Fetching connect token from {url}");
                let connect_token = fetch_connect_token(url, username, token_password)?;
                ClientAuthentication::Secure { connect_token }
            }
            None => {
                // Open mode (LAN/dev). The previous implementation used
                // the wall-clock millisecond timestamp as the client id,
                // which collides when two people connect in the same
                // millisecond and is trivially predictable. Random is
                // free.
                let mut client_id = u64::from_le_bytes(generate_random_bytes());
                if client_id == 0 {
                    client_id = 1;
                }
                ClientAuthentication::Unsecure {
                    server_addr,
                    client_id,
                    user_data: Some(protocol::encode_username(&protocol::sanitize_username(
                        username,
                    ))),
                    protocol_id: PROTOCOL_ID,
                }
            }
        };

        let transport = NetcodeClientTransport::new(current_time, authentication, socket)
            .map_err(|e| format!("Failed to create transport: {e}"))?;

        Ok(NetworkClient {
            client,
            transport,
            usernames: HashMap::new(),
            message_queue: Vec::new(),
            last_updated: Instant::now(),
            reassembler: VideoReassembler::new(),
            last_speaking_sent: false,
            upload_total: None,
            upload_last_pct: 255,
            last_stats_check: Instant::now(),
            announced_connected: false,
            send_pressure_warned: false,
        })
    }

    // ── Tick ────────────────────────────────────────────────────────

    pub fn update(&mut self) {
        let now = Instant::now();
        let duration = now - self.last_updated;
        self.last_updated = now;

        self.client.update(duration);

        if let Err(e) = self.transport.update(duration, &mut self.client) {
            log::error!("Transport error: {e:?}");
            self.message_queue
                .push(NetworkMessage::Disconnected(e.to_string()));
            return;
        }

        if let Some(reason) = self.client.disconnect_reason() {
            log::error!("Renet disconnect reason: {reason:?}");
            self.message_queue
                .push(NetworkMessage::Disconnected(reason.to_string()));
            return;
        }

        self.sample_upload_progress();

        // Announce Connected exactly once, when the handshake completes.
        if !self.announced_connected && self.client.is_connected() {
            self.announced_connected = true;
            self.message_queue.push(NetworkMessage::Connected);
        }

        self.process_control_messages();
        self.process_voice_messages();
        self.process_video_messages();
        self.process_system_audio_messages();

        if let Err(e) = self.transport.send_packets(&mut self.client) {
            // Transient send-buffer pressure. Two flavors, same
            // meaning for UDP as congestion loss — drop the datagrams
            // and let renet's reliability layer re-send; never a
            // reason to tear the connection down:
            //   * WouldBlock — the socket's own queue is full
            //     (WSAEWOULDBLOCK = 10035 on Windows).
            //   * ENOBUFS "no buffer space available" — the OS or
            //     interface queue is full (WSAENOBUFS = 10055 on
            //     Windows, errno 105 on Linux, 55 on macOS). Windows
            //     raises this routinely during video keyframe bursts,
            //     especially on Wi-Fi, even with a large SO_SNDBUF.
            // If pressure persists long enough to matter, keepalives
            // stop landing and netcode's own timeout produces a real
            // disconnect. The typed check is primary; the string probe
            // is a fallback for errors surfacing through wrapped
            // variants — computed once, only on the (rare) error path.
            #[cfg(windows)]
            const ENOBUFS: i32 = 10055;
            #[cfg(target_os = "macos")]
            const ENOBUFS: i32 = 55;
            #[cfg(not(any(windows, target_os = "macos")))]
            const ENOBUFS: i32 = 105;
            let is_transient = matches!(
                &e,
                NetcodeTransportError::IO(io_err)
                    if io_err.kind() == std::io::ErrorKind::WouldBlock
                        || io_err.raw_os_error() == Some(ENOBUFS)
            ) || {
                let msg = e.to_string();
                msg.contains("10035")
                    || msg.contains("10055")
                    || msg.contains("WouldBlock")
                    || msg.contains("buffer space")
            };
            if is_transient {
                if !self.send_pressure_warned {
                    self.send_pressure_warned = true;
                    log::warn!(
                        "send_packets: OS send buffer pressure ({e}); dropping this batch and continuing (logged once per connection)"
                    );
                }
                log::debug!("send_packets: transient buffer pressure, will retry next tick");
            } else {
                self.message_queue
                    .push(NetworkMessage::Disconnected(format!(
                        "Failed to send packets: {e}"
                    )));
            }
        }

        // Sample link statistics for the UI every ~2 seconds. renet
        // already measures loss and throughput alongside RTT — the old
        // code exposed only RTT and left the rest unread.
        if now.duration_since(self.last_stats_check) >= NETWORK_STATS_INTERVAL {
            self.last_stats_check = now;
            if self.client.is_connected() {
                let info = self.client.network_info();
                self.message_queue.push(NetworkMessage::NetworkStats {
                    rtt_ms: info.rtt * 1000.0,
                    loss_pct: info.packet_loss * 100.0,
                    kbps_up: info.bytes_sent_per_second * 8.0 / 1000.0,
                    kbps_down: info.bytes_received_per_second * 8.0 / 1000.0,
                });
            }
        }
    }

    // ── Control channels (control + signal) ─────────────────────────

    fn process_control_messages(&mut self) {
        for channel in [CONTROL_CHANNEL, SIGNAL_CHANNEL, MEDIA_CHANNEL] {
            while let Some(message) = self.client.receive_message(channel) {
                let Some(msg) = protocol::decode_server_message(&message) else {
                    continue;
                };
                self.handle_server_message(msg);
            }
        }
    }

    fn handle_server_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Init {
                usernames,
                client_id,
                server_name,
                greeting,
            } => {
                self.usernames = usernames.clone();
                self.message_queue.push(NetworkMessage::InitClient {
                    client_id,
                    users: usernames,
                    server_name,
                    greeting,
                });
            }
            ServerMessage::UserJoined {
                client_id,
                username,
            } => {
                self.usernames.insert(client_id, username.clone());
                self.message_queue.push(NetworkMessage::UserJoined {
                    client_id,
                    username,
                });
            }
            ServerMessage::UserLeft { client_id } => {
                let username = self
                    .usernames
                    .remove(&client_id)
                    .unwrap_or_else(|| format!("User{client_id}"));
                self.message_queue.push(NetworkMessage::UserLeft {
                    client_id,
                    username,
                });
            }
            ServerMessage::Chat(msg) => {
                let username = self
                    .usernames
                    .get(&msg.client_id)
                    .cloned()
                    .unwrap_or_else(|| format!("User{}", msg.client_id));
                self.message_queue.push(NetworkMessage::TextMessage {
                    username,
                    text: msg.text,
                });
            }
            ServerMessage::ChatImage(img) => {
                let username = self
                    .usernames
                    .get(&img.client_id)
                    .cloned()
                    .unwrap_or_else(|| format!("User{}", img.client_id));
                self.message_queue.push(NetworkMessage::ImageMessage {
                    username,
                    data: img.data,
                });
            }
            ServerMessage::Speaking {
                client_id,
                is_speaking,
            } => {
                self.message_queue.push(NetworkMessage::UserSpeaking {
                    client_id,
                    is_speaking,
                });
            }
            ServerMessage::AudioState {
                client_id,
                muted,
                deafened,
            } => {
                self.message_queue.push(NetworkMessage::UserAudioState {
                    client_id,
                    muted,
                    deafened,
                });
            }
            ServerMessage::ScreenShareStarted {
                client_id,
                width,
                height,
            } => {
                self.message_queue.push(NetworkMessage::ScreenShareStarted {
                    client_id,
                    width: u32::from(width),
                    height: u32::from(height),
                });
            }
            ServerMessage::ScreenShareStopped { client_id } => {
                // Forget any partial frames from that stream.
                self.reassembler.forget_sender(client_id);
                self.message_queue
                    .push(NetworkMessage::ScreenShareStopped { client_id });
            }
            ServerMessage::ShareDenied { reason } => {
                warn!("Server denied our screen share: {}", reason.describe());
                self.message_queue
                    .push(NetworkMessage::ShareDenied { reason });
            }
            ServerMessage::SubscribeDenied { target_id, reason } => {
                warn!(
                    "Server denied subscribe to {target_id}: {}",
                    reason.describe()
                );
                self.message_queue
                    .push(NetworkMessage::SubscribeDenied { target_id, reason });
            }
            ServerMessage::KeyframeRequested { viewer_id } => {
                self.message_queue
                    .push(NetworkMessage::KeyframeRequested { viewer_id });
            }
            ServerMessage::StreamHealth {
                worst_loss,
                worst_rtt_ms,
                viewers,
            } => {
                self.message_queue.push(NetworkMessage::StreamHealth {
                    worst_loss,
                    worst_rtt_ms,
                    viewers,
                });
            } // No catch-all: the match is exhaustive on purpose, so a
              // new ServerMessage variant is a compile error here (a
              // reminder to handle it) instead of a silent log line.
              // Truly unknown wire data never reaches this match — it
              // fails deserialization above and is skipped.
        }
    }

    // ── Voice channel ───────────────────────────────────────────────

    fn process_voice_messages(&mut self) {
        while let Some(message) = self.client.receive_message(VOICE_CHANNEL) {
            // [sender_id][voice packet] — sender stamped by the server.
            let Some((sender_id, packet)) = split_sender(&message) else {
                continue;
            };
            let Some((header, frame)) = parse_voice(packet) else {
                continue;
            };
            self.message_queue.push(NetworkMessage::VoiceData {
                client_id: sender_id,
                seq: header.seq,
                timestamp: header.timestamp,
                data: frame.to_vec(),
            });
        }
    }

    // ── Video channel ───────────────────────────────────────────────

    fn process_video_messages(&mut self) {
        while let Some(message) = self.client.receive_message(VIDEO_CHANNEL) {
            let Some((sender_id, chunk)) = split_sender(&message) else {
                continue;
            };
            if let Some(frame) = self.reassembler.push_chunk(sender_id, chunk) {
                self.message_queue.push(NetworkMessage::VideoFrame {
                    client_id: frame.sender_id,
                    seq: frame.seq,
                    is_keyframe: frame.is_keyframe,
                    width: u32::from(frame.width),
                    height: u32::from(frame.height),
                    data: frame.data,
                });
            }
        }
    }

    // ── System audio channel ────────────────────────────────────────

    fn process_system_audio_messages(&mut self) {
        while let Some(message) = self.client.receive_message(SYSTEM_AUDIO_CHANNEL) {
            let Some((sender_id, packet)) = split_sender(&message) else {
                continue;
            };
            let Some((header, payload)) = parse_system_audio(packet) else {
                continue;
            };
            self.message_queue.push(NetworkMessage::SystemAudioData {
                client_id: sender_id,
                seq: header.seq,
                data: payload.to_vec(),
            });
        }
    }

    // ── Outbound messages ───────────────────────────────────────────

    /// Send a control-plane message on its designated channel.
    /// Report outbound image progress from the media channel's
    /// buffered-bytes drain (see `upload_total`). Emits only on change,
    /// so this is a handful of messages per upload, not per tick.
    fn sample_upload_progress(&mut self) {
        let Some(total) = self.upload_total else {
            return;
        };
        let available = self
            .client
            .channel_available_memory(protocol::MEDIA_CHANNEL);
        let used = protocol::channels::MEDIA_BUFFER_BYTES.saturating_sub(available);
        let pct = if used == 0 {
            100
        } else {
            // `used` can momentarily exceed `total` (envelope bytes);
            // clamp so progress never goes negative.
            #[allow(clippy::cast_possible_truncation)]
            {
                (total.saturating_sub(used.min(total)) * 100 / total.max(1)) as u8
            }
        };
        if pct != self.upload_last_pct {
            self.upload_last_pct = pct;
            self.message_queue
                .push(NetworkMessage::UploadProgress { pct });
        }
        if pct == 100 {
            self.upload_total = None;
        }
    }

    fn send_control(&mut self, msg: &ClientMessage) {
        match protocol::encode_client_message(msg) {
            Some(encoded) => self.client.send_message(msg.channel(), encoded),
            None => warn!("Failed to serialize client message"),
        }
    }

    pub fn send_message(&mut self, text: String) {
        self.send_control(&ClientMessage::Chat(text));
    }

    /// Send a chat image. `send_control` refuses (with a warning) if the
    /// payload somehow exceeds the protocol budget.
    pub fn send_image(&mut self, data: Vec<u8>) {
        self.upload_total = Some(data.len());
        self.upload_last_pct = 255;
        self.send_control(&ClientMessage::ChatImage(data));
    }

    pub fn send_speaking_status(&mut self, is_speaking: bool) {
        // Edge-triggered by the voice thread; dedupe defensively in case
        // the same state is ever delivered twice.
        if is_speaking == self.last_speaking_sent {
            return;
        }
        self.send_control(&ClientMessage::Speaking(is_speaking));
        self.last_speaking_sent = is_speaking;
    }

    /// Broadcast our mute/deafen state to all other users via the server.
    pub fn send_audio_state(&mut self, muted: bool, deafened: bool) {
        self.send_control(&ClientMessage::AudioState { muted, deafened });
    }

    /// Send one Opus voice frame. One frame per packet — no bundling,
    /// no added latency (see `protocol::voice`).
    pub fn send_voice_frame(&mut self, seq: u32, ts_48k: u32, opus_frame: &[u8]) {
        match pack_voice(seq, ts_48k, opus_frame) {
            Some(packet) => self.client.send_message(VOICE_CHANNEL, packet),
            None => log::debug!(
                "Dropping voice frame seq={seq}: invalid size {} bytes",
                opus_frame.len()
            ),
        }
    }

    /// Chunk and send an encoded H.264 frame on the video channel.
    pub fn send_video_frame(
        &mut self,
        seq: u32,
        is_keyframe: bool,
        width: u16,
        height: u16,
        data: &[u8],
    ) {
        // `pack_video_frame` refuses empty or oversize frames (past the
        // chunk format's 255 × 1080 B limit). The encoder's VBV keeps
        // frames well under it, but a misbehaving codec must not kill
        // the stream — drop the frame; the viewer sees a seq gap and
        // recovers via keyframe request.
        match pack_video_frame(seq, is_keyframe, width, height, data) {
            Some(chunks) => {
                for chunk in chunks {
                    self.client.send_message(VIDEO_CHANNEL, chunk);
                }
            }
            None => warn!(
                "📺 Encoded frame seq={seq} not packable ({} bytes, max {}), dropping",
                data.len(),
                protocol::MAX_VIDEO_FRAME_BYTES,
            ),
        }
    }

    /// Send an encoded system audio (Opus stereo) packet.
    pub fn send_system_audio(&mut self, seq: u32, channels: u8, duration_ms: u16, opus: &[u8]) {
        match pack_system_audio(seq, channels, duration_ms, opus) {
            Some(packet) => self.client.send_message(SYSTEM_AUDIO_CHANNEL, packet),
            None => warn!(
                "System audio packet not packable ({} bytes), dropping",
                opus.len()
            ),
        }
    }

    /// Send screen share signaling. Dimensions are clamped into the
    /// wire format's u16 range (the server validates 16..=4096 anyway).
    pub fn send_screen_share_start(&mut self, width: u32, height: u32) {
        let width = u16::try_from(width.min(4096)).unwrap_or(u16::MAX);
        let height = u16::try_from(height.min(4096)).unwrap_or(u16::MAX);
        self.send_control(&ClientMessage::ScreenShareStart { width, height });
    }

    pub fn send_screen_share_stop(&mut self) {
        self.send_control(&ClientMessage::ScreenShareStop);
    }

    pub fn send_subscribe_screen(&mut self, target_id: u64) {
        self.send_control(&ClientMessage::SubscribeScreen { target_id });
    }

    pub fn send_unsubscribe_screen(&mut self, target_id: u64) {
        self.send_control(&ClientMessage::UnsubscribeScreen { target_id });
    }

    /// Ask the server to relay a keyframe request to the user we're
    /// watching. Travels on the signal channel: reliable, but never
    /// queued behind chat or presence traffic.
    pub fn send_request_keyframe(&mut self, target_id: u64) {
        self.send_control(&ClientMessage::RequestKeyframe { target_id });
    }

    pub fn poll_messages(&mut self) -> Vec<NetworkMessage> {
        std::mem::take(&mut self.message_queue)
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    pub fn is_connecting(&self) -> bool {
        self.client.is_connecting()
    }

    pub fn disconnect_reason(&self) -> Option<renet::DisconnectReason> {
        self.client.disconnect_reason()
    }

    pub fn disconnect(&mut self) {
        self.client.disconnect();
        // Flush the disconnect packet over the wire. Without this,
        // the server won't know we left until its keepalive times out.
        //
        // Both client.update() AND transport.update() are needed:
        // the disconnect packet is generated by the netcode transport
        // layer, not the renet client layer.
        let duration = Instant::now() - self.last_updated;
        self.last_updated = Instant::now();
        self.client.update(duration);
        let _ = self.transport.update(duration, &mut self.client);
        let _ = self.transport.send_packets(&mut self.client);
    }
}

// ════════════════════════════════════════════════════════════════════════
//  Network control messages (UI thread → network thread)
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum NetworkControl {
    Connect {
        addr: SocketAddr,
        nickname: String,
        /// Token issuer URL for secure servers (`None` = open connect).
        token_url: Option<String>,
        /// Optional shared password for the issuer.
        token_password: Option<String>,
    },
    Disconnect,
    SendMessage(String),
    /// Send a chat image (encoded PNG; the UI caps it at
    /// `protocol::MAX_CHAT_IMAGE_BYTES` before it gets here).
    SendImage(Vec<u8>),
    SendVoicePacket(AudioPacket),
    SendSpeakingStatus(bool),
    /// Broadcast mute/deafen state to other users.
    SendAudioState {
        muted: bool,
        deafened: bool,
    },

    // ── Screen share ────────────────────────────────────────────────
    /// Signal that we started sharing (control → server → all clients).
    ScreenShareStart {
        width: u32,
        height: u32,
    },
    /// Signal that we stopped sharing.
    ScreenShareStop,
    /// Send an encoded H.264 frame on the video channel.
    SendVideoFrame {
        seq: u32,
        is_keyframe: bool,
        width: u16,
        height: u16,
        data: Vec<u8>,
    },
    /// Send an Opus stereo system audio packet on the system audio channel.
    SendSystemAudio {
        seq: u32,
        channels: u8,
        duration_ms: u16,
        data: Vec<u8>,
    },
    /// Request a fresh keyframe from the user we're watching
    /// (viewer → server → sharer).
    RequestKeyframe {
        target_id: u64,
    },
    /// Subscribe to a remote user's screen share stream.
    SubscribeScreen {
        target_id: u64,
    },
    /// Unsubscribe from a remote user's screen share stream.
    UnsubscribeScreen {
        target_id: u64,
    },

    Shutdown,
}

// ════════════════════════════════════════════════════════════════════════
//  Network thread handle
// ════════════════════════════════════════════════════════════════════════

pub struct NetworkThreadHandle {
    pub tx: Sender<NetworkControl>,
    pub msg_rx: Receiver<NetworkMessage>,
    /// Joined by the frontend on shutdown (with a timeout).
    pub join_handle: Option<thread::JoinHandle<()>>,
}

impl NetworkThreadHandle {
    /// Spawn the network thread with pre-created channels.
    ///
    /// The caller creates channels externally so that `network_tx` can be
    /// shared with the voice thread before the network thread starts
    /// (resolving the circular dependency).
    pub fn spawn(
        tx: Sender<NetworkControl>,
        control_rx: Receiver<NetworkControl>,
        msg_tx: Sender<NetworkMessage>,
        msg_rx: Receiver<NetworkMessage>,
        voice_tx: Sender<VoiceControl>,
        screen_tx: Sender<ScreenControl>,
        wake: WakeFn,
    ) -> Self {
        let join_handle = thread::spawn(move || {
            run_network_loop(control_rx, msg_tx, voice_tx, screen_tx, wake);
        });

        Self {
            tx,
            msg_rx,
            join_handle: Some(join_handle),
        }
    }

    /// Send shutdown and wait for the thread to exit.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(NetworkControl::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NetworkThreadHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Thread loop ─────────────────────────────────────────────────────────

/// Handle one control message. Returns `false` when the loop should stop.
fn handle_control(
    ctrl: NetworkControl,
    client: &mut Option<NetworkClient>,
    msg_tx: &Sender<NetworkMessage>,
    wake: &WakeFn,
) -> bool {
    match ctrl {
        NetworkControl::Connect {
            addr,
            nickname,
            token_url,
            token_password,
        } => {
            // Replace any existing session cleanly first.
            if let Some(mut old) = client.take() {
                old.disconnect();
            }
            // Blocking: includes the token fetch in secure mode. That's
            // fine — this is the network thread, the UI is elsewhere,
            // and there is no session to service yet.
            match NetworkClient::connect(
                addr,
                &nickname,
                token_url.as_deref(),
                token_password.as_deref(),
            ) {
                // Connected is announced by NetworkClient::update()
                // once the handshake actually completes.
                Ok(c) => *client = Some(c),
                Err(e) => {
                    msg_tx.send(NetworkMessage::Disconnected(e)).ok();
                    wake();
                }
            }
        }
        NetworkControl::Disconnect => {
            if let Some(mut c) = client.take() {
                c.disconnect();
            }
            msg_tx
                .send(NetworkMessage::Disconnected("User disconnected".into()))
                .ok();
            wake();
        }
        NetworkControl::SendMessage(text) => {
            if let Some(c) = client {
                c.send_message(text);
            }
        }
        NetworkControl::SendImage(data) => {
            if let Some(c) = client {
                c.send_image(data);
            }
        }
        NetworkControl::SendVoicePacket(packet) => {
            if let Some(c) = client {
                // The packet's ssrc is not sent — the server stamps the
                // authenticated sender id on relay.
                c.send_voice_frame(
                    packet.header.seq,
                    packet.header.timestamp,
                    &packet.opus_data,
                );
            }
        }
        NetworkControl::SendSpeakingStatus(speaking) => {
            if let Some(c) = client {
                c.send_speaking_status(speaking);
            }
        }
        NetworkControl::SendAudioState { muted, deafened } => {
            if let Some(c) = client {
                c.send_audio_state(muted, deafened);
            }
        }

        // ── Screen share controls ───────────────────────────────────
        NetworkControl::ScreenShareStart { width, height } => {
            if let Some(c) = client {
                c.send_screen_share_start(width, height);
            }
        }
        NetworkControl::ScreenShareStop => {
            if let Some(c) = client {
                c.send_screen_share_stop();
            }
        }
        NetworkControl::SendVideoFrame {
            seq,
            is_keyframe,
            width,
            height,
            data,
        } => {
            if let Some(c) = client {
                c.send_video_frame(seq, is_keyframe, width, height, &data);
            }
        }
        NetworkControl::SendSystemAudio {
            seq,
            channels,
            duration_ms,
            data,
        } => {
            if let Some(c) = client {
                c.send_system_audio(seq, channels, duration_ms, &data);
            }
        }
        NetworkControl::RequestKeyframe { target_id } => {
            if let Some(c) = client {
                c.send_request_keyframe(target_id);
            }
        }
        NetworkControl::SubscribeScreen { target_id } => {
            if let Some(c) = client {
                c.send_subscribe_screen(target_id);
            }
        }
        NetworkControl::UnsubscribeScreen { target_id } => {
            if let Some(c) = client {
                c.send_unsubscribe_screen(target_id);
            }
        }

        NetworkControl::Shutdown => {
            if let Some(mut c) = client.take() {
                c.disconnect();
            }
            return false;
        }
    }
    true
}

fn run_network_loop(
    control_rx: Receiver<NetworkControl>,
    msg_tx: Sender<NetworkMessage>,
    voice_tx: Sender<VoiceControl>,
    screen_tx: Sender<ScreenControl>,
    wake: WakeFn,
) {
    let mut client: Option<NetworkClient> = None;
    let mut running = true;

    while running {
        // Block on the control channel instead of the old try_recv +
        // fixed 1 ms sleep. Outbound work (voice frames from the voice
        // thread, video chunks from the screen thread) wakes this loop
        // *immediately*. Connected, the 1 ms timeout doubles as the
        // inbound polling cadence (renet needs regular update() calls
        // to service the UDP socket). Disconnected there is nothing to
        // poll, so block outright — zero wakeups until Connect or
        // Shutdown arrives.
        let recv_result = if client.is_some() {
            control_rx.recv_timeout(Duration::from_millis(1))
        } else {
            control_rx
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected)
        };

        match recv_result {
            Ok(ctrl) => {
                if !handle_control(ctrl, &mut client, &msg_tx, &wake) {
                    running = false;
                }
                // Drain a bounded burst of whatever else queued up.
                for _ in 0..63 {
                    match control_rx.try_recv() {
                        Ok(ctrl) => {
                            if !handle_control(ctrl, &mut client, &msg_tx, &wake) {
                                running = false;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            running = false;
                            break;
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Poll network
        if let Some(c) = &mut client {
            c.update();

            for msg in c.poll_messages() {
                match msg {
                    // Voice → voice thread (hot path, bypass app)
                    NetworkMessage::VoiceData {
                        client_id,
                        seq,
                        timestamp,
                        data,
                    } => {
                        let packet = AudioPacket::new(seq, timestamp, client_id, data);
                        let _ = voice_tx.try_send(VoiceControl::IncomingVoicePacket(packet));
                    }

                    // Video → screen thread (hot path, bypass app)
                    NetworkMessage::VideoFrame {
                        client_id,
                        seq,
                        is_keyframe,
                        width,
                        height,
                        data,
                    } => {
                        if let Err(e) = screen_tx.try_send(ScreenControl::IncomingVideoFrame {
                            client_id,
                            seq,
                            is_keyframe,
                            width,
                            height,
                            data,
                        }) {
                            log::warn!(
                                "📺 Screen channel full, dropped frame seq={seq} keyframe={is_keyframe} - {e}"
                            );
                        }
                    }

                    // System audio → screen thread (hot path, bypass app)
                    NetworkMessage::SystemAudioData {
                        client_id,
                        seq,
                        data,
                    } => {
                        let _ = screen_tx.try_send(ScreenControl::IncomingSystemAudio {
                            client_id,
                            seq,
                            data,
                        });
                    }

                    // Screen share signaling → screen thread + app
                    NetworkMessage::ScreenShareStarted {
                        client_id,
                        width,
                        height,
                    } => {
                        let _ = screen_tx.try_send(ScreenControl::PeerStartedSharing {
                            client_id,
                            width,
                            height,
                        });
                        let _ = msg_tx.send(NetworkMessage::ScreenShareStarted {
                            client_id,
                            width,
                            height,
                        });
                        wake();
                    }
                    NetworkMessage::ScreenShareStopped { client_id } => {
                        let _ = screen_tx.try_send(ScreenControl::PeerStoppedSharing { client_id });
                        let _ = msg_tx.send(NetworkMessage::ScreenShareStopped { client_id });
                        wake();
                    }

                    // Keyframe request from a viewer → screen thread
                    // (hot path, bypass app)
                    NetworkMessage::KeyframeRequested { viewer_id } => {
                        log::info!("📺 Keyframe requested by viewer {viewer_id}");
                        let _ = screen_tx.try_send(ScreenControl::ForceKeyframe);
                    }

                    // Viewer health for our stream → screen thread
                    // (drives fps adaptation while sharing)
                    NetworkMessage::StreamHealth {
                        worst_loss,
                        worst_rtt_ms,
                        viewers,
                    } => {
                        let _ = screen_tx.try_send(ScreenControl::StreamHealth {
                            worst_loss,
                            worst_rtt_ms,
                            viewers,
                        });
                    }

                    // Everything else → app. These carry state (join /
                    // leave, init, chat, mute states, denials) — losing
                    // one desyncs the UI until reconnect, so use a
                    // blocking send. The app drains this channel every
                    // event-loop pass.
                    other => {
                        let _ = msg_tx.send(other);
                        wake();
                    }
                }
            }

            if !c.is_connected() && !c.is_connecting() {
                log::error!(
                    "Connection lost — is_connected=false, is_connecting=false, disconnect_reason={:?}",
                    c.disconnect_reason()
                );
                client = None;
                msg_tx
                    .send(NetworkMessage::Disconnected("Connection lost".into()))
                    .ok();
                wake();
            }
        }
    }
}
