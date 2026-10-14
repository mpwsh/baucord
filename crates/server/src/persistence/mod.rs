//! The persistence thread and the relay tick's handle to it (spec §0–§1).
//!
//! **The tick never touches disk.** One thread owns the `SQLite`
//! connection; the tick talks to it through [`Persist`]:
//!
//! - writes ([`Command`]) are fire-and-forget on a bounded queue. When
//!   the queue is full — the thread is behind a slow disk — the write is
//!   dropped and counted (`dropped_writes` in the stats line). Voice,
//!   video and admission never wait.
//! - reads ([`Query`]) carry a reply channel and a deadline. The tick
//!   polls for answers every tick ([`Persist::poll`]); an answer past its
//!   deadline resolves as [`Outcome::TimedOut`] and the client gets an
//!   empty page with a retry hint. A reply that arrives later is
//!   discarded — it was already answered.
//! - telemetry is coalesced per user on the tick side and handed over
//!   as a batch; if the queue is full the batch goes back to the caller
//!   (last wins), never into a backlog.
//!
//! The thread runs in both modes. With persistence off the store is
//! memory-only: writes are discarded, reads answer
//! [`Answer::Unavailable`], and the thread exists so session-scoped work
//! (progress accrual, later) has one home.
//!
//! Housekeeping on the thread: WAL checkpoint every 60 s, retention
//! prune hourly, and a `PRAGMA data_version` poll every 2 s that
//! reloads the policy tables and hands the tick a fresh
//! [`Notice::Policy`] when another process (the CLI, or `sqlite3`)
//! committed to the file.

mod store;

use std::{
    fmt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use crossbeam_channel::{
    bounded, select_biased, unbounded, Receiver, Sender, TryRecvError, TrySendError,
};
use log::{debug, info, warn};
use protocol::{TelemetrySample, UserId};

use self::store::Sqlite;
pub use self::store::{
    backup_to_tar, blobs_dir, check, cli_create_invite, cli_find_user, cli_kick,
    cli_lift_restriction, cli_list_invites, cli_load_policy, cli_prune, cli_purge, cli_restrict,
    cli_revoke_invite, cli_role_create, cli_role_delete, cli_role_edit, cli_set_key, cli_set_roles,
    cli_telemetry, hex, parse_hex_id, restore_from_tar, BackupReport, CheckReport, RestoreReport,
    Retention, RoleEdit, TelemetryLine, GENERAL_CONV, SCHEMA_VERSION,
};
use crate::{
    clock::WallMs,
    config::PersistenceSettings,
    policy::{InviteRecord, KeyRecord, Policy, Restriction, UserChannelRecord, UserRecord},
};

/// General-chat messages the tick keeps in memory for replay and for
/// edit/delete checks (spec §6): at least `history.replay`, and enough
/// that the last few minutes of a lively room stay editable.
pub const RECENT_RING: u16 = 200;

/// Depth of the read-request queue. Reads are rare (a history page on
/// join, a DM inbox) and each carries its own reply channel.
const READ_QUEUE: usize = 256;
/// The thread wakes at least this often for housekeeping.
const CHECKPOINT_INTERVAL: Duration = Duration::from_mins(1);
const PRUNE_INTERVAL: Duration = Duration::from_hours(1);
const DATA_VERSION_INTERVAL: Duration = Duration::from_secs(2);
/// On shutdown the thread drains what is queued for at most this long;
/// the rest is counted as dropped.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);
/// How long the tick waits for the thread to finish before detaching.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(6);
/// How many snapshots in a row the tick sets aside as older than its
/// own writes before it takes one anyway. Each retry is one more read
/// of the policy tables; a server changing policy on every tick for
/// this long is not waiting for a quieter moment that will come.
const MAX_STALE_RELOADS: u32 = 4;
/// How long the tick keeps its short poll for a snapshot it asked for.
const RELOAD_WAIT: Duration = Duration::from_secs(5);
/// After [`MAX_STALE_RELOADS`] stale snapshots in a row the tick keeps
/// the policy it has and asks again this much later, off its short
/// poll, when the writes may have let up; it used to take the stale
/// one at once, dropping from memory whatever it had just changed (a
/// ban from the panel, say).
const RELOAD_BACKOFF: Duration = Duration::from_secs(1);
/// How many such pauses before a stale snapshot is taken after all: a
/// server that writes policy without a break for seconds on end still
/// gets the other process's change, a few seconds late.
const MAX_RELOAD_BACKOFFS: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("database directory {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "database schema is version {found}, but this server only knows up to {supported}: \
         a newer baucord wrote it — refusing to start rather than guess"
    )]
    Downgrade { found: u32, supported: u32 },
    #[error("no database at {0} — is persistence enabled, and has the server run once?")]
    Missing(PathBuf),
    #[error("persistence is disabled (persistence.enabled: false); nothing to operate on")]
    Disabled,
    #[error("archive {0}: {1}")]
    Archive(PathBuf, #[source] std::io::Error),
    #[error("could not start the persistence thread: {0}")]
    Thread(#[source] std::io::Error),
    #[error("no role with id {0}")]
    UnknownRole(u32),
    #[error("no user named {0}")]
    NoSuchUser(String),
    #[error(
        "'{0}' names more than one user — use an id instead (the first 8 digits do):\n  {list}",
        list = .1.join("\n  ")
    )]
    Ambiguous(String, Vec<String>),
    #[error("the last admin keeps admin")]
    LastAdmin,
    #[error("no role named {0}")]
    NoSuchRole(String),
    #[error("a role is already named {0}")]
    RoleNameTaken(String),
    #[error(
        "'{0}' cannot name a role: 1–32 lowercase letters, digits and hyphens, and not a \
         built-in's name"
    )]
    BadRoleName(String),
    #[error("the built-in role {0} cannot be changed (its permissions come from the binary)")]
    BuiltinRole(String),
    #[error("'{0}' is not a permission; the names are {list}", list = permission_name_list())]
    BadPermission(String),
    #[error("a custom role cannot hold `admin`; only the built-in admin role grants it")]
    AdminBit,
    #[error("'{0}' is not a role icon; the choices are {list}", list = role_icon_list())]
    BadIcon(String),
    #[error("the server holds {0} (its -wal file exists): stop it first, or pass --force")]
    Locked(PathBuf),
    #[error("{0} is not a baucord backup (no baucord.db inside)")]
    NotABackup(PathBuf),
}

// ── Rows ────────────────────────────────────────────────────────────────

/// One stored chat message (general or DM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub msg_id: u64,
    pub conv_id: u64,
    pub ts: WallMs,
    pub user_id: UserId,
    pub body: String,
    /// The author's name of record (read side only; the users table
    /// carries it).
    pub name: String,
    pub edited: bool,
    /// A file posted with this message (spec §7); the body is empty then.
    pub blob_id: Option<protocol::BlobId>,
}

/// A minute's worth of progress for one user (spec §9.1), added to the
/// counters.
#[derive(Debug, Clone, Default)]
pub struct ProgressDelta {
    pub user_id: UserId,
    pub xp: u64,
    pub talk_seconds: u64,
    pub share_seconds: u64,
    pub watch_seconds: u64,
    pub online_seconds: u64,
    pub messages: u64,
    pub ts: WallMs,
}

/// One telemetry sample: the columns worth querying pulled out, the
/// whole sample as a blob for everything else (spec §9).
#[derive(Debug, Clone)]
pub struct TelemetryRow {
    pub ts: WallMs,
    pub user_id: UserId,
    pub key_hash: UserId,
    pub rtt_ms: u32,
    pub path_loss: f32,
    /// The client's own smoothed uplink voice loss, when audio ran.
    pub voice_loss: Option<f32>,
    /// Video groups the sharer's own uplink abandoned, when sharing.
    pub groups_behind: Option<u64>,
    /// The sharer's bitrate rung, percent, when sharing.
    pub rung: Option<u32>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub sample: Vec<u8>,
}

impl TelemetryRow {
    /// Pull the queryable columns out of a sample. `None` if the sample
    /// does not serialize (it always does; the codec is infallible for
    /// these types).
    #[must_use]
    pub fn from_sample(
        ts: WallMs,
        user_id: UserId,
        key_hash: UserId,
        sample: &TelemetrySample,
    ) -> Option<Self> {
        let encoded = protocol::encode_telemetry_sample(sample)?;
        let audio = sample.audio.as_ref();
        let sharing = sample.sharing.as_ref();
        Some(Self {
            ts,
            user_id,
            key_hash,
            rtt_ms: sample.path.rtt_ms,
            path_loss: sample.path.loss_pct,
            voice_loss: audio.map(|a| a.smoothed_loss_pct),
            groups_behind: sharing.map(|s| s.groups_reset),
            rung: sharing.map(|s| s.rung_pct),
            input_device: audio.map(|a| a.input_device.clone()),
            output_device: audio.map(|a| a.output_device.clone()),
            sample: encoded,
        })
    }
}

/// One telemetry event.
#[derive(Debug, Clone)]
pub struct TelemetryEventRow {
    pub ts: WallMs,
    pub user_id: UserId,
    pub key_hash: UserId,
    /// A short stable name (`share_started`, …) for querying.
    pub kind: String,
    pub event: Vec<u8>,
}

/// `star, crown, …` for the `BadIcon` message.
fn role_icon_list() -> String {
    protocol::permissions::ROLE_ICONS.join(", ")
}

/// `join, speak, text, …` for the `BadPermission` message.
fn permission_name_list() -> String {
    protocol::permissions::PERMISSION_NAMES
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One report to store (`protocol::safety`, 1.2).
#[derive(Debug, Clone)]
pub struct ReportRow {
    pub ts: WallMs,
    pub reporter: UserId,
    pub target_user: Option<UserId>,
    pub target_name: String,
    pub msg_id: Option<u64>,
    pub in_dm: bool,
    pub excerpt: String,
    pub reason: protocol::ReportReason,
    pub note: String,
}

/// A stored message found for a report, with its conversation.
#[derive(Debug, Clone)]
pub struct ReportedMessage {
    pub row: MessageRow,
    pub conv: ConvKey,
}

/// One audit line (spec §8): who did what to whom.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub ts: WallMs,
    /// `None` for the server itself.
    pub actor: Option<UserId>,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

/// A blob to store: what the uploader said, what the server sniffed,
/// and the bytes (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRecord {
    pub blob_id: protocol::BlobId,
    /// Sniffed.
    pub mime: String,
    pub declared_mime: Option<String>,
    pub size: u32,
    pub name: String,
    pub created_by: Option<UserId>,
    pub created_at: WallMs,
    pub bytes: Vec<u8>,
}

// ── Commands, queries, answers ──────────────────────────────────────────

/// A fire-and-forget write.
#[derive(Debug)]
pub enum Command {
    InsertMessage(MessageRow),
    /// A batch coalesced per user by the tick.
    Telemetry(Vec<TelemetryRow>),
    TelemetryEvent(TelemetryEventRow),
    // ── Policy (spec §3–§4): the tick applied it in memory first ───
    /// Create a user, or refresh their name of record and last-seen.
    UpsertUser {
        user_id: UserId,
        user: UserRecord,
    },
    /// Link a key to a user, or refresh its last-seen / retired state.
    UpsertKey {
        key_hash: UserId,
        key: KeyRecord,
    },
    GrantRole {
        user_id: UserId,
        role_id: u32,
        granted_by: Option<UserId>,
        ts: WallMs,
    },
    RevokeRole {
        user_id: UserId,
        role_id: u32,
    },
    /// Forget every device key of a user (a kick).
    DeleteUserKeys {
        user_id: UserId,
    },
    EditMessage {
        msg_id: u64,
        body: String,
        ts: WallMs,
    },
    DeleteMessage {
        msg_id: u64,
        ts: WallMs,
    },
    UpsertChannel(UserChannelRecord),
    DeleteChannel {
        stable_id: u64,
    },
    AddProgress(ProgressDelta),
    PurgeUser {
        user_id: UserId,
    },
    InsertInvite {
        code: String,
        invite: InviteRecord,
    },
    UseInvite {
        code: String,
    },
    RevokeInvite {
        code: String,
        ts: WallMs,
    },
    InsertRestriction(Restriction),
    DeleteRestriction {
        id: u64,
    },
    Audit(AuditRow),
    /// A user's profile columns (spec §7.1).
    SetProfile {
        user_id: UserId,
        profile: protocol::Profile,
        ts: WallMs,
    },
    /// Bytes to `blobs/<hex>` and a row in `blobs` (spec §7).
    StoreBlob(Box<BlobRecord>),
    /// How many profiles point at a blob.
    /// Remove a blob outright (spec §7: eviction at the cap, an admin's
    /// delete): the row and the bytes.
    DeleteBlob {
        blob_id: protocol::BlobId,
    },
    /// A custom role from the panel (spec §4); the tick picked the id.
    InsertRole {
        id: u32,
        name: String,
        permissions: u64,
        rank: u16,
        icon: String,
        color: u32,
    },
    UpdateRole {
        id: u32,
        name: String,
        permissions: u64,
        rank: u16,
        icon: String,
        color: u32,
    },
    DeleteRole {
        id: u32,
        ts: WallMs,
    },
    BlobRefs {
        blob_id: protocol::BlobId,
        refs: u32,
    },
    /// Everything up to `msg_id` in a conversation has been read.
    MarkRead {
        conv: ConvKey,
        user_id: UserId,
        msg_id: u64,
    },
    // ── 1.2: blocks and reports (protocol::safety) ───────────────────
    InsertBlock {
        blocker: UserId,
        blocked: UserId,
        ts: WallMs,
    },
    DeleteBlock {
        blocker: UserId,
        blocked: UserId,
    },
    InsertReport(Box<ReportRow>),
    ResolveReport {
        id: u64,
        by: Option<UserId>,
        ts: WallMs,
    },
    /// Several writes that are one event — a first visit is the user,
    /// their key, their role, the invite's use and the audit line —
    /// in one transaction: all of it lands or none of it, and in one
    /// commit where each used to take its own (review P3-9). Built by
    /// [`Persist::begin_group`] … [`Persist::end_group`].
    Together(Vec<Command>),
}

impl Command {
    /// Policy writes are few and matter across restarts; a drop is
    /// logged, not just counted.
    fn is_policy(&self) -> bool {
        match self {
            Command::InsertMessage(_) | Command::Telemetry(_) | Command::TelemetryEvent(_) => false,
            Command::Together(parts) => parts.iter().any(Command::is_policy),
            _ => true,
        }
    }

    /// Whether this writes a table [`store::Sqlite::load_policy`] reads:
    /// what makes a snapshot read before it stale. Counting every write
    /// but messages and telemetry — a read mark, an audit line, a
    /// report — made snapshots stale for nothing on a busy server.
    /// Spelled out, so a new command has to be placed.
    fn changes_policy(&self) -> bool {
        match self {
            Command::UpsertUser { .. }
            | Command::UpsertKey { .. }
            | Command::GrantRole { .. }
            | Command::RevokeRole { .. }
            | Command::DeleteUserKeys { .. }
            | Command::UpsertChannel(_)
            | Command::DeleteChannel { .. }
            | Command::AddProgress(_)
            | Command::PurgeUser { .. }
            | Command::InsertInvite { .. }
            | Command::UseInvite { .. }
            | Command::RevokeInvite { .. }
            | Command::InsertRestriction(_)
            | Command::DeleteRestriction { .. }
            | Command::SetProfile { .. }
            | Command::StoreBlob(_)
            | Command::DeleteBlob { .. }
            | Command::InsertRole { .. }
            | Command::UpdateRole { .. }
            | Command::DeleteRole { .. }
            | Command::BlobRefs { .. }
            | Command::InsertBlock { .. }
            | Command::DeleteBlock { .. } => true,
            Command::InsertMessage(_)
            | Command::Telemetry(_)
            | Command::TelemetryEvent(_)
            | Command::EditMessage { .. }
            | Command::DeleteMessage { .. }
            | Command::Audit(_)
            | Command::MarkRead { .. }
            | Command::InsertReport(_)
            | Command::ResolveReport { .. } => false,
            Command::Together(parts) => parts.iter().any(Command::changes_policy),
        }
    }
}

enum Envelope {
    Write(Command),
    /// The tick set a snapshot aside as older than its own writes and
    /// wants the policy as it stands once everything queued ahead of
    /// this is written.
    Reload,
    Shutdown,
}

/// A read with a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// A stored blob's bytes, from disk (spec §7).
    Blob { blob_id: protocol::BlobId },
    /// The audit log, newest first, before entry `before` (spec §8).
    Audit { before: Option<u64>, limit: u16 },
    /// Store a DM before anyone sees it (spec §6): the conversation is
    /// created on the first message between the two; the reply is the
    /// sender's ack.
    StoreDm {
        row: MessageRow,
        a: UserId,
        b: UserId,
    },
    /// This user's DM conversations with unread counts (spec §6).
    Conversations { user_id: UserId },
    /// The last `limit` messages of a conversation before `before`
    /// (exclusive; `None` = newest), oldest first.
    MessagesBefore {
        conv: ConvKey,
        before: Option<u64>,
        limit: u16,
    },
    /// One message, for a report: its text and its conversation.
    ReportedMessage { msg_id: u64 },
    /// Reports for the panel, newest first.
    Reports { resolved: bool, limit: u16 },
}

/// Which conversation a read or a mark is about (spec §6): the general
/// chat, or the DM between two users (order-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvKey {
    General,
    Dm(UserId, UserId),
}

#[derive(Debug)]
pub enum Answer {
    Messages(Vec<MessageRow>),
    /// `None` when the file is missing or does not match its hash.
    Blob(Option<Vec<u8>>),
    /// A DM is on disk.
    Stored,
    /// Audit entries with their ids, newest first.
    Audit(Vec<(u64, AuditRow)>),
    Conversations(Vec<protocol::DmSummary>),
    /// Boxed: a whole message row, where every other answer is small.
    ReportedMessage(Option<Box<ReportedMessage>>),
    Reports(Vec<protocol::ReportSummary>),
    /// Persistence is off: there is nothing to read.
    Unavailable,
    Failed(String),
}

/// How a pending read resolved.
#[derive(Debug)]
pub enum Outcome {
    Answered(Answer),
    /// The deadline passed first. The client gets an empty answer with
    /// a retry hint; a reply arriving later is discarded.
    TimedOut,
}

// One message per query on one channel; the small variant is a test
// hook. Boxing every query to shrink an enum nobody stores buys nothing.
#[expect(clippy::large_enum_variant)]
enum Request {
    Query {
        /// Echoed with the answer, on the one channel every answer
        /// comes back on.
        id: AskId,
        query: Query,
        /// Set by the tick when the read's deadline passed: nobody
        /// waits for the answer any more.
        gave_up: Arc<AtomicBool>,
    },
    /// Test hook: park the thread until the sender side is dropped.
    Stall(Receiver<()>),
}

/// Something the thread noticed that the tick should act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Another process committed to the database (spec §1: the CLI, or
    /// an operator with `sqlite3`), and this is the policy as it now
    /// stands on disk. The tick swaps it in. Every in-session change
    /// queued before the reload was already written and is in here.
    /// `writes_seen` counts the policy writes the thread had taken off
    /// the queue when it read the tables: [`Persist::poll`] holds back
    /// a snapshot older than the tick's own writes and asks for a
    /// fresh one, so a ban applied between the read and the swap is
    /// not dropped from memory (review P1-22).
    Policy {
        policy: Box<Policy>,
        writes_seen: u64,
    },
    /// An account was purged: these files other people uploaded lost
    /// this many references with it (posts in its DM conversations, its
    /// avatar). The tick takes them off its counts, and a file nothing
    /// points at any more goes.
    RefsReleased(Vec<(protocol::BlobId, u32)>),
    /// A retention pass removed messages or files: the replay ring
    /// drops what the store no longer has, the file counts lose the
    /// references those messages held, and the metadata of deleted
    /// files goes — so a later announce of the same bytes is asked to
    /// upload them again instead of being told they are here.
    Pruned {
        /// Messages older than this left the store.
        history_cutoff: Option<WallMs>,
        /// References released, per file.
        released: Vec<(protocol::BlobId, u32)>,
        /// Files deleted.
        files: Vec<protocol::BlobId>,
    },
}

/// Handle to a pending read, returned by [`Persist::ask`] and echoed by
/// [`Persist::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AskId(u64);

struct PendingAsk {
    id: AskId,
    deadline: Instant,
    /// Shared with the queued request: set when this read times out,
    /// so the thread skips it. A `StoreDm` reported as failed was
    /// stored later anyway, and "Try again" stored it twice (review
    /// P2-9).
    gave_up: Arc<AtomicBool>,
}

/// What one [`Persist::poll`] produced.
#[derive(Debug, Default)]
pub struct Polled {
    pub answers: Vec<(AskId, Outcome)>,
    pub notices: Vec<Notice>,
}

/// Keeps the persistence thread parked while it exists (tests only).
pub struct StallGuard {
    _release: Sender<()>,
}

#[derive(Default)]
struct Counters {
    written: AtomicU64,
    write_errors: AtomicU64,
    answered: AtomicU64,
    external_changes: AtomicU64,
    pruned: AtomicU64,
    /// Writes still queued when the shutdown drain budget ran out.
    drained_dropped: AtomicU64,
}

/// A snapshot for the stats line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistStats {
    pub persistent: bool,
    pub queued: usize,
    pub capacity: usize,
    /// Writes refused because the queue was full (spec §1:
    /// `persistence_dropped_writes`).
    pub dropped_writes: u64,
    pub written: u64,
    pub write_errors: u64,
    pub timed_out_reads: u64,
    pub answered_reads: u64,
    pub external_changes: u64,
    pub pruned_rows: u64,
}

impl fmt::Display for PersistStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.persistent {
            return write!(f, "Persistence: off (memory only for this session)");
        }
        write!(
            f,
            "Persistence: queue {}/{}, written {}, dropped {}, write errors {}, \
             reads answered {}, timed out {}, external changes {}, pruned {}",
            self.queued,
            self.capacity,
            self.written,
            self.dropped_writes,
            self.write_errors,
            self.answered_reads,
            self.timed_out_reads,
            self.external_changes,
            self.pruned_rows,
        )
    }
}

/// What the tick needs from the database at startup.
#[derive(Debug, Clone)]
pub struct StartupInfo {
    /// The persisted message-id watermark plus one.
    pub next_message_id: u64,
    /// Users, keys, roles, invites, restrictions, channels, progress —
    /// empty but for the built-in roles with persistence off.
    pub policy: Policy,
    /// The newest general-chat messages, oldest first: the tick's
    /// replay ring starts from these.
    pub recent: Vec<MessageRow>,
}

// ── The tick's handle ───────────────────────────────────────────────────

/// Owned by the tick. Every method returns immediately.
pub struct Persist {
    writes: Sender<Envelope>,
    reads: Sender<Request>,
    /// Every answer, with the ask it is for: one channel for all reads,
    /// where each read used to make a channel of its own (review
    /// P2-52). The sender here is for the answers given on the spot (no
    /// database, a full queue).
    answers: Receiver<(AskId, Answer)>,
    answer_now: Sender<(AskId, Answer)>,
    notices: Receiver<Notice>,
    done: Receiver<()>,
    handle: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
    persistent: bool,
    capacity: usize,
    dropped_writes: u64,
    timed_out_reads: u64,
    pending: Vec<PendingAsk>,
    next_ask: u64,
    /// Policy writes queued so far, the twin of the thread's
    /// `policy_seen`.
    policy_sent: u64,
    /// A fresh snapshot is wanted and the request is not queued yet.
    reload_wanted: bool,
    /// A requested snapshot is out, awaited until then.
    reload_due: Option<Instant>,
    /// Snapshots set aside in a row as older than the tick's writes.
    stale_reloads: u32,
    /// The next snapshot is asked for no sooner than this (after a run
    /// of stale ones, [`RELOAD_BACKOFF`]).
    reload_after: Option<Instant>,
    /// Pauses taken since the last snapshot that was used.
    reload_backoffs: u32,
    /// Open [`Self::begin_group`] calls, and the writes sent since the
    /// first of them.
    group_depth: u32,
    group: Vec<Command>,
}

impl Persist {
    /// Open the store (migrating it) on the calling thread — so a broken
    /// database fails startup before the socket binds — then start the
    /// thread that owns it from here on.
    ///
    /// # Errors
    /// The database could not be opened or migrated, or the thread
    /// could not be spawned.
    pub fn spawn(settings: &PersistenceSettings) -> Result<(Self, StartupInfo), PersistError> {
        let (db, next_message_id, policy, recent) = if settings.enabled {
            let path = PathBuf::from(settings.path.trim());
            let (db, next) = Sqlite::open(&path)?;
            // Before anything reads the counts: from here on the tick
            // keeps them, and this is the one moment nothing else does.
            match db.recount_blob_refs()? {
                0 => {}
                n => info!("File reference counts: {n} corrected from the stored messages"),
            }
            let policy = db.load_policy()?;
            let recent = db.messages_before(ConvKey::General, None, RECENT_RING)?;
            info!(
                "Policy loaded: {} users, {} keys, {} roles, {} invites, {} restrictions",
                policy.users.len(),
                policy.keys.len(),
                policy.roles.len(),
                policy.invites.len(),
                policy.restrictions.len()
            );
            (Some(db), next, policy, recent)
        } else {
            info!(
                "Persistence off: roles, invites and bans last until the process exits; \
                 nothing is written to disk"
            );
            (None, 1, Policy::new(), Vec::new())
        };
        let capacity = settings.write_queue.max(1);
        let (writes_tx, writes_rx) = bounded(capacity);
        let (reads_tx, reads_rx) = bounded(READ_QUEUE);
        let (answers_tx, answers_rx) = unbounded();
        let (notices_tx, notices_rx) = unbounded();
        let (done_tx, done_rx) = bounded(1);
        let counters = Arc::new(Counters::default());
        let now = Instant::now();
        // No database, no thread: every method answers on the spot
        // (`send` drops, `ask` says unavailable). It used to run anyway,
        // waking every second to do nothing (review P2-47).
        let handle = match db {
            None => None,
            Some(db) => {
                let worker = Worker {
                    db,
                    writes: writes_rx,
                    reads: reads_rx,
                    answers: answers_tx.clone(),
                    notices: notices_tx,
                    counters: Arc::clone(&counters),
                    retention: Retention {
                        history_days: settings.history.retain_days,
                        telemetry_days: settings.telemetry.retain_days,
                        file_days: settings.files.retain_days,
                    },
                    last_checkpoint: now,
                    // First prune right away: a server that restarts
                    // often would otherwise never reach the hourly one.
                    last_prune: now.checked_sub(PRUNE_INTERVAL).unwrap_or(now),
                    last_version_poll: now,
                    policy_seen: 0,
                    stopping: false,
                };
                Some(
                    thread::Builder::new()
                        .name("persistence".to_string())
                        .spawn(move || worker.run(&done_tx))
                        .map_err(PersistError::Thread)?,
                )
            }
        };
        Ok((
            Self {
                writes: writes_tx,
                reads: reads_tx,
                answers: answers_rx,
                answer_now: answers_tx,
                notices: notices_rx,
                done: done_rx,
                handle,
                counters,
                persistent: settings.enabled,
                capacity,
                dropped_writes: 0,
                timed_out_reads: 0,
                pending: Vec::new(),
                next_ask: 1,
                policy_sent: 0,
                reload_wanted: false,
                reload_due: None,
                stale_reloads: 0,
                reload_after: None,
                reload_backoffs: 0,
                group_depth: 0,
                group: Vec::new(),
            },
            StartupInfo {
                next_message_id,
                policy,
                recent,
            },
        ))
    }

    /// Whether a database is behind this handle.
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    /// Queue a write. With persistence off this is a no-op; with the
    /// queue full the write is dropped and counted (and, for a policy
    /// write, logged: those are rare and matter across restarts).
    pub fn send(&mut self, command: Command) {
        if !self.persistent {
            return;
        }
        if self.group_depth > 0 {
            self.group.push(command);
            return;
        }
        let policy = command.changes_policy();
        if let Err(err) = self.writes.try_send(Envelope::Write(command)) {
            self.dropped_writes += 1;
            if let TrySendError::Full(Envelope::Write(cmd))
            | TrySendError::Disconnected(Envelope::Write(cmd)) = &err
                && cmd.is_policy()
            {
                warn!("persistence queue full: a policy write was dropped ({cmd:?})");
            }
        } else if policy {
            self.policy_sent += 1;
        }
    }

    /// Everything [`Self::send`] is given from here to the matching
    /// [`Self::end_group`] is one event and goes to the store as one
    /// write ([`Command::Together`]). Calls nest; the outermost pair
    /// decides.
    pub fn begin_group(&mut self) {
        self.group_depth += 1;
    }

    /// Close [`Self::begin_group`]: the outermost close queues what was
    /// gathered.
    pub fn end_group(&mut self) {
        self.group_depth = self.group_depth.saturating_sub(1);
        if self.group_depth > 0 {
            return;
        }
        let mut parts = std::mem::take(&mut self.group);
        match parts.len() {
            0 => {}
            1 => {
                if let Some(only) = parts.pop() {
                    self.send(only);
                }
            }
            _ => self.send(Command::Together(parts)),
        }
    }

    /// Queue a coalesced telemetry batch. Unlike [`Self::send`], a full
    /// queue hands the rows back so the caller keeps coalescing them
    /// (last wins) instead of dropping or backlogging.
    ///
    /// # Errors
    /// The rows, when the queue is full or persistence is off.
    pub fn send_telemetry(&self, rows: Vec<TelemetryRow>) -> Result<(), Vec<TelemetryRow>> {
        if !self.persistent {
            return Err(rows);
        }
        match self
            .writes
            .try_send(Envelope::Write(Command::Telemetry(rows)))
        {
            Ok(()) => Ok(()),
            Err(
                TrySendError::Full(Envelope::Write(Command::Telemetry(rows)))
                | TrySendError::Disconnected(Envelope::Write(Command::Telemetry(rows))),
            ) => Err(rows),
            Err(_) => Err(Vec::new()),
        }
    }

    /// Start a read. The answer arrives through [`Self::poll`], no later
    /// than `deadline` — as [`Outcome::TimedOut`] if the thread had not
    /// answered by then.
    pub fn ask(&mut self, query: Query, deadline: Instant) -> AskId {
        let id = AskId(self.next_ask);
        self.next_ask += 1;
        let gave_up = Arc::new(AtomicBool::new(false));
        if self.persistent {
            if self
                .reads
                .try_send(Request::Query {
                    id,
                    query,
                    gave_up: Arc::clone(&gave_up),
                })
                .is_err()
            {
                // Answer now rather than sit out the deadline.
                let _ = self
                    .answer_now
                    .send((id, Answer::Failed("read queue full".to_string())));
            }
        } else {
            let _ = self.answer_now.send((id, Answer::Unavailable));
        }
        self.pending.push(PendingAsk {
            id,
            deadline,
            gave_up,
        });
        id
    }

    /// Collect answers that arrived, expire reads past their deadline,
    /// and pick up notices. Called once per tick.
    pub fn poll(&mut self, now: Instant) -> Polled {
        let mut answers = Vec::new();
        for (id, answer) in self.answers.try_iter() {
            // An answer nobody waits for any more — it came after its
            // deadline — is dropped.
            if let Some(i) = self.pending.iter().position(|ask| ask.id == id) {
                self.pending.swap_remove(i);
                answers.push((id, Outcome::Answered(answer)));
            }
        }
        // A thread that ended (it never should before `shutdown`)
        // answers nothing more: say so now, not at each deadline.
        let gone = self.handle.as_ref().is_some_and(JoinHandle::is_finished);
        let mut i = 0;
        while i < self.pending.len() {
            let outcome = if gone {
                Outcome::Answered(Answer::Failed("persistence thread is gone".to_string()))
            } else if now >= self.pending[i].deadline {
                self.timed_out_reads += 1;
                self.pending[i].gave_up.store(true, Ordering::Relaxed);
                Outcome::TimedOut
            } else {
                i += 1;
                continue;
            };
            let ask = self.pending.swap_remove(i);
            answers.push((ask.id, outcome));
        }
        let mut notices = Vec::new();
        for notice in self.notices.try_iter() {
            let Notice::Policy { writes_seen, .. } = &notice else {
                notices.push(notice);
                continue;
            };
            self.reload_due = None;
            if *writes_seen >= self.policy_sent {
                self.stale_reloads = 0;
                self.reload_backoffs = 0;
            } else if self.stale_reloads < MAX_STALE_RELOADS {
                // The tick changed policy after this was read: swapping
                // it in would undo that change in memory. The next one
                // is read behind the write, and carries whatever the
                // other process changed as well.
                self.stale_reloads += 1;
                self.reload_wanted = true;
                continue;
            } else if self.reload_backoffs < MAX_RELOAD_BACKOFFS {
                // Taking it would drop from memory what the tick just
                // changed. Keep what is in memory and ask again a
                // little later, when the writes may have let up.
                info!(
                    "policy reload: {} snapshots in a row were older than this server's own \
                     writes; asking again in {} s",
                    self.stale_reloads + 1,
                    RELOAD_BACKOFF.as_secs()
                );
                self.stale_reloads = 0;
                self.reload_backoffs += 1;
                self.reload_wanted = true;
                self.reload_after = Some(now + RELOAD_BACKOFF);
                continue;
            } else {
                warn!(
                    "policy reload: snapshots stayed older than this server's own writes \
                     for {} s; taking this one, a change made in the last moments may be \
                     missing from memory until the next reload",
                    u64::from(MAX_RELOAD_BACKOFFS) * RELOAD_BACKOFF.as_secs()
                );
                self.stale_reloads = 0;
                self.reload_backoffs = 0;
            }
            self.reload_wanted = false;
            notices.push(notice);
        }
        if self.reload_wanted
            && self.reload_after.is_none_or(|t| now >= t)
            && self.writes.try_send(Envelope::Reload).is_ok()
        {
            self.reload_wanted = false;
            self.reload_after = None;
            self.reload_due = Some(now + RELOAD_WAIT);
        }
        if self.reload_due.is_some_and(|due| now >= due) {
            warn!("policy reload: the snapshot asked for did not arrive");
            self.reload_due = None;
        }
        Polled { answers, notices }
    }

    /// A fresh policy snapshot is being asked for or awaited: the tick
    /// polls on its short interval, as for a read ([`Self::reads_in_flight`]).
    #[must_use]
    pub fn reload_in_flight(&self) -> bool {
        (self.reload_wanted && self.reload_after.is_none()) || self.reload_due.is_some()
    }

    /// Park the thread until the guard is dropped. For tests: this is
    /// how "the disk stalled for 10 s" is staged.
    #[must_use]
    pub fn stall(&self) -> StallGuard {
        let (release, hold) = bounded::<()>(0);
        if self
            .reads
            .send_timeout(Request::Stall(hold), Duration::from_secs(1))
            .is_err()
        {
            warn!("could not stall the persistence thread (read queue full or thread gone)");
        }
        StallGuard { _release: release }
    }

    /// Reads asked and not yet collected by [`Self::poll`]. The tests'
    /// simulated clock waits on the real one while any is out, so a
    /// busy machine cannot run a read past its deadline in a blink.
    #[must_use]
    pub fn reads_in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Counters for the stats line.
    #[must_use]
    pub fn stats(&self) -> PersistStats {
        let c = &self.counters;
        PersistStats {
            persistent: self.persistent,
            queued: self.writes.len(),
            capacity: self.capacity,
            dropped_writes: self.dropped_writes + c.drained_dropped.load(Ordering::Relaxed),
            written: c.written.load(Ordering::Relaxed),
            write_errors: c.write_errors.load(Ordering::Relaxed),
            timed_out_reads: self.timed_out_reads,
            answered_reads: c.answered.load(Ordering::Relaxed),
            external_changes: c.external_changes.load(Ordering::Relaxed),
            pruned_rows: c.pruned.load(Ordering::Relaxed),
        }
    }

    /// Ask the thread to drain what is queued (bounded) and exit; wait
    /// for it, bounded. Safe to call twice. One deadline covers both
    /// waits: with one each, a jammed queue held shutdown for twice
    /// [`SHUTDOWN_WAIT`], past Docker's 10 s stop timeout (review P2-5).
    pub fn shutdown(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        if self
            .writes
            .send_deadline(Envelope::Shutdown, deadline)
            .is_err()
        {
            warn!("persistence queue jammed at shutdown; not waiting for it");
        }
        match self.done.recv_deadline(deadline) {
            Ok(()) => {
                let _ = handle.join();
            }
            Err(_) => warn!(
                "persistence thread did not finish within {} s; leaving it behind",
                SHUTDOWN_WAIT.as_secs()
            ),
        }
    }
}

impl Drop for Persist {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── The thread ──────────────────────────────────────────────────────────

struct Worker {
    db: Sqlite,
    writes: Receiver<Envelope>,
    reads: Receiver<Request>,
    answers: Sender<(AskId, Answer)>,
    notices: Sender<Notice>,
    counters: Arc<Counters>,
    retention: Retention,
    last_checkpoint: Instant,
    last_prune: Instant,
    last_version_poll: Instant,
    /// Policy writes taken off the queue so far, written or failed.
    policy_seen: u64,
    /// A shutdown came out of the queue outside the loop's own receive.
    stopping: bool,
}

enum Step {
    Read(Request),
    Write(Command),
    Reload,
    Stop,
    Idle,
}

impl Worker {
    fn run(mut self, done: &Sender<()>) {
        loop {
            let step = select_biased! {
                recv(self.reads) -> request => match request {
                    Ok(request) => Step::Read(request),
                    Err(_) => Step::Stop,
                },
                recv(self.writes) -> envelope => match envelope {
                    Ok(Envelope::Write(command)) => Step::Write(command),
                    Ok(Envelope::Reload) => Step::Reload,
                    Ok(Envelope::Shutdown) | Err(_) => Step::Stop,
                },
                default(self.until_next_job()) => Step::Idle,
            };
            match step {
                Step::Read(request) => self.handle_request(request),
                Step::Write(command) => self.apply(&command),
                Step::Reload => self.send_policy(),
                Step::Stop => break,
                Step::Idle => {}
            }
            self.housekeeping();
            if self.stopping {
                break;
            }
        }
        self.drain();
        self.db.close();
        let _ = done.send(());
    }

    /// How long the thread may sleep with nothing queued: until its
    /// next periodic job (the `data_version` poll, in practice). It
    /// used to wake every second whatever was due (review P2-47).
    fn until_next_job(&self) -> Duration {
        let due = (self.last_version_poll + DATA_VERSION_INTERVAL)
            .min(self.last_checkpoint + CHECKPOINT_INTERVAL)
            .min(self.last_prune + PRUNE_INTERVAL);
        due.saturating_duration_since(Instant::now())
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::Query { id, query, gave_up } => {
                // Timed out while it waited in the queue: the tick has
                // told the client it failed, so it must not happen now.
                if gave_up.load(Ordering::Relaxed) {
                    return;
                }
                let answer = self.answer(&query);
                if self.answers.send((id, answer)).is_ok() {
                    self.counters.answered.fetch_add(1, Ordering::Relaxed);
                }
            }
            Request::Stall(hold) => {
                debug!("persistence thread stalled (test hook)");
                let _ = hold.recv();
                debug!("persistence thread released");
            }
        }
    }

    fn answer(&self, query: &Query) -> Answer {
        let db = &self.db;
        match query {
            Query::Blob { blob_id } => Answer::Blob(db.read_blob(blob_id)),
            Query::StoreDm { row, a, b } => match db.store_dm(row, a, b) {
                Ok(()) => Answer::Stored,
                Err(e) => Answer::Failed(e.to_string()),
            },
            Query::Audit { before, limit } => match db.audit_before(*before, *limit) {
                Ok(rows) => Answer::Audit(rows),
                Err(e) => Answer::Failed(e.to_string()),
            },
            Query::Conversations { user_id } => match db.conversations_of(user_id) {
                Ok(list) => Answer::Conversations(list),
                Err(e) => Answer::Failed(e.to_string()),
            },
            Query::MessagesBefore {
                conv,
                before,
                limit,
            } => match db.messages_before(*conv, *before, *limit) {
                Ok(rows) => Answer::Messages(rows),
                Err(e) => Answer::Failed(e.to_string()),
            },
            Query::ReportedMessage { msg_id } => match db.message_for_report(*msg_id) {
                Ok(found) => Answer::ReportedMessage(found.map(Box::new)),
                Err(e) => Answer::Failed(e.to_string()),
            },
            Query::Reports { resolved, limit } => match db.reports(*resolved, *limit) {
                Ok(list) => Answer::Reports(list),
                Err(e) => Answer::Failed(e.to_string()),
            },
        }
    }

    fn apply(&mut self, command: &Command) {
        if command.changes_policy() {
            self.policy_seen += 1;
        }
        match self.write(command) {
            Ok(()) => {
                self.counters.written.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                self.counters.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!("persistence write failed: {e}");
            }
        }
    }

    /// A group, as one transaction: the first part that fails undoes
    /// the ones before it.
    fn write_together(&mut self, parts: &[Command]) -> Result<(), rusqlite::Error> {
        self.db.begin()?;
        match parts.iter().try_for_each(|part| self.write(part)) {
            Ok(()) => self.db.commit(),
            Err(e) => {
                if let Err(undo) = self.db.rollback() {
                    warn!("a failed group of writes was not rolled back: {undo}");
                }
                Err(e)
            }
        }
    }

    fn write(&mut self, command: &Command) -> Result<(), rusqlite::Error> {
        let db = &mut self.db;
        match command {
            Command::Together(parts) => self.write_together(parts),
            Command::InsertMessage(row) => db.insert_message(row),
            Command::Telemetry(rows) => db.insert_telemetry(rows),
            Command::TelemetryEvent(row) => db.insert_telemetry_event(row),
            Command::UpsertUser { user_id, user } => db.upsert_user(user_id, user),
            Command::SetProfile {
                user_id,
                profile,
                ts,
            } => db.set_profile(user_id, profile, *ts),
            Command::StoreBlob(blob) => match db.store_blob(blob) {
                Ok(()) => Ok(()),
                Err(PersistError::Sqlite(e)) => Err(e),
                Err(e) => {
                    warn!("blob {} not stored: {e}", store::hex(&blob.blob_id));
                    Ok(())
                }
            },
            Command::BlobRefs { blob_id, refs } => db.set_blob_refs(blob_id, *refs),
            Command::DeleteBlob { blob_id } => db.delete_blob(blob_id),
            Command::InsertRole {
                id,
                name,
                permissions,
                rank,
                icon,
                color,
            } => db.insert_role(*id, name, *permissions, *rank, icon, *color),
            Command::UpdateRole {
                id,
                name,
                permissions,
                rank,
                icon,
                color,
            } => db.update_role(*id, name, *permissions, *rank, icon, *color),
            Command::DeleteRole { id, ts } => db.delete_role(*id, *ts),
            Command::MarkRead {
                conv,
                user_id,
                msg_id,
            } => db.mark_read(*conv, user_id, *msg_id),
            Command::UpsertKey { key_hash, key } => db.upsert_key(key_hash, key),
            Command::GrantRole {
                user_id,
                role_id,
                granted_by,
                ts,
            } => db.grant_role(user_id, *role_id, granted_by.as_ref(), *ts),
            Command::RevokeRole { user_id, role_id } => db.revoke_role(user_id, *role_id),
            Command::DeleteUserKeys { user_id } => db.delete_user_keys(user_id),
            Command::EditMessage { msg_id, body, ts } => db.edit_message(*msg_id, body, *ts),
            Command::DeleteMessage { msg_id, ts } => db.delete_message(*msg_id, *ts),
            Command::UpsertChannel(c) => db.upsert_channel(c),
            Command::DeleteChannel { stable_id } => db.delete_channel(*stable_id),
            Command::AddProgress(p) => db.add_progress(p),
            Command::PurgeUser { user_id } => db.purge_user(user_id).map(|released| {
                if !released.is_empty() {
                    let _ = self.notices.send(Notice::RefsReleased(released));
                }
            }),
            Command::InsertInvite { code, invite } => db.insert_invite(code, invite),
            Command::UseInvite { code } => db.use_invite(code),
            Command::RevokeInvite { code, ts } => db.revoke_invite(code, *ts),
            Command::InsertRestriction(r) => db.insert_restriction(r),
            Command::DeleteRestriction { id } => db.delete_restriction(*id),
            Command::Audit(row) => db.insert_audit(row),
            Command::InsertBlock {
                blocker,
                blocked,
                ts,
            } => db.insert_block(blocker, blocked, *ts),
            Command::DeleteBlock { blocker, blocked } => db.delete_block(blocker, blocked),
            Command::InsertReport(row) => db.insert_report(row),
            Command::ResolveReport { id, by, ts } => db.resolve_report(*id, by.as_ref(), *ts),
        }
    }

    fn housekeeping(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_version_poll) >= DATA_VERSION_INTERVAL {
            self.last_version_poll = now;
            self.reload_if_changed_externally();
        }
        let db = &mut self.db;
        if now.duration_since(self.last_checkpoint) >= CHECKPOINT_INTERVAL {
            self.last_checkpoint = now;
            if let Err(e) = db.checkpoint() {
                warn!("WAL checkpoint failed: {e}");
            }
        }
        if now.duration_since(self.last_prune) >= PRUNE_INTERVAL {
            self.last_prune = now;
            match db.prune_with_files(wall_now(), self.retention) {
                Ok(pruned) if pruned.removed == 0 => {}
                Ok(pruned) => {
                    self.counters
                        .pruned
                        .fetch_add(pruned.removed, Ordering::Relaxed);
                    info!("Pruned {} rows past their retention window", pruned.removed);
                    let _ = self.notices.send(Notice::Pruned {
                        history_cutoff: pruned.history_cutoff,
                        released: pruned.released,
                        files: pruned.files,
                    });
                }
                Err(e) => warn!("retention prune failed: {e}"),
            }
        }
    }

    /// When another process committed to the file, hand the tick the
    /// policy tables as they now stand.
    fn reload_if_changed_externally(&mut self) {
        match self.db.changed_externally() {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                warn!("data_version poll failed: {e}");
                return;
            }
        }
        self.counters
            .external_changes
            .fetch_add(1, Ordering::Relaxed);
        info!("Database changed by another process (the CLI, or a hand edit)");
        // The tick applied in memory what it queued here; a snapshot
        // without those rows would undo it (a ban dropped, SEC-11), so
        // they land first (review P1-22).
        self.apply_queued();
        self.send_policy();
    }

    /// Hand the tick the policy tables as they stand, with how many of
    /// its policy writes they can hold.
    fn send_policy(&mut self) {
        match self.db.load_policy() {
            Ok(policy) => {
                let _ = self.notices.send(Notice::Policy {
                    policy: Box::new(policy),
                    writes_seen: self.policy_seen,
                });
            }
            Err(e) => warn!("could not reload the policy tables: {e}"),
        }
    }

    /// Apply the writes queued so far (not the ones that keep arriving
    /// meanwhile). A shutdown met on the way is kept for the loop.
    fn apply_queued(&mut self) {
        for _ in 0..self.writes.len() {
            match self.writes.try_recv() {
                Ok(Envelope::Write(command)) => self.apply(&command),
                // The snapshot that follows this drain answers it.
                Ok(Envelope::Reload) => {}
                Ok(Envelope::Shutdown) | Err(TryRecvError::Disconnected) => {
                    self.stopping = true;
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }

    /// Apply what is still queued, for at most [`SHUTDOWN_DRAIN`].
    fn drain(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_DRAIN;
        while let Ok(envelope) = self.writes.try_recv() {
            match envelope {
                Envelope::Write(command) if Instant::now() < deadline => self.apply(&command),
                Envelope::Write(_) => {
                    self.counters
                        .drained_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
                Envelope::Reload | Envelope::Shutdown => {}
            }
        }
    }
}

/// Real wall time for housekeeping on the thread and for the CLI
/// (retention windows are measured against the clock on the wall, not
/// the tick's).
pub(crate) fn wall_now() -> WallMs {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(enabled: bool, path: &str, write_queue: usize) -> PersistenceSettings {
        PersistenceSettings {
            enabled,
            path: path.to_string(),
            write_queue,
            ..PersistenceSettings::default()
        }
    }

    /// A row stamped now: the thread prunes past the retention window
    /// at startup, so a row with a tiny `ts` would vanish mid-test.
    fn message(msg_id: u64) -> Command {
        Command::InsertMessage(MessageRow {
            msg_id,
            conv_id: GENERAL_CONV,
            ts: wall_now() + msg_id,
            user_id: [3u8; 32],
            body: format!("m{msg_id}"),
            name: String::new(),
            edited: false,
            blob_id: None,
        })
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "condition not met in time");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn temp_db(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "baucord-persist-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ))
            .join("baucord.db")
    }

    #[test]
    fn writes_past_the_queue_are_dropped_and_counted_while_stalled() {
        let (mut p, _) = Persist::spawn(&settings(true, ":memory:", 4)).expect("spawn");
        let guard = p.stall();
        // The stall request must be picked up before writes pile up.
        thread::sleep(Duration::from_millis(50));
        for id in 1..=10 {
            p.send(message(id));
        }
        assert_eq!(p.stats().dropped_writes, 6, "{}", p.stats());
        assert_eq!(p.stats().written, 0);
        drop(guard);
        wait_until(|| p.stats().written == 4);
        assert_eq!(p.stats().queued, 0);
    }

    #[test]
    fn a_read_past_its_deadline_times_out_and_the_late_reply_is_discarded() {
        let (mut p, _) = Persist::spawn(&settings(true, ":memory:", 8)).expect("spawn");
        let guard = p.stall();
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        let id = p.ask(
            Query::MessagesBefore {
                conv: ConvKey::General,
                before: None,
                limit: 10,
            },
            start + Duration::from_millis(50),
        );
        assert!(p.poll(start).answers.is_empty());
        thread::sleep(Duration::from_millis(80));
        let polled = p.poll(Instant::now());
        assert!(
            matches!(polled.answers.as_slice(), [(got, Outcome::TimedOut)] if *got == id),
            "{polled:?}"
        );
        assert_eq!(p.stats().timed_out_reads, 1);
        drop(guard);
        thread::sleep(Duration::from_millis(100));
        assert!(p.poll(Instant::now()).answers.is_empty());
    }

    #[test]
    fn a_store_that_timed_out_in_the_queue_is_not_run_later() {
        // The disk stalls, a DM's store times out and the sender is told
        // it failed. Once the disk is back the store must not happen:
        // the retry would put the message there twice (review P2-9).
        let (mut p, _) = Persist::spawn(&settings(true, ":memory:", 8)).expect("spawn");
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let guard = p.stall();
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        let Command::InsertMessage(row) = message(1) else {
            unreachable!()
        };
        let id = p.ask(
            Query::StoreDm { row, a, b },
            start + Duration::from_millis(50),
        );
        thread::sleep(Duration::from_millis(80));
        let polled = p.poll(Instant::now());
        assert!(
            matches!(polled.answers.as_slice(), [(got, Outcome::TimedOut)] if *got == id),
            "{polled:?}"
        );
        drop(guard);

        let read = p.ask(
            Query::MessagesBefore {
                conv: ConvKey::Dm(a, b),
                before: None,
                limit: 10,
            },
            Instant::now() + Duration::from_secs(5),
        );
        let mut answer = None;
        wait_until(|| {
            answer = p.poll(Instant::now()).answers.pop();
            answer.is_some()
        });
        match answer.expect("answer") {
            (got, Outcome::Answered(Answer::Messages(rows))) => {
                assert_eq!(got, read);
                assert!(rows.is_empty(), "the timed-out DM was stored: {rows:?}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        p.shutdown();
    }

    #[test]
    fn a_read_that_arrives_in_time_is_answered() {
        let (mut p, _) = Persist::spawn(&settings(true, ":memory:", 8)).expect("spawn");
        p.send(message(1));
        p.send(message(2));
        wait_until(|| p.stats().written == 2);
        let id = p.ask(
            Query::MessagesBefore {
                conv: ConvKey::General,
                before: None,
                limit: 10,
            },
            Instant::now() + Duration::from_secs(2),
        );
        let mut answers = Vec::new();
        wait_until(|| {
            answers.extend(p.poll(Instant::now()).answers);
            !answers.is_empty()
        });
        match answers.pop() {
            Some((got, Outcome::Answered(Answer::Messages(rows)))) => {
                assert_eq!(got, id);
                assert_eq!(rows.iter().map(|m| m.msg_id).collect::<Vec<_>>(), [1, 2]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn memory_mode_answers_unavailable_at_once_and_drops_nothing() {
        let (mut p, info) = Persist::spawn(&settings(false, "unused", 1)).expect("spawn");
        assert!(!p.is_persistent());
        assert_eq!(info.next_message_id, 1);
        for id in 1..=10 {
            p.send(message(id));
        }
        assert_eq!(p.stats().dropped_writes, 0);
        let now = Instant::now();
        let id = p.ask(
            Query::MessagesBefore {
                conv: ConvKey::General,
                before: None,
                limit: 10,
            },
            now + Duration::from_secs(1),
        );
        let polled = p.poll(now);
        assert!(
            matches!(
                polled.answers.as_slice(),
                [(got, Outcome::Answered(Answer::Unavailable))] if *got == id
            ),
            "{polled:?}"
        );
    }

    #[test]
    fn telemetry_comes_back_when_the_queue_is_full() {
        let (p, _) = Persist::spawn(&settings(true, ":memory:", 1)).expect("spawn");
        let guard = p.stall();
        thread::sleep(Duration::from_millis(50));
        let row = TelemetryRow {
            ts: 1,
            user_id: [1u8; 32],
            key_hash: [1u8; 32],
            rtt_ms: 1,
            path_loss: 0.0,
            voice_loss: None,
            groups_behind: None,
            rung: None,
            input_device: None,
            output_device: None,
            sample: Vec::new(),
        };
        assert!(p.send_telemetry(vec![row.clone()]).is_ok());
        let returned = p
            .send_telemetry(vec![row])
            .expect_err("second batch must come back");
        assert_eq!(returned.len(), 1);
        assert_eq!(p.stats().dropped_writes, 0);
        drop(guard);
        wait_until(|| p.stats().written == 1);
    }

    #[test]
    fn shutdown_drains_queued_writes_to_disk() {
        let path = temp_db("drain");
        let path_str = path.to_string_lossy().into_owned();
        {
            let (mut p, _) = Persist::spawn(&settings(true, &path_str, 16)).expect("spawn");
            for id in 1..=5 {
                p.send(message(id));
            }
            p.shutdown();
        }
        let (db, next) = Sqlite::open(&path).expect("reopen");
        assert_eq!(next, 6);
        db.close();
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A group is one write: all of it lands, in one commit — or, when
    /// a part fails, none of it does (review P3-9).
    #[test]
    fn a_group_of_writes_lands_whole_or_not_at_all() {
        use protocol::permissions::ROLE_MEMBER;
        let path = temp_db("group");
        let path_str = path.to_string_lossy().into_owned();
        let user = |name: &str| UserRecord {
            name: name.to_string(),
            created_at: 1,
            last_seen: 1,
            invited_with: None,
            roles: std::collections::BTreeSet::new(),
            profile: protocol::Profile::default(),
        };
        let (alice, bob, nobody) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        {
            let (mut p, _) = Persist::spawn(&settings(true, &path_str, 16)).expect("spawn");
            p.begin_group();
            p.send(Command::UpsertUser {
                user_id: alice,
                user: user("alice"),
            });
            // Nested, as an admin command calling a helper that groups.
            p.begin_group();
            p.send(Command::GrantRole {
                user_id: alice,
                role_id: ROLE_MEMBER,
                granted_by: None,
                ts: 1,
            });
            p.end_group();
            assert_eq!(p.stats().queued, 0, "held until the outermost close");
            p.end_group();

            // The second part names a user who does not exist: the
            // first part must not survive it.
            p.begin_group();
            p.send(Command::UpsertUser {
                user_id: bob,
                user: user("bob"),
            });
            p.send(Command::GrantRole {
                user_id: nobody,
                role_id: ROLE_MEMBER,
                granted_by: None,
                ts: 1,
            });
            p.end_group();
            wait_until(|| {
                let stats = p.stats();
                stats.written + stats.write_errors == 2
            });
            let stats = p.stats();
            assert_eq!((stats.written, stats.write_errors), (1, 1), "{stats:?}");
            p.shutdown();
        }
        let (db, _) = Sqlite::open(&path).expect("reopen");
        let policy = db.load_policy().expect("policy");
        assert!(policy.users[&alice].roles.contains(&ROLE_MEMBER));
        assert!(
            !policy.users.contains_key(&bob),
            "rolled back with its group"
        );
        db.close();
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_reload_includes_the_writes_queued_before_it() {
        // The tick queued a ban; the CLI wrote meanwhile. The snapshot the
        // tick swaps in must still hold the ban (review P1-22, SEC-11).
        let path = temp_db("reload-drain");
        let path_str = path.to_string_lossy().into_owned();
        let (mut p, _) = Persist::spawn(&settings(true, &path_str, 16)).expect("spawn");
        let guard = p.stall();
        thread::sleep(Duration::from_millis(50));
        p.send(Command::InsertRestriction(Restriction {
            id: 7,
            user_id: None,
            key_hash: Some([5; 32]),
            ip: None,
            kind: crate::policy::RestrictionKind::Server,
            until: None,
            reason: "queued on the tick".to_string(),
            by: None,
            created_at: 1,
        }));
        cli_create_invite(&path, protocol::permissions::ROLE_GUEST, None, None).expect("cli");
        // Released after the poll is due, the reload is the thread's
        // next step, ahead of the queued write.
        thread::sleep(DATA_VERSION_INTERVAL + Duration::from_millis(100));
        drop(guard);
        let mut reloaded = None;
        wait_until(|| {
            for notice in p.poll(Instant::now()).notices {
                if let Notice::Policy { policy, .. } = notice {
                    reloaded = Some(policy);
                }
            }
            reloaded.is_some()
        });
        let policy = reloaded.expect("a reload");
        assert_eq!(policy.invites.len(), 1, "the CLI's invite");
        assert!(
            policy.restrictions.iter().any(|r| r.id == 7),
            "the queued ban: {:?}",
            policy.restrictions
        );
        p.shutdown();
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_snapshot_older_than_a_queued_write_is_held_back() {
        // The CLI wrote; the thread read the tables; then the tick
        // banned someone, before it saw the snapshot. Swapping that
        // snapshot in would drop the ban from memory (review P1-22):
        // it is set aside, and the one asked for in its place holds
        // both changes.
        fn ban(id: u64) -> Command {
            Command::InsertRestriction(Restriction {
                id,
                user_id: None,
                key_hash: Some([5; 32]),
                ip: None,
                kind: crate::policy::RestrictionKind::Server,
                until: None,
                reason: "after the snapshot".to_string(),
                by: None,
                created_at: 1,
            })
        }
        let path = temp_db("reload-stale");
        let path_str = path.to_string_lossy().into_owned();
        let (mut p, _) = Persist::spawn(&settings(true, &path_str, 16)).expect("spawn");
        cli_create_invite(&path, protocol::permissions::ROLE_GUEST, None, None).expect("cli");
        wait_until(|| p.stats().external_changes == 1);
        // The snapshot follows the count by one table read.
        thread::sleep(Duration::from_millis(200));
        p.send(ban(7));
        let first = p.poll(Instant::now());
        assert!(
            !first
                .notices
                .iter()
                .any(|n| matches!(n, Notice::Policy { .. })),
            "the stale snapshot must not reach the tick"
        );
        assert!(p.reload_in_flight(), "a fresh snapshot was asked for");

        let mut reloaded = None;
        wait_until(|| {
            for notice in p.poll(Instant::now()).notices {
                if let Notice::Policy { policy, .. } = notice {
                    reloaded = Some(policy);
                }
            }
            reloaded.is_some()
        });
        let policy = reloaded.expect("a reload");
        assert_eq!(policy.invites.len(), 1, "the CLI's invite");
        assert!(
            policy.restrictions.iter().any(|r| r.id == 7),
            "the ban queued after the first snapshot: {:?}",
            policy.restrictions
        );
        assert!(!p.reload_in_flight());

        // A server that never stops writing policy still gets a
        // snapshot: after a few stale ones and a few pauses (about
        // `MAX_RELOAD_BACKOFFS` seconds) it takes what it has.
        cli_create_invite(&path, protocol::permissions::ROLE_GUEST, None, None).expect("cli");
        let mut next_id = 100;
        let mut taken = None;
        let deadline = Instant::now() + RELOAD_BACKOFF * (MAX_RELOAD_BACKOFFS + 5);
        while taken.is_none() {
            assert!(Instant::now() < deadline, "no snapshot was ever taken");
            p.send(ban(next_id));
            next_id += 1;
            for notice in p.poll(Instant::now()).notices {
                if let Notice::Policy { policy, .. } = notice {
                    taken = Some(policy);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(taken.expect("a reload").invites.len(), 2);
        p.shutdown();
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
