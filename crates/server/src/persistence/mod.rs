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
        atomic::{AtomicU64, Ordering},
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

pub use self::store::{
    backup_to_tar, blobs_dir, check, cli_create_invite, cli_find_user, cli_kick,
    cli_lift_restriction, cli_list_invites, cli_load_policy, cli_prune, cli_purge, cli_restrict,
    cli_revoke_invite, cli_role_create, cli_role_delete, cli_role_edit, cli_set_key, cli_set_roles,
    cli_telemetry, hex, parse_hex_id, restore_from_tar, BackupReport, CheckReport, RestoreReport,
    Retention, RoleEdit, TelemetryLine, GENERAL_CONV, SCHEMA_VERSION,
};
use self::store::{Sqlite, Store};
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
const HOUSEKEEPING_TICK: Duration = Duration::from_secs(1);
const CHECKPOINT_INTERVAL: Duration = Duration::from_mins(1);
const PRUNE_INTERVAL: Duration = Duration::from_hours(1);
const DATA_VERSION_INTERVAL: Duration = Duration::from_secs(2);
/// On shutdown the thread drains what is queued for at most this long;
/// the rest is counted as dropped.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);
/// How long the tick waits for the thread to finish before detaching.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(6);

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
}

impl Command {
    /// Policy writes are few and matter across restarts; a drop is
    /// logged, not just counted.
    fn is_policy(&self) -> bool {
        !matches!(
            self,
            Command::InsertMessage(_) | Command::Telemetry(_) | Command::TelemetryEvent(_)
        )
    }
}

enum Envelope {
    Write(Command),
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
        query: Query,
        reply: Sender<Answer>,
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
    /// queued before the reload was already written and is in here; a
    /// change queued in the few milliseconds between the reload's read
    /// and the swap is re-read from disk on the next external change.
    Policy(Box<Policy>),
}

/// Handle to a pending read, returned by [`Persist::ask`] and echoed by
/// [`Persist::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AskId(u64);

struct PendingAsk {
    id: AskId,
    rx: Receiver<Answer>,
    deadline: Instant,
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
        let (store, next_message_id, policy, recent) = if settings.enabled {
            let path = PathBuf::from(settings.path.trim());
            let (db, next) = Sqlite::open(&path)?;
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
            (Store::Sqlite(db), next, policy, recent)
        } else {
            info!(
                "Persistence off: roles, invites and bans last until the process exits; \
                 nothing is written to disk"
            );
            (Store::Memory, 1, Policy::new(), Vec::new())
        };
        let capacity = settings.write_queue.max(1);
        let (writes_tx, writes_rx) = bounded(capacity);
        let (reads_tx, reads_rx) = bounded(READ_QUEUE);
        let (notices_tx, notices_rx) = unbounded();
        let (done_tx, done_rx) = bounded(1);
        let counters = Arc::new(Counters::default());
        let now = Instant::now();
        let worker = Worker {
            store,
            writes: writes_rx,
            reads: reads_rx,
            notices: notices_tx,
            counters: Arc::clone(&counters),
            retention: Retention {
                history_days: settings.history.retain_days,
                telemetry_days: settings.telemetry.retain_days,
                file_days: settings.files.retain_days,
            },
            last_checkpoint: now,
            // First prune right away: a server that restarts often
            // would otherwise never reach the hourly one.
            last_prune: now.checked_sub(PRUNE_INTERVAL).unwrap_or(now),
            last_version_poll: now,
        };
        let handle = thread::Builder::new()
            .name("persistence".to_string())
            .spawn(move || worker.run(&done_tx))
            .map_err(PersistError::Thread)?;
        Ok((
            Self {
                writes: writes_tx,
                reads: reads_tx,
                notices: notices_rx,
                done: done_rx,
                handle: Some(handle),
                counters,
                persistent: settings.enabled,
                capacity,
                dropped_writes: 0,
                timed_out_reads: 0,
                pending: Vec::new(),
                next_ask: 1,
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
        if let Err(err) = self.writes.try_send(Envelope::Write(command)) {
            self.dropped_writes += 1;
            if let TrySendError::Full(Envelope::Write(cmd))
            | TrySendError::Disconnected(Envelope::Write(cmd)) = &err
                && cmd.is_policy()
            {
                warn!("persistence queue full: a policy write was dropped ({cmd:?})");
            }
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
        let (reply, rx) = bounded(1);
        if self.persistent {
            if let Err(
                TrySendError::Full(Request::Query { reply, .. })
                | TrySendError::Disconnected(Request::Query { reply, .. }),
            ) = self.reads.try_send(Request::Query { query, reply })
            {
                // Answer now rather than sit out the deadline.
                let _ = reply.send(Answer::Failed("read queue full".to_string()));
            }
        } else {
            let _ = reply.send(Answer::Unavailable);
        }
        self.pending.push(PendingAsk { id, rx, deadline });
        id
    }

    /// Collect answers that arrived, expire reads past their deadline,
    /// and pick up notices. Called once per tick.
    pub fn poll(&mut self, now: Instant) -> Polled {
        let mut answers = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let resolved = match self.pending[i].rx.try_recv() {
                Ok(answer) => Some(Outcome::Answered(answer)),
                Err(TryRecvError::Disconnected) => Some(Outcome::Answered(Answer::Failed(
                    "persistence thread is gone".to_string(),
                ))),
                Err(TryRecvError::Empty) if now >= self.pending[i].deadline => {
                    self.timed_out_reads += 1;
                    Some(Outcome::TimedOut)
                }
                Err(TryRecvError::Empty) => None,
            };
            match resolved {
                Some(outcome) => {
                    let ask = self.pending.swap_remove(i);
                    answers.push((ask.id, outcome));
                }
                None => i += 1,
            }
        }
        let notices: Vec<Notice> = self.notices.try_iter().collect();
        Polled { answers, notices }
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
    /// for it, bounded. Safe to call twice.
    pub fn shutdown(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if self
            .writes
            .send_timeout(Envelope::Shutdown, SHUTDOWN_WAIT)
            .is_err()
        {
            warn!("persistence queue jammed at shutdown; not waiting for it");
        }
        match self.done.recv_timeout(SHUTDOWN_WAIT) {
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
    store: Store,
    writes: Receiver<Envelope>,
    reads: Receiver<Request>,
    notices: Sender<Notice>,
    counters: Arc<Counters>,
    retention: Retention,
    last_checkpoint: Instant,
    last_prune: Instant,
    last_version_poll: Instant,
}

enum Step {
    Read(Request),
    Write(Command),
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
                    Ok(Envelope::Shutdown) | Err(_) => Step::Stop,
                },
                default(HOUSEKEEPING_TICK) => Step::Idle,
            };
            match step {
                Step::Read(request) => self.handle_request(request),
                Step::Write(command) => self.apply(&command),
                Step::Stop => break,
                Step::Idle => {}
            }
            self.housekeeping();
        }
        self.drain();
        match self.store {
            Store::Sqlite(db) => db.close(),
            Store::Memory => {}
        }
        let _ = done.send(());
    }

    fn handle_request(&mut self, request: Request) {
        match request {
            Request::Query { query, reply } => {
                let answer = self.answer(&query);
                if reply.send(answer).is_ok() {
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
        let Store::Sqlite(db) = &self.store else {
            return Answer::Unavailable;
        };
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
        }
    }

    fn apply(&mut self, command: &Command) {
        let Store::Sqlite(db) = &mut self.store else {
            return;
        };
        let result = match command {
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
            Command::DeleteRole { id } => db.delete_role(*id),
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
            Command::PurgeUser { user_id } => db.purge_user(user_id),
            Command::InsertInvite { code, invite } => db.insert_invite(code, invite),
            Command::UseInvite { code } => db.use_invite(code),
            Command::RevokeInvite { code, ts } => db.revoke_invite(code, *ts),
            Command::InsertRestriction(r) => db.insert_restriction(r),
            Command::DeleteRestriction { id } => db.delete_restriction(*id),
            Command::Audit(row) => db.insert_audit(row),
        };
        match result {
            Ok(()) => {
                self.counters.written.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                self.counters.write_errors.fetch_add(1, Ordering::Relaxed);
                warn!("persistence write failed: {e}");
            }
        }
    }

    fn housekeeping(&mut self) {
        let Store::Sqlite(db) = &mut self.store else {
            return;
        };
        let now = Instant::now();
        if now.duration_since(self.last_version_poll) >= DATA_VERSION_INTERVAL {
            self.last_version_poll = now;
            match db.changed_externally() {
                Ok(true) => {
                    self.counters
                        .external_changes
                        .fetch_add(1, Ordering::Relaxed);
                    info!("Database changed by another process (the CLI, or a hand edit)");
                    match db.load_policy() {
                        Ok(policy) => {
                            let _ = self.notices.send(Notice::Policy(Box::new(policy)));
                        }
                        Err(e) => warn!("could not reload the policy tables: {e}"),
                    }
                }
                Ok(false) => {}
                Err(e) => warn!("data_version poll failed: {e}"),
            }
        }
        if now.duration_since(self.last_checkpoint) >= CHECKPOINT_INTERVAL {
            self.last_checkpoint = now;
            if let Err(e) = db.checkpoint() {
                warn!("WAL checkpoint failed: {e}");
            }
        }
        if now.duration_since(self.last_prune) >= PRUNE_INTERVAL {
            self.last_prune = now;
            match db.prune(wall_now(), self.retention) {
                Ok(0) => {}
                Ok(removed) => {
                    self.counters.pruned.fetch_add(removed, Ordering::Relaxed);
                    info!("Pruned {removed} rows past their retention window");
                }
                Err(e) => warn!("retention prune failed: {e}"),
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
                Envelope::Shutdown => {}
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
}
