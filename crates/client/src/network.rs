// network.rs — QUIC client + network thread.
//
// NetworkClient handles the session (transport driver, admission,
// protocol, serialization); NetworkThreadHandle owns the thread that
// pumps it and routes voice packets to the voice thread and video
// groups to the screen thread.
//
// All wire protocol types come from the shared `protocol` crate; the
// transport (quinn-proto driven from this thread, no runtime) from
// `transport`.
//
// Authentication (spec §3): the server's identity is an Ed25519 key we
// pin by fingerprint — from the invite string, or on first use — and
// the room password rides inside `Hello` over the encrypted connection.
// Our own key signs the TLS exporter so the server can derive a stable
// user id. A server whose key changed is refused before any application
// data flows; the UI decides whether to accept the new one.
//
// Media attribution: relayed media arrives wrapped in the server's
// sender envelope ([sender_id: u64][packet]) — the sender id is stamped
// by the server from the authenticated transport, so it cannot be
// spoofed by other clients. See `protocol::relayed`.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use audio::{AudioPacket, PayloadType};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use log::{info, warn};
use protocol::{
    features, pack_system_audio, pack_video_frame, pack_voice_with_flags, parse_system_audio,
    parse_voice, split_sender, AudioSample, Auth, ChannelDenyReason, ChannelInfo, ClientId,
    ClientInfo, ClientKeyProof, ClientMessage, DenyReason, Lane, PathSample, RefusedReason,
    ServerMessage, ShareScope, SharingSample, TelemetryEvent, TelemetrySample, VideoReassembler,
    ViewingSample, VOICE_FLAG_RED,
};
use sha2::{Digest, Sha256};
use transport::{
    identity::{CLIENT_AUTH_EXPORTER_BYTES, CLIENT_AUTH_EXPORTER_LABEL},
    Budget, ClientKey, DisconnectReason, Driver, Event, Fingerprint, LinkStats, Observation,
    PeerId, Trust, UdpIo, UploadId, VideoItem, OWN_TRACK,
};

use crate::{screen::ScreenControl, voice::VoiceControl, WakeFn};

/// How often to sample link statistics for the UI and the screen
/// thread's bitrate ladder. The ladder's dwell time is sized against
/// this cadence (see `RATE_DWELL` in screen.rs).
const NETWORK_STATS_INTERVAL: Duration = Duration::from_secs(2);

/// How often a telemetry sample goes to the server (spec §9.1).
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(10);

/// Log link samples at or above this transport-level loss so a bad
/// stretch is visible in the log file, not just the overlay. Sampled
/// every [`NETWORK_STATS_INTERVAL`], so a sustained bad link costs one
/// line per 2 s.
const LINK_LOG_LOSS_PCT: f64 = 5.0;
/// While the link carries video-scale traffic, log from this much lower
/// loss. A 1080p60 frame is ~16 chunks and one lost chunk loses the
/// frame, so 1–5% packet loss is a broken stream — and the 2026-08-29
/// incident sat exactly there, invisible under the 5% threshold. The
/// throughput gate keeps a quiet voice link's idle-loss artifacts from
/// filling the log.
const LINK_LOG_LOSS_PCT_STREAMING: f64 = 1.0;
const LINK_LOG_STREAMING_KBPS: f64 = 500.0;

/// A voice receiver report covers a window in which the sender was
/// actually talking only if the relay expected at least this many
/// packets (spec §6.1: "exercised"). Below that the loss figure is a
/// handful of DTX refreshes and says nothing.
const VOICE_REPORT_MIN_PACKETS: u32 = 15;

/// Ceiling applied to voice loss readings before they are fed to the
/// encoder. Its rate controller only needs to know "the link is bad";
/// the exact figure above 50 adds nothing.
const UPLINK_LOSS_CLAMP_PCT: f64 = 50.0;

/// The relay sends a receiver report every 2 s unconditionally. None
/// for this long means the control stream is stalled — the link is
/// bad, not quiet — and the audio ladder must not read the silence as
/// "all clear" (the 2026-09-03 05:07 log: ladder stepped *up* to 24 kbps
/// during a 90 % loss blackout because the reports had gone stale).
const REPORT_STALL: Duration = Duration::from_secs(6);

/// Longest a relayed image may sit half-assembled before it is
/// forgotten (its sender vanished or gave up mid-pace).
const IMAGE_PARTIAL_TTL: Duration = Duration::from_secs(120);

/// One "send my log" upload per this interval, mirroring the server's
/// acceptance window.
const LOG_UPLOAD_MIN_INTERVAL: Duration = Duration::from_secs(600);

// ── Messages sent to the UI / App layer ─────────────────────────────────

/// Why a session ended for good. The UI maps each to a sentence a
/// person can act on; nothing here is meant to be displayed raw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectKind {
    /// The user clicked Disconnect.
    UserRequested,
    /// The server refused admission (wrong password is reported
    /// separately as `AccessDenied` because it has its own prompt).
    Refused(RefusedReason),
    /// This client is too old (or too new) for the server.
    Outdated,
    /// Could not even start: socket, TLS setup, bad address.
    LocalError,
    /// Anything else the transport reported and we did not classify.
    Other,
}

/// Why a session dropped in a way worth retrying automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossKind {
    /// Nothing from the server for the idle timeout.
    Timeout,
    /// The server no longer knew our session (it timed us out first,
    /// or restarted).
    Reset,
    /// The connection attempt itself did not get through: the
    /// handshake never completed, or the server closed it because our
    /// Hello never arrived — a link losing most of its packets.
    Unreachable,
}

#[derive(Debug, Clone)]
pub enum NetworkMessage {
    /// Outbound image upload progress, 0–100. 100 means the transport
    /// has taken every byte — the UI hides its indicator on it.
    UploadProgress {
        pct: u8,
    },
    /// Admitted by the server (`Welcome` received).
    Connected,
    /// The server presented `fingerprint`. `first_use` when we had no
    /// pin for this server (trust on first use): the app stores it on
    /// the server entry. Also sent after a signed key rotation, with
    /// the new fingerprint, so the pin follows.
    ServerIdentity {
        fingerprint: String,
        first_use: bool,
    },
    /// The server's key differs from the one we pinned, and no signed
    /// successor proof vouched for it. Nothing was sent to it. The app
    /// asks the user; connecting again with `fingerprint = observed`
    /// accepts the new key.
    IdentityChanged {
        expected: String,
        observed: String,
    },
    /// The session ended and will not come back on its own. `kind` is
    /// what the UI shows (a short localized sentence per kind);
    /// `detail` is the transport's own wording, for the log and the
    /// system chat line only — never the toast.
    Disconnected {
        kind: DisconnectKind,
        detail: String,
    },
    /// An established session (or an attempt) died for network
    /// reasons. Nothing about the credentials changed: the app should
    /// reconnect with the same parameters after a short backoff rather
    /// than send the user back to the connect screen. Same split:
    /// `kind` for the UI, `detail` for the log.
    ConnectionLost {
        kind: LossKind,
        detail: String,
    },
    /// The server refused our `Hello` for a wrong or missing room
    /// password. The UI prompts for it and offers to save it on the
    /// server entry.
    AccessDenied,
    InitClient {
        users: HashMap<ClientId, String>,
        client_id: u64,
        server_name: String,
        greeting: String,
        /// Voice channels this server defines, in id order.
        channels: Vec<ChannelInfo>,
        /// Which channel every connected user (including us) is in.
        memberships: HashMap<ClientId, u16>,
    },
    UserJoined {
        client_id: ClientId,
        username: String,
        channel_id: u16,
    },
    /// A user (possibly us — the echo is authoritative) moved channel.
    UserMovedChannel {
        client_id: ClientId,
        channel_id: u16,
    },
    /// The server refused our channel join.
    ChannelJoinDenied {
        channel_id: u16,
        reason: ChannelDenyReason,
    },
    /// The server hot-reloaded its channel list. Ids are renumbered, so
    /// this carries the full new list plus authoritative memberships.
    ChannelListUpdated {
        channels: Vec<ChannelInfo>,
        memberships: HashMap<ClientId, u16>,
    },
    UserLeft {
        client_id: ClientId,
        username: String,
    },
    TextMessage {
        username: String,
        text: String,
    },
    /// A chat image from a user (encoded PNG/JPEG/GIF bytes, ≤
    /// `protocol::MAX_CHAT_IMAGE_BYTES`, content hash verified).
    ImageMessage {
        username: String,
        data: Vec<u8>,
    },
    VoiceData {
        client_id: ClientId,
        seq: u32,
        timestamp: u32,
        data: Vec<u8>,
        /// `data` is a redundant audio bundle (`VOICE_FLAG_RED`), not a
        /// bare Opus packet.
        red: bool,
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
    /// Periodic link statistics for the UI. `loss_pct` is the
    /// transport's own accounting and informational only — the media
    /// paths get their loss from receiver reports and group resets.
    NetworkStats {
        rtt_ms: f64,
        /// Transport-level packet loss, 0.0–100.0 (percent).
        loss_pct: f64,
        /// Outbound throughput in kbit/s.
        kbps_up: f64,
        /// Inbound throughput in kbit/s.
        kbps_down: f64,
    },
    /// The relay's account of our voice over the last two seconds
    /// (routed to the voice thread, not app): the only loss figure the
    /// audio ladder acts on (spec §6.1).
    VoiceReceiverReport {
        /// Percent of expected packets that never arrived, clamped.
        loss_pct: u32,
        rtt_ms: u32,
        /// The window held enough packets to mean something.
        exercised: bool,
    },
    /// Our own video uplink as the transport measures it (routed to the
    /// screen thread, not app): the share of groups we had to abandon,
    /// which is what the bitrate ladder steps on.
    UplinkVideo {
        /// Groups reset over groups started in the window, percent.
        behind_pct: f32,
        rtt_ms: f32,
        kbps_up: f32,
    },

    // ── Screen share events ─────────────────────────────────────────
    /// A remote user started sharing their screen.
    ScreenShareStarted {
        client_id: ClientId,
        width: u32,
        height: u32,
        scope: ShareScope,
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
        /// Sender capture time, ms on the sender's clock (wraps).
        timestamp_ms: u32,
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
    /// (routed to screen thread, not app — shown in the sharer's stats).
    StreamHealth {
        worst_loss: f32,
        worst_rtt_ms: f32,
        viewers: u16,
    },
}

// ════════════════════════════════════════════════════════════════════════
//  NetworkClient — session state
// ════════════════════════════════════════════════════════════════════════

/// A relayed chat image being reassembled from paced chunks.
struct IncomingImage {
    count: u32,
    received: u32,
    bytes: usize,
    hash: [u8; 32],
    chunks: Vec<Option<Vec<u8>>>,
    started: Instant,
}

/// What we say once the transport is up.
struct PendingHello {
    username: String,
    password: Option<String>,
    client_key: Option<Arc<ClientKey>>,
}

/// Where the session is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// QUIC handshake in progress.
    Connecting,
    /// Transport up, `Hello` sent, waiting for the verdict.
    AwaitingWelcome,
    /// Admitted; the application protocol is live.
    Admitted,
    /// Over — the reason has been reported to the app.
    Closed,
}

/// Latest numbers the other threads handed us for the telemetry
/// sample; `None`/empty until they do.
#[derive(Default)]
struct TelemetryInputs {
    audio: Option<AudioSample>,
    sharing: Option<SharingSample>,
    viewing: Vec<ViewingSample>,
}

#[allow(clippy::struct_excessive_bools)] // independent session flags
pub(crate) struct NetworkClient {
    driver: Driver,
    udp: UdpIo,
    /// The server, as the driver names it.
    server: PeerId,
    phase: Phase,
    hello: Option<PendingHello>,
    /// The identity the server presented (once the handshake verified
    /// it).
    identity: Option<Observation>,
    client_info: ClientInfo,
    usernames: HashMap<ClientId, String>,
    message_queue: Vec<NetworkMessage>,
    reassembler: VideoReassembler,
    // Speaking status dedupe (only send on change)
    last_speaking_sent: bool,
    /// In-flight image upload, as the transport tracks it.
    upload: Option<UploadId>,
    /// Last percentage reported to the UI (255 = force first emit).
    upload_last_pct: u8,
    /// Reassembly of relayed images, keyed by (sender, upload id).
    incoming_images: HashMap<(u64, u32), IncomingImage>,
    last_stats_check: Instant,
    /// Transport counters at the last stats sample.
    stats_baseline: Option<LinkStats>,
    /// Transport counters at the last telemetry sample (its own
    /// baseline: the 2 s sampler advances `stats_baseline`, so sharing
    /// it under-reported the 10 s throughput ~5×).
    telemetry_baseline: Option<LinkStats>,
    /// Telemetry negotiated with the server.
    telemetry_enabled: bool,
    last_telemetry: Instant,
    telemetry: TelemetryInputs,
    last_log_upload: Option<Instant>,
    /// Frames dropped because no video group was open (delta frames
    /// before the first keyframe, or after a reset). Logged once per
    /// episode.
    video_no_group: bool,
    /// Keyframe requests we sent per sharer (viewing telemetry).
    keyframe_requests_sent: HashMap<ClientId, u64>,
    /// `DatagramsBlocked` reports (telemetry).
    datagrams_blocked: u64,
    /// The last receive pass stopped with datagrams still queued; the
    /// loop must come straight back rather than sleep.
    backlog: bool,
    /// When the last voice receiver report arrived (admission counts as
    /// the first), for the stall detector.
    last_report: Instant,
    /// A stall has been reported; cleared when reports resume.
    reports_stalled: bool,
}

impl NetworkClient {
    /// Bind a socket and start the QUIC handshake. Nothing blocks; the
    /// outcome arrives through [`Self::poll_messages`].
    ///
    /// # Errors
    /// Socket or TLS setup failure, preformatted for the error toast.
    pub fn connect(
        server_addr: SocketAddr,
        username: &str,
        password: Option<&str>,
        fingerprint: Option<Fingerprint>,
        client_key: Option<Arc<ClientKey>>,
        portable: bool,
    ) -> Result<Self, String> {
        let budget = Budget::client();
        let bind: SocketAddr = if server_addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("literal")
        } else {
            "[::]:0".parse().expect("literal")
        };
        let udp = UdpIo::bind(bind, budget.socket_buffer_bytes)
            .map_err(|e| format!("Failed to bind socket: {e}"))?;
        let mut driver = Driver::new_client(fingerprint, budget)
            .map_err(|e| format!("Failed to set up the transport: {e}"))?;
        driver.set_max_gso_segments(udp.max_gso_segments());
        let now = Instant::now();
        let server = driver
            .connect(now, server_addr)
            .map_err(|e| format!("Failed to connect: {e}"))?;
        info!(
            "Connecting to {server_addr} ({})",
            fingerprint.map_or_else(
                || "no pin — trust on first use".to_string(),
                |fp| format!("pinned {fp}")
            )
        );

        Ok(NetworkClient {
            driver,
            udp,
            server,
            phase: Phase::Connecting,
            hello: Some(PendingHello {
                username: username.to_string(),
                password: password.map(str::to_string),
                client_key,
            }),
            identity: None,
            client_info: ClientInfo {
                version: protocol::CLIENT_VERSION.to_string(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                portable,
            },
            usernames: HashMap::new(),
            message_queue: Vec::new(),
            reassembler: VideoReassembler::new(),
            last_speaking_sent: false,
            upload: None,
            upload_last_pct: 255,
            incoming_images: HashMap::new(),
            last_stats_check: now,
            stats_baseline: None,
            telemetry_baseline: None,
            telemetry_enabled: false,
            last_telemetry: now,
            telemetry: TelemetryInputs::default(),
            last_log_upload: None,
            video_no_group: false,
            keyframe_requests_sent: HashMap::new(),
            datagrams_blocked: 0,
            backlog: false,
            last_report: now,
            reports_stalled: false,
        })
    }

    // ── Tick ────────────────────────────────────────────────────────

    /// Receive, process, send. Cheap when nothing happened.
    pub fn update(&mut self) {
        if self.phase == Phase::Closed {
            return;
        }
        let now = Instant::now();
        if let Err(e) = self.udp.recv_batch(|d| self.driver.handle_datagram(now, d)) {
            warn!("UDP receive error: {e}");
        }
        self.driver.handle_timeouts(now);

        self.process_events(now);
        if self.phase != Phase::Closed {
            self.process_control_messages(now);
            self.process_voice_messages();
            self.process_video_messages(now);
            self.process_system_audio_messages();
            self.report_upload_progress();
            self.sample_link_stats(now);
            self.maybe_send_telemetry(now);
        }

        self.flush();
    }

    /// Push everything the driver produced onto the wire.
    fn flush(&mut self) {
        while let Some(t) = self.driver.poll_transmit() {
            self.udp.send(&t);
        }
    }

    /// The next moment the transport needs a tick, for the loop's sleep.
    pub fn next_timeout(&mut self) -> Option<Instant> {
        self.driver.next_timeout()
    }

    /// Datagrams were still queued on the socket after the last tick.
    pub fn has_backlog(&self) -> bool {
        self.backlog
    }

    fn process_events(&mut self, now: Instant) {
        let events: Vec<Event> = self.driver.drain_events().collect();
        for event in events {
            match event {
                Event::Connected { identity, .. } => {
                    self.identity = identity;
                    if let Some(Observation::Accepted { fingerprint, trust }) = &self.identity {
                        info!("Transport up; server identity {fingerprint} ({trust:?})");
                    }
                    self.send_hello(now);
                }
                Event::Disconnected { reason, .. } => {
                    self.on_disconnected(&reason);
                }
                Event::DatagramsBlocked { .. } => {
                    self.datagrams_blocked += 1;
                    if self.datagrams_blocked == 1 {
                        warn!("Voice datagram send buffer full; oldest packets are being dropped");
                        self.send_event(now, TelemetryEvent::SendBufferPressure);
                    }
                }
            }
        }
    }

    fn on_disconnected(&mut self, reason: &DisconnectReason) {
        if self.phase == Phase::Closed {
            return;
        }
        let was_connecting = matches!(self.phase, Phase::Connecting | Phase::AwaitingWelcome);
        let was_admitted = self.phase == Phase::Admitted;
        self.phase = Phase::Closed;
        // The server closes a connection whose Hello never arrived: on a
        // link losing most packets the handshake can squeak through
        // while the Hello stream data does not. A network condition,
        // not a refusal — retry like any other transient loss.
        let hello_lost = was_connecting
            && matches!(reason, DisconnectReason::ApplicationClosed { reason, .. } if reason == "no hello");
        match reason {
            _ if hello_lost => {
                warn!("Connection attempt failed: the server did not receive our Hello in time");
                self.message_queue.push(NetworkMessage::ConnectionLost {
                    kind: LossKind::Unreachable,
                    detail: "hello not received".to_string(),
                });
            }
            transient if transient.is_transient() && (was_admitted || was_connecting) => {
                let kind = match transient {
                    DisconnectReason::Reset => LossKind::Reset,
                    _ if was_connecting => LossKind::Unreachable,
                    _ => LossKind::Timeout,
                };
                warn!("Connection lost ({kind:?}): {transient}");
                self.message_queue.push(NetworkMessage::ConnectionLost {
                    kind,
                    detail: transient.to_string(),
                });
            }
            DisconnectReason::IdentityChanged { expected, observed } => {
                warn!("Server identity changed: pinned {expected}, presented {observed}");
                self.message_queue.push(NetworkMessage::IdentityChanged {
                    expected: expected.to_string(),
                    observed: observed.to_string(),
                });
            }
            DisconnectReason::ApplicationClosed { .. } if was_connecting => {
                // The server closed under a `Refused`; that verdict was
                // already reported (or never arrived, in which case say
                // so).
                if !self.message_queue.iter().any(|m| {
                    matches!(
                        m,
                        NetworkMessage::AccessDenied | NetworkMessage::Disconnected { .. }
                    )
                }) {
                    self.message_queue.push(NetworkMessage::Disconnected {
                        kind: DisconnectKind::Other,
                        detail: reason.to_string(),
                    });
                }
            }
            DisconnectReason::AlpnMismatch => {
                log::error!("Disconnected: {reason}");
                self.message_queue.push(NetworkMessage::Disconnected {
                    kind: DisconnectKind::Outdated,
                    detail: reason.to_string(),
                });
            }
            other => {
                log::error!("Disconnected: {other}");
                self.message_queue.push(NetworkMessage::Disconnected {
                    kind: DisconnectKind::Other,
                    detail: other.to_string(),
                });
            }
        }
    }

    /// Say who we are, once the transport is up.
    fn send_hello(&mut self, now: Instant) {
        let Some(hello) = self.hello.take() else {
            return;
        };
        let client_key = hello.client_key.as_ref().and_then(|key| {
            let mut exported = [0u8; CLIENT_AUTH_EXPORTER_BYTES];
            self.driver
                .export_keying_material(self.server, CLIENT_AUTH_EXPORTER_LABEL, b"", &mut exported)
                .map(|()| ClientKeyProof {
                    public_key: key.public_key(),
                    signature: key.sign(&exported),
                })
        });
        let msg = ClientMessage::Hello {
            username: hello.username,
            auth: Auth::Shared {
                password: hello.password,
            },
            client_key,
            features: features::CLIENT_1_0,
            client_version: protocol::CLIENT_VERSION.to_string(),
        };
        self.send_control(now, &msg);
        self.phase = Phase::AwaitingWelcome;
    }

    // ── Link statistics & telemetry ─────────────────────────────────

    /// Sample transport statistics every ~2 seconds: the UI overlay
    /// (path RTT/loss/throughput) and the screen thread's bitrate ladder
    /// (the share of video groups the transport abandoned).
    ///
    /// Samples at or above [`LINK_LOG_LOSS_PCT`] are also written to the
    /// log. Loss here is the transport's own accounting (packets it
    /// declared lost); the media paths never act on it.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn sample_link_stats(&mut self, now: Instant) {
        if now.duration_since(self.last_stats_check) < NETWORK_STATS_INTERVAL {
            return;
        }
        let interval = now.duration_since(self.last_stats_check).as_secs_f64();
        self.last_stats_check = now;
        if self.phase != Phase::Admitted {
            return;
        }
        let Some(stats) = self.driver.stats(self.server) else {
            return;
        };
        let base = self.stats_baseline.unwrap_or_default();
        self.stats_baseline = Some(stats);
        let rtt_ms = stats.rtt.as_secs_f64() * 1000.0;
        let loss_pct = stats.path_loss_pct_since(&base);
        let kbps_up =
            (stats.bytes_tx.saturating_sub(base.bytes_tx)) as f64 * 8.0 / 1000.0 / interval;
        let kbps_down =
            (stats.bytes_rx.saturating_sub(base.bytes_rx)) as f64 * 8.0 / 1000.0 / interval;
        let streaming = kbps_up >= LINK_LOG_STREAMING_KBPS;
        if loss_pct >= LINK_LOG_LOSS_PCT || (streaming && loss_pct >= LINK_LOG_LOSS_PCT_STREAMING) {
            info!(
                "link: path loss {loss_pct:.1}%, rtt {rtt_ms:.0} ms, cwnd {} KB, up {kbps_up:.0} kbit/s, down {kbps_down:.0} kbit/s, dgram drops {}",
                stats.cwnd / 1024,
                stats.datagram_send_drops - base.datagram_send_drops,
            );
        }
        self.message_queue.push(NetworkMessage::NetworkStats {
            rtt_ms,
            loss_pct,
            kbps_up,
            kbps_down,
        });

        // No receiver report for a while: the control stream is stalled
        // under loss. Feed the ladder a "bad link" reading so it holds
        // its lowest rung instead of decaying to clean and climbing.
        let since_report = now.duration_since(self.last_report);
        if since_report >= REPORT_STALL {
            if !self.reports_stalled {
                warn!(
                    "No receiver report for {:.0} s (path loss {loss_pct:.0}%); holding the audio ladder down",
                    since_report.as_secs_f64()
                );
                self.reports_stalled = true;
            }
            #[allow(clippy::cast_sign_loss)] // both non-negative by construction
            self.message_queue
                .push(NetworkMessage::VoiceReceiverReport {
                    loss_pct: UPLINK_LOSS_CLAMP_PCT as u32,
                    rtt_ms: rtt_ms.max(0.0) as u32,
                    exercised: true,
                });
        }

        // The sharer's leg, measured on the video itself: only groups
        // *we* abandoned because our uplink was behind.
        let started = stats.groups_started.saturating_sub(base.groups_started);
        let reset = stats.groups_reset.saturating_sub(base.groups_reset);
        if reset > 0 {
            // The evidence for tuning the group deadlines: which rule
            // abandoned our groups, per 2 s window.
            info!(
                "video uplink: abandoned {reset}/{started} groups (superseded {}, deadline {}, unacked {}), rtt {rtt_ms:.0} ms, path loss {loss_pct:.1}%, up {kbps_up:.0} kbit/s",
                stats.groups_reset_superseded - base.groups_reset_superseded,
                stats.groups_reset_deadline - base.groups_reset_deadline,
                stats.groups_reset_unacked - base.groups_reset_unacked,
            );
        }
        if started > 0 || reset > 0 {
            let behind_pct = if started == 0 {
                100.0
            } else {
                (reset as f32 * 100.0 / started as f32).min(100.0)
            };
            self.message_queue.push(NetworkMessage::UplinkVideo {
                behind_pct,
                rtt_ms: rtt_ms as f32,
                kbps_up: kbps_up as f32,
            });
        }
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn maybe_send_telemetry(&mut self, now: Instant) {
        if !self.telemetry_enabled
            || self.phase != Phase::Admitted
            || now.duration_since(self.last_telemetry) < TELEMETRY_INTERVAL
        {
            return;
        }
        let interval = now.duration_since(self.last_telemetry).as_secs_f64();
        self.last_telemetry = now;
        let Some(stats) = self.driver.stats(self.server) else {
            return;
        };
        let base = self.telemetry_baseline.unwrap_or_default();
        self.telemetry_baseline = Some(stats);
        let path = PathSample {
            rtt_ms: stats.rtt.as_millis().min(u128::from(u32::MAX)) as u32,
            loss_pct: stats.path_loss_pct_since(&base) as f32,
            cwnd: stats.cwnd,
            bytes_in_flight: 0,
            kbps_up: (stats.bytes_tx.saturating_sub(base.bytes_tx)) as f32 * 8.0
                / 1000.0
                / interval as f32,
            kbps_down: (stats.bytes_rx.saturating_sub(base.bytes_rx)) as f32 * 8.0
                / 1000.0
                / interval as f32,
            datagram_send_drops: stats.datagram_send_drops,
            datagrams_blocked: self.datagrams_blocked,
        };
        let mut sharing = self.telemetry.sharing.clone();
        if let Some(s) = sharing.as_mut() {
            s.groups_reset = stats.groups_reset;
        }
        let sample = TelemetrySample {
            client: self.client_info.clone(),
            path,
            audio: self.telemetry.audio.clone(),
            sharing,
            viewing: self.telemetry.viewing.clone(),
        };
        self.send_control(now, &ClientMessage::Telemetry(Box::new(sample)));
    }

    /// Send a telemetry event now (a handful per session).
    pub fn send_event(&mut self, now: Instant, event: TelemetryEvent) {
        if self.telemetry_enabled && self.phase == Phase::Admitted {
            self.send_control(now, &ClientMessage::TelemetryEvent(event));
        }
    }

    /// Latest audio counters for the next telemetry sample.
    pub fn set_audio_sample(&mut self, sample: AudioSample) {
        self.telemetry.audio = Some(sample);
    }

    /// Latest sharing counters (`None` once the share stops).
    pub fn set_sharing_sample(&mut self, sample: Option<SharingSample>) {
        self.telemetry.sharing = sample;
    }

    /// Latest per-stream viewing counters.
    pub fn set_viewing_samples(&mut self, mut samples: Vec<ViewingSample>) {
        for s in &mut samples {
            s.keyframe_requests_sent = self
                .keyframe_requests_sent
                .get(&s.sharer)
                .copied()
                .unwrap_or(0);
        }
        self.telemetry.viewing = samples;
    }

    // ── Control lanes (control + signal + media) ────────────────────

    fn process_control_messages(&mut self, now: Instant) {
        for lane in [Lane::Control, Lane::Signal, Lane::Media] {
            while let Some(message) = self.driver.recv(self.server, lane) {
                let Some(msg) = protocol::decode_server_message(&message) else {
                    continue;
                };
                self.handle_server_message(now, msg);
                if self.phase == Phase::Closed {
                    return;
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_server_message(&mut self, now: Instant, msg: ServerMessage) {
        match msg {
            ServerMessage::Welcome {
                client_id,
                user_id: _,
                features: server_features,
            } => {
                if self.phase != Phase::AwaitingWelcome {
                    return;
                }
                self.phase = Phase::Admitted;
                self.telemetry_enabled = features::has(
                    features::negotiate(features::CLIENT_1_0, server_features),
                    features::TELEMETRY,
                );
                self.last_telemetry = now;
                self.last_report = now;
                self.stats_baseline = self.driver.stats(self.server);
                self.telemetry_baseline = self.stats_baseline;
                info!(
                    "Admitted as {client_id:#x} (server features {server_features:#x}, telemetry {})",
                    if self.telemetry_enabled { "on" } else { "off" }
                );
                if let Some(Observation::Accepted { fingerprint, trust }) = &self.identity {
                    let first_use = matches!(trust, Trust::FirstUse | Trust::Successor { .. });
                    if let Trust::Successor { from } = trust {
                        info!("Server rotated its key: {from} → {fingerprint}; updating the pin");
                    }
                    self.message_queue.push(NetworkMessage::ServerIdentity {
                        fingerprint: fingerprint.to_string(),
                        first_use,
                    });
                }
                self.message_queue.push(NetworkMessage::Connected);
            }
            ServerMessage::Refused { reason } => {
                warn!("Server refused us: {}", reason.describe());
                self.phase = Phase::Closed;
                match reason {
                    RefusedReason::WrongPassword => {
                        self.message_queue.push(NetworkMessage::AccessDenied);
                    }
                    other => self.message_queue.push(NetworkMessage::Disconnected {
                        kind: DisconnectKind::Refused(other),
                        detail: other.describe().to_string(),
                    }),
                }
                self.driver.close(now, self.server, "refused");
            }
            ServerMessage::Init {
                usernames,
                client_id,
                server_name,
                greeting,
                channels,
                memberships,
            } => {
                self.usernames.clone_from(&usernames);
                self.message_queue.push(NetworkMessage::InitClient {
                    client_id,
                    users: usernames,
                    server_name,
                    greeting,
                    channels,
                    memberships,
                });
            }
            ServerMessage::UserJoined {
                client_id,
                username,
                channel_id,
            } => {
                self.usernames.insert(client_id, username.clone());
                self.message_queue.push(NetworkMessage::UserJoined {
                    client_id,
                    username,
                    channel_id,
                });
            }
            ServerMessage::UserMovedChannel {
                client_id,
                channel_id,
            } => {
                self.message_queue.push(NetworkMessage::UserMovedChannel {
                    client_id,
                    channel_id,
                });
            }
            ServerMessage::JoinChannelDenied { channel_id, reason } => {
                warn!(
                    "Server denied joining channel {channel_id}: {}",
                    reason.describe()
                );
                self.message_queue
                    .push(NetworkMessage::ChannelJoinDenied { channel_id, reason });
            }
            ServerMessage::ChannelListUpdated {
                channels,
                memberships,
            } => {
                self.message_queue.push(NetworkMessage::ChannelListUpdated {
                    channels,
                    memberships,
                });
            }
            ServerMessage::UserLeft { client_id } => {
                let username = self
                    .usernames
                    .remove(&client_id)
                    .unwrap_or_else(|| format!("User{client_id:x}"));
                self.message_queue.push(NetworkMessage::UserLeft {
                    client_id,
                    username,
                });
            }
            ServerMessage::Chat(msg) => {
                let username = self.username_of(msg.client_id);
                self.message_queue.push(NetworkMessage::TextMessage {
                    username,
                    text: msg.text,
                });
            }
            ServerMessage::ChatImageChunk {
                client_id,
                upload_id,
                index,
                count,
                hash,
                data,
                ..
            } => {
                if let Some(image) =
                    self.accept_image_chunk(now, client_id, upload_id, index, count, hash, data)
                {
                    let username = self.username_of(client_id);
                    self.message_queue.push(NetworkMessage::ImageMessage {
                        username,
                        data: image,
                    });
                }
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
                scope,
            } => {
                self.message_queue.push(NetworkMessage::ScreenShareStarted {
                    client_id,
                    width: u32::from(width),
                    height: u32::from(height),
                    scope,
                });
            }
            ServerMessage::ScreenShareStopped { client_id } => {
                // Forget any partial frames from that stream.
                self.reassembler.forget_sender(client_id);
                self.keyframe_requests_sent.remove(&client_id);
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
                    "Server denied subscribe to {target_id:#x}: {}",
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
            }
            ServerMessage::VoiceReceiverReport {
                received: _,
                expected,
                lost,
                late: _,
                rtt_ms,
            } => {
                self.last_report = now;
                if self.reports_stalled {
                    info!("Receiver reports resumed");
                    self.reports_stalled = false;
                }
                let exercised = expected >= VOICE_REPORT_MIN_PACKETS;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let loss_pct = if expected == 0 {
                    0
                } else {
                    (f64::from(lost) * 100.0 / f64::from(expected))
                        .round()
                        .min(UPLINK_LOSS_CLAMP_PCT) as u32
                };
                self.message_queue
                    .push(NetworkMessage::VoiceReceiverReport {
                        loss_pct,
                        rtt_ms,
                        exercised,
                    });
            } // No catch-all: the match is exhaustive on purpose, so a
              // new ServerMessage variant is a compile error here (a
              // reminder to handle it) instead of a silent log line.
              // Truly unknown wire data never reaches this match — it
              // fails deserialization above and is skipped.
        }
    }

    fn username_of(&self, client_id: ClientId) -> String {
        self.usernames
            .get(&client_id)
            .cloned()
            .unwrap_or_else(|| format!("User{client_id:x}"))
    }

    // ── Voice lane ──────────────────────────────────────────────────

    fn process_voice_messages(&mut self) {
        while let Some(message) = self.driver.recv(self.server, Lane::Voice) {
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
                red: header.is_red(),
            });
        }
    }

    // ── Video lane ──────────────────────────────────────────────────

    /// Groups arrive per sharer (the track). A reset group means the
    /// relay abandoned it for us — we were behind — so any partial
    /// frame of that sharer is dropped; the next group starts with a
    /// keyframe.
    fn process_video_messages(&mut self, now: Instant) {
        while let Some((track, item)) = self.driver.recv_video(self.server) {
            match item {
                VideoItem::GroupStart | VideoItem::GroupEnd => {}
                VideoItem::GroupReset => {
                    self.reassembler.forget_sender(track);
                    log::debug!("video group from {track:#x} reset by the relay (we were behind)");
                    // The relay gave up on that group; a keyframe is
                    // what gets us back in.
                    self.send_request_keyframe(now, track);
                }
                VideoItem::Chunk(message) => {
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
                            timestamp_ms: frame.timestamp_ms,
                            data: frame.data,
                        });
                    }
                }
            }
        }
    }

    // ── System audio lane ───────────────────────────────────────────

    fn process_system_audio_messages(&mut self) {
        while let Some(message) = self.driver.recv(self.server, Lane::SystemAudio) {
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

    fn send_control(&mut self, now: Instant, msg: &ClientMessage) {
        debug_assert!(!msg.is_upload_chunk(), "uploads go through send_media");
        match protocol::encode_client_message(msg) {
            Some(encoded) => {
                if let Err(e) = self.driver.send(now, self.server, msg.lane(), &encoded) {
                    log::debug!("send on {} failed: {e}", msg.lane());
                }
            }
            None => warn!("Failed to serialize client message"),
        }
    }

    pub fn send_message(&mut self, now: Instant, text: String) {
        self.send_control(now, &ClientMessage::Chat(text));
    }

    /// Queue a chat image for a *paced* upload: split into
    /// [`protocol::IMAGE_CHUNK_BYTES`] chunks on one Media stream, which
    /// the transport trickles out at [`protocol::MEDIA_UPLOAD_PACE`] so
    /// the router queue voice shares never backs up behind bulk. A new
    /// image replaces any upload still in flight (matching the UI,
    /// which sends one at a time).
    pub fn send_image(&mut self, now: Instant, data: &[u8]) {
        if data.len() > protocol::MAX_CHAT_IMAGE_BYTES {
            warn!("Refusing oversized image ({} KB)", data.len() / 1024);
            return;
        }
        let Some(frames) = chunk_upload(data, 0, |upload_id, index, count, chunk| {
            ClientMessage::ChatImageChunk {
                upload_id,
                index,
                count,
                data: chunk,
            }
        }) else {
            return;
        };
        match self.driver.send_media(now, self.server, frames) {
            Ok(id) => {
                self.upload = Some(id);
                self.upload_last_pct = 255;
                self.report_upload_progress();
            }
            Err(e) => warn!("Image upload could not start: {e}"),
        }
    }

    /// Upload our log file to the server (spec §9.1), at most once per
    /// ten minutes. The bytes are the tail the UI decided to send, ≤
    /// [`protocol::MAX_LOG_UPLOAD_BYTES`].
    pub fn send_log(&mut self, now: Instant, data: &[u8]) {
        if self
            .last_log_upload
            .is_some_and(|t| now.duration_since(t) < LOG_UPLOAD_MIN_INTERVAL)
        {
            warn!("Log upload skipped: one per ten minutes");
            return;
        }
        let tail = if data.len() > protocol::MAX_LOG_UPLOAD_BYTES {
            &data[data.len() - protocol::MAX_LOG_UPLOAD_BYTES..]
        } else {
            data
        };
        let Some(frames) = chunk_upload(tail, 1, |upload_id, index, count, chunk| {
            ClientMessage::LogUploadChunk {
                upload_id,
                index,
                count,
                data: chunk,
            }
        }) else {
            return;
        };
        match self.driver.send_media(now, self.server, frames) {
            Ok(_) => {
                self.last_log_upload = Some(now);
                info!("Uploading {} KB of log to the server", tail.len() / 1024);
            }
            Err(e) => warn!("Log upload could not start: {e}"),
        }
    }

    /// Report image upload progress as the transport takes the bytes.
    fn report_upload_progress(&mut self) {
        let Some(id) = self.upload else {
            return;
        };
        let (pct, done) = match self.driver.upload_progress(self.server, id) {
            #[allow(clippy::cast_possible_truncation)]
            Some((written, total)) if total > 0 => {
                ((written * 100 / total) as u8, written >= total)
            }
            // Retired by the transport: fully acknowledged.
            _ => (100, true),
        };
        if pct != self.upload_last_pct {
            self.upload_last_pct = pct;
            self.message_queue
                .push(NetworkMessage::UploadProgress { pct });
        }
        if done {
            self.upload = None;
        }
    }

    /// Fold one relayed chunk into its image; `Some(bytes)` when the
    /// image completes and its content hash checks out. Bounds mirror
    /// the codec's: chunk counts and total size are capped, a fresh
    /// `upload_id` from the same sender abandons the old partial, and
    /// partials older than two minutes are pruned.
    #[allow(clippy::too_many_arguments)]
    fn accept_image_chunk(
        &mut self,
        now: Instant,
        client_id: u64,
        upload_id: u32,
        index: u32,
        count: u32,
        hash: [u8; 32],
        data: Vec<u8>,
    ) -> Option<Vec<u8>> {
        if count == 0 || count > protocol::MAX_IMAGE_CHUNKS || index >= count {
            return None;
        }
        self.incoming_images
            .retain(|_, img| now.duration_since(img.started) < IMAGE_PARTIAL_TTL);
        // A newer upload from the same sender obsoletes any partial.
        self.incoming_images
            .retain(|(sender, id), _| *sender != client_id || *id == upload_id);

        let entry = self
            .incoming_images
            .entry((client_id, upload_id))
            .or_insert_with(|| IncomingImage {
                count,
                received: 0,
                bytes: 0,
                hash,
                chunks: vec![None; count as usize],
                started: now,
            });
        if entry.count != count || entry.hash != hash || entry.chunks[index as usize].is_some() {
            return None; // inconsistent or duplicate — drop
        }
        entry.bytes += data.len();
        if entry.bytes > protocol::MAX_CHAT_IMAGE_BYTES {
            self.incoming_images.remove(&(client_id, upload_id));
            return None;
        }
        entry.chunks[index as usize] = Some(data);
        entry.received += 1;
        if entry.received < entry.count {
            return None;
        }
        let entry = self.incoming_images.remove(&(client_id, upload_id))?;
        let mut image = Vec::with_capacity(entry.bytes);
        for chunk in entry.chunks {
            image.extend_from_slice(&chunk?);
        }
        let digest: [u8; 32] = Sha256::digest(&image).into();
        if digest != entry.hash {
            warn!("Relayed image from {client_id:#x} failed its content hash; dropped");
            return None;
        }
        Some(image)
    }

    pub fn send_speaking_status(&mut self, now: Instant, is_speaking: bool) {
        // Edge-triggered by the voice thread; dedupe defensively in case
        // the same state is ever delivered twice.
        if is_speaking == self.last_speaking_sent {
            return;
        }
        self.send_control(now, &ClientMessage::Speaking(is_speaking));
        self.last_speaking_sent = is_speaking;
    }

    /// Broadcast our mute/deafen state to all other users via the server.
    pub fn send_audio_state(&mut self, now: Instant, muted: bool, deafened: bool) {
        self.send_control(now, &ClientMessage::AudioState { muted, deafened });
    }

    /// Send one voice packet: a bare Opus frame (`flags == 0`) or a
    /// redundant audio bundle (`VOICE_FLAG_RED`). One datagram per audio
    /// packet — no bundling of *frames* here, no added latency (see
    /// `protocol::voice`); RED bundles carry copies of packets already
    /// sent, which costs bytes, not delay.
    pub fn send_voice_frame(
        &mut self,
        now: Instant,
        seq: u32,
        ts_48k: u32,
        flags: u8,
        payload: &[u8],
    ) {
        match pack_voice_with_flags(seq, ts_48k, flags, payload) {
            Some(packet) => {
                if let Err(e) = self.driver.send(now, self.server, Lane::Voice, &packet) {
                    log::debug!("voice packet seq={seq} not sent: {e}");
                }
            }
            None => log::debug!(
                "Dropping voice packet seq={seq}: invalid size {} bytes",
                payload.len()
            ),
        }
    }

    /// Chunk and send an encoded H.264 frame. A keyframe opens a new
    /// group (spec §6.2) — the previous one is finished, or reset if
    /// the uplink still had bytes of it waiting. Delta frames ride the
    /// open group; with none open (nothing since the last reset) they
    /// are dropped, since without their keyframe they decode to noise.
    #[allow(clippy::too_many_arguments)]
    pub fn send_video_frame(
        &mut self,
        now: Instant,
        seq: u32,
        is_keyframe: bool,
        width: u16,
        height: u16,
        timestamp_ms: u32,
        data: &[u8],
    ) {
        if is_keyframe {
            self.driver.video_new_group(now, self.server, OWN_TRACK);
        }
        if !self.driver.video_group_open(self.server, OWN_TRACK) {
            if !self.video_no_group {
                warn!(
                    "Video frame seq={seq} dropped: the current group was abandoned (uplink behind) — \
                     waiting for the next keyframe"
                );
                self.video_no_group = true;
            }
            return;
        }
        if self.video_no_group {
            info!("Video resumed at keyframe seq={seq}");
            self.video_no_group = false;
        }

        // `pack_video_frame` refuses empty or oversize frames (past the
        // chunk format's 255 × 1080 B limit). The encoder's VBV keeps
        // frames well under it, but a misbehaving codec must not kill
        // the stream — drop the frame; the viewer sees a seq gap and
        // recovers via keyframe request.
        match pack_video_frame(seq, is_keyframe, width, height, timestamp_ms, data) {
            Some(chunks) => {
                for chunk in chunks {
                    let _ = self.driver.send(now, self.server, Lane::Video, &chunk);
                }
            }
            None => warn!(
                "Encoded frame seq={seq} not packable ({} bytes, max {}), dropping",
                data.len(),
                protocol::MAX_VIDEO_FRAME_BYTES,
            ),
        }
    }

    /// Send an encoded system audio (Opus stereo) packet.
    pub fn send_system_audio(
        &mut self,
        now: Instant,
        seq: u32,
        channels: u8,
        duration_ms: u16,
        opus: &[u8],
    ) {
        match pack_system_audio(seq, channels, duration_ms, opus) {
            Some(packet) => {
                let _ = self
                    .driver
                    .send(now, self.server, Lane::SystemAudio, &packet);
            }
            None => warn!(
                "System audio packet not packable ({} bytes), dropping",
                opus.len()
            ),
        }
    }

    /// Send screen share signaling. Dimensions are clamped into the
    /// wire format's u16 range (the server validates 16..=4096 anyway).
    pub fn send_screen_share_start(
        &mut self,
        now: Instant,
        width: u32,
        height: u32,
        scope: ShareScope,
    ) {
        let width = u16::try_from(width.min(4096)).unwrap_or(u16::MAX);
        let height = u16::try_from(height.min(4096)).unwrap_or(u16::MAX);
        self.send_control(
            now,
            &ClientMessage::ScreenShareStart {
                width,
                height,
                scope,
            },
        );
    }

    /// Ask to move to `channel_id` (with the channel password, if any).
    pub fn send_join_channel(&mut self, now: Instant, channel_id: u16, password: Option<String>) {
        self.send_control(
            now,
            &ClientMessage::JoinChannel {
                channel_id,
                password,
            },
        );
    }

    /// Stop sharing: close the open group cleanly, then tell the room.
    pub fn send_screen_share_stop(&mut self, now: Instant) {
        self.driver.video_finish_group(now, self.server, OWN_TRACK);
        self.video_no_group = false;
        self.send_control(now, &ClientMessage::ScreenShareStop);
    }

    pub fn send_subscribe_screen(&mut self, now: Instant, target_id: u64) {
        self.send_control(now, &ClientMessage::SubscribeScreen { target_id });
    }

    pub fn send_unsubscribe_screen(&mut self, now: Instant, target_id: u64) {
        self.reassembler.forget_sender(target_id);
        self.send_control(now, &ClientMessage::UnsubscribeScreen { target_id });
    }

    /// Ask the server to relay a keyframe request to the user we're
    /// watching. Travels on the signal lane: reliable, but never queued
    /// behind chat or presence traffic.
    pub fn send_request_keyframe(&mut self, now: Instant, target_id: u64) {
        *self.keyframe_requests_sent.entry(target_id).or_insert(0) += 1;
        self.send_control(now, &ClientMessage::RequestKeyframe { target_id });
    }

    pub fn poll_messages(&mut self) -> Vec<NetworkMessage> {
        std::mem::take(&mut self.message_queue)
    }

    /// The session is over and its reason has been reported.
    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// Close the connection and push the close packet out.
    pub fn disconnect(&mut self) {
        let now = Instant::now();
        if self.phase != Phase::Closed {
            self.driver.video_finish_group(now, self.server, OWN_TRACK);
            self.driver.close(now, self.server, "bye");
            self.phase = Phase::Closed;
        }
        self.flush();
    }
}

/// Split `data` into one `Media` frame per [`protocol::IMAGE_CHUNK_BYTES`]
/// chunk, each an encoded `build` message. `upload_id` is per kind so
/// the server tells an image from a log.
fn chunk_upload(
    data: &[u8],
    upload_id: u32,
    build: impl Fn(u32, u32, u32, Vec<u8>) -> ClientMessage,
) -> Option<Vec<protocol::Bytes>> {
    let chunks: Vec<&[u8]> = data.chunks(protocol::IMAGE_CHUNK_BYTES).collect();
    if chunks.is_empty() {
        return None;
    }
    let count = u32::try_from(chunks.len()).ok()?;
    let mut frames = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.into_iter().enumerate() {
        let index = u32::try_from(i).ok()?;
        let msg = build(upload_id, index, count, chunk.to_vec());
        let encoded = protocol::encode_client_message(&msg)?;
        frames.push(protocol::Bytes::from(encoded));
    }
    Some(frames)
}

// ════════════════════════════════════════════════════════════════════════
//  Network control messages (UI thread → network thread)
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum NetworkControl {
    Connect {
        addr: SocketAddr,
        nickname: String,
        /// The room password, for servers running `secure: true`.
        password: Option<String>,
        /// The pinned server identity (`SHA256:…`); `None` = trust on
        /// first use. An unparsable value is treated as `None`.
        fingerprint: Option<String>,
        /// Our own identity key, if the app could load one.
        client_key: Option<Arc<ClientKey>>,
        /// Settings live next to the executable (telemetry).
        portable: bool,
    },
    Disconnect,
    SendMessage(String),
    /// Send a chat image (encoded PNG; the UI caps it at
    /// `protocol::MAX_CHAT_IMAGE_BYTES` before it gets here).
    SendImage(Vec<u8>),
    /// Upload our log to the server (the tail, ≤ 2 MiB).
    SendLog(Vec<u8>),
    SendVoicePacket(AudioPacket),
    SendSpeakingStatus(bool),
    /// Broadcast mute/deafen state to other users.
    SendAudioState {
        muted: bool,
        deafened: bool,
    },

    // ── Voice channels ──────────────────────────────────────────────
    /// Move to another voice channel (answered by `UserMovedChannel`
    /// or `ChannelJoinDenied`).
    JoinChannel {
        channel_id: u16,
        password: Option<String>,
    },

    // ── Screen share ────────────────────────────────────────────────
    /// Signal that we started sharing (control → server → the audience
    /// the scope allows).
    ScreenShareStart {
        width: u32,
        height: u32,
        scope: ShareScope,
    },
    /// Signal that we stopped sharing.
    ScreenShareStop,
    /// Send an encoded H.264 frame on the video lane.
    SendVideoFrame {
        seq: u32,
        is_keyframe: bool,
        width: u16,
        height: u16,
        /// Capture time, ms (wraps at u32; viewers unwrap it).
        timestamp_ms: u32,
        data: Vec<u8>,
    },
    /// Send an Opus stereo system audio packet on the system audio lane.
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

    // ── Telemetry (spec §9.1) ───────────────────────────────────────
    /// A notable moment, forwarded to the server immediately.
    TelemetryEvent(TelemetryEvent),
    /// Latest audio counters for the periodic sample.
    TelemetryAudio(AudioSample),
    /// Latest sharing counters (`None` when not sharing).
    TelemetrySharing(Option<SharingSample>),
    /// Latest per-stream viewing counters.
    TelemetryViewing(Vec<ViewingSample>),

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
    ///
    /// # Panics
    /// If the OS refuses to create the thread.
    pub fn spawn(
        tx: Sender<NetworkControl>,
        control_rx: Receiver<NetworkControl>,
        msg_tx: Sender<NetworkMessage>,
        msg_rx: Receiver<NetworkMessage>,
        voice_tx: Sender<VoiceControl>,
        screen_tx: Sender<ScreenControl>,
        wake: WakeFn,
    ) -> Self {
        let join_handle = thread::Builder::new()
            .name("client-network".into())
            .spawn(move || {
                run_network_loop(&control_rx, &msg_tx, &voice_tx, &screen_tx, &wake);
            })
            .expect("failed to spawn network thread");

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
#[allow(clippy::too_many_lines)]
fn handle_control(
    ctrl: NetworkControl,
    client: &mut Option<NetworkClient>,
    msg_tx: &Sender<NetworkMessage>,
    wake: &WakeFn,
) -> bool {
    let now = Instant::now();
    match ctrl {
        NetworkControl::Connect {
            addr,
            nickname,
            password,
            fingerprint,
            client_key,
            portable,
        } => {
            // Replace any existing session cleanly first.
            if let Some(mut old) = client.take() {
                old.disconnect();
            }
            let pin = fingerprint.as_deref().and_then(|fp| {
                let parsed = fp.parse::<Fingerprint>().ok();
                if parsed.is_none() {
                    warn!("Ignoring unparsable server fingerprint '{fp}'");
                }
                parsed
            });
            match NetworkClient::connect(
                addr,
                &nickname,
                password.as_deref(),
                pin,
                client_key,
                portable,
            ) {
                // Connected is announced once the server admits us.
                Ok(c) => *client = Some(c),
                Err(e) => {
                    msg_tx
                        .send(NetworkMessage::Disconnected {
                            kind: DisconnectKind::LocalError,
                            detail: e,
                        })
                        .ok();
                    wake();
                }
            }
        }
        NetworkControl::Disconnect => {
            if let Some(mut c) = client.take() {
                c.disconnect();
            }
            msg_tx
                .send(NetworkMessage::Disconnected {
                    kind: DisconnectKind::UserRequested,
                    detail: "user disconnected".into(),
                })
                .ok();
            wake();
        }
        NetworkControl::SendMessage(text) => {
            if let Some(c) = client {
                c.send_message(now, text);
            }
        }
        NetworkControl::SendImage(data) => {
            if let Some(c) = client {
                c.send_image(now, &data);
            }
        }
        NetworkControl::SendLog(data) => {
            if let Some(c) = client {
                c.send_log(now, &data);
            }
        }
        NetworkControl::SendVoicePacket(packet) => {
            if let Some(c) = client {
                // The packet's ssrc is not sent — the server stamps the
                // authenticated sender id on relay.
                let flags = match packet.header.payload_type {
                    PayloadType::Red => VOICE_FLAG_RED,
                    PayloadType::Opus => 0,
                };
                c.send_voice_frame(
                    now,
                    packet.header.seq,
                    packet.header.timestamp,
                    flags,
                    &packet.opus_data,
                );
            }
        }
        NetworkControl::SendSpeakingStatus(speaking) => {
            if let Some(c) = client {
                c.send_speaking_status(now, speaking);
            }
        }
        NetworkControl::SendAudioState { muted, deafened } => {
            if let Some(c) = client {
                c.send_audio_state(now, muted, deafened);
            }
        }

        NetworkControl::JoinChannel {
            channel_id,
            password,
        } => {
            if let Some(c) = client {
                c.send_join_channel(now, channel_id, password);
            }
        }

        // ── Screen share controls ───────────────────────────────────
        NetworkControl::ScreenShareStart {
            width,
            height,
            scope,
        } => {
            if let Some(c) = client {
                c.send_screen_share_start(now, width, height, scope);
            }
        }
        NetworkControl::ScreenShareStop => {
            if let Some(c) = client {
                c.send_screen_share_stop(now);
            }
        }
        NetworkControl::SendVideoFrame {
            seq,
            is_keyframe,
            width,
            height,
            timestamp_ms,
            data,
        } => {
            if let Some(c) = client {
                c.send_video_frame(now, seq, is_keyframe, width, height, timestamp_ms, &data);
            }
        }
        NetworkControl::SendSystemAudio {
            seq,
            channels,
            duration_ms,
            data,
        } => {
            if let Some(c) = client {
                c.send_system_audio(now, seq, channels, duration_ms, &data);
            }
        }
        NetworkControl::RequestKeyframe { target_id } => {
            if let Some(c) = client {
                c.send_request_keyframe(now, target_id);
            }
        }
        NetworkControl::SubscribeScreen { target_id } => {
            if let Some(c) = client {
                c.send_subscribe_screen(now, target_id);
            }
        }
        NetworkControl::UnsubscribeScreen { target_id } => {
            if let Some(c) = client {
                c.send_unsubscribe_screen(now, target_id);
            }
        }

        NetworkControl::TelemetryEvent(event) => {
            if let Some(c) = client {
                c.send_event(now, event);
            }
        }
        NetworkControl::TelemetryAudio(sample) => {
            if let Some(c) = client {
                c.set_audio_sample(sample);
            }
        }
        NetworkControl::TelemetrySharing(sample) => {
            if let Some(c) = client {
                c.set_sharing_sample(sample);
            }
        }
        NetworkControl::TelemetryViewing(samples) => {
            if let Some(c) = client {
                c.set_viewing_samples(samples);
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

/// Route one message from the session to whoever consumes it: media
/// and link feedback to the voice/screen threads (hot paths, bypassing
/// the app), everything else to the app.
#[allow(clippy::too_many_lines)]
fn route_message(
    msg: NetworkMessage,
    msg_tx: &Sender<NetworkMessage>,
    voice_tx: &Sender<VoiceControl>,
    screen_tx: &Sender<ScreenControl>,
    wake: &WakeFn,
) {
    match msg {
        // Voice → voice thread
        NetworkMessage::VoiceData {
            client_id,
            seq,
            timestamp,
            data,
            red,
        } => {
            let payload_type = if red {
                PayloadType::Red
            } else {
                PayloadType::Opus
            };
            let packet = AudioPacket::new_typed(seq, timestamp, client_id, payload_type, data);
            let _ = voice_tx.try_send(VoiceControl::IncomingVoicePacket(packet));
        }

        // Video → screen thread
        NetworkMessage::VideoFrame {
            client_id,
            seq,
            is_keyframe,
            width,
            height,
            timestamp_ms,
            data,
        } => {
            if let Err(e) = screen_tx.try_send(ScreenControl::IncomingVideoFrame {
                client_id,
                seq,
                is_keyframe,
                width,
                height,
                timestamp_ms,
                data,
            }) {
                warn!("Screen channel full, dropped frame seq={seq} keyframe={is_keyframe} - {e}");
            }
        }

        // System audio → screen thread
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
            scope,
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
                scope,
            });
            wake();
        }
        NetworkMessage::ScreenShareStopped { client_id } => {
            let _ = screen_tx.try_send(ScreenControl::PeerStoppedSharing { client_id });
            let _ = msg_tx.send(NetworkMessage::ScreenShareStopped { client_id });
            wake();
        }

        // Keyframe request from a viewer → screen thread
        NetworkMessage::KeyframeRequested { viewer_id } => {
            info!("Keyframe requested by viewer {viewer_id:#x}");
            let _ = screen_tx.try_send(ScreenControl::ForceKeyframe);
        }

        // The relay's account of our voice → voice thread: this is the
        // encoder's only loss input (spec §6.1). RTT lets its rate
        // controller tell congestion (loss + RTT inflation: send fewer
        // bytes) from random loss (add redundancy).
        NetworkMessage::VoiceReceiverReport {
            loss_pct,
            rtt_ms,
            exercised,
        } => {
            if exercised {
                let _ = voice_tx.try_send(VoiceControl::SetUplinkLoss(loss_pct));
            }
            let _ = voice_tx.try_send(VoiceControl::SetLinkRtt(rtt_ms));
        }

        // Our video uplink → screen thread (drives the bitrate ladder
        // while sharing).
        NetworkMessage::UplinkVideo {
            behind_pct,
            rtt_ms,
            kbps_up,
        } => {
            let _ = screen_tx.try_send(ScreenControl::UplinkStats {
                loss_pct: behind_pct,
                rtt_ms,
                kbps_up,
            });
        }

        // Link stats → app (UI only).
        NetworkMessage::NetworkStats { .. } => {
            let _ = msg_tx.send(msg);
            wake();
        }

        // Viewer health for our stream → screen thread (sharer's stats)
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

        // Everything else → app. These carry state (join / leave, init,
        // chat, mute states, denials, identity) — losing one desyncs
        // the UI until reconnect, so use a blocking send. The app
        // drains this channel every event-loop pass.
        other => {
            let _ = msg_tx.send(other);
            wake();
        }
    }
}

fn run_network_loop(
    control_rx: &Receiver<NetworkControl>,
    msg_tx: &Sender<NetworkMessage>,
    voice_tx: &Sender<VoiceControl>,
    screen_tx: &Sender<ScreenControl>,
    wake: &WakeFn,
) {
    let mut client: Option<NetworkClient> = None;
    let mut running = true;

    while running {
        // Block on the control channel. Outbound work (voice frames
        // from the voice thread, video chunks from the screen thread)
        // wakes this loop *immediately*. Connected, the timeout is the
        // inbound polling cadence — 1 ms, capped by the transport's
        // next timer so acks and keep-alives are never late.
        // Disconnected there is nothing to poll, so block outright.
        let recv_result = match &mut client {
            Some(c) if c.has_backlog() => control_rx.try_recv().map_err(|e| match e {
                TryRecvError::Empty => RecvTimeoutError::Timeout,
                TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            }),
            Some(c) => {
                let mut wait = Duration::from_millis(1);
                if let Some(deadline) = c.next_timeout() {
                    wait = wait.min(deadline.saturating_duration_since(Instant::now()));
                }
                control_rx.recv_timeout(wait)
            }
            None => control_rx
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected),
        };

        match recv_result {
            Ok(ctrl) => {
                if !handle_control(ctrl, &mut client, msg_tx, wake) {
                    running = false;
                }
                // Drain a bounded burst of whatever else queued up.
                for _ in 0..63 {
                    match control_rx.try_recv() {
                        Ok(ctrl) => {
                            if !handle_control(ctrl, &mut client, msg_tx, wake) {
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
                route_message(msg, msg_tx, voice_tx, screen_tx, wake);
            }
            if c.is_closed() {
                // The reason was already reported by the session.
                client = None;
            }
        }
    }
}
