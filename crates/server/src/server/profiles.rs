//! Profiles (spec §7.1) and the blobs that carry avatars (spec §7).
//!
//! A profile is how a name looks — color, style, picture — and lives
//! on the user row, so it follows the key. The server is the gate:
//! the contrast check, the gold band and the size cap run here, so a
//! gold name is trustworthy and nobody is unreadable.
//!
//! Blobs are the spec's §7 messages narrowed to what 1.1.2's first
//! half needs: avatar-sized images (`avatars.max_kb`), deduplicated by
//! hash, stored on disk next to the database when persistence is on
//! and in memory for the run when it is off. Files as cards ride the
//! same messages later.

use std::{sync::Arc, time::Duration};

use log::{debug, info, warn};
use protocol::{
    features, permissions, BlobId, ClientId, Profile, ProfileDenyReason, RenameDenyReason,
    RestrictionKind, ServerMessage, UploadRejectReason, UserId,
};
use sha2::{Digest, Sha256};

use super::{fold_upload, ChatServer, Folded};
use crate::{
    persistence::{Answer, AskId, BlobRecord, Command, Outcome, Query},
    policy::{BlobMeta, Restriction},
};

/// One profile change (and one rename) per this long, per user.
pub(super) const PROFILE_RATE: Duration = Duration::from_secs(30);
/// How long a blob read from disk may take before the fetcher is told
/// the blob is missing.
const BLOB_DEADLINE: Duration = Duration::from_secs(10);
/// The chat background every name color is checked against (spec
/// §7.1): `#1a1d21`.
const CHAT_BG: [u8; 3] = [0x1a, 0x1d, 0x21];
/// Minimum contrast of a name color against the background.
const MIN_CONTRAST: f64 = 3.0;

/// An upload the client announced and may now send chunks for.
pub(super) struct AnnouncedUpload {
    pub upload_id: u32,
    pub sha256: BlobId,
    pub size: u32,
    pub name: String,
    pub mime: String,
}

impl ChatServer {
    // ── Roster ──────────────────────────────────────────────────────

    /// The profile line for a session, for sessions that render it.
    pub(super) fn user_profile(&self, client: ClientId, user: &UserId) -> ServerMessage {
        ServerMessage::UserProfile {
            client_id: client,
            profile: self
                .policy
                .users
                .get(user)
                .map(|u| u.profile.clone())
                .unwrap_or_default(),
        }
    }

    /// Send only to a session that negotiated `PROFILES`.
    pub(super) fn send_if_profiles(&mut self, client: ClientId, msg: &ServerMessage) {
        if self
            .features_of
            .get(&client)
            .is_some_and(|f| f & features::PROFILES != 0)
        {
            self.send_msg(client, msg);
        }
    }

    fn broadcast_if_profiles(&mut self, msg: &ServerMessage) {
        for peer in self.clients() {
            self.send_if_profiles(peer, msg);
        }
    }

    // ── SetProfile ──────────────────────────────────────────────────

    /// Validate and apply a session's profile (spec §7.1), then tell
    /// everyone. Refusals carry the reason; nothing is applied on one.
    pub(super) fn handle_set_profile(&mut self, client: ClientId, profile: Profile) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        let now = self.wall_now();
        if self
            .policy
            .restriction_on(&user, RestrictionKind::Profile, now)
            .is_some()
        {
            self.deny_profile(client, ProfileDenyReason::Locked);
            return;
        }
        if self
            .profile_changed_at
            .get(&user)
            .is_some_and(|t| self.now.duration_since(*t) < PROFILE_RATE)
        {
            self.deny_profile(client, ProfileDenyReason::RateLimited);
            return;
        }
        if let Some(color) = profile.name_color {
            if contrast_on_chat_bg(color) < MIN_CONTRAST {
                self.deny_profile(client, ProfileDenyReason::LowContrast);
                return;
            }
            if is_gold(color) && !self.has_permission(client, permissions::ADMIN) {
                self.deny_profile(client, ProfileDenyReason::ReservedColor);
                return;
            }
        }
        if let Some(blob) = profile.avatar
            && !self.is_avatar_blob(&blob)
        {
            self.deny_profile(client, ProfileDenyReason::BadAvatar);
            return;
        }
        self.apply_profile(&user, profile, now);
        self.profile_changed_at.insert(user, self.now);
    }

    fn deny_profile(&mut self, client: ClientId, reason: ProfileDenyReason) {
        info!(
            "{} ({client:#x}) profile refused: {}",
            self.get_username(client),
            reason.describe()
        );
        self.send_msg(client, &ServerMessage::ProfileDenied { reason });
    }

    /// Store a profile on the user row, keep the avatar references
    /// straight, persist, broadcast.
    fn apply_profile(&mut self, user: &UserId, profile: Profile, now: crate::clock::WallMs) {
        let Some(record) = self.policy.users.get_mut(user) else {
            return;
        };
        let old_avatar = record.profile.avatar;
        record.profile = profile.clone();
        if old_avatar != profile.avatar {
            if let Some(old) = old_avatar {
                self.blob_ref(&old, -1);
            }
            if let Some(new) = profile.avatar {
                self.blob_ref(&new, 1);
            }
        }
        self.persist.send(Command::SetProfile {
            user_id: *user,
            profile: profile.clone(),
            ts: now,
        });
        self.broadcast_if_profiles(&ServerMessage::ProfileUpdated {
            user_id: *user,
            profile,
        });
    }

    /// An admin cleared a profile (spec §7.1); with `lock`, a
    /// `profile` restriction keeps it cleared until lifted.
    pub(super) fn reset_profile(
        &mut self,
        actor: UserId,
        target: &UserId,
        lock: bool,
        actor_name: &str,
    ) {
        let now = self.wall_now();
        self.apply_profile(target, Profile::default(), now);
        self.audit(
            Some(actor),
            "profile.reset",
            Some(super::hex(&target[..8])),
            Some(if lock { "locked" } else { "cleared" }.to_string()),
        );
        if lock {
            self.restrict(
                Restriction {
                    id: 0,
                    user_id: Some(*target),
                    key_hash: None,
                    ip: None,
                    kind: RestrictionKind::Profile,
                    until: None,
                    reason: "profile locked by an admin".to_string(),
                    by: Some(actor),
                    created_at: now,
                },
                actor_name,
            );
        }
    }

    // ── SetNickname ─────────────────────────────────────────────────

    /// A new name for a live session: the rules of `Hello` (sanitize,
    /// unique among the users online, a guest cannot take an offline
    /// member's name), one per 30 s, then everyone's roster renames.
    pub(super) fn handle_set_nickname(&mut self, client: ClientId, name: &str) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        let name = protocol::sanitize_username(name);
        if name.is_empty() {
            self.send_msg(
                client,
                &ServerMessage::RenameDenied {
                    reason: RenameDenyReason::Invalid,
                },
            );
            return;
        }
        if self
            .renamed_at
            .get(&client)
            .is_some_and(|t| self.now.duration_since(*t) < PROFILE_RATE)
        {
            self.send_msg(
                client,
                &ServerMessage::RenameDenied {
                    reason: RenameDenyReason::RateLimited,
                },
            );
            return;
        }
        let own_sessions = self.sessions_of(&user);
        let taken_online = self
            .usernames
            .iter()
            .any(|(c, existing)| !own_sessions.contains(c) && existing.eq_ignore_ascii_case(&name));
        let guest_takes_member = self.policy.rank_of(&user) == permissions::RANK_GUEST
            && self.policy.name_held_by_member(&name, &user);
        if taken_online || guest_takes_member {
            self.send_msg(
                client,
                &ServerMessage::RenameDenied {
                    reason: RenameDenyReason::Taken,
                },
            );
            return;
        }
        let old = self.get_username(client);
        info!("[{old}] is now [{name}]");
        for c in &own_sessions {
            self.usernames.insert(*c, name.clone());
        }
        if let Some(record) = self.policy.users.get_mut(&user) {
            record.name.clone_from(&name);
            let record = record.clone();
            self.persist.send(Command::UpsertUser {
                user_id: user,
                user: record,
            });
        }
        self.renamed_at.insert(client, self.now);
        let msg = ServerMessage::UserRenamed {
            client_id: client,
            name,
        };
        self.broadcast_if_profiles(&msg);
    }

    // ── Blobs: upload ───────────────────────────────────────────────

    /// A client announced an upload. Known hashes are acknowledged
    /// without transfer; otherwise the size and type are checked and
    /// the chunks may follow. One in flight per client.
    pub(super) fn handle_upload_begin(
        &mut self,
        client: ClientId,
        upload_id: u32,
        name: String,
        mime: String,
        size: u32,
        sha256: BlobId,
    ) {
        if self.policy.blobs.contains_key(&sha256) {
            self.send_msg(
                client,
                &ServerMessage::UploadAccept {
                    upload_id,
                    needed: false,
                },
            );
            return;
        }
        let cap = self.avatar_cap();
        if size == 0 || usize::try_from(size).is_ok_and(|s| s > cap) {
            self.reject_upload(client, upload_id, UploadRejectReason::TooLarge);
            return;
        }
        if !is_avatar_mime(&mime) {
            self.reject_upload(client, upload_id, UploadRejectReason::BadType);
            return;
        }
        self.blob_uploads.remove(&client);
        self.announced_uploads.insert(
            client,
            AnnouncedUpload {
                upload_id,
                sha256,
                size,
                name: protocol::truncate_on_char_boundary(
                    &name.chars().filter(|c| !c.is_control()).collect::<String>(),
                    120,
                ),
                mime,
            },
        );
        self.send_msg(
            client,
            &ServerMessage::UploadAccept {
                upload_id,
                needed: true,
            },
        );
    }

    /// One slice of an announced upload; on completion the bytes are
    /// hashed, sniffed and stored.
    pub(super) fn handle_upload_chunk(
        &mut self,
        client: ClientId,
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    ) {
        let Some(announced) = self.announced_uploads.get(&client) else {
            self.reject_upload(client, upload_id, UploadRejectReason::NotAnnounced);
            return;
        };
        if announced.upload_id != upload_id {
            self.reject_upload(client, upload_id, UploadRejectReason::NotAnnounced);
            return;
        }
        let cap = self.avatar_cap();
        #[allow(clippy::cast_possible_truncation)] // small: cap / chunk
        let max_chunks = cap.div_ceil(protocol::IMAGE_CHUNK_BYTES) as u32;
        let Some(folded) = fold_upload(
            &mut self.blob_uploads,
            client,
            upload_id,
            index,
            count,
            data,
            max_chunks,
            cap,
            self.now,
        ) else {
            return;
        };
        let bytes = match folded {
            Folded::Complete(bytes) => bytes,
            Folded::Malformed => {
                self.announced_uploads.remove(&client);
                self.reject_upload(client, upload_id, UploadRejectReason::Refused);
                return;
            }
            Folded::Oversized(_) => {
                self.announced_uploads.remove(&client);
                self.reject_upload(client, upload_id, UploadRejectReason::TooLarge);
                return;
            }
        };
        let Some(announced) = self.announced_uploads.remove(&client) else {
            return;
        };
        let digest: BlobId = Sha256::digest(&bytes).into();
        if digest != announced.sha256 || bytes.len() != announced.size as usize {
            self.reject_upload(client, upload_id, UploadRejectReason::HashMismatch);
            return;
        }
        let Some(sniffed) = sniff_image_mime(&bytes) else {
            self.reject_upload(client, upload_id, UploadRejectReason::BadType);
            return;
        };
        let created_by = self.user_of.get(&client).copied();
        info!(
            "[{}] uploaded a {} blob ({} KB)",
            self.get_username(client),
            sniffed,
            bytes.len() / 1024
        );
        self.store_blob(
            digest,
            BlobMeta {
                mime: sniffed.to_string(),
                size: announced.size,
                name: announced.name,
                created_by,
                created_at: self.wall_now(),
                refs: 0,
            },
            Some(announced.mime),
            bytes,
        );
        self.send_msg(
            client,
            &ServerMessage::UploadDone {
                upload_id,
                blob_id: digest,
            },
        );
    }

    fn reject_upload(&mut self, client: ClientId, upload_id: u32, reason: UploadRejectReason) {
        debug!(
            "upload {upload_id} from {} rejected: {}",
            self.get_username(client),
            reason.describe()
        );
        self.send_msg(client, &ServerMessage::UploadRejected { upload_id, reason });
    }

    /// Keep a blob: metadata in the policy, bytes in memory (the only
    /// copy without persistence) and, with it, on disk.
    fn store_blob(
        &mut self,
        blob_id: BlobId,
        meta: BlobMeta,
        declared_mime: Option<String>,
        bytes: Vec<u8>,
    ) {
        self.persist.send(Command::StoreBlob(Box::new(BlobRecord {
            blob_id,
            mime: meta.mime.clone(),
            declared_mime,
            size: meta.size,
            name: meta.name.clone(),
            created_by: meta.created_by,
            created_at: meta.created_at,
            bytes: bytes.clone(),
        })));
        self.policy.blobs.insert(blob_id, meta);
        self.blob_bytes.insert(blob_id, Arc::new(bytes));
    }

    /// Adjust how many profiles point at a blob; an unreferenced blob
    /// leaves memory (and, without persistence, existence).
    fn blob_ref(&mut self, blob_id: &BlobId, delta: i32) {
        let Some(meta) = self.policy.blobs.get_mut(blob_id) else {
            return;
        };
        meta.refs = if delta < 0 {
            meta.refs.saturating_sub(delta.unsigned_abs())
        } else {
            meta.refs.saturating_add(delta.unsigned_abs())
        };
        let refs = meta.refs;
        self.persist.send(Command::BlobRefs {
            blob_id: *blob_id,
            refs,
        });
        if refs == 0 {
            self.blob_bytes.remove(blob_id);
            if !self.persist.is_persistent() {
                self.policy.blobs.remove(blob_id);
            }
        }
    }

    /// The avatar size cap in bytes (`avatars.max_kb`).
    fn avatar_cap(&self) -> usize {
        usize::try_from(self.config.persistence.avatars.max_kb)
            .unwrap_or(256)
            .saturating_mul(1024)
    }

    /// A blob the server holds, of an image type, within the avatar cap.
    fn is_avatar_blob(&self, blob_id: &BlobId) -> bool {
        let cap = self.avatar_cap();
        self.policy.blobs.get(blob_id).is_some_and(|meta| {
            is_avatar_mime(&meta.mime) && usize::try_from(meta.size).is_ok_and(|s| s <= cap)
        })
    }

    // ── Blobs: fetch ────────────────────────────────────────────────

    /// A client wants a blob's bytes: from memory at once; from disk
    /// through the persistence thread; `BlobMissing` otherwise.
    pub(super) fn handle_fetch_blob(&mut self, client: ClientId, blob_id: BlobId) {
        if let Some(bytes) = self.blob_bytes.get(&blob_id).cloned() {
            self.send_blob(client, blob_id, &bytes);
            return;
        }
        if !self.policy.blobs.contains_key(&blob_id) || !self.persist.is_persistent() {
            self.send_msg(client, &ServerMessage::BlobMissing { blob_id });
            return;
        }
        if let Some((_, waiting)) = self.pending_blobs.get_mut(&blob_id) {
            waiting.push(client);
            return;
        }
        let ask = self
            .persist
            .ask(Query::Blob { blob_id }, self.now + BLOB_DEADLINE);
        self.pending_blobs.insert(blob_id, (ask, vec![client]));
    }

    /// Whether an answered ask belongs to a blob fetch (the
    /// persistence poll routes by this).
    pub(super) fn is_blob_ask(&self, ask: AskId) -> bool {
        self.pending_blobs.values().any(|(a, _)| *a == ask)
    }

    /// The persistence thread answered a blob read (or did not, in
    /// time): every waiting fetcher gets the bytes or `BlobMissing`.
    pub(super) fn resolve_blob_answer(&mut self, ask: AskId, outcome: Outcome) {
        let Some(blob_id) = self
            .pending_blobs
            .iter()
            .find(|(_, (a, _))| *a == ask)
            .map(|(id, _)| *id)
        else {
            return;
        };
        let Some((_, waiting)) = self.pending_blobs.remove(&blob_id) else {
            return;
        };
        let bytes = match outcome {
            Outcome::Answered(Answer::Blob(Some(bytes))) => {
                let bytes = Arc::new(bytes);
                self.blob_bytes.insert(blob_id, bytes.clone());
                Some(bytes)
            }
            Outcome::Answered(Answer::Failed(e)) => {
                warn!("blob read failed: {e}");
                None
            }
            _ => None,
        };
        for client in waiting {
            match &bytes {
                Some(bytes) => self.send_blob(client, blob_id, bytes),
                None => self.send_msg(client, &ServerMessage::BlobMissing { blob_id }),
            }
        }
    }

    /// Chunk a blob onto one paced Media stream to a client (the same
    /// path an image relay takes).
    fn send_blob(&mut self, client: ClientId, blob_id: BlobId, bytes: &[u8]) {
        #[allow(clippy::cast_possible_truncation)] // bounded by the avatar cap
        let count = bytes.len().div_ceil(protocol::IMAGE_CHUNK_BYTES) as u32;
        let mut frames = Vec::with_capacity(count as usize);
        for (i, chunk) in bytes.chunks(protocol::IMAGE_CHUNK_BYTES).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let msg = ServerMessage::BlobChunk {
                blob_id,
                index: i as u32,
                count,
                data: chunk.to_vec(),
            };
            let Some(encoded) = protocol::encode_server_message(&msg) else {
                warn!("blob chunk failed to encode; dropping fetch");
                return;
            };
            frames.push(protocol::Bytes::from(encoded));
        }
        self.stats.bytes_sent += bytes.len() as u64;
        if let Err(e) = self.driver.send_media(self.now, client, frames.into_iter()) {
            debug!("blob send to {client:#x} failed: {e}");
        }
    }
}

// ── Color rules (spec §7.1) ───────────────────────────────────────────

fn channel(byte: u8) -> f64 {
    let c = f64::from(byte) / 255.0;
    if c <= 0.039_28 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(rgb: [u8; 3]) -> f64 {
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

fn rgb_of(color: u32) -> [u8; 3] {
    let [_, r, g, b] = color.to_be_bytes();
    [r, g, b]
}

/// WCAG contrast ratio of a name color against the chat background.
pub(super) fn contrast_on_chat_bg(color: u32) -> f64 {
    let a = luminance(rgb_of(color));
    let b = luminance(CHAT_BG);
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// The gold band the server reserves for admins: hue 38°–65°,
/// saturation ≥ 0.35 (HSV).
pub(super) fn is_gold(color: u32) -> bool {
    let [r, g, b] = rgb_of(color).map(|c| f64::from(c) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if max <= 0.0 || delta <= 0.0 {
        return false;
    }
    let saturation = delta / max;
    let hue = if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    (38.0..=65.0).contains(&hue) && saturation >= 0.35
}

// ── Image sniffing ────────────────────────────────────────────────────

fn is_avatar_mime(mime: &str) -> bool {
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp"
    )
}

/// The type the bytes say they are, for the types an avatar may be.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&super::PNG_MAGIC) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_and_gold_follow_the_spec() {
        // The client's palette entries all read; near-background does not.
        assert!(contrast_on_chat_bg(0x00E0_6C75) >= 3.0);
        assert!(contrast_on_chat_bg(0x004A_9AE0) >= 3.0);
        assert!(contrast_on_chat_bg(0x0020_2020) < 3.0);
        // Gold: hue 38°–65°, saturation ≥ 0.35.
        assert!(is_gold(0x00E5_C158)); // the theme's gold
        assert!(is_gold(0x00FF_D700)); // pure gold
        assert!(!is_gold(0x00FF_8A3D)); // orange, hue ~22°
        assert!(!is_gold(0x00F5_F5DC)); // beige: too pale
        assert!(!is_gold(0x00C3_E88D)); // lime, hue ~86°
    }

    #[test]
    fn sniffing_admits_the_three_types() {
        assert_eq!(
            sniff_image_mime(&super::super::PNG_MAGIC),
            Some("image/png")
        );
        // A PNG needs the whole eight-byte signature, not just "PNG".
        assert_eq!(
            sniff_image_mime(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0]),
            None
        );
        assert_eq!(
            sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(sniff_image_mime(&webp), Some("image/webp"));
        assert_eq!(sniff_image_mime(b"GIF89a"), None);
    }
}
