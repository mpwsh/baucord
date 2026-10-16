//! History, edits, the chat rate limit and progress on the tick (spec
//! §6, §9.1).
//!
//! The newest general-chat messages live in a ring in memory: the
//! replay on join comes from it (no disk read on the admission path),
//! and it decides whether an author may still edit or delete. Older
//! pages are a read on the persistence thread with a deadline; a
//! stalled disk answers `HistoryUnavailable` with a retry hint, never a
//! wait. Progress accrues once a minute from what the tick already
//! knows — presence, who spoke, who shares to whom — never per packet.

use std::time::Duration;

use log::info;
use protocol::{
    features, permissions, ConvRef, HistoryMessage, RestrictionKind, ServerMessage, UserId,
};

use super::{hex, ChatServer, Waiting};
use crate::persistence::{
    Answer, Command, ConvKey, MessageRow, Outcome, ProgressDelta, Query, RECENT_RING,
};

/// How long a history read may take before the client is told to retry.
const HISTORY_DEADLINE: Duration = Duration::from_secs(2);
const HISTORY_RETRY_SECS: u16 = 5;
/// The largest page a client may ask for.
const HISTORY_MAX_LIMIT: u16 = 200;

/// xp per minute (spec §9.1, as decided: presence, voice and sharing
/// earn it; typing does not, so there is nothing to spam for).
const XP_ONLINE: u64 = 1;
/// Per chat message, counted at the minute flush (spec §9.1).
const XP_MESSAGE: u64 = 1;
const XP_TALKING: u64 = 5;
const XP_WATCHING: u64 = 2;
const XP_SHARING: u64 = 10;

impl ChatServer {
    // ── Chat rate limit (`limits.chat`) ─────────────────────────────

    /// Token bucket per session: `burst` messages, refilled at
    /// `burst / per_secs` per second. A refused message is dropped and
    /// the session told once per empty bucket.
    pub(super) fn chat_allowed_by_rate(&mut self, client: protocol::ClientId) -> bool {
        let limit = self.config.limits.chat;
        let burst = f64::from(limit.burst.max(1));
        let per_secs = limit.per_secs.max(1);
        let now = self.now;
        let (tokens, last) = self.chat_tokens.entry(client).or_insert((burst, now));
        let refill = now.duration_since(*last).as_secs_f64() * burst / per_secs as f64;
        *tokens = (*tokens + refill).min(burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            return true;
        }
        // Milliseconds until one token is back.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let retry_ms = ((1.0 - *tokens) * per_secs as f64 / burst * 1000.0).ceil() as u32;
        info!(
            "{} ({client:#x}) chat rate limited; retry in {retry_ms} ms",
            self.get_username(client)
        );
        self.send_msg(client, &ServerMessage::ChatRateLimited { retry_ms });
        false
    }

    // ── The ring and replay ─────────────────────────────────────────

    pub(super) fn remember_message(&mut self, row: &MessageRow) {
        if self.recent.len() >= usize::from(RECENT_RING) {
            self.recent.pop_front();
        }
        self.recent.push_back(row.clone());
    }

    /// The last `history.replay` messages, right after `Init`.
    pub(super) fn replay_history(&mut self, client: protocol::ClientId) {
        if !self.has_feature(client, features::HISTORY) {
            return;
        }
        let replay = usize::from(self.config.persistence.history.replay);
        let start = self.recent.len().saturating_sub(replay);
        let rows: Vec<MessageRow> = self.recent.iter().skip(start).cloned().collect();
        // "More" only when the store can answer; the ring is all there
        // is otherwise.
        let more = self.persist.is_persistent() && !rows.is_empty();
        self.send_rows_as_history(client, ConvRef::General, &rows, more);
    }

    /// Rows → a page for `client`, wherever they came from (the ring
    /// or the store): a file row is named in its text for a session
    /// without `FILES`, and its card follows the page with it.
    fn send_rows_as_history(
        &mut self,
        client: protocol::ClientId,
        conv: ConvRef,
        rows: &[MessageRow],
        more: bool,
    ) {
        let rich = self.has_feature(client, features::FILES);
        let mut page: Vec<HistoryMessage> = rows
            .iter()
            .map(|r| {
                let mut m = to_wire(r);
                if !rich && let Some(text) = self.plain_history_text(r) {
                    m.text = text;
                }
                m
            })
            .collect();
        // One `History` per page, cut to fit one message: every shipped
        // client takes exactly one per request (it prepends a page and
        // appends the replay), so a page split in two came out in the
        // wrong order, and anything over 17 KiB was dropped unread
        // (review P0-1). The newest rows stay; the rest come back with
        // the next "Load earlier".
        let newest = page.last().cloned();
        let mut dropped = protocol::keep_suffix_that_fits(&mut page, protocol::LIST_ITEMS_BUDGET);
        // Not even the newest row fits (stored before the cap on
        // `max_chat_bytes`): it goes cut short rather than not at all.
        // An empty page that said "more" was asked for again and again,
        // the cursor never moving (review 2026-10, P2-26).
        if page.is_empty()
            && let Some(mut m) = newest
        {
            m.text =
                protocol::truncate_on_char_boundary(&m.text, crate::config::MAX_CHAT_BYTES_LIMIT);
            page.push(m);
            dropped = rows.len() - 1;
        }
        let rows = &rows[dropped..];
        self.send_authors(client, rows.iter().map(|r| r.user_id));
        self.send_msg(
            client,
            &ServerMessage::History {
                conv,
                messages: page,
                more: more || dropped > 0,
            },
        );
        if rich {
            for attachments in self.history_attachments(conv, rows) {
                self.send_msg(client, &attachments);
            }
        }
    }

    /// `FetchHistory`: from the ring when it covers the range, else a
    /// read with a deadline.
    pub(super) fn handle_fetch_history(
        &mut self,
        client: protocol::ClientId,
        conv: ConvRef,
        before: Option<u64>,
        limit: u16,
    ) {
        if !self.has_feature(client, features::HISTORY) {
            return;
        }
        let limit = limit.clamp(1, HISTORY_MAX_LIMIT);
        if let ConvRef::Dm(other) = conv {
            // DMs live only in the store (no ring); without one there
            // is nothing to page.
            let key = self.dm_conv_key(client, other);
            let (Some(key), true) = (key, self.persist.is_persistent()) else {
                self.send_msg(
                    client,
                    &ServerMessage::History {
                        conv,
                        messages: Vec::new(),
                        more: false,
                    },
                );
                return;
            };
            let ask = self.persist.ask(
                Query::MessagesBefore {
                    conv: key,
                    before,
                    limit,
                },
                self.now + HISTORY_DEADLINE,
            );
            self.waiting.insert(
                ask,
                Waiting::History {
                    client,
                    conv,
                    limit,
                },
            );
            return;
        }
        let oldest_in_ring = self.recent.front().map(|m| m.msg_id);
        let ring_covers = match (before, oldest_in_ring) {
            (Some(before), Some(oldest)) => before > oldest,
            (None, Some(_)) => true,
            _ => false,
        };
        if ring_covers {
            let rows: Vec<MessageRow> = self
                .recent
                .iter()
                .rev()
                .filter(|m| before.is_none_or(|b| m.msg_id < b))
                .take(usize::from(limit))
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let more = self.persist.is_persistent() && !rows.is_empty();
            self.send_rows_as_history(client, conv, &rows, more);
            return;
        }
        if !self.persist.is_persistent() {
            self.send_msg(
                client,
                &ServerMessage::History {
                    conv,
                    messages: Vec::new(),
                    more: false,
                },
            );
            return;
        }
        let ask = self.persist.ask(
            Query::MessagesBefore {
                conv: ConvKey::General,
                before,
                limit,
            },
            self.now + HISTORY_DEADLINE,
        );
        self.waiting.insert(
            ask,
            Waiting::History {
                client,
                conv,
                limit,
            },
        );
    }

    /// One history read the persistence thread answered (or did not,
    /// in time).
    pub(super) fn resolve_history_answer(
        &mut self,
        client: protocol::ClientId,
        conv: protocol::ConvRef,
        limit: u16,
        outcome: Outcome,
    ) {
        match outcome {
            Outcome::Answered(Answer::Messages(rows)) => {
                let more = rows.len() >= usize::from(limit);
                self.send_rows_as_history(client, conv, &rows, more);
            }
            Outcome::Answered(Answer::Unavailable) => {
                self.send_msg(
                    client,
                    &ServerMessage::History {
                        conv,
                        messages: Vec::new(),
                        more: false,
                    },
                );
            }
            // Routed by `poll_persistence`; these never land on a
            // history ask.
            Outcome::Answered(
                Answer::Blob(_)
                | Answer::Stored
                | Answer::Conversations(_)
                | Answer::Audit(_)
                | Answer::ReportedMessage(_)
                | Answer::Reports(_)
                | Answer::Count(_),
            ) => {
                log::warn!("unexpected answer on a history ask; ignored");
            }
            Outcome::Answered(Answer::Failed(e)) => {
                log::warn!("history read failed: {e}");
                self.send_msg(
                    client,
                    &ServerMessage::HistoryUnavailable {
                        conv,
                        retry_secs: HISTORY_RETRY_SECS,
                    },
                );
            }
            Outcome::TimedOut => {
                self.send_msg(
                    client,
                    &ServerMessage::HistoryUnavailable {
                        conv,
                        retry_secs: HISTORY_RETRY_SECS,
                    },
                );
            }
        }
    }

    // ── Edit and delete ─────────────────────────────────────────────

    /// Your own recent message, retexted. Beyond the ring the server no
    /// longer knows the author, so nothing older is editable.
    pub(super) fn handle_edit_message(
        &mut self,
        client: protocol::ClientId,
        msg_id: u64,
        text: String,
    ) {
        if !self.allowed(
            client,
            permissions::TEXT_GENERAL,
            RestrictionKind::GeneralText,
        ) {
            return;
        }
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        let text = protocol::truncate_on_char_boundary(&text, self.config.limits.max_chat_bytes);
        if text.trim().is_empty() {
            return;
        }
        let Some(row) = self.recent.iter_mut().find(|m| m.msg_id == msg_id) else {
            return;
        };
        if row.user_id != user {
            return;
        }
        row.body.clone_from(&text);
        row.edited = true;
        let ts = self.wall_now();
        self.persist.send(Command::EditMessage {
            msg_id,
            body: text.clone(),
            ts,
        });
        info!(
            "{} ({client:#x}) edited message {msg_id}",
            self.get_username(client)
        );
        // Not to whoever blocked the author: they never saw the message,
        // and an edit of one off screen is shown with its text.
        let edited = ServerMessage::MessageEdited { msg_id, text };
        let recipients = self.audience_of(&user);
        self.send_to(&recipients, &edited);
    }

    /// Your own recent message removed; `by_admin` skips the ownership
    /// check (the caller checked `RESTRICT`).
    pub(super) fn handle_delete_message(
        &mut self,
        client: protocol::ClientId,
        msg_id: u64,
        by_admin: bool,
    ) -> bool {
        let Some(user) = self.user_of.get(&client).copied() else {
            return false;
        };
        let Some(pos) = self.recent.iter().position(|m| m.msg_id == msg_id) else {
            return false;
        };
        if !by_admin && self.recent[pos].user_id != user {
            return false;
        }
        if let Some(blob_id) = self.recent[pos].blob_id {
            self.blob_ref(&blob_id, -1);
        }
        self.recent.remove(pos);
        let ts = self.wall_now();
        self.persist.send(Command::DeleteMessage { msg_id, ts });
        info!(
            "{} ({client:#x}) deleted message {msg_id}{}",
            self.get_username(client),
            if by_admin { " (moderation)" } else { "" }
        );
        self.broadcast_msg(&ServerMessage::MessageDeleted { msg_id });
        true
    }

    // ── Progress (spec §9.1) ────────────────────────────────────────

    /// Once a minute: what everyone did this minute, as xp and seconds.
    pub(super) fn flush_progress(&mut self) {
        let ts = self.wall_now();
        let clients = self.clients();
        for client in &clients {
            let Some(user) = self.user_of.get(client).copied() else {
                continue;
            };
            let channel = self.channel_of.get(client).copied();
            // Nothing accrues outside a room: the lobby is nowhere
            // and AFK is away (spec §12, 1.1.5).
            if channel.is_none_or(|c| c == self.default_channel || Some(c) == self.afk_channel()) {
                continue;
            }
            let others_in_channel = channel.is_some_and(|c| {
                self.channel_of
                    .iter()
                    .any(|(other, ch)| other != client && *ch == c)
            });
            let talked = self.spoke.contains(client) && others_in_channel;
            let watching = self
                .video
                .sharers()
                .keys()
                .any(|sharer| *sharer != *client && self.video.is_subscribed(*client, *sharer));
            let sharing_to_someone =
                self.video.is_sharing(*client) && self.video.viewer_count(*client) > 0;
            let p = self.progress.entry(user).or_default();
            p.xp += XP_ONLINE;
            p.online_seconds += 60;
            if talked {
                p.xp += XP_TALKING;
                p.talk_seconds += 60;
            }
            if watching {
                p.xp += XP_WATCHING;
                p.watch_seconds += 60;
            }
            if sharing_to_someone {
                p.xp += XP_SHARING;
                p.share_seconds += 60;
            }
        }
        self.spoke.clear();

        let deltas: Vec<(UserId, ProgressDelta)> = self.progress.drain().collect();
        for (user, mut delta) in deltas {
            delta.user_id = user;
            delta.ts = ts;
            // Chat earns a little (the counter is kept anyway).
            delta.xp += XP_MESSAGE * delta.messages;
            let before = self.policy.level_of(&user);
            *self.policy.xp.entry(user).or_insert(0) += delta.xp;
            let after = self.policy.level_of(&user);
            self.persist.send(Command::AddProgress(delta));
            // Everyone's bar moves once a minute: each of this user's
            // sessions, to every peer that renders it — which for a
            // session in the lobby is itself alone (review P2-1: this
            // told every client it was there, by its id).
            for session in self.sessions_of(&user) {
                let msg = self.user_progress(session, &user);
                let recipients = self.audience_about(session);
                self.send_to(&recipients, &msg);
            }
            if after > before {
                let xp = self.policy.xp.get(&user).copied().unwrap_or(0);
                info!("{} reached level {after}", hex(&user[..8]));
                for session in self.sessions_of(&user) {
                    self.send_msg(session, &ServerMessage::ProgressUpdate { xp, level: after });
                    let details = self.user_details(session, &user);
                    let recipients = self.audience_about(session);
                    self.send_to(&recipients, &details);
                }
            }
        }
    }

    /// Something a session did that counts against idling (spec §5).
    pub(super) fn note_activity(&mut self, client: protocol::ClientId) {
        self.last_activity.insert(client, self.now);
    }

    /// Whether the session negotiated `bit`.
    pub(super) fn has_feature(&self, client: protocol::ClientId, bit: u64) -> bool {
        self.features_of.get(&client).is_some_and(|f| f & bit != 0)
    }

    /// Delete a person (spec §8): an admin's purge.
    pub(super) fn purge_user(&mut self, actor: UserId, user_id: &UserId) {
        self.erase_user(Some(actor), user_id, "account removed", "user.purge");
    }

    /// Delete a person, by an admin or by themselves
    /// (`DeleteAccount`): sessions closed with `reason`, every trace in
    /// memory gone, their recent messages taken off everyone's screen,
    /// the rows queued for deletion (`Store::purge_user`), their
    /// uploaded logs removed, one audit line (`action`) kept.
    /// A purge took references off files other people uploaded
    /// (`Notice::RefsReleased`): the counts follow, and a file nothing
    /// points at any more is deleted now rather than at the next prune
    /// (it belonged to a conversation that no longer exists).
    pub(super) fn release_blob_refs(&mut self, released: &[(protocol::BlobId, u32)]) {
        for (blob, count) in released {
            self.blob_ref(blob, -i32::try_from(*count).unwrap_or(i32::MAX));
            if self.policy.blobs.get(blob).is_some_and(|b| b.refs == 0) {
                self.forget_blob(blob);
                self.persist.send(Command::DeleteBlob { blob_id: *blob });
            }
        }
    }

    /// A retention pass ran (`Notice::Pruned`). Messages older than
    /// `history_cutoff` left the store: they leave the replay ring too
    /// (a quiet server's ring can reach back past the window, and
    /// would replay what retention promised was gone), and the file
    /// references they held come off the counts — the store has taken
    /// them off its own (review P2-11). Then the files it deleted.
    pub(super) fn apply_prune(
        &mut self,
        history_cutoff: Option<crate::clock::WallMs>,
        released: &[(protocol::BlobId, u32)],
        files: &[protocol::BlobId],
    ) {
        if let Some(cutoff) = history_cutoff {
            // No count is touched here: `released` covers these rows.
            self.recent.retain(|m| m.ts >= cutoff);
        }
        for (blob, count) in released {
            self.blob_ref(blob, -i32::try_from(*count).unwrap_or(i32::MAX));
        }
        self.forget_pruned_files(files);
    }

    /// The retention prune deleted these files. An unreferenced file's
    /// metadata stays here on purpose — a re-announce of the same bytes
    /// is answered "already here" — so it must go with the file, or the
    /// next post of it points at nothing (review P0-4). One referenced
    /// again in the moment between the prune's read and this notice has
    /// lost its file; say so.
    fn forget_pruned_files(&mut self, files: &[protocol::BlobId]) {
        for blob in files {
            let Some(meta) = self.forget_blob(blob) else {
                continue;
            };
            if meta.refs > 0 {
                log::warn!(
                    "file {} was pruned while {} post(s) took it up again; it must be re-sent",
                    hex(&blob[..8]),
                    meta.refs
                );
            }
        }
    }

    pub(super) fn erase_user(
        &mut self,
        actor: Option<UserId>,
        user_id: &UserId,
        reason: &str,
        action: &str,
    ) {
        // The line first, while the actor's name still resolves.
        self.audit(actor, action, Some(hex(&user_id[..8])), None);
        let name = self
            .policy
            .users
            .get(user_id)
            .map(|u| u.name.clone())
            .unwrap_or_default();
        let avatar = self
            .policy
            .users
            .get(user_id)
            .and_then(|u| u.profile.avatar);
        for session in self.sessions_of(user_id) {
            self.remove_session(session, protocol::RemovedKind::Kicked, reason, "server");
        }
        self.policy.users.remove(user_id);
        self.policy.keys.retain(|_, k| k.user_id != *user_id);
        self.policy
            .restrictions
            .retain(|r| r.user_id.as_ref() != Some(user_id));
        self.policy.xp.remove(user_id);
        // Their files (spec §8). Files are shared by content: someone
        // else's post or avatar may be the same one. What nothing points
        // at goes now; the rest stays, without an uploader, and loses
        // the references the store's purge removes (their posts, their
        // DM conversations, their avatar: `Notice::RefsReleased`), which
        // deletes whatever that leaves unused. Without a store there are
        // no stored posts, and the avatar is released here.
        if !self.persist.is_persistent()
            && let Some(blob) = avatar
        {
            self.blob_ref(&blob, -1);
        }
        let theirs: Vec<protocol::BlobId> = self
            .policy
            .blobs
            .iter()
            .filter(|(_, b)| b.created_by == Some(*user_id))
            .map(|(id, _)| *id)
            .collect();
        for blob in theirs {
            match self.policy.blobs.get_mut(&blob) {
                Some(meta) if meta.refs > 0 => meta.created_by = None,
                _ => {
                    self.forget_blob(&blob);
                    self.persist.send(Command::DeleteBlob { blob_id: blob });
                }
            }
        }
        // Their recent messages leave everyone's screen now, not at
        // the next reconnect.
        let gone: Vec<u64> = self
            .recent
            .iter()
            .filter(|m| m.user_id == *user_id)
            .map(|m| m.msg_id)
            .collect();
        self.recent.retain(|m| m.user_id != *user_id);
        for msg_id in gone {
            self.broadcast_msg(&ServerMessage::MessageDeleted { msg_id });
        }
        self.last_telemetry.remove(user_id);
        self.pending_telemetry.remove(user_id);
        self.progress.remove(user_id);
        self.profile_changed_at.remove(user_id);
        self.uploads_today.remove(user_id);
        self.report_times.remove(user_id);
        self.proven_uploads.retain(|(user, _)| user != user_id);
        self.block_times.remove(user_id);
        self.policy
            .invites
            .retain(|_, i| i.created_by != Some(*user_id));
        // Their blocks go, and blocks on them: whoever comes back with
        // the same key is a new account.
        self.policy.blocks.remove(user_id);
        for set in self.policy.blocks.values_mut() {
            set.remove(user_id);
        }
        self.policy.blocks.retain(|_, set| !set.is_empty());
        self.persist.send(Command::PurgeUser { user_id: *user_id });
        self.delete_uploaded_logs(user_id, &name);
        self.refresh_enforcement();
    }
}

fn to_wire(m: &MessageRow) -> HistoryMessage {
    HistoryMessage {
        id: m.msg_id,
        ts: m.ts,
        user_id: m.user_id,
        name: m.name.clone(),
        text: m.body.clone(),
        edited: m.edited,
    }
}
