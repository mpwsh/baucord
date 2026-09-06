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

use super::{hex, ChatServer};
use crate::persistence::{
    Answer, AskId, Command, ConvKey, MessageRow, Outcome, ProgressDelta, Query, RECENT_RING,
};

/// A history page stays under this many bytes of text (spec §10:
/// pages ≤ 64 KiB).
const HISTORY_PAGE_BYTES: usize = 60 * 1024;
/// How long a history read may take before the client is told to retry.
const HISTORY_DEADLINE: Duration = Duration::from_secs(2);
const HISTORY_RETRY_SECS: u16 = 5;
/// The largest page a client may ask for.
const HISTORY_MAX_LIMIT: u16 = 200;

/// xp per minute (spec §9.1, as decided: presence, voice and sharing
/// earn it; typing does not, so there is nothing to spam for).
const XP_ONLINE: u64 = 1;
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
        self.send_if(
            client,
            features::HISTORY,
            &ServerMessage::ChatRateLimited { retry_ms },
        );
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
        let page: Vec<HistoryMessage> = self.recent.iter().skip(start).map(to_wire).collect();
        // "More" only when the store can answer; the ring is all there
        // is otherwise.
        let more = self.persist.is_persistent() && !page.is_empty();
        self.send_history_pages(client, ConvRef::General, page, more);
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
            self.pending_history.insert(ask, (client, conv, limit));
            return;
        }
        let oldest_in_ring = self.recent.front().map(|m| m.msg_id);
        let ring_covers = match (before, oldest_in_ring) {
            (Some(before), Some(oldest)) => before > oldest,
            (None, Some(_)) => true,
            _ => false,
        };
        if ring_covers {
            let page: Vec<HistoryMessage> = self
                .recent
                .iter()
                .rev()
                .filter(|m| before.is_none_or(|b| m.msg_id < b))
                .take(usize::from(limit))
                .map(to_wire)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let more = self.persist.is_persistent() && !page.is_empty();
            self.send_history_pages(client, conv, page, more);
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
        self.pending_history.insert(ask, (client, conv, limit));
    }

    /// One history read the persistence thread answered (or did not,
    /// in time).
    pub(super) fn resolve_history_answer(&mut self, ask: AskId, outcome: Outcome) {
        let Some((client, conv, limit)) = self.pending_history.remove(&ask) else {
            return;
        };
        match outcome {
            Outcome::Answered(Answer::Messages(rows)) => {
                let page: Vec<HistoryMessage> = rows.iter().map(to_wire).collect();
                let more = rows.len() >= usize::from(limit);
                self.send_history_pages(client, conv, page, more);
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
            Outcome::Answered(Answer::Blob(_) | Answer::Stored | Answer::Conversations(_)) => {
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

    /// Split a page so no single message exceeds the size budget;
    /// every batch but the last says `more: true`.
    fn send_history_pages(
        &mut self,
        client: protocol::ClientId,
        conv: ConvRef,
        page: Vec<HistoryMessage>,
        more: bool,
    ) {
        let mut batches: Vec<Vec<HistoryMessage>> = vec![Vec::new()];
        let mut bytes = 0usize;
        for m in page {
            let size = m.text.len() + m.name.len() + 64;
            if bytes + size > HISTORY_PAGE_BYTES && !batches.last().is_some_and(Vec::is_empty) {
                batches.push(Vec::new());
                bytes = 0;
            }
            bytes += size;
            batches.last_mut().expect("one batch").push(m);
        }
        let last = batches.len() - 1;
        for (i, messages) in batches.into_iter().enumerate() {
            self.send_msg(
                client,
                &ServerMessage::History {
                    conv,
                    messages,
                    more: more || i < last,
                },
            );
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
        self.broadcast_msg(&ServerMessage::MessageEdited { msg_id, text });
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
            let before = self.policy.level_of(&user);
            *self.policy.xp.entry(user).or_insert(0) += delta.xp;
            let after = self.policy.level_of(&user);
            self.persist.send(Command::AddProgress(delta));
            if after > before {
                let xp = self.policy.xp.get(&user).copied().unwrap_or(0);
                info!("{} reached level {after}", hex(&user[..8]));
                for session in self.sessions_of(&user) {
                    self.send_if(
                        session,
                        features::PROGRESS,
                        &ServerMessage::ProgressUpdate { xp, level: after },
                    );
                    let details = self.user_details(session, &user);
                    for peer in self.clients() {
                        self.send_if_roles(peer, &details);
                    }
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

    /// Send only when the session negotiated `bit`.
    pub(super) fn send_if(&mut self, client: protocol::ClientId, bit: u64, msg: &ServerMessage) {
        if self.has_feature(client, bit) {
            self.send_msg(client, msg);
        }
    }

    /// Delete a person (spec §8): sessions closed, every trace in
    /// memory gone, the rows queued for deletion, one audit line kept.
    pub(super) fn purge_user(&mut self, actor: UserId, user_id: &UserId) {
        for session in self.sessions_of(user_id) {
            self.remove_session(
                session,
                protocol::RemovedKind::Kicked,
                "account removed",
                "server",
            );
        }
        self.policy.users.remove(user_id);
        self.policy.keys.retain(|_, k| k.user_id != *user_id);
        self.policy
            .restrictions
            .retain(|r| r.user_id.as_ref() != Some(user_id));
        self.policy.xp.remove(user_id);
        self.recent.retain(|m| m.user_id != *user_id);
        self.last_telemetry.remove(user_id);
        self.persist.send(Command::PurgeUser { user_id: *user_id });
        self.audit(Some(actor), "user.purge", Some(hex(&user_id[..8])), None);
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
