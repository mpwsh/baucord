//! Chat server — admission, message routing, media relay.
//!
//! Wire types, the lane table, and serialization all come from the
//! shared `protocol` crate; the QUIC transport and identity pinning from
//! `transport`. This file owns policy: who may join (the shared
//! password, rate-limited), who may share and watch (limits), and when
//! to tell clients how their media is doing (receiver reports, stream
//! health).
//!
//! The server is sans-IO: [`ChatServer::update`] takes `now` and works
//! on the embedded [`Driver`]; the binary's loop and the test harness
//! move datagrams in and out around it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use log::{debug, info, warn};
use protocol::{
    features, Auth, ChannelDenyReason, ChannelInfo, ChatMessage, ClientId, ClientKeyProof,
    ClientMessage, DenyReason, Lane, RefusedReason, ServerMessage, ShareScope, TelemetryEvent,
    TelemetrySample,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use transport::{
    identity::{verify_ed25519, CLIENT_AUTH_EXPORTER_BYTES, CLIENT_AUTH_EXPORTER_LABEL},
    Budget, DisconnectReason, Driver, DriverHost, Event, Fingerprint, LinkStats, ServerIdentity,
};

use crate::{
    config::{validate_channel_list, ChannelSettings, Config},
    relay::{ShareInfo, VideoRelay, VoiceRelay},
};

/// Announced share dimensions must fall in this range (per-chunk
/// dimensions are additionally validated by the shared header parser).
const MIN_SHARE_DIMENSION: u16 = 16;
const MAX_SHARE_DIMENSION: u16 = 4096;

/// How often to sample viewer health and report to sharers.
const STREAM_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// How often each client gets its voice receiver report (spec §6.1).
const VOICE_REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// A tick is considered "media-active" for sleep scheduling if any
/// voice/video/system-audio packet arrived within this window.
const MEDIA_ACTIVE_WINDOW: Duration = Duration::from_millis(500);

/// How often to stat the config file for hot reload. A stat every few
/// seconds is free; anything file-watcher-fancier buys nothing here.
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// A connection that has not sent `Hello` this long after the
/// handshake is closed. The handshake itself already proved the peer
/// is a real client; on a link losing most of its packets the Hello
/// stream data needs several retransmissions on top (seen: handshake
/// up after 9 s, Hello still missing at +5 s). A pending slot for ten
/// seconds costs nothing.
const HELLO_DEADLINE: Duration = Duration::from_secs(10);

/// Failed admissions per source address in [`ADMISSION_FAILURE_WINDOW`]
/// before further attempts are refused outright.
const ADMISSION_FAILURE_LIMIT: usize = 5;
const ADMISSION_FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Time given for a `Refused` message to reach the peer before the
/// connection is closed under it.
const REFUSED_CLOSE_DELAY: Duration = Duration::from_millis(300);

/// Uploads whose sender stalled this long are forgotten.
const UPLOAD_STALL: Duration = Duration::from_secs(120);

/// One log upload per client per this interval (spec §9.1).
const LOG_UPLOAD_MIN_INTERVAL: Duration = Duration::from_secs(600);

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

// ── Stats ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct ServerStats {
    messages_count: u64,
    bytes_sent: u64,
    bytes_received: u64,
    admitted: u64,
    refused: u64,
}

// ── ChatServer ──────────────────────────────────────────────────────────

/// One client's in-flight chunked upload (image or log).
struct IncomingUpload {
    upload_id: u32,
    count: u32,
    received: u32,
    bytes: usize,
    chunks: Vec<Option<Vec<u8>>>,
    started: Instant,
}

/// A connection past the handshake, waiting for its `Hello`.
struct PendingPeer {
    since: Instant,
    remote: SocketAddr,
}

pub struct ChatServer {
    config: Config,
    driver: Driver,
    fingerprint: Fingerprint,
    /// The clock of the current tick; every timestamp in this file
    /// comes from here so the simulation harness controls it.
    now: Instant,
    pending: HashMap<ClientId, PendingPeer>,
    /// Recent failed admissions per source address.
    admission_failures: HashMap<IpAddr, VecDeque<Instant>>,
    usernames: HashMap<ClientId, String>,
    /// Negotiated feature bits per admitted client.
    features_of: HashMap<ClientId, u64>,
    /// Latest self-reported audio state: client → (muted, deafened).
    audio_states: HashMap<ClientId, (bool, bool)>,
    /// Clients currently deafened (voice relay skips them).
    deafened: HashSet<ClientId>,
    /// Voice-channel membership: client → channel id (index into
    /// `channel_infos`). Every admitted client has an entry.
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
    /// Per-client (time, bytes) of recently *accepted* chat images,
    /// pruned to the last minute — the sliding window behind
    /// `limits.max_image_kb_per_minute`.
    image_bytes: HashMap<ClientId, Vec<(Instant, usize)>>,
    /// In-progress image uploads, at most one per client — a chunk
    /// with a fresh `upload_id` abandons the old partial.
    uploads: HashMap<ClientId, IncomingUpload>,
    /// In-progress log uploads, same shape.
    log_uploads: HashMap<ClientId, IncomingUpload>,
    /// When each client last completed a log upload.
    last_log_upload: HashMap<ClientId, Instant>,
    /// Server-wide window of accepted image bytes — the aggregate
    /// budget the per-client one can't provide.
    image_bytes_global: Vec<(Instant, usize)>,
    /// Server-assigned chat message ids, unique for this run.
    next_message_id: u64,
    /// Per-client transport counters at the last `link:` report.
    link_baseline: HashMap<ClientId, LinkStats>,
    last_stats: Instant,
    last_health: Instant,
    last_voice_report: Instant,
    /// Last per-client `link:` log line (see `logging.link_stats_interval_secs`).
    last_link_log: Instant,
    /// Last instant any media packet arrived (drives adaptive sleep).
    last_media: Instant,
    stats: ServerStats,
    voice: VoiceRelay,
    video: VideoRelay,
}

impl DriverHost for ChatServer {
    fn driver(&mut self) -> &mut Driver {
        &mut self.driver
    }
}

impl ChatServer {
    /// Build the server around `identity`. Binds nothing: the caller
    /// owns the socket (or the simulated network) and pumps
    /// [`DriverHost::driver`] around [`Self::update`].
    ///
    /// # Errors
    /// Invalid channel or authentication configuration, or a TLS setup
    /// failure.
    pub fn new(
        config: Config,
        identity: &ServerIdentity,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        config
            .validate_authentication()
            .map_err(|e| format!("invalid server.yaml: {e}"))?;

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

        // ── Admission ───────────────────────────────────────────────
        if config.authentication.secure {
            info!("Secure mode: clients must present the shared password in Hello");
        } else {
            warn!("╔══════════════════════════════════════════════════════════════╗");
            warn!("║ AUTHENTICATION DISABLED — running in open mode.              ║");
            warn!("║ Anyone who can reach this UDP port can join with any name.   ║");
            warn!("║ Set `authentication.secure: true` and a password for real    ║");
            warn!("║ deployments. (Connections are still encrypted and pinned.)   ║");
            warn!("╚══════════════════════════════════════════════════════════════╝");
            if config.authentication.password.is_some() {
                warn!("authentication.password is set but ignored while secure is false");
            }
        }

        // ── Transport ───────────────────────────────────────────────
        let mut budget = Budget::server();
        budget.media_pace_bytes_per_sec = config.limits.media_relay_kb_per_sec.max(64) * 1024;
        budget.socket_buffer_bytes = send_buffer_target(config.network.max_clients);
        let driver = Driver::new_server(identity, config.network.max_clients, budget)?;

        let now = Instant::now();
        Ok(Self {
            config,
            driver,
            fingerprint: identity.fingerprint(),
            now,
            pending: HashMap::new(),
            admission_failures: HashMap::new(),
            usernames: HashMap::new(),
            features_of: HashMap::new(),
            audio_states: HashMap::new(),
            deafened: HashSet::new(),
            channel_of: HashMap::new(),
            channel_infos,
            default_channel,
            config_path: None,
            config_mtime: None,
            last_config_check: now,
            image_bytes: HashMap::new(),
            uploads: HashMap::new(),
            log_uploads: HashMap::new(),
            last_log_upload: HashMap::new(),
            image_bytes_global: Vec::new(),
            next_message_id: 1,
            link_baseline: HashMap::new(),
            last_stats: now,
            last_health: now,
            last_voice_report: now,
            last_link_log: now,
            last_media: now,
            stats: ServerStats::default(),
            voice: VoiceRelay::new(),
            video: VideoRelay::new(),
        })
    }

    /// The identity clients pin.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The transport budget in force (the binary sizes the socket
    /// buffers from it).
    #[must_use]
    pub fn budget(&self) -> &Budget {
        self.driver.budget()
    }

    /// Drive one tick: transport timers, admission, control messages,
    /// media relay, periodic reports.
    pub fn update(&mut self, now: Instant) {
        self.now = now;
        self.driver.handle_timeouts(now);

        self.process_events();
        self.process_control_messages();
        self.expire_pending();

        let clients = self.clients();
        let media_before = self.media_packet_count();
        self.voice.process(
            &mut self.driver,
            now,
            &clients,
            &self.deafened,
            &self.channel_of,
        );
        self.video.process(&mut self.driver, now, &clients);
        self.video
            .process_system_audio(&mut self.driver, now, &clients);
        if self.media_packet_count() != media_before {
            self.last_media = now;
        }

        if now.duration_since(self.last_health) >= STREAM_HEALTH_INTERVAL {
            self.send_stream_health();
            self.last_health = now;
        }

        if now.duration_since(self.last_voice_report) >= VOICE_REPORT_INTERVAL {
            self.send_voice_reports();
            self.last_voice_report = now;
        }

        if now.duration_since(self.last_config_check) >= CONFIG_CHECK_INTERVAL {
            self.check_config_reload();
            // Same lazy cadence: drop uploads whose sender stalled
            // mid-pace for two minutes (crashed, gave up, vanished).
            self.uploads
                .retain(|_, u| now.duration_since(u.started) < UPLOAD_STALL);
            self.log_uploads
                .retain(|_, u| now.duration_since(u.started) < UPLOAD_STALL);
            self.last_config_check = now;
        }

        if now.duration_since(self.last_stats)
            >= Duration::from_secs(self.config.logging.stats_interval_secs)
        {
            self.print_stats();
            self.last_stats = now;
        }
    }

    /// Sleep the main loop should take before the next tick.
    ///
    /// - Empty server: 20 ms (nothing to relay to anyone).
    /// - Media in the last 500 ms: 1 ms (sub-millisecond relay matters).
    /// - Clients connected but quiet: 5 ms — adds at most 5 ms to the
    ///   *first* packet of a talk burst, then the loop runs hot again;
    ///   inaudible next to the jitter buffer, and it cuts idle CPU ~5×.
    ///
    /// The caller additionally wakes for the transport's next timer.
    #[must_use]
    pub fn suggested_sleep(&self) -> Duration {
        if self.driver.peer_count() == 0 {
            Duration::from_millis(20)
        } else if self.now.duration_since(self.last_media) < MEDIA_ACTIVE_WINDOW {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(5)
        }
    }

    /// Admitted clients, in no particular order.
    fn clients(&self) -> Vec<ClientId> {
        self.usernames.keys().copied().collect()
    }

    fn media_packet_count(&self) -> u64 {
        self.voice.stats.packets_received
            + self.video.stats.chunks_received
            + self.video.stats.sys_audio_received
    }

    // ── Transport events ────────────────────────────────────────────

    fn process_events(&mut self) {
        let events: Vec<Event> = self.driver.drain_events().collect();
        for event in events {
            match event {
                Event::Connected { peer, remote, .. } => {
                    debug!("connection {peer:#x} from {remote} — waiting for Hello");
                    self.pending.insert(
                        peer,
                        PendingPeer {
                            since: self.now,
                            remote,
                        },
                    );
                }
                Event::Disconnected { peer, reason } => {
                    if self.pending.remove(&peer).is_some() {
                        debug!("connection {peer:#x} left before Hello: {reason}");
                    } else if self.usernames.contains_key(&peer) {
                        self.handle_client_disconnected(peer, &reason);
                    }
                }
                Event::DatagramsBlocked { peer } => {
                    debug!("datagram send buffer full toward {peer:#x}; dropping oldest");
                }
            }
        }
    }

    /// Close connections that never said Hello.
    fn expire_pending(&mut self) {
        let now = self.now;
        let late: Vec<ClientId> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.since) > HELLO_DEADLINE)
            .map(|(id, _)| *id)
            .collect();
        for peer in late {
            if let Some(p) = self.pending.remove(&peer) {
                info!(
                    "connection {peer:#x} from {} sent no Hello; closing",
                    p.remote
                );
            }
            self.driver.close(now, peer, "no hello");
        }
    }

    // ── Admission ───────────────────────────────────────────────────

    /// Handle the first Control frame of a pending connection.
    fn handle_hello(&mut self, peer: ClientId, raw: &[u8]) {
        let Some(pending) = self.pending.remove(&peer) else {
            return;
        };
        let ip = pending.remote.ip();
        self.stats.bytes_received += raw.len() as u64;

        let Some(ClientMessage::Hello {
            username,
            auth,
            client_key,
            features: client_features,
            client_version,
        }) = protocol::decode_client_message(raw)
        else {
            warn!(
                "connection {peer:#x} from {}: first message was not Hello",
                pending.remote
            );
            self.refuse(peer, ip, RefusedReason::BadHello, false);
            return;
        };

        if self.recent_failures(ip) >= ADMISSION_FAILURE_LIMIT {
            warn!("connection {peer:#x} from {ip}: too many failed admissions; refusing");
            self.refuse(peer, ip, RefusedReason::RateLimited, false);
            return;
        }

        if self.config.authentication.secure {
            let expected = self
                .config
                .authentication
                .password
                .as_deref()
                .unwrap_or_default();
            let Auth::Shared { password } = &auth;
            let ok = password.as_deref().is_some_and(|given| {
                given.len() == expected.len()
                    && bool::from(given.as_bytes().ct_eq(expected.as_bytes()))
            });
            if !ok {
                warn!(
                    "connection {peer:#x} from {}: wrong password",
                    pending.remote
                );
                self.refuse(peer, ip, RefusedReason::WrongPassword, true);
                return;
            }
        }

        if self.usernames.len() >= self.config.network.max_clients {
            info!("connection {peer:#x} from {}: server full", pending.remote);
            self.refuse(peer, ip, RefusedReason::ServerFull, false);
            return;
        }

        let mut username = protocol::sanitize_username(&username);
        if username.is_empty() {
            username = format!("User{peer:x}");
        }

        if let Some(proof) = client_key {
            self.note_client_key(peer, &username, &proof);
        }

        let negotiated = features::negotiate(features::SERVER_1_0, client_features);
        self.features_of.insert(peer, negotiated);
        self.stats.admitted += 1;
        info!(
            "{username} ({peer:#x}) admitted from {} (client {client_version}, features {negotiated:#x})",
            pending.remote
        );

        self.send_msg(
            peer,
            &ServerMessage::Welcome {
                client_id: peer,
                user_id: None,
                features: features::SERVER_1_0,
            },
        );
        self.handle_client_connected(peer, username);
    }

    /// Verify a client identity proof and log the derived user id. A
    /// 1.0 server only logs; nothing is stored or honored yet.
    fn note_client_key(&self, peer: ClientId, username: &str, proof: &ClientKeyProof) {
        let mut exported = [0u8; CLIENT_AUTH_EXPORTER_BYTES];
        if self
            .driver
            .export_keying_material(peer, CLIENT_AUTH_EXPORTER_LABEL, b"", &mut exported)
            .is_none()
        {
            return;
        }
        if verify_ed25519(&proof.public_key, &exported, &proof.signature) {
            let user_id: [u8; 32] = Sha256::digest(proof.public_key).into();
            info!(
                "{username} ({peer:#x}) proved client key; user id {}",
                hex(&user_id[..8])
            );
        } else {
            warn!("{username} ({peer:#x}) sent a client key proof that does not verify");
        }
    }

    fn recent_failures(&mut self, ip: IpAddr) -> usize {
        let now = self.now;
        let Some(times) = self.admission_failures.get_mut(&ip) else {
            return 0;
        };
        while times
            .front()
            .is_some_and(|t| now.duration_since(*t) > ADMISSION_FAILURE_WINDOW)
        {
            times.pop_front();
        }
        if times.is_empty() {
            self.admission_failures.remove(&ip);
            return 0;
        }
        times.len()
    }

    /// Send `Refused`, then close once it had time to land.
    fn refuse(&mut self, peer: ClientId, ip: IpAddr, reason: RefusedReason, count: bool) {
        if count {
            self.admission_failures
                .entry(ip)
                .or_default()
                .push_back(self.now);
        }
        self.stats.refused += 1;
        self.send_msg(peer, &ServerMessage::Refused { reason });
        self.driver.close_after(self.now, peer, REFUSED_CLOSE_DELAY);
    }

    // ── Message send helpers ────────────────────────────────────────

    fn send_msg(&mut self, client_id: ClientId, msg: &ServerMessage) {
        debug_assert!(
            !msg.is_upload_chunk(),
            "image chunks go through relay_image"
        );
        if let Some(encoded) = protocol::encode_server_message(msg) {
            self.stats.bytes_sent += encoded.len() as u64;
            if let Err(e) = self.driver.send(self.now, client_id, msg.lane(), &encoded) {
                debug!("send to {client_id:#x} on {} failed: {e}", msg.lane());
            }
        }
    }

    fn broadcast_msg(&mut self, msg: &ServerMessage) {
        for client in self.clients() {
            self.send_msg(client, msg);
        }
    }

    fn broadcast_msg_except(&mut self, except: ClientId, msg: &ServerMessage) {
        for client in self.clients() {
            if client != except {
                self.send_msg(client, msg);
            }
        }
    }

    /// The next server-assigned chat message id and its wall-clock
    /// timestamp (ms since the Unix epoch).
    fn stamp(&mut self) -> (u64, u64) {
        let id = self.next_message_id;
        self.next_message_id += 1;
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        (id, ts)
    }

    // ── Uploads ─────────────────────────────────────────────────────

    /// Fold one upload chunk into the sender's in-flight image; on
    /// completion, validate (magic, size, bandwidth budgets) and hand
    /// the image to the paced relay. Bounds mirror the codec's, plus:
    /// one upload per client (a fresh `upload_id` abandons the old
    /// partial), a projected-size check on the claimed chunk count so
    /// an absurd upload dies on its first chunk, and a running byte
    /// cap so lying `count` values can't overshoot.
    fn handle_image_chunk(
        &mut self,
        client_id: ClientId,
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    ) {
        let Some(image) = fold_upload(
            &mut self.uploads,
            client_id,
            upload_id,
            index,
            count,
            data,
            protocol::MAX_IMAGE_CHUNKS,
            protocol::MAX_CHAT_IMAGE_BYTES,
            self.now,
        ) else {
            return;
        };
        let image = match image {
            Folded::Complete(image) => image,
            Folded::Malformed => {
                warn!(
                    "Dropping malformed image chunk from {} (count {count}, index {index})",
                    self.get_username(client_id),
                );
                return;
            }
            Folded::Oversized(bytes) => {
                warn!(
                    "Dropping oversized image upload from {} ({} KB and counting)",
                    self.get_username(client_id),
                    bytes / 1024,
                );
                return;
            }
        };

        // Relay only what claims to be a PNG, JPEG, or GIF — the
        // desktop encodes exactly these, so anything else is a
        // modified client wasting everyone's bandwidth. Receivers
        // sniff the same magic to pick a decoder. Checked *before*
        // the bandwidth budget so junk never consumes anyone's window.
        let is_png = image.starts_with(&PNG_MAGIC);
        let is_jpeg = image.starts_with(&[0xFF, 0xD8, 0xFF]);
        let is_gif = image.starts_with(b"GIF87a") || image.starts_with(b"GIF89a");
        if !is_png && !is_jpeg && !is_gif {
            warn!(
                "Dropping non-PNG/JPEG/GIF chat image ({} bytes) from {}",
                image.len(),
                self.get_username(client_id),
            );
            return;
        }
        if !self.allow_image(client_id, image.len()) {
            return;
        }
        info!(
            "[{}] sent an image ({} KB)",
            self.get_username(client_id),
            image.len() / 1024,
        );
        self.stats.messages_count += 1;
        self.relay_image(client_id, upload_id, &image);
    }

    /// Queue a validated image for relay: re-chunk, pre-encode each
    /// chunk *once*, and open one Media stream per receiver carrying
    /// all of them. The transport paces each stream at
    /// `limits.media_relay_kb_per_sec` so a big image never floods a
    /// receiver's downlink queue — voice shares that queue.
    fn relay_image(&mut self, client_id: ClientId, upload_id: u32, image: &[u8]) {
        let (id, ts) = self.stamp();
        let hash: [u8; 32] = Sha256::digest(image).into();
        #[allow(clippy::cast_possible_truncation)] // bounded by MAX_IMAGE_CHUNKS
        let count = image.len().div_ceil(protocol::IMAGE_CHUNK_BYTES) as u32;
        let mut encoded: Vec<protocol::Bytes> = Vec::with_capacity(count as usize);
        for (i, chunk) in image.chunks(protocol::IMAGE_CHUNK_BYTES).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let msg = ServerMessage::ChatImageChunk {
                client_id,
                upload_id,
                index: i as u32,
                count,
                id,
                ts,
                hash,
                data: chunk.to_vec(),
            };
            let Some(bytes) = protocol::encode_server_message(&msg) else {
                warn!("Image relay chunk failed to encode; dropping image");
                return;
            };
            encoded.push(protocol::Bytes::from(bytes));
        }
        for peer in self.clients() {
            self.stats.bytes_sent += image.len() as u64;
            if let Err(e) = self
                .driver
                .send_media(self.now, peer, encoded.iter().cloned())
            {
                debug!("image relay to {peer:#x} failed: {e}");
            }
        }
    }

    /// Sliding-window *bandwidth* budget for chat images: admit the
    /// image only if the last minute's accepted bytes plus this one
    /// stay within both the per-client and server-wide budgets; bytes
    /// are recorded only for accepted images. Dropped images cost the
    /// sender nothing here (their upload already crossed the wire —
    /// that part is physics), but they are never multiplied into
    /// `clients ×` relay egress.
    fn allow_image(&mut self, client_id: ClientId, len: usize) -> bool {
        let per_client = self.config.limits.max_image_kb_per_minute * 1024;
        let global = self.config.limits.max_image_kb_per_minute_global * 1024;
        if per_client == 0 || global == 0 {
            return false;
        }
        let now = self.now;
        let window = Duration::from_secs(60);

        let entries = self.image_bytes.entry(client_id).or_default();
        entries.retain(|(t, _)| now.duration_since(*t) < window);
        let client_used: usize = entries.iter().map(|(_, b)| b).sum();
        if client_used + len > per_client {
            warn!(
                "Image bandwidth limit for {}: {} KB used of {} KB/min, dropping a {} KB image",
                self.usernames
                    .get(&client_id)
                    .map_or("<unknown>", String::as_str),
                client_used / 1024,
                per_client / 1024,
                len / 1024,
            );
            return false;
        }

        self.image_bytes_global
            .retain(|(t, _)| now.duration_since(*t) < window);
        let global_used: usize = self.image_bytes_global.iter().map(|(_, b)| b).sum();
        if global_used + len > global {
            warn!(
                "Server-wide image bandwidth limit: {} KB used of {} KB/min, \
                 dropping a {} KB image from {}",
                global_used / 1024,
                global / 1024,
                len / 1024,
                self.usernames
                    .get(&client_id)
                    .map_or("<unknown>", String::as_str),
            );
            return false;
        }

        self.image_bytes
            .entry(client_id)
            .or_default()
            .push((now, len));
        self.image_bytes_global.push((now, len));
        true
    }

    /// A "send my log to this server" upload (§9.1): reassembled like an
    /// image, capped at [`protocol::MAX_LOG_UPLOAD_BYTES`], one per
    /// client per ten minutes, stored under `<identity.dir>/logs/`.
    fn handle_log_chunk(
        &mut self,
        client_id: ClientId,
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    ) {
        if index == 0
            && self
                .last_log_upload
                .get(&client_id)
                .is_some_and(|t| self.now.duration_since(*t) < LOG_UPLOAD_MIN_INTERVAL)
        {
            debug!(
                "Ignoring log upload from {}: one per {} minutes",
                self.get_username(client_id),
                LOG_UPLOAD_MIN_INTERVAL.as_secs() / 60
            );
            self.log_uploads.remove(&client_id);
            return;
        }
        let Some(folded) = fold_upload(
            &mut self.log_uploads,
            client_id,
            upload_id,
            index,
            count,
            data,
            protocol::MAX_LOG_UPLOAD_CHUNKS,
            protocol::MAX_LOG_UPLOAD_BYTES,
            self.now,
        ) else {
            return;
        };
        let log = match folded {
            Folded::Complete(log) => log,
            Folded::Malformed | Folded::Oversized(_) => {
                warn!(
                    "Dropping malformed or oversized log upload from {}",
                    self.get_username(client_id)
                );
                return;
            }
        };
        self.last_log_upload.insert(client_id, self.now);
        let username = self.get_username(client_id);
        let dir = PathBuf::from(&self.config.identity.dir).join("logs");
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let safe: String = username
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let path = dir.join(format!("{safe}-{client_id:x}-{stamp}.log"));
        match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &log)) {
            Ok(()) => info!(
                "[{username}] uploaded their log ({} KB) → {}",
                log.len() / 1024,
                path.display()
            ),
            Err(e) => warn!(
                "[{username}] uploaded their log ({} KB) but it could not be stored at {}: {e}",
                log.len() / 1024,
                path.display()
            ),
        }
    }

    // ── Connection events ───────────────────────────────────────────

    fn handle_client_connected(&mut self, client_id: ClientId, username: String) {
        info!("{username} ({client_id:#x}) connected");
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

    fn handle_client_disconnected(&mut self, client_id: ClientId, reason: &DisconnectReason) {
        let username = self
            .usernames
            .remove(&client_id)
            .unwrap_or_else(|| format!("User{client_id:x}"));
        self.features_of.remove(&client_id);
        self.audio_states.remove(&client_id);
        self.deafened.remove(&client_id);
        self.channel_of.remove(&client_id);
        self.image_bytes.remove(&client_id);
        self.uploads.remove(&client_id);
        self.log_uploads.remove(&client_id);
        self.last_log_upload.remove(&client_id);
        self.link_baseline.remove(&client_id);

        match reason {
            DisconnectReason::ApplicationClosed { .. } | DisconnectReason::LocallyClosed => {
                info!("{username} ({client_id:#x}) disconnected: {reason}");
            }
            DisconnectReason::TimedOut => {
                warn!(
                    "{username} ({client_id:#x}) timed out — no packets for the idle \
                     timeout (client crashed, network gone, or a NAT dropped the mapping)"
                );
            }
            other => warn!("{username} ({client_id:#x}) disconnected: {other}"),
        }

        let was_sharing = self.video.is_sharing(client_id);
        self.video
            .handle_disconnect(&mut self.driver, self.now, client_id);
        self.voice.handle_disconnect(client_id);
        if was_sharing {
            self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
        }

        self.broadcast_msg(&ServerMessage::UserLeft { client_id });
    }

    // ── Control-plane messages ──────────────────────────────────────

    fn process_control_messages(&mut self) {
        // Pending connections: exactly one Control frame matters — the
        // Hello. Anything else they send before admission is discarded.
        let pending: Vec<ClientId> = self.pending.keys().copied().collect();
        for peer in pending {
            if let Some(raw) = self.driver.recv(peer, Lane::Control) {
                self.handle_hello(peer, &raw);
            }
            for lane in [Lane::Signal, Lane::Media, Lane::Voice, Lane::SystemAudio] {
                while self.driver.recv(peer, lane).is_some() {}
            }
            while self.driver.recv_video(peer).is_some() {}
        }

        for client_id in self.clients() {
            for lane in [Lane::Control, Lane::Signal, Lane::Media] {
                while let Some(message) = self.driver.recv(client_id, lane) {
                    self.stats.bytes_received += message.len() as u64;
                    match protocol::decode_client_message(&message) {
                        Some(msg) => self.handle_client_message(client_id, msg),
                        None => debug!(
                            "Undecodable {lane} message ({} bytes) from {client_id:#x}",
                            message.len()
                        ),
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_client_message(&mut self, client_id: ClientId, message: ClientMessage) {
        match message {
            ClientMessage::Hello { .. } => {
                debug!("{client_id:#x} sent a second Hello; ignored");
            }
            ClientMessage::Chat(text) => {
                let text =
                    protocol::truncate_on_char_boundary(&text, self.config.limits.max_chat_bytes);
                let username = self.get_username(client_id);
                info!("[{username}]: {text}");
                self.stats.messages_count += 1;
                let (id, ts) = self.stamp();
                self.broadcast_msg(&ServerMessage::Chat(ChatMessage {
                    id,
                    ts,
                    client_id,
                    text,
                }));
            }
            ClientMessage::ChatImageChunk {
                upload_id,
                index,
                count,
                data,
            } => {
                self.handle_image_chunk(client_id, upload_id, index, count, data);
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
                self.video
                    .stop_sharing(&mut self.driver, self.now, client_id);
                self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
            }
            ClientMessage::SubscribeScreen { target_id } => {
                self.handle_subscribe(client_id, target_id);
            }
            ClientMessage::UnsubscribeScreen { target_id } => {
                self.video
                    .unsubscribe(&mut self.driver, self.now, client_id, target_id);
            }
            ClientMessage::RequestKeyframe { target_id } => {
                // Only honored from subscribed viewers, so a misbehaving
                // client can't force arbitrary encoders to spam IDRs.
                // The per-sharer gate is the single authoritative rate
                // limit for all keyframe request paths.
                if self.video.is_subscribed(client_id, target_id)
                    && self.video.allow_keyframe_request(self.now, target_id)
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

            // ── Telemetry ───────────────────────────────────────────
            ClientMessage::Telemetry(sample) => {
                if self.accepts_telemetry(client_id) && self.config.logging.telemetry {
                    info!(
                        "telemetry: {} ({client_id:#x}): {}",
                        self.get_username(client_id),
                        summarize_telemetry(&sample)
                    );
                }
            }
            ClientMessage::TelemetryEvent(event) => {
                if self.accepts_telemetry(client_id) {
                    info!(
                        "telemetry event: {} ({client_id:#x}): {}",
                        self.get_username(client_id),
                        describe_event(&event)
                    );
                }
            }
            ClientMessage::LogUploadChunk {
                upload_id,
                index,
                count,
                data,
            } => {
                self.handle_log_chunk(client_id, upload_id, index, count, data);
            }
        }
    }

    fn accepts_telemetry(&self, client_id: ClientId) -> bool {
        self.features_of
            .get(&client_id)
            .is_some_and(|f| features::has(*f, features::TELEMETRY))
    }

    fn handle_share_start(
        &mut self,
        client_id: ClientId,
        width: u16,
        height: u16,
        scope: ShareScope,
    ) {
        let dims_ok = (MIN_SHARE_DIMENSION..=MAX_SHARE_DIMENSION).contains(&width)
            && (MIN_SHARE_DIMENSION..=MAX_SHARE_DIMENSION).contains(&height);
        if !dims_ok {
            warn!("{client_id:#x} announced share with invalid dimensions {width}x{height}");
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
                "Share denied for {client_id:#x}: {} of {} share slots in use",
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
                        .clients()
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
                warn!(
                    "Config reload: {} is not valid YAML — keeping current config: {e}",
                    path.display()
                );
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
            Ok(()) => info!(
                "Config reload: channel list applied from {}",
                path.display()
            ),
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
    ///
    /// # Errors
    /// The list failed validation; nothing was changed.
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
                    self.video
                        .unsubscribe(&mut self.driver, self.now, viewer, sharer);
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
            for peer in self.clients() {
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
    fn handle_join_channel(
        &mut self,
        client_id: ClientId,
        channel_id: u16,
        password: Option<&str>,
    ) {
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
        self.channel_of
            .values()
            .filter(|c| **c == channel_id)
            .count()
    }

    /// Admitted clients in `channel` (None never matches anyone).
    fn peers_in_channel(&self, channel: Option<u16>) -> Vec<ClientId> {
        self.clients()
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
                self.video
                    .unsubscribe(&mut self.driver, self.now, viewer, sharer);
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
                    self.video
                        .unsubscribe(&mut self.driver, self.now, mover, sharer);
                }
                self.send_msg(
                    mover,
                    &ServerMessage::ScreenShareStopped { client_id: sharer },
                );
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
                "Subscribe denied for {client_id:#x} → {target_id:#x}: viewer limit {} reached",
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
        if self.video.allow_keyframe_request(self.now, target_id) {
            self.send_keyframe_request(target_id, client_id);
        }
    }

    // ── Reports ─────────────────────────────────────────────────────

    /// Sample viewer health per stream and report to sharers.
    fn send_stream_health(&mut self) {
        let reports = self.video.stream_health(&self.driver);
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

    /// Every two seconds, tell each client what the relay actually
    /// received of its voice (spec §6.1) — the only loss figure its
    /// audio ladder may act on — and, when enabled, log the `link:`
    /// line for the same interval.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn send_voice_reports(&mut self) {
        let link_interval = Duration::from_secs(self.config.logging.link_stats_interval_secs);
        let log_due = !link_interval.is_zero()
            && self.now.duration_since(self.last_link_log) >= link_interval;
        if log_due {
            self.last_link_log = self.now;
        }
        let interval_secs = VOICE_REPORT_INTERVAL.as_secs_f64();

        for client_id in self.clients() {
            let Some(stats) = self.driver.stats(client_id) else {
                continue;
            };
            let voice = self.voice.seq.take_window(client_id).unwrap_or_default();
            let rtt_ms = stats.rtt.as_millis().min(u128::from(u32::MAX)) as u32;
            self.send_msg(
                client_id,
                &ServerMessage::VoiceReceiverReport {
                    received: voice.received,
                    expected: voice.expected,
                    lost: voice.lost(),
                    late: voice.late,
                    rtt_ms,
                },
            );

            if log_due {
                let base = self
                    .link_baseline
                    .get(&client_id)
                    .copied()
                    .unwrap_or_default();
                let username = self.get_username(client_id);
                let tx_kbit = (stats.bytes_tx.saturating_sub(base.bytes_tx)) as f64 * 8.0
                    / 1000.0
                    / interval_secs;
                let rx_kbit = (stats.bytes_rx.saturating_sub(base.bytes_rx)) as f64 * 8.0
                    / 1000.0
                    / interval_secs;
                let sup = stats
                    .groups_reset_superseded
                    .saturating_sub(base.groups_reset_superseded);
                let dl = stats
                    .groups_reset_deadline
                    .saturating_sub(base.groups_reset_deadline);
                let ack = stats
                    .groups_reset_unacked
                    .saturating_sub(base.groups_reset_unacked);
                let upstream = stats
                    .groups_reset_upstream
                    .saturating_sub(base.groups_reset_upstream);
                let behind = stats.groups_reset.saturating_sub(base.groups_reset);
                let cause = if behind > 0 || upstream > 0 {
                    format!(
                        " (superseded {sup}, deadline {dl}, unacked {ack}, upstream {upstream})"
                    )
                } else {
                    String::new()
                };
                info!(
                    "link: {username} ({client_id:#x}): rtt {rtt_ms} ms, cwnd {} KB, path loss {:.1}%, \
                     tx {tx_kbit:.0} kbit/s, rx {rx_kbit:.0} kbit/s, mtu {}, dgram drops {}, \
                     groups behind {behind}/{}{cause} | voice rx {} pkt, expected {}, lost {} ({:.1}%), late {}{}",
                    stats.cwnd / 1024,
                    stats.path_loss_pct_since(&base),
                    stats.current_mtu,
                    stats.datagram_send_drops - base.datagram_send_drops,
                    stats.groups_started - base.groups_started,
                    voice.received,
                    voice.expected,
                    voice.lost(),
                    voice.loss_pct(),
                    voice.late,
                    if voice.resyncs > 0 {
                        format!(", resyncs {}", voice.resyncs)
                    } else {
                        String::new()
                    },
                );
            }
            self.link_baseline.insert(client_id, stats);
        }
    }

    /// Forward a keyframe request to `sharer` on behalf of `viewer`.
    fn send_keyframe_request(&mut self, sharer: ClientId, viewer: ClientId) {
        debug!("Forwarding keyframe request: viewer {viewer:#x} → sharer {sharer:#x}");
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
            .unwrap_or_else(|| format!("User{client_id:x}"))
    }

    #[allow(clippy::cast_precision_loss)]
    fn print_stats(&self) {
        info!("=== {} Statistics ===", self.config.server.name);
        info!(
            "Connected clients: {} (admitted {}, refused {}, transport-refused {})",
            self.usernames.len(),
            self.stats.admitted,
            self.stats.refused,
            self.driver.refused_count(),
        );
        info!("Text messages: {}", self.stats.messages_count);

        self.voice.print_stats();
        self.video.print_stats();

        info!(
            "Control data sent: {:.2} MB, received: {:.2} MB",
            self.stats.bytes_sent as f64 / 1_048_576.0,
            self.stats.bytes_received as f64 / 1_048_576.0
        );

        for client_id in self.clients() {
            if let Some(stats) = self.driver.stats(client_id) {
                let username = self.get_username(client_id);
                info!(
                    "  {username} ({client_id:#x}): rtt {} ms, cwnd {} KB, lost {}/{} pkt, tx {:.2} MB, rx {:.2} MB, groups behind {}/{} (superseded {}, deadline {}, unacked {}, upstream {})",
                    stats.rtt.as_millis(),
                    stats.cwnd / 1024,
                    stats.lost_packets,
                    stats.sent_packets,
                    stats.bytes_tx as f64 / 1_048_576.0,
                    stats.bytes_rx as f64 / 1_048_576.0,
                    stats.groups_reset,
                    stats.groups_started,
                    stats.groups_reset_superseded,
                    stats.groups_reset_deadline,
                    stats.groups_reset_unacked,
                    stats.groups_reset_upstream,
                );
            }
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Outcome of folding one chunk into an upload.
enum Folded {
    Complete(Vec<u8>),
    Malformed,
    Oversized(usize),
}

/// Fold one chunk into `uploads[client]`, creating the entry on a fresh
/// `upload_id` (which abandons any older partial). `None` while the
/// upload is still incomplete (or the chunk was a duplicate).
#[allow(clippy::too_many_arguments)]
fn fold_upload(
    uploads: &mut HashMap<ClientId, IncomingUpload>,
    client_id: ClientId,
    upload_id: u32,
    index: u32,
    count: u32,
    data: Vec<u8>,
    max_chunks: u32,
    max_bytes: usize,
    now: Instant,
) -> Option<Folded> {
    if count == 0
        || count > max_chunks
        || index >= count
        || (count as usize - 1) * protocol::IMAGE_CHUNK_BYTES > max_bytes
    {
        return Some(Folded::Malformed);
    }

    let stale = uploads
        .get(&client_id)
        .is_some_and(|u| u.upload_id != upload_id);
    if stale {
        uploads.remove(&client_id);
    }
    let upload = uploads.entry(client_id).or_insert_with(|| IncomingUpload {
        upload_id,
        count,
        received: 0,
        bytes: 0,
        chunks: vec![None; count as usize],
        started: now,
    });
    if upload.count != count || upload.chunks[index as usize].is_some() {
        return None; // inconsistent or duplicate — drop silently
    }
    upload.bytes += data.len();
    if upload.bytes > max_bytes {
        let bytes = upload.bytes;
        uploads.remove(&client_id);
        return Some(Folded::Oversized(bytes));
    }
    upload.chunks[index as usize] = Some(data);
    upload.received += 1;
    if upload.received < upload.count {
        return None;
    }

    let upload = uploads.remove(&client_id)?;
    let mut whole = Vec::with_capacity(upload.bytes);
    for chunk in upload.chunks {
        whole.extend_from_slice(&chunk?);
    }
    Some(Folded::Complete(whole))
}

/// One-line rendering of a telemetry sample for the log.
#[allow(clippy::format_push_string)] // a log line; readability over allocation
fn summarize_telemetry(s: &TelemetrySample) -> String {
    let mut out = format!(
        "v{} {}/{}{} | path rtt {} ms, loss {:.1}%, cwnd {} KB, up {:.0} kbit/s, down {:.0} kbit/s, dgram drops {}, blocked {}",
        s.client.version,
        s.client.os,
        s.client.arch,
        if s.client.portable { " portable" } else { "" },
        s.path.rtt_ms,
        s.path.loss_pct,
        s.path.cwnd / 1024,
        s.path.kbps_up,
        s.path.kbps_down,
        s.path.datagram_send_drops,
        s.path.datagrams_blocked,
    );
    if let Some(a) = &s.audio {
        out.push_str(&format!(
            " | audio {} kbit/s {} {} ms{} fec {}% loss {:.1}%{} sent {} enc {} µs over/under {}/{} in '{}' out '{}' | rx {} lost {} fec {} red {} plc {} jitter {}/{} ms dec {} µs",
            a.bitrate_bps / 1000,
            a.band,
            a.frame_ms,
            if a.red { " RED" } else { "" },
            a.fec_hint_pct,
            a.smoothed_loss_pct,
            if a.exercised { "" } else { " (idle)" },
            a.packets_sent,
            a.encode_us,
            a.input_overruns,
            a.output_underruns,
            a.input_device,
            a.output_device,
            a.packets_received,
            a.packets_lost,
            a.fec_recoveries,
            a.red_recoveries,
            a.plc_frames,
            a.jitter_ms,
            a.jitter_target_ms,
            a.decode_us,
        ));
    }
    if let Some(v) = &s.sharing {
        out.push_str(&format!(
            " | sharing {} ({}) rung {}% fps {:.1} scale {}% forced IDR {} kf req {} groups reset {}",
            v.preset,
            v.encoder,
            v.rung_pct,
            v.fps_encoded,
            v.fps_scale_pct,
            v.forced_keyframes,
            v.keyframe_requests,
            v.groups_reset,
        ));
    }
    for v in &s.viewing {
        out.push_str(&format!(
            " | viewing {:#x}: decoded {} dropped {} kf req {} dec {} µs",
            v.sharer, v.frames_decoded, v.frames_dropped, v.keyframe_requests_sent, v.decode_us,
        ));
    }
    out
}

fn describe_event(e: &TelemetryEvent) -> String {
    match e {
        TelemetryEvent::RateRungChanged {
            level,
            bitrate_bps,
            red,
        } => format!(
            "audio rung → {level} ({} kbit/s{})",
            bitrate_bps / 1000,
            if *red { ", RED" } else { "" }
        ),
        TelemetryEvent::IdentityPromptShown => "identity prompt shown".to_string(),
        TelemetryEvent::DeviceChanged { description } => format!("device changed: {description}"),
        TelemetryEvent::InputDiscontinuityBurst { count } => {
            format!("input discontinuity burst ({count})")
        }
        TelemetryEvent::SendBufferPressure => "send buffer pressure".to_string(),
        TelemetryEvent::ShareStarted { preset, encoder } => {
            format!("share started ({preset}, {encoder})")
        }
        TelemetryEvent::ShareStopped => "share stopped".to_string(),
        TelemetryEvent::Disconnected { reason } => format!("disconnected: {reason}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// OS send-buffer target for the relay socket: the worst burst is one
/// full-size chat image (up to [`protocol::MAX_CHAT_IMAGE_BYTES`],
/// 10 MiB) fanned out to every client at once, so scale with
/// `max_clients`. Floor: 8 MiB. Ceiling: 64 MiB — the buffer doesn't
/// have to hold the entire fan-out, only the gap between enqueue and
/// NIC drain; past 64 MiB, per-connection pacing spreads the rest.
fn send_buffer_target(max_clients: usize) -> usize {
    (max_clients * protocol::MAX_CHAT_IMAGE_BYTES).clamp(8 * 1024 * 1024, 64 * 1024 * 1024)
}

#[cfg(test)]
mod socket_tests {
    use super::send_buffer_target;

    #[test]
    fn send_buffer_scales_with_clients() {
        // A solo room keeps the 8 MiB floor.
        assert_eq!(send_buffer_target(0), 8 * 1024 * 1024);
        // Images are 10 MiB units: even a 4-user room wants 40 MiB.
        assert_eq!(send_buffer_target(4), 40 * 1024 * 1024);
        // 12 users — the room size that broke for real — hits the
        // ceiling, as does anything larger.
        assert_eq!(send_buffer_target(12), 64 * 1024 * 1024);
        assert_eq!(send_buffer_target(64), 64 * 1024 * 1024);
    }
}
