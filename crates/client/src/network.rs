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
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{AudioPacket, IncomingSender, PayloadType};
use crossbeam_channel::{Receiver, RecvError, Sender, TryRecvError};
use log::{debug, info, warn};
use protocol::{
    features, pack_system_audio, pack_video_frame, pack_voice_with_flags, parse_system_audio,
    parse_voice, split_sender, AdminError, AdminOutcome, AudioSample, Auth, ChannelDenyReason,
    ChannelInfo, ChannelUiDenyReason, ClientId, ClientInfo, ClientKeyProof, ClientMessage, ConvRef,
    DenyReason, HistoryMessage, InviteDenyReason, InviteInvalidReason, Lane, NeedInviteReason,
    PathSample, RefusedReason, RemovedKind, RestrictionKind, RoleId, ServerMessage, ShareScope,
    SharingSample, TelemetryEvent, TelemetrySample, UserId, VideoReassembler, ViewingSample,
    VOICE_FLAG_RED,
};
use transport::{
    identity::{CLIENT_AUTH_EXPORTER_BYTES, CLIENT_AUTH_EXPORTER_LABEL},
    Budget, ClientKey, DisconnectReason, Driver, Event, Fingerprint, LinkStats, Observation,
    PeerId, Trust, UdpIo, UploadId, VideoItem, OWN_TRACK,
};

use crate::{
    inbound_wake::InboundWake,
    screen::{ScreenControl, ScreenTx},
    upload_turns::{QueuedBlob, UploadTurns},
    voice::VoiceControl,
    WakeFn,
};

/// The loop's tick where nothing announces a datagram (the inbound
/// wake could not start) and the socket must be polled.
const BUSY_TICK: Duration = Duration::from_millis(1);

/// Passes of the network loop since the process started: each is one
/// wake of the thread. For the battery tests and diagnostics.
static LOOP_PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times the network thread has woken so far.
#[must_use]
pub fn loop_passes() -> u64 {
    LOOP_PASSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times the inbound watcher threads (`inbound_wake`) have
/// woken so far: the rest of a quiet session's wakes.
#[must_use]
pub fn watcher_wakes() -> u64 {
    crate::inbound_wake::wakes()
}

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
/// A gap this long between voice packets ends a run of them: the
/// server's report interval, so every interval of a run held voice.
const VOICE_RUN_GAP: Duration = Duration::from_secs(2);

/// Longest a relayed image may sit half-assembled before it is
/// forgotten (its sender vanished or gave up mid-pace).
const IMAGE_PARTIAL_TTL: Duration = Duration::from_mins(2);

/// Rows of admin listings held while their answers are still arriving
/// (`AdminRows`). A server sends at most a megabyte for one answer;
/// this is far past what that holds.
const MAX_HELD_ADMIN_ROWS: usize = 50_000;

/// The pages of admin listings that arrived ahead of their
/// `AdminResult`, by request id (feature bit `ADMIN_PAGES`). The
/// result closes an answer: the app is handed one list, in the order
/// the server sent it, and never half of one.
#[derive(Default)]
struct AdminPages {
    held: HashMap<u32, protocol::AdminOutcome>,
}

impl AdminPages {
    /// One more page of the answer to `request_id`.
    fn hold(&mut self, request_id: u32, rows: protocol::AdminOutcome) {
        let held: usize = self.held.values().map(protocol::AdminOutcome::rows).sum();
        if held.saturating_add(rows.rows()) > MAX_HELD_ADMIN_ROWS {
            warn!("admin rows for request {request_id} dropped: {held} already held");
            return;
        }
        // `prepend` puts its argument in front: the new page takes
        // what is held and replaces it.
        let mut joined = rows;
        if let Some(earlier) = self.held.remove(&request_id)
            && !joined.prepend(earlier)
        {
            warn!("admin rows for request {request_id} changed kind; kept the newer");
        }
        self.held.insert(request_id, joined);
    }

    /// The result for `request_id` landed: put the held pages in front
    /// of its rows. A refusal drops them.
    fn close(
        &mut self,
        request_id: u32,
        result: &mut Result<protocol::AdminOutcome, protocol::AdminError>,
    ) {
        let Some(earlier) = self.held.remove(&request_id) else {
            return;
        };
        if let Ok(outcome) = result
            && !outcome.prepend(earlier)
        {
            warn!("admin rows for request {request_id} do not match its result");
        }
    }
}

/// One "send my log" upload per this interval, mirroring the server's
/// acceptance window.
pub const LOG_UPLOAD_MIN_INTERVAL: Duration = Duration::from_mins(10);

// ── Messages sent to the UI / App layer ─────────────────────────────────

/// Why a session ended for good. The UI maps each to a sentence a
/// person can act on; nothing here is meant to be displayed raw.
/// Not `Copy`: a 1.1 refusal (`Banned`) carries its reason text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectKind {
    /// The user clicked Disconnect.
    UserRequested,
    /// The server refused admission. The verdicts with their own
    /// prompt (`NeedInvite`, `InviteInvalid`, `NameInUse`) are reported
    /// as their own messages instead.
    Refused(RefusedReason),
    /// The server ended an admitted session on purpose (spec §4.1).
    Removed {
        kind: RemovedKind,
        reason: String,
        by: String,
    },
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
    /// The server closed us on purpose because it is restarting (a
    /// deploy). It will be back in seconds.
    ServerRestart,
}

/// The close reason the server sends when it is shutting down for a
/// restart. Wire contract with the server binary; do not reword.
const SERVER_RESTART_REASON: &str = "server restarting";

/// The first bytes of an id, for log lines.
fn hex_short(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().take(8).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// What the session produces for the other worker threads, not for the
/// app: media to play and the link feedback the encoders steer by. They
/// were variants of [`NetworkMessage`], which the app then had to name
/// in an arm that never ran (review P2-53).
enum Hot {
    VoiceData {
        client_id: ClientId,
        seq: u32,
        timestamp: u32,
        data: Vec<u8>,
        /// `data` is a redundant audio bundle (`VOICE_FLAG_RED`), not a
        /// bare Opus packet.
        red: bool,
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
        /// The counts behind `behind_pct`, and the transport's packet
        /// loss over the same window (the sharer's summary line).
        groups_started: u32,
        groups_reset: u32,
        path_loss_pct: f32,
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
    KeyframeRequested { viewer_id: ClientId },
    /// Aggregated viewer health for the stream we are sharing
    /// (routed to screen thread, not app — shown in the sharer's stats).
    StreamHealth {
        worst_loss: f32,
        worst_rtt_ms: f32,
        viewers: u16,
    },
}

/// One thing a pass of the session produced, in the order it happened.
enum Routed {
    App(NetworkMessage),
    Hot(Hot),
}

impl From<NetworkMessage> for Routed {
    fn from(msg: NetworkMessage) -> Self {
        Routed::App(msg)
    }
}

impl From<Hot> for Routed {
    fn from(hot: Hot) -> Self {
        Routed::Hot(hot)
    }
}

/// What the session has produced since the last poll.
#[derive(Default)]
struct Outbox(Vec<Routed>);

impl Outbox {
    fn push(&mut self, item: impl Into<Routed>) {
        self.0.push(item.into());
    }

    /// The messages for the app among them.
    fn for_app(&self) -> impl Iterator<Item = &NetworkMessage> {
        self.0.iter().filter_map(|item| match item {
            Routed::App(msg) => Some(msg),
            Routed::Hot(_) => None,
        })
    }
}

#[derive(Debug, Clone)]
pub enum NetworkMessage {
    /// Progress of one of our uploads, 0–100, named so the UI moves the
    /// ring on that upload's own card (review P1-17). 100 means the
    /// transport has taken every byte.
    UploadProgress {
        upload: UploadKey,
        pct: u8,
    },
    /// Admitted by the server (`Welcome` received).
    /// Admitted. `lobby`: the server runs the default channel as a
    /// lobby (spec §12, 1.1.5) — hide it and render "joined <room>".
    Connected {
        lobby: bool,
        /// The server takes blocks, reports and account deletion
        /// (1.2, `features::SAFETY`): the UI offers them only then.
        safety: bool,
    },
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
    /// This device's key is unknown to the server (or was unlinked):
    /// the UI shows the invite-code field. `KeyRetired` needs an admin
    /// to re-link the key; a code will not help.
    NeedInvite {
        why: NeedInviteReason,
    },
    /// The code we sent did not admit: the UI says which and keeps the
    /// field.
    InviteInvalid {
        why: InviteInvalidReason,
    },
    /// Someone online already has that nickname (or a member holds it
    /// and we are a guest): pick another.
    NameInUse,
    /// Who we are on this server, right after `Welcome` (spec §10).
    Session {
        user_id: UserId,
        /// Bitset from `protocol::permissions`.
        permissions: u64,
        /// `false`: the server forgets roles, bans and invites when it
        /// restarts; the UI shows a standing notice.
        persistent: bool,
    },
    /// Answer to `NetworkControl::CreateInvite`.
    InviteCreated {
        code: String,
        grants_role: RoleId,
        /// Milliseconds since the Unix epoch; `None` = never.
        expires_at: Option<u64>,
        max_uses: Option<u32>,
    },
    InviteDenied {
        reason: InviteDenyReason,
    },
    /// Who a session is, for the roster (spec §8): after each
    /// `UserJoined` and once per user after `Init`.
    UserDetails {
        client_id: ClientId,
        user_id: UserId,
        roles: Vec<RoleId>,
        level: u16,
    },
    /// A restriction now applies to us (spec §4.2).
    Restricted {
        kind: RestrictionKind,
        until: Option<u64>,
        reason: String,
    },
    /// A restriction on us ended.
    RestrictionLifted {
        kind: RestrictionKind,
    },
    /// The answer to `NetworkControl::Admin`.
    AdminResult {
        request_id: u32,
        result: Result<AdminOutcome, AdminError>,
    },
    /// A page of stored messages, oldest first (spec §6).
    History {
        conv: ConvRef,
        messages: Vec<HistoryMessage>,
        more: bool,
    },
    HistoryUnavailable {
        conv: ConvRef,
        retry_secs: u16,
    },
    MessageEdited {
        msg_id: u64,
        text: String,
    },
    MessageDeleted {
        msg_id: u64,
    },
    ChatRateLimited {
        retry_ms: u32,
    },
    /// Our xp crossed a level (spec §9.1).
    ProgressUpdate {
        xp: u64,
        level: u16,
    },
    /// Where a session's user stands (spec §9.1), for the card's bar.
    UserProgress {
        client_id: u64,
        xp: u64,
        level: u16,
    },
    /// The server's role table (spec §4), custom roles included.
    RoleList {
        roles: Vec<protocol::RoleInfo>,
    },
    /// An audit line as it happens (spec §8), for the admin channel.
    AuditLine {
        entry: protocol::AuditEntry,
    },
    /// A file in a conversation (spec §7): a card, from `username`.
    FileMessage {
        conv: protocol::ConvRef,
        from: UserId,
        username: String,
        id: u64,
        ts: u64,
        client_id: u64,
        attachment: protocol::Attachment,
    },
    /// A blob fetch is `pct` of the way in (a Save's download).
    BlobProgress {
        blob_id: protocol::BlobId,
        pct: u8,
    },
    /// A blob fetch cannot complete: too large for the reassembler,
    /// or the bytes did not match their hash.
    BlobFailed {
        blob_id: protocol::BlobId,
    },
    /// The cards of a history page, by message id.
    HistoryAttachments {
        conv: protocol::ConvRef,
        items: Vec<(u64, protocol::Attachment)>,
    },
    /// Per-channel facts `ChannelInfo` cannot carry (spec §5).
    ChannelDetails {
        channel_id: u16,
        stable_id: u64,
        owner: Option<UserId>,
        min_role: Option<RoleId>,
        permanent: bool,
        afk_after_min: Option<u16>,
        is_afk: bool,
    },
    ChannelDenied {
        reason: ChannelUiDenyReason,
    },
    /// The server moved us to the AFK channel.
    MovedIdle {
        after_min: u16,
    },
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
    /// A general-chat message. `id` is the server's message id — the
    /// handle for edit/delete and for matching `MessageEdited` /
    /// `MessageDeleted` to a row; `ts` is the server wall clock in
    /// milliseconds since the Unix epoch.
    TextMessage {
        client_id: ClientId,
        id: u64,
        ts: u64,
        username: String,
        text: String,
        /// The author's account, when the server says (1.2's
        /// `ChatFrom`): someone in the lobby has no `UserDetails`.
        author: Option<UserId>,
    },
    /// A chat image from a user (encoded PNG/JPEG/GIF bytes, ≤
    /// `protocol::MAX_CHAT_IMAGE_BYTES`, content hash verified). Same
    /// `id` / `ts` stamping as a text message.
    ImageMessage {
        client_id: ClientId,
        id: u64,
        ts: u64,
        username: String,
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

    // ── 1.1.2: profiles and blobs (spec §7, §7.1) ────────────────────
    /// A session's look, with the roster (after its `UserDetails`).
    UserProfile {
        client_id: ClientId,
        profile: protocol::Profile,
    },
    /// A profile changed (ours included, as the server's echo).
    ProfileUpdated {
        user_id: UserId,
        profile: protocol::Profile,
    },
    ProfileDenied {
        reason: protocol::ProfileDenyReason,
    },
    UserRenamed {
        client_id: ClientId,
        name: String,
    },
    RenameDenied {
        reason: protocol::RenameDenyReason,
    },
    /// The server has the announced bytes already, or took them:
    /// either way the hash is usable now.
    UploadDone {
        upload_id: u32,
        blob_id: protocol::BlobId,
    },
    UploadRejected {
        upload_id: u32,
        reason: protocol::UploadRejectReason,
    },
    /// A fetched blob, whole and hash-verified.
    Blob {
        blob_id: protocol::BlobId,
        data: Vec<u8>,
    },
    BlobMissing {
        blob_id: protocol::BlobId,
    },

    // ── 1.1.1: direct messages (spec §6) ─────────────────────────────
    /// A live DM; `username` is the sender's session name.
    Direct {
        from: UserId,
        username: String,
        id: u64,
        ts: u64,
        text: String,
    },
    /// Our DM `client_ref` has its id.
    MessageAck {
        client_ref: u32,
        msg_id: u64,
    },
    MessageFailed {
        client_ref: u32,
        reason: protocol::DmFailReason,
    },
    /// Our DM conversations with unread counts (after the general
    /// replay on join).
    Conversations {
        dms: Vec<protocol::DmSummary>,
    },

    // ── 1.2: blocks, reports, deleting one's account ─────────────────
    /// Everyone we block, whole (on join and after every change, from
    /// any of our devices).
    Blocks {
        users: Vec<protocol::BlockedUser>,
    },
    /// The answer to `NetworkControl::Report`.
    ReportAck {
        result: Result<(), protocol::ReportRefusal>,
    },
    /// Our account is gone; the session ends right after.
    AccountDeleted,
    AccountDeleteRefused {
        reason: protocol::DeleteRefusal,
    },
    /// Who wrote what is about to arrive, for authors with no roster
    /// row: someone in the lobby (their session's name is learned here
    /// too, so the post that follows carries it), or the people behind
    /// a history page or the DM list.
    Authors {
        authors: Vec<protocol::AuthorInfo>,
    },
}

/// The name a post gets when its session was never named to us: what
/// a late introduction (`Authors`) replaces.
#[must_use]
pub fn fallback_username(client_id: ClientId) -> String {
    format!("User{client_id:x}")
}

// ════════════════════════════════════════════════════════════════════════
//  NetworkClient — session state
// ════════════════════════════════════════════════════════════════════════

/// A relayed chat image or a fetched file, coming in as paced chunks on
/// one stream — in order, so they are appended to one buffer and hashed
/// as they come. Each chunk used to be kept apart and the whole joined
/// and hashed at the end: a 50 MB file was held twice at that moment.
/// One type for both; images and files each had a copy of this (review
/// P3-14).
struct Reassembly {
    count: u32,
    /// The chunk that comes next.
    next: u32,
    bytes: Vec<u8>,
    hasher: transport::sha256::Hasher,
    /// What the whole must hash to.
    hash: [u8; 32],
    started: Instant,
    /// The percent last reported (blob fetches only).
    last_pct: u8,
    /// When the last chunk came, for the idle window.
    last_chunk: Instant,
}

/// What a chunk did to its [`Reassembly`].
enum Folded {
    /// Taken; more to come.
    More,
    /// The last one, and the hash checks out: the bytes.
    Whole(Vec<u8>),
    /// Out of turn, over the size cap, or the whole fails its hash:
    /// the transfer is dead.
    Broken(&'static str),
}

impl Reassembly {
    fn new(count: u32, hash: [u8; 32], now: Instant) -> Self {
        Self {
            count,
            next: 0,
            bytes: Vec::new(),
            hasher: transport::sha256::Hasher::new(),
            hash,
            started: now,
            last_pct: 0,
            last_chunk: now,
        }
    }

    /// Take chunk `index` of `count`; the whole may be `cap` bytes.
    fn push(&mut self, index: u32, count: u32, data: &[u8], cap: usize, now: Instant) -> Folded {
        if count != self.count || index > self.next {
            return Folded::Broken("a chunk out of turn");
        }
        if index < self.next {
            // Had already: the same file asked for twice comes down two
            // streams, and both are the same bytes.
            return Folded::More;
        }
        if self.bytes.len() + data.len() > cap {
            return Folded::Broken("larger than allowed");
        }
        self.hasher.update(data);
        self.bytes.extend_from_slice(data);
        self.next += 1;
        self.last_chunk = now;
        if self.next < self.count {
            return Folded::More;
        }
        let digest = std::mem::take(&mut self.hasher).finish();
        if digest == self.hash {
            Folded::Whole(std::mem::take(&mut self.bytes))
        } else {
            Folded::Broken("its content does not match its hash")
        }
    }

    /// How far along, in percent.
    fn pct(&self) -> u8 {
        #[allow(clippy::cast_possible_truncation)] // ≤ 100
        let pct = (u64::from(self.next) * 100 / u64::from(self.count.max(1))) as u8;
        pct
    }
}

/// An announced blob upload waiting for the server's `UploadAccept`.
struct PendingBlob {
    blob_id: [u8; 32],
    data: Vec<u8>,
}

/// An upload as the UI asked for it: the chat image (one at a time, as
/// the wire carries it), or a blob by the upload id the UI gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploadKey {
    Image,
    Blob(u32),
}

/// What we say once the transport is up.
struct PendingHello {
    username: String,
    /// Presented on a first visit (spec §3.1); a known key ignores it.
    invite_code: Option<String>,
    /// The device key: the login (spec §3).
    client_key: Arc<ClientKey>,
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
    message_queue: Outbox,
    reassembler: VideoReassembler,
    // Speaking status dedupe (only send on change)
    last_speaking_sent: bool,
    /// The chat image on the Media lane, as the transport tracks it,
    /// until the transport retires it: what `CancelImage` resets. Its
    /// own slot, so a blob upload never hides it (review P1-17).
    image_upload: Option<UploadId>,
    /// Blob uploads on the Media lane, by our upload id, until the
    /// transport retires them: `CancelUpload` resets that stream and
    /// no other.
    blob_uploads: HashMap<u32, UploadId>,
    /// The percent last reported to the UI for each upload in flight:
    /// every one reports its own, under its key.
    upload_pct: HashMap<UploadKey, u8>,
    /// Reassembly of relayed images, keyed by (sender, upload id).
    incoming_images: HashMap<(u64, u32), Reassembly>,
    /// Blobs being fetched, keyed by hash (spec §7).
    incoming_blobs: HashMap<[u8; 32], Reassembly>,
    /// The blob announced and waiting for the server's `UploadAccept`,
    /// by upload id.
    pending_blobs: HashMap<u32, PendingBlob>,
    /// Which blob is with the server, and those waiting for it to be
    /// over: the server holds one announcement a session, so two files
    /// dropped together go one after the other (`upload_turns`).
    blob_turns: UploadTurns,
    /// Blobs whose fetch was cancelled: chunks still arriving are
    /// dropped until the blob is asked for again.
    ignored_blobs: HashSet<[u8; 32]>,
    /// Rows of an admin listing that arrived ahead of its
    /// `AdminResult` (`AdminRows`), by request id.
    admin_rows: AdminPages,
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
    /// The server sends no report for an interval without our voice
    /// (`features::QUIET_REPORTS`): silence from it then means nothing.
    quiet_reports: bool,
    /// When voice last left, and when the run of it began: reports are
    /// only missed while we have been sending for a while.
    voice_sent: Option<Instant>,
    voice_since: Option<Instant>,
    /// A stall has been reported; cleared when reports resume.
    reports_stalled: bool,
    /// Rings the loop when a datagram arrives, so it can sleep between
    /// them (`inbound_wake`); `None` = poll the socket every tick.
    inbound: Option<InboundWake>,
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
        invite_code: Option<&str>,
        fingerprint: Option<Fingerprint>,
        client_key: Arc<ClientKey>,
        portable: bool,
    ) -> Result<Self, String> {
        let budget = Budget::client();
        let bind: SocketAddr = if server_addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("literal")
        } else {
            "[::]:0".parse().expect("literal")
        };
        let mut udp = UdpIo::bind(bind, budget.socket_buffer_bytes)
            .map_err(|e| format!("Failed to bind socket: {e}"))?;
        if budget.mark_voice {
            udp.mark_voice(server_addr);
        }
        let mut driver = Driver::new_client(fingerprint, budget)
            .map_err(|e| format!("Failed to set up the transport: {e}"))?;
        driver.set_max_gso_segments(udp.max_gso_segments());
        let inbound = InboundWake::start(&udp);
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
                invite_code: invite_code.map(str::to_string),
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
            message_queue: Outbox::default(),
            reassembler: VideoReassembler::new(),
            last_speaking_sent: false,
            image_upload: None,
            blob_uploads: HashMap::new(),
            upload_pct: HashMap::new(),
            incoming_images: HashMap::new(),
            incoming_blobs: HashMap::new(),
            pending_blobs: HashMap::new(),
            blob_turns: UploadTurns::default(),
            ignored_blobs: HashSet::new(),
            admin_rows: AdminPages::default(),
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
            quiet_reports: false,
            voice_sent: None,
            voice_since: None,
            reports_stalled: false,
            inbound,
        })
    }

    // ── Tick ────────────────────────────────────────────────────────

    /// Receive, process, send. Cheap when nothing happened.
    pub fn update(&mut self) {
        if self.phase == Phase::Closed {
            return;
        }
        let now = Instant::now();
        // The socket can give up on GSO at any send (review P1-14).
        self.driver
            .set_max_gso_segments(self.udp.max_gso_segments());
        match self.udp.recv_batch(|d| self.driver.handle_datagram(now, d)) {
            Ok(outcome) => {
                self.backlog = !outcome.drained;
                if outcome.drained
                    && let Some(w) = self.inbound.as_mut()
                {
                    w.drained();
                }
            }
            Err(e) => {
                warn!("UDP receive error: {e}");
                self.backlog = false;
                if let Some(w) = self.inbound.as_mut() {
                    w.drained();
                }
            }
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

    /// Datagrams were still queued on the socket after the last tick.
    pub fn has_backlog(&self) -> bool {
        self.backlog
    }

    /// How long the loop may sleep before the next tick, unless a
    /// command or (with the inbound wake) a datagram comes first: until
    /// the transport's next deadline (its timers, the video deadlines,
    /// the upload pacer) or our own 2 s link stats and 10 s telemetry,
    /// whichever is first (review P1-2). A quiet connection wakes a few
    /// times a second, a share or an upload only when something is due.
    /// Without the inbound wake nothing announces a datagram, and the
    /// socket is polled every [`BUSY_TICK`].
    pub fn loop_wait(&mut self, now: Instant) -> Duration {
        let mut due = self.last_stats_check + NETWORK_STATS_INTERVAL;
        if self.telemetry_enabled && self.phase == Phase::Admitted {
            due = due.min(self.last_telemetry + TELEMETRY_INTERVAL);
        }
        if let Some(deadline) = self.driver.next_deadline() {
            due = due.min(deadline);
        }
        let wait = due.saturating_duration_since(now);
        if self.inbound.is_some() {
            wait
        } else {
            wait.min(BUSY_TICK)
        }
    }

    /// The inbound wake's ring, for the loop's select.
    fn inbound_ready(&self) -> Option<Receiver<()>> {
        self.inbound.as_ref().map(|w| w.ready().clone())
    }

    /// The loop took a ring; the next drained receive re-arms it.
    fn took_inbound(&mut self) {
        if let Some(w) = self.inbound.as_mut() {
            w.took();
        }
    }

    /// The watcher is gone (its `poll` failed): back to polling.
    fn lost_inbound(&mut self) {
        if self.inbound.take().is_some() {
            warn!("inbound wake lost; polling the socket every {BUSY_TICK:?}");
        }
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
        // Not closed, so connecting or admitted: one flag says which.
        let was_connecting = matches!(self.phase, Phase::Connecting | Phase::AwaitingWelcome);
        self.phase = Phase::Closed;
        // The server closes a connection whose Hello never arrived: on a
        // link losing most packets the handshake can squeak through
        // while the Hello stream data does not. A network condition,
        // not a refusal — retry like any other transient loss.
        let hello_lost = was_connecting
            && matches!(reason, DisconnectReason::ApplicationClosed { reason, .. } if reason == "no hello");
        let server_restart = matches!(
            reason,
            DisconnectReason::ApplicationClosed { reason, .. } if reason == SERVER_RESTART_REASON
        );
        match reason {
            _ if server_restart => {
                info!("The server is restarting; reconnecting shortly");
                self.message_queue.push(NetworkMessage::ConnectionLost {
                    kind: LossKind::ServerRestart,
                    detail: SERVER_RESTART_REASON.to_string(),
                });
            }
            _ if hello_lost => {
                warn!("Connection attempt failed: the server did not receive our Hello in time");
                self.message_queue.push(NetworkMessage::ConnectionLost {
                    kind: LossKind::Unreachable,
                    detail: "hello not received".to_string(),
                });
            }
            transient if transient.is_transient() => {
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
            DisconnectReason::ApplicationClosed { .. } => {
                // The server closed under a `Refused` (or, once
                // admitted, a `Removed`, which closes the session before
                // this arrives); that verdict was already reported — or
                // never arrived, in which case say so.
                if !self.message_queue.for_app().any(|m| {
                    matches!(
                        m,
                        NetworkMessage::NeedInvite { .. }
                            | NetworkMessage::InviteInvalid { .. }
                            | NetworkMessage::NameInUse
                            | NetworkMessage::Disconnected { .. }
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
        let mut exported = [0u8; CLIENT_AUTH_EXPORTER_BYTES];
        let client_key = self
            .driver
            .export_keying_material(self.server, CLIENT_AUTH_EXPORTER_LABEL, b"", &mut exported)
            .map(|()| ClientKeyProof {
                public_key: hello.client_key.public_key(),
                signature: hello.client_key.sign(&exported),
            });
        if client_key.is_none() {
            warn!("No TLS exporter after the handshake; the server will refuse the Hello");
        }
        let auth = match hello.invite_code {
            Some(code) => Auth::Invite { code },
            None => Auth::Key,
        };
        let msg = ClientMessage::Hello {
            username: hello.username,
            auth,
            client_key,
            features: features::CLIENT_1_1,
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
        self.prune_transfers(now);
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
        // While our video flows, the sharer's summary line carries the
        // link (`client::viewlog::Sharing`); this line is for the rest.
        let started = stats.groups_started.saturating_sub(base.groups_started);
        let reset = stats.groups_reset.saturating_sub(base.groups_reset);
        let level = if started > 0 || reset > 0 {
            log::Level::Debug
        } else {
            log::Level::Info
        };
        if loss_pct >= LINK_LOG_LOSS_PCT || (streaming && loss_pct >= LINK_LOG_LOSS_PCT_STREAMING) {
            log::log!(
                level,
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
        // Unless none was due: a server with `QUIET_REPORTS` reports
        // only on voice it expected, so its silence counts once we have
        // been sending for as long as a stall takes to show.
        let since_report = now.duration_since(self.last_report);
        let due = !self.quiet_reports
            || (self
                .voice_sent
                .is_some_and(|at| now.duration_since(at) < VOICE_RUN_GAP)
                && self
                    .voice_since
                    .is_some_and(|at| now.duration_since(at) >= REPORT_STALL));
        if since_report >= REPORT_STALL && due {
            if !self.reports_stalled {
                warn!(
                    "No receiver report for {:.0} s (path loss {loss_pct:.0}%); holding the audio ladder down",
                    since_report.as_secs_f64()
                );
                self.reports_stalled = true;
            }
            #[allow(clippy::cast_sign_loss)] // both non-negative by construction
            self.message_queue.push(Hot::VoiceReceiverReport {
                loss_pct: UPLINK_LOSS_CLAMP_PCT as u32,
                rtt_ms: rtt_ms.max(0.0) as u32,
                exercised: true,
            });
        }

        // The sharer's leg, measured on the video itself: only groups
        // *we* abandoned because our uplink was behind.
        if reset > 0 {
            // The evidence for tuning the group deadlines: which rule
            // abandoned our groups, per 2 s window (the sharer's summary
            // line has the totals).
            debug!(
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
            self.message_queue.push(Hot::UplinkVideo {
                behind_pct,
                rtt_ms: rtt_ms as f32,
                kbps_up: kbps_up as f32,
                groups_started: u32::try_from(started).unwrap_or(u32::MAX),
                groups_reset: u32::try_from(reset).unwrap_or(u32::MAX),
                path_loss_pct: loss_pct as f32,
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
                    // A newer server's variant, or one over the size bound:
                    // skipped, but never silently (review P0-1).
                    debug!(
                        "skipped an undecodable {lane} message ({} bytes)",
                        message.len()
                    );
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
                self.quiet_reports = features::has(
                    features::negotiate(features::CLIENT_1_1, server_features),
                    features::QUIET_REPORTS,
                );
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
                self.message_queue.push(NetworkMessage::Connected {
                    lobby: features::has(server_features, features::LOBBY),
                    safety: features::has(server_features, features::SAFETY),
                });
            }
            ServerMessage::Refused { reason } => {
                warn!("Server refused us: {}", reason.describe());
                self.phase = Phase::Closed;
                let verdict = match reason {
                    RefusedReason::NeedInvite { why } => NetworkMessage::NeedInvite { why },
                    RefusedReason::InviteInvalid { why } => NetworkMessage::InviteInvalid { why },
                    RefusedReason::NameInUse => NetworkMessage::NameInUse,
                    // A 1.0 server wanted its room password: this client
                    // no longer has that door — the server is the old one.
                    RefusedReason::WrongPassword => NetworkMessage::Disconnected {
                        kind: DisconnectKind::Outdated,
                        detail: "1.0 server (password admission)".to_string(),
                    },
                    other => {
                        let detail = other.describe().to_string();
                        NetworkMessage::Disconnected {
                            kind: DisconnectKind::Refused(other),
                            detail,
                        }
                    }
                };
                self.message_queue.push(verdict);
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
                    .unwrap_or_else(|| fallback_username(client_id));
                self.message_queue.push(NetworkMessage::UserLeft {
                    client_id,
                    username,
                });
            }
            ServerMessage::Chat(msg) => {
                let username = self.username_of(msg.client_id);
                self.message_queue.push(NetworkMessage::TextMessage {
                    client_id: msg.client_id,
                    id: msg.id,
                    ts: msg.ts,
                    username,
                    text: msg.text,
                    author: None,
                });
            }
            ServerMessage::ChatImageChunk {
                client_id,
                upload_id,
                index,
                count,
                id,
                ts,
                hash,
                data,
            } => {
                // `id` / `ts` repeat on every chunk of one upload; the
                // last one to land is as good as any.
                if let Some(image) =
                    self.accept_image_chunk(now, client_id, upload_id, index, count, hash, &data)
                {
                    let username = self.username_of(client_id);
                    self.message_queue.push(NetworkMessage::ImageMessage {
                        client_id,
                        id,
                        ts,
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
                    .push(Hot::KeyframeRequested { viewer_id });
            }
            ServerMessage::StreamHealth {
                worst_loss,
                worst_rtt_ms,
                viewers,
            } => {
                self.message_queue.push(Hot::StreamHealth {
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
                self.message_queue.push(Hot::VoiceReceiverReport {
                    loss_pct,
                    rtt_ms,
                    exercised,
                });
            }
            // ── 1.1: session, removal, invites ───────────────────────
            ServerMessage::Session {
                user_id,
                permissions,
                persistent,
            } => {
                info!(
                    "Session: user {}, permissions {permissions:#x}, persistent {persistent}",
                    hex_short(&user_id)
                );
                self.message_queue.push(NetworkMessage::Session {
                    user_id,
                    permissions,
                    persistent,
                });
            }
            ServerMessage::Removed { kind, reason, by } => {
                // The verdict is the message; the server closes 300 ms
                // later, but there is nothing to wait for.
                warn!("Removed from the server ({kind:?}) by {by}: {reason}");
                self.phase = Phase::Closed;
                let detail = format!("{kind:?} by {by}: {reason}");
                self.message_queue.push(NetworkMessage::Disconnected {
                    kind: DisconnectKind::Removed { kind, reason, by },
                    detail,
                });
                self.driver.close(now, self.server, "removed");
            }
            ServerMessage::InviteCreated {
                code,
                grants_role,
                expires_at,
                max_uses,
            } => {
                info!("Invite created: {code} (role {grants_role})");
                self.message_queue.push(NetworkMessage::InviteCreated {
                    code,
                    grants_role,
                    expires_at,
                    max_uses,
                });
            }
            ServerMessage::InviteDenied { reason } => {
                warn!("Invite refused: {reason:?}");
                self.message_queue
                    .push(NetworkMessage::InviteDenied { reason });
            }
            ServerMessage::UserDetails {
                client_id,
                user_id,
                roles,
                level,
            } => {
                self.message_queue.push(NetworkMessage::UserDetails {
                    client_id,
                    user_id,
                    roles,
                    level,
                });
            }
            ServerMessage::Restricted {
                kind,
                until,
                reason,
            } => {
                warn!("Restricted ({}) until {until:?}: {reason}", kind.name());
                self.message_queue.push(NetworkMessage::Restricted {
                    kind,
                    until,
                    reason,
                });
            }
            ServerMessage::RestrictionLifted { kind } => {
                info!("Restriction lifted: {}", kind.name());
                self.message_queue
                    .push(NetworkMessage::RestrictionLifted { kind });
            }
            ServerMessage::AdminRows { request_id, rows } => {
                self.admin_rows.hold(request_id, rows);
            }
            ServerMessage::AdminResult {
                request_id,
                mut result,
            } => {
                self.admin_rows.close(request_id, &mut result);
                self.message_queue
                    .push(NetworkMessage::AdminResult { request_id, result });
            }
            ServerMessage::History {
                conv,
                messages,
                more,
            } => {
                self.message_queue.push(NetworkMessage::History {
                    conv,
                    messages,
                    more,
                });
            }
            ServerMessage::HistoryUnavailable { conv, retry_secs } => {
                warn!("History unavailable; retry in {retry_secs} s");
                self.message_queue
                    .push(NetworkMessage::HistoryUnavailable { conv, retry_secs });
            }
            ServerMessage::MessageEdited { msg_id, text } => {
                self.message_queue
                    .push(NetworkMessage::MessageEdited { msg_id, text });
            }
            ServerMessage::MessageDeleted { msg_id } => {
                self.message_queue
                    .push(NetworkMessage::MessageDeleted { msg_id });
            }
            ServerMessage::ChatRateLimited { retry_ms } => {
                warn!("Chat rate limited; retry in {retry_ms} ms");
                self.message_queue
                    .push(NetworkMessage::ChatRateLimited { retry_ms });
            }
            ServerMessage::ProgressUpdate { xp, level } => {
                info!("Progress: {xp} xp, level {level}");
                self.message_queue
                    .push(NetworkMessage::ProgressUpdate { xp, level });
            }
            ServerMessage::UserProgress {
                client_id,
                xp,
                level,
            } => {
                self.message_queue.push(NetworkMessage::UserProgress {
                    client_id,
                    xp,
                    level,
                });
            }
            ServerMessage::RoleList { roles } => {
                self.message_queue.push(NetworkMessage::RoleList { roles });
            }
            ServerMessage::AuditLine { entry } => {
                self.message_queue.push(NetworkMessage::AuditLine { entry });
            }
            ServerMessage::FileMessage {
                conv,
                from,
                msg,
                attachment,
            } => {
                let username = self.username_of(msg.client_id);
                self.message_queue.push(NetworkMessage::FileMessage {
                    conv,
                    from,
                    username,
                    id: msg.id,
                    ts: msg.ts,
                    client_id: msg.client_id,
                    attachment,
                });
            }
            ServerMessage::HistoryAttachments { conv, items } => {
                self.message_queue
                    .push(NetworkMessage::HistoryAttachments { conv, items });
            }
            ServerMessage::ChannelDetails {
                channel_id,
                stable_id,
                owner,
                min_role,
                permanent,
                afk_after_min,
                is_afk,
            } => {
                self.message_queue.push(NetworkMessage::ChannelDetails {
                    channel_id,
                    stable_id,
                    owner,
                    min_role,
                    permanent,
                    afk_after_min,
                    is_afk,
                });
            }
            ServerMessage::ChannelDenied { reason } => {
                warn!("Channel request refused: {}", reason.describe());
                self.message_queue
                    .push(NetworkMessage::ChannelDenied { reason });
            }
            ServerMessage::MovedIdle { after_min } => {
                info!("Moved to the AFK channel after {after_min} idle minutes");
                self.message_queue
                    .push(NetworkMessage::MovedIdle { after_min });
            }
            ServerMessage::UserProfile { client_id, profile } => {
                self.message_queue
                    .push(NetworkMessage::UserProfile { client_id, profile });
            }
            ServerMessage::ProfileUpdated { user_id, profile } => {
                self.message_queue
                    .push(NetworkMessage::ProfileUpdated { user_id, profile });
            }
            ServerMessage::ProfileDenied { reason } => {
                info!("Profile refused: {}", reason.describe());
                self.message_queue
                    .push(NetworkMessage::ProfileDenied { reason });
            }
            ServerMessage::UserRenamed { client_id, name } => {
                self.usernames.insert(client_id, name.clone());
                self.message_queue
                    .push(NetworkMessage::UserRenamed { client_id, name });
            }
            ServerMessage::RenameDenied { reason } => {
                info!("Rename refused: {}", reason.describe());
                self.message_queue
                    .push(NetworkMessage::RenameDenied { reason });
            }
            ServerMessage::UploadAccept { upload_id, needed } => {
                self.continue_blob_upload(now, upload_id, needed);
            }
            ServerMessage::UploadDone { upload_id, blob_id } => {
                self.message_queue
                    .push(NetworkMessage::UploadDone { upload_id, blob_id });
                self.end_blob_turn(now, upload_id);
            }
            ServerMessage::UploadRejected { upload_id, reason } => {
                info!("Upload {upload_id} rejected: {}", reason.describe());
                // The server drops whatever else comes for it.
                self.cancel_blob_upload(now, upload_id);
                self.message_queue
                    .push(NetworkMessage::UploadRejected { upload_id, reason });
            }
            ServerMessage::BlobChunk {
                blob_id,
                index,
                count,
                data,
            } => {
                if let Some(data) = self.accept_blob_chunk(now, blob_id, index, count, &data) {
                    self.message_queue
                        .push(NetworkMessage::Blob { blob_id, data });
                }
            }
            ServerMessage::BlobMissing { blob_id } => {
                self.incoming_blobs.remove(&blob_id);
                self.message_queue
                    .push(NetworkMessage::BlobMissing { blob_id });
            }
            ServerMessage::Direct { from, msg } => {
                let username = self.username_of(msg.client_id);
                self.message_queue.push(NetworkMessage::Direct {
                    from,
                    username,
                    id: msg.id,
                    ts: msg.ts,
                    text: msg.text,
                });
            }
            ServerMessage::MessageAck { client_ref, msg_id } => {
                self.message_queue
                    .push(NetworkMessage::MessageAck { client_ref, msg_id });
            }
            ServerMessage::MessageFailed { client_ref, reason } => {
                info!("DM {client_ref} failed: {}", reason.describe());
                self.message_queue
                    .push(NetworkMessage::MessageFailed { client_ref, reason });
            }
            ServerMessage::Conversations { dms } => {
                self.message_queue
                    .push(NetworkMessage::Conversations { dms });
            }
            ServerMessage::Blocks { users } => {
                self.message_queue.push(NetworkMessage::Blocks { users });
            }
            ServerMessage::ReportAck { result } => {
                if let Err(refusal) = result {
                    info!("Report not filed: {refusal:?}");
                }
                self.message_queue
                    .push(NetworkMessage::ReportAck { result });
            }
            ServerMessage::AccountDeleted => {
                info!("The server deleted our account");
                self.message_queue.push(NetworkMessage::AccountDeleted);
            }
            ServerMessage::AccountDeleteRefused { reason } => {
                info!("Account deletion refused: {reason:?}");
                self.message_queue
                    .push(NetworkMessage::AccountDeleteRefused { reason });
            }
            ServerMessage::ChatFrom { from, msg } => {
                let username = self.username_of(msg.client_id);
                self.message_queue.push(NetworkMessage::TextMessage {
                    client_id: msg.client_id,
                    id: msg.id,
                    ts: msg.ts,
                    username,
                    text: msg.text,
                    author: Some(from),
                });
            }
            ServerMessage::Authors { authors } => {
                // A session in the lobby was never in `Init` or a
                // `UserJoined`: its name comes from here, before its post.
                for author in &authors {
                    if let Some(client_id) = author.client_id {
                        self.usernames.insert(client_id, author.name.clone());
                    }
                }
                self.message_queue.push(NetworkMessage::Authors { authors });
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
            .unwrap_or_else(|| fallback_username(client_id))
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
            self.message_queue.push(Hot::VoiceData {
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
                        self.message_queue.push(Hot::VideoFrame {
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
            self.message_queue.push(Hot::SystemAudioData {
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

    /// Ask the server for an invite code (spec §3.1).
    pub fn send_create_invite(
        &mut self,
        now: Instant,
        grants_role: Option<RoleId>,
        expires_secs: Option<u64>,
        max_uses: Option<u32>,
    ) {
        self.send_control(
            now,
            &ClientMessage::CreateInvite {
                grants_role,
                expires_secs,
                max_uses,
            },
        );
    }

    /// Queue a chat image for a *paced* upload: split into
    /// [`protocol::IMAGE_CHUNK_BYTES`] chunks on one Media stream, which
    /// the transport trickles out at [`protocol::MEDIA_UPLOAD_PACE`] so
    /// the router queue voice shares never backs up behind bulk. The
    /// image slot (what `CancelImage` stops) follows the newest image,
    /// as the UI's card does: it sends one at a time.
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
                // A newer image takes the slot and the card's ring.
                self.upload_pct.remove(&UploadKey::Image);
                self.image_upload = Some(id);
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

    /// Report each upload's progress as the transport takes its bytes,
    /// and forget uploads the transport retired (acknowledged, reset, or
    /// gone with the connection): a cancel must never reach a stream
    /// that is over, or someone else's.
    fn report_upload_progress(&mut self) {
        if self.image_upload.is_none() && self.blob_uploads.is_empty() {
            return;
        }
        let Self {
            driver,
            server,
            image_upload,
            blob_uploads,
            upload_pct,
            message_queue,
            ..
        } = self;
        // Whether the upload is still the transport's.
        let mut report = |upload: UploadKey, id: UploadId| {
            let progress = driver.upload_progress(*server, id);
            let pct = match progress {
                #[allow(clippy::cast_possible_truncation)]
                Some((written, total)) if total > 0 => (written * 100 / total) as u8,
                // Nothing to send, or retired: fully acknowledged.
                _ => 100,
            };
            if upload_pct.insert(upload, pct) != Some(pct) {
                message_queue.push(NetworkMessage::UploadProgress { upload, pct });
            }
            if progress.is_none() {
                upload_pct.remove(&upload);
            }
            progress.is_some()
        };
        image_upload.take_if(|id| !report(UploadKey::Image, *id));
        blob_uploads.retain(|upload_id, id| report(UploadKey::Blob(*upload_id), *id));
    }

    /// Fold one relayed chunk into its image; `Some(bytes)` when the
    /// image completes and its content hash checks out. Bounds mirror
    /// the codec's: chunk counts and total size are capped, and a fresh
    /// `upload_id` from the same sender abandons the old partial.
    #[allow(clippy::too_many_arguments)]
    fn accept_image_chunk(
        &mut self,
        now: Instant,
        client_id: u64,
        upload_id: u32,
        index: u32,
        count: u32,
        hash: [u8; 32],
        data: &[u8],
    ) -> Option<Vec<u8>> {
        if count == 0 || count > protocol::MAX_IMAGE_CHUNKS || index >= count {
            return None;
        }
        // A newer upload from the same sender obsoletes any partial.
        self.incoming_images
            .retain(|(sender, id), _| *sender != client_id || *id == upload_id);
        let key = (client_id, upload_id);
        // Only a first chunk starts one: the rest of a transfer that
        // was dropped must not begin it again from the middle.
        if index == 0 {
            self.incoming_images
                .entry(key)
                .or_insert_with(|| Reassembly::new(count, hash, now));
        }
        let entry = self.incoming_images.get_mut(&key)?;
        if entry.hash != hash {
            self.incoming_images.remove(&key);
            return None;
        }
        match entry.push(index, count, data, protocol::MAX_CHAT_IMAGE_BYTES, now) {
            Folded::More => None,
            Folded::Whole(image) => {
                self.incoming_images.remove(&key);
                Some(image)
            }
            Folded::Broken(why) => {
                self.incoming_images.remove(&key);
                warn!("Relayed image from {client_id:#x} dropped: {why}");
                None
            }
        }
    }

    /// Fold one fetched chunk into its blob; `Some(bytes)` when whole
    /// and the hash checks out. Same bounds as images, with the blob
    /// ceiling; the hash is the blob's id, so nothing else is trusted.
    fn accept_blob_chunk(
        &mut self,
        now: Instant,
        blob_id: [u8; 32],
        index: u32,
        count: u32,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        if count == 0 || count > protocol::MAX_BLOB_CHUNKS || index >= count {
            return None;
        }
        if self.ignored_blobs.contains(&blob_id) {
            return None;
        }
        if index == 0 {
            self.incoming_blobs
                .entry(blob_id)
                .or_insert_with(|| Reassembly::new(count, blob_id, now));
        }
        let entry = self.incoming_blobs.get_mut(&blob_id)?;
        match entry.push(index, count, data, protocol::MAX_BLOB_BYTES, now) {
            Folded::More => {
                let pct = entry.pct();
                if pct != entry.last_pct {
                    entry.last_pct = pct;
                    self.message_queue
                        .push(NetworkMessage::BlobProgress { blob_id, pct });
                }
                None
            }
            Folded::Whole(bytes) => {
                self.incoming_blobs.remove(&blob_id);
                Some(bytes)
            }
            Folded::Broken(why) => {
                self.incoming_blobs.remove(&blob_id);
                warn!("Fetched file dropped: {why}");
                self.message_queue
                    .push(NetworkMessage::BlobFailed { blob_id });
                None
            }
        }
    }

    /// Give up on transfers that stopped coming: an image two minutes
    /// after it began, a file two minutes after its last chunk (idle,
    /// not age: a big file over a slow link outlives the image window).
    /// On the link-stats tick; this used to run only when a chunk of
    /// some other transfer arrived, so a partial with no successor
    /// stayed for the rest of the session.
    fn prune_transfers(&mut self, now: Instant) {
        self.incoming_images
            .retain(|_, image| now.duration_since(image.started) < IMAGE_PARTIAL_TTL);
        let stalled: Vec<[u8; 32]> = self
            .incoming_blobs
            .iter()
            .filter(|(_, blob)| now.duration_since(blob.last_chunk) >= IMAGE_PARTIAL_TTL)
            .map(|(id, _)| *id)
            .collect();
        for blob_id in stalled {
            self.incoming_blobs.remove(&blob_id);
            warn!("Fetched file dropped: nothing more of it came");
            self.message_queue
                .push(NetworkMessage::BlobFailed { blob_id });
        }
    }

    /// Upload a blob (spec §7): at once if no other is with the
    /// server, else when its turn comes (`upload_turns`).
    fn begin_blob_upload(
        &mut self,
        now: Instant,
        upload_id: u32,
        name: String,
        mime: String,
        data: Vec<u8>,
    ) {
        if u32::try_from(data.len()).is_err() {
            return;
        }
        let blob = QueuedBlob {
            upload_id,
            name,
            mime,
            data,
        };
        if let Some(blob) = self.blob_turns.ask(blob) {
            self.announce_blob(now, blob);
        }
    }

    /// Announce a blob whose turn it is: the server answers whether it
    /// needs the bytes, and `continue_blob_upload` sends them on one
    /// paced Media stream.
    fn announce_blob(&mut self, now: Instant, blob: QueuedBlob) {
        let QueuedBlob {
            upload_id,
            name,
            mime,
            data,
        } = blob;
        let blob_id = transport::sha256::digest(&data);
        // Checked when it was asked for.
        let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
        self.send_control(
            now,
            &ClientMessage::UploadBegin {
                upload_id,
                name,
                mime,
                size,
                sha256: blob_id,
            },
        );
        self.pending_blobs
            .insert(upload_id, PendingBlob { blob_id, data });
    }

    /// `upload_id` is over — stored, refused, already known, cancelled:
    /// if it was the one with the server, the next one waiting is
    /// announced.
    fn end_blob_turn(&mut self, now: Instant, upload_id: u32) {
        if let Some(next) = self.blob_turns.over(upload_id) {
            self.announce_blob(now, next);
        }
    }

    fn continue_blob_upload(&mut self, now: Instant, upload_id: u32, needed: bool) {
        let Some(pending) = self.pending_blobs.remove(&upload_id) else {
            return;
        };
        if !needed {
            // Known hash: the server's `UploadDone` will not come; the
            // announcement was the whole exchange.
            self.message_queue.push(NetworkMessage::UploadDone {
                upload_id,
                blob_id: pending.blob_id,
            });
            self.end_blob_turn(now, upload_id);
            return;
        }
        let frames = chunk_upload(
            &pending.data,
            upload_id,
            |upload_id, index, count, chunk| ClientMessage::UploadChunk {
                upload_id,
                index,
                count,
                data: chunk,
            },
        );
        let Some(frames) = frames else {
            // Nothing will come of it: the next one need not wait.
            self.end_blob_turn(now, upload_id);
            return;
        };
        match self.driver.send_media(now, self.server, frames) {
            Ok(id) => {
                self.blob_uploads.insert(upload_id, id);
                self.report_upload_progress();
            }
            Err(e) => {
                warn!("Blob upload could not start: {e}");
                self.end_blob_turn(now, upload_id);
            }
        }
    }

    /// Stop a blob upload: still waiting for its turn → never
    /// announced; not yet accepted → forgotten; in flight → its media
    /// stream is reset, so the server drops the partial. The next one
    /// waiting takes the turn (its announcement replaces this one on
    /// the server).
    fn cancel_blob_upload(&mut self, now: Instant, upload_id: u32) {
        if self.blob_turns.forget(upload_id) {
            return;
        }
        if self.pending_blobs.remove(&upload_id).is_none()
            && let Some(id) = self.blob_uploads.remove(&upload_id)
        {
            self.upload_pct.remove(&UploadKey::Blob(upload_id));
            let _ = self.driver.cancel_media(now, self.server, id);
        }
        self.end_blob_turn(now, upload_id);
    }

    /// Stop the image in flight, if any: its stream is reset.
    fn cancel_image(&mut self, now: Instant) {
        if let Some(id) = self.image_upload.take() {
            self.upload_pct.remove(&UploadKey::Image);
            let _ = self.driver.cancel_media(now, self.server, id);
        }
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
                if self
                    .voice_sent
                    .is_none_or(|at| now.duration_since(at) >= VOICE_RUN_GAP)
                {
                    self.voice_since = Some(now);
                }
                self.voice_sent = Some(now);
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
                // Counted in the sharer's summary line (abandoned groups).
                debug!(
                    "Video frame seq={seq} dropped: the current group was abandoned (uplink behind) — \
                     waiting for the next keyframe"
                );
                self.video_no_group = true;
            }
            return;
        }
        if self.video_no_group {
            debug!("Video resumed at keyframe seq={seq}");
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

    fn poll_messages(&mut self) -> Vec<Routed> {
        std::mem::take(&mut self.message_queue.0)
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

/// The running voice engine's packet handles, which the voice thread
/// hands over at each start (review P1-5): received voice goes straight
/// into the engine, stamped at the socket read, and the encoder's
/// packets straight out — no hop through the voice thread either way.
pub struct VoiceLink {
    pub incoming: IncomingSender,
    pub outgoing: Receiver<AudioPacket>,
    /// A microphone test, or Settings' meter out of a voice room: our
    /// packets are dropped, not sent.
    pub testing: Arc<AtomicBool>,
    /// An engine started with no session (`VoiceControl::Start` without
    /// a client id): none of its packets is ever sent. Fixed for the
    /// link's life — a session brings a new engine and a new link — so
    /// what a microphone heard for a meter cannot follow the user into
    /// a room.
    pub local_only: bool,
}

impl std::fmt::Debug for VoiceLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceLink").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum NetworkControl {
    Connect {
        addr: SocketAddr,
        nickname: String,
        /// The invite code for a first visit, if we have one.
        invite_code: Option<String>,
        /// The pinned server identity (`SHA256:…`); `None` = trust on
        /// first use. An unparsable value is treated as `None`.
        fingerprint: Option<String>,
        /// Our own identity key: the login.
        client_key: Arc<ClientKey>,
        /// Settings live next to the executable (telemetry).
        portable: bool,
    },
    /// Mint an invite (spec §3.1); answered by `InviteCreated` or
    /// `InviteDenied`.
    CreateInvite {
        /// `None` = guest.
        grants_role: Option<RoleId>,
        expires_secs: Option<u64>,
        max_uses: Option<u32>,
    },
    /// Announce and, if the server wants it, send a blob. Ends in
    /// `NetworkMessage::UploadDone` / `UploadRejected`.
    UploadBlob {
        upload_id: u32,
        name: String,
        mime: String,
        data: Vec<u8>,
    },
    FetchBlob {
        blob_id: protocol::BlobId,
    },
    /// Stop an upload: forgotten if not yet accepted, the stream reset
    /// if the bytes are going. Nothing answers.
    CancelUpload {
        upload_id: u32,
    },
    /// Stop the image send in flight (the stream is reset).
    CancelImage,
    /// Stop a fetch: the partial is dropped and later chunks of this
    /// blob are ignored until it is asked for again.
    CancelFetch {
        blob_id: protocol::BlobId,
    },
    Disconnect,
    /// Send a message that needs nothing else from the network thread:
    /// an edit, a channel change, an admin command, a block. Each of
    /// these was a variant here restating the wire message field for
    /// field, and an arm passing it on (review P2-53).
    Send(ClientMessage),
    SendMessage(String),
    /// Send a chat image (encoded PNG; the UI caps it at
    /// `protocol::MAX_CHAT_IMAGE_BYTES` before it gets here).
    SendImage(Vec<u8>),
    /// Upload our log to the server (the tail, ≤ 2 MiB).
    SendLog(Vec<u8>),
    /// The voice engine started (`Some`) or stopped (`None`).
    VoiceLink(Option<VoiceLink>),
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
    /// (resolving the circular dependency). `msg_tx` must be unbounded:
    /// the network thread never waits for the UI (review P1-16).
    ///
    /// # Panics
    /// If the OS refuses to create the thread.
    pub fn spawn(
        tx: Sender<NetworkControl>,
        inputs: NetworkInputs,
        msg_tx: Sender<NetworkMessage>,
        msg_rx: Receiver<NetworkMessage>,
        voice_tx: Sender<VoiceControl>,
        screen_tx: impl Into<ScreenTx>,
        wake: WakeFn,
    ) -> Self {
        let screen_tx = screen_tx.into();
        let join_handle = thread::Builder::new()
            .name("client-network".into())
            .spawn(move || {
                run_network_loop(
                    &inputs.control,
                    &inputs.media,
                    &msg_tx,
                    &voice_tx,
                    &screen_tx,
                    &wake,
                );
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
    voice: &mut Option<VoiceLink>,
    msg_tx: &Sender<NetworkMessage>,
    wake: &WakeFn,
) -> bool {
    let now = Instant::now();
    match ctrl {
        NetworkControl::Connect {
            addr,
            nickname,
            invite_code,
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
                invite_code.as_deref(),
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
        // A message that needs nothing from this thread but sending.
        NetworkControl::Send(message) => {
            if let Some(c) = client {
                c.send_control(now, &message);
            }
        }
        NetworkControl::SendMessage(text) => {
            if let Some(c) = client {
                c.send_message(now, text);
            }
        }
        NetworkControl::CreateInvite {
            grants_role,
            expires_secs,
            max_uses,
        } => {
            if let Some(c) = client {
                c.send_create_invite(now, grants_role, expires_secs, max_uses);
            }
        }
        NetworkControl::SendImage(data) => {
            if let Some(c) = client {
                c.send_image(now, &data);
            }
        }
        NetworkControl::UploadBlob {
            upload_id,
            name,
            mime,
            data,
        } => {
            if let Some(c) = client {
                c.begin_blob_upload(now, upload_id, name, mime, data);
            }
        }
        NetworkControl::CancelUpload { upload_id } => {
            if let Some(c) = client {
                c.cancel_blob_upload(now, upload_id);
            }
        }
        NetworkControl::CancelImage => {
            if let Some(c) = client {
                c.cancel_image(now);
            }
        }
        NetworkControl::CancelFetch { blob_id } => {
            if let Some(c) = client {
                c.incoming_blobs.remove(&blob_id);
                c.ignored_blobs.insert(blob_id);
            }
        }
        NetworkControl::FetchBlob { blob_id } => {
            if let Some(c) = client {
                c.ignored_blobs.remove(&blob_id);
            }
            if let Some(c) = client {
                c.send_control(now, &ClientMessage::FetchBlob { blob_id });
            }
        }
        NetworkControl::SendLog(data) => {
            if let Some(c) = client {
                c.send_log(now, &data);
            }
        }
        NetworkControl::VoiceLink(link) => *voice = link,
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

/// Whether anyone can see the app (`set_ui_visible`): what only feeds
/// the screen is not sent to it otherwise.
static UI_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The app went off screen (a phone in the background) or came back.
/// Off screen the link stats are not handed to the UI: they were a wake
/// of its thread every two seconds to redraw a line nobody could see
/// (review P2-48). Any thread.
pub fn set_ui_visible(visible: bool) {
    UI_VISIBLE.store(visible, Ordering::Relaxed);
}

/// Events worth a warning, said once and then counted: at most a line
/// every [`Self::EVERY`], with how many there were since the last.
#[derive(Default)]
struct Tally {
    count: u32,
    said: Option<Instant>,
}

impl Tally {
    const EVERY: Duration = Duration::from_secs(5);

    /// One more. The count to report, when a line is due.
    fn note(&mut self, now: Instant) -> Option<u32> {
        self.count += 1;
        if self
            .said
            .is_some_and(|at| now.duration_since(at) < Self::EVERY)
        {
            return None;
        }
        self.said = Some(now);
        Some(std::mem::take(&mut self.count))
    }
}

/// Route one thing the session produced to whoever consumes it: media
/// and link feedback to the voice/screen threads (hot paths, bypassing
/// the app), everything else to the app.
fn route_message(
    msg: Routed,
    msg_tx: &Sender<NetworkMessage>,
    voice: Option<&VoiceLink>,
    voice_tx: &Sender<VoiceControl>,
    screen_tx: &ScreenTx,
    wake: &WakeFn,
    frames_dropped: &mut Tally,
) {
    let msg = match msg {
        Routed::Hot(hot) => {
            route_hot(hot, voice, voice_tx, screen_tx, frames_dropped);
            return;
        }
        Routed::App(msg) => msg,
    };
    match msg {
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

        // Link stats → app (UI only): nothing to show them on while
        // the app is off screen.
        NetworkMessage::NetworkStats { .. } => {
            if UI_VISIBLE.load(Ordering::Relaxed) {
                let _ = msg_tx.send(msg);
                wake();
            }
        }

        // Everything else → app. These carry state (join / leave, init,
        // chat, mute states, denials, identity) — losing one desyncs
        // the UI until reconnect, so never `try_send`. The app's channel
        // is unbounded (review P1-16), so this never blocks the network
        // thread either; the app drains it every event-loop pass.
        other => {
            let _ = msg_tx.send(other);
            wake();
        }
    }
}

/// Media and link feedback, straight to the thread that uses them.
fn route_hot(
    hot: Hot,
    voice: Option<&VoiceLink>,
    voice_tx: &Sender<VoiceControl>,
    screen_tx: &ScreenTx,
    frames_dropped: &mut Tally,
) {
    match hot {
        // Voice → the engine, stamped here at the socket read (review
        // P1-5). No engine (it is starting, or stopped): dropped.
        Hot::VoiceData {
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
            if let Some(link) = voice {
                let packet = AudioPacket::new_typed(seq, timestamp, client_id, payload_type, data);
                let _ = link.incoming.send(packet);
            }
        }

        // Video → screen thread
        Hot::VideoFrame {
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
                // Once, then counted: a screen thread that is behind
                // drops every frame, and each was a line (review P3-15).
                log::debug!("Screen channel: dropped frame seq={seq} keyframe={is_keyframe}: {e}");
                if let Some(n) = frames_dropped.note(Instant::now()) {
                    warn!("The screen thread is behind: {n} video frame(s) dropped before it");
                }
            }
        }

        // System audio → screen thread
        Hot::SystemAudioData {
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

        // Keyframe request from a viewer → screen thread
        Hot::KeyframeRequested { viewer_id } => {
            // The sharer's summary line counts these (review P3-15).
            debug!("Keyframe requested by viewer {viewer_id:#x}");
            let _ = screen_tx.try_send(ScreenControl::ForceKeyframe);
        }

        // The relay's account of our voice → voice thread: this is the
        // encoder's only loss input (spec §6.1). RTT lets its rate
        // controller tell congestion (loss + RTT inflation: send fewer
        // bytes) from random loss (add redundancy).
        Hot::VoiceReceiverReport {
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
        Hot::UplinkVideo {
            behind_pct,
            rtt_ms,
            kbps_up,
            groups_started,
            groups_reset,
            path_loss_pct,
        } => {
            let _ = screen_tx.try_send(ScreenControl::UplinkStats {
                loss_pct: behind_pct,
                rtt_ms,
                kbps_up,
                groups_started,
                groups_reset,
                path_loss_pct,
            });
        }

        // Viewer health for our stream → screen thread (sharer's stats)
        Hot::StreamHealth {
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
    }
}

/// Our share's media on its way to the wire: what the screen thread
/// and the system-audio encoder hand the network thread. It has its
/// own bounded channel ([`MEDIA_QUEUE`]), filled by `try_send`, so a
/// burst of frames can drop frames but can never sit in front of the
/// UI's commands or make a sender wait (review P1-16).
#[derive(Debug)]
pub enum MediaOut {
    /// An encoded H.264 frame for the video lane.
    VideoFrame {
        seq: u32,
        is_keyframe: bool,
        width: u16,
        height: u16,
        /// Capture time, ms (wraps at u32; viewers unwrap it).
        timestamp_ms: u32,
        data: Vec<u8>,
    },
    /// An Opus stereo system-audio packet for the system audio lane.
    SystemAudio {
        seq: u32,
        channels: u8,
        duration_ms: u16,
        data: Vec<u8>,
    },
}

/// What the network thread reads besides its socket: the commands, and
/// our share's media on its own bounded channel.
pub struct NetworkInputs {
    pub control: Receiver<NetworkControl>,
    pub media: Receiver<MediaOut>,
}

/// Slots in the outbound media channel: about 2 s of a 60 fps share
/// with its sound. A network thread that far behind is not going to
/// send those frames in time to matter.
pub const MEDIA_QUEUE: usize = 256;

/// Media handed to the transport in one loop pass before it flushes.
const MEDIA_BURST: usize = 64;

/// What woke the network loop.
enum Woke {
    Control(NetworkControl),
    /// A frame or a sound packet of our own share.
    Media(MediaOut),
    /// The voice encoder's next packet, or its channel closing with
    /// the engine.
    Voice(Result<AudioPacket, RecvError>),
    /// A datagram rang, a deadline came, or there was no waiting.
    Due,
    Gone,
}

#[allow(clippy::too_many_lines)] // one loop: wait, commands, media, voice, poll
fn run_network_loop(
    control_rx: &Receiver<NetworkControl>,
    media_rx: &Receiver<MediaOut>,
    msg_tx: &Sender<NetworkMessage>,
    voice_tx: &Sender<VoiceControl>,
    screen_tx: &ScreenTx,
    wake: &WakeFn,
) {
    let mut client: Option<NetworkClient> = None;
    let mut voice: Option<VoiceLink> = None;
    let no_voice = crossbeam_channel::never::<AudioPacket>();
    let no_inbound = crossbeam_channel::never::<()>();
    let mut frames_dropped = Tally::default();
    let mut running = true;

    while running {
        LOOP_PASSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Block on what can have work for us: a command, our share's
        // next frame or sound packet (review P1-16), the voice encoder's
        // next packet (review P1-5), an arriving datagram through the inbound
        // wake (`inbound_wake.rs`). The timeout is `loop_wait`: until the
        // transport's next deadline or our own periodic stats, so acks,
        // keep-alives, video deadlines and the upload pacer are never
        // late and nothing ticks for nothing (review P1-2); 1 ms only
        // when nothing announces datagrams and the socket must be
        // polled. Disconnected there is no deadline, so no timeout.
        let outgoing = voice.as_ref().map_or(&no_voice, |v| &v.outgoing);
        let woke = match &mut client {
            Some(c) if c.has_backlog() => match control_rx.try_recv() {
                Ok(ctrl) => Woke::Control(ctrl),
                // Media waiting is picked up by the drain below.
                Err(TryRecvError::Empty) => Woke::Due,
                Err(TryRecvError::Disconnected) => Woke::Gone,
            },
            Some(c) => {
                let wait = c.loop_wait(Instant::now());
                let ready = c.inbound_ready();
                crossbeam_channel::select! {
                    recv(control_rx) -> msg => msg.map_or(Woke::Gone, Woke::Control),
                    recv(media_rx) -> media => media.map_or(Woke::Due, Woke::Media),
                    recv(outgoing) -> packet => Woke::Voice(packet),
                    recv(ready.as_ref().unwrap_or(&no_inbound)) -> rang => {
                        if rang.is_ok() {
                            c.took_inbound();
                        } else {
                            c.lost_inbound();
                        }
                        Woke::Due
                    },
                    default(wait) => Woke::Due,
                }
            }
            // No session, nothing to send media on: what the screen
            // thread still queues is dropped as it comes.
            None => crossbeam_channel::select! {
                recv(control_rx) -> msg => msg.map_or(Woke::Gone, Woke::Control),
                recv(media_rx) -> media => media.map_or(Woke::Due, Woke::Media),
                recv(outgoing) -> packet => Woke::Voice(packet),
            },
        };

        // Commands first, whatever woke the loop: the screen thread
        // announces a share on the command channel before its first
        // frame goes on the media one, and that order must hold here.
        let mut first_media = None;
        match woke {
            Woke::Control(ctrl) => {
                if !handle_control(ctrl, &mut client, &mut voice, msg_tx, wake) {
                    running = false;
                }
            }
            Woke::Media(media) => first_media = Some(media),
            Woke::Voice(Ok(packet)) => send_voice(client.as_mut(), voice.as_ref(), &packet),
            // The engine is gone; the voice thread says so too, or
            // hands over the next one.
            Woke::Voice(Err(RecvError)) => voice = None,
            Woke::Due => {}
            Woke::Gone => break,
        }
        // Drain a bounded burst of whatever else queued up.
        for _ in 0..63 {
            match control_rx.try_recv() {
                Ok(ctrl) => {
                    if !handle_control(ctrl, &mut client, &mut voice, msg_tx, wake) {
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
        let mut sent = 0;
        while let Some(media) = first_media.take().or_else(|| {
            (sent < MEDIA_BURST)
                .then(|| media_rx.try_recv().ok())
                .flatten()
        }) {
            sent += 1;
            send_media(client.as_mut(), media);
        }

        // Whatever else the encoder has queued, before the transport
        // flushes.
        if let Some(link) = &voice {
            loop {
                match link.outgoing.try_recv() {
                    Ok(packet) => send_voice(client.as_mut(), Some(link), &packet),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        voice = None;
                        break;
                    }
                }
            }
        }

        // Poll network
        if let Some(c) = &mut client {
            c.update();
            for msg in c.poll_messages() {
                route_message(
                    msg,
                    msg_tx,
                    voice.as_ref(),
                    voice_tx,
                    screen_tx,
                    wake,
                    &mut frames_dropped,
                );
            }
            if c.is_closed() {
                // The reason was already reported by the session.
                client = None;
            }
        }
    }
}

/// A frame or a sound packet of our share onto its lane; dropped when
/// there is no session.
fn send_media(client: Option<&mut NetworkClient>, media: MediaOut) {
    let Some(c) = client else {
        return;
    };
    let now = Instant::now();
    match media {
        MediaOut::VideoFrame {
            seq,
            is_keyframe,
            width,
            height,
            timestamp_ms,
            data,
        } => c.send_video_frame(now, seq, is_keyframe, width, height, timestamp_ms, &data),
        MediaOut::SystemAudio {
            seq,
            channels,
            duration_ms,
            data,
        } => c.send_system_audio(now, seq, channels, duration_ms, &data),
    }
}

/// One of our voice packets onto the wire, unless a microphone test
/// owns the engine, the engine was started for no session, or there
/// is no session. The packet's ssrc is not
/// sent: the server stamps the authenticated sender id on relay.
fn send_voice(client: Option<&mut NetworkClient>, link: Option<&VoiceLink>, packet: &AudioPacket) {
    let (Some(c), Some(link)) = (client, link) else {
        return;
    };
    if link.local_only || link.testing.load(Ordering::Relaxed) {
        return;
    }
    let flags = match packet.header.payload_type {
        PayloadType::Red => VOICE_FLAG_RED,
        PayloadType::Opus => 0,
    };
    c.send_voice_frame(
        Instant::now(),
        packet.header.seq,
        packet.header.timestamp,
        flags,
        &packet.opus_data,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit(ids: std::ops::Range<u64>) -> protocol::AdminOutcome {
        protocol::AdminOutcome::Audit(
            ids.map(|id| protocol::AuditEntry {
                id,
                ts: id,
                actor: None,
                target: None,
                action: "test".to_string(),
                detail: None,
            })
            .collect(),
        )
    }

    /// Review P0-1: pages arrive before the result that closes them.
    #[test]
    fn admin_pages_are_joined_in_order_when_the_result_lands() {
        let mut pages = AdminPages::default();
        pages.hold(7, audit(0..3));
        // Another answer in between keeps its own rows.
        pages.hold(8, audit(100..101));
        pages.hold(7, audit(3..5));
        let mut result = Ok(audit(5..6));
        pages.close(7, &mut result);
        assert_eq!(result, Ok(audit(0..6)));
        let mut other = Ok(audit(101..102));
        pages.close(8, &mut other);
        assert_eq!(other, Ok(audit(100..102)));
        assert!(pages.held.is_empty());

        // An answer of one message is left as it came.
        let mut single = Ok(audit(0..2));
        pages.close(9, &mut single);
        assert_eq!(single, Ok(audit(0..2)));
    }

    #[test]
    fn a_refusal_drops_the_pages_and_a_flood_is_cut() {
        let mut pages = AdminPages::default();
        pages.hold(1, audit(0..3));
        let mut refused = Err(protocol::AdminError::NoPermission);
        pages.close(1, &mut refused);
        assert_eq!(refused, Err(protocol::AdminError::NoPermission));
        assert!(pages.held.is_empty());

        // Rows of another listing under the same id do not mix in.
        pages.hold(2, protocol::AdminOutcome::Invites(Vec::new()));
        let mut result = Ok(audit(0..1));
        pages.close(2, &mut result);
        assert_eq!(result, Ok(audit(0..1)));

        let cap = MAX_HELD_ADMIN_ROWS as u64;
        pages.hold(3, audit(0..cap));
        pages.hold(3, audit(cap..cap + 1));
        let mut result = Ok(audit(cap + 1..cap + 2));
        pages.close(3, &mut result);
        let Ok(joined) = result else {
            panic!("an answer");
        };
        assert_eq!(
            joined.rows(),
            MAX_HELD_ADMIN_ROWS + 1,
            "the page past the cap was dropped"
        );
    }

    fn whole(parts: &[&[u8]]) -> [u8; 32] {
        let mut hasher = transport::sha256::Hasher::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finish()
    }

    #[test]
    fn chunks_in_order_make_the_whole_and_it_is_checked() {
        let now = Instant::now();
        let parts: [&[u8]; 3] = [&[1; 100], &[2; 100], &[3; 7]];
        let mut r = Reassembly::new(3, whole(&parts), now);
        assert!(matches!(r.push(0, 3, parts[0], 1000, now), Folded::More));
        assert_eq!(r.pct(), 33);
        assert!(matches!(r.push(1, 3, parts[1], 1000, now), Folded::More));
        let Folded::Whole(bytes) = r.push(2, 3, parts[2], 1000, now) else {
            panic!("the last chunk completes it");
        };
        assert_eq!(bytes, parts.concat());

        // The same chunks under another hash: refused at the end.
        let mut r = Reassembly::new(3, [0; 32], now);
        let _ = r.push(0, 3, parts[0], 1000, now);
        let _ = r.push(1, 3, parts[1], 1000, now);
        assert!(matches!(
            r.push(2, 3, parts[2], 1000, now),
            Folded::Broken(_)
        ));
    }

    #[test]
    fn a_chunk_out_of_turn_or_past_the_cap_breaks_the_transfer() {
        let now = Instant::now();
        let mut r = Reassembly::new(3, [0; 32], now);
        assert!(matches!(
            r.push(1, 3, &[1; 10], 1000, now),
            Folded::Broken(_)
        ));
        let mut r = Reassembly::new(3, [0; 32], now);
        assert!(matches!(r.push(0, 3, &[1; 10], 1000, now), Folded::More));
        // The same chunk again (a second stream of the same file) is
        // passed over; a gap is the end.
        assert!(matches!(r.push(0, 3, &[1; 10], 1000, now), Folded::More));
        assert_eq!(r.bytes.len(), 10);
        assert!(matches!(
            r.push(2, 3, &[1; 10], 1000, now),
            Folded::Broken(_)
        ));
        let mut r = Reassembly::new(3, [0; 32], now);
        assert!(matches!(
            r.push(0, 4, &[1; 10], 1000, now),
            Folded::Broken(_)
        ));
        let mut r = Reassembly::new(3, [0; 32], now);
        assert!(matches!(r.push(0, 3, &[1; 600], 1000, now), Folded::More));
        assert!(matches!(
            r.push(1, 3, &[1; 600], 1000, now),
            Folded::Broken(_)
        ));
    }

    #[test]
    fn warnings_are_said_once_then_counted() {
        let t0 = Instant::now();
        let mut tally = Tally::default();
        assert_eq!(tally.note(t0), Some(1), "the first, at once");
        assert_eq!(tally.note(t0 + Duration::from_secs(1)), None);
        assert_eq!(tally.note(t0 + Duration::from_secs(2)), None);
        assert_eq!(
            tally.note(t0 + Tally::EVERY),
            Some(3),
            "the two in between and this one"
        );
    }
}
