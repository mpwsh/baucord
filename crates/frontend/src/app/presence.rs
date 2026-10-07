//! Who is here: `Init`, arrivals and departures, channel moves and
//! rebuilds, permissions, roles and ranks. (Not `roster`: that name is
//! app-core's module, imported by the parent and used all over.)
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    pub(super) fn handle_init_client(
        &mut self,
        client_id: u64,
        users: &HashMap<u64, String>,
        server_name: &str,
        greeting: &str,
        channels: &[client::ChannelInfo],
        memberships: &HashMap<u64, u16>,
    ) {
        // Fresh chat per connection — same semantics as the old
        // rebuild-the-main-view path.
        self.history.clear();
        self.chat_model.set_vec(Vec::new());
        self.images.clear();
        self.gifs.clear();
        self.reset_chat_flow();

        let s = self.main.global::<State>();
        // A session opens on the list: in the narrow layout that is the
        // sidebar's page, not whichever chat was showing last time.
        s.set_narrow_chat_open(false);
        s.set_phase(Phase::Main);
        // Connected: stay connected off screen too (Android's foreground
        // service and its notification, D34; nothing on the desktop).
        crate::host::keep_alive(Some(server_name));

        self.our_client_id = Some(client_id);
        self.server_name = server_name.to_string();
        // The lobby is wherever the server put us on arrival.
        self.lobby_name = if self.lobby {
            memberships
                .get(&client_id)
                .and_then(|ch| channels.iter().find(|c| c.id == *ch))
                .map(|c| c.name.clone())
        } else {
            None
        };
        // `ChannelDetails` follow, one per channel, and are keyed by
        // these ids — a stale set from an earlier list must not leak.
        self.channel_infos = channels.to_vec();
        self.channel_details.clear();
        self.rebuild_channels();
        self.pending_channel_join = None;
        let default_audio = PeerAudioSettings::default();
        for (&id, name) in users {
            let audio = peer_audio_from(
                self.settings
                    .peer_settings
                    .get(&peer_key(name))
                    .unwrap_or(&default_audio),
            );
            let channel_id = memberships.get(&id).copied().unwrap_or_default();
            self.users
                .insert(id, UserState::new(name.clone(), audio, channel_id));
        }

        // Start voice engine. With the lobby we arrive in it, and a
        // phone opens no microphone there (ANDROID-SPEC.md D53).
        self.voice_in_room = !self.lobby || self.is_testing_mic;
        self.voice
            .tx
            .send(VoiceControl::Start {
                settings: Box::new(self.settings.clone()),
                our_client_id: client_id,
                in_room: self.voice_in_room,
            })
            .ok();
        // Join ding, queued behind Start on the same channel so it
        // plays on the freshly started (or still-running) engine.
        self.play_chime(SoundType::UserJoined);

        // Sync per-user audio settings to voice engine
        for (&id, user) in &self.users {
            self.voice.tx.send(VoiceControl::AddPeer(id)).ok();
            self.voice
                .tx
                .send(VoiceControl::SetPeerAudio(id, peer_audio_to(user.audio)))
                .ok();
        }

        let s = self.main.global::<State>();
        s.set_status_text(format!("●  {}", t!("status-connected")).into());
        s.set_status_color(slint::Color::from_rgb_u8(0x2B, 0xAC, 0x76));
        // The name is the sidebar's title and the greeting the General
        // header's second line: neither repeats in chat on every login.
        let s = self.main.global::<State>();
        s.set_server_name(server_name.into());
        s.set_greeting(greeting.into());
        self.update_user_list();
        self.show_main();
        // An automatic reconnect lands in the lobby like any connect;
        // go back to the room the drop took us out of (a saved
        // password serves, a missing one asks).
        if let Some(name) = self.rejoin_channel.take()
            && let Some(id) = self.channels.iter().find(|c| c.name == name).map(|c| c.id)
        {
            self.channel_clicked(id);
        }
    }

    pub(super) fn handle_user_joined(&mut self, client_id: u64, username: String, channel_id: u16) {
        let saved = self
            .settings
            .peer_settings
            .get(&peer_key(&username))
            .cloned()
            .unwrap_or_default();

        self.voice.tx.send(VoiceControl::AddPeer(client_id)).ok();
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(client_id, saved.clone()))
            .ok();

        if self.lobby {
            let channel = self.channel_name(channel_id);
            self.push_chat_system(&t!(
                "chat-user-joined-channel",
                "name" => username.as_str(),
                "channel" => channel.as_str()
            ));
        } else {
            self.push_chat_system(&t!("chat-user-joined", "name" => username.as_str()));
        }
        self.rebuild_dms_model();
        // Off screen on a phone, the first voice into empty rooms is
        // news: someone is starting a call.
        self.alert_first_join(client_id, &username, channel_id);
        let mut user = UserState::new(username, peer_audio_from(&saved), channel_id);
        // `UserDetails` normally follows `UserJoined`; if it got here
        // first (a replaced session's details), honor it.
        if let Some((_, roles)) = self.details.get(&client_id) {
            user.role = role_of(roles);
        }
        self.users.insert(client_id, user);
        // Join ding only for arrivals into *our* channel — like the
        // classics, other rooms come and go silently.
        if Some(channel_id) == self.our_channel() {
            self.play_chime(SoundType::UserJoined);
        }
        self.update_user_list();
    }

    pub(super) fn handle_user_left(&mut self, client_id: u64, username: &str) {
        self.details.remove(&client_id);
        self.telemetry_notes.remove(&client_id);
        self.profiles.remove(&client_id);
        self.rebuild_dms_model();
        self.voice.tx.send(VoiceControl::RemovePeer(client_id)).ok();
        let was_in_our_channel = self
            .users
            .remove(&client_id)
            .is_some_and(|u| Some(u.channel_id) == self.our_channel());
        self.active_sharers.remove(&client_id);
        self.sharer_ids = self.active_sharers.keys().copied().collect();

        // Close viewer if we were watching this user
        if let Some(viewer) = self.viewers.remove(&client_id) {
            viewer.close();
        }

        self.push_chat_system(&t!("chat-user-left", "name" => username));
        if was_in_our_channel {
            self.play_chime(SoundType::UserLeft);
        }
        self.update_user_list();
    }

    /// In the lobby: connected, but in no room yet (spec §12, 1.1.5).
    pub(super) fn in_lobby(&self) -> bool {
        self.lobby
            && self
                .our_channel()
                .map(|c| self.channel_name(c))
                .is_some_and(|name| Some(name) == self.lobby_name)
    }

    /// The channel we are currently in, per server echo.
    pub(super) fn our_channel(&self) -> Option<u16> {
        self.our_client_id
            .and_then(|id| self.users.get(&id))
            .map(|u| u.channel_id)
    }

    pub(super) fn channel_name(&self, channel_id: u16) -> String {
        self.channels
            .iter()
            .find(|c| c.id == channel_id)
            .map_or_else(|| format!("#{channel_id}"), |c| c.name.clone())
    }

    pub(super) fn handle_user_moved(&mut self, client_id: u64, channel_id: u16) {
        let our_channel_before = self.our_channel();
        let old = if let Some(user) = self.users.get_mut(&client_id) {
            let old = user.channel_id;
            user.channel_id = channel_id;
            Some(old)
        } else {
            None
        };

        if Some(client_id) == self.our_client_id {
            // Our own move confirmed — close the prompt if it drove
            // this, and ding into the new room. Back into the lobby
            // (an admin's move, a kick) there is no room to ding
            // into: the banner says where we are.
            self.pending_channel_join = None;
            let state = self.main.global::<State>();
            state.set_channel_prompt_open(false);
            state.set_channel_password("".into());
            if self.in_lobby() {
                self.update_user_list();
                return;
            }
            let channel = self.channel_name(channel_id);
            self.push_chat_system(&t!("chat-you-moved", "channel" => channel.as_str()));
            // The streams for the room first (a phone opens them only
            // now, D53), then the ding — on the new output, not the old.
            self.sync_voice_room();
            self.play_chime(SoundType::UserJoined);
        } else {
            let name = self.username_of(client_id);
            let channel = self.channel_name(channel_id);
            self.push_chat_system(&t!(
                "chat-user-moved",
                "name" => name.as_str(),
                "channel" => channel.as_str()
            ));
            // Out of the AFK room into empty rooms: a call starting too.
            if old.is_some_and(|room| self.is_quiet_room(room)) {
                self.alert_first_join(client_id, &name, channel_id);
            }
            if Some(channel_id) == our_channel_before {
                // Someone stepped into our room.
                self.play_chime(SoundType::UserJoined);
            } else if old.is_some() && old == our_channel_before {
                // Someone stepped out of our room.
                self.play_chime(SoundType::UserLeft);
            }
        }
        self.update_user_list();
    }

    /// The server hot-reloaded its channel list: resync wholesale.
    /// Ids are renumbered, so an open password prompt is re-targeted by
    /// channel *name* (and closed if that channel is gone). Deliberately
    /// no join/leave sounds — an admin edit shouldn't set off a chorus.
    pub(super) fn handle_channels_updated(
        &mut self,
        channels: &[client::ChannelInfo],
        memberships: &HashMap<u64, u16>,
    ) {
        let pending_name = self.pending_channel_join.and_then(|id| {
            self.channels
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.name.clone())
        });

        // Ids are renumbered: the facts keyed by the old ids are void
        // until the server's fresh `ChannelDetails` land.
        self.channel_infos = channels.to_vec();
        self.channel_details.clear();
        self.rebuild_channels();
        for (id, channel_id) in memberships {
            if let Some(user) = self.users.get_mut(id) {
                user.channel_id = *channel_id;
            }
        }

        self.pending_channel_join = pending_name
            .as_ref()
            .and_then(|name| self.channels.iter().find(|c| &c.name == name))
            .map(|c| c.id);
        if pending_name.is_some() && self.pending_channel_join.is_none() {
            // The channel the prompt was for no longer exists.
            self.channel_password_cancel();
        }

        self.push_chat_system(&format!(
            "{}: {}",
            self.server_name,
            t!("chat-channels-updated"),
        ));
        // The list the server just sent is the answer to a create or
        // an edit the dialog is waiting on.
        if self.channel_dialog.as_ref().is_some_and(|d| d.pending) {
            self.channel_dialog_cancel();
        }
        self.update_user_list();
    }

    /// Derive the sidebar's channel list from the wire list, the
    /// per-channel facts and what this session may see (spec §12: a
    /// `min_role` above our rank hides the channel; `MANAGE_CHANNELS`
    /// sees everything).
    pub(super) fn rebuild_channels(&mut self) {
        let our_rank = self.our_rank();
        let sees_all =
            client::permissions::has(self.permissions, client::permissions::MANAGE_CHANNELS);
        self.channels = self
            .channel_infos
            .iter()
            .map(|c| {
                let facts = self.channel_details.get(&c.id);
                let gate_rank = facts.and_then(|f| f.min_role).and_then(builtin_rank);
                ChannelMeta {
                    id: c.id,
                    name: c.name.clone(),
                    locked: c.has_password,
                    user_limit: c.user_limit,
                    members_only: facts.is_some_and(|f| f.min_role.is_some()),
                    temporary: facts.is_some_and(|f| !f.permanent),
                    is_afk: facts.is_some_and(|f| f.is_afk),
                    owned: facts
                        .and_then(|f| f.owner)
                        .is_some_and(|owner| Some(owner) == self.our_user_id),
                    // Config channels have no owner and stable id 0.
                    from_config: facts.is_none_or(|f| f.stable_id == 0),
                    // A gate we cannot rank (a custom role) is left to
                    // the server: the channel stays visible and
                    // `JoinChannel` says no. The lobby is nowhere:
                    // its row and its members (us, until we pick a
                    // room) stay out entirely.
                    hidden: (!sees_all && gate_rank.is_some_and(|needed| needed > our_rank))
                        || self.lobby_name.as_deref() == Some(c.name.as_str()),
                }
            })
            .collect();
        let options: Vec<ChannelOption> = self
            .channels
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| ChannelOption {
                id: i32::from(c.id),
                name: c.name.as_str().into(),
            })
            .collect();
        self.channel_options.set_vec(options);
    }

    /// What this session may do, as the bools the UI draws from
    /// (spec §8: the server decides; the client renders).
    pub(super) fn push_permissions(&mut self) {
        use client::permissions as p;
        let has = |bit: u64| p::has(self.permissions, bit);
        let s = self.main.global::<State>();
        s.set_perm_mute(has(p::MUTE) || has(p::RESTRICT));
        s.set_perm_disconnect(has(p::DISCONNECT));
        s.set_perm_kick(has(p::KICK));
        s.set_perm_ban(has(p::BAN));
        s.set_perm_move(has(p::MANAGE_CHANNELS));
        s.set_perm_roles(has(p::MANAGE_ROLES));
        s.set_perm_users(has(p::MANAGE_USERS));
        s.set_perm_create_channels(has(p::CREATE_CHANNELS));
        s.set_perm_create_invites(has(p::CREATE_INVITES));
        s.set_perm_manage_invites(has(p::MANAGE_INVITES));
        s.set_perm_panel(has(p::PANEL));
        s.set_perm_admin_channel(has(p::ADMIN_CHANNEL));
        s.set_perm_manage_own(has(p::MANAGE_OWN_CHANNELS));
        s.set_perm_delete_any(has(p::DELETE_MESSAGES));
        s.set_perm_dm(has(p::DM));
        s.set_our_role(
            self.our_client_id
                .and_then(|id| self.details.get(&id))
                .map_or(0, |(_, roles)| role_of(roles).as_int()),
        );
        s.set_server_persistent(self.server_persistent);
        crate::settingsui::refresh_role_line(self);
        // The channel's row, for whoever may read it (the borrow of
        // the window's globals is over by here).
        if p::has(self.permissions, p::ADMIN_CHANNEL) {
            self.ensure_admin_channel();
            self.rebuild_dms_model();
        }
    }

    /// Does a message name us — the nickname as a whole word, or
    /// `@nickname`? Case-insensitive.
    pub(super) fn mentions_us(&self, text: &str) -> bool {
        let nick = self.settings.user.nickname.trim().to_lowercase();
        if nick.is_empty() {
            return false;
        }
        text.to_lowercase()
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@'))
            .any(|word| word.trim_start_matches('@') == nick)
    }

    /// The chat preferences the rows read (Settings → General → Chat).
    pub fn push_ui_prefs(&self) {
        let s = self.main.global::<State>();
        s.set_show_timestamps(self.settings.ui.show_timestamps);
        s.set_compact_rows(self.settings.ui.compact_rows);
        s.set_messages_collapsed(self.settings.ui.messages_collapsed);
        s.set_sidebar_width(f32::from(self.settings.ui.sidebar_width.clamp(220, 480)));
        s.set_messages_height(f32::from(self.settings.ui.messages_height.clamp(100, 480)));
    }

    /// The Voice/Messages divider was let go: keep the height.
    pub(super) fn messages_resized(&mut self, height: f32) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let height = height.clamp(100.0, 480.0).round() as u16;
        if height != self.settings.ui.messages_height {
            self.settings.ui.messages_height = height;
            self.settings.save();
        }
    }

    /// The sidebar's edge was let go: keep the width.
    pub(super) fn sidebar_resized(&mut self, width: f32) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let width = width.clamp(220.0, 480.0).round() as u16;
        if width != self.settings.ui.sidebar_width {
            self.settings.ui.sidebar_width = width;
            self.settings.save();
        }
    }

    /// The Messages header's chevron: fold the section or open it,
    /// and remember which.
    pub(super) fn toggle_messages(&mut self) {
        self.settings.ui.messages_collapsed = !self.settings.ui.messages_collapsed;
        self.settings.save();
        self.push_ui_prefs();
    }

    /// Push-to-talk on or off: tell the engine, and the mic button
    /// what to say.
    pub fn apply_push_to_talk(&self) {
        let on = self.settings.audio.push_to_talk;
        self.voice.tx.send(VoiceControl::SetPushToTalk(on)).ok();
        let bind = self.settings.bind_for(BindableAction::PushToTalk);
        let s = self.main.global::<State>();
        s.set_ptt_mode(on);
        s.set_ptt_held(false);
        s.set_ptt_tooltip(
            format!(
                "{} {}",
                t!("ptt-tooltip"),
                if bind.is_empty() { "—" } else { bind }
            )
            .into(),
        );
    }

    /// The byte budget an outgoing image is shrunk to fit: the setting,
    /// never above the wire cap.
    pub fn image_budget(&self) -> usize {
        let wanted = usize::try_from(self.settings.ui.image_max_kb)
            .unwrap_or(usize::MAX)
            .saturating_mul(1024);
        wanted.clamp(64 * 1024, client::MAX_CHAT_IMAGE_BYTES)
    }

    /// The join / leave chime, unless the settings silence it.
    pub(super) fn play_chime(&self, sound: SoundType) {
        if self.settings.sounds.join_leave_on() {
            self.voice.tx.send(VoiceControl::PlaySound(sound)).ok();
        }
    }

    /// The name the server knows this session by, while connected.
    pub fn session_name(&self) -> Option<&str> {
        if !self.connected {
            return None;
        }
        self.our_client_id
            .and_then(|id| self.users.get(&id))
            .map(|u| u.username.as_str())
    }

    /// Our role badge and level as the roster knows them (for the
    /// profile tab).
    pub fn our_role_and_level(&self) -> (Role, u16) {
        self.our_client_id
            .and_then(|id| self.users.get(&id))
            .map_or((Role::Unknown, 0), |u| (u.role, u.level))
    }

    /// The role table's ids in rank order (the panel's lists and the
    /// Role dropdown share it).
    pub(crate) fn role_table_in_rank_order(&self) -> Vec<client::RoleId> {
        let mut ids: Vec<(u16, client::RoleId)> =
            self.roles.values().map(|r| (r.rank, r.id)).collect();
        ids.sort_unstable();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// A user's name of record by id: the roster knows the online
    /// ones; the rest are a short id.
    #[allow(dead_code)] // kept for the admin feed's next columns
    pub(crate) fn name_of_user_id(&self, user: &client::UserId) -> String {
        self.details
            .iter()
            .find(|(_, (id, _))| id == user)
            .and_then(|(client, _)| self.users.get(client))
            .map_or_else(|| short_id(&user[..8]), |u| u.username.clone())
    }

    /// A role's name for people: the built-ins localized, a custom one
    /// as the table has it, `role N` for one we never heard of.
    pub(crate) fn role_name_of(&self, role: client::RoleId) -> String {
        match Role::from_int(i32::try_from(role).unwrap_or(0)) {
            Some(builtin) if builtin != Role::Unknown => role_name(builtin),
            _ => self
                .roles
                .get(&role)
                .map_or_else(|| format!("role {role}"), |r| r.name.clone()),
        }
    }

    /// A role as typed in `/grant`: any name in the table
    /// (case-insensitive) or an id.
    pub(super) fn parse_role_name(&self, text: &str) -> Option<client::RoleId> {
        let lower = text.trim().to_ascii_lowercase();
        self.roles
            .values()
            .find(|r| r.name == lower)
            .map(|r| r.id)
            .or_else(|| lower.parse().ok())
    }

    /// A client's highest-ranked role, as the table knows it (the
    /// badge and its color come from it); `None` for a built-in
    /// without a style, or no entry.
    pub(super) fn top_role_info(&self, client_id: u64) -> Option<&client::protocol::RoleInfo> {
        self.details
            .get(&client_id)
            .and_then(|(_, roles)| {
                roles
                    .iter()
                    .filter_map(|r| self.roles.get(r))
                    .max_by_key(|info| (info.rank, info.id))
            })
            .filter(|info| !info.icon.is_empty() || info.color != 0)
    }

    /// The name of a client's highest-ranked role, for the card.
    pub(super) fn top_role_name(&self, client_id: u64) -> String {
        let top = self
            .details
            .get(&client_id)
            .and_then(|(_, roles)| {
                roles
                    .iter()
                    .filter_map(|r| self.roles.get(r).map(|info| (info.rank, *r)))
                    .max()
            })
            .map(|(_, id)| id);
        match top {
            Some(id) => self.role_name_of(id),
            None => self
                .users
                .get(&client_id)
                .map_or_else(String::new, |u| role_name(u.role)),
        }
    }

    /// Push the "Text size" setting to every window's Theme (globals
    /// are per window instance).
    pub fn apply_text_scale(&self) {
        let scale = self.settings.ui.text_scale;
        self.main.global::<ui::Theme>().set_scale(scale);
        if let Some(win) = &self.settings_win {
            win.global::<ui::Theme>().set_scale(scale);
        }
        if let Some(win) = &self.admin_win {
            win.global::<ui::Theme>().set_scale(scale);
        }
        for viewer in self.viewers.values() {
            viewer.apply_text_scale(scale);
        }
    }

    /// A session's highest rank across its roles — from the server's
    /// table, so a custom role counts for the rank rule (a rank-29
    /// steward outranks organizers); a guest's until its details
    /// arrive.
    pub(super) fn rank_of_client(&self, client_id: u64) -> client::permissions::Rank {
        self.details
            .get(&client_id)
            .map_or(client::permissions::RANK_GUEST, |(_, roles)| {
                roles
                    .iter()
                    .filter_map(|r| {
                        self.roles
                            .get(r)
                            .map(|info| info.rank)
                            .or_else(|| builtin_rank(*r))
                    })
                    .max()
                    .unwrap_or(client::permissions::RANK_GUEST)
            })
    }

    /// Our highest built-in rank, from our roles; a guest's when the
    /// details have not arrived yet.
    pub(super) fn our_rank(&self) -> client::permissions::Rank {
        self.our_client_id
            .map_or(client::permissions::RANK_GUEST, |id| {
                self.rank_of_client(id)
            })
    }
}
