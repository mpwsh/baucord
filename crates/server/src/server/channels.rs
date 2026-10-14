//! Channels made from the UI (spec §5), on top of the config ones.
//!
//! The list clients see is the config channels, in file order, then
//! the UI-created ones in creation order; wire ids are list positions,
//! as in 1.0, and `ChannelDetails` carries the stable id a client can
//! key on across renumbering. Every change goes through the same
//! rebuild the config hot-reload uses, so members follow their channel
//! by name and a removed channel's members land in the default.

use std::time::Duration;

use log::{info, warn};
use protocol::{
    features, permissions, ChannelSpec, ChannelUiDenyReason, ClientId, ServerMessage, UserId,
};

use super::{hex, ChatServer};
use crate::{
    config::{validate_channel_list, ChannelSettings, MAX_CHANNELS},
    persistence::Command,
    policy::UserChannelRecord,
};

/// A channel name's length cap; the file has none, the UI needs one.
const MAX_CHANNEL_NAME: usize = 40;

impl ChatServer {
    // ── The list ────────────────────────────────────────────────────

    /// The UI-created channel at wire position `channel_id`, if that
    /// position is one.
    pub(super) fn user_channel_at(&self, channel_id: u16) -> Option<&UserChannelRecord> {
        let index = usize::from(channel_id).checked_sub(self.config.channels.len())?;
        self.policy.channels.get(index)
    }

    fn user_channel_at_mut(&mut self, channel_id: u16) -> Option<&mut UserChannelRecord> {
        let index = usize::from(channel_id).checked_sub(self.config.channels.len())?;
        self.policy.channels.get_mut(index)
    }

    /// The wire position of the config channel idle users are moved to.
    pub(super) fn afk_channel(&self) -> Option<u16> {
        let name = self.config.user_channels.afk_channel.as_deref()?.trim();
        self.config
            .channels
            .iter()
            .position(|c| c.name.trim() == name)
            .and_then(|i| u16::try_from(i).ok())
    }

    /// Recompute the channel list from the config and the UI-created
    /// channels, renumber, move members, announce. A UI channel whose
    /// name collides with a config channel loses: the file owns the
    /// name.
    ///
    /// # Errors
    /// The config channels alone fail validation (nothing changed).
    pub(crate) fn rebuild_channel_list(&mut self) -> Result<(), String> {
        validate_channel_list(&self.config.channels)?;
        let taken: Vec<String> = self
            .config
            .channels
            .iter()
            .map(|c| c.name.trim().to_ascii_lowercase())
            .collect();
        let colliding: Vec<u64> = self
            .policy
            .channels
            .iter()
            .filter(|c| taken.contains(&c.name.trim().to_ascii_lowercase()))
            .map(|c| c.stable_id)
            .collect();
        for id in colliding {
            warn!("UI channel {id} has a config channel's name now; dropping it");
            self.policy.channels.retain(|c| c.stable_id != id);
            self.persist.send(Command::DeleteChannel { stable_id: id });
        }
        while self.config.channels.len() + self.policy.channels.len() > MAX_CHANNELS {
            let Some(dropped) = self.policy.channels.pop() else {
                break;
            };
            warn!("too many channels; dropping UI channel '{}'", dropped.name);
            self.persist.send(Command::DeleteChannel {
                stable_id: dropped.stable_id,
            });
        }
        let mut channels = self.config.channels.clone();
        channels.extend(self.policy.channels.iter().map(|c| ChannelSettings {
            name: c.name.clone(),
            default: false,
            password: c.password.clone(),
            user_limit: c.user_limit,
        }));
        self.apply_channel_list(channels)?;
        // The same messages for everyone: built and encoded once each.
        let mut recipients = self.clients();
        recipients.retain(|client| self.has_feature(*client, features::CHANNELS_UI));
        for msg in self.channel_details() {
            self.send_to(&recipients, &msg);
        }
        Ok(())
    }

    /// `ChannelDetails` for every channel, to one session (after
    /// `Init`).
    pub(super) fn send_channel_details(&mut self, client: ClientId) {
        if !self.has_feature(client, features::CHANNELS_UI) {
            return;
        }
        for msg in self.channel_details() {
            self.send_msg(client, &msg);
        }
    }

    /// One `ChannelDetails` per channel, as they stand.
    fn channel_details(&self) -> Vec<ServerMessage> {
        let afk = self.afk_channel();
        let config_len = self.config.channels.len();
        let mut details = Vec::with_capacity(self.channel_list.len());
        for (i, _) in self.channel_list.iter().enumerate() {
            let Ok(channel_id) = u16::try_from(i) else {
                break;
            };
            let user = i
                .checked_sub(config_len)
                .and_then(|k| self.policy.channels.get(k));
            details.push(ServerMessage::ChannelDetails {
                channel_id,
                stable_id: user.map_or(0, |c| c.stable_id),
                owner: user.and_then(|c| c.owner),
                min_role: user.and_then(|c| c.min_role),
                permanent: user.is_none_or(|c| c.permanent),
                afk_after_min: user.and_then(|c| c.afk_after_min),
                is_afk: afk == Some(channel_id),
            });
        }
        details
    }

    // ── Gates at join ───────────────────────────────────────────────

    /// A `min_role` gate on the channel a session wants to join
    /// (spec §5): joinable from that role's rank up.
    pub(super) fn join_gate_allows(&self, client: ClientId, channel_id: u16) -> bool {
        let Some(min_role) = self.user_channel_at(channel_id).and_then(|c| c.min_role) else {
            return true;
        };
        let Some(user) = self.user_of.get(&client) else {
            return false;
        };
        let needed = self.policy.roles.get(&min_role).map_or(0, |r| r.rank);
        self.policy.rank_of(user) >= needed
    }

    // ── Create / edit / delete / kick ───────────────────────────────

    pub(super) fn handle_create_channel(&mut self, client: ClientId, spec: ChannelSpec) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        let perms = self.effective_permissions(&user);
        if !permissions::has(perms, permissions::CREATE_CHANNELS) {
            self.deny_channel(client, ChannelUiDenyReason::NoPermission);
            return;
        }
        let owned = self
            .policy
            .channels
            .iter()
            .filter(|c| c.owner == Some(user))
            .count();
        if owned >= usize::from(self.config.user_channels.max_per_user)
            && !permissions::has(perms, permissions::BYPASS_LIMITS)
        {
            self.deny_channel(client, ChannelUiDenyReason::Limit);
            return;
        }
        let Some(spec) = self.check_spec(client, spec, None) else {
            return;
        };
        let now = self.wall_now();
        let stable_id = self.policy.next_channel_id;
        self.policy.next_channel_id += 1;
        let record = UserChannelRecord {
            stable_id,
            name: spec.name,
            password: spec.password,
            user_limit: spec.user_limit,
            min_role: spec.min_role,
            permanent: spec.permanent,
            afk_after_min: spec.afk_after_min,
            owner: Some(user),
            created_at: now,
            empty_since: Some(now),
        };
        info!(
            "{} ({client:#x}) created channel '{}' (#{stable_id}{})",
            self.get_username(client),
            record.name,
            if record.permanent { ", permanent" } else { "" }
        );
        self.persist.send(Command::UpsertChannel(record.clone()));
        self.policy.channels.push(record);
        self.audit(
            Some(user),
            "channel.create",
            Some(stable_id.to_string()),
            None,
        );
        if let Err(e) = self.rebuild_channel_list() {
            warn!("channel list rebuild after create failed: {e}");
        }
        // The creator goes straight in.
        if let Some(pos) = self.wire_position_of(stable_id) {
            self.place_in_channel(client, pos);
        }
    }

    pub(super) fn handle_edit_channel(
        &mut self,
        client: ClientId,
        channel_id: u16,
        spec: ChannelSpec,
    ) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        if usize::from(channel_id) >= self.channel_list.len() {
            self.deny_channel(client, ChannelUiDenyReason::NoSuchChannel);
            return;
        }
        let Some(existing) = self.user_channel_at(channel_id).cloned() else {
            // Config channels belong to the file.
            self.deny_channel(client, ChannelUiDenyReason::ConfigOwned);
            return;
        };
        if !self.may_manage(&user, &existing) {
            self.deny_channel(client, ChannelUiDenyReason::NoPermission);
            return;
        }
        let Some(spec) = self.check_spec(client, spec, Some(existing.stable_id)) else {
            return;
        };
        if let Some(c) = self.user_channel_at_mut(channel_id) {
            c.name = spec.name;
            c.password = spec.password;
            c.user_limit = spec.user_limit;
            c.min_role = spec.min_role;
            c.permanent = spec.permanent;
            c.afk_after_min = spec.afk_after_min;
        }
        let updated = self.user_channel_at(channel_id).cloned();
        if let Some(record) = updated {
            info!(
                "{} ({client:#x}) edited channel '{}' (#{})",
                self.get_username(client),
                record.name,
                record.stable_id
            );
            self.persist.send(Command::UpsertChannel(record.clone()));
            self.audit(
                Some(user),
                "channel.edit",
                Some(record.stable_id.to_string()),
                None,
            );
        }
        if let Err(e) = self.rebuild_channel_list() {
            warn!("channel list rebuild after edit failed: {e}");
        }
    }

    pub(super) fn handle_delete_channel(&mut self, client: ClientId, channel_id: u16) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        if usize::from(channel_id) >= self.channel_list.len() {
            self.deny_channel(client, ChannelUiDenyReason::NoSuchChannel);
            return;
        }
        let Some(existing) = self.user_channel_at(channel_id).cloned() else {
            self.deny_channel(client, ChannelUiDenyReason::ConfigOwned);
            return;
        };
        if !self.may_manage(&user, &existing) {
            self.deny_channel(client, ChannelUiDenyReason::NoPermission);
            return;
        }
        info!(
            "{} ({client:#x}) deleted channel '{}' (#{})",
            self.get_username(client),
            existing.name,
            existing.stable_id
        );
        self.delete_user_channel(existing.stable_id);
        self.audit(
            Some(user),
            "channel.delete",
            Some(existing.stable_id.to_string()),
            None,
        );
    }

    /// Send a member of a channel back to the default (spec §5: the
    /// owner's power; not a server kick).
    pub(super) fn handle_channel_kick(
        &mut self,
        client: ClientId,
        channel_id: u16,
        target: UserId,
    ) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        if usize::from(channel_id) >= self.channel_list.len() {
            self.deny_channel(client, ChannelUiDenyReason::NoSuchChannel);
            return;
        }
        let owner_here = self
            .user_channel_at(channel_id)
            .is_some_and(|c| c.owner == Some(user));
        let perms = self.effective_permissions(&user);
        let may_manage = permissions::has(perms, permissions::MANAGE_CHANNELS)
            || (owner_here && permissions::has(perms, permissions::MANAGE_OWN_CHANNELS));
        if !may_manage {
            self.deny_channel(client, ChannelUiDenyReason::NoPermission);
            return;
        }
        let default = self.default_channel;
        for session in self.sessions_of(&target) {
            if self.channel_of.get(&session) == Some(&channel_id) {
                info!(
                    "{} ({client:#x}) sent {} out of channel {channel_id}",
                    self.get_username(client),
                    hex(&target[..8])
                );
                self.place_in_channel(session, default);
            }
        }
    }

    /// Remove a UI channel everywhere: memory, the row, the list.
    pub(super) fn delete_user_channel(&mut self, stable_id: u64) {
        self.policy.channels.retain(|c| c.stable_id != stable_id);
        self.persist.send(Command::DeleteChannel { stable_id });
        if let Err(e) = self.rebuild_channel_list() {
            warn!("channel list rebuild after delete failed: {e}");
        }
    }

    fn wire_position_of(&self, stable_id: u64) -> Option<u16> {
        let index = self
            .policy
            .channels
            .iter()
            .position(|c| c.stable_id == stable_id)?;
        u16::try_from(self.config.channels.len() + index).ok()
    }

    /// Owner with `MANAGE_OWN_CHANNELS`, or anyone with `MANAGE_CHANNELS`.
    fn may_manage(&self, user: &UserId, channel: &UserChannelRecord) -> bool {
        let perms = self.effective_permissions(user);
        (channel.owner.as_ref() == Some(user)
            && permissions::has(perms, permissions::MANAGE_OWN_CHANNELS))
            || permissions::has(perms, permissions::MANAGE_CHANNELS)
    }

    /// Validate a spec against the list; `except` is the channel being
    /// edited (its own name is not "taken"). Refuses with the reason.
    fn check_spec(
        &mut self,
        client: ClientId,
        mut spec: ChannelSpec,
        except: Option<u64>,
    ) -> Option<ChannelSpec> {
        spec.name = spec.name.trim().to_string();
        if spec.name.is_empty()
            || spec.name.chars().count() > MAX_CHANNEL_NAME
            || spec.user_limit == Some(0)
            || spec.afk_after_min == Some(0)
        {
            self.deny_channel(client, ChannelUiDenyReason::Invalid);
            return None;
        }
        if let Some(role) = spec.min_role
            && !self.policy.roles.contains_key(&role)
        {
            self.deny_channel(client, ChannelUiDenyReason::Invalid);
            return None;
        }
        spec.password = spec
            .password
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        let lower = spec.name.to_ascii_lowercase();
        let taken = self
            .config
            .channels
            .iter()
            .any(|c| c.name.trim().to_ascii_lowercase() == lower)
            || self.policy.channels.iter().any(|c| {
                Some(c.stable_id) != except && c.name.trim().to_ascii_lowercase() == lower
            });
        if taken {
            self.deny_channel(client, ChannelUiDenyReason::NameTaken);
            return None;
        }
        Some(spec)
    }

    fn deny_channel(&mut self, client: ClientId, reason: ChannelUiDenyReason) {
        info!(
            "{} ({client:#x}) channel request refused: {}",
            self.get_username(client),
            reason.describe()
        );
        self.send_msg(client, &ServerMessage::ChannelDenied { reason });
    }

    // ── Sweeps (every 30 s) ─────────────────────────────────────────

    /// Temporary channels evaporate after `temporary_after_min` empty;
    /// idle members go to the AFK channel after their channel's
    /// `afk_after_min` (spec §5).
    pub(super) fn sweep_channels(&mut self) {
        let now = self.wall_now();
        let config_len = self.config.channels.len();

        // Temporary channels.
        let mut to_delete = Vec::new();
        let mut to_persist = Vec::new();
        let limit_ms = u64::from(self.config.user_channels.temporary_after_min) * 60_000;
        for (k, channel) in self.policy.channels.iter_mut().enumerate() {
            let Ok(pos) = u16::try_from(config_len + k) else {
                break;
            };
            let occupied = self.channel_of.values().any(|c| *c == pos);
            match (occupied, channel.empty_since) {
                (true, Some(_)) => {
                    channel.empty_since = None;
                    to_persist.push(channel.clone());
                }
                (false, None) => {
                    channel.empty_since = Some(now);
                    to_persist.push(channel.clone());
                }
                (false, Some(since))
                    if !channel.permanent && now.saturating_sub(since) >= limit_ms =>
                {
                    to_delete.push((channel.stable_id, channel.name.clone()));
                }
                _ => {}
            }
        }
        for record in to_persist {
            self.persist.send(Command::UpsertChannel(record));
        }
        for (stable_id, name) in to_delete {
            info!("temporary channel '{name}' (#{stable_id}) was empty long enough; removed");
            self.delete_user_channel(stable_id);
        }

        // Auto-AFK.
        let Some(afk) = self.afk_channel() else {
            return;
        };
        let default_after = self.config.user_channels.afk_after_min;
        let lobby = self.default_channel;
        let idle: Vec<(ClientId, u16)> = self
            .channel_of
            .iter()
            .filter(|(_, ch)| **ch != afk && **ch != lobby)
            .filter_map(|(client, ch)| {
                let after_min = self
                    .user_channel_at(*ch)
                    .and_then(|c| c.afk_after_min)
                    .map_or(default_after, u32::from);
                if after_min == 0 {
                    return None;
                }
                let idle_for = self
                    .last_activity
                    .get(client)
                    .map_or(Duration::ZERO, |t| self.now.duration_since(*t));
                (idle_for >= Duration::from_secs(u64::from(after_min) * 60))
                    .then_some((*client, u16::try_from(after_min).unwrap_or(u16::MAX)))
            })
            .collect();
        for (client, after_min) in idle {
            info!(
                "{} ({client:#x}) idle for {after_min} min; moved to the AFK channel",
                self.get_username(client)
            );
            self.place_in_channel(client, afk);
            self.note_activity(client);
            self.send_msg(client, &ServerMessage::MovedIdle { after_min });
        }
    }
}
