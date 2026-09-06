//! Direct messages (spec §6).
//!
//! A DM is stored before anyone sees it when there is a store: the
//! recipient may be offline, so the sender's `MessageAck` waits for the
//! insert's reply (deadline [`DM_STORE_DEADLINE`]), and an online
//! recipient gets `Direct` in the same tick the reply lands. Without a
//! store a DM is live only: delivered if the recipient is online,
//! `MessageFailed { Offline }` otherwise. Both ends need `DM` — a guest
//! on either side is refused — and a `dm` restriction on the sender
//! refuses with `Restricted` plus the failure the client waits for.
//!
//! Unread counts come from `message_reads`: `MarkRead` moves the mark,
//! `Conversations` (after the general replay on join) reports what is
//! past it per conversation.

use std::time::Duration;

use log::{info, warn};
use protocol::{
    features, permissions, ChatMessage, ClientId, ConvRef, DmFailReason, RestrictionKind,
    ServerMessage, UserId,
};

use super::ChatServer;
use crate::persistence::{Answer, AskId, Command, ConvKey, MessageRow, Outcome, Query};

/// How long the store may take to confirm a DM before the sender is
/// told it failed (spec §6).
pub(super) const DM_STORE_DEADLINE: Duration = Duration::from_secs(2);

/// A DM waiting for the store's reply.
pub(super) struct PendingDm {
    pub sender: ClientId,
    pub from: UserId,
    pub to: UserId,
    pub client_ref: u32,
    pub msg: ChatMessage,
}

impl ChatServer {
    /// The DM conversation between two users, as the store keys it.
    fn dm_key(a: UserId, b: UserId) -> ConvKey {
        ConvKey::Dm(a, b)
    }

    /// `DirectMessage` from a session (spec §6).
    pub(super) fn handle_direct_message(
        &mut self,
        client: ClientId,
        to: UserId,
        text: String,
        client_ref: u32,
    ) {
        let Some(from) = self.user_of.get(&client).copied() else {
            return;
        };
        if !self.allowed(client, permissions::DM, RestrictionKind::Dm) {
            self.fail_dm(client, client_ref, DmFailReason::NoPermission);
            return;
        }
        if !self.policy.users.contains_key(&to) {
            self.fail_dm(client, client_ref, DmFailReason::NoSuchUser);
            return;
        }
        if !permissions::has(self.effective_permissions(&to), permissions::DM) {
            self.fail_dm(client, client_ref, DmFailReason::NoPermission);
            return;
        }
        if !self.chat_allowed_by_rate(client) {
            // `ChatRateLimited` already went out; the ref still needs
            // an answer so the client can drop the pending row.
            self.fail_dm(client, client_ref, DmFailReason::Store);
            return;
        }
        let text =
            protocol::truncate_on_char_boundary(text.trim(), self.config.limits.max_chat_bytes);
        if text.is_empty() {
            self.fail_dm(client, client_ref, DmFailReason::Store);
            return;
        }
        self.note_activity(client);
        let (id, ts) = self.stamp();
        let msg = ChatMessage {
            id,
            ts,
            client_id: client,
            text,
        };
        if !self.persist.is_persistent() {
            // Live only: it exists exactly as long as the recipient is
            // here to take it.
            if self.sessions_of(&to).is_empty() {
                self.fail_dm(client, client_ref, DmFailReason::Offline);
                return;
            }
            self.deliver_dm(client, &from, &to, client_ref, &msg);
            return;
        }
        let row = MessageRow {
            msg_id: id,
            conv_id: 0, // the store decides from the pair
            ts,
            user_id: from,
            body: msg.text.clone(),
            name: self.get_username(client),
            edited: false,
        };
        let ask = self.persist.ask(
            Query::StoreDm {
                row,
                a: from,
                b: to,
            },
            self.now + DM_STORE_DEADLINE,
        );
        self.pending_dms.insert(
            ask,
            PendingDm {
                sender: client,
                from,
                to,
                client_ref,
                msg,
            },
        );
    }

    /// The store answered a `StoreDm` (or did not, in time).
    pub(super) fn resolve_dm_answer(&mut self, ask: AskId, outcome: Outcome) {
        let Some(pending) = self.pending_dms.remove(&ask) else {
            return;
        };
        match outcome {
            Outcome::Answered(Answer::Stored) => {
                self.deliver_dm(
                    pending.sender,
                    &pending.from,
                    &pending.to,
                    pending.client_ref,
                    &pending.msg,
                );
            }
            Outcome::Answered(Answer::Failed(e)) => {
                warn!("DM store failed: {e}");
                self.fail_dm(pending.sender, pending.client_ref, DmFailReason::Store);
            }
            _ => {
                warn!("DM store did not answer in time");
                self.fail_dm(pending.sender, pending.client_ref, DmFailReason::Store);
            }
        }
    }

    /// Ack the sender, hand the message to every session of the
    /// recipient that renders DMs.
    fn deliver_dm(
        &mut self,
        sender: ClientId,
        from: &UserId,
        to: &UserId,
        client_ref: u32,
        msg: &ChatMessage,
    ) {
        info!(
            "[{}] → [{}] (dm, {} chars)",
            self.get_username(sender),
            self.policy.users.get(to).map_or("?", |u| u.name.as_str()),
            msg.text.chars().count()
        );
        self.stats.messages_count += 1;
        self.send_msg(
            sender,
            &ServerMessage::MessageAck {
                client_ref,
                msg_id: msg.id,
            },
        );
        let direct = ServerMessage::Direct {
            from: *from,
            msg: msg.clone(),
        };
        for session in self.sessions_of(to) {
            if self.has_feature(session, features::DM) {
                self.send_msg(session, &direct);
            }
        }
    }

    fn fail_dm(&mut self, client: ClientId, client_ref: u32, reason: DmFailReason) {
        self.send_msg(client, &ServerMessage::MessageFailed { client_ref, reason });
    }

    /// `MarkRead`: the mark moves forward; nothing to keep without a
    /// store.
    pub(super) fn handle_mark_read(&mut self, client: ClientId, conv: ConvRef, msg_id: u64) {
        let Some(me) = self.user_of.get(&client).copied() else {
            return;
        };
        if !self.persist.is_persistent() {
            return;
        }
        let key = match conv {
            ConvRef::General => ConvKey::General,
            ConvRef::Dm(other) => Self::dm_key(me, other),
        };
        self.persist.send(Command::MarkRead {
            conv: key,
            user_id: me,
            msg_id,
        });
    }

    /// Ask the store for this session's DM conversations; the answer
    /// goes out as `Conversations`.
    pub(super) fn send_conversations(&mut self, client: ClientId) {
        if !self.persist.is_persistent() || !self.has_feature(client, features::DM) {
            return;
        }
        let Some(me) = self.user_of.get(&client).copied() else {
            return;
        };
        let ask = self.persist.ask(
            Query::Conversations { user_id: me },
            self.now + DM_STORE_DEADLINE,
        );
        self.pending_convs.insert(ask, client);
    }

    pub(super) fn resolve_conversations_answer(&mut self, ask: AskId, outcome: Outcome) {
        let Some(client) = self.pending_convs.remove(&ask) else {
            return;
        };
        if let Outcome::Answered(Answer::Conversations(dms)) = outcome {
            self.send_msg(client, &ServerMessage::Conversations { dms });
        }
    }

    /// Whether an answered ask belongs to a DM store or a conversation
    /// list (the persistence poll routes by this).
    pub(super) fn is_dm_ask(&self, ask: AskId) -> bool {
        self.pending_dms.contains_key(&ask)
    }

    pub(super) fn is_conversations_ask(&self, ask: AskId) -> bool {
        self.pending_convs.contains_key(&ask)
    }

    /// The store key for a DM history read by this session.
    pub(super) fn dm_conv_key(&self, client: ClientId, other: UserId) -> Option<ConvKey> {
        self.user_of.get(&client).map(|me| Self::dm_key(*me, other))
    }
}
