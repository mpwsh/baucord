//! The admin panel's commands (spec §8, 1.1.3): the audit log as a
//! disk read, custom roles over the wire (the same rules as `server
//! role`), the blob list and delete, and the server's counters. The
//! permission check and the in-memory change happen here on the
//! tick; the persistence thread does the writes; everything is
//! audited like the rest of moderation.

use std::time::Duration;

use log::{info, warn};
use protocol::{
    permissions, AdminCommand, AdminError, AdminOutcome, AuditEntry, BlobSummary, ClientId,
    ServerMessage, StatsSummary, UserId,
};

use super::{hex, ChatServer, Waiting};
use crate::{
    persistence::{Answer, Command, Outcome, Query},
    policy::RoleRecord,
};

/// How long the panel waits for the audit read.
const AUDIT_DEADLINE: Duration = Duration::from_secs(2);
use crate::policy::MAX_ROLES;

impl ChatServer {
    /// `AuditLog`: ask the store; the answer goes out as the command's
    /// `AdminResult` when it lands (`resolve_audit_answer`).
    pub(super) fn handle_audit_log(
        &mut self,
        client: ClientId,
        request_id: u32,
        before: Option<u64>,
        limit: u16,
    ) {
        let allowed = self
            .user_of
            .get(&client)
            .is_some_and(|u| permissions::has(self.effective_permissions(u), permissions::PANEL));
        if !allowed {
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
                    result: Ok(AdminOutcome::Audit(Vec::new())),
                },
            );
            return;
        }
        let ask = self.persist.ask(
            Query::Audit {
                before,
                limit: limit.clamp(1, 200),
            },
            self.now + AUDIT_DEADLINE,
        );
        self.waiting
            .insert(ask, Waiting::Audit { client, request_id });
    }

    /// The store answered an audit read (or did not, in time).
    pub(super) fn resolve_audit_answer(
        &mut self,
        client: ClientId,
        request_id: u32,
        outcome: Outcome,
    ) {
        let result = match outcome {
            Outcome::Answered(Answer::Audit(rows)) => Ok(AdminOutcome::Audit(
                rows.into_iter()
                    .map(|(id, row)| AuditEntry {
                        id,
                        ts: row.ts,
                        actor: row.actor.map(|a| self.name_of_user(&a)),
                        target: row
                            .target
                            .as_deref()
                            .map(|t| self.audit_target_name(&row.action, t)),
                        action: row.action,
                        detail: row.detail,
                    })
                    .collect(),
            )),
            Outcome::Answered(Answer::Failed(e)) => Err(AdminError::Invalid(e)),
            _ => Err(AdminError::Invalid(
                "the audit log did not answer in time".into(),
            )),
        };
        self.send_admin_result(client, request_id, result);
    }

    /// An audit target as the panel should read it: a user's short id
    /// becomes their name, a role or channel id its name, a
    /// restriction or message id gets a `#`. Codes and hashes stay.
    pub(super) fn audit_target_name(&self, action: &str, target: &str) -> String {
        // The target decoded once and compared as bytes: encoding every
        // user's id for every row made some 200,000 strings for one
        // audit page on a server with 1,000 users (review P3-10).
        let user_named = || {
            let short = short_id(target)?;
            self.policy
                .users
                .iter()
                .find(|(id, _)| id[..8] == short)
                .map(|(_, u)| u.name.clone())
        };
        match action {
            a if a.starts_with("user.")
                || a == "restrict"
                || a == "profile.reset"
                || a == "report.user"
                || a == "report.message" =>
            {
                user_named().unwrap_or_else(|| target.to_string())
            }
            a if a.starts_with("role.") => target
                .parse::<u32>()
                .ok()
                .and_then(|id| self.policy.roles.get(&id))
                .map_or_else(|| format!("role {target}"), |r| r.name.clone()),
            a if a.starts_with("channel.") => target
                .parse::<u64>()
                .ok()
                .and_then(|stable| {
                    self.policy
                        .channels
                        .iter()
                        .find(|c| c.stable_id == stable)
                        .map(|c| c.name.clone())
                })
                .unwrap_or_else(|| format!("channel {target}")),
            "restriction.lift" => format!("restriction #{target}"),
            "message.delete" => format!("message #{target}"),
            "report.resolve" => format!("report #{target}"),
            _ => target.to_string(),
        }
    }

    /// A user's name of record, or a short id.
    pub(super) fn name_of_user(&self, user: &UserId) -> String {
        self.policy
            .users
            .get(user)
            .map_or_else(|| hex(&user[..4]), |u| u.name.clone())
    }

    /// The panel's remaining commands (the synchronous ones); `None`
    /// when `command` is not one of them.
    pub(super) fn handle_panel_command(
        &mut self,
        actor: UserId,
        perms: u64,
        command: &AdminCommand,
    ) -> Option<Result<AdminOutcome, AdminError>> {
        let need = |bit: u64| {
            if permissions::has(perms, bit) {
                Ok(())
            } else {
                Err(AdminError::NoPermission)
            }
        };
        Some(match command {
            AdminCommand::CreateRole {
                name,
                permissions: set,
                icon,
                color,
            } => need(permissions::MANAGE_ROLES)
                .and_then(|()| self.create_role(actor, name, *set, icon, *color)),
            AdminCommand::EditRole {
                role,
                name,
                permissions: set,
                icon,
                color,
            } => need(permissions::MANAGE_ROLES).and_then(|()| {
                self.edit_role(actor, *role, name.as_deref(), *set, icon.as_deref(), *color)
            }),
            AdminCommand::DeleteRole { role } => {
                need(permissions::MANAGE_ROLES).and_then(|()| self.delete_role(actor, *role))
            }
            AdminCommand::ListBlobs => need(permissions::RESTRICT).map(|()| {
                let mut blobs: Vec<BlobSummary> = self
                    .policy
                    .blobs
                    .iter()
                    .map(|(id, b)| BlobSummary {
                        blob_id: *id,
                        name: b.name.clone(),
                        mime: b.mime.clone(),
                        size: b.size,
                        created_by: b.created_by.map(|u| self.name_of_user(&u)),
                        created_at: b.created_at,
                        refs: b.refs,
                    })
                    .collect();
                blobs.sort_by_key(|b| std::cmp::Reverse(b.created_at));
                AdminOutcome::Blobs(blobs)
            }),
            AdminCommand::DeleteBlob { blob_id } => need(permissions::RESTRICT).map(|()| {
                if let Some(gone) = self.forget_blob(blob_id) {
                    self.persist.send(Command::DeleteBlob { blob_id: *blob_id });
                    self.audit(
                        Some(actor),
                        "blob.delete",
                        Some(hex(&blob_id[..8])),
                        Some(format!(
                            "{} ({})",
                            gone.name,
                            super::files::human_size(u64::from(gone.size))
                        )),
                    );
                }
                AdminOutcome::Done
            }),
            AdminCommand::ServerStats => need(permissions::PANEL).map(|()| {
                let persist = self.persist.stats();
                AdminOutcome::Stats(StatsSummary {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    uptime_secs: self.now.duration_since(self.started).as_secs(),
                    online: u32::try_from(self.user_of.len()).unwrap_or(u32::MAX),
                    users: u32::try_from(self.policy.users.len()).unwrap_or(u32::MAX),
                    messages: self.stats.messages_count,
                    blobs: u32::try_from(self.policy.blobs.len()).unwrap_or(u32::MAX),
                    blob_bytes: self.policy.blobs.values().map(|b| u64::from(b.size)).sum(),
                    blob_cap_bytes: u64::from(self.config.persistence.files.total_cap_gb) << 30,
                    persistent: persist.persistent,
                    queue_depth: u32::try_from(persist.queued).unwrap_or(u32::MAX),
                    queue_capacity: u32::try_from(persist.capacity).unwrap_or(u32::MAX),
                    dropped_writes: persist.dropped_writes,
                    write_errors: persist.write_errors,
                })
            }),
            AdminCommand::AuditLog { .. } => {
                // Routed before the synchronous dispatch; never here.
                Err(AdminError::Invalid("audit reads are asynchronous".into()))
            }
            _ => return None,
        })
    }

    // ── Roles over the wire (spec §4) ──────────────────────────────────
    // The same rules as `server role`: a strict name, never `ADMIN`, a
    // rank derived from the set; and, since there is an actor here,
    // the role must stay below the actor's own rank.

    fn check_role_for(
        &self,
        actor: UserId,
        name: &str,
        perms: u64,
        icon: &str,
        current: Option<u32>,
    ) -> Result<u16, AdminError> {
        if !permissions::valid_custom_role_name(name) {
            return Err(AdminError::Invalid(format!(
                "'{name}' cannot name a role: 1–32 lowercase letters, digits and hyphens"
            )));
        }
        if !permissions::valid_role_icon(icon) {
            return Err(AdminError::Invalid(format!("'{icon}' is not a role icon")));
        }
        if permissions::has(perms, permissions::ADMIN) {
            return Err(AdminError::Invalid(
                "a custom role cannot hold `admin`".to_string(),
            ));
        }
        if self
            .policy
            .roles
            .iter()
            .any(|(id, r)| r.name == name && Some(*id) != current)
        {
            return Err(AdminError::Invalid(format!(
                "a role is already named {name}"
            )));
        }
        let rank = permissions::derived_rank(perms);
        if rank >= self.policy.rank_of(&actor) {
            return Err(AdminError::Outranked);
        }
        Ok(rank)
    }

    fn create_role(
        &mut self,
        actor: UserId,
        name: &str,
        perms: u64,
        icon: &str,
        color: u32,
    ) -> Result<AdminOutcome, AdminError> {
        if self.policy.roles.len() >= MAX_ROLES {
            return Err(AdminError::Invalid(format!(
                "a server has at most {MAX_ROLES} roles"
            )));
        }
        let rank = self.check_role_for(actor, name, perms, icon, None)?;
        let id = self.policy.roles.keys().max().map_or(1, |m| m + 1);
        self.policy.roles.insert(
            id,
            RoleRecord {
                name: name.to_string(),
                permissions: perms,
                rank,
                builtin: false,
                icon: icon.to_string(),
                color,
            },
        );
        self.persist.send(Command::InsertRole {
            id,
            name: name.to_string(),
            permissions: perms,
            rank,
            icon: icon.to_string(),
            color,
        });
        self.audit(
            Some(actor),
            "role.create",
            Some(id.to_string()),
            Some(format!(
                "{name}: {}",
                permissions::permission_names(perms).join(", ")
            )),
        );
        info!("role {name} ({id}) created from the panel");
        self.broadcast_roles();
        Ok(AdminOutcome::RoleCreated(id))
    }

    fn edit_role(
        &mut self,
        actor: UserId,
        id: u32,
        name: Option<&str>,
        perms: Option<u64>,
        icon: Option<&str>,
        color: Option<u32>,
    ) -> Result<AdminOutcome, AdminError> {
        let current = self
            .policy
            .roles
            .get(&id)
            .cloned()
            .ok_or(AdminError::NoSuchRole)?;
        if current.builtin {
            return Err(AdminError::Invalid(
                "built-in roles cannot be changed".to_string(),
            ));
        }
        // The actor must outrank the role as it is, too.
        if current.rank >= self.policy.rank_of(&actor) {
            return Err(AdminError::Outranked);
        }
        let name = name.unwrap_or(&current.name).to_string();
        let perms = perms.unwrap_or(current.permissions);
        let icon = icon.unwrap_or(&current.icon).to_string();
        let color = color.unwrap_or(current.color);
        let rank = self.check_role_for(actor, &name, perms, &icon, Some(id))?;
        self.policy.roles.insert(
            id,
            RoleRecord {
                name: name.clone(),
                permissions: perms,
                rank,
                builtin: false,
                icon: icon.clone(),
                color,
            },
        );
        self.persist.send(Command::UpdateRole {
            id,
            name: name.clone(),
            permissions: perms,
            rank,
            icon,
            color,
        });
        self.audit(
            Some(actor),
            "role.edit",
            Some(id.to_string()),
            Some(format!(
                "{name}: {}",
                permissions::permission_names(perms).join(", ")
            )),
        );
        self.broadcast_roles();
        for user in self.holders_of(id) {
            self.roles_changed(&user);
        }
        Ok(AdminOutcome::Done)
    }

    fn delete_role(&mut self, actor: UserId, id: u32) -> Result<AdminOutcome, AdminError> {
        let role = self
            .policy
            .roles
            .get(&id)
            .cloned()
            .ok_or(AdminError::NoSuchRole)?;
        if role.builtin {
            return Err(AdminError::Invalid(
                "built-in roles cannot be deleted".to_string(),
            ));
        }
        if role.rank >= self.policy.rank_of(&actor) {
            return Err(AdminError::Outranked);
        }
        let holders = self.holders_of(id);
        for user in &holders {
            if let Some(record) = self.policy.users.get_mut(user) {
                record.roles.remove(&id);
            }
        }
        self.policy.roles.remove(&id);
        // The store revokes the invites that grant it and opens the
        // channels gated on it in the same transaction; mirror that, so
        // a live invite cannot admit someone into a role that is gone.
        let now = self.wall_now();
        for invite in self.policy.invites.values_mut() {
            if invite.grants_role == id {
                invite.revoked_at.get_or_insert(now);
                invite.grants_role = protocol::permissions::ROLE_GUEST;
            }
        }
        let mut gated = false;
        for channel in &mut self.policy.channels {
            if channel.min_role == Some(id) {
                channel.min_role = None;
                gated = true;
            }
        }
        self.persist.send(Command::DeleteRole { id, ts: now });
        self.audit(
            Some(actor),
            "role.delete",
            Some(id.to_string()),
            Some(role.name.clone()),
        );
        info!("role {} ({id}) deleted from the panel", role.name);
        if gated && let Err(e) = self.rebuild_channel_list() {
            warn!("channel list rebuild after a role delete failed: {e}");
        }
        self.broadcast_roles();
        for user in holders {
            self.roles_changed(&user);
        }
        Ok(AdminOutcome::Done)
    }

    /// Everyone holding role `id`.
    fn holders_of(&self, id: u32) -> Vec<UserId> {
        self.policy
            .users
            .iter()
            .filter(|(_, u)| u.roles.contains(&id))
            .map(|(user, _)| *user)
            .collect()
    }

    /// The table to every session that reads it.
    fn broadcast_roles(&mut self) {
        let roles = self.role_list();
        let recipients = self.clients();
        self.send_to(&recipients, &roles);
    }
}

/// The first 8 bytes of a user id, from the 16 hex digits the audit
/// log stores as a target; `None` for anything else.
fn short_id(target: &str) -> Option<[u8; 8]> {
    let digits = target.as_bytes();
    if digits.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *byte = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod role_list_tests {
    use protocol::{RoleInfo, ServerMessage};

    use super::MAX_ROLES;

    #[test]
    fn the_most_roles_fit_one_role_list() {
        let roles = (0..MAX_ROLES)
            .map(|i| RoleInfo {
                id: u32::MAX - u32::try_from(i).unwrap_or(0),
                name: "r".repeat(32),
                rank: u16::MAX,
                permissions: u64::MAX,
                builtin: false,
                icon: "gamepad".into(),
                color: u32::MAX,
            })
            .collect();
        let len = protocol::encode_server_message(&ServerMessage::RoleList { roles })
            .map_or(usize::MAX, |b| b.len());
        assert!(len <= protocol::LIST_ITEMS_BUDGET, "{len} bytes");
    }
}

#[cfg(test)]
mod short_id_tests {
    use super::short_id;
    use crate::persistence::hex;

    #[test]
    fn a_short_id_is_what_the_audit_log_wrote() {
        let id = [0xab, 0x01, 0xff, 0x00, 0x7e, 0x10, 0x9c, 0x33];
        assert_eq!(short_id(&hex(&id)), Some(id));
        assert_eq!(short_id("abc"), None);
        assert_eq!(short_id("zz01ff007e109c33"), None);
        assert_eq!(short_id("é01ff007e109c33"), None);
    }
}
