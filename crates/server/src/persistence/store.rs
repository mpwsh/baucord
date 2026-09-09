//! The `SQLite` side of persistence: opening, migrating, the handful of
//! statements the persistence thread runs, and the offline operations
//! (`backup`, `check`) the CLI runs against a database file.
//!
//! Everything here is called from the persistence thread or from a CLI
//! subcommand — never from the relay tick (spec §0, rule 1).

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use log::{info, warn};
use protocol::{permissions::BUILTIN_ROLES, UserId};
use rusqlite::{params, Connection, OptionalExtension, MAIN_DB};
use sha2::Digest;

use super::{
    AuditRow, BlobRecord, ConvKey, MessageRow, PersistError, ProgressDelta, TelemetryEventRow,
    TelemetryRow,
};
use crate::{
    clock::WallMs,
    policy::{
        BlobMeta, InviteRecord, KeyRecord, Policy, Restriction, RestrictionKind, RoleRecord,
        UserChannelRecord, UserRecord,
    },
};

/// The general chat is conversation 1 (spec §6).
pub const GENERAL_CONV: u64 = 1;

/// Forward-only migrations; index + 1 is the schema version they
/// produce. A database newer than this list refuses to open.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_foundation.sql"),
    include_str!("migrations/0002_message_edits.sql"),
    include_str!("migrations/0003_role_style.sql"),
];

/// The schema version this build produces.
#[allow(clippy::cast_possible_truncation)] // a handful of migrations
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Tables `check` counts, in the order they are printed.
const COUNTED_TABLES: &[&str] = &[
    "users",
    "user_keys",
    "roles",
    "invites",
    "restrictions",
    "channels",
    "messages",
    "blobs",
    "audit",
    "user_progress",
    "telemetry_samples",
    "telemetry_events",
];

/// Wait this long on a locked database (a CLI command mid-backup)
/// before giving up on a statement.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Retention windows, in days. Zero keeps rows forever.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub history_days: u32,
    pub telemetry_days: u32,
    /// Unreferenced blobs older than this go (spec §7).
    pub file_days: u32,
}

/// Where data goes. `Memory` is persistence off: writes are discarded,
/// reads answer "unavailable", and nothing survives the process.
pub enum Store {
    Sqlite(Sqlite),
    Memory,
}

/// An open database and the connection-local `data_version` used to
/// notice other processes writing to it.
pub struct Sqlite {
    conn: Connection,
    data_version: i64,
    /// Where blob bytes live: `blobs/` next to the database (spec §2),
    /// a per-process temp dir for an in-memory database.
    blobs: PathBuf,
}

impl Sqlite {
    /// Open (creating if needed), apply pending migrations, re-sync the
    /// built-in roles, and return the store with the message-id
    /// watermark to rebase the in-memory counter on.
    ///
    /// # Errors
    /// I/O or `SQLite` failure, or a database newer than this build.
    pub fn open(path: &Path) -> Result<(Self, u64), PersistError> {
        if !is_memory_path(path)
            && let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir).map_err(|e| PersistError::Io(dir.to_path_buf(), e))?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // WAL so the CLI (and a backup) can read while the server
        // writes; NORMAL loses at most the last transactions on power
        // loss, never consistency — the right trade for chat history.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        migrate(&mut conn)?;
        sync_builtin_roles(&conn)?;
        let watermark: i64 =
            conn.query_row("SELECT COALESCE(MAX(msg_id), 0) FROM messages", [], |row| {
                row.get(0)
            })?;
        let next_message_id = u64::try_from(watermark).unwrap_or(0).saturating_add(1);
        let data_version = read_data_version(&conn)?;
        info!(
            "Database {} open (schema {SCHEMA_VERSION}, next message id {next_message_id})",
            path.display()
        );
        let blobs = if is_memory_path(path) {
            std::env::temp_dir().join(format!("baucord-blobs-{}", std::process::id()))
        } else {
            blobs_dir(path)
        };
        Ok((
            Self {
                conn,
                data_version,
                blobs,
            },
            next_message_id,
        ))
    }

    // ── Profiles and blobs (spec §7) ─────────────────────────────────

    /// Write a user's profile columns.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn set_profile(
        &self,
        user_id: &[u8; 32],
        profile: &protocol::Profile,
        ts: WallMs,
    ) -> Result<(), rusqlite::Error> {
        let style: i64 = match profile.name_style {
            protocol::NameStyle::Normal => 0,
            protocol::NameStyle::Bold => 1,
            protocol::NameStyle::Italic => 2,
        };
        self.conn.execute(
            "UPDATE users SET name_color = ?2, name_style = ?3, avatar_blob = ?4,
                              profile_updated_at = ?5
             WHERE user_id = ?1",
            params![
                user_id,
                profile.name_color.map(i64::from),
                style,
                profile.avatar,
                ordered(ts)
            ],
        )?;
        Ok(())
    }

    /// Store a blob: bytes under `blobs/<hex>`, a row in `blobs`. A
    /// known hash only refreshes the row.
    ///
    /// # Errors
    /// I/O on the blob file, or `SQLite` failure.
    pub fn store_blob(&self, blob: &BlobRecord) -> Result<(), PersistError> {
        fs::create_dir_all(&self.blobs).map_err(|e| PersistError::Io(self.blobs.clone(), e))?;
        let path = self.blobs.join(hex(&blob.blob_id));
        if !path.exists() {
            let tmp = path.with_extension("part");
            fs::write(&tmp, &blob.bytes).map_err(|e| PersistError::Io(tmp.clone(), e))?;
            fs::rename(&tmp, &path).map_err(|e| PersistError::Io(path.clone(), e))?;
        }
        self.conn.execute(
            "INSERT INTO blobs
                 (blob_id, mime, declared_mime, size, name, created_by, created_at, refs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
             ON CONFLICT(blob_id) DO NOTHING",
            params![
                blob.blob_id,
                blob.mime,
                blob.declared_mime,
                i64::from(blob.size),
                blob.name,
                blob.created_by,
                ordered(blob.created_at)
            ],
        )?;
        Ok(())
    }

    /// Adjust a blob's reference count (profiles pointing at it).
    ///
    /// Remove a blob: its row and its bytes (a missing file is fine).
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn delete_blob(&self, blob_id: &[u8; 32]) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("DELETE FROM blobs WHERE blob_id = ?1", params![blob_id])?;
        let path = self.blobs.join(hex(blob_id));
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("blob file {} not removed: {e}", path.display());
        }
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn set_blob_refs(&self, blob_id: &[u8; 32], refs: u32) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE blobs SET refs = ?2 WHERE blob_id = ?1",
            params![blob_id, i64::from(refs)],
        )?;
        Ok(())
    }

    /// A blob's bytes from disk, verified against its hash; `None`
    /// when the file is missing or corrupt.
    #[must_use]
    pub fn read_blob(&self, blob_id: &[u8; 32]) -> Option<Vec<u8>> {
        let path = self.blobs.join(hex(blob_id));
        let bytes = fs::read(&path).ok()?;
        let digest: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        if digest != *blob_id {
            warn!("blob {} on disk does not match its hash", path.display());
            return None;
        }
        Some(bytes)
    }

    fn load_blobs(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT blob_id, mime, size, name, created_by, created_at, refs FROM blobs")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, [u8; 32]>(0)?,
                BlobMeta {
                    mime: r.get(1)?,
                    size: u32::try_from(r.get::<_, i64>(2)?).unwrap_or(u32::MAX),
                    name: r.get(3)?,
                    created_by: r.get(4)?,
                    created_at: unordered(r.get(5)?),
                    refs: u32::try_from(r.get::<_, i64>(6)?).unwrap_or(0),
                },
            ))
        })? {
            let (id, meta) = row?;
            policy.blobs.insert(id, meta);
        }
        Ok(())
    }

    /// Insert one general-chat (or DM) message. A duplicate id is
    /// ignored: the tick's counter is the authority.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn insert_message(&self, m: &MessageRow) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO messages (msg_id, conv_id, ts, user_id, body, blob_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                ordered(m.msg_id),
                ordered(m.conv_id),
                ordered(m.ts),
                m.user_id,
                m.body,
                m.blob_id
            ],
        )?;
        Ok(())
    }

    /// Insert a coalesced batch of telemetry samples in one transaction.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn insert_telemetry(&mut self, rows: &[TelemetryRow]) -> Result<(), rusqlite::Error> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO telemetry_samples
                 (ts, user_id, key_hash, rtt_ms, path_loss, voice_loss, groups_behind,
                  rung, input_device, output_device, sample)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for r in rows {
                stmt.execute(params![
                    ordered(r.ts),
                    r.user_id,
                    r.key_hash,
                    r.rtt_ms,
                    r.path_loss,
                    r.voice_loss,
                    r.groups_behind.map(ordered),
                    r.rung,
                    r.input_device,
                    r.output_device,
                    r.sample,
                ])?;
            }
        }
        tx.commit()
    }

    /// Insert one telemetry event.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn insert_telemetry_event(&self, e: &TelemetryEventRow) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO telemetry_events (ts, user_id, key_hash, kind, event)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ordered(e.ts), e.user_id, e.key_hash, e.kind, e.event],
        )?;
        Ok(())
    }

    /// The last `limit` undeleted messages of `conv_id` before
    /// `before` (exclusive; `None` = the newest), oldest first.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn messages_before(
        &self,
        conv: ConvKey,
        before: Option<u64>,
        limit: u16,
    ) -> Result<Vec<MessageRow>, rusqlite::Error> {
        let Some(conv_id) = self.conv_id(conv, false)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT m.msg_id, m.conv_id, m.ts, m.user_id, m.body, m.edited_at,
                    COALESCE(u.name, ''), m.blob_id
             FROM messages m LEFT JOIN users u ON u.user_id = m.user_id
             WHERE m.conv_id = ?1 AND m.deleted_at IS NULL AND (?2 IS NULL OR m.msg_id < ?2)
             ORDER BY m.msg_id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![ordered(conv_id), before.map(ordered), i64::from(limit)],
            |row| {
                Ok(MessageRow {
                    msg_id: unordered(row.get(0)?),
                    conv_id: unordered(row.get(1)?),
                    ts: unordered(row.get(2)?),
                    user_id: row.get(3)?,
                    body: row.get(4)?,
                    edited: row.get::<_, Option<i64>>(5)?.is_some(),
                    name: row.get(6)?,
                    blob_id: row.get(7)?,
                })
            },
        )?;
        let mut out: Vec<MessageRow> = rows.collect::<Result<_, _>>()?;
        out.reverse();
        Ok(out)
    }

    // ── Direct messages (spec §6) ──────────────────────────────────────

    /// The `conv_id` of a conversation; a DM's is created on demand
    /// when `create` is set (the first message between the two).
    fn conv_id(&self, conv: ConvKey, create: bool) -> Result<Option<u64>, rusqlite::Error> {
        let ConvKey::Dm(x, y) = conv else {
            return Ok(Some(GENERAL_CONV));
        };
        let (a, b) = if x <= y { (x, y) } else { (y, x) };
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT conv_id FROM conversations WHERE kind = 1 AND user_a = ?1 AND user_b = ?2",
                params![a, b],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = found {
            return Ok(Some(unordered(id)));
        }
        if !create {
            return Ok(None);
        }
        self.conn.execute(
            "INSERT INTO conversations (kind, user_a, user_b) VALUES (1, ?1, ?2)",
            params![a, b],
        )?;
        Ok(Some(unordered(self.conn.last_insert_rowid())))
    }

    /// Store a DM: find or create the conversation, then the row. The
    /// row's `conv_id` is ignored; the pair decides.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn store_dm(
        &self,
        row: &MessageRow,
        a: &UserId,
        b: &UserId,
    ) -> Result<(), rusqlite::Error> {
        let conv_id = self
            .conv_id(ConvKey::Dm(*a, *b), true)?
            .unwrap_or(GENERAL_CONV);
        let mut stored = row.clone();
        stored.conv_id = conv_id;
        self.insert_message(&stored)
    }

    /// Everything up to `msg_id` has been read by `user_id`; marks
    /// only move forward.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn mark_read(
        &self,
        conv: ConvKey,
        user_id: &UserId,
        msg_id: u64,
    ) -> Result<(), rusqlite::Error> {
        let Some(conv_id) = self.conv_id(conv, false)? else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO message_reads (conv_id, user_id, last_read_msg_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(conv_id, user_id) DO UPDATE SET
                 last_read_msg_id = MAX(last_read_msg_id, excluded.last_read_msg_id)",
            params![ordered(conv_id), user_id, ordered(msg_id)],
        )?;
        Ok(())
    }

    /// This user's DM conversations: the other party, how many of
    /// their messages are past the read mark, the newest message.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn conversations_of(
        &self,
        user_id: &UserId,
    ) -> Result<Vec<protocol::DmSummary>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT CASE WHEN c.user_a = ?1 THEN c.user_b ELSE c.user_a END AS other,
                    COALESCE((SELECT u.name FROM users u
                              WHERE u.user_id = CASE WHEN c.user_a = ?1 THEN c.user_b ELSE c.user_a END), ''),
                    (SELECT COUNT(*) FROM messages m
                     WHERE m.conv_id = c.conv_id AND m.deleted_at IS NULL AND m.user_id != ?1
                       AND m.msg_id > COALESCE((SELECT r.last_read_msg_id FROM message_reads r
                                                WHERE r.conv_id = c.conv_id AND r.user_id = ?1), 0)),
                    COALESCE((SELECT m.ts FROM messages m
                              WHERE m.conv_id = c.conv_id AND m.deleted_at IS NULL
                              ORDER BY m.msg_id DESC LIMIT 1), 0),
                    COALESCE((SELECT MAX(m.msg_id) FROM messages m
                              WHERE m.conv_id = c.conv_id AND m.deleted_at IS NULL), 0)
             FROM conversations c
             WHERE c.kind = 1 AND (c.user_a = ?1 OR c.user_b = ?1)
             ORDER BY 4 DESC",
        )?;
        let rows = stmt.query_map(params![user_id], |row| {
            Ok(protocol::DmSummary {
                user_id: row.get(0)?,
                name: row.get(1)?,
                unread: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(u32::MAX),
                last_ts: unordered(row.get(3)?),
                last_msg_id: unordered(row.get(4)?),
            })
        })?;
        rows.collect()
    }

    /// Fold the WAL back into the main file without blocking writers.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn checkpoint(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
    }

    /// Delete rows past their retention window. Returns rows removed.
    ///
    /// # Errors
    /// `SQLite` failure.
    /// Unreferenced blobs older than the file window (spec §7): the
    /// rows go here, the bytes with them. Returns the ids removed.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn prune_blobs(&self, cutoff: WallMs) -> Result<Vec<[u8; 32]>, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT blob_id FROM blobs WHERE refs = 0 AND created_at < ?1")?;
        let ids: Vec<[u8; 32]> = stmt
            .query_map(params![ordered(cutoff)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in &ids {
            self.delete_blob(id)?;
        }
        Ok(ids)
    }

    pub fn prune(&self, now: WallMs, retention: Retention) -> Result<u64, rusqlite::Error> {
        let mut removed = 0u64;
        if let Some(cutoff) = cutoff(now, retention.history_days) {
            removed += count(self.conn.execute(
                "DELETE FROM messages WHERE ts < ?1",
                params![ordered(cutoff)],
            )?);
        }
        if let Some(cutoff) = cutoff(now, retention.telemetry_days) {
            removed += count(self.conn.execute(
                "DELETE FROM telemetry_samples WHERE ts < ?1",
                params![ordered(cutoff)],
            )?);
            removed += count(self.conn.execute(
                "DELETE FROM telemetry_events WHERE ts < ?1",
                params![ordered(cutoff)],
            )?);
        }
        if let Some(cut) = cutoff(now, retention.file_days) {
            removed += self.prune_blobs(cut)?.len() as u64;
        }
        Ok(removed)
    }

    /// Whether another connection committed to the database since the
    /// last call (the CLI, or an operator with `sqlite3`). Our own
    /// commits never change the value.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn changed_externally(&mut self) -> Result<bool, rusqlite::Error> {
        let version = read_data_version(&self.conn)?;
        let changed = version != self.data_version;
        self.data_version = version;
        Ok(changed)
    }

    // ── Policy (spec §0 rule 4: loaded once, consulted in RAM) ──────

    /// Read every policy table into a [`Policy`]. Run at startup and
    /// again after another process changed the file.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn load_policy(&self) -> Result<Policy, rusqlite::Error> {
        let mut policy = Policy::new();
        self.load_roles(&mut policy)?;
        self.load_users(&mut policy)?;
        self.load_keys(&mut policy)?;
        self.load_invites(&mut policy)?;
        self.load_restrictions(&mut policy)?;
        self.load_channels(&mut policy)?;
        self.load_progress(&mut policy)?;
        self.load_blobs(&mut policy)?;
        Ok(policy)
    }

    fn load_channels(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT channel_id, name, password, user_limit, min_role, permanent, afk_after_min,
                    created_by, created_at, empty_since
             FROM channels WHERE source = 1 ORDER BY created_at, channel_id",
        )?;
        for row in stmt.query_map([], |r| {
            Ok(UserChannelRecord {
                stable_id: unordered(r.get(0)?),
                name: r.get(1)?,
                password: r.get(2)?,
                user_limit: r.get(3)?,
                min_role: r.get(4)?,
                permanent: r.get::<_, i64>(5)? != 0,
                afk_after_min: r.get(6)?,
                owner: r.get(7)?,
                created_at: unordered(r.get(8)?),
                empty_since: r.get::<_, Option<i64>>(9)?.map(unordered),
            })
        })? {
            let channel = row?;
            policy.next_channel_id = policy.next_channel_id.max(channel.stable_id + 1);
            policy.channels.push(channel);
        }
        Ok(())
    }

    fn load_progress(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare("SELECT user_id, xp FROM user_progress")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, [u8; 32]>(0)?, unordered(r.get(1)?))))? {
            let (user, xp) = row?;
            policy.xp.insert(user, xp);
        }
        Ok(())
    }

    fn load_roles(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT role_id, name, permissions, rank, builtin, icon, color FROM roles")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                RoleRecord {
                    name: r.get(1)?,
                    permissions: bits_from_sql(r.get(2)?),
                    rank: r.get(3)?,
                    builtin: r.get::<_, i64>(4)? != 0,
                    icon: r.get(5)?,
                    color: u32::try_from(r.get::<_, i64>(6)?).unwrap_or(0),
                },
            ))
        })? {
            let (id, role) = row?;
            policy.roles.insert(id, role);
        }
        Ok(())
    }

    fn load_users(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, name, created_at, last_seen, invited_with,
                        name_color, name_style, avatar_blob
                 FROM users",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, [u8; 32]>(0)?,
                UserRecord {
                    name: r.get(1)?,
                    created_at: unordered(r.get(2)?),
                    last_seen: unordered(r.get(3)?),
                    invited_with: r.get(4)?,
                    roles: BTreeSet::new(),
                    profile: protocol::Profile {
                        name_color: r
                            .get::<_, Option<i64>>(5)?
                            .and_then(|c| u32::try_from(c).ok()),
                        name_style: match r.get::<_, i64>(6)? {
                            1 => protocol::NameStyle::Bold,
                            2 => protocol::NameStyle::Italic,
                            _ => protocol::NameStyle::Normal,
                        },
                        avatar: r.get::<_, Option<[u8; 32]>>(7)?,
                    },
                },
            ))
        })? {
            let (id, user) = row?;
            policy.users.insert(id, user);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT user_id, role_id FROM user_roles")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, u32>(1)?)))? {
            let (user, role) = row?;
            if let Some(u) = policy.users.get_mut(&user) {
                u.roles.insert(role);
            }
        }
        Ok(())
    }

    fn load_keys(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT key_hash, user_id, added_at, last_seen, retired_at FROM user_keys")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, [u8; 32]>(0)?,
                KeyRecord {
                    user_id: r.get(1)?,
                    added_at: unordered(r.get(2)?),
                    last_seen: unordered(r.get(3)?),
                    retired_at: r.get::<_, Option<i64>>(4)?.map(unordered),
                },
            ))
        })? {
            let (hash, key) = row?;
            policy.keys.insert(hash, key);
        }
        Ok(())
    }

    fn load_invites(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT code, created_by, created_at, expires_at, max_uses, uses, grants_role,
                    revoked_at
             FROM invites",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                InviteRecord {
                    created_by: r.get(1)?,
                    created_at: unordered(r.get(2)?),
                    expires_at: r.get::<_, Option<i64>>(3)?.map(unordered),
                    max_uses: r.get(4)?,
                    uses: r.get(5)?,
                    grants_role: r.get(6)?,
                    revoked_at: r.get::<_, Option<i64>>(7)?.map(unordered),
                },
            ))
        })? {
            let (code, invite) = row?;
            policy.invites.insert(code, invite);
        }
        Ok(())
    }

    fn load_restrictions(&self, policy: &mut Policy) -> Result<(), rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, key_hash, ip, kind, until, reason, by, created_at
             FROM restrictions ORDER BY id",
        )?;
        for row in stmt.query_map([], |r| {
            let kind: i64 = r.get(4)?;
            let ip: Option<String> = r.get(3)?;
            Ok((
                Restriction {
                    id: unordered(r.get(0)?),
                    user_id: r.get(1)?,
                    key_hash: r.get(2)?,
                    ip: None,
                    kind: RestrictionKind::Server,
                    until: r.get::<_, Option<i64>>(5)?.map(unordered),
                    reason: r.get(6)?,
                    by: r.get(7)?,
                    created_at: unordered(r.get(8)?),
                },
                kind,
                ip,
            ))
        })? {
            let (mut restriction, kind, ip) = row?;
            let id = restriction.id;
            let Some(kind) = RestrictionKind::from_db(kind) else {
                warn!("restriction {id} has unknown kind {kind}; ignoring it");
                continue;
            };
            restriction.kind = kind;
            restriction.ip = ip.and_then(|text| {
                let parsed = text.parse().ok();
                if parsed.is_none() {
                    warn!("restriction {id} has an unparsable address {text:?}; ignoring it");
                }
                parsed
            });
            policy.restrictions.push(restriction);
            policy.next_restriction_id = policy.next_restriction_id.max(id + 1);
        }
        Ok(())
    }

    /// Create or refresh a user row (name and last-seen).
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn upsert_user(
        &self,
        user_id: &[u8; 32],
        user: &UserRecord,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO users (user_id, name, created_at, last_seen, invited_with)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id) DO UPDATE SET
                 name = excluded.name,
                 last_seen = excluded.last_seen",
            params![
                user_id,
                user.name,
                ordered(user.created_at),
                ordered(user.last_seen),
                user.invited_with
            ],
        )?;
        Ok(())
    }

    /// Link (or refresh) a key on a user.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn upsert_key(&self, key_hash: &[u8; 32], key: &KeyRecord) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO user_keys (key_hash, user_id, added_at, last_seen, retired_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key_hash) DO UPDATE SET
                 user_id = excluded.user_id,
                 last_seen = excluded.last_seen,
                 retired_at = excluded.retired_at",
            params![
                key_hash,
                key.user_id,
                ordered(key.added_at),
                ordered(key.last_seen),
                key.retired_at.map(ordered)
            ],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn grant_role(
        &self,
        user_id: &[u8; 32],
        role_id: u32,
        granted_by: Option<&[u8; 32]>,
        now: WallMs,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO user_roles (user_id, role_id, granted_by, granted_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![user_id, role_id, granted_by, ordered(now)],
        )?;
        Ok(())
    }

    /// Add a custom role (spec §4). The id is `SQLite`'s next.
    ///
    /// # Errors
    /// `SQLite` failure (a duplicate name is the UNIQUE constraint).
    pub fn create_role(
        &self,
        name: &str,
        permissions: u64,
        rank: u16,
        icon: &str,
        color: u32,
    ) -> Result<u32, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO roles (name, permissions, rank, builtin, icon, color)
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![name, bits_to_sql(permissions), rank, icon, i64::from(color)],
        )?;
        u32::try_from(self.conn.last_insert_rowid())
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, self.conn.last_insert_rowid()))
    }

    /// A custom role with the id the tick picked (`role_id` is a plain
    /// INTEGER PRIMARY KEY, so explicit ids are fine).
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn insert_role(
        &self,
        role_id: u32,
        name: &str,
        permissions: u64,
        rank: u16,
        icon: &str,
        color: u32,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO roles (role_id, name, permissions, rank, builtin, icon, color)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![
                role_id,
                name,
                bits_to_sql(permissions),
                rank,
                icon,
                i64::from(color)
            ],
        )?;
        Ok(())
    }

    /// Rewrite a custom role's name, permissions and rank.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn update_role(
        &self,
        role_id: u32,
        name: &str,
        permissions: u64,
        rank: u16,
        icon: &str,
        color: u32,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE roles SET name = ?2, permissions = ?3, rank = ?4, icon = ?5, color = ?6
             WHERE role_id = ?1 AND builtin = 0",
            params![
                role_id,
                name,
                bits_to_sql(permissions),
                rank,
                icon,
                i64::from(color)
            ],
        )?;
        Ok(())
    }

    /// Remove a custom role; `user_roles` rows cascade, so whoever
    /// held it simply loses it.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn delete_role(&self, role_id: u32) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "DELETE FROM roles WHERE role_id = ?1 AND builtin = 0",
            params![role_id],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn revoke_role(&self, user_id: &[u8; 32], role_id: u32) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "DELETE FROM user_roles WHERE user_id = ?1 AND role_id = ?2",
            params![user_id, role_id],
        )?;
        Ok(())
    }

    /// Forget every device key of a user.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn delete_user_keys(&self, user_id: &[u8; 32]) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("DELETE FROM user_keys WHERE user_id = ?1", params![user_id])?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn insert_invite(&self, code: &str, invite: &InviteRecord) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO invites
             (code, created_by, created_at, expires_at, max_uses, uses, grants_role, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                code,
                invite.created_by,
                ordered(invite.created_at),
                invite.expires_at.map(ordered),
                invite.max_uses,
                invite.uses,
                invite.grants_role,
                invite.revoked_at.map(ordered)
            ],
        )?;
        Ok(())
    }

    /// Count one admission through `code`.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn use_invite(&self, code: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE invites SET uses = uses + 1 WHERE code = ?1",
            params![code],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn revoke_invite(&self, code: &str, now: WallMs) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE invites SET revoked_at = ?2 WHERE code = ?1 AND revoked_at IS NULL",
            params![code, ordered(now)],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn insert_restriction(&self, r: &Restriction) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO restrictions
             (id, user_id, key_hash, ip, kind, until, reason, by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                ordered(r.id),
                r.user_id,
                r.key_hash,
                r.ip.map(|ip| ip.to_string()),
                r.kind.to_db(),
                r.until.map(ordered),
                r.reason,
                r.by,
                ordered(r.created_at)
            ],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn delete_restriction(&self, id: u64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "DELETE FROM restrictions WHERE id = ?1",
            params![ordered(id)],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn insert_audit(&self, a: &AuditRow) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO audit (ts, actor_user_id, action, target, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ordered(a.ts), a.actor, a.action, a.target, a.detail],
        )?;
        Ok(())
    }

    /// The audit log, newest first, `limit` entries before id `before`.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn audit_before(
        &self,
        before: Option<u64>,
        limit: u16,
    ) -> Result<Vec<(u64, AuditRow)>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, actor_user_id, action, target, detail FROM audit
             WHERE (?1 IS NULL OR id < ?1) ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![before.map(ordered), i64::from(limit)], |row| {
            Ok((
                unordered(row.get(0)?),
                AuditRow {
                    ts: unordered(row.get(1)?),
                    actor: row.get(2)?,
                    action: row.get(3)?,
                    target: row.get(4)?,
                    detail: row.get(5)?,
                },
            ))
        })?;
        rows.collect()
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn edit_message(&self, msg_id: u64, body: &str, ts: WallMs) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE messages SET body = ?2, edited_at = ?3 WHERE msg_id = ?1",
            params![ordered(msg_id), body, ordered(ts)],
        )?;
        Ok(())
    }

    /// Soft delete: the row stays, history skips it.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn delete_message(&self, msg_id: u64, ts: WallMs) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE messages SET deleted_at = ?2 WHERE msg_id = ?1",
            params![ordered(msg_id), ordered(ts)],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn upsert_channel(&self, c: &UserChannelRecord) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO channels (channel_id, name, password, user_limit, min_role, permanent,
                                   afk_after_min, is_afk, source, created_by, created_at,
                                   position, empty_since)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 1, ?8, ?9, 0, ?10)
             ON CONFLICT(channel_id) DO UPDATE SET
                 name = excluded.name, password = excluded.password,
                 user_limit = excluded.user_limit, min_role = excluded.min_role,
                 permanent = excluded.permanent, afk_after_min = excluded.afk_after_min,
                 empty_since = excluded.empty_since",
            params![
                ordered(c.stable_id),
                c.name,
                c.password,
                c.user_limit,
                c.min_role,
                i64::from(c.permanent),
                c.afk_after_min,
                c.owner,
                ordered(c.created_at),
                c.empty_since.map(ordered)
            ],
        )?;
        Ok(())
    }

    /// # Errors
    /// `SQLite` failure.
    pub fn delete_channel(&self, stable_id: u64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "DELETE FROM channels WHERE channel_id = ?1",
            params![ordered(stable_id)],
        )?;
        Ok(())
    }

    /// Add to a user's progress counters (spec §9.1).
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn add_progress(&self, p: &ProgressDelta) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO user_progress (user_id, xp, talk_seconds, share_seconds, watch_seconds,
                                        online_seconds, messages, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(user_id) DO UPDATE SET
                 xp = xp + excluded.xp,
                 talk_seconds = talk_seconds + excluded.talk_seconds,
                 share_seconds = share_seconds + excluded.share_seconds,
                 watch_seconds = watch_seconds + excluded.watch_seconds,
                 online_seconds = online_seconds + excluded.online_seconds,
                 messages = messages + excluded.messages,
                 updated_at = excluded.updated_at",
            params![
                p.user_id,
                ordered(p.xp),
                ordered(p.talk_seconds),
                ordered(p.share_seconds),
                ordered(p.watch_seconds),
                ordered(p.online_seconds),
                ordered(p.messages),
                ordered(p.ts)
            ],
        )?;
        Ok(())
    }

    /// Delete a person (spec §8): the row cascades to keys, roles,
    /// restrictions and progress; their messages go too.
    ///
    /// # Errors
    /// `SQLite` failure.
    pub fn purge_user(&self, user_id: &[u8; 32]) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("DELETE FROM messages WHERE user_id = ?1", params![user_id])?;
        self.conn.execute(
            "DELETE FROM invites WHERE created_by = ?1",
            params![user_id],
        )?;
        self.conn
            .execute("DELETE FROM users WHERE user_id = ?1", params![user_id])?;
        Ok(())
    }

    /// Final checkpoint on shutdown so the main file is complete on its
    /// own (a restore from a plain file copy then has nothing to
    /// recover).
    pub fn close(self) {
        if let Err(e) = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            log::warn!("Final WAL checkpoint failed: {e}");
        }
        if let Err((_, e)) = self.conn.close() {
            log::warn!("Closing the database: {e}");
        }
    }
}

/// `":memory:"` is `SQLite`'s private in-memory database — used by unit
/// tests; it has no directory to create.
fn is_memory_path(path: &Path) -> bool {
    path.to_str() == Some(":memory:")
}

fn read_data_version(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row("PRAGMA data_version", [], |row| row.get(0))
}

/// Apply every migration past the recorded version, each in its own
/// transaction. A database from a newer build is an error, not a guess.
fn migrate(conn: &mut Connection) -> Result<(), PersistError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")?;
    let current: u32 = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .optional()?
        .unwrap_or(0);
    if current > SCHEMA_VERSION {
        return Err(PersistError::Downgrade {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    let applied = usize::try_from(current).unwrap_or(usize::MAX);
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let version = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("DELETE FROM schema_version", [])?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![version],
        )?;
        tx.commit()?;
        info!("Database migrated to schema version {version}");
    }
    Ok(())
}

/// Write the built-in roles' current permission sets and ranks. Run at
/// every startup so a permission added in code reaches an existing
/// database without a migration; custom roles are untouched.
fn sync_builtin_roles(conn: &Connection) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(
        "INSERT INTO roles (role_id, name, permissions, rank, builtin)
         VALUES (?1, ?2, ?3, ?4, 1)
         ON CONFLICT(role_id) DO UPDATE SET
             name = excluded.name,
             permissions = excluded.permissions,
             rank = excluded.rank,
             builtin = 1",
    )?;
    for role in BUILTIN_ROLES {
        stmt.execute(params![
            role.id,
            role.name,
            bits_to_sql(role.permissions),
            role.rank
        ])?;
    }
    Ok(())
}

/// `now − days`, or `None` when `days` is zero (keep forever).
fn cutoff(now: WallMs, days: u32) -> Option<WallMs> {
    if days == 0 {
        return None;
    }
    let window = u64::from(days).saturating_mul(24 * 60 * 60 * 1000);
    Some(now.saturating_sub(window))
}

/// Ids and timestamps: unsigned in Rust, `INTEGER` (i64) in `SQLite`.
/// They never approach the sign bit; clamp rather than wrap so ordering
/// is preserved if they ever did.
fn ordered(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unordered(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Permission bitsets keep their bit pattern across the i64 column.
fn bits_to_sql(bits: u64) -> i64 {
    i64::from_ne_bytes(bits.to_ne_bytes())
}

fn bits_from_sql(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

/// Rows affected, as a counter.
fn count(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

// ── Offline operations (CLI) ────────────────────────────────────────────

/// `server backup <file.tar>`: a consistent snapshot of the database
/// through `SQLite`'s online backup API (a plain copy of a WAL database
/// is not safe) plus the `blobs/` directory, in one archive — either
/// half alone restores to dangling references.
///
/// # Errors
/// No database at `db_path`, a `SQLite` failure, or an archive I/O error.
/// `server restore`: unpack a `server backup` archive over the data
/// directory — the database and the blobs. Refuses while the server
/// holds the database (the `-wal` file exists) unless forced. The
/// current database, if any, is kept beside as `baucord.db.before-restore`.
///
/// # Errors
/// The archive is missing or not a backup, the server is running, or
/// an I/O failure.
pub fn restore_from_tar(
    db_path: &Path,
    archive: &Path,
    force: bool,
) -> Result<RestoreReport, PersistError> {
    if !archive.is_file() {
        return Err(PersistError::Missing(archive.to_path_buf()));
    }
    let io_err = |e: std::io::Error| PersistError::Io(db_path.to_path_buf(), e);
    let wal = db_path.with_extension("db-wal");
    if wal.exists() && !force {
        return Err(PersistError::Locked(db_path.to_path_buf()));
    }
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_err)?;
    let file = File::open(archive).map_err(|e| PersistError::Io(archive.to_path_buf(), e))?;
    let mut tar = tar::Archive::new(file);
    let mut report = RestoreReport::default();
    // Unpack into a staging directory first: a bad archive must not
    // leave half a database behind.
    let staging = dir.join(format!(".restore-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(io_err)?;
    let result = (|| -> Result<(), PersistError> {
        for entry in tar
            .entries()
            .map_err(|e| PersistError::Io(archive.to_path_buf(), e))?
        {
            let mut entry = entry.map_err(|e| PersistError::Io(archive.to_path_buf(), e))?;
            let path = entry
                .path()
                .map_err(|e| PersistError::Io(archive.to_path_buf(), e))?
                .into_owned();
            let name = path.to_string_lossy().to_string();
            if name == "baucord.db" {
                entry.unpack(staging.join("baucord.db")).map_err(io_err)?;
                report.database = true;
            } else if let Some(rest) = name.strip_prefix("blobs/")
                && !rest.is_empty()
                && !rest.contains("..")
            {
                let target = staging.join("blobs").join(rest);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(io_err)?;
                }
                if entry.header().entry_type().is_file() {
                    entry.unpack(&target).map_err(io_err)?;
                    report.blobs += 1;
                }
            }
        }
        if !report.database {
            return Err(PersistError::NotABackup(archive.to_path_buf()));
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    // Swap in: the old database is kept beside, blobs replaced whole.
    if db_path.is_file() {
        let kept = db_path.with_extension("db.before-restore");
        fs::rename(db_path, &kept).map_err(io_err)?;
        report.kept = Some(kept);
    }
    for suffix in ["db-wal", "db-shm"] {
        let _ = fs::remove_file(db_path.with_extension(suffix));
    }
    fs::rename(staging.join("baucord.db"), db_path).map_err(io_err)?;
    let blobs = blobs_dir(db_path);
    let _ = fs::remove_dir_all(&blobs);
    if staging.join("blobs").is_dir() {
        fs::rename(staging.join("blobs"), &blobs).map_err(io_err)?;
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(report)
}

/// What `server restore` put in place.
#[derive(Debug, Default)]
pub struct RestoreReport {
    pub database: bool,
    pub blobs: usize,
    /// Where the previous database went.
    pub kept: Option<PathBuf>,
}

/// `server db prune`: the hourly retention pass, now. Refuses while
/// the server holds the database.
///
/// # Errors
/// No database, the server is running, or a `SQLite` failure.
pub fn cli_prune(db_path: &Path, retention: Retention) -> Result<u64, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    if db_path.with_extension("db-wal").exists() {
        return Err(PersistError::Locked(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let removed = db.prune(super::wall_now(), retention)?;
    db.close();
    Ok(removed)
}

pub fn backup_to_tar(db_path: &Path, out: &Path) -> Result<BackupReport, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let src = Connection::open(db_path)?;
    src.busy_timeout(BUSY_TIMEOUT)?;
    let snapshot = out.with_extension(format!("snapshot-{}.db", std::process::id()));
    src.backup(MAIN_DB, &snapshot, None)?;
    drop(src);

    let result = write_archive(db_path, &snapshot, out);
    // Best effort: the snapshot is scratch either way.
    let _ = fs::remove_file(&snapshot);
    result
}

fn write_archive(
    db_path: &Path,
    snapshot: &Path,
    out: &Path,
) -> Result<BackupReport, PersistError> {
    let archive_err = |e: std::io::Error| PersistError::Archive(out.to_path_buf(), e);
    let file = File::create(out).map_err(archive_err)?;
    let mut tar = tar::Builder::new(file);
    tar.append_path_with_name(snapshot, "baucord.db")
        .map_err(archive_err)?;
    let blobs = blobs_dir(db_path);
    let blob_files = if blobs.is_dir() {
        tar.append_dir_all("blobs", &blobs).map_err(archive_err)?;
        fs::read_dir(&blobs).map_or(0, |entries| entries.filter_map(Result::ok).count())
    } else {
        0
    };
    let file = tar.into_inner().map_err(archive_err)?;
    file.sync_all().map_err(archive_err)?;
    let db_bytes = fs::metadata(snapshot).map_or(0, |m| m.len());
    Ok(BackupReport {
        db_bytes,
        blob_files,
    })
}

/// Where blobs live: `blobs/` next to the database file (spec §1).
#[must_use]
pub fn blobs_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map_or_else(|| PathBuf::from("blobs"), |dir| dir.join("blobs"))
}

/// What `backup_to_tar` archived.
#[derive(Debug, Clone, Copy)]
pub struct BackupReport {
    pub db_bytes: u64,
    pub blob_files: usize,
}

/// `server invite create`: mint a code straight into the database.
/// A running server notices within 2 s (`data_version`).
///
/// # Errors
/// No database at `db_path`, an unknown role, or a `SQLite` failure.
pub fn cli_create_invite(
    db_path: &Path,
    role: u32,
    ttl_ms: Option<u64>,
    max_uses: Option<u32>,
) -> Result<(String, InviteRecord), PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let known_role: Option<i64> = db
        .conn
        .query_row(
            "SELECT role_id FROM roles WHERE role_id = ?1",
            params![role],
            |r| r.get(0),
        )
        .optional()?;
    if known_role.is_none() {
        return Err(PersistError::UnknownRole(role));
    }
    let now = super::wall_now();
    let code = crate::policy::generate_invite_code();
    let invite = InviteRecord {
        created_by: None,
        created_at: now,
        expires_at: ttl_ms.map(|ttl| now.saturating_add(ttl)),
        max_uses,
        uses: 0,
        grants_role: role,
        revoked_at: None,
    };
    db.insert_invite(&code, &invite)?;
    db.insert_audit(&AuditRow {
        ts: now,
        actor: None,
        action: "invite.create".to_string(),
        target: Some(code.clone()),
        detail: Some(format!(
            "cli: role {role}, max_uses {max_uses:?}, ttl_ms {ttl_ms:?}"
        )),
    })?;
    db.close();
    Ok((code, invite))
}

/// `server user list` and friends: the policy tables as they stand.
///
/// # Errors
/// No database at `db_path`, or a `SQLite` failure.
pub fn cli_load_policy(db_path: &Path) -> Result<Policy, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    db.close();
    Ok(policy)
}

/// A user named on the command line: a name of record
/// (case-insensitive), or a user id in hex — the full 64 digits or a
/// prefix of at least 8 that matches exactly one user. Several people
/// with the same name are told apart by their ids.
///
/// # Errors
/// No database, no such user, or an ambiguous name.
pub fn cli_find_user(db_path: &Path, who: &str) -> Result<([u8; 32], Policy), PersistError> {
    let policy = cli_load_policy(db_path)?;
    let who = who.trim();
    let lower = who.to_ascii_lowercase();
    let looks_hex = lower.len() >= 8 && lower.bytes().all(|b| b.is_ascii_hexdigit());
    let mut matches: Vec<[u8; 32]> = if looks_hex {
        policy
            .users
            .keys()
            .filter(|id| hex(*id).starts_with(&lower))
            .copied()
            .collect()
    } else {
        Vec::new()
    };
    if matches.is_empty() {
        matches = policy
            .users
            .iter()
            .filter(|(_, u)| u.name.eq_ignore_ascii_case(who))
            .map(|(id, _)| *id)
            .collect();
    }
    match matches.as_slice() {
        [one] => Ok((*one, policy)),
        [] => Err(PersistError::NoSuchUser(who.to_string())),
        many => {
            let now = super::wall_now();
            let mut lines: Vec<String> = many
                .iter()
                .filter_map(|id| policy.users.get(id).map(|u| (id, u)))
                .map(|(id, u)| {
                    let roles: Vec<&str> = u
                        .roles
                        .iter()
                        .filter_map(|r| policy.roles.get(r))
                        .map(|r| r.name.as_str())
                        .collect();
                    let ago = now.saturating_sub(u.last_seen) / 60_000;
                    format!(
                        "{} ({}, last seen {} min ago)",
                        hex(&id[..8]),
                        if roles.is_empty() {
                            "no roles".to_string()
                        } else {
                            roles.join(",")
                        },
                        ago
                    )
                })
                .collect();
            lines.sort();
            Err(PersistError::Ambiguous(who.to_string(), lines))
        }
    }
}

/// A full 64-hex-digit id; `None` for anything else.
#[must_use]
pub fn parse_hex_id(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Hex of a byte slice.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// `server user grant|revoke`: replace a user's role set. The last
/// admin keeps `admin`. A running server picks it up within 2 s.
///
/// # Errors
/// No database, an unknown role, or the last admin.
pub fn cli_set_roles(
    db_path: &Path,
    user_id: &[u8; 32],
    roles: &[u32],
) -> Result<(), PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    for role in roles {
        if !policy.roles.contains_key(role) {
            return Err(PersistError::UnknownRole(*role));
        }
    }
    let admin_bit = protocol::permissions::ADMIN;
    let new_perms = roles
        .iter()
        .filter_map(|r| policy.roles.get(r))
        .fold(0, |acc, r| acc | r.permissions);
    let was_admin = protocol::permissions::has(policy.permissions_of(user_id), admin_bit);
    let others = policy
        .users
        .keys()
        .filter(|u| *u != user_id)
        .any(|u| protocol::permissions::has(policy.permissions_of(u), admin_bit));
    if was_admin && !protocol::permissions::has(new_perms, admin_bit) && !others {
        return Err(PersistError::LastAdmin);
    }
    let before: Vec<u32> = policy
        .users
        .get(user_id)
        .map(|u| u.roles.iter().copied().collect())
        .unwrap_or_default();
    let now = super::wall_now();
    for role in &before {
        if !roles.contains(role) {
            db.revoke_role(user_id, *role)?;
        }
    }
    for role in roles {
        if !before.contains(role) {
            db.grant_role(user_id, *role, None, now)?;
        }
    }
    db.insert_audit(&AuditRow {
        ts: now,
        actor: None,
        action: "user.roles".to_string(),
        target: Some(hex(&user_id[..8])),
        detail: Some(format!("cli: {roles:?}")),
    })?;
    db.close();
    Ok(())
}

/// What `server role edit` changes; `None` keeps the current value.
/// The rank is never given: it follows the permissions
/// (`permissions::derived_rank`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RoleEdit {
    pub name: Option<String>,
    pub permissions: Option<u64>,
    pub icon: Option<String>,
    pub color: Option<u32>,
}

/// The rules a custom role obeys (spec §4): a strict name that is not
/// a built-in's, no `ADMIN` bit, a glyph from the list.
fn check_custom_role(name: &str, permissions: u64, icon: &str) -> Result<(), PersistError> {
    use protocol::permissions as p;
    if !p::valid_custom_role_name(name) {
        return Err(PersistError::BadRoleName(name.to_string()));
    }
    if p::has(permissions, p::ADMIN) {
        return Err(PersistError::AdminBit);
    }
    if !p::valid_role_icon(icon) {
        return Err(PersistError::BadIcon(icon.to_string()));
    }
    Ok(())
}

/// A role named on the command line: a name (case-insensitive) or an
/// id.
fn find_role(policy: &Policy, who: &str) -> Result<u32, PersistError> {
    let lower = who.trim().to_ascii_lowercase();
    if let Some((id, _)) = policy.roles.iter().find(|(_, r)| r.name == lower) {
        return Ok(*id);
    }
    lower
        .parse::<u32>()
        .ok()
        .filter(|id| policy.roles.contains_key(id))
        .ok_or_else(|| PersistError::NoSuchRole(who.trim().to_string()))
}

/// `server role create`: a custom role with a permission set; its rank
/// follows from the set. A running server picks it up within 2 s and
/// tells every client the new table.
///
/// # Errors
/// No database, a bad name or permission set, a name in use, or a
/// `SQLite` failure.
pub fn cli_role_create(
    db_path: &Path,
    name: &str,
    permissions: u64,
    icon: &str,
    color: u32,
) -> Result<u32, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    check_custom_role(name, permissions, icon)?;
    let rank = protocol::permissions::derived_rank(permissions);
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    if policy.roles.values().any(|r| r.name == name) {
        db.close();
        return Err(PersistError::RoleNameTaken(name.to_string()));
    }
    let id = db.create_role(name, permissions, rank, icon, color)?;
    db.insert_audit(&AuditRow {
        ts: super::wall_now(),
        actor: None,
        action: "role.create".to_string(),
        target: Some(id.to_string()),
        detail: Some(format!(
            "cli: {name} rank {rank} permissions {permissions:#x}"
        )),
    })?;
    db.close();
    Ok(id)
}

/// `server role edit`: change a custom role's name or permissions (the
/// rank follows). Built-ins are refused: their sets come from the
/// binary and are rewritten at every start.
///
/// # Errors
/// No database or role, a built-in, a bad value, or a `SQLite` failure.
pub fn cli_role_edit(db_path: &Path, who: &str, edit: &RoleEdit) -> Result<u32, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    let id = find_role(&policy, who)?;
    let current = &policy.roles[&id];
    if current.builtin {
        db.close();
        return Err(PersistError::BuiltinRole(current.name.clone()));
    }
    let name = edit.name.clone().unwrap_or_else(|| current.name.clone());
    let permissions = edit.permissions.unwrap_or(current.permissions);
    let icon = edit.icon.clone().unwrap_or_else(|| current.icon.clone());
    let color = edit.color.unwrap_or(current.color);
    check_custom_role(&name, permissions, &icon)?;
    let rank = protocol::permissions::derived_rank(permissions);
    if name != current.name && policy.roles.values().any(|r| r.name == name) {
        db.close();
        return Err(PersistError::RoleNameTaken(name));
    }
    db.update_role(id, &name, permissions, rank, &icon, color)?;
    db.insert_audit(&AuditRow {
        ts: super::wall_now(),
        actor: None,
        action: "role.edit".to_string(),
        target: Some(id.to_string()),
        detail: Some(format!(
            "cli: {name} rank {rank} permissions {permissions:#x}"
        )),
    })?;
    db.close();
    Ok(id)
}

/// `server role delete`: remove a custom role; whoever held it loses
/// it (their session's permissions refresh within 2 s).
///
/// # Errors
/// No database or role, a built-in, or a `SQLite` failure.
pub fn cli_role_delete(db_path: &Path, who: &str) -> Result<(u32, String), PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    let id = find_role(&policy, who)?;
    let role = &policy.roles[&id];
    if role.builtin {
        db.close();
        return Err(PersistError::BuiltinRole(role.name.clone()));
    }
    let name = role.name.clone();
    db.delete_role(id)?;
    db.insert_audit(&AuditRow {
        ts: super::wall_now(),
        actor: None,
        action: "role.delete".to_string(),
        target: Some(id.to_string()),
        detail: Some(format!("cli: {name}")),
    })?;
    db.close();
    Ok((id, name))
}

/// `server user kick`: forget a user's device keys and roles, so they
/// need a new invite. A running server closes the session within 2 s.
///
/// # Errors
/// No database, or a `SQLite` failure.
pub fn cli_kick(db_path: &Path, user_id: &[u8; 32], reason: &str) -> Result<(), PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    db.delete_user_keys(user_id)?;
    if let Some(u) = policy.users.get(user_id) {
        for role in &u.roles {
            db.revoke_role(user_id, *role)?;
        }
    }
    db.insert_audit(&AuditRow {
        ts: super::wall_now(),
        actor: None,
        action: "user.kick".to_string(),
        target: Some(hex(&user_id[..8])),
        detail: Some(format!("cli: {reason}")),
    })?;
    db.close();
    Ok(())
}

/// `server user purge`: delete a person and their messages; one audit
/// line remains (spec §8).
///
/// # Errors
/// No database, or a `SQLite` failure.
pub fn cli_purge(db_path: &Path, user_id: &[u8; 32]) -> Result<(), PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    db.purge_user(user_id)?;
    db.insert_audit(&AuditRow {
        ts: super::wall_now(),
        actor: None,
        action: "user.purge".to_string(),
        target: Some(hex(&user_id[..8])),
        detail: Some("cli".to_string()),
    })?;
    db.close();
    Ok(())
}

/// One stored telemetry sample, the queryable columns (spec §9).
#[derive(Debug, Clone)]
pub struct TelemetryLine {
    pub ts: WallMs,
    pub rtt_ms: u32,
    pub path_loss: f32,
    pub voice_loss: Option<f32>,
    pub groups_behind: Option<u64>,
    pub rung: Option<u32>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

/// `server telemetry <who>`: the newest `limit` samples of a user
/// since `since` (ms since the epoch), newest last.
///
/// # Errors
/// No database, or a `SQLite` failure.
pub fn cli_telemetry(
    db_path: &Path,
    user_id: &[u8; 32],
    since: WallMs,
    limit: u32,
) -> Result<Vec<TelemetryLine>, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let mut stmt = db.conn.prepare(
        "SELECT ts, rtt_ms, path_loss, voice_loss, groups_behind, rung, input_device,
                output_device
         FROM telemetry_samples WHERE user_id = ?1 AND ts >= ?2
         ORDER BY ts DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![user_id, ordered(since), i64::from(limit)], |r| {
        Ok(TelemetryLine {
            ts: unordered(r.get(0)?),
            rtt_ms: r.get(1)?,
            path_loss: r.get(2)?,
            voice_loss: r.get(3)?,
            groups_behind: r.get::<_, Option<i64>>(4)?.map(unordered),
            rung: r.get(5)?,
            input_device: r.get(6)?,
            output_device: r.get(7)?,
        })
    })?;
    let mut lines: Vec<TelemetryLine> = rows.collect::<Result<_, _>>()?;
    drop(stmt);
    db.close();
    lines.reverse();
    Ok(lines)
}

/// `server user ban`, `server user mute`: add a restriction. The
/// id is assigned here; a running server picks the row up within 2 s
/// and removes a live session if it is a `server` one.
///
/// # Errors
/// No database, or a `SQLite` failure.
pub fn cli_restrict(db_path: &Path, mut restriction: Restriction) -> Result<u64, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    restriction.id = policy.next_restriction_id;
    restriction.created_at = super::wall_now();
    db.insert_restriction(&restriction)?;
    db.insert_audit(&AuditRow {
        ts: restriction.created_at,
        actor: None,
        action: "restrict".to_string(),
        target: restriction
            .user_id
            .map(|u| hex(&u[..8]))
            .or_else(|| restriction.key_hash.map(|k| hex(&k[..8]))),
        detail: Some(format!(
            "cli: {} until {:?}: {}",
            restriction.kind.name(),
            restriction.until,
            restriction.reason
        )),
    })?;
    db.close();
    Ok(restriction.id)
}

/// `server restriction lift ID`. Returns whether it existed.
///
/// # Errors
/// No database, or a `SQLite` failure.
pub fn cli_lift_restriction(db_path: &Path, id: u64) -> Result<bool, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let changed = db.conn.execute(
        "DELETE FROM restrictions WHERE id = ?1",
        params![ordered(id)],
    )?;
    if changed > 0 {
        db.insert_audit(&AuditRow {
            ts: super::wall_now(),
            actor: None,
            action: "restriction.lift".to_string(),
            target: Some(id.to_string()),
            detail: Some("cli".to_string()),
        })?;
    }
    db.close();
    Ok(changed > 0)
}

/// `server key link|unlink`: attach a key to a user, or retire it.
///
/// # Errors
/// No database, or a `SQLite` failure.
pub fn cli_set_key(
    db_path: &Path,
    key_hash: &[u8; 32],
    link_to: Option<&[u8; 32]>,
) -> Result<bool, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    let now = super::wall_now();
    let existing = policy.keys.get(key_hash).cloned();
    let (record, action) = if let Some(user) = link_to {
        (
            KeyRecord {
                user_id: *user,
                added_at: existing.as_ref().map_or(now, |k| k.added_at),
                last_seen: now,
                retired_at: None,
            },
            "key.link",
        )
    } else {
        let Some(mut key) = existing else {
            db.close();
            return Ok(false);
        };
        key.retired_at = Some(now);
        (key, "key.unlink")
    };
    db.upsert_key(key_hash, &record)?;
    db.insert_audit(&AuditRow {
        ts: now,
        actor: None,
        action: action.to_string(),
        target: Some(hex(&key_hash[..8])),
        detail: Some("cli".to_string()),
    })?;
    db.close();
    Ok(true)
}

/// `server invite list`: every code, newest first.
///
/// # Errors
/// No database at `db_path`, or a `SQLite` failure.
pub fn cli_list_invites(db_path: &Path) -> Result<Vec<(String, InviteRecord)>, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let policy = db.load_policy()?;
    db.close();
    let mut invites: Vec<(String, InviteRecord)> = policy.invites.into_iter().collect();
    invites.sort_by(|a, b| {
        b.1.created_at
            .cmp(&a.1.created_at)
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(invites)
}

/// `server invite revoke CODE`. Returns whether the code existed and
/// was not already revoked.
///
/// # Errors
/// No database at `db_path`, or a `SQLite` failure.
pub fn cli_revoke_invite(db_path: &Path, code: &str) -> Result<bool, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let (db, _) = Sqlite::open(db_path)?;
    let now = super::wall_now();
    let changed = db.conn.execute(
        "UPDATE invites SET revoked_at = ?2 WHERE code = ?1 AND revoked_at IS NULL",
        params![code, ordered(now)],
    )?;
    if changed > 0 {
        db.insert_audit(&AuditRow {
            ts: now,
            actor: None,
            action: "invite.revoke".to_string(),
            target: Some(code.to_string()),
            detail: Some("cli".to_string()),
        })?;
    }
    db.close();
    Ok(changed > 0)
}

/// `server db check`: integrity, schema version and row counts. Opens
/// the file as-is — no migration is applied.
///
/// # Errors
/// No database at `db_path`, or a `SQLite` failure.
pub fn check(db_path: &Path) -> Result<CheckReport, PersistError> {
    if !db_path.is_file() {
        return Err(PersistError::Missing(db_path.to_path_buf()));
    }
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mut integrity_stmt = conn.prepare("PRAGMA integrity_check")?;
    let integrity: Vec<String> = integrity_stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    drop(integrity_stmt);
    let schema_version: Option<u32> = conn
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .optional()
        .unwrap_or(None);
    let mut counts = Vec::with_capacity(COUNTED_TABLES.len());
    for table in COUNTED_TABLES {
        // Table names come from our own constant list, never from input.
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or(-1);
        counts.push((*table, count));
    }
    let file_bytes = fs::metadata(db_path).map_or(0, |m| m.len());
    Ok(CheckReport {
        integrity,
        schema_version,
        supported_version: SCHEMA_VERSION,
        counts,
        file_bytes,
    })
}

/// What `check` found.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// `["ok"]` when `SQLite` found nothing wrong; otherwise one line per
    /// problem.
    pub integrity: Vec<String>,
    /// `None` if the file has no `schema_version` table (not a baucord
    /// database, or an empty file).
    pub schema_version: Option<u32>,
    pub supported_version: u32,
    /// Rows per table; `-1` where the table does not exist.
    pub counts: Vec<(&'static str, i64)>,
    pub file_bytes: u64,
}

impl CheckReport {
    /// Integrity passed and the schema is one this build can open.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.integrity.as_slice() == ["ok"]
            && self
                .schema_version
                .is_some_and(|v| v <= self.supported_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Sqlite {
        Sqlite::open(Path::new(":memory:")).expect("open").0
    }

    fn row(msg_id: u64, body: &str) -> MessageRow {
        MessageRow {
            msg_id,
            conv_id: GENERAL_CONV,
            ts: 1_000 + msg_id,
            user_id: [7u8; 32],
            body: body.to_string(),
            name: String::new(),
            edited: false,
            blob_id: None,
        }
    }

    #[test]
    fn open_creates_schema_and_starts_ids_at_one() {
        let (_, next) = Sqlite::open(Path::new(":memory:")).expect("open");
        assert_eq!(next, 1);
    }

    #[test]
    fn open_rebases_the_message_counter_on_the_watermark() {
        let dir = std::env::temp_dir().join(format!(
            "baucord-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let path = dir.join("baucord.db");
        {
            let (db, _) = Sqlite::open(&path).expect("open");
            db.insert_message(&row(41, "a")).expect("insert");
            db.insert_message(&row(42, "b")).expect("insert");
            db.close();
        }
        let (db, next) = Sqlite::open(&path).expect("reopen");
        assert_eq!(next, 43);
        db.close();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_roles_are_present_after_open() {
        let db = memory();
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM roles WHERE builtin = 1", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(n, 4);
    }

    #[test]
    fn messages_before_pages_backwards_and_returns_oldest_first() {
        let db = memory();
        for id in 1..=5 {
            db.insert_message(&row(id, "x")).expect("insert");
        }
        let page = db.messages_before(ConvKey::General, None, 2).expect("read");
        assert_eq!(
            page.iter().map(|m| m.msg_id).collect::<Vec<_>>(),
            vec![4, 5]
        );
        let older = db
            .messages_before(ConvKey::General, Some(4), 10)
            .expect("read");
        assert_eq!(
            older.iter().map(|m| m.msg_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn duplicate_message_ids_are_ignored_not_errors() {
        let db = memory();
        db.insert_message(&row(1, "first")).expect("insert");
        db.insert_message(&row(1, "again")).expect("dup insert");
        let page = db
            .messages_before(ConvKey::General, None, 10)
            .expect("read");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].body, "first");
    }

    #[test]
    fn prune_removes_only_rows_past_the_window() {
        let db = memory();
        let day = 24 * 60 * 60 * 1000;
        db.insert_message(&MessageRow {
            ts: 10 * day,
            ..row(1, "old")
        })
        .expect("insert");
        db.insert_message(&MessageRow {
            ts: 95 * day,
            ..row(2, "new")
        })
        .expect("insert");
        let removed = db
            .prune(
                100 * day,
                Retention {
                    history_days: 30,
                    telemetry_days: 0,
                    file_days: 0,
                },
            )
            .expect("prune");
        assert_eq!(removed, 1);
        let left = db
            .messages_before(ConvKey::General, None, 10)
            .expect("read");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].body, "new");
    }

    #[test]
    fn zero_retention_keeps_everything() {
        let db = memory();
        db.insert_message(&row(1, "keep")).expect("insert");
        let removed = db
            .prune(
                u64::MAX / 2,
                Retention {
                    history_days: 0,
                    telemetry_days: 0,
                    file_days: 0,
                },
            )
            .expect("prune");
        assert_eq!(removed, 0);
    }

    #[test]
    fn own_writes_do_not_count_as_external_changes() {
        let mut db = memory();
        db.insert_message(&row(1, "x")).expect("insert");
        assert!(!db.changed_externally().expect("poll"));
    }

    #[test]
    fn a_newer_schema_refuses_to_open() {
        let dir = std::env::temp_dir().join(format!(
            "baucord-store-newer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let path = dir.join("baucord.db");
        {
            let (db, _) = Sqlite::open(&path).expect("open");
            db.conn
                .execute("UPDATE schema_version SET version = 999", [])
                .expect("bump");
            db.close();
        }
        match Sqlite::open(&path) {
            Err(PersistError::Downgrade { found: 999, .. }) => {}
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("a newer schema must refuse to open"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn telemetry_batches_land_in_one_transaction() {
        let mut db = memory();
        let rows: Vec<TelemetryRow> = (0..3)
            .map(|i| TelemetryRow {
                ts: 1_000 + i,
                user_id: [1u8; 32],
                key_hash: [1u8; 32],
                rtt_ms: 40,
                path_loss: 0.5,
                voice_loss: Some(1.0),
                groups_behind: None,
                rung: None,
                input_device: Some("mic".to_string()),
                output_device: None,
                sample: vec![1, 2, 3],
            })
            .collect();
        db.insert_telemetry(&rows).expect("insert");
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM telemetry_samples", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 3);
    }

    #[test]
    fn policy_round_trips_through_the_tables() {
        let db = memory();
        let alice = [1u8; 32];
        let key = [7u8; 32];
        let mut user = UserRecord {
            name: "alice".into(),
            created_at: 10,
            last_seen: 10,
            invited_with: Some("K7FQ2MX9ABCD".into()),
            roles: BTreeSet::new(),
            profile: protocol::Profile::default(),
        };
        db.upsert_user(&alice, &user).expect("user");
        db.upsert_key(
            &key,
            &KeyRecord {
                user_id: alice,
                added_at: 10,
                last_seen: 10,
                retired_at: None,
            },
        )
        .expect("key");
        db.grant_role(&alice, protocol::permissions::ROLE_MEMBER, None, 10)
            .expect("role");
        db.grant_role(&alice, protocol::permissions::ROLE_MEMBER, None, 11)
            .expect("duplicate grant is ignored");
        db.insert_invite(
            "K7FQ2MX9ABCD",
            &InviteRecord {
                created_by: Some(alice),
                created_at: 10,
                expires_at: Some(99),
                max_uses: Some(3),
                uses: 0,
                grants_role: protocol::permissions::ROLE_GUEST,
                revoked_at: None,
            },
        )
        .expect("invite");
        db.use_invite("K7FQ2MX9ABCD").expect("use");
        let ban = Restriction {
            id: 5,
            user_id: Some(alice),
            key_hash: None,
            ip: Some("10.0.0.9".parse().unwrap()),
            kind: RestrictionKind::Server,
            until: None,
            reason: "spam".into(),
            by: None,
            created_at: 12,
        };
        db.insert_restriction(&ban).expect("restriction");
        user.name = "Alice".into();
        user.last_seen = 20;
        db.upsert_user(&alice, &user).expect("refresh");

        let policy = db.load_policy().expect("load");
        let loaded = &policy.users[&alice];
        assert_eq!(loaded.name, "Alice");
        assert_eq!(loaded.last_seen, 20);
        assert_eq!(loaded.created_at, 10, "created_at is never refreshed");
        assert!(loaded.roles.contains(&protocol::permissions::ROLE_MEMBER));
        assert_eq!(policy.keys[&key].user_id, alice);
        assert_eq!(policy.invites["K7FQ2MX9ABCD"].uses, 1);
        assert_eq!(policy.restrictions, vec![ban]);
        assert_eq!(policy.next_restriction_id, 6);
        assert_eq!(policy.roles.len(), 4);
    }

    #[test]
    fn check_reports_ok_on_a_fresh_database() {
        let dir = std::env::temp_dir().join(format!(
            "baucord-store-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let path = dir.join("baucord.db");
        Sqlite::open(&path).expect("open").0.close();
        let report = check(&path).expect("check");
        assert!(report.is_ok(), "{report:?}");
        assert_eq!(report.schema_version, Some(SCHEMA_VERSION));
        let _ = fs::remove_dir_all(&dir);
    }
}
