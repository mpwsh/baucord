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
    crypto::rustls::QuicClientConfig, ClientConfig, Connection, ConnectionError, ConnectionHandle,
    DatagramEvent, Dir, Endpoint, EndpointConfig, Event as QuicEvent, FinishError, ReadError,
    SendDatagramError, StreamEvent, StreamId, VarInt, WriteError,
};

use crate::{
    config::Budget,
    identity::{Fingerprint, IdentityError, Observation, PinnedVerifier},
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
    /// Video groups this side opened — or could not, for want of
    /// stream credit: such a group is counted here and as reset.
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

/// Video group counters for one outgoing track, cumulative since the
/// track's first group. [`LinkStats`] sums these over every track of a
/// connection; a relay reporting on one sharer wants only that
/// sharer's (review P2-3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackStats {
    /// Groups opened on this track.
    pub groups_started: u64,
    /// Groups abandoned because the peer was behind — the rule of
    /// [`LinkStats::groups_reset`].
    pub groups_reset: u64,
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

    /// When the oldest queued byte reaches [`VIDEO_GROUP_DEADLINE`]
    /// (`None` while nothing is queued).
    fn due_at(&self) -> Option<Instant> {
        self.oldest_at.map(|t| t + VIDEO_GROUP_DEADLINE)
    }

    /// A video group's byte or time bound is passed (§6.2). The same
    /// instant [`Driver::next_deadline`] wakes the loop for.
    fn overdue(&self, now: Instant) -> bool {
        self.bytes > VIDEO_GROUP_MAX_PENDING || self.due_at().is_some_and(|due| now >= due)
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
            // Handed over, not copied: the transport keeps a reference
            // to the same bytes — the ones a relay queued for every
            // other viewer too (review P2-32).
            let mut chunk = [front.slice(self.offset..)];
            match conn.send_stream(id).write_chunks(&mut chunk) {
                Ok(written) => {
                    let n = written.bytes;
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
    track: Track,
    finished_at: Instant,
}

impl FinishingGroup {
    /// When an unacknowledged finish goes stale: the group deadline
    /// plus a couple of RTTs for the acks themselves.
    fn due_at(&self, rtt: Duration) -> Instant {
        self.finished_at + VIDEO_GROUP_DEADLINE + 2 * rtt
    }
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

// connected / lost / admitted / dirty are four separate facts about a
// connection, each read on its own; none is a state of the others.
#[allow(clippy::struct_excessive_bools)]
struct Peer {
    id: PeerId,
    conn: Connection,
    connected: bool,
    /// Set once `ConnectionLost` was seen; the peer lingers until the
    /// connection is drained so the endpoint can retire its ids.
    lost: bool,
    /// The application admitted this connection ([`Driver::admit`]):
    /// it no longer sits in the waiting room.
    admitted: bool,
    /// The application queued something since the last drain; listed
    /// in [`Driver::dirty`].
    dirty: bool,
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
    /// Group counters per outgoing track, kept until the application
    /// forgets the track.
    video_tracks: HashMap<Track, TrackStats>,
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
            admitted: false,
            dirty: false,
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
            video_tracks: HashMap::new(),
            uploads: Vec::new(),
            next_upload: 1,
            media_tokens: 0.0,
            media_last_refill: now,
            counters: Counters::default(),
        }
    }

    /// See [`Driver::next_deadline`].
    fn next_deadline(&mut self, media_pace: usize) -> Option<Instant> {
        let quinn = self.conn.poll_timeout();
        if !self.connected || self.lost {
            // Only quinn's own timers (and a deferred close until the
            // connection is lost): the lanes are not flushed yet, or no
            // longer.
            return earliest(quinn, self.close_at.filter(|_| !self.lost));
        }
        let mut at = earliest(quinn, self.close_at);
        // Lane bytes the stream would not take, and a group's bytes in
        // flight, move when the peer acks or grants credit: a datagram
        // arrives and its pass flushes them. What runs on a clock of its
        // own is the deadline of a group's oldest queued byte…
        for g in self.video_out.values() {
            at = earliest(at, g.out.due_at());
        }
        // …the ack deadline of a finished group…
        let rtt = self.conn.rtt();
        for g in &self.video_finishing {
            at = earliest(at, Some(g.due_at(rtt)));
        }
        // …and the pacer's refill, when an upload waits for tokens (one
        // whose stream is blocked waits for the peer instead, as above).
        for u in &self.uploads {
            if let Some(front) = u.out.pending.front() {
                #[allow(clippy::cast_precision_loss)]
                let cost = (front.len() - u.out.offset) as f64;
                at = earliest(
                    at,
                    media_refill_at(self.media_tokens, self.media_last_refill, cost, media_pace),
                );
            }
        }
        at
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

// ── A payload for many peers ────────────────────────────────────────────

/// One payload, put the way its lane carries it once, for sending to
/// many peers ([`Driver::send_shared`]): each send shares the bytes
/// where [`Driver::send`] builds them again — a relayed voice packet
/// was copied twice per listener, a broadcast framed once per session
/// (review P2-32).
#[derive(Debug, Clone)]
pub struct Shared {
    lane: Lane,
    /// A datagram's tag byte and payload, or a stream frame's length
    /// prefix and payload.
    wire: Bytes,
    /// The payload's own length, for the lane's budget.
    len: usize,
}

impl Shared {
    /// `payload` for `lane`.
    #[must_use]
    pub fn new(lane: Lane, payload: &[u8]) -> Self {
        Self::of(lane, &[payload])
    }

    /// A relayed media packet for `lane`: `sender`'s id, then the
    /// packet as it arrived — [`protocol::wrap_sender`]'s layout,
    /// without the copy in between.
    #[must_use]
    pub fn relayed(lane: Lane, sender: u64, packet: &[u8]) -> Self {
        Self::of(lane, &[&sender.to_le_bytes(), packet])
    }

    fn of(lane: Lane, parts: &[&[u8]]) -> Self {
        let len: usize = parts.iter().map(|part| part.len()).sum();
        let mut wire = match lane {
            Lane::Voice | Lane::SystemAudio => {
                let mut wire = BytesMut::with_capacity(1 + len);
                wire.extend_from_slice(&[lane.tag()]);
                wire
            }
            Lane::Control | Lane::Signal | Lane::Video | Lane::Media => {
                let mut wire = BytesMut::with_capacity(frame::PREFIX_BYTES + len);
                wire.extend_from_slice(&frame::prefix(len));
                wire
            }
        };
        for part in parts {
            wire.extend_from_slice(part);
        }
        Self {
            lane,
            wire: wire.freeze(),
            len,
        }
    }

    /// The payload's length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// ── The driver ──────────────────────────────────────────────────────────

enum Role {
    Client {
        config: ClientConfig,
        verifier: Arc<PinnedVerifier>,
    },
    // Made by `Driver::new_server`, which a client build leaves out.
    #[cfg_attr(not(any(test, feature = "server")), allow(dead_code))]
    Server {
        max_peers: usize,
        /// Room for connections the application has not admitted yet,
        /// on top of `max_peers`; `None` counts every connection
        /// against `max_peers` alike.
        waiting: Option<WaitingRoom>,
    },
}

/// Where a server keeps connections its application has not admitted
/// yet ([`Driver::admit`]): a few seats of their own, so a full server
/// can still shake hands and say *why* it refuses — or let a returning
/// user's new connection replace their old one — and so the seats of
/// the admitted cannot be taken by connections that never say a word
/// (review P3-5, plan SEC-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitingRoom {
    /// Unadmitted connections at once, from anywhere.
    pub seats: usize,
    /// …and from one address (an IPv6 /64 counts as one): one host
    /// opening connections in a loop cannot fill the room.
    pub per_address: usize,
}

impl WaitingRoom {
    /// Above this many waiting connections a new one must prove its
    /// source address first (a QUIC Retry): half the room. A forged
    /// source cannot answer one, so a flood of forged first packets
    /// stops taking seats here.
    fn retry_above(self) -> usize {
        self.seats / 2
    }
}

/// What one address is counted under in the waiting room: an IPv4
/// address as it is, an IPv6 address by its /64 (one subscriber holds
/// a whole /64).
fn address_group(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
            IpAddr::V4,
        ),
    }
}

/// What to do with a first packet.
enum Gate {
    Accept,
    /// Answer with a Retry: come back with proof of the address.
    Retry,
    Refuse,
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
    /// Peers the application queued something for since their last
    /// drain, and the latest time it did: they are drained once, when
    /// the host next asks for transmits or timers — not once per send.
    /// A keyframe relayed to a viewer was thirty sends and thirty
    /// drains of that viewer's connection; several speakers' packets in
    /// one pass now leave in one UDP datagram (review P2-31).
    dirty: Vec<ConnectionHandle>,
    dirty_at: Option<Instant>,
    /// Connections refused for want of a seat.
    refused: u64,
    /// First packets answered with a Retry (see [`WaitingRoom`]).
    retried: u64,
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
    #[cfg(any(test, feature = "server"))]
    pub fn new_server(
        identity: &crate::identity::ServerIdentity,
        max_peers: usize,
        budget: Budget,
    ) -> Result<Self, IdentityError> {
        use quinn_proto::{crypto::rustls::QuicServerConfig, ServerConfig};
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
            Role::Server {
                max_peers,
                waiting: None,
            },
            budget,
        ))
    }

    /// Give a server a [`WaitingRoom`]: `max_peers` then bounds the
    /// connections the application admitted — which it enforces, with
    /// [`Self::admit`] — and the room bounds the rest. No effect on a
    /// client.
    #[must_use]
    pub fn with_waiting_room(mut self, room: WaitingRoom) -> Self {
        if let Role::Server { waiting, .. } = &mut self.role {
            *waiting = Some(room);
        }
        self
    }

    /// The application let `peer` in: its seat in the waiting room is
    /// free for the next arrival.
    pub fn admit(&mut self, peer: PeerId) {
        if let Some(handle) = self.ids.get(&peer)
            && let Some(p) = self.peers.get_mut(handle)
        {
            p.admitted = true;
        }
    }

    /// Whether a first packet from `remote` gets a connection.
    fn gate(&self, remote: SocketAddr, validated: bool, may_retry: bool) -> Gate {
        let Role::Server { max_peers, waiting } = &self.role else {
            return Gate::Refuse;
        };
        let Some(room) = waiting else {
            return if self.peer_count() >= *max_peers {
                Gate::Refuse
            } else {
                Gate::Accept
            };
        };
        let group = address_group(remote.ip());
        let (mut waiting, mut from_here) = (0usize, 0usize);
        for p in self.peers.values().filter(|p| !p.lost && !p.admitted) {
            waiting += 1;
            if address_group(p.conn.remote_address().ip()) == group {
                from_here += 1;
            }
        }
        if waiting >= room.seats
            || from_here >= room.per_address
            || self.peer_count() >= max_peers.saturating_add(room.seats)
        {
            Gate::Refuse
        } else if waiting >= room.retry_above() && !validated && may_retry {
            Gate::Retry
        } else {
            Gate::Accept
        }
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
            dirty: Vec::new(),
            dirty_at: None,
            refused: 0,
            retried: 0,
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

    #[must_use]
    pub fn remote_address(&self, peer: PeerId) -> Option<SocketAddr> {
        self.peer(peer).map(|p| p.conn.remote_address())
    }

    #[must_use]
    pub fn stats(&self, peer: PeerId) -> Option<LinkStats> {
        self.peer(peer).map(Peer::stats)
    }

    /// The group counters of one outgoing video track toward `peer`;
    /// zeroes for a track that has not opened a group yet.
    #[must_use]
    pub fn track_stats(&self, peer: PeerId, track: Track) -> Option<TrackStats> {
        self.peer(peer)
            .map(|p| p.video_tracks.get(&track).copied().unwrap_or_default())
    }

    /// Tests: refuse every stream `peer` has open toward us, the way a
    /// receiver does to one that broke the protocol (`STOP_SENDING`). No
    /// well-behaved peer gets there, so the sender's side of it has no
    /// other way to be run.
    #[cfg(test)]
    pub(crate) fn stop_incoming_streams(&mut self, now: Instant, peer: PeerId) {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&handle) {
            for id in std::mem::take(&mut p.uni_in).into_keys() {
                let _ = p.conn.recv_stream(id).stop(PROTOCOL_VIOLATION_CODE);
            }
        }
        // The frame leaves with this peer's next pass.
        self.touch(handle, now);
    }

    /// The application is done with `track` toward `peer` (the viewer
    /// unsubscribed, the sharer stopped): its counters go.
    pub fn video_forget_track(&mut self, peer: PeerId, track: Track) {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&handle) {
            p.video_tracks.remove(&track);
        }
    }

    /// Connections refused because the server, or its waiting room,
    /// was full.
    #[must_use]
    pub fn refused_count(&self) -> u64 {
        self.refused
    }

    /// First packets asked to prove their source address before
    /// getting a seat.
    #[must_use]
    pub fn retried_count(&self) -> u64 {
        self.retried
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
        // What was queued for it goes to the transport first.
        if self.peers.get(&handle).is_some_and(|p| p.dirty) {
            self.drain_peer(handle, now);
        }
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
                let gate = self.gate(
                    remote,
                    incoming.remote_address_validated(),
                    incoming.may_retry(),
                );
                let incoming = match gate {
                    Gate::Accept => incoming,
                    Gate::Refuse => {
                        self.refused += 1;
                        let t = self.endpoint.refuse(incoming, &mut self.buf);
                        self.push_transmit(&t);
                        return;
                    }
                    Gate::Retry => {
                        self.retried += 1;
                        let t = match self.endpoint.retry(incoming, &mut self.buf) {
                            Ok(t) => t,
                            // Unreachable — the gate asked `may_retry`
                            // — and a refusal is the safe answer.
                            Err(e) => self.endpoint.refuse(e.into_incoming(), &mut self.buf),
                        };
                        self.push_transmit(&t);
                        return;
                    }
                };
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

    /// The latest instant [`Self::handle_timeouts`] must run by, if no
    /// datagram arrives first (review P1-2). A loop sleeps until then or
    /// until the socket is readable, and never ticks on a fixed short
    /// interval: every datagram is handled the moment it lands, and
    /// nothing below runs late.
    ///
    /// It covers quinn's timers (loss recovery, acks, keep-alive, idle,
    /// pacing), a deferred close, the video deadlines (a group's oldest
    /// queued byte, a finished group's acks) and the media pacer's next
    /// refill. Work that waits on the peer (a blocked stream, a cwnd
    /// that is full) needs no deadline: the peer's ack or credit is a
    /// datagram, and the pass that handles it flushes. An idle
    /// connection, or a share whose bytes are all acknowledged, is due
    /// only at its next quinn timer.
    ///
    /// Poll transmits until `None` before asking: queued transmits are
    /// not a deadline.
    #[must_use]
    pub fn next_deadline(&mut self) -> Option<Instant> {
        self.flush_dirty();
        let pace = self.budget.media_pace_bytes_per_sec;
        self.peers
            .values_mut()
            .fold(None, |at, p| earliest(at, p.next_deadline(pace)))
    }

    /// Fire every timer at or before `now`, and run the work that is
    /// due (deadline resets, media pacing, deferred closes) — for the
    /// peers that have any: one whose next deadline is still ahead and
    /// that nothing was queued for is not touched. Every peer used to
    /// be drained on every call (review P2-31); what moves a peer
    /// between its deadlines is a datagram from it, and
    /// [`Self::handle_datagram`] drains it then.
    pub fn handle_timeouts(&mut self, now: Instant) {
        let pace = self.budget.media_pace_bytes_per_sec;
        let due: Vec<ConnectionHandle> = self
            .peers
            .iter_mut()
            .filter_map(|(handle, p)| {
                (p.dirty || p.next_deadline(pace).is_some_and(|at| at <= now)).then_some(*handle)
            })
            .collect();
        // Every dirty peer is among them.
        self.dirty.clear();
        self.dirty_at = None;
        for handle in due {
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

    /// The application queued something for this peer: drain it once,
    /// when the host next asks for transmits or timers.
    fn touch(&mut self, handle: ConnectionHandle, now: Instant) {
        if let Some(p) = self.peers.get_mut(&handle)
            && !p.dirty
        {
            p.dirty = true;
            self.dirty.push(handle);
        }
        self.dirty_at = Some(self.dirty_at.map_or(now, |at| at.max(now)));
    }

    /// Drain the peers [`Self::touch`] listed.
    fn flush_dirty(&mut self) {
        let Some(now) = self.dirty_at.take() else {
            return;
        };
        for handle in std::mem::take(&mut self.dirty) {
            // Drained since for another reason (a datagram from it): done.
            if self.peers.get(&handle).is_some_and(|p| p.dirty) {
                self.drain_peer(handle, now);
            }
        }
    }

    // ── Outbound datagrams ──────────────────────────────────────────

    /// The next datagram (batch) to put on the wire, if any.
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        // What the application queued since the last pass becomes
        // packets here, once per peer.
        if self.transmits.is_empty() {
            self.flush_dirty();
        }
        self.transmits.pop_front()
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
                video_send(p, OWN_TRACK, frame::encode(bytes), now);
                Ok(())
            }
            Lane::Media => Err(SendError::NotAStreamLane(lane)),
        };
        self.touch(handle, now);
        result
    }

    /// [`Self::send`] for a payload going to many peers: `shared` was
    /// put on the wire once ([`Shared`]) and each call shares its
    /// bytes.
    ///
    /// # Errors
    /// As [`Self::send`].
    pub fn send_shared(
        &mut self,
        now: Instant,
        peer: PeerId,
        shared: &Shared,
    ) -> Result<(), SendError> {
        let lane = shared.lane;
        let handle = *self.ids.get(&peer).ok_or(SendError::UnknownPeer)?;
        let p = self.peers.get_mut(&handle).ok_or(SendError::UnknownPeer)?;
        if !p.connected || p.lost {
            return Err(SendError::NotConnected);
        }
        let result = match lane {
            Lane::Voice | Lane::SystemAudio => {
                send_datagram_wire(p, lane, shared.wire.clone(), shared.len, &mut self.events)
            }
            Lane::Control | Lane::Signal => {
                let bidi = p.bidi_mut(lane).ok_or(SendError::NoStream(lane))?;
                bidi.out.push(shared.wire.clone(), now);
                Ok(())
            }
            Lane::Video => {
                video_send(p, OWN_TRACK, shared.wire.clone(), now);
                Ok(())
            }
            Lane::Media => Err(SendError::NotAStreamLane(lane)),
        };
        self.touch(handle, now);
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
                p.video_tracks.entry(track).or_default().groups_started += 1;
            } else {
                // Stream credit exhausted: the peer holds too many
                // groups open — behave as behind and skip this one. It
                // counts as started too, or a track that never opens
                // would read as healthy.
                p.counters.groups_reset_deadline += 1;
                p.counters.groups_started += 1;
                let stats = p.video_tracks.entry(track).or_default();
                stats.groups_started += 1;
                note_group_reset(p, track);
            }
        }
        self.touch(handle, now);
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
        self.touch(handle, now);
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
        self.touch(handle, now);
    }

    /// Video: send one chunk on the track's current group. Dropped
    /// silently when the group was reset (the next keyframe reopens).
    /// The relay sends with [`Self::send_video_shared`] and a sharer
    /// with [`Self::send`] on [`Lane::Video`]; this per-track form is
    /// what the simulator's tests drive.
    ///
    /// # Errors
    /// Unknown or unconnected peer.
    #[cfg(test)]
    pub(crate) fn send_video(
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
        video_send(p, track, frame::encode(bytes), now);
        self.touch(handle, now);
        Ok(())
    }

    /// [`Self::send_video`] for a chunk going to many viewers: `shared`
    /// (a [`Shared`] for [`Lane::Video`]) is framed once.
    ///
    /// # Errors
    /// Unknown or unconnected peer, or a payload made for another lane.
    pub fn send_video_shared(
        &mut self,
        now: Instant,
        peer: PeerId,
        track: Track,
        shared: &Shared,
    ) -> Result<(), SendError> {
        if shared.lane != Lane::Video {
            return Err(SendError::NotAStreamLane(shared.lane));
        }
        let handle = *self.ids.get(&peer).ok_or(SendError::UnknownPeer)?;
        let p = self.peers.get_mut(&handle).ok_or(SendError::UnknownPeer)?;
        if !p.connected || p.lost {
            return Err(SendError::NotConnected);
        }
        video_send(p, track, shared.wire.clone(), now);
        self.touch(handle, now);
        Ok(())
    }

    /// The peers with something to read ([`Self::recv`],
    /// [`Self::recv_video`]). A server asked every client for every
    /// lane on every pass: six queue lookups a client, nearly all of
    /// them empty (review P2-31).
    #[must_use]
    pub fn readable(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| {
                !(p.inbox_control.is_empty()
                    && p.inbox_signal.is_empty()
                    && p.inbox_voice.is_empty()
                    && p.inbox_system_audio.is_empty()
                    && p.inbox_media.is_empty()
                    && p.inbox_video.is_empty())
            })
            .map(|p| p.id)
            .collect()
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
        self.touch(handle, now);
        Ok(id)
    }

    /// Abort a media upload: the stream is reset (the peer's reader sees
    /// it end short and drops the partial) and the queue is freed. `true`
    /// when there was one.
    pub fn cancel_media(&mut self, now: Instant, peer: PeerId, upload: UploadId) -> bool {
        let Some(handle) = self.ids.get(&peer).copied() else {
            return false;
        };
        let Some(p) = self.peers.get_mut(&handle) else {
            return false;
        };
        let Some(i) = p.uploads.iter().position(|u| u.id == upload) else {
            return false;
        };
        let u = p.uploads.remove(i);
        let _ = p.conn.send_stream(u.stream).reset(GROUP_RESET_CODE);
        p.counters.uploads_dropped += 1;
        // The reset frame goes out with this pass, and the lanes get
        // the send window it gave back.
        self.touch(handle, now);
        true
    }

    /// `(bytes handed to the transport, total)` for an upload still in
    /// progress; `None` once it is fully written and finished (or
    /// dropped).
    #[must_use]
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
        if let Some(p) = self.peers.get_mut(&handle) {
            p.dirty = false;
        }
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
                    let stopped: Vec<Track> = p
                        .video_out
                        .iter()
                        .filter(|(_, g)| g.stream == id)
                        .map(|(track, _)| *track)
                        .collect();
                    for track in stopped {
                        p.video_out.remove(&track);
                        note_group_reset(p, track);
                    }
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
    send_datagram_wire(p, lane, d.freeze(), bytes.len(), events)
}

/// Send one datagram already carrying its lane tag; `len` is the
/// payload's own length.
fn send_datagram_wire(
    p: &mut Peer,
    lane: Lane,
    d: Bytes,
    len: usize,
    events: &mut VecDeque<Event>,
) -> Result<(), SendError> {
    if len > lane.max_frame_bytes() {
        return Err(SendError::TooLarge(lane, len));
    }
    let mut datagrams = p.conn.datagrams();
    if let Some(max) = datagrams.max_size()
        && d.len() > max
    {
        return Err(SendError::TooLarge(lane, len));
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
        Err(SendDatagramError::TooLarge) => Err(SendError::TooLarge(lane, len)),
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

/// One group on `track` was abandoned because the peer was behind:
/// counted for the connection and for the track. A track the
/// application has forgotten is not brought back by a late reset.
fn note_group_reset(p: &mut Peer, track: Track) {
    p.counters.groups_reset += 1;
    if let Some(stats) = p.video_tracks.get_mut(&track) {
        stats.groups_reset += 1;
    }
}

/// Queue one chunk, already framed, on the track's current group.
fn video_send(p: &mut Peer, track: Track, frame: Bytes, now: Instant) {
    let Some(group) = p.video_out.get_mut(&track) else {
        return; // group was reset; wait for the next keyframe
    };
    // Byte and time bounds (§6.2): a behind peer loses the group, not
    // its place in the queue.
    if group.out.overdue(now) {
        video_close_current(p, track, now, Some(ResetCause::Deadline));
        return;
    }
    group.out.push(frame, now);
    // Into the transport as far as it takes them, now: what stays
    // queued here is what the peer is behind by, and the bounds above
    // and the "newer keyframe" rule are measured on it. (A stream that
    // is gone is found by the pass's flush.)
    let _ = group.out.flush(&mut p.conn, group.stream, now);
}

/// Finish the track's current outgoing group, or reset it for `cause`.
/// With no cause given, a group that still has unsent bytes is reset
/// anyway — that is the "newer keyframe exists" rule — and counted as
/// superseded.
fn video_close_current(p: &mut Peer, track: Track, now: Instant, cause: Option<ResetCause>) {
    let Some(mut group) = p.video_out.remove(&track) else {
        return;
    };
    if cause.is_none() {
        // A last try: only bytes the transport will not take count as
        // unsent.
        let _ = group.out.flush(&mut p.conn, group.stream, now);
    }
    let cause = match cause {
        Some(c) => Some(c),
        None if !group.out.is_empty() => {
            p.counters.groups_reset_superseded += 1;
            note_group_reset(p, track);
            let _ = p.conn.send_stream(group.stream).reset(GROUP_RESET_CODE);
            return;
        }
        None => None,
    };
    if let Some(cause) = cause {
        match cause {
            ResetCause::Deadline => {
                p.counters.groups_reset_deadline += 1;
                note_group_reset(p, track);
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
        track,
        finished_at: now,
    });
}

/// Per-tick flush of every outgoing queue, in lane priority order.
fn flush_peer(p: &mut Peer, now: Instant, media_pace: usize) {
    // A reset hands its unsent bytes' share of the send window back at
    // once, and nothing else would offer it to the lanes flushed before
    // it until the next datagram or timer: go round again.
    while flush_lanes(p, now) {}

    // Media: paced. Token bucket refilled from the wall clock we are
    // given; burst capped (`media_burst`).
    #[allow(clippy::cast_precision_loss)]
    {
        let dt = now
            .saturating_duration_since(p.media_last_refill)
            .as_secs_f64();
        p.media_last_refill = now;
        p.media_tokens = (p.media_tokens + dt * media_pace as f64).min(media_burst());
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

/// Flush Signal, Control and the current video groups, then apply the
/// video deadlines. `true` when a group was reset.
fn flush_lanes(p: &mut Peer, now: Instant) -> bool {
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
        if gone || (!g.out.is_empty() && g.out.overdue(now)) {
            reset_tracks.push(*track);
        }
    }
    let mut reset = !reset_tracks.is_empty();
    for track in reset_tracks {
        video_close_current(p, track, now, Some(ResetCause::Deadline));
    }
    // Finished groups not acknowledged within the deadline (plus a
    // couple of RTTs for the acks themselves) are stale: reset them so
    // the transport frees their send-window share for fresh groups.
    let rtt = p.conn.rtt();
    let mut i = 0;
    while i < p.video_finishing.len() {
        let g = &p.video_finishing[i];
        if now >= g.due_at(rtt) {
            let track = g.track;
            let _ = p.conn.send_stream(g.stream).reset(GROUP_RESET_CODE);
            p.counters.groups_reset_unacked += 1;
            note_group_reset(p, track);
            p.video_finishing.remove(i);
            reset = true;
            continue;
        }
        i += 1;
    }
    reset
}

/// The media bucket's cap: two frames, so an idle bucket cannot blast a
/// backlog we were pacing to avoid.
fn media_burst() -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        2.0 * (protocol::IMAGE_CHUNK_BYTES + frame::PREFIX_BYTES) as f64
    }
}

/// When the media bucket, holding `tokens` at `last_refill`, will hold
/// `cost`. `None` when it already does (the upload waits on its stream,
/// not the pacer) or never will (a frame over the burst, no pace).
fn media_refill_at(tokens: f64, last_refill: Instant, cost: f64, pace: usize) -> Option<Instant> {
    if tokens >= cost || pace == 0 || cost > media_burst() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let wait = Duration::from_secs_f64((cost - tokens) / pace as f64);
    // A microsecond over, so float rounding never wakes the loop a hair
    // early to find the bucket still short.
    Some(last_refill + wait + Duration::from_micros(1))
}

/// The earlier of two optional instants.
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
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
