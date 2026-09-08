//! Moderation and administration on the tick (spec §4.1, §4.2, §8).
//!
//! Every `AdminCommand` is checked here like any other message: the
//! actor's effective permissions, then the rank rule (nobody acts on an
//! equal or higher rank, nobody grants a rank above their own, the last
//! admin keeps `admin`), then the in-memory change, then the rows and
//! the audit line. Enforcement helpers live here too: what a session
//! may do is its roles' union minus active restrictions, computed on
//! control messages and cached only where the media path needs it
//! (the relay's `muted` set).

use std::collections::HashSet;

use log::info;
use protocol::{
    admin::{
        AdminCommand, AdminError, AdminOutcome, AdminTarget, InviteSummary, KeySummary,
        RestrictionSummary, UserSummary,
    },
    features,
    permissions::{self, RoleId},
    ClientId, RemovedKind, RestrictionKind, RoleInfo, ServerMessage, UserId,
};

use super::{hex, ChatServer};
use crate::{
    persistence::Command,
    policy::{KeyRecord, Restriction},
};

/// What a restriction of each kind subtracts (spec §4.2).
fn permission_removed_by(kind: RestrictionKind) -> u64 {
    match kind {
        RestrictionKind::Server => permissions::JOIN_CHANNELS,
        RestrictionKind::Voice => permissions::SPEAK,
        RestrictionKind::GeneralText => permissions::TEXT_GENERAL,
        RestrictionKind::Dm => permissions::DM,
        RestrictionKind::Upload => permissions::UPLOAD,
        RestrictionKind::Share => permissions::SHARE,
        RestrictionKind::Profile => 0,
    }
}

impl ChatServer {
    // ── Enforcement ─────────────────────────────────────────────────

    /// A user's roles unioned, minus what active restrictions take
    /// away (spec §4.2).
    pub(super) fn effective_permissions(&self, user: &UserId) -> u64 {
        let now = self.wall_now();
        let mut perms = self.policy.permissions_of(user);
        for r in &self.policy.restrictions {
            if r.active(now) && r.user_id.as_ref() == Some(user) {
                perms &= !permission_removed_by(r.kind);
            }
        }
        perms
    }

    /// Whether the session holds `bit` right now.
    pub(super) fn has_permission(&self, client: ClientId, bit: u64) -> bool {
        self.user_of
            .get(&client)
            .is_some_and(|user| permissions::has(self.effective_permissions(user), bit))
    }

    /// `has_permission`, and when it fails the session is told why
    /// (`Restricted`), so the UI shows the reason instead of a silent
    /// failure. `kind` is the restriction that would explain it.
    pub(super) fn allowed(&mut self, client: ClientId, bit: u64, kind: RestrictionKind) -> bool {
        if self.has_permission(client, bit) {
            return true;
        }
        let Some(user) = self.user_of.get(&client).copied() else {
            return false;
        };
        let now = self.wall_now();
        let (until, reason) = self
            .policy
            .restriction_on(&user, kind, now)
            .map_or((None, String::new()), |r| (r.until, r.reason.clone()));
        info!(
            "{} ({client:#x}) refused: no {} ({})",
            self.get_username(client),
            kind.name(),
            if reason.is_empty() {
                "not in their roles"
            } else {
                reason.as_str()
            }
        );
        self.send_if_roles(
            client,
            &ServerMessage::Restricted {
                kind,
                until,
                reason,
            },
        );
        false
    }

    /// Recompute the relay's `muted` set from the policy: sessions with
    /// no `SPEAK` right now. Called after every policy change.
    pub(super) fn refresh_enforcement(&mut self) {
        let muted: HashSet<ClientId> = self
            .user_of
            .iter()
            .filter(|(_, user)| {
                !permissions::has(self.effective_permissions(user), permissions::SPEAK)
            })
            .map(|(client, _)| *client)
            .collect();
        self.muted = muted;
    }

    /// Once a second: drop restrictions past their `until`, tell the
    /// sessions they applied to, refresh the relay's set.
    pub(super) fn sweep_restrictions(&mut self) {
        let now = self.wall_now();
        let expired: Vec<Restriction> = self
            .policy
            .restrictions
            .iter()
            .filter(|r| !r.active(now))
            .cloned()
            .collect();
        if expired.is_empty() {
            return;
        }
        self.policy.expire_restrictions(now);
        for r in expired {
            info!("restriction {} ({}) ran out", r.id, r.kind.name());
            self.persist.send(Command::DeleteRestriction { id: r.id });
            if let Some(user) = r.user_id
                && r.kind != RestrictionKind::Server
            {
                let msg = ServerMessage::RestrictionLifted { kind: r.kind };
                for client in self.sessions_of(&user) {
                    self.send_if_roles(client, &msg);
                }
            }
        }
        self.refresh_enforcement();
    }

    /// After the persistence thread handed over a policy another
    /// process changed: bans land on live sessions, mutes take effect,
    /// roles refresh in every roster.
    pub(super) fn apply_reloaded_policy(&mut self) {
        let now = self.wall_now();
        let hit: Vec<(ClientId, Restriction)> = self
            .user_of
            .iter()
            .filter_map(|(client, user)| {
                let key = self.key_of.get(client)?;
                let ip = self.driver.remote_address(*client).map(|a| a.ip());
                self.policy
                    .restrictions
                    .iter()
                    .find(|r| {
                        r.kind == RestrictionKind::Server
                            && r.active(now)
                            && (r.user_id.as_ref() == Some(user)
                                || r.key_hash.as_ref() == Some(key)
                                || (ip.is_some() && r.ip == ip))
                    })
                    .map(|r| (*client, r.clone()))
            })
            .collect();
        for (client, r) in hit {
            self.remove_session(
                client,
                RemovedKind::Banned { until: r.until },
                &r.reason,
                "server",
            );
        }
        // A kick from the CLI shows up as the session's key no longer
        // belonging to its user.
        let kicked: Vec<ClientId> = self
            .user_of
            .iter()
            .filter(|(client, user)| {
                self.key_of
                    .get(*client)
                    .is_none_or(|key| self.policy.keys.get(key).map(|k| k.user_id) != Some(**user))
            })
            .map(|(client, _)| *client)
            .collect();
        for client in kicked {
            self.remove_session(client, RemovedKind::Kicked, "access removed", "server");
        }
        self.refresh_enforcement();
        let details: Vec<ServerMessage> = self
            .user_of
            .iter()
            .map(|(c, u)| self.user_details(*c, u))
            .collect();
        for msg in details {
            for peer in self.clients() {
                self.send_if_roles(peer, &msg);
            }
        }
        // Each session's own permission set, so its gates follow a
        // `server user grant` without a reconnect.
        let persistent = self.persist.is_persistent();
        let sessions: Vec<(ClientId, ServerMessage)> = self
            .user_of
            .iter()
            .map(|(c, u)| {
                (
                    *c,
                    ServerMessage::Session {
                        user_id: *u,
                        permissions: self.effective_permissions(u),
                        persistent,
                    },
                )
            })
            .collect();
        for (client, session) in sessions {
            self.send_if_roles(client, &session);
        }
        // The table itself may have changed (`server role`).
        let roles = self.role_list();
        for peer in self.clients() {
            self.send_if(peer, features::ROLE_LIST, &roles);
        }
    }

    /// The roster line for a session (spec §8): who it is and its roles.
    pub(super) fn user_details(&self, client: ClientId, user: &UserId) -> ServerMessage {
        ServerMessage::UserDetails {
            client_id: client,
            user_id: *user,
            roles: self
                .policy
                .users
                .get(user)
                .map(|u| u.roles.iter().copied().collect())
                .unwrap_or_default(),
            level: self.policy.level_of(user),
        }
    }

    /// Send only to a session that negotiated `ROLES`.
    /// The role table, in rank order (spec §4), for clients that name
    /// roles by id.
    pub(super) fn role_list(&self) -> ServerMessage {
        let mut roles: Vec<RoleInfo> = self
            .policy
            .roles
            .iter()
            .map(|(id, r)| RoleInfo {
                id: *id,
                name: r.name.clone(),
                rank: r.rank,
                permissions: r.permissions,
                builtin: r.builtin,
                icon: r.icon.clone(),
                color: r.color,
            })
            .collect();
        roles.sort_by_key(|r| (r.rank, r.id));
        ServerMessage::RoleList { roles }
    }

    /// Where a session's user stands (spec §9.1): the xp behind the
    /// level `UserDetails` carries, for sessions that render the bar.
    pub(super) fn user_progress(&self, client: ClientId, user: &UserId) -> ServerMessage {
        ServerMessage::UserProgress {
            client_id: client,
            xp: self.policy.xp.get(user).copied().unwrap_or(0),
            level: self.policy.level_of(user),
        }
    }

    /// A role's name for an audit line (custom ones included).
    pub(super) fn role_name_for_audit(&self, role: RoleId) -> String {
        self.policy
            .roles
            .get(&role)
            .map_or_else(|| format!("role {role}"), |r| r.name.clone())
    }

    /// A channel's name for an audit line, or its id.
    pub(super) fn channel_name_for_audit(&self, channel_id: u16) -> String {
        self.channel_infos
            .iter()
            .find(|c| c.id == channel_id)
            .map_or_else(|| format!("channel {channel_id}"), |c| c.name.clone())
    }

    pub(super) fn send_if_roles(&mut self, client: ClientId, msg: &ServerMessage) {
        if self
            .features_of
            .get(&client)
            .is_some_and(|f| f & features::ROLES != 0)
        {
            self.send_msg(client, msg);
        }
    }

    /// Live sessions of a user (normally one; a replacement in flight
    /// can make it two for a moment).
    pub(super) fn sessions_of(&self, user: &UserId) -> Vec<ClientId> {
        self.user_of
            .iter()
            .filter(|(_, u)| *u == user)
            .map(|(c, _)| *c)
            .collect()
    }

    /// The display name for `by` fields: the actor's session name, or
    /// their name of record, or `server`.
    fn actor_name(&self, actor: Option<UserId>) -> String {
        actor
            .and_then(|a| {
                self.sessions_of(&a)
                    .first()
                    .map(|c| self.get_username(*c))
                    .or_else(|| self.policy.users.get(&a).map(|u| u.name.clone()))
            })
            .unwrap_or_else(|| "server".to_string())
    }

    // ── Admin commands ──────────────────────────────────────────────

    /// Run one command for the session that sent it (spec §8). One
    /// arm per command: long, but flat.
    ///
    /// # Errors
    /// Why it was refused; the client shows the line.
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_admin(
        &mut self,
        client: ClientId,
        command: AdminCommand,
    ) -> Result<AdminOutcome, AdminError> {
        let Some(actor) = self.user_of.get(&client).copied() else {
            return Err(AdminError::NoPermission);
        };
        let perms = self.effective_permissions(&actor);
        let need = |bit: u64| {
            if permissions::has(perms, bit) {
                Ok(())
            } else {
                Err(AdminError::NoPermission)
            }
        };
        let actor_name = self.get_username(client);
        info!("{actor_name} ({client:#x}) admin: {command:?}");
        if let Some(result) = self.handle_panel_command(actor, perms, &command) {
            return result;
        }
        match command {
            AdminCommand::ListUsers => {
                need(permissions::MANAGE_USERS)?;
                Ok(AdminOutcome::Users(self.list_users()))
            }
            AdminCommand::ListRestrictions => {
                if !permissions::has(perms, permissions::RESTRICT)
                    && !permissions::has(perms, permissions::MUTE)
                {
                    return Err(AdminError::NoPermission);
                }
                Ok(AdminOutcome::Restrictions(self.list_restrictions()))
            }
            AdminCommand::ListInvites => {
                need(permissions::MANAGE_INVITES)?;
                Ok(AdminOutcome::Invites(self.list_invites()))
            }
            AdminCommand::SetRoles { target, roles } => {
                need(permissions::MANAGE_ROLES)?;
                let user_id = self.resolve_target(&target)?;
                self.set_roles(actor, &user_id, &roles)?;
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Restrict {
                target,
                kind,
                duration_secs,
                reason,
            } => {
                if kind == RestrictionKind::Voice {
                    if !permissions::has(perms, permissions::MUTE)
                        && !permissions::has(perms, permissions::RESTRICT)
                    {
                        return Err(AdminError::NoPermission);
                    }
                } else {
                    need(permissions::RESTRICT)?;
                }
                let (user_id, key_hash, ip) = match target {
                    AdminTarget::User(_) | AdminTarget::Name(_) => {
                        let user = self.resolve_target(&target)?;
                        self.check_target(actor, &user)?;
                        (Some(user), None, None)
                    }
                    AdminTarget::Key(key) => {
                        if let Some(owner) = self.policy.keys.get(&key).map(|k| k.user_id) {
                            self.check_target(actor, &owner)?;
                        }
                        (None, Some(key), None)
                    }
                    AdminTarget::Address(text) => {
                        let ip = text.trim().parse().map_err(|_| {
                            AdminError::Invalid(format!("'{text}' is not an address"))
                        })?;
                        (None, None, Some(ip))
                    }
                };
                let now = self.wall_now();
                let by_name = actor_name.clone();
                self.restrict(
                    Restriction {
                        id: 0,
                        user_id,
                        key_hash,
                        ip,
                        kind,
                        until: duration_secs.map(|s| now.saturating_add(s.saturating_mul(1000))),
                        reason,
                        by: Some(actor),
                        created_at: now,
                    },
                    &by_name,
                );
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Lift { restriction_id } => {
                let Some(r) = self
                    .policy
                    .restrictions
                    .iter()
                    .find(|r| r.id == restriction_id)
                else {
                    return Err(AdminError::NoSuchRestriction);
                };
                if r.kind == RestrictionKind::Voice {
                    if !permissions::has(perms, permissions::MUTE)
                        && !permissions::has(perms, permissions::RESTRICT)
                    {
                        return Err(AdminError::NoPermission);
                    }
                } else {
                    need(permissions::RESTRICT)?;
                }
                self.lift_restriction(restriction_id, Some(actor));
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Unmute { target, kind } => {
                if kind == RestrictionKind::Voice {
                    if !permissions::has(perms, permissions::MUTE)
                        && !permissions::has(perms, permissions::RESTRICT)
                    {
                        return Err(AdminError::NoPermission);
                    }
                } else {
                    need(permissions::RESTRICT)?;
                }
                let user_id = self.resolve_target(&target)?;
                if self.lift_all(&user_id, kind, actor) == 0 {
                    return Err(AdminError::NoSuchRestriction);
                }
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Disconnect { target, reason } => {
                need(permissions::DISCONNECT)?;
                let user_id = self.resolve_target(&target)?;
                self.check_target(actor, &user_id)?;
                let sessions = self.sessions_of(&user_id);
                if sessions.is_empty() {
                    return Err(AdminError::NoSuchUser);
                }
                for session in sessions {
                    self.remove_session(session, RemovedKind::Disconnected, &reason, &actor_name);
                }
                self.audit(
                    Some(actor),
                    "user.disconnect",
                    Some(hex(&user_id[..8])),
                    Some(reason),
                );
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Kick { target, reason } => {
                need(permissions::KICK)?;
                let user_id = self.resolve_target(&target)?;
                self.check_target(actor, &user_id)?;
                self.kick_user(actor, &actor_name, &user_id, &reason);
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Ban {
                target,
                duration_secs,
                reason,
            } => {
                need(permissions::BAN)?;
                let user_id = self.resolve_target(&target)?;
                self.check_target(actor, &user_id)?;
                let now = self.wall_now();
                // Block what is known about the person: the user, the
                // key of the live session (or their latest key), and the
                // address they are on — unless someone else is on that
                // address right now (a shared machine, a household
                // behind one NAT), in which case the address is left
                // alone and the log says so.
                let session = self.sessions_of(&user_id).first().copied();
                let key_hash = session
                    .and_then(|c| self.key_of.get(&c).copied())
                    .or_else(|| self.latest_key_of(&user_id));
                let ip = session
                    .and_then(|c| self.driver.remote_address(c))
                    .map(|a| a.ip());
                let shared = ip.is_some_and(|ip| {
                    self.user_of.iter().any(|(client, user)| {
                        *user != user_id
                            && self.driver.remote_address(*client).map(|a| a.ip()) == Some(ip)
                    })
                });
                let ip = if shared {
                    info!(
                        "ban of {}: address {} is shared with another session; not banning it",
                        hex(&user_id[..8]),
                        ip.map_or_else(|| "?".to_string(), |ip| ip.to_string())
                    );
                    None
                } else {
                    ip
                };
                self.restrict(
                    Restriction {
                        id: 0,
                        user_id: Some(user_id),
                        key_hash,
                        ip,
                        kind: RestrictionKind::Server,
                        until: duration_secs.map(|s| now.saturating_add(s.saturating_mul(1000))),
                        reason,
                        by: Some(actor),
                        created_at: now,
                    },
                    &actor_name,
                );
                Ok(AdminOutcome::Done)
            }
            AdminCommand::Unban { target } => {
                need(permissions::BAN)?;
                let user_id = self.resolve_target(&target)?;
                if self.lift_all(&user_id, RestrictionKind::Server, actor) == 0 {
                    return Err(AdminError::NoSuchRestriction);
                }
                Ok(AdminOutcome::Done)
            }
            AdminCommand::MoveUser { target, channel_id } => {
                need(permissions::MANAGE_CHANNELS)?;
                if usize::from(channel_id) >= self.channel_list.len() {
                    return Err(AdminError::NoSuchChannel);
                }
                let user_id = self.resolve_target(&target)?;
                let sessions = self.sessions_of(&user_id);
                if sessions.is_empty() {
                    return Err(AdminError::NoSuchUser);
                }
                for session in sessions {
                    self.place_in_channel(session, channel_id);
                }
                self.audit(
                    Some(actor),
                    "user.move",
                    Some(hex(&user_id[..8])),
                    Some(format!("to {}", self.channel_name_for_audit(channel_id))),
                );
                Ok(AdminOutcome::Done)
            }
            AdminCommand::RevokeInvite { code } => {
                need(permissions::MANAGE_INVITES)?;
                let code = protocol::invite::normalize_code(&code)
                    .ok_or_else(|| AdminError::Invalid("not an invite code".to_string()))?;
                if self.revoke_invite(&code, Some(actor)) {
                    Ok(AdminOutcome::Done)
                } else {
                    Err(AdminError::NoSuchInvite)
                }
            }
            AdminCommand::LinkKey { target, key_hash } => {
                need(permissions::MANAGE_USERS)?;
                let user_id = self.resolve_target(&target)?;
                self.link_key(actor, &user_id, &key_hash)?;
                Ok(AdminOutcome::Done)
            }
            AdminCommand::UnlinkKey { key_hash } => {
                need(permissions::MANAGE_USERS)?;
                if let Some(owner) = self.policy.keys.get(&key_hash).map(|k| k.user_id) {
                    self.check_target(actor, &owner)?;
                }
                if self.unlink_key(&key_hash) {
                    Ok(AdminOutcome::Done)
                } else {
                    Err(AdminError::NoSuchUser)
                }
            }
            AdminCommand::DeleteMessage { msg_id } => {
                need(permissions::RESTRICT)?;
                if self.handle_delete_message(client, msg_id, true) {
                    self.audit(
                        Some(actor),
                        "message.delete",
                        Some(msg_id.to_string()),
                        None,
                    );
                    Ok(AdminOutcome::Done)
                } else {
                    Err(AdminError::Invalid(
                        "no such recent message (older ones cannot be deleted)".to_string(),
                    ))
                }
            }
            AdminCommand::PurgeUser { target } => {
                need(permissions::MANAGE_USERS)?;
                let user_id = self.resolve_target(&target)?;
                self.check_target(actor, &user_id)?;
                self.purge_user(actor, &user_id);
                Ok(AdminOutcome::Done)
            }
            AdminCommand::LastTelemetry { target } => {
                need(permissions::MANAGE_USERS)?;
                let user_id = self.resolve_target(&target)?;
                Ok(AdminOutcome::Telemetry(
                    self.last_telemetry.get(&user_id).map(|(ts, line)| {
                        protocol::TelemetrySummary {
                            ts: *ts,
                            line: line.clone(),
                        }
                    }),
                ))
            }
            AdminCommand::ResetProfile { target, lock } => {
                need(permissions::RESTRICT)?;
                let user_id = self.resolve_target(&target)?;
                self.check_target(actor, &user_id)?;
                self.reset_profile(actor, &user_id, lock, &actor_name);
                Ok(AdminOutcome::Done)
            }
            // The panel's commands answered above.
            AdminCommand::AuditLog { .. }
            | AdminCommand::CreateRole { .. }
            | AdminCommand::EditRole { .. }
            | AdminCommand::DeleteRole { .. }
            | AdminCommand::ListBlobs
            | AdminCommand::DeleteBlob { .. }
            | AdminCommand::ServerStats => Err(AdminError::Invalid("unreachable".into())),
        }
    }

    /// The user a target names. Ids are exact; names are the name of
    /// record, case-insensitive; a hex prefix of at least 8 digits
    /// matches an id. When several users share a name, an online one
    /// wins if there is exactly one; otherwise the candidates are
    /// listed and the id is the way to say which.
    fn resolve_target(&self, target: &AdminTarget) -> Result<UserId, AdminError> {
        let name = match target {
            AdminTarget::User(id) => {
                return if self.policy.users.contains_key(id) {
                    Ok(*id)
                } else {
                    Err(AdminError::NoSuchUser)
                };
            }
            AdminTarget::Name(name) => name.trim(),
            AdminTarget::Key(_) | AdminTarget::Address(_) => {
                return Err(AdminError::Invalid("expected a user".to_string()));
            }
        };
        let lower = name.to_ascii_lowercase();
        let looks_hex = lower.len() >= 8 && lower.bytes().all(|b| b.is_ascii_hexdigit());
        let mut matches: Vec<UserId> = if looks_hex {
            self.policy
                .users
                .keys()
                .filter(|id| hex(&id[..]).starts_with(&lower))
                .copied()
                .collect()
        } else {
            Vec::new()
        };
        if matches.is_empty() {
            matches = self
                .policy
                .users
                .iter()
                .filter(|(_, u)| u.name.eq_ignore_ascii_case(name))
                .map(|(id, _)| *id)
                .collect();
        }
        if matches.len() > 1 {
            let online: Vec<UserId> = matches
                .iter()
                .filter(|id| !self.sessions_of(id).is_empty())
                .copied()
                .collect();
            if online.len() == 1 {
                return Ok(online[0]);
            }
        }
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => Err(AdminError::NoSuchUser),
            many => {
                let mut ids: Vec<String> = many
                    .iter()
                    .map(|id| {
                        let roles: Vec<&str> = self
                            .policy
                            .users
                            .get(id)
                            .map(|u| {
                                u.roles
                                    .iter()
                                    .filter_map(|r| self.policy.roles.get(r))
                                    .map(|r| r.name.as_str())
                                    .collect()
                            })
                            .unwrap_or_default();
                        format!(
                            "{} ({})",
                            hex(&id[..8]),
                            if roles.is_empty() {
                                "no roles".to_string()
                            } else {
                                roles.join(",")
                            }
                        )
                    })
                    .collect();
                ids.sort();
                Err(AdminError::Invalid(format!(
                    "'{name}' names {} users — use the id (first 8 digits): {}",
                    many.len(),
                    ids.join(", ")
                )))
            }
        }
    }

    /// Kick (spec §4.1 as decided): close the sessions, forget the
    /// device keys and the roles. The user row stays (history keeps
    /// its author); the next connection is an unknown device that
    /// needs a new invite.
    fn kick_user(&mut self, actor: UserId, actor_name: &str, user_id: &UserId, reason: &str) {
        for session in self.sessions_of(user_id) {
            self.remove_session(session, RemovedKind::Kicked, reason, actor_name);
        }
        self.policy.keys.retain(|_, k| k.user_id != *user_id);
        self.persist
            .send(Command::DeleteUserKeys { user_id: *user_id });
        let roles: Vec<RoleId> = self
            .policy
            .users
            .get(user_id)
            .map(|u| u.roles.iter().copied().collect())
            .unwrap_or_default();
        if let Some(u) = self.policy.users.get_mut(user_id) {
            u.roles.clear();
        }
        for role in roles {
            self.persist.send(Command::RevokeRole {
                user_id: *user_id,
                role_id: role,
            });
        }
        self.audit(
            Some(actor),
            "user.kick",
            Some(hex(&user_id[..8])),
            Some(reason.to_string()),
        );
        self.refresh_enforcement();
    }

    /// Remove every restriction of `kind` on a user. Returns how many.
    fn lift_all(&mut self, user_id: &UserId, kind: RestrictionKind, actor: UserId) -> usize {
        let ids: Vec<u64> = self
            .policy
            .restrictions
            .iter()
            .filter(|r| r.kind == kind && r.user_id.as_ref() == Some(user_id))
            .map(|r| r.id)
            .collect();
        for id in &ids {
            self.lift_restriction(*id, Some(actor));
        }
        ids.len()
    }

    /// The rank rule (spec §8): the target must exist, must not be the
    /// actor, and must rank below the actor.
    fn check_target(&self, actor: UserId, target: &UserId) -> Result<(), AdminError> {
        if !self.policy.users.contains_key(target) {
            return Err(AdminError::NoSuchUser);
        }
        if *target == actor {
            return Err(AdminError::Yourself);
        }
        if self.policy.rank_of(target) >= self.policy.rank_of(&actor) {
            return Err(AdminError::Outranked);
        }
        Ok(())
    }

    /// Replace a user's role set. Every role must exist and rank below
    /// the actor; the last admin keeps `admin`. Public for the tests
    /// and the CLI path.
    ///
    /// # Errors
    /// The rank rule, an unknown role or user, or the last admin.
    pub fn set_roles(
        &mut self,
        actor: UserId,
        user_id: &UserId,
        roles: &[RoleId],
    ) -> Result<(), AdminError> {
        if !self.policy.users.contains_key(user_id) {
            return Err(AdminError::NoSuchUser);
        }
        let actor_rank = self.policy.rank_of(&actor);
        // Equal rank is fine here — admins manage each other's roles,
        // and their own — but never someone above you.
        if self.policy.rank_of(user_id) > actor_rank {
            return Err(AdminError::Outranked);
        }
        for role in roles {
            let Some(r) = self.policy.roles.get(role) else {
                return Err(AdminError::NoSuchRole);
            };
            if r.rank > actor_rank {
                return Err(AdminError::Outranked);
            }
        }
        let new_perms = roles
            .iter()
            .filter_map(|r| self.policy.roles.get(r))
            .fold(0, |acc, r| acc | r.permissions);
        let was_admin = permissions::has(self.policy.permissions_of(user_id), permissions::ADMIN);
        if was_admin && !permissions::has(new_perms, permissions::ADMIN) {
            let other_admins = self
                .policy
                .users
                .keys()
                .filter(|u| *u != user_id)
                .any(|u| permissions::has(self.policy.permissions_of(u), permissions::ADMIN));
            if !other_admins {
                return Err(AdminError::LastAdmin);
            }
        }

        let before: Vec<RoleId> = self
            .policy
            .users
            .get(user_id)
            .map(|u| u.roles.iter().copied().collect())
            .unwrap_or_default();
        let now = self.wall_now();
        for role in &before {
            if !roles.contains(role) {
                if let Some(u) = self.policy.users.get_mut(user_id) {
                    u.roles.remove(role);
                }
                self.persist.send(Command::RevokeRole {
                    user_id: *user_id,
                    role_id: *role,
                });
            }
        }
        for role in roles {
            if !before.contains(role) {
                self.policy.grant_role(user_id, *role);
                self.persist.send(Command::GrantRole {
                    user_id: *user_id,
                    role_id: *role,
                    granted_by: Some(actor),
                    ts: now,
                });
            }
        }
        self.audit(
            Some(actor),
            "user.roles",
            Some(hex(&user_id[..8])),
            Some(if roles.is_empty() {
                "no roles".to_string()
            } else {
                roles
                    .iter()
                    .map(|r| self.role_name_for_audit(*r))
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        );
        self.roles_changed(user_id);
        Ok(())
    }

    /// Attach a device key to a user: a retired key comes back, an
    /// unknown hash is claimed. A key on someone else is moved only if
    /// the actor outranks them.
    ///
    /// # Errors
    /// Unknown user, or the key's current owner outranks the actor.
    pub fn link_key(
        &mut self,
        actor: UserId,
        user_id: &UserId,
        key_hash: &UserId,
    ) -> Result<(), AdminError> {
        if !self.policy.users.contains_key(user_id) {
            return Err(AdminError::NoSuchUser);
        }
        if let Some(owner) = self.policy.keys.get(key_hash).map(|k| k.user_id)
            && owner != *user_id
        {
            self.check_target(actor, &owner)?;
        }
        let now = self.wall_now();
        let added_at = self.policy.keys.get(key_hash).map_or(now, |k| k.added_at);
        self.policy.keys.insert(
            *key_hash,
            KeyRecord {
                user_id: *user_id,
                added_at,
                last_seen: now,
                retired_at: None,
            },
        );
        self.persist_key(key_hash);
        self.audit(
            Some(actor),
            "key.link",
            Some(hex(&key_hash[..8])),
            Some(format!("user {}", hex(&user_id[..8]))),
        );
        Ok(())
    }

    /// After a role change: the relay set, the roster lines, and a
    /// fresh `Session` for the user's own sessions so their UI follows.
    pub(super) fn roles_changed(&mut self, user_id: &UserId) {
        self.refresh_enforcement();
        let sessions = self.sessions_of(user_id);
        let perms = self.effective_permissions(user_id);
        let persistent = self.persist.is_persistent();
        for client in &sessions {
            let session = ServerMessage::Session {
                user_id: *user_id,
                permissions: perms,
                persistent,
            };
            self.send_if_roles(*client, &session);
            let details = self.user_details(*client, user_id);
            for peer in self.clients() {
                self.send_if_roles(peer, &details);
            }
        }
    }

    /// The key a user used most recently, for a ban when they are not
    /// online right now.
    fn latest_key_of(&self, user_id: &UserId) -> Option<UserId> {
        self.policy
            .keys
            .iter()
            .filter(|(_, k)| k.user_id == *user_id)
            .max_by_key(|(_, k)| k.last_seen)
            .map(|(hash, _)| *hash)
    }

    fn list_users(&self) -> Vec<UserSummary> {
        let mut users: Vec<UserSummary> = self
            .policy
            .users
            .iter()
            .map(|(id, u)| UserSummary {
                user_id: *id,
                name: u.name.clone(),
                roles: u.roles.iter().copied().collect(),
                client_id: self.sessions_of(id).first().copied(),
                last_seen: u.last_seen,
                keys: self
                    .policy
                    .keys
                    .iter()
                    .filter(|(_, k)| k.user_id == *id)
                    .map(|(hash, k)| KeySummary {
                        key_hash: *hash,
                        retired: k.retired_at.is_some(),
                        last_seen: k.last_seen,
                    })
                    .collect(),
            })
            .collect();
        users.sort_by(|a, b| {
            b.client_id
                .is_some()
                .cmp(&a.client_id.is_some())
                .then_with(|| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                })
        });
        users
    }

    fn list_restrictions(&self) -> Vec<RestrictionSummary> {
        self.policy
            .restrictions
            .iter()
            .map(|r| RestrictionSummary {
                id: r.id,
                user_id: r.user_id,
                user_name: r
                    .user_id
                    .and_then(|u| self.policy.users.get(&u))
                    .map(|u| u.name.clone()),
                key_hash: r.key_hash,
                address: r.ip.map(|ip| ip.to_string()),
                kind: r.kind,
                until: r.until,
                reason: r.reason.clone(),
                by: self.actor_name(r.by),
                created_at: r.created_at,
            })
            .collect()
    }

    fn list_invites(&self) -> Vec<InviteSummary> {
        let mut invites: Vec<InviteSummary> = self
            .policy
            .invites
            .iter()
            .map(|(code, i)| InviteSummary {
                code: code.clone(),
                grants_role: i.grants_role,
                created_by: i
                    .created_by
                    .and_then(|u| self.policy.users.get(&u))
                    .map(|u| u.name.clone()),
                created_at: i.created_at,
                expires_at: i.expires_at,
                max_uses: i.max_uses,
                uses: i.uses,
                revoked: i.revoked_at.is_some(),
            })
            .collect();
        invites.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        invites
    }
}
