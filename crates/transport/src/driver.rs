//! The sans-IO driver (spec §4.2): everything `quinn-proto` hands back
//! to its embedder — endpoint routing, the connection table, timers,
//! transmit draining, endpoint events, application events, stream and
//! datagram bookkeeping — plus the lane mapping of §4.4.
//!
//! Nothing here touches a socket or a clock. `now` is an argument to
//! every entry point, inbound datagrams are pushed in, outbound ones are
//! pulled out. That is what lets the same code run against a real UDP
//! socket ([`crate::udp`]) and the in-process harness ([`crate::sim`]).
//!
//! quinn's own runtime is the reference for timer and event semantics;
//! the shape below mirrors it: after *every* interaction with a
//! connection, endpoint events are forwarded, application events are
//! drained, pending stream writes are flushed and transmits are polled
//! until `None`. A missed step here stalls a connection silently.

use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use log::{debug, info, warn};
use protocol::{frame, Lane};
use quinn_proto::{
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
    ClientConfig, Connection, ConnectionError, ConnectionHandle, DatagramEvent, Dir, Endpoint,
    EndpointConfig, Event as QuicEvent, FinishError, ReadError, SendDatagramError, ServerConfig,
    StreamEvent, StreamId, VarInt, WriteError,
};

use crate::{
    config::Budget,
    identity::{Fingerprint, IdentityError, Observation, PinnedVerifier, ServerIdentity},
};

/// Transport-level identity of a connection. The server uses it as the
/// application's `ClientId`: random, non-zero, assigned at admission.
pub type PeerId = u64;

/// Anything that owns a [`Driver`]: a bare driver, or an application
/// (the server) that embeds one. The I/O loop and the simulation
/// harness pump the driver through this; the harness reaches the
/// application by downcasting.
pub trait DriverHost: std::any::Any {
    fn driver(&mut self) -> &mut Driver;
}

impl DriverHost for Driver {
    fn driver(&mut self) -> &mut Driver {
        self
    }
}

/// A stream/upload handle on the Media lane.
pub type UploadId = u64;

/// Which video stream a group belongs to when a peer carries several
/// (a viewer watching two sharers): the relay uses the sharer's id, a
/// sharer sending its own screen uses [`OWN_TRACK`].
pub type Track = u64;

/// The track a sharer uses for its own outgoing groups.
pub const OWN_TRACK: Track = 0;

/// Bytes of group-stream header after the lane tag: the track id.
const TRACK_BYTES: usize = 8;

/// A datagram on its way in.
#[derive(Debug)]
pub struct InboundDatagram {
    pub remote: SocketAddr,
    pub local_ip: Option<IpAddr>,
    pub ecn: Option<quinn_proto::EcnCodepoint>,
    pub data: BytesMut,
}

/// A datagram (or a GSO batch of them) on its way out.
#[derive(Debug, Clone)]
pub struct Transmit {
    pub destination: SocketAddr,
    pub ecn: Option<quinn_proto::EcnCodepoint>,
    pub contents: Bytes,
    /// `Some(n)`: `contents` is several datagrams of `n` bytes each
    /// (the last may be shorter) — send with GSO or split.
    pub segment_size: Option<usize>,
    pub src_ip: Option<IpAddr>,
}

/// Why a connection ended, in application terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// We closed it.
    LocallyClosed,
    /// The peer closed it with an application code.
    ApplicationClosed { code: u64, reason: String },
    /// No traffic for `max_idle_timeout`.
    TimedOut,
    /// The peer sent a stateless reset: it no longer knows this
    /// connection (it timed us out first, or restarted). Seen after a
    /// blackout on our side that outlasted the idle timeout.
    Reset,
    /// The peer's version of the protocol is different (ALPN refused
    /// in the TLS handshake).
    AlpnMismatch,
    /// The server's certificate did not match the pin and carried no
    /// valid successor proof.
    IdentityChanged {
        expected: Fingerprint,
        observed: Fingerprint,
    },
    /// Anything else the transport reported.
    Transport(String),
}

impl DisconnectReason {
    /// The session died for network reasons and a fresh connection
    /// with the same parameters is the sensible next step. Refusals,
    /// identity or version mismatches, and deliberate closes are not.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::TimedOut | Self::Reset)
    }
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisconnectReason::LocallyClosed => f.write_str("closed locally"),
            DisconnectReason::ApplicationClosed { code, reason } if reason.is_empty() => {
                write!(f, "closed by peer (code {code})")
            }
            DisconnectReason::ApplicationClosed { reason, .. } => f.write_str(reason),
            DisconnectReason::TimedOut => {
                f.write_str("connection timed out (no packets from the peer)")
            }
            DisconnectReason::Reset => f.write_str(
                "the server no longer knows this connection (it timed us out, or restarted)",
            ),
            DisconnectReason::AlpnMismatch => f.write_str(protocol::OUTDATED_MESSAGE),
            DisconnectReason::IdentityChanged { .. } => f.write_str("server identity changed"),
            DisconnectReason::Transport(s) => f.write_str(s),
        }
    }
}

/// Events the application drains after each tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The handshake completed; lanes are usable. On the client side
    /// `identity` says how the server's certificate was accepted.
    Connected {
        peer: PeerId,
        remote: SocketAddr,
        identity: Option<Observation>,
    },
    /// The connection is gone. Emitted exactly once per peer.
    Disconnected {
        peer: PeerId,
        reason: DisconnectReason,
    },
    /// The datagram send buffer overflowed and the packet being sent
    /// was dropped. Emitted once per episode; the count is in
    /// [`LinkStats::datagram_send_drops`].
    DatagramsBlocked { peer: PeerId },
}

/// Something read from the Video lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoItem {
    /// A new group (keyframe-led stream) started.
    GroupStart,
    /// One chunk, in order within its group.
    Chunk(Bytes),
    /// The group ended cleanly.
    GroupEnd,
    /// The sender abandoned the group; whatever partial frame the
    /// reader holds is garbage.
    GroupReset,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SendError {
    #[error("unknown peer")]
    UnknownPeer,
    #[error("peer is not connected")]
    NotConnected,
    #[error("a {0} packet of {1} bytes exceeds the datagram budget")]
    TooLarge(Lane, usize),
    #[error("no open {0} stream")]
    NoStream(Lane),
    #[error("the {0} lane is not a stream lane")]
    NotAStreamLane(Lane),
}

/// Path statistics for a peer, cumulative since the connection opened.
/// Callers diff consecutive samples for rates. `path_loss` is
/// transport-level and **informational only** — it never steers a
/// media decision (spec §6.1).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LinkStats {
    pub rtt: Duration,
    pub cwnd: u64,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub congestion_events: u64,
    pub current_mtu: u16,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub datagrams_tx: u64,
    pub datagrams_rx: u64,
    /// Voice / system-audio datagrams dropped because the send buffer
    /// was full.
    pub datagram_send_drops: u64,
    /// Episodes of send-buffer pressure.
    pub datagrams_blocked: u64,
    /// Video groups this side opened.
    pub groups_started: u64,
    /// Video groups this side abandoned because the *peer was behind*:
    /// the sum of the three causes below plus groups the peer stopped
    /// reading. Application-requested resets (a relay mirroring the
    /// sharer's own reset, an unsubscribe) are counted separately in
    /// `groups_reset_upstream` and are not this peer's fault.
    pub groups_reset: u64,
    /// …because a newer keyframe arrived while bytes of the previous
    /// group were still waiting to be handed to the transport.
    pub groups_reset_superseded: u64,
    /// …because queued bytes waited past the deadline or byte bound.
    pub groups_reset_deadline: u64,
    /// …because a finished group was not acknowledged in time.
    pub groups_reset_unacked: u64,
    /// Groups reset at the application's request (see `groups_reset`).
    pub groups_reset_upstream: u64,
    /// Video groups the peer abandoned on us.
    pub groups_reset_by_peer: u64,
    /// Media uploads this side abandoned because the peer stopped
    /// reading or the connection closed.
    pub uploads_dropped: u64,
}

impl LinkStats {
    /// Packets declared lost as a share of packets sent since `earlier`.
    #[must_use]
    pub fn path_loss_pct_since(&self, earlier: &LinkStats) -> f64 {
        let sent = self.sent_packets.saturating_sub(earlier.sent_packets);
        if sent == 0 {
            return 0.0;
        }
        let lost = self.lost_packets.saturating_sub(earlier.lost_packets);
        #[allow(clippy::cast_precision_loss)]
        {
            lost as f64 * 100.0 / sent as f64
        }
    }
}

// ── Group / upload deadlines (spec §6.2) ────────────────────────────────

/// A video group whose oldest unsent byte is older than this is reset
/// **[default: 500 ms]**: nothing older than one GOP ever queues.
pub const VIDEO_GROUP_DEADLINE: Duration = Duration::from_millis(500);
/// A video group with more than this waiting for the transport is reset
/// regardless of age.
pub const VIDEO_GROUP_MAX_PENDING: usize = 1_000_000;
/// Application error code used when a group is abandoned.
const GROUP_RESET_CODE: VarInt = VarInt::from_u32(1);
/// Application error code used when a stream carries an invalid tag or
/// frame.
const PROTOCOL_VIOLATION_CODE: VarInt = VarInt::from_u32(2);
/// Close code for application-initiated closes.
const APP_CLOSE_CODE: VarInt = VarInt::from_u32(0);
/// TLS alert `no_application_protocol`, as a QUIC `CRYPTO_ERROR` code.
const ALPN_MISMATCH_CODE: u64 = 0x100 + 120;

// ── Per-stream output queue ─────────────────────────────────────────────

/// Bytes accepted from the application but not yet accepted by the
/// transport (flow control or congestion window full).
#[derive(Debug, Default)]
struct OutQueue {
    pending: VecDeque<Bytes>,
    /// Bytes of `pending.front()` already written.
    offset: usize,
    bytes: usize,
    /// When the oldest pending byte was queued.
    oldest_at: Option<Instant>,
    /// Bytes handed to the transport so far.
    written: usize,
}

impl OutQueue {
    fn push(&mut self, chunk: Bytes, now: Instant) {
        if chunk.is_empty() {
            return;
        }
        if self.pending.is_empty() {
            self.oldest_at = Some(now);
        }
        self.bytes += chunk.len();
        self.pending.push_back(chunk);
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn age(&self, now: Instant) -> Duration {
        self.oldest_at
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.offset = 0;
        self.bytes = 0;
        self.oldest_at = None;
    }

    /// Write as much as the transport takes. `true` if the stream is
    /// gone (stopped by the peer / unknown) and the queue was dropped.
    fn flush(&mut self, conn: &mut Connection, id: StreamId, now: Instant) -> bool {
        while let Some(front) = self.pending.front() {
            let slice = &front[self.offset..];
            match conn.send_stream(id).write(slice) {
                Ok(n) => {
                    self.offset += n;
                    self.bytes -= n;
                    self.written += n;
                    if self.offset >= front.len() {
                        self.pending.pop_front();
                        self.offset = 0;
                        self.oldest_at = (!self.pending.is_empty()).then_some(now);
                    }
                }
                Err(WriteError::Blocked) => return false,
                Err(WriteError::Stopped(_) | WriteError::ClosedStream) => {
                    self.clear();
                    return true;
                }
            }
        }
        false
    }
}

// ── Per-peer lane state ─────────────────────────────────────────────────

#[derive(Debug)]
struct BidiLane {
    stream: StreamId,
    decoder: frame::Decoder,
    out: OutQueue,
}

#[derive(Debug)]
struct UniIn {
    /// Learned from the first byte.
    lane: Option<Lane>,
    /// Video only: learned from the eight bytes after the tag.
    track: Option<Track>,
    /// Header bytes collected until the lane (and track) are known.
    header: Vec<u8>,
    decoder: frame::Decoder,
    /// Video only: items decoded from this stream, released to the
    /// inbox strictly group by group per track (streams can arrive
    /// interleaved).
    items: VecDeque<VideoItem>,
    /// Finished or reset; retired once its items are released.
    done: bool,
}

impl UniIn {
    fn untagged() -> Self {
        Self {
            lane: None,
            track: None,
            header: Vec::with_capacity(1 + TRACK_BYTES),
            decoder: frame::Decoder::new(0),
            items: VecDeque::new(),
            done: false,
        }
    }

    /// Whether the header is fully known.
    fn ready(&self) -> bool {
        match self.lane {
            None => false,
            Some(Lane::Video) => self.track.is_some(),
            Some(_) => true,
        }
    }
}

#[derive(Debug)]
struct GroupOut {
    stream: StreamId,
    out: OutQueue,
}

/// A group whose data is all inside the transport, waiting to be
/// acknowledged. Reset if that takes longer than the deadline: the
/// transport's send window is a byte bound, this is the time bound.
#[derive(Debug)]
struct FinishingGroup {
    stream: StreamId,
    finished_at: Instant,
}

#[derive(Debug)]
struct UploadOut {
    id: UploadId,
    stream: StreamId,
    out: OutQueue,
    total: usize,
    /// `finish()` called; dropped from bookkeeping once acknowledged.
    finishing: bool,
}

#[derive(Debug, Default)]
struct Counters {
    datagram_send_drops: u64,
    datagrams_blocked: u64,
    blocked_episode: bool,
    groups_started: u64,
    groups_reset: u64,
    groups_reset_superseded: u64,
    groups_reset_deadline: u64,
    groups_reset_unacked: u64,
    groups_reset_upstream: u64,
    groups_reset_by_peer: u64,
    uploads_dropped: u64,
}

struct Peer {
    id: PeerId,
    conn: Connection,
    connected: bool,
    /// Set once `ConnectionLost` was seen; the peer lingers until the
    /// connection is drained so the endpoint can retire its ids.
    lost: bool,
    close_at: Option<Instant>,
    control: Option<BidiLane>,
    signal: Option<BidiLane>,
    /// Accepted bidi streams whose lane tag hasn't arrived yet.
    untagged_bidi: Vec<StreamId>,
    uni_in: HashMap<StreamId, UniIn>,
    inbox_control: VecDeque<Bytes>,
    inbox_signal: VecDeque<Bytes>,
    inbox_voice: VecDeque<Bytes>,
    inbox_system_audio: VecDeque<Bytes>,
    inbox_media: VecDeque<Bytes>,
    inbox_video: VecDeque<(Track, VideoItem)>,
    video_out: HashMap<Track, GroupOut>,
    /// Groups finished but not yet acknowledged.
    video_finishing: Vec<FinishingGroup>,
    uploads: Vec<UploadOut>,
    next_upload: UploadId,
    /// Media pacing token bucket, bytes.
    media_tokens: f64,
    media_last_refill: Instant,
    counters: Counters,
}

impl Peer {
    fn new(id: PeerId, conn: Connection, now: Instant) -> Self {
        Self {
            id,
            conn,
            connected: false,
            lost: false,
            close_at: None,
            control: None,
            signal: None,
            untagged_bidi: Vec::new(),
            uni_in: HashMap::new(),
            inbox_control: VecDeque::new(),
            inbox_signal: VecDeque::new(),
            inbox_voice: VecDeque::new(),
            inbox_system_audio: VecDeque::new(),
            inbox_media: VecDeque::new(),
            inbox_video: VecDeque::new(),
            video_out: HashMap::new(),
            video_finishing: Vec::new(),
            uploads: Vec::new(),
            next_upload: 1,
            media_tokens: 0.0,
            media_last_refill: now,
            counters: Counters::default(),
        }
    }

    fn bidi_mut(&mut self, lane: Lane) -> Option<&mut BidiLane> {
        match lane {
            Lane::Control => self.control.as_mut(),
            Lane::Signal => self.signal.as_mut(),
            _ => None,
        }
    }

    fn stats(&self) -> LinkStats {
        let s = self.conn.stats();
        LinkStats {
            rtt: s.path.rtt,
            cwnd: s.path.cwnd,
            lost_packets: s.path.lost_packets,
            sent_packets: s.path.sent_packets,
            congestion_events: s.path.congestion_events,
            current_mtu: s.path.current_mtu,
            bytes_tx: s.udp_tx.bytes,
            bytes_rx: s.udp_rx.bytes,
            datagrams_tx: s.udp_tx.datagrams,
            datagrams_rx: s.udp_rx.datagrams,
            datagram_send_drops: self.counters.datagram_send_drops,
            datagrams_blocked: self.counters.datagrams_blocked,
            groups_started: self.counters.groups_started,
            groups_reset: self.counters.groups_reset,
            groups_reset_superseded: self.counters.groups_reset_superseded,
            groups_reset_deadline: self.counters.groups_reset_deadline,
            groups_reset_unacked: self.counters.groups_reset_unacked,
            groups_reset_upstream: self.counters.groups_reset_upstream,
            groups_reset_by_peer: self.counters.groups_reset_by_peer,
            uploads_dropped: self.counters.uploads_dropped,
        }
    }
}

// ── The driver ──────────────────────────────────────────────────────────

enum Role {
    Client {
        config: ClientConfig,
        verifier: Arc<PinnedVerifier>,
    },
    Server {
        max_peers: usize,
    },
}

/// One `quinn-proto` endpoint per socket, driven by the caller.
pub struct Driver {
    endpoint: Endpoint,
    role: Role,
    budget: Budget,
    peers: HashMap<ConnectionHandle, Peer>,
    ids: HashMap<PeerId, ConnectionHandle>,
    events: VecDeque<Event>,
    transmits: VecDeque<Transmit>,
    buf: Vec<u8>,
    /// GSO segments the socket can send per call (1 without GSO).
    max_gso_segments: usize,
    /// Peers whose `ConnectionLost` was reported but that are not yet
    /// drained; polled for timers only.
    refused: u64,
}

impl Driver {
    /// A client endpoint pinning `pin` (`None`: trust on first use).
    ///
    /// # Errors
    /// rustls configuration failure (should not happen with `ring`).
    pub fn new_client(pin: Option<Fingerprint>, budget: Budget) -> Result<Self, IdentityError> {
        let verifier = PinnedVerifier::new(pin);
        let crypto = verifier.client_crypto()?;
        let quic = QuicClientConfig::try_from(crypto).map_err(|e| {
            IdentityError::Tls(rustls::Error::General(format!("quic client config: {e}")))
        })?;
        let mut config = ClientConfig::new(Arc::new(quic));
        config.transport_config(budget.transport_config());
        let endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, true, None);
        Ok(Self::with_endpoint(
            endpoint,
            Role::Client { config, verifier },
            budget,
        ))
    }

    /// A client offering a different ALPN — only for the harness, to
    /// exercise the "outdated client" path.
    ///
    /// # Errors
    /// rustls configuration failure.
    #[cfg(any(test, feature = "sim"))]
    pub fn new_client_with_alpn(
        pin: Option<Fingerprint>,
        budget: Budget,
        alpn: &[u8],
    ) -> Result<Self, IdentityError> {
        let verifier = PinnedVerifier::new(pin);
        let mut crypto = (*verifier.client_crypto()?).clone();
        crypto.alpn_protocols = vec![alpn.to_vec()];
        let quic = QuicClientConfig::try_from(Arc::new(crypto)).map_err(|e| {
            IdentityError::Tls(rustls::Error::General(format!("quic client config: {e}")))
        })?;
        let mut config = ClientConfig::new(Arc::new(quic));
        config.transport_config(budget.transport_config());
        let endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, true, None);
        Ok(Self::with_endpoint(
            endpoint,
            Role::Client { config, verifier },
            budget,
        ))
    }

    /// A server endpoint presenting `identity`, admitting at most
    /// `max_peers` connections.
    ///
    /// # Errors
    /// rustls configuration failure.
    pub fn new_server(
        identity: &ServerIdentity,
        max_peers: usize,
        budget: Budget,
    ) -> Result<Self, IdentityError> {
        let crypto = identity.server_crypto()?;
        let quic = QuicServerConfig::try_from(crypto).map_err(|e| {
            IdentityError::Tls(rustls::Error::General(format!("quic server config: {e}")))
        })?;
        let mut config = ServerConfig::with_crypto(Arc::new(quic));
        config.transport_config(budget.transport_config());
        config.migration(true);
        let endpoint = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            Some(Arc::new(config)),
            true,
            None,
        );
        Ok(Self::with_endpoint(
            endpoint,
            Role::Server { max_peers },
            budget,
        ))
    }

    fn with_endpoint(endpoint: Endpoint, role: Role, budget: Budget) -> Self {
        Self {
            endpoint,
            role,
            budget,
            peers: HashMap::new(),
            ids: HashMap::new(),
            events: VecDeque::new(),
            transmits: VecDeque::new(),
            buf: Vec::with_capacity(64 * 1024),
            max_gso_segments: 1,
            refused: 0,
        }
    }

    /// Tell the driver how many GSO segments one transmit may carry.
    pub fn set_max_gso_segments(&mut self, segments: usize) {
        self.max_gso_segments = segments.max(1);
    }

    #[must_use]
    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    #[must_use]
    pub fn is_server(&self) -> bool {
        matches!(self.role, Role::Server { .. })
    }

    /// Connected peers (handshake complete, not lost), in no order.
    #[must_use]
    pub fn peers(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| p.connected && !p.lost)
            .map(|p| p.id)
            .collect()
    }

    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.values().filter(|p| !p.lost).count()
    }

    #[must_use]
    pub fn is_connected(&self, peer: PeerId) -> bool {
        self.peer(peer).is_some_and(|p| p.connected && !p.lost)
    }

    /// Whether any peer is still handshaking.
    #[must_use]
    pub fn is_connecting(&self, peer: PeerId) -> bool {
        self.peer(peer).is_some_and(|p| !p.connected && !p.lost)
    }

    #[must_use]
    pub fn remote_address(&self, peer: PeerId) -> Option<SocketAddr> {
        self.peer(peer).map(|p| p.conn.remote_address())
    }

    #[must_use]
    pub fn stats(&self, peer: PeerId) -> Option<LinkStats> {
        self.peer(peer).map(Peer::stats)
    }

    /// Connections refused because the server was full.
    #[must_use]
    pub fn refused_count(&self) -> u64 {
        self.refused
    }

    fn peer(&self, id: PeerId) -> Option<&Peer> {
        self.ids.get(&id).and_then(|h| self.peers.get(h))
    }

    fn peer_mut(&mut self, id: PeerId) -> Option<&mut Peer> {
        let handle = *self.ids.get(&id)?;
        self.peers.get_mut(&handle)
    }

    // ── Connecting / closing ────────────────────────────────────────

    /// Client: start a connection to `remote`. Returns the peer id the
    /// server will be known by locally.
    ///
    /// # Errors
    /// Called on a server driver, or quinn refused (CID space exhausted).
    pub fn connect(&mut self, now: Instant, remote: SocketAddr) -> Result<PeerId, String> {
        let Role::Client { config, .. } = &self.role else {
            return Err("connect() on a server driver".to_string());
        };
        let (handle, conn) = self
            .endpoint
            .connect(now, config.clone(), remote, PinnedVerifier::server_name())
            .map_err(|e| e.to_string())?;
        let id = fresh_peer_id(&self.ids);
        self.peers.insert(handle, Peer::new(id, conn, now));
        self.ids.insert(id, handle);
        self.drain_peer(handle, now);
        Ok(id)
    }

    /// Close a connection now. A `Disconnected` event follows once the
    /// transport has drained.
    pub fn close(&mut self, now: Instant, peer: PeerId, reason: &str) {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&handle) {
            p.conn.close(
                now,
                APP_CLOSE_CODE,
                Bytes::copy_from_slice(reason.as_bytes()),
            );
            p.close_at = None;
        }
        self.drain_peer(handle, now);
    }

    /// Close after `delay`, giving queued stream data (a `Refused`
    /// message) time to reach the peer first.
    pub fn close_after(&mut self, now: Instant, peer: PeerId, delay: Duration) {
        if let Some(p) = self.peer_mut(peer) {
            p.close_at = Some(now + delay);
        }
    }

    /// TLS exporter for the client-key proof (Part C). `None` until the
    /// handshake completed.
    #[must_use]
    pub fn export_keying_material(
        &self,
        peer: PeerId,
        label: &[u8],
        context: &[u8],
        out: &mut [u8],
    ) -> Option<()> {
        let p = self.peer(peer)?;
        if !p.connected {
            return None;
        }
        p.conn
            .crypto_session()
            .export_keying_material(out, label, context)
            .ok()
    }

    // ── Inbound datagrams ───────────────────────────────────────────

    /// Feed one received datagram (or GRO batch — quinn splits it).
    pub fn handle_datagram(&mut self, now: Instant, dgram: InboundDatagram) {
        let InboundDatagram {
            remote,
            local_ip,
            ecn,
            data,
        } = dgram;
        let Some(event) = self
            .endpoint
            .handle(now, remote, local_ip, ecn, data, &mut self.buf)
        else {
            return;
        };
        match event {
            DatagramEvent::ConnectionEvent(handle, ev) => {
                if let Some(p) = self.peers.get_mut(&handle) {
                    p.conn.handle_event(ev);
                    self.drain_peer(handle, now);
                }
            }
            DatagramEvent::NewConnection(incoming) => {
                let full = match &self.role {
                    Role::Server { max_peers } => self.peer_count() >= *max_peers,
                    Role::Client { .. } => true,
                };
                if full {
                    self.refused += 1;
                    let t = self.endpoint.refuse(incoming, &mut self.buf);
                    self.push_transmit(&t);
                    return;
                }
                match self.endpoint.accept(incoming, now, &mut self.buf, None) {
                    Ok((handle, conn)) => {
                        let id = fresh_peer_id(&self.ids);
                        debug!("accepted connection from {remote} as peer {id:#x}");
                        self.peers.insert(handle, Peer::new(id, conn, now));
                        self.ids.insert(id, handle);
                        self.drain_peer(handle, now);
                    }
                    Err(e) => {
                        debug!("refused connection from {remote}: {}", e.cause);
                        if let Some(t) = e.response {
                            self.push_transmit(&t);
                        }
                    }
                }
            }
            DatagramEvent::Response(t) => self.push_transmit(&t),
        }
    }

    // ── Timers ──────────────────────────────────────────────────────

    /// Earliest instant any connection needs `handle_timeouts` by.
    #[must_use]
    pub fn next_timeout(&mut self) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        for p in self.peers.values_mut() {
            let mut t = p.conn.poll_timeout();
            if let Some(close_at) = p.close_at {
                t = Some(t.map_or(close_at, |t| t.min(close_at)));
            }
            if let Some(t) = t {
                earliest = Some(earliest.map_or(t, |e| e.min(t)));
            }
        }
        earliest
    }

    /// Fire every timer at or before `now`, and run the per-tick work
    /// (deadline resets, media pacing, deferred closes).
    pub fn handle_timeouts(&mut self, now: Instant) {
        let handles: Vec<ConnectionHandle> = self.peers.keys().copied().collect();
        for handle in handles {
            let Some(p) = self.peers.get_mut(&handle) else {
                continue;
            };
            if p.conn.poll_timeout().is_some_and(|t| t <= now) {
                p.conn.handle_timeout(now);
            }
            if p.close_at.is_some_and(|t| t <= now) && !p.lost {
                p.close_at = None;
                p.conn.close(now, APP_CLOSE_CODE, Bytes::new());
            }
            self.drain_peer(handle, now);
        }
    }

    // ── Outbound datagrams ──────────────────────────────────────────

    /// The next datagram (batch) to put on the wire, if any.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// Whether transmits are waiting.
    #[must_use]
    pub fn has_transmits(&self) -> bool {
        !self.transmits.is_empty()
    }

    fn push_transmit(&mut self, t: &quinn_proto::Transmit) {
        let contents = Bytes::copy_from_slice(&self.buf[..t.size]);
        self.buf.clear();
        self.transmits.push_back(Transmit {
            destination: t.destination,
            ecn: t.ecn,
            contents,
            segment_size: t.segment_size,
            src_ip: t.src_ip,
        });
    }

    // ── Events ──────────────────────────────────────────────────────

    /// Application events since the last drain.
    pub fn drain_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.events.drain(..)
    }

    // ── Lane API (spec §4.4) ────────────────────────────────────────

    /// Send on a lane. Control/Signal frames are length-prefixed and
    /// queued on their stream; Voice/`SystemAudio` become datagrams;
    /// Video goes to the [`OWN_TRACK`] group (see [`Self::send_video`]
    /// for the relay's per-track form). Media uses [`Self::send_media`].
    ///
    /// # Errors
    /// Unknown or unconnected peer, a datagram over budget, a stream
    /// lane with no stream, or the Media lane.
    pub fn send(
        &mut self,
        now: Instant,
        peer: PeerId,
        lane: Lane,
        bytes: &[u8],
    ) -> Result<(), SendError> {
        let handle = *self.ids.get(&peer).ok_or(SendError::UnknownPeer)?;
        let p = self.peers.get_mut(&handle).ok_or(SendError::UnknownPeer)?;
        if !p.connected || p.lost {
            return Err(SendError::NotConnected);
        }
        let result = match lane {
            Lane::Voice | Lane::SystemAudio => send_datagram(p, lane, bytes, &mut self.events),
            Lane::Control | Lane::Signal => {
                let frame = frame::encode(bytes);
                let bidi = p.bidi_mut(lane).ok_or(SendError::NoStream(lane))?;
                bidi.out.push(frame, now);
                Ok(())
            }
            Lane::Video => {
                video_send(p, OWN_TRACK, bytes, now);
                Ok(())
            }
            Lane::Media => Err(SendError::NotAStreamLane(lane)),
        };
        self.drain_peer(handle, now);
        result
    }

    /// Video (§6.2): begin a new group on `track` — every group starts
    /// with a keyframe. The track's previous group is finished if fully
    /// written, reset if it still had unsent bytes (a newer keyframe
    /// exists).
    pub fn video_new_group(&mut self, now: Instant, peer: PeerId, track: Track) {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&handle) {
            video_close_current(p, track, now, None);
            if let Some(stream) = p.conn.streams().open(Dir::Uni) {
                let mut header = Vec::with_capacity(1 + TRACK_BYTES);
                header.push(Lane::Video.tag());
                header.extend_from_slice(&track.to_le_bytes());
                let mut out = OutQueue::default();
                out.push(Bytes::from(header), now);
                let _ = p
                    .conn
                    .send_stream(stream)
                    .set_priority(Lane::Video.priority());
                p.video_out.insert(track, GroupOut { stream, out });
                p.counters.groups_started += 1;
            } else {
                // Stream credit exhausted: the peer holds too many
                // groups open — behave as behind and skip this one.
                p.counters.groups_reset_deadline += 1;
                p.counters.groups_reset += 1;
            }
        }
        self.drain_peer(handle, now);
    }

    /// Video: end the track's current group cleanly (sharer stopped, or
    /// the relay saw the sharer's group end).
    pub fn video_finish_group(&mut self, now: Instant, peer: PeerId, track: Track) {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&handle) {
            video_close_current(p, track, now, None);
        }
        self.drain_peer(handle, now);
    }

    /// Video: abandon the track's current group at the application's
    /// request (the relay saw the sharer reset it, a viewer
    /// unsubscribed). Not counted against this peer.
    pub fn video_reset_group(&mut self, now: Instant, peer: PeerId, track: Track) {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&handle) {
            video_close_current(p, track, now, Some(ResetCause::Upstream));
        }
        self.drain_peer(handle, now);
    }

    /// Video: send one chunk on the track's current group. Dropped
    /// silently when the group was reset (the next keyframe reopens).
    ///
    /// # Errors
    /// Unknown or unconnected peer.
    pub fn send_video(
        &mut self,
        now: Instant,
        peer: PeerId,
        track: Track,
        bytes: &[u8],
    ) -> Result<(), SendError> {
        let handle = *self.ids.get(&peer).ok_or(SendError::UnknownPeer)?;
        let p = self.peers.get_mut(&handle).ok_or(SendError::UnknownPeer)?;
        if !p.connected || p.lost {
            return Err(SendError::NotConnected);
        }
        video_send(p, track, bytes, now);
        self.drain_peer(handle, now);
        Ok(())
    }

    /// Whether `peer` currently has an open outgoing video group on
    /// `track`.
    #[must_use]
    pub fn video_group_open(&self, peer: PeerId, track: Track) -> bool {
        self.peer(peer)
            .is_some_and(|p| p.video_out.contains_key(&track))
    }

    /// Media: open one upload stream carrying `frames` (each length-
    /// prefixed), paced at the budget's media rate, finished when
    /// drained. Progress via [`Self::upload_progress`].
    ///
    /// # Errors
    /// Unknown/unconnected peer, or no stream credit left.
    pub fn send_media(
        &mut self,
        now: Instant,
        peer: PeerId,
        frames: impl IntoIterator<Item = Bytes>,
    ) -> Result<UploadId, SendError> {
        let handle = *self.ids.get(&peer).ok_or(SendError::UnknownPeer)?;
        let p = self.peers.get_mut(&handle).ok_or(SendError::UnknownPeer)?;
        if !p.connected || p.lost {
            return Err(SendError::NotConnected);
        }
        let stream = p
            .conn
            .streams()
            .open(Dir::Uni)
            .ok_or(SendError::NoStream(Lane::Media))?;
        let _ = p
            .conn
            .send_stream(stream)
            .set_priority(Lane::Media.priority());
        let mut out = OutQueue::default();
        out.push(Bytes::copy_from_slice(&[Lane::Media.tag()]), now);
        for f in frames {
            out.push(frame::encode(&f), now);
        }
        let total = out.bytes;
        let id = p.next_upload;
        p.next_upload += 1;
        p.uploads.push(UploadOut {
            id,
            stream,
            out,
            total,
            finishing: false,
        });
        self.drain_peer(handle, now);
        Ok(id)
    }

    /// `(bytes handed to the transport, total)` for an upload still in
    /// progress; `None` once it is fully written and finished (or
    /// dropped).
    #[must_use]
    /// Abort a media upload: the stream is reset (the peer's reader sees
    /// it end short and drops the partial) and the queue is freed. `true`
    /// when there was one.
    pub fn cancel_media(&mut self, peer: PeerId, upload: UploadId) -> bool {
        let Some(p) = self.peer_mut(peer) else {
            return false;
        };
        let Some(i) = p.uploads.iter().position(|u| u.id == upload) else {
            return false;
        };
        let u = p.uploads.remove(i);
        let _ = p.conn.send_stream(u.stream).reset(GROUP_RESET_CODE);
        p.counters.uploads_dropped += 1;
        true
    }

    pub fn upload_progress(&self, peer: PeerId, upload: UploadId) -> Option<(usize, usize)> {
        let p = self.peer(peer)?;
        let u = p.uploads.iter().find(|u| u.id == upload)?;
        Some((u.out.written, u.total))
    }

    /// Pop the next frame received on a Control / Signal / Media lane or
    /// the next datagram on Voice / `SystemAudio`. Use
    /// [`Self::recv_video`] for the Video lane.
    pub fn recv(&mut self, peer: PeerId, lane: Lane) -> Option<Bytes> {
        let p = self.peer_mut(peer)?;
        match lane {
            Lane::Control => p.inbox_control.pop_front(),
            Lane::Signal => p.inbox_signal.pop_front(),
            Lane::Voice => p.inbox_voice.pop_front(),
            Lane::SystemAudio => p.inbox_system_audio.pop_front(),
            Lane::Media => p.inbox_media.pop_front(),
            Lane::Video => None,
        }
    }

    /// Pop the next Video lane item with its track. Items are in order
    /// within a track; tracks interleave.
    pub fn recv_video(&mut self, peer: PeerId) -> Option<(Track, VideoItem)> {
        self.peer_mut(peer)?.inbox_video.pop_front()
    }

    // ── The per-connection drain (the driver loop proper) ───────────

    /// After any interaction with a connection: forward endpoint
    /// events, process application events, flush writes, poll
    /// transmits, and retire the peer once drained.
    fn drain_peer(&mut self, handle: ConnectionHandle, now: Instant) {
        // Endpoint events may loop back connection events.
        loop {
            let Some(p) = self.peers.get_mut(&handle) else {
                return;
            };
            let Some(ev) = p.conn.poll_endpoint_events() else {
                break;
            };
            let drained = ev.is_drained();
            if let Some(cev) = self.endpoint.handle_event(handle, ev)
                && let Some(p) = self.peers.get_mut(&handle)
            {
                p.conn.handle_event(cev);
            }
            if drained {
                // A timed-out connection is drained in the same pass that
                // reports `ConnectionLost`; surface that event first.
                self.process_app_events(handle, now);
                self.retire(handle);
                return;
            }
        }

        self.process_app_events(handle, now);

        let Some(p) = self.peers.get_mut(&handle) else {
            return;
        };
        if p.connected && !p.lost {
            flush_peer(p, now, self.budget.media_pace_bytes_per_sec);
        }

        // Transmits until None — never leave any unpolled.
        loop {
            let Some(p) = self.peers.get_mut(&handle) else {
                return;
            };
            let Some(t) = p
                .conn
                .poll_transmit(now, self.max_gso_segments, &mut self.buf)
            else {
                break;
            };
            self.push_transmit(&t);
        }
    }

    fn retire(&mut self, handle: ConnectionHandle) {
        if let Some(p) = self.peers.remove(&handle) {
            self.ids.remove(&p.id);
            debug!("peer {:#x} drained", p.id);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_app_events(&mut self, handle: ConnectionHandle, now: Instant) {
        let is_client = matches!(self.role, Role::Client { .. });
        loop {
            let Some(p) = self.peers.get_mut(&handle) else {
                return;
            };
            let Some(ev) = p.conn.poll() else {
                return;
            };
            match ev {
                // Writable: flushed by drain_peer after this loop.
                // Available: uni-stream credit; groups/uploads retry on
                // their next open.
                QuicEvent::HandshakeDataReady
                | QuicEvent::Stream(StreamEvent::Writable { .. } | StreamEvent::Available { .. }) =>
                    {}
                QuicEvent::Connected => {
                    p.connected = true;
                    let identity = match &self.role {
                        Role::Client { verifier, .. } => {
                            open_bidi_lanes(p, now);
                            verifier.observation()
                        }
                        Role::Server { .. } => None,
                    };
                    let remote = p.conn.remote_address();
                    info!("peer {:#x} connected from {remote}", p.id);
                    self.events.push_back(Event::Connected {
                        peer: p.id,
                        remote,
                        identity,
                    });
                }
                QuicEvent::ConnectionLost { reason } => {
                    if p.lost {
                        continue;
                    }
                    p.lost = true;
                    let identity = match &self.role {
                        Role::Client { verifier, .. } => verifier.observation(),
                        Role::Server { .. } => None,
                    };
                    let mapped = map_connection_error(&reason, identity, is_client);
                    // Everything queued is gone with the connection.
                    p.counters.uploads_dropped += p.uploads.len() as u64;
                    p.uploads.clear();
                    p.video_out.clear();
                    p.video_finishing.clear();
                    info!("peer {:#x} disconnected: {mapped}", p.id);
                    self.events.push_back(Event::Disconnected {
                        peer: p.id,
                        reason: mapped,
                    });
                }
                QuicEvent::Stream(StreamEvent::Opened { dir }) => {
                    while let Some(id) = p.conn.streams().accept(dir) {
                        match dir {
                            Dir::Bi => p.untagged_bidi.push(id),
                            Dir::Uni => {
                                p.uni_in.insert(id, UniIn::untagged());
                            }
                        }
                        read_stream(p, id);
                    }
                }
                QuicEvent::Stream(StreamEvent::Readable { id }) => read_stream(p, id),
                QuicEvent::Stream(StreamEvent::Finished { id }) => {
                    p.video_finishing.retain(|g| g.stream != id);
                    p.uploads.retain(|u| u.stream != id);
                }
                QuicEvent::Stream(StreamEvent::Stopped { id, .. }) => {
                    // The peer stopped reading: whatever we queued for
                    // that stream is garbage now.
                    let before = p.video_out.len();
                    p.video_out.retain(|_, g| g.stream != id);
                    p.counters.groups_reset += (before - p.video_out.len()) as u64;
                    p.video_finishing.retain(|g| g.stream != id);
                    let before = p.uploads.len();
                    p.uploads.retain(|u| u.stream != id);
                    p.counters.uploads_dropped += (before - p.uploads.len()) as u64;
                }
                QuicEvent::DatagramReceived => {
                    while let Some(d) = p.conn.datagrams().recv() {
                        route_datagram(p, &d);
                    }
                }
                QuicEvent::DatagramsUnblocked => {
                    p.counters.blocked_episode = false;
                }
            }
        }
    }
}

// ── Free helpers on a peer (keep `Driver` borrows simple) ───────────────

fn fresh_peer_id(ids: &HashMap<PeerId, ConnectionHandle>) -> PeerId {
    loop {
        let id: u64 = rand::random();
        if id != 0 && !ids.contains_key(&id) {
            return id;
        }
    }
}

/// Client side: open Control then Signal right after the handshake and
/// stamp each with its tag byte.
fn open_bidi_lanes(p: &mut Peer, now: Instant) {
    for lane in [Lane::Control, Lane::Signal] {
        let Some(stream) = p.conn.streams().open(Dir::Bi) else {
            warn!("could not open the {lane} stream: no credit");
            continue;
        };
        let _ = p.conn.send_stream(stream).set_priority(lane.priority());
        let mut out = OutQueue::default();
        out.push(Bytes::copy_from_slice(&[lane.tag()]), now);
        let bidi = BidiLane {
            stream,
            decoder: frame::Decoder::new(lane.max_frame_bytes()),
            out,
        };
        match lane {
            Lane::Control => p.control = Some(bidi),
            Lane::Signal => p.signal = Some(bidi),
            _ => {}
        }
    }
}

fn send_datagram(
    p: &mut Peer,
    lane: Lane,
    bytes: &[u8],
    events: &mut VecDeque<Event>,
) -> Result<(), SendError> {
    if bytes.len() > lane.max_frame_bytes() {
        return Err(SendError::TooLarge(lane, bytes.len()));
    }
    let mut d = BytesMut::with_capacity(1 + bytes.len());
    d.extend_from_slice(&[lane.tag()]);
    d.extend_from_slice(bytes);
    let d = d.freeze();
    let mut datagrams = p.conn.datagrams();
    if let Some(max) = datagrams.max_size()
        && d.len() > max
    {
        return Err(SendError::TooLarge(lane, bytes.len()));
    }
    // Time bound (§6.1): the send buffer is ~200 ms of voice; when it is
    // full the packet is dropped and counted. `drop = false` on purpose:
    // quinn-proto 0.11.17's drop-oldest path (`send(_, true)`) decrements
    // its `payload_bytes` accounting twice per evicted datagram, the
    // counter wraps, and the next eviction panics with
    // "datagrams.outgoing.payload_bytes desynchronized" — which killed
    // the client's network thread the first time a link stalled. Until
    // the fix (quinn's `make_space_for`) is released, the newest packet
    // is the one that goes; the buffer is short enough that the
    // difference is a few frames of audio during a stall either way.
    match datagrams.send(d, false) {
        Ok(()) => {
            p.counters.blocked_episode = false;
            Ok(())
        }
        Err(SendDatagramError::Blocked(_)) => {
            p.counters.datagram_send_drops += 1;
            if !p.counters.blocked_episode {
                p.counters.blocked_episode = true;
                p.counters.datagrams_blocked += 1;
                events.push_back(Event::DatagramsBlocked { peer: p.id });
            }
            Ok(())
        }
        Err(SendDatagramError::TooLarge) => Err(SendError::TooLarge(lane, bytes.len())),
        Err(SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled) => {
            Err(SendError::NotConnected)
        }
    }
}

/// Why an outgoing group is being abandoned — accounted separately so
/// the logs can tell a behind peer from an upstream reset.
#[derive(Debug, Clone, Copy)]
enum ResetCause {
    /// Queued bytes waited past the deadline or byte bound.
    Deadline,
    /// The application asked (relay mirroring the sharer, unsubscribe).
    Upstream,
}

fn video_send(p: &mut Peer, track: Track, bytes: &[u8], now: Instant) {
    let Some(group) = p.video_out.get_mut(&track) else {
        return; // group was reset; wait for the next keyframe
    };
    // Byte and time bounds (§6.2): a behind peer loses the group, not
    // its place in the queue.
    if group.out.bytes > VIDEO_GROUP_MAX_PENDING || group.out.age(now) > VIDEO_GROUP_DEADLINE {
        video_close_current(p, track, now, Some(ResetCause::Deadline));
        return;
    }
    group.out.push(frame::encode(bytes), now);
}

/// Finish the track's current outgoing group, or reset it for `cause`.
/// With no cause given, a group that still has unsent bytes is reset
/// anyway — that is the "newer keyframe exists" rule — and counted as
/// superseded.
fn video_close_current(p: &mut Peer, track: Track, now: Instant, cause: Option<ResetCause>) {
    let Some(group) = p.video_out.remove(&track) else {
        return;
    };
    let cause = match cause {
        Some(c) => Some(c),
        None if !group.out.is_empty() => {
            p.counters.groups_reset_superseded += 1;
            p.counters.groups_reset += 1;
            let _ = p.conn.send_stream(group.stream).reset(GROUP_RESET_CODE);
            return;
        }
        None => None,
    };
    if let Some(cause) = cause {
        match cause {
            ResetCause::Deadline => {
                p.counters.groups_reset_deadline += 1;
                p.counters.groups_reset += 1;
            }
            ResetCause::Upstream => p.counters.groups_reset_upstream += 1,
        }
        let _ = p.conn.send_stream(group.stream).reset(GROUP_RESET_CODE);
        return;
    }
    match p.conn.send_stream(group.stream).finish() {
        Ok(()) | Err(FinishError::ClosedStream) => {}
        Err(FinishError::Stopped(_)) => return,
    }
    p.video_finishing.push(FinishingGroup {
        stream: group.stream,
        finished_at: now,
    });
}

/// Per-tick flush of every outgoing queue, in lane priority order.
fn flush_peer(p: &mut Peer, now: Instant, media_pace: usize) {
    if let Some(l) = p.signal.as_mut() {
        l.out.flush(&mut p.conn, l.stream, now);
    }
    if let Some(l) = p.control.as_mut() {
        l.out.flush(&mut p.conn, l.stream, now);
    }
    // Current video groups: flush, then apply the deadline to what is
    // still waiting (a group can fall behind between sends).
    let mut reset_tracks = Vec::new();
    for (track, g) in &mut p.video_out {
        let gone = g.out.flush(&mut p.conn, g.stream, now);
        if gone
            || (!g.out.is_empty()
                && (g.out.bytes > VIDEO_GROUP_MAX_PENDING || g.out.age(now) > VIDEO_GROUP_DEADLINE))
        {
            reset_tracks.push(*track);
        }
    }
    for track in reset_tracks {
        video_close_current(p, track, now, Some(ResetCause::Deadline));
    }
    // Finished groups not acknowledged within the deadline (plus a
    // couple of RTTs for the acks themselves) are stale: reset them so
    // the transport frees their send-window share for fresh groups.
    let ack_deadline = VIDEO_GROUP_DEADLINE + 2 * p.conn.rtt();
    let mut i = 0;
    while i < p.video_finishing.len() {
        let g = &p.video_finishing[i];
        if now.saturating_duration_since(g.finished_at) > ack_deadline {
            let _ = p.conn.send_stream(g.stream).reset(GROUP_RESET_CODE);
            p.counters.groups_reset_unacked += 1;
            p.counters.groups_reset += 1;
            p.video_finishing.remove(i);
            continue;
        }
        i += 1;
    }

    // Media: paced. Token bucket refilled from the wall clock we are
    // given; burst capped at two frames so an idle bucket cannot blast a
    // backlog we were pacing to avoid.
    #[allow(clippy::cast_precision_loss)]
    {
        let dt = now
            .saturating_duration_since(p.media_last_refill)
            .as_secs_f64();
        p.media_last_refill = now;
        let burst = 2.0 * (protocol::IMAGE_CHUNK_BYTES + frame::PREFIX_BYTES) as f64;
        p.media_tokens = (p.media_tokens + dt * media_pace as f64).min(burst);
    }
    let mut i = 0;
    while i < p.uploads.len() {
        let u = &mut p.uploads[i];
        // Write whole pending chunks while tokens allow.
        let mut gone = false;
        while let Some(front) = u.out.pending.front() {
            #[allow(clippy::cast_precision_loss)]
            let cost = (front.len() - u.out.offset) as f64;
            if p.media_tokens < cost {
                break;
            }
            let before = u.out.pending.len();
            gone = u.out.flush_one(&mut p.conn, u.stream, now);
            if gone {
                break;
            }
            if u.out.pending.len() == before {
                break; // blocked mid-chunk
            }
            p.media_tokens -= cost;
        }
        if gone {
            p.counters.uploads_dropped += 1;
            p.uploads.remove(i);
            continue;
        }
        if u.out.is_empty() && !u.finishing {
            u.finishing = true;
            match p.conn.send_stream(u.stream).finish() {
                Ok(()) => {}
                Err(FinishError::Stopped(_) | FinishError::ClosedStream) => {
                    p.uploads.remove(i);
                    continue;
                }
            }
        }
        i += 1;
    }
}

impl OutQueue {
    /// Flush at most the front chunk. `true` if the stream is gone.
    fn flush_one(&mut self, conn: &mut Connection, id: StreamId, now: Instant) -> bool {
        let Some(front) = self.pending.front() else {
            return false;
        };
        let slice = &front[self.offset..];
        match conn.send_stream(id).write(slice) {
            Ok(n) => {
                self.offset += n;
                self.bytes -= n;
                self.written += n;
                if self.offset >= front.len() {
                    self.pending.pop_front();
                    self.offset = 0;
                    self.oldest_at = (!self.pending.is_empty()).then_some(now);
                }
                false
            }
            Err(WriteError::Blocked) => false,
            Err(WriteError::Stopped(_) | WriteError::ClosedStream) => {
                self.clear();
                true
            }
        }
    }
}

/// Read everything currently available on `id` and route it by lane.
fn read_stream(p: &mut Peer, id: StreamId) {
    // Pull all chunks first so the borrow of `conn` ends before we touch
    // the lane tables.
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut status = ReadStatus::More;
    {
        let mut recv = p.conn.recv_stream(id);
        let Ok(mut reader) = recv.read(true) else {
            return;
        };
        loop {
            match reader.next(usize::MAX) {
                Ok(Some(chunk)) => chunks.push(chunk.bytes),
                Ok(None) => {
                    status = ReadStatus::Finished;
                    break;
                }
                Err(ReadError::Blocked) => break,
                Err(ReadError::Reset(_)) => {
                    status = ReadStatus::Reset;
                    break;
                }
            }
        }
        let _ = reader.finalize();
    }

    // Untagged bidi: the first byte names the lane.
    if let Some(pos) = p.untagged_bidi.iter().position(|s| *s == id) {
        let Some(first) = chunks.first().and_then(|c| c.first().copied()) else {
            if status != ReadStatus::More {
                p.untagged_bidi.remove(pos);
            }
            return;
        };
        p.untagged_bidi.remove(pos);
        let Some(lane) =
            Lane::from_tag(first).filter(|l| matches!(l, Lane::Control | Lane::Signal))
        else {
            let _ = p.conn.recv_stream(id).stop(PROTOCOL_VIOLATION_CODE);
            return;
        };
        let bidi = BidiLane {
            stream: id,
            decoder: frame::Decoder::new(lane.max_frame_bytes()),
            out: OutQueue::default(),
        };
        match lane {
            Lane::Control => p.control = Some(bidi),
            Lane::Signal => p.signal = Some(bidi),
            _ => unreachable!("filtered above"),
        }
        chunks[0] = chunks[0].slice(1..);
    }

    if let Some(lane) = [Lane::Control, Lane::Signal]
        .into_iter()
        .find(|l| p.bidi_mut(*l).is_some_and(|b| b.stream == id))
    {
        let peer_id = p.id;
        let bidi = p.bidi_mut(lane).expect("checked");
        for c in &chunks {
            bidi.decoder.push(c);
        }
        let mut frames = Vec::new();
        let mut violation = None;
        loop {
            match bidi.decoder.next_frame() {
                Ok(Some(f)) => frames.push(f),
                Ok(None) => break,
                Err(e) => {
                    violation = Some(e);
                    break;
                }
            }
        }
        let inbox = match lane {
            Lane::Control => &mut p.inbox_control,
            _ => &mut p.inbox_signal,
        };
        inbox.extend(frames);
        if let Some(e) = violation {
            warn!("peer {peer_id:#x}: {lane} stream: {e}");
            let _ = p.conn.recv_stream(id).stop(PROTOCOL_VIOLATION_CODE);
        }
        // A finished or reset control stream means the peer is going
        // away; the connection-level close follows.
        return;
    }

    read_uni_stream(p, id, chunks, status);
}

/// Route chunks read from a unidirectional stream (Video group or Media
/// upload), learning its lane from the first byte.
fn read_uni_stream(p: &mut Peer, id: StreamId, chunks: Vec<Bytes>, mut status: ReadStatus) {
    let Some(uni) = p.uni_in.get_mut(&id) else {
        return;
    };
    let mut chunks = chunks.into_iter();
    // Collect the header (tag, then the track for Video) before any
    // frame bytes are decoded.
    while !uni.ready() {
        let Some(mut chunk) = chunks.next() else {
            if status != ReadStatus::More {
                p.uni_in.remove(&id);
                promote_video(p);
            }
            return;
        };
        while !uni.ready() && !chunk.is_empty() {
            let need = match uni.lane {
                None => 1,
                Some(Lane::Video) => TRACK_BYTES - uni.header.len(),
                Some(_) => 0,
            };
            let take = need.min(chunk.len());
            uni.header.extend_from_slice(&chunk[..take]);
            chunk = chunk.slice(take..);
            if uni.lane.is_none() {
                let tag = uni.header[0];
                uni.header.clear();
                let Some(lane @ (Lane::Video | Lane::Media)) = Lane::from_tag(tag) else {
                    let _ = p.conn.recv_stream(id).stop(PROTOCOL_VIOLATION_CODE);
                    p.uni_in.remove(&id);
                    promote_video(p);
                    return;
                };
                uni.lane = Some(lane);
                uni.decoder = frame::Decoder::new(lane.max_frame_bytes());
            } else if uni.lane == Some(Lane::Video) && uni.header.len() == TRACK_BYTES {
                let mut raw = [0u8; TRACK_BYTES];
                raw.copy_from_slice(&uni.header);
                uni.track = Some(Track::from_le_bytes(raw));
                uni.items.push_back(VideoItem::GroupStart);
            }
        }
        if uni.ready() {
            uni.decoder.push(&chunk);
        }
    }
    for c in chunks {
        uni.decoder.push(&c);
    }
    let lane = uni.lane.expect("ready implies a lane");
    let mut media_frames = Vec::new();
    let mut violation = None;
    loop {
        match uni.decoder.next_frame() {
            Ok(Some(f)) => match lane {
                Lane::Video => uni.items.push_back(VideoItem::Chunk(f)),
                _ => media_frames.push(f),
            },
            Ok(None) => break,
            Err(e) => {
                violation = Some(e);
                status = ReadStatus::Reset;
                break;
            }
        }
    }
    match (status, lane) {
        (ReadStatus::More, _) => {}
        (ReadStatus::Finished, Lane::Video) => {
            uni.items.push_back(VideoItem::GroupEnd);
            uni.done = true;
        }
        (ReadStatus::Reset, Lane::Video) => {
            uni.items.push_back(VideoItem::GroupReset);
            uni.done = true;
            p.counters.groups_reset_by_peer += 1;
        }
        (ReadStatus::Finished | ReadStatus::Reset, _) => {
            p.uni_in.remove(&id);
        }
    }
    p.inbox_media.extend(media_frames);
    if let Some(e) = violation {
        warn!("peer {:#x}: {lane} stream: {e}", p.id);
        let _ = p.conn.recv_stream(id).stop(PROTOCOL_VIOLATION_CODE);
    }
    promote_video(p);
}

/// Release video items to the inbox strictly in group order per track:
/// the lowest-numbered open stream of a track goes first, and a stream
/// whose header is still incomplete (it might be that track's next
/// group) holds the line for every track behind it.
fn promote_video(p: &mut Peer) {
    loop {
        let min_unknown = p
            .uni_in
            .iter()
            .filter(|(_, u)| u.lane != Some(Lane::Media) && !u.ready())
            .map(|(id, _)| *id)
            .min();
        let mut heads: HashMap<Track, StreamId> = HashMap::new();
        for (id, u) in &p.uni_in {
            if let Some(track) = u.track {
                heads
                    .entry(track)
                    .and_modify(|h| *h = (*h).min(*id))
                    .or_insert(*id);
            }
        }
        let mut progressed = false;
        for (track, id) in heads {
            if min_unknown.is_some_and(|m| m < id) {
                continue;
            }
            let Some(u) = p.uni_in.get_mut(&id) else {
                continue;
            };
            if !u.items.is_empty() {
                p.inbox_video
                    .extend(u.items.drain(..).map(|it| (track, it)));
                progressed = true;
            }
            if u.done {
                p.uni_in.remove(&id);
                progressed = true;
            }
        }
        if !progressed {
            return;
        }
    }
}

#[derive(PartialEq, Eq)]
enum ReadStatus {
    More,
    Finished,
    Reset,
}

fn route_datagram(p: &mut Peer, d: &Bytes) {
    let Some(&tag) = d.first() else {
        return;
    };
    let payload = d.slice(1..);
    match Lane::from_tag(tag) {
        Some(Lane::Voice) if payload.len() <= Lane::Voice.max_frame_bytes() => {
            p.inbox_voice.push_back(payload);
        }
        Some(Lane::SystemAudio) if payload.len() <= Lane::SystemAudio.max_frame_bytes() => {
            p.inbox_system_audio.push_back(payload);
        }
        _ => {}
    }
}

fn map_connection_error(
    e: &ConnectionError,
    identity: Option<Observation>,
    is_client: bool,
) -> DisconnectReason {
    if let Some(Observation::Rejected { expected, observed }) = identity {
        return DisconnectReason::IdentityChanged { expected, observed };
    }
    match e {
        ConnectionError::TimedOut => DisconnectReason::TimedOut,
        ConnectionError::Reset => DisconnectReason::Reset,
        ConnectionError::LocallyClosed => DisconnectReason::LocallyClosed,
        ConnectionError::ApplicationClosed(close) => DisconnectReason::ApplicationClosed {
            code: close.error_code.into_inner(),
            reason: String::from_utf8_lossy(&close.reason).into_owned(),
        },
        ConnectionError::ConnectionClosed(close)
            if u64::from(close.error_code) == ALPN_MISMATCH_CODE =>
        {
            DisconnectReason::AlpnMismatch
        }
        ConnectionError::TransportError(te) if u64::from(te.code) == ALPN_MISMATCH_CODE => {
            DisconnectReason::AlpnMismatch
        }
        ConnectionError::TransportError(te)
            if is_client && u64::from(te.code) >= 0x100 && u64::from(te.code) < 0x200 =>
        {
            // Some other TLS alert from the server during the handshake.
            DisconnectReason::Transport(format!("TLS handshake failed: {te}"))
        }
        other => DisconnectReason::Transport(other.to_string()),
    }
}
