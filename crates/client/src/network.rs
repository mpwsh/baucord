// network.rs — Renet client + network thread.
//
// NetworkClient handles protocol, serialization, voice bundling.
// NetworkThreadHandle owns the thread that polls the client and routes
// voice packets to the voice thread and video packets to the screen thread.
//
// All wire protocol types come from the shared `protocol` crate.

use bincode::Options;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use log::warn;
use renet::{ChannelConfig, ClientId, ConnectionConfig, RenetClient, SendType};
use renet_netcode::{
    ClientAuthentication, NETCODE_USER_DATA_BYTES, NetcodeClientTransport, NetcodeTransportError,
};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use audio::AudioPacket;

use protocol::{
    ClientMessage, PROTOCOL_ID, RELIABLE_BUFFER_BYTES, RELIABLE_CHANNEL, SYSTEM_AUDIO_BUFFER_BYTES,
    SYSTEM_AUDIO_CHANNEL, SYSTEM_AUDIO_MTU_BUDGET, ServerMessage, VIDEO_BUFFER_BYTES,
    VIDEO_CHANNEL, VOICE_BATCH_THRESHOLD, VOICE_BUFFER_BYTES, VOICE_CHANNEL, VOICE_MAX_FRAME_SIZE,
    VOICE_MAX_FRAMES_PER_PACKET, VOICE_MTU_BUDGET, VOICE_PROTOCOL_VERSION, VideoReassembler,
    VoiceHeader, pack_system_audio, pack_video_frame, parse_system_audio_header,
    system_audio_payload,
};

use crate::screen::ScreenControl;
use crate::voice::{VoiceControl, WakeFn};

// ── Messages sent to the UI / App layer ─────────────────────────────────

#[derive(Debug, Clone)]
pub enum NetworkMessage {
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
    LatencyUpdate {
        rtt_ms: f64,
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
}

// ── Voice packet bundler ────────────────────────────────────────────────

struct VoiceBundler {
    buf: Vec<u8>,
    frames: u8,
    base_seq: u32,
    base_ts: u32,
}

impl VoiceBundler {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(VOICE_MTU_BUDGET),
            frames: 0,
            base_seq: 0,
            base_ts: 0,
        }
    }

    fn begin(&mut self, client_id: u64, seq: u32, ts_48k: u32) {
        self.buf.clear();
        self.frames = 0;
        self.base_seq = seq;
        self.base_ts = ts_48k;

        self.buf.push(VOICE_PROTOCOL_VERSION);
        self.buf.extend_from_slice(&[0, 0, 0]); // reserved
        self.buf.extend_from_slice(&client_id.to_le_bytes());
        self.buf.extend_from_slice(&seq.to_le_bytes());
        self.buf.extend_from_slice(&ts_48k.to_le_bytes());
        self.buf.push(0); // placeholder for frame_count
        self.buf.extend_from_slice(&[0, 0, 0]); // padding
    }

    fn try_push_frame(&mut self, opus_frame: &[u8]) -> bool {
        if self.frames as usize >= VOICE_MAX_FRAMES_PER_PACKET {
            return false;
        }
        if opus_frame.len() > VOICE_MAX_FRAME_SIZE {
            return false;
        }

        let need = 2 + opus_frame.len();
        if self.buf.len() + need > VOICE_MTU_BUDGET {
            return false;
        }

        self.buf
            .extend_from_slice(&(opus_frame.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(opus_frame);
        self.frames += 1;
        true
    }

    fn is_empty(&self) -> bool {
        self.frames == 0
    }

    fn finish_into_vec(&mut self) -> Option<Vec<u8>> {
        if self.frames == 0 {
            return None;
        }
        self.buf[20] = self.frames; // frame_count at byte offset 20
        self.frames = 0;
        // Hand off the buffer and leave a fresh one behind. This also
        // fixes a real bug: the old version cloned the buffer WITHOUT
        // resetting `frames`, so every network-loop tick re-sent the
        // current bundle and each new frame was appended to an
        // already-sent packet — duplicate, cumulatively growing voice
        // packets on the wire.
        Some(std::mem::replace(
            &mut self.buf,
            Vec::with_capacity(VOICE_MTU_BUDGET),
        ))
    }
}

// ── Serialization helper ────────────────────────────────────────────────

/// Serialize a `ClientMessage` via bincode, logging on failure.
fn encode_client_message(msg: &ClientMessage) -> Option<Vec<u8>> {
    bincode::options()
        .serialize(msg)
        .map_err(|e| {
            warn!("Failed to serialize client message: {e}");
        })
        .ok()
}

// ── Username encoding for netcode ───────────────────────────────────────

fn encode_username(name: &str) -> [u8; NETCODE_USER_DATA_BYTES] {
    let mut user_data = [0u8; NETCODE_USER_DATA_BYTES];
    // Truncate on a char boundary so a multi-byte character is never split.
    let mut len = name.len().min(NETCODE_USER_DATA_BYTES - 8);
    while len > 0 && !name.is_char_boundary(len) {
        len -= 1;
    }
    user_data[0] = len as u8;
    user_data[1..=len].copy_from_slice(&name.as_bytes()[..len]);
    user_data
}

// ════════════════════════════════════════════════════════════════════════
//  NetworkClient — protocol + state
// ════════════════════════════════════════════════════════════════════════

pub(crate) struct NetworkClient {
    client: RenetClient,
    transport: NetcodeClientTransport,
    usernames: HashMap<ClientId, String>,
    client_id: u64,
    message_queue: Vec<NetworkMessage>,
    last_updated: Instant,
    voice_bundler: VoiceBundler,
    reassembler: VideoReassembler,

    // Reorder tracking for received voice packets
    expected_seqs: HashMap<u64, u32>,

    // Speaking status dedupe (only send on change)
    last_speaking_sent: bool,
    last_rtt_check: Instant,
    /// Whether we've announced `NetworkMessage::Connected` to the app.
    /// Set once the netcode handshake actually completes — previously
    /// Connected fired (twice) as soon as the socket was created.
    announced_connected: bool,
}

impl NetworkClient {
    pub fn connect(server_addr: SocketAddr, username: String) -> Result<Self, String> {
        let reliable = ChannelConfig {
            channel_id: RELIABLE_CHANNEL,
            max_memory_usage_bytes: RELIABLE_BUFFER_BYTES,
            send_type: SendType::ReliableOrdered {
                resend_time: Duration::from_millis(200),
            },
        };

        let voice = ChannelConfig {
            channel_id: VOICE_CHANNEL,
            max_memory_usage_bytes: VOICE_BUFFER_BYTES,
            send_type: SendType::Unreliable,
        };

        let video = ChannelConfig {
            channel_id: VIDEO_CHANNEL,
            max_memory_usage_bytes: VIDEO_BUFFER_BYTES,
            send_type: SendType::Unreliable,
        };

        let system_audio = ChannelConfig {
            channel_id: SYSTEM_AUDIO_CHANNEL,
            max_memory_usage_bytes: SYSTEM_AUDIO_BUFFER_BYTES,
            send_type: SendType::Unreliable,
        };

        let channels = vec![reliable, voice, video, system_audio];

        let connection_config = ConnectionConfig {
            available_bytes_per_tick: 300_000,
            server_channels_config: channels.clone(),
            client_channels_config: channels,
        };

        let client = RenetClient::new(connection_config);

        let socket =
            UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Failed to bind socket: {e}"))?;
        let sock_ref = socket2::SockRef::from(&socket);
        sock_ref
            .set_send_buffer_size(protocol::UDP_SOCKET_BUFFER_BYTES)
            .ok();
        sock_ref
            .set_recv_buffer_size(protocol::UDP_SOCKET_BUFFER_BYTES)
            .ok();

        let current_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| format!("System time error: {e}"))?;

        let client_id = current_time.as_millis() as u64;

        let authentication = ClientAuthentication::Unsecure {
            server_addr,
            client_id,
            user_data: Some(encode_username(&username)),
            protocol_id: PROTOCOL_ID,
        };

        let transport = NetcodeClientTransport::new(current_time, authentication, socket)
            .map_err(|e| format!("Failed to create transport: {e}"))?;

        Ok(NetworkClient {
            client,
            transport,
            usernames: HashMap::new(),
            client_id,
            message_queue: Vec::new(),
            last_updated: Instant::now(),
            voice_bundler: VoiceBundler::new(),
            expected_seqs: HashMap::new(),
            last_speaking_sent: false,
            last_rtt_check: Instant::now(),
            announced_connected: false,
            reassembler: VideoReassembler::new(),
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

        // Announce Connected exactly once, when the handshake completes.
        if !self.announced_connected && self.client.is_connected() {
            self.announced_connected = true;
            self.message_queue.push(NetworkMessage::Connected);
        }

        self.process_reliable_messages();
        self.process_voice_messages();
        self.process_video_messages();
        self.process_system_audio_messages();

        if let Err(e) = self.transport.send_packets(&mut self.client) {
            let is_would_block = matches!(
                &e,
                NetcodeTransportError::IO(io_err)
                    if io_err.kind() == std::io::ErrorKind::WouldBlock
            ) || e.to_string().contains("10035")
                || e.to_string().contains("WouldBlock");
            if is_would_block {
                log::debug!("send_packets: WouldBlock (OS buffer full), will retry next tick");
            } else {
                self.message_queue
                    .push(NetworkMessage::Disconnected(format!(
                        "Failed to send packets: {e}"
                    )));
            }
        }

        // Sample RTT every ~2 seconds
        if now.duration_since(self.last_rtt_check) >= Duration::from_secs(2) {
            self.last_rtt_check = now;
            if self.client.is_connected() {
                self.message_queue.push(NetworkMessage::LatencyUpdate {
                    rtt_ms: self.client.rtt() * 1000.0,
                });
            }
        }
    }

    // ── Reliable channel ────────────────────────────────────────────

    fn process_reliable_messages(&mut self) {
        while let Some(message) = self.client.receive_message(RELIABLE_CHANNEL) {
            let Ok(msg) = bincode::options().deserialize::<ServerMessage>(&message) else {
                continue;
            };

            match msg {
                ServerMessage::InitClient {
                    usernames,
                    client_id,
                    server_name,
                    greeting,
                } => {
                    self.usernames = usernames.clone();
                    self.client_id = client_id;
                    self.message_queue.push(NetworkMessage::InitClient {
                        client_id,
                        users: usernames,
                        server_name,
                        greeting,
                    });
                }
                ServerMessage::ClientConnected {
                    client_id,
                    username,
                } => {
                    self.usernames.insert(client_id, username.clone());
                    self.message_queue.push(NetworkMessage::UserJoined {
                        client_id,
                        username,
                    });
                }
                ServerMessage::ClientDisconnected { client_id } => {
                    let username = self
                        .usernames
                        .remove(&client_id)
                        .unwrap_or_else(|| format!("User{client_id}"));

                    self.message_queue.push(NetworkMessage::UserLeft {
                        client_id,
                        username,
                    });
                }
                ServerMessage::ClientMessage(msg) => {
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
                ServerMessage::VoiceData { .. } => {
                    // Voice data shouldn't arrive on the reliable channel
                }
                ServerMessage::ScreenShareStarted {
                    client_id,
                    width,
                    height,
                } => {
                    self.message_queue.push(NetworkMessage::ScreenShareStarted {
                        client_id,
                        width,
                        height,
                    });
                }
                ServerMessage::ScreenShareStopped { client_id } => {
                    self.message_queue
                        .push(NetworkMessage::ScreenShareStopped { client_id });
                }
                ServerMessage::KeyframeRequested { viewer_id } => {
                    self.message_queue
                        .push(NetworkMessage::KeyframeRequested { viewer_id });
                }
                // No catch-all: the match is exhaustive on purpose, so a
                // new ServerMessage variant is a compile error here (a
                // reminder to handle it) instead of a silent log line.
                // Truly unknown wire data never reaches this match — it
                // fails bincode deserialization above and is skipped.
            }
        }
    }

    // ── Voice channel ───────────────────────────────────────────────

    fn process_voice_messages(&mut self) {
        while let Some(message) = self.client.receive_message(VOICE_CHANNEL) {
            self.process_voice_packet(message.to_vec());
        }
    }

    fn process_voice_packet(&mut self, message: Vec<u8>) -> bool {
        const OPUS_10MS_48K: u32 = 480;

        // Structural validation
        if message.len() < VoiceHeader::SIZE + 3 {
            return false;
        }
        if message[0] != VOICE_PROTOCOL_VERSION {
            return false;
        }

        let mut off = 4; // skip version + reserved

        if off + 8 + 4 + 4 + 1 + 3 > message.len() {
            return false;
        }

        let client_id = u64::from_le_bytes(message[off..off + 8].try_into().unwrap());
        off += 8;
        let base_seq = u32::from_le_bytes(message[off..off + 4].try_into().unwrap());
        off += 4;
        let base_ts = u32::from_le_bytes(message[off..off + 4].try_into().unwrap());
        off += 4;
        let frame_count = message[off] as usize;
        off += 4; // frame_count + padding

        if frame_count == 0 || frame_count > VOICE_MAX_FRAMES_PER_PACKET {
            return false;
        }

        // Track sequence for reorder detection
        let expected = self
            .expected_seqs
            .get(&client_id)
            .copied()
            .unwrap_or(base_seq);
        let seq_diff = base_seq.wrapping_sub(expected) as i32;

        // Unbundle frames
        let mut seq_frame = base_seq;
        let mut ts_frame = base_ts;

        for _ in 0..frame_count {
            if off + 2 > message.len() {
                return false;
            }
            let len = u16::from_le_bytes(message[off..off + 2].try_into().unwrap()) as usize;
            off += 2;

            if len == 0 || len > VOICE_MAX_FRAME_SIZE || off + len > message.len() {
                return false;
            }

            let data = message[off..off + len].to_vec();
            off += len;

            self.message_queue.push(NetworkMessage::VoiceData {
                client_id,
                seq: seq_frame,
                timestamp: ts_frame,
                data,
            });

            seq_frame = seq_frame.wrapping_add(1);
            ts_frame = ts_frame.wrapping_add(OPUS_10MS_48K);
        }

        // Advance expected seq only if packet is new or ahead
        if seq_diff >= 0 {
            self.expected_seqs
                .insert(client_id, base_seq.wrapping_add(frame_count as u32));
        }

        true
    }

    // ── Video channels ──────────────────────────────────────────────

    fn process_video_messages(&mut self) {
        // All video now arrives as chunks on the unreliable channel
        while let Some(message) = self.client.receive_message(VIDEO_CHANNEL) {
            if let Some(frame) = self.reassembler.push_chunk(&message) {
                self.message_queue.push(NetworkMessage::VideoFrame {
                    client_id: frame.client_id,
                    seq: frame.seq,
                    is_keyframe: frame.is_keyframe,
                    width: frame.width as u32,
                    height: frame.height as u32,
                    data: frame.data,
                });
            }
        }
    }

    // ── System audio channel ────────────────────────────────────────

    fn process_system_audio_messages(&mut self) {
        while let Some(message) = self.client.receive_message(SYSTEM_AUDIO_CHANNEL) {
            self.process_system_audio_packet(&message);
        }
    }

    fn process_system_audio_packet(&mut self, message: &[u8]) {
        let Some(hdr) = parse_system_audio_header(message) else {
            return;
        };
        let Some(payload) = system_audio_payload(message) else {
            return;
        };

        self.message_queue.push(NetworkMessage::SystemAudioData {
            client_id: hdr.client_id,
            seq: hdr.seq,
            data: payload.to_vec(),
        });
    }

    // ── Outbound messages ───────────────────────────────────────────

    pub fn flush_voice_packet(&mut self) {
        if let Some(pkt) = self.voice_bundler.finish_into_vec()
            && pkt.len() <= VOICE_MTU_BUDGET
        {
            self.client.send_message(VOICE_CHANNEL, pkt);
        }
    }

    pub fn send_message(&mut self, text: String) {
        if let Some(encoded) = encode_client_message(&ClientMessage::Text(text)) {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    pub fn send_speaking_status(&mut self, is_speaking: bool) {
        // Edge-triggered by the voice thread; dedupe defensively in case
        // the same state is ever delivered twice. (The old 50 ms throttle
        // was dead code — this is only ever called on state changes.)
        if is_speaking == self.last_speaking_sent {
            return;
        }
        if let Some(encoded) = encode_client_message(&ClientMessage::Speaking(is_speaking)) {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
        self.last_speaking_sent = is_speaking;
    }

    /// Broadcast our mute/deafen state to all other users via the server.
    pub fn send_audio_state(&mut self, muted: bool, deafened: bool) {
        if let Some(encoded) = encode_client_message(&ClientMessage::AudioState { muted, deafened })
        {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    /// Send one 10 ms Opus frame through the voice bundler.
    ///
    /// Uses the caller's sequence number and timestamp to maintain
    /// one authoritative sequence chain from encoder to decoder.
    pub fn send_voice_frame(
        &mut self,
        our_client_id: ClientId,
        seq: u32,
        ts_48k: u32,
        opus_frame: &[u8],
    ) {
        if self.voice_bundler.is_empty() {
            self.voice_bundler.begin(our_client_id, seq, ts_48k);
        }

        if !self.voice_bundler.try_push_frame(opus_frame) {
            self.flush_voice_packet();
            self.voice_bundler.begin(our_client_id, seq, ts_48k);
            if !self.voice_bundler.try_push_frame(opus_frame) {
                return; // frame too large, drop
            }
        }

        if self.voice_bundler.frames >= VOICE_BATCH_THRESHOLD {
            self.flush_voice_packet();
        }
    }

    /// Send an encoded system audio (Opus stereo) packet.
    pub fn send_system_audio(
        &mut self,
        our_client_id: u64,
        seq: u32,
        channels: u8,
        duration_ms: u16,
        opus_data: &[u8],
    ) {
        let buf = pack_system_audio(our_client_id, seq, channels, duration_ms, opus_data);
        if buf.len() > SYSTEM_AUDIO_MTU_BUDGET {
            warn!(
                "System audio packet too large: {} bytes, dropping",
                buf.len()
            );
            return;
        }
        self.client.send_message(SYSTEM_AUDIO_CHANNEL, buf);
    }

    /// Send screen share signaling on the reliable channel.
    pub fn send_screen_share_start(&mut self, width: u32, height: u32) {
        if let Some(encoded) =
            encode_client_message(&ClientMessage::ScreenShareStart { width, height })
        {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    pub fn send_screen_share_stop(&mut self) {
        if let Some(encoded) = encode_client_message(&ClientMessage::ScreenShareStop) {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    pub fn send_subscribe_screen(&mut self, target_id: u64) {
        if let Some(encoded) = encode_client_message(&ClientMessage::SubscribeScreen { target_id })
        {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    pub fn send_unsubscribe_screen(&mut self, target_id: u64) {
        if let Some(encoded) =
            encode_client_message(&ClientMessage::UnsubscribeScreen { target_id })
        {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    /// Ask the server to relay a keyframe request to the user we're
    /// watching. Sent when our jitter buffer hit an unrecoverable gap or
    /// we joined mid-GOP; the sharer answers with a forced IDR. Reliable
    /// channel — this message is rare (client-side rate-limited) and
    /// must not be lost.
    pub fn send_request_keyframe(&mut self, target_id: u64) {
        if let Some(encoded) = encode_client_message(&ClientMessage::RequestKeyframe { target_id })
        {
            self.client.send_message(RELIABLE_CHANNEL, encoded);
        }
    }

    pub fn poll_messages(&mut self) -> Vec<NetworkMessage> {
        self.message_queue.drain(..).collect()
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    pub fn is_connecting(&self) -> bool {
        self.client.is_connecting()
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
        self.client.update(duration);
        let _ = self.transport.update(duration, &mut self.client);
        let _ = self.transport.send_packets(&mut self.client);
    }

    pub fn client_id(&self) -> u64 {
        self.transport.client_id()
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
    },
    Disconnect,
    SendMessage(String),
    SendVoicePacket(AudioPacket),
    SendSpeakingStatus(bool),
    /// Broadcast mute/deafen state to other users.
    SendAudioState {
        muted: bool,
        deafened: bool,
    },

    // ── Screen share ────────────────────────────────────────────────
    /// Signal that we started sharing (reliable → server → all clients).
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
    pub(crate) join_handle: Option<thread::JoinHandle<()>>,
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
        // Process up to 32 control messages per tick
        for _ in 0..32 {
            match control_rx.try_recv() {
                Ok(ctrl) => match ctrl {
                    NetworkControl::Connect { addr, nickname } => {
                        // Replace any existing session cleanly first.
                        if let Some(mut old) = client.take() {
                            old.disconnect();
                        }
                        match NetworkClient::connect(addr, nickname) {
                            // Connected is announced by NetworkClient::update()
                            // once the handshake actually completes.
                            Ok(c) => client = Some(c),
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
                        if let Some(c) = &mut client {
                            c.send_message(text);
                        }
                    }
                    NetworkControl::SendVoicePacket(packet) => {
                        if let Some(c) = &mut client {
                            c.send_voice_frame(
                                packet.header.ssrc,
                                packet.header.seq,
                                packet.header.timestamp,
                                &packet.opus_data,
                            );
                        }
                    }
                    NetworkControl::SendSpeakingStatus(speaking) => {
                        if let Some(c) = &mut client {
                            c.send_speaking_status(speaking);
                        }
                    }
                    NetworkControl::SendAudioState { muted, deafened } => {
                        if let Some(c) = &mut client {
                            c.send_audio_state(muted, deafened);
                        }
                    }

                    // ── Screen share controls ───────────────────────
                    NetworkControl::ScreenShareStart { width, height } => {
                        if let Some(c) = &mut client {
                            c.send_screen_share_start(width, height);
                        }
                    }
                    NetworkControl::ScreenShareStop => {
                        if let Some(c) = &mut client {
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
                        if let Some(c) = &mut client {
                            // pack_video_frame panics past the chunk
                            // format's limit (255 × 1080 B). The encoder's
                            // VBV keeps frames well under it, but a codec
                            // misbehaving must not kill the network
                            // thread — drop the frame instead. The viewer
                            // sees a seq gap and recovers via keyframe
                            // request.
                            if data.len() > protocol::MAX_VIDEO_FRAME_BYTES {
                                warn!(
                                    "📺 Encoded frame seq={seq} too large for chunking \
                                     ({} bytes > {}), dropping",
                                    data.len(),
                                    protocol::MAX_VIDEO_FRAME_BYTES,
                                );
                                continue;
                            }
                            let chunks = pack_video_frame(
                                c.client_id(),
                                seq,
                                is_keyframe,
                                width,
                                height,
                                &data,
                            );
                            for chunk in chunks {
                                c.client.send_message(VIDEO_CHANNEL, chunk);
                            }
                        }
                    }
                    NetworkControl::SendSystemAudio {
                        seq,
                        channels,
                        duration_ms,
                        data,
                    } => {
                        if let Some(c) = &mut client {
                            c.send_system_audio(c.client_id(), seq, channels, duration_ms, &data);
                        }
                    }
                    NetworkControl::RequestKeyframe { target_id } => {
                        if let Some(c) = &mut client {
                            c.send_request_keyframe(target_id);
                        }
                    }
                    NetworkControl::SubscribeScreen { target_id } => {
                        if let Some(c) = &mut client {
                            c.send_subscribe_screen(target_id);
                        }
                    }
                    NetworkControl::UnsubscribeScreen { target_id } => {
                        if let Some(c) = &mut client {
                            c.send_unsubscribe_screen(target_id);
                        }
                    }

                    NetworkControl::Shutdown => {
                        if let Some(mut c) = client.take() {
                            c.disconnect();
                        }
                        running = false;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
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

                    // Everything else → app. These carry state (join /
                    // leave, init, chat, mute states) — losing one desyncs
                    // the UI until reconnect, so use a blocking send. The
                    // app drains this channel every event-loop pass.
                    other => {
                        let _ = msg_tx.send(other);
                        wake();
                    }
                }
            }

            c.flush_voice_packet();

            if !c.is_connected() && !c.is_connecting() {
                log::error!(
                    "Connection lost — is_connected=false, is_connecting=false, disconnect_reason={:?}",
                    c.client.disconnect_reason()
                );
                client = None;
                msg_tx
                    .send(NetworkMessage::Disconnected("Connection lost".into()))
                    .ok();
                wake();
            }
        }

        if client.is_some() {
            thread::sleep(Duration::from_millis(1));
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }
}
