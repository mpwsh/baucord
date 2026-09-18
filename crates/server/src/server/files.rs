//! Files in conversations (spec §7).
//!
//! An upload is either avatar-sized (an image within `avatars.max_kb`,
//! open to everyone as before) or a *file*: anything else, gated by
//! `UPLOAD`, `files.max_mb`, the per-user daily allowance and the
//! store's total cap — the cap evicts the oldest unreferenced blobs
//! first and refuses only when nothing is evictable. A stored blob is
//! posted into General or a DM with `SendFile`; it then rides as a
//! `FileMessage` (sessions without `FILES` get a plain `Chat` naming
//! it) and sits in history as a row with a `blob_id`, replayed with
//! its `HistoryAttachments`. Nothing is kept without persistence: a
//! memory-mode server refuses files — a 50 MB live relay to everyone
//! online is not a path this server has.

use log::info;
use protocol::{
    features, permissions, Attachment, BlobId, ChatMessage, ClientId, ConvRef, DmFailReason,
    RestrictionKind, ServerMessage, UploadRejectReason, UserId,
};

use super::{dms::PendingDm, ChatServer};
use crate::persistence::{Command, MessageRow, Query, GENERAL_CONV};

/// A UTC day, for the allowance.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// One user's upload volume for the day it was last counted in.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct DailyUpload {
    pub day: u64,
    pub bytes: u64,
}

/// `📎 notes.pdf (1.2 MB)`: what a session without `FILES` reads.
pub(super) fn fallback_text(attachment: &Attachment) -> String {
    format!(
        "\u{1F4CE} {} ({})",
        attachment.name,
        human_size(u64::from(attachment.size))
    )
}

/// `3 d`, `5 h`, `20 min`, `40 s`: a span, for audit lines.
#[must_use]
pub fn human_duration(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{} d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{} h", secs / 3_600)
    } else if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// `1.2 MB`, `640 KB`, `12 B`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes >= 1 << 30 {
        format!("{:.1} GB", b / f64::from(1u32 << 30))
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", b / f64::from(1u32 << 20))
    } else if bytes >= 1 << 10 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

impl ChatServer {
    /// The per-file cap in bytes (`files.max_mb`, never above what the
    /// wire carries).
    pub(super) fn file_cap(&self) -> usize {
        usize::try_from(u64::from(self.config.persistence.files.max_mb) << 20)
            .unwrap_or(usize::MAX)
            .min(protocol::MAX_BLOB_BYTES)
    }

    /// Every stored blob's bytes, as the policy knows them.
    fn blob_bytes_total(&self) -> u64 {
        self.policy.blobs.values().map(|b| u64::from(b.size)).sum()
    }

    /// Admit a file upload at announce time, or say why not. Runs the
    /// cap's eviction, so a refusal means nothing more can go.
    pub(super) fn admit_file_upload(
        &mut self,
        client: ClientId,
        size: u32,
    ) -> Result<(), UploadRejectReason> {
        if !self.has_feature(client, features::FILES) || !self.persist.is_persistent() {
            return Err(UploadRejectReason::NoPermission);
        }
        if !self.allowed(client, permissions::UPLOAD, RestrictionKind::GeneralText) {
            return Err(UploadRejectReason::NoPermission);
        }
        if size == 0 || usize::try_from(size).is_ok_and(|s| s > self.file_cap()) {
            return Err(UploadRejectReason::TooLarge);
        }
        let Some(user) = self.user_of.get(&client).copied() else {
            return Err(UploadRejectReason::NoPermission);
        };
        let allowance = u64::from(self.config.persistence.files.per_user_daily_mb) << 20;
        let today = self.wall_now() / DAY_MS;
        let used = self
            .uploads_today
            .get(&user)
            .filter(|d| d.day == today)
            .map_or(0, |d| d.bytes);
        if !permissions::has(
            self.effective_permissions(&user),
            permissions::BYPASS_LIMITS,
        ) && used + u64::from(size) > allowance
        {
            return Err(UploadRejectReason::Quota);
        }
        if !self.make_room_for(u64::from(size)) {
            return Err(UploadRejectReason::Full);
        }
        // The same sliding-minute transfer budget the chat-image relay
        // keeps: per client and server-wide.
        if !permissions::has(
            self.effective_permissions(&user),
            permissions::BYPASS_LIMITS,
        ) && !self.allow_image(client, usize::try_from(size).unwrap_or(usize::MAX))
        {
            return Err(UploadRejectReason::Quota);
        }
        Ok(())
    }

    /// Count a finished upload against the user's day.
    pub(super) fn note_upload(&mut self, user: UserId, size: u32) {
        let today = self.wall_now() / DAY_MS;
        let entry = self.uploads_today.entry(user).or_default();
        if entry.day != today {
            *entry = DailyUpload {
                day: today,
                bytes: 0,
            };
        }
        entry.bytes += u64::from(size);
    }

    /// Make `needed` bytes fit under `files.total_cap_gb`, evicting the
    /// oldest unreferenced blobs first; `false` when nothing more can
    /// go.
    fn make_room_for(&mut self, needed: u64) -> bool {
        let cap = u64::from(self.config.persistence.files.total_cap_gb) << 30;
        let mut total = self.blob_bytes_total();
        while total + needed > cap {
            let Some(oldest) = self
                .policy
                .blobs
                .iter()
                .filter(|(_, b)| b.refs == 0)
                .min_by_key(|(id, b)| (b.created_at, **id))
                .map(|(id, _)| *id)
            else {
                return false;
            };
            if let Some(gone) = self.policy.blobs.remove(&oldest) {
                total -= u64::from(gone.size);
                info!(
                    "evicted blob {} ({}, {}) for room",
                    crate::persistence::hex(&oldest),
                    gone.name,
                    human_size(u64::from(gone.size))
                );
            }
            self.blob_bytes.remove(&oldest);
            self.persist.send(Command::DeleteBlob { blob_id: oldest });
        }
        true
    }

    /// The card's view of a blob, from the policy's metadata.
    pub(super) fn attachment_of(&self, blob_id: &BlobId) -> Option<Attachment> {
        self.policy.blobs.get(blob_id).map(|b| Attachment {
            blob_id: *blob_id,
            name: b.name.clone(),
            mime: b.mime.clone(),
            size: b.size,
        })
    }

    /// `SendFile`: post an uploaded blob into General or a DM.
    pub(super) fn handle_send_file(
        &mut self,
        client: ClientId,
        conv: ConvRef,
        blob_id: BlobId,
        client_ref: u32,
    ) {
        let Some(from) = self.user_of.get(&client).copied() else {
            return;
        };
        let fail = |this: &mut Self, reason: DmFailReason| {
            this.send_msg(client, &ServerMessage::MessageFailed { client_ref, reason });
        };
        if !self.has_feature(client, features::FILES) || !self.persist.is_persistent() {
            fail(self, DmFailReason::NoPermission);
            return;
        }
        // Your own upload, stored: never someone else's by hash.
        let Some(attachment) = self
            .policy
            .blobs
            .get(&blob_id)
            .filter(|b| b.created_by == Some(from))
            .and(self.attachment_of(&blob_id))
        else {
            fail(self, DmFailReason::NoSuchBlob);
            return;
        };
        if !self.allowed(client, permissions::UPLOAD, RestrictionKind::GeneralText) {
            fail(self, DmFailReason::NoPermission);
            return;
        }
        match conv {
            ConvRef::General => {
                if !self.allowed(
                    client,
                    permissions::TEXT_GENERAL,
                    RestrictionKind::GeneralText,
                ) {
                    fail(self, DmFailReason::NoPermission);
                    return;
                }
                if !self.chat_allowed_by_rate(client) {
                    fail(self, DmFailReason::Store);
                    return;
                }
                let username = self.get_username(client);
                info!("[{username}] posted {}", fallback_text(&attachment));
                self.stats.messages_count += 1;
                self.note_activity(client);
                let (id, ts) = self.stamp();
                let msg = ChatMessage {
                    id,
                    ts,
                    client_id: client,
                    text: String::new(),
                };
                self.blob_ref(&blob_id, 1);
                let row = MessageRow {
                    msg_id: id,
                    conv_id: GENERAL_CONV,
                    ts,
                    user_id: from,
                    body: String::new(),
                    name: username,
                    edited: false,
                    blob_id: Some(blob_id),
                };
                self.remember_message(&row);
                self.persist.send(Command::InsertMessage(row));
                self.send_msg(
                    client,
                    &ServerMessage::MessageAck {
                        client_ref,
                        msg_id: id,
                    },
                );
                self.broadcast_file(ConvRef::General, from, &msg, &attachment, None);
            }
            ConvRef::Dm(to) => {
                if !self.allowed(client, permissions::DM, RestrictionKind::Dm)
                    || !self.policy.users.contains_key(&to)
                    || !permissions::has(self.effective_permissions(&to), permissions::DM)
                {
                    fail(self, DmFailReason::NoPermission);
                    return;
                }
                if !self.chat_allowed_by_rate(client) {
                    fail(self, DmFailReason::Store);
                    return;
                }
                self.note_activity(client);
                let (id, ts) = self.stamp();
                let msg = ChatMessage {
                    id,
                    ts,
                    client_id: client,
                    text: String::new(),
                };
                self.blob_ref(&blob_id, 1);
                let row = MessageRow {
                    msg_id: id,
                    conv_id: 0,
                    ts,
                    user_id: from,
                    body: String::new(),
                    name: self.get_username(client),
                    edited: false,
                    blob_id: Some(blob_id),
                };
                let ask = self.persist.ask(
                    Query::StoreDm {
                        row,
                        a: from,
                        b: to,
                    },
                    self.now + super::dms::DM_STORE_DEADLINE,
                );
                self.pending_dms.insert(
                    ask,
                    PendingDm {
                        sender: client,
                        from,
                        to,
                        client_ref,
                        msg,
                        attachment: Some(attachment),
                    },
                );
            }
        }
    }

    /// A file into the room, or to one user's sessions: `FileMessage`
    /// for sessions with the bit, a plain `Chat` (or `Direct`) naming
    /// it for the others.
    pub(super) fn broadcast_file(
        &mut self,
        conv: ConvRef,
        from: UserId,
        msg: &ChatMessage,
        attachment: &Attachment,
        to: Option<&UserId>,
    ) {
        let rich = ServerMessage::FileMessage {
            conv,
            from,
            msg: msg.clone(),
            attachment: attachment.clone(),
        };
        let plain_msg = ChatMessage {
            text: fallback_text(attachment),
            ..msg.clone()
        };
        let plain = match conv {
            ConvRef::General => ServerMessage::Chat(plain_msg),
            ConvRef::Dm(_) => ServerMessage::Direct {
                from,
                msg: plain_msg,
            },
        };
        let targets = match to {
            Some(user) => self
                .sessions_of(user)
                .into_iter()
                .filter(|s| self.has_feature(*s, features::DM))
                .collect(),
            None => self.clients(),
        };
        for peer in targets {
            if self.has_feature(peer, features::FILES) {
                self.send_msg(peer, &rich);
            } else {
                self.send_msg(peer, &plain);
            }
        }
    }

    /// The attachments of a history page, for a session with the bit;
    /// `None` when the page has none.
    pub(super) fn history_attachments(
        &self,
        conv: ConvRef,
        rows: &[MessageRow],
    ) -> Option<ServerMessage> {
        let items: Vec<(u64, Attachment)> = rows
            .iter()
            .filter_map(|r| {
                r.blob_id
                    .and_then(|b| self.attachment_of(&b))
                    .map(|a| (r.msg_id, a))
            })
            .collect();
        (!items.is_empty()).then_some(ServerMessage::HistoryAttachments { conv, items })
    }

    /// A history row for a session without the bit: the file named in
    /// the text, since it cannot draw the card.
    pub(super) fn plain_history_text(&self, row: &MessageRow) -> Option<String> {
        row.blob_id
            .and_then(|b| self.attachment_of(&b))
            .map(|a| fallback_text(&a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_a_file_manager() {
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(640 * 1024), "640 KB");
        assert_eq!(human_size(1_258_291), "1.2 MB");
        assert_eq!(human_size(3 << 30), "3.0 GB");
    }
}
