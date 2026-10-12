//! Block, report and delete-my-account (`protocol::safety`, 1.2;
//! feature bit `SAFETY`; PERSISTENCE-SPEC §12 S1).
//!
//! - **Blocks** live in `policy.blocks` (loaded with the policy, so a
//!   CLI reload keeps them) and on disk. The relay drops the blocked
//!   person's voice on its way to the blocker's sessions
//!   (`blocked_pairs`, recomputed with the rest of enforcement), and
//!   DMs between the two are refused both ways with
//!   `DmFailReason::Unavailable`. Hiding their messages is the
//!   client's: it gets the list as `Blocks` on join and on every
//!   change, on every device.
//! - **Reports** copy the reported text in as it was, are stored, and
//!   reach the moderators online at once as an `AuditLine` (with the
//!   excerpt; the audit row on disk keeps only the reason, so a DM's
//!   text does not outlive the report). A DM can only be reported by
//!   someone in it.
//! - **Delete account** is the admin purge, done by the owner, with
//!   one difference: the last admin is told to hand over first.

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use log::{info, warn};
use protocol::{
    features, permissions, AdminError, AdminOutcome, BlockedUser, ClientId, ConvRef, DeleteRefusal,
    DmFailReason, ReportReason, ReportRefusal, ReportTarget, ServerMessage, UserId,
    MAX_REPORT_EXCERPT_BYTES, MAX_REPORT_NOTE_BYTES,
};

use super::{hex, ChatServer, Waiting};
use crate::persistence::{
    Answer, AuditRow, Command, ConvKey, MessageRow, Outcome, Query, ReportRow, ReportedMessage,
};

/// Most people one account may block (bounds memory and the list).
const MAX_BLOCKS: usize = 1000;
/// Block or unblock changes one account may make per [`BLOCK_WINDOW`]:
/// each is a write, an enforcement pass and the list to every device.
const MAX_BLOCK_CHANGES_PER_WINDOW: usize = 30;
const BLOCK_WINDOW: Duration = Duration::from_mins(1);
/// Reports one account may file per [`REPORT_WINDOW`].
const MAX_REPORTS_PER_WINDOW: usize = 10;
const REPORT_WINDOW: Duration = Duration::from_mins(10);
/// How long a report waits for the store to find the message.
const REPORT_DEADLINE: Duration = Duration::from_secs(2);
/// How much of a reported message the live line to moderators shows.
const LIVE_EXCERPT_CHARS: usize = 160;

/// A report waiting for the store to find its message.
pub(super) struct PendingReport {
    pub client: ClientId,
    pub reporter: UserId,
    pub conv: ConvRef,
    pub reason: ReportReason,
    pub note: String,
}

impl ChatServer {
    // ── Blocks ──────────────────────────────────────────────────────

    /// `Block` / `Unblock` from a session.
    pub(super) fn handle_block(&mut self, client: ClientId, user: UserId, block: bool) {
        let Some(me) = self.user_of.get(&client).copied() else {
            return;
        };
        if user != me && self.block_change_allowed(&me) {
            let changed = if block {
                self.policy.users.contains_key(&user) && {
                    let set = self.policy.blocks.entry(me).or_default();
                    set.len() < MAX_BLOCKS && set.insert(user)
                }
            } else {
                let removed = self
                    .policy
                    .blocks
                    .get_mut(&me)
                    .is_some_and(|set| set.remove(&user));
                if self.policy.blocks.get(&me).is_some_and(HashSet::is_empty) {
                    self.policy.blocks.remove(&me);
                }
                removed
            };
            if changed {
                let ts = self.wall_now();
                self.persist.send(if block {
                    Command::InsertBlock {
                        blocker: me,
                        blocked: user,
                        ts,
                    }
                } else {
                    Command::DeleteBlock {
                        blocker: me,
                        blocked: user,
                    }
                });
                info!(
                    "{} ({client:#x}) {} {}",
                    self.get_username(client),
                    if block { "blocked" } else { "unblocked" },
                    hex(&user[..8])
                );
                self.refresh_enforcement();
            }
        }
        // The list goes back either way: the answer the client waits
        // for, and every other device of the account follows.
        self.send_blocks_to_user(&me);
    }

    /// Everyone `user` blocked, with names, for Settings.
    fn blocks_of(&self, user: &UserId) -> Vec<BlockedUser> {
        let mut list: Vec<BlockedUser> = self
            .policy
            .blocks
            .get(user)
            .into_iter()
            .flatten()
            .filter_map(|blocked| {
                self.policy.users.get(blocked).map(|u| BlockedUser {
                    user_id: *blocked,
                    name: u.name.clone(),
                })
            })
            .collect();
        list.sort_by_key(|b| b.name.to_lowercase());
        list
    }

    /// The block list to one session (on join).
    pub(super) fn send_blocks(&mut self, client: ClientId) {
        let Some(me) = self.user_of.get(&client).copied() else {
            return;
        };
        let msg = ServerMessage::Blocks {
            users: self.blocks_of(&me),
        };
        self.send_msg(client, &msg);
    }

    fn send_blocks_to_user(&mut self, user: &UserId) {
        let msg = ServerMessage::Blocks {
            users: self.blocks_of(user),
        };
        for session in self.sessions_of(user) {
            self.send_msg(session, &msg);
        }
    }

    /// Whether either of two users blocked the other: no DMs then.
    pub(super) fn blocked_either(&self, a: &UserId, b: &UserId) -> bool {
        let blocks =
            |x: &UserId, y: &UserId| self.policy.blocks.get(x).is_some_and(|s| s.contains(y));
        blocks(a, b) || blocks(b, a)
    }

    /// The sessions that get what `author` posts in General: everyone
    /// but the sessions of accounts that blocked them.
    pub(super) fn audience_of(&self, author: &UserId) -> Vec<ClientId> {
        let blocked_by = |viewer: &UserId| {
            self.policy
                .blocks
                .get(viewer)
                .is_some_and(|set| set.contains(author))
        };
        self.clients()
            .into_iter()
            .filter(|c| self.user_of.get(c).is_none_or(|u| !blocked_by(u)))
            .collect()
    }

    /// The relay's pairs: (sender, receiver) sessions where the
    /// receiver's user blocked the sender's. Recomputed with the rest
    /// of enforcement (joins, leaves, policy changes, blocks).
    pub(super) fn compute_blocked_pairs(&self) -> HashSet<(ClientId, ClientId)> {
        let mut pairs = HashSet::new();
        if self.policy.blocks.is_empty() {
            return pairs;
        }
        for (receiver, receiver_user) in &self.user_of {
            let Some(blocked) = self.policy.blocks.get(receiver_user) else {
                continue;
            };
            for (sender, sender_user) in &self.user_of {
                if blocked.contains(sender_user) {
                    pairs.insert((*sender, *receiver));
                }
            }
        }
        pairs
    }

    /// A DM refusal as this session can decode it: `Unavailable` is
    /// 1.2; an older client hears `NoPermission`.
    pub(super) fn dm_refusal_for(&self, client: ClientId, reason: DmFailReason) -> DmFailReason {
        if reason == DmFailReason::Unavailable && !self.has_feature(client, features::SAFETY) {
            DmFailReason::NoPermission
        } else {
            reason
        }
    }

    // ── Reports ─────────────────────────────────────────────────────

    /// `Report` from a session.
    pub(super) fn handle_report(
        &mut self,
        client: ClientId,
        target: ReportTarget,
        reason: ReportReason,
        note: &str,
    ) {
        let Some(me) = self.user_of.get(&client).copied() else {
            return;
        };
        if !self.report_allowed_by_rate(&me) {
            self.answer_report(client, Err(ReportRefusal::TooMany));
            return;
        }
        let note = protocol::truncate_on_char_boundary(note.trim(), MAX_REPORT_NOTE_BYTES);
        match target {
            ReportTarget::User { user } => {
                if user == me {
                    self.answer_report(client, Err(ReportRefusal::Yourself));
                    return;
                }
                let Some(name) = self.policy.users.get(&user).map(|u| u.name.clone()) else {
                    self.answer_report(client, Err(ReportRefusal::NoSuchUser));
                    return;
                };
                self.file_report(
                    client,
                    ReportRow {
                        ts: self.wall_now(),
                        reporter: me,
                        target_user: Some(user),
                        target_name: name,
                        msg_id: None,
                        in_dm: false,
                        excerpt: String::new(),
                        reason,
                        note,
                    },
                );
            }
            ReportTarget::Message { conv, msg_id } => {
                // The recent General messages are in memory; anything
                // else is a store read.
                if conv == ConvRef::General
                    && let Some(row) = self.recent.iter().find(|m| m.msg_id == msg_id).cloned()
                {
                    self.report_message(client, me, &row, false, reason, note);
                    return;
                }
                if !self.persist.is_persistent() {
                    self.answer_report(client, Err(ReportRefusal::NoSuchMessage));
                    return;
                }
                let ask = self.persist.ask(
                    Query::ReportedMessage { msg_id },
                    self.now + REPORT_DEADLINE,
                );
                self.waiting.insert(
                    ask,
                    Waiting::Report(PendingReport {
                        client,
                        reporter: me,
                        conv,
                        reason,
                        note,
                    }),
                );
            }
        }
    }

    /// The store found (or did not) the message a report is about.
    pub(super) fn resolve_report_answer(&mut self, pending: PendingReport, outcome: Outcome) {
        let found = match outcome {
            Outcome::Answered(Answer::ReportedMessage(Some(found))) => found,
            Outcome::Answered(Answer::ReportedMessage(None)) => {
                self.answer_report(pending.client, Err(ReportRefusal::NoSuchMessage));
                return;
            }
            other => {
                warn!("report lookup failed: {other:?}");
                self.answer_report(pending.client, Err(ReportRefusal::NoSuchMessage));
                return;
            }
        };
        let ReportedMessage { row, conv } = *found;
        // The message must be where the reporter says it is, and a DM
        // only reportable by someone in it: a guessed id must not turn
        // someone else's conversation into a report.
        let in_dm = match (pending.conv, conv) {
            (ConvRef::General, ConvKey::General) => false,
            (ConvRef::Dm(other), ConvKey::Dm(a, b))
                if (a == pending.reporter && b == other)
                    || (b == pending.reporter && a == other) =>
            {
                true
            }
            _ => {
                self.answer_report(pending.client, Err(ReportRefusal::NoSuchMessage));
                return;
            }
        };
        self.report_message(
            pending.client,
            pending.reporter,
            &row,
            in_dm,
            pending.reason,
            pending.note,
        );
    }

    fn report_message(
        &mut self,
        client: ClientId,
        reporter: UserId,
        row: &MessageRow,
        in_dm: bool,
        reason: ReportReason,
        note: String,
    ) {
        if row.user_id == reporter {
            self.answer_report(client, Err(ReportRefusal::Yourself));
            return;
        }
        // The store's answer can land after either account was deleted:
        // a report must not bring a purged user's text back.
        if !self.policy.users.contains_key(&reporter) {
            return;
        }
        if !self.policy.users.contains_key(&row.user_id) {
            self.answer_report(client, Err(ReportRefusal::NoSuchMessage));
            return;
        }
        let excerpt = if row.body.is_empty() {
            // A file: its name stands for the text.
            row.blob_id
                .and_then(|b| self.policy.blobs.get(&b))
                .map_or_else(|| "[file]".to_string(), |b| format!("[file] {}", b.name))
        } else {
            protocol::truncate_on_char_boundary(&row.body, MAX_REPORT_EXCERPT_BYTES)
        };
        let target_name = self
            .policy
            .users
            .get(&row.user_id)
            .map_or_else(|| row.name.clone(), |u| u.name.clone());
        self.file_report(
            client,
            ReportRow {
                ts: self.wall_now(),
                reporter,
                target_user: Some(row.user_id),
                target_name,
                msg_id: Some(row.msg_id),
                in_dm,
                excerpt,
                reason,
                note,
            },
        );
    }

    /// Store the report, tell the moderators online, answer the
    /// reporter.
    fn file_report(&mut self, client: ClientId, report: ReportRow) {
        let action = if report.msg_id.is_some() {
            "report.message"
        } else {
            "report.user"
        };
        info!(
            "{} ({client:#x}) reported {} ({}{})",
            self.get_username(client),
            report.target_name,
            report.reason.name(),
            if report.in_dm {
                ", a direct message"
            } else {
                ""
            }
        );
        let target = report.target_user.map(|u| hex(&u[..8]));
        // On disk the audit keeps the fact; the text lives in the
        // report, which is pruned once handled.
        self.persist.send(Command::Audit(AuditRow {
            ts: report.ts,
            actor: Some(report.reporter),
            action: action.to_string(),
            target,
            detail: Some(report.reason.name().to_string()),
        }));
        let mut detail = report.reason.name().to_string();
        if !report.excerpt.is_empty() {
            let short: String = report.excerpt.chars().take(LIVE_EXCERPT_CHARS).collect();
            let more = if report.excerpt.chars().count() > LIVE_EXCERPT_CHARS {
                "…"
            } else {
                ""
            };
            detail.push_str(": \"");
            detail.push_str(&short);
            detail.push_str(more);
            detail.push('"');
        }
        if !report.note.is_empty() {
            detail.push_str(" — ");
            detail.push_str(&report.note);
        }
        let line = ServerMessage::AuditLine {
            entry: protocol::AuditEntry {
                id: 0,
                ts: report.ts,
                actor: Some(self.name_of_user(&report.reporter)),
                action: action.to_string(),
                target: Some(report.target_name.clone()),
                detail: Some(detail),
            },
        };
        let readers: Vec<ClientId> = self
            .user_of
            .iter()
            .filter(|(_, u)| {
                let perms = self.effective_permissions(u);
                permissions::has(perms, permissions::ADMIN_CHANNEL)
                    && permissions::has(perms, permissions::RESTRICT)
            })
            .map(|(c, _)| *c)
            .collect();
        for peer in readers {
            self.send_msg(peer, &line);
        }
        self.persist.send(Command::InsertReport(Box::new(report)));
        self.answer_report(client, Ok(()));
    }

    fn answer_report(&mut self, client: ClientId, result: Result<(), ReportRefusal>) {
        self.send_msg(client, &ServerMessage::ReportAck { result });
    }

    /// At most [`MAX_BLOCK_CHANGES_PER_WINDOW`] per account per window;
    /// past it the list goes back unchanged.
    fn block_change_allowed(&mut self, user: &UserId) -> bool {
        let now = self.now;
        let times = self.block_times.entry(*user).or_default();
        while times
            .front()
            .is_some_and(|t: &Instant| now.duration_since(*t) > BLOCK_WINDOW)
        {
            times.pop_front();
        }
        if times.len() >= MAX_BLOCK_CHANGES_PER_WINDOW {
            return false;
        }
        times.push_back(now);
        true
    }

    /// At most [`MAX_REPORTS_PER_WINDOW`] per account per window.
    fn report_allowed_by_rate(&mut self, user: &UserId) -> bool {
        let now = self.now;
        let times = self.report_times.entry(*user).or_default();
        while times
            .front()
            .is_some_and(|t: &Instant| now.duration_since(*t) > REPORT_WINDOW)
        {
            times.pop_front();
        }
        if times.len() >= MAX_REPORTS_PER_WINDOW {
            return false;
        }
        times.push_back(now);
        true
    }

    // ── The Reports tab (admin panel) ───────────────────────────────

    /// `ListReports`: a store read, answered when it lands.
    pub(super) fn handle_list_reports(
        &mut self,
        client: ClientId,
        request_id: u32,
        resolved: bool,
    ) {
        if !self.has_permission(client, permissions::RESTRICT) {
            self.send_msg(
                client,
                &ServerMessage::AdminResult {
                    request_id,
                    result: Err(AdminError::NoPermission),
                },
            );
            return;
        }
        if !self.persist.is_persistent() {
            self.send_msg(
                client,
                &ServerMessage::AdminResult {
                    request_id,
                    result: Ok(AdminOutcome::Reports(Vec::new())),
                },
            );
            return;
        }
        let ask = self.persist.ask(
            Query::Reports {
                resolved,
                limit: 200,
            },
            self.now + REPORT_DEADLINE,
        );
        self.waiting
            .insert(ask, Waiting::ReportList { client, request_id });
    }

    pub(super) fn resolve_report_list_answer(
        &mut self,
        client: ClientId,
        request_id: u32,
        outcome: Outcome,
    ) {
        let result = match outcome {
            Outcome::Answered(Answer::Reports(list)) => Ok(AdminOutcome::Reports(list)),
            Outcome::Answered(Answer::Failed(e)) => Err(AdminError::Invalid(e)),
            _ => Err(AdminError::Invalid(
                "the reports did not answer in time".into(),
            )),
        };
        // Newest first; a report can carry a 2,000-byte excerpt and a
        // 500-byte note, so 200 of them are pages, not a message.
        self.send_admin_result(client, request_id, result);
    }

    /// `ResolveReport`: handled, out of the open list.
    pub(super) fn handle_resolve_report(
        &mut self,
        client: ClientId,
        id: u64,
    ) -> Result<AdminOutcome, AdminError> {
        if !self.has_permission(client, permissions::RESTRICT) {
            return Err(AdminError::NoPermission);
        }
        let actor = self.user_of.get(&client).copied();
        let ts = self.wall_now();
        self.persist
            .send(Command::ResolveReport { id, by: actor, ts });
        self.audit(actor, "report.resolve", Some(id.to_string()), None);
        Ok(AdminOutcome::Done)
    }

    // ── Delete my account ───────────────────────────────────────────

    /// `DeleteAccount` from a session: the account and everything it
    /// posted, now.
    pub(super) fn handle_delete_account(&mut self, client: ClientId) {
        let Some(me) = self.user_of.get(&client).copied() else {
            return;
        };
        let is_admin = permissions::has(self.policy.permissions_of(&me), permissions::ADMIN);
        let other_admins = self.policy.users.keys().any(|u| {
            *u != me && permissions::has(self.policy.permissions_of(u), permissions::ADMIN)
        });
        if is_admin && !other_admins {
            self.send_msg(
                client,
                &ServerMessage::AccountDeleteRefused {
                    reason: DeleteRefusal::LastAdmin,
                },
            );
            return;
        }
        info!(
            "{} ({client:#x}) deleted their account ({})",
            self.get_username(client),
            hex(&me[..8])
        );
        self.send_msg(client, &ServerMessage::AccountDeleted);
        // No actor: the record keeps that an account went at its
        // owner's request, not who they were.
        self.erase_user(None, &me, "account deleted", "user.delete");
    }

    /// The client logs a user uploaded (`<identity.dir>/logs`) go with
    /// their account — on a thread of their own, not on the tick:
    /// listing a directory of every log ever uploaded and deleting
    /// from it waits on the disk, and voice waits behind the tick
    /// (review P3-13). Not the persistence thread: without a database
    /// there is none, and the logs are there all the same.
    pub(super) fn delete_uploaded_logs(&self, user: &UserId, name: &str) {
        let dir = PathBuf::from(&self.config.identity.dir).join("logs");
        let (user, name) = (*user, name.to_string());
        let spawned = std::thread::Builder::new()
            .name("log-cleanup".to_string())
            .spawn(move || remove_logs_of(&dir, &user, &name));
        if let Err(e) = spawned {
            warn!("uploaded logs not removed: no thread to do it on ({e})");
        }
    }
}

/// Delete the uploaded logs of `user` in `dir`. Names are `<name>-<user
/// id>-<session>-<time>.log` since 1.2, `<name>-<session>-<time>.log`
/// before: those go by the name.
fn remove_logs_of(dir: &std::path::Path, user: &UserId, name: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let id = hex(&user[..8]);
    let safe = log_safe_name(name);
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(stem) = file_name
            .to_str()
            .and_then(|n| n.strip_suffix(".log"))
            .map(str::to_string)
        else {
            continue;
        };
        let parts: Vec<&str> = stem.split('-').collect();
        // Before 1.2 the name was all a log said of its author: a
        // name with no letter or digit in it says nothing at all.
        let theirs = match parts.as_slice() {
            [_, user_part, _, _] => *user_part == id,
            [name_part, _, _] => {
                *name_part == safe && safe.chars().any(|c| c.is_ascii_alphanumeric())
            }
            _ => false,
        };
        if theirs {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => warn!("uploaded log {} not removed: {e}", entry.path().display()),
            }
        }
    }
    if removed > 0 {
        info!("removed {removed} uploaded log(s) of {id}");
    }
}

/// A name as it appears in an uploaded log's file name: letters and
/// digits, everything else `_` (so a name never contains the `-` that
/// separates the parts).
pub(super) fn log_safe_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_accounts_logs_go_and_nobody_elses() {
        let dir = std::env::temp_dir().join(format!(
            "baucord-logs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let user = [0xabu8; 32];
        let id = hex(&user[..8]);
        let other = hex(&[0xcdu8; 8]);
        let theirs = [
            format!("ana-{id}-12-1700000000.log"),
            // Renamed since: the id still says whose it is.
            format!("someone_else-{id}-13-1700000001.log"),
            // Before 1.2 the name was all there was.
            "ana-9-1600000000.log".to_string(),
        ];
        let kept = [
            format!("ana-{other}-12-1700000000.log"),
            "bob-9-1600000000.log".to_string(),
            "notes.txt".to_string(),
        ];
        for name in theirs.iter().chain(&kept) {
            std::fs::write(dir.join(name), b"log").expect("write");
        }
        remove_logs_of(&dir, &user, "ana");
        for name in &theirs {
            assert!(!dir.join(name).exists(), "{name} should be gone");
        }
        for name in &kept {
            assert!(dir.join(name).exists(), "{name} should be kept");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_reports_list_splits_into_pages_that_each_encode() {
        let long = |n: usize| "x".repeat(n);
        let list: Vec<protocol::ReportSummary> = (0..200)
            .map(|i| protocol::ReportSummary {
                id: i,
                ts: i,
                reporter: Some(long(32)),
                target_user: Some([1u8; 32]),
                target_name: long(32),
                msg_id: Some(i),
                in_dm: false,
                excerpt: long(protocol::MAX_REPORT_EXCERPT_BYTES),
                reason: protocol::ReportReason::Spam,
                note: long(protocol::MAX_REPORT_NOTE_BYTES),
                resolved: false,
            })
            .collect();
        // Worst case, ~2.6 KB a report: a 17 KiB message (what every
        // shipped client decodes) holds six, and the first page is what
        // such a client gets.
        let pages = AdminOutcome::Reports(list).into_pages(protocol::LIST_ITEMS_BUDGET);
        let mut next = 0u64;
        for (i, page) in pages.iter().enumerate() {
            let AdminOutcome::Reports(rows) = page else {
                panic!("page {i} is not a reports page");
            };
            if i + 1 < pages.len() {
                assert!(rows.len() >= 6, "page {i} holds {}", rows.len());
            }
            for row in rows {
                assert_eq!(row.id, next, "newest first, kept in order");
                next += 1;
            }
            let msg = if i + 1 == pages.len() {
                ServerMessage::AdminResult {
                    request_id: 1,
                    result: Ok(page.clone()),
                }
            } else {
                ServerMessage::AdminRows {
                    request_id: 1,
                    rows: page.clone(),
                }
            };
            assert!(protocol::encode_server_message(&msg).is_some(), "page {i}");
        }
        assert_eq!(next, 200, "every report is on a page");
    }
}
