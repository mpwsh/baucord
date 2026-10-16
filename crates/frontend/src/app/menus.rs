//! Context menus (users, rows, channels), moderation and its confirmation, per-user audio, watching a share.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    /// The client id behind roster row `index`, if it still exists.
    pub fn user_id_at(&self, index: i32) -> Option<u64> {
        let row = self.users_model.row_data(usize::try_from(index).ok()?)?;
        if row.is_channel {
            return None;
        }
        Some(roster::join_id(row.client_id_lo, row.client_id_hi))
    }

    pub fn username_of(&self, client_id: u64) -> String {
        self.session.users.get(&client_id).map_or_else(
            || t!("fallback-username", "id" => client_id.to_string()),
            |u| u.username.clone(),
        )
    }

    /// The server-side identity behind roster row `index`, as an admin
    /// command names it.
    pub(super) fn admin_target_at(&self, index: i32) -> Option<AdminTarget> {
        let client_id = self.user_id_at(index)?;
        let (user_id, _) = self.session.details.get(&client_id)?;
        Some(AdminTarget::User(*user_id))
    }

    /// A drag payload over a channel row: `Some((client, channel))`
    /// when the payload is one of our users and the row is a channel
    /// they are not already in. Used both to accept the hover and to
    /// act on the drop.
    pub(super) fn drop_target(
        &self,
        data: &slint::DataTransfer,
        channel_index: i32,
    ) -> Option<(u64, u16)> {
        let client_id = data.user_data()?.downcast::<DragUser>().ok()?.0;
        let row = self
            .users_model
            .row_data(usize::try_from(channel_index).ok()?)?;
        if !row.is_channel {
            return None;
        }
        let channel_id = u16::try_from(row.channel_id).ok()?;
        let current = self.session.users.get(&client_id).map(|u| u.channel_id);
        (current != Some(channel_id)).then_some((client_id, channel_id))
    }

    /// The drop: the same `MoveUser` the "Move to" submenu sends,
    /// the server judging permission and the target's gates.
    pub(super) fn move_user_to(&mut self, client_id: u64, channel_id: u16) {
        let Some((user_id, _)) = self.session.details.get(&client_id) else {
            return;
        };
        let target = AdminTarget::User(*user_id);
        self.send_admin(
            AdminCommand::MoveUser { target, channel_id },
            PendingAdmin::Show,
        );
    }

    // ── Context menus ───────────────────────────────────────────────
    // A row asks for its menu at a window position; app-core's `menu`
    // decides the entries from what the session may do (spec §8), the
    // overlay in main.slint draws them, and the pick comes back to
    // `menu_activated`, which resolves it against the remembered
    // target. Every action sends the same `NetworkControl` its slash
    // command does — one authorization path, two front ends.

    /// What this session may do, as the menu builders read it.
    pub(super) fn menu_session(&self) -> menu::Session {
        use client::permissions as p;
        let has = |bit: u64| p::has(self.session.permissions, bit);
        menu::Session {
            mute: has(p::MUTE) || has(p::RESTRICT),
            disconnect: has(p::DISCONNECT),
            kick: has(p::KICK),
            ban: has(p::BAN),
            move_users: has(p::MANAGE_CHANNELS),
            roles: has(p::MANAGE_ROLES),
            dm: has(p::DM),
            manage_own_channels: has(p::MANAGE_OWN_CHANNELS),
            create_channels: has(p::CREATE_CHANNELS),
            safety: self.session.safety.on,
            our_rank: self.our_rank(),
        }
    }

    /// A roster row asked for its menu (a right-click, or a left-click
    /// on a user row). The rank rule (spec §8) and the channel powers
    /// are worked out here, from the session, rather than carried on
    /// every row (review P1-9).
    pub(super) fn open_user_menu(&mut self, index: i32, x: f32, y: f32) {
        use client::permissions as p;
        let Some(row) = usize::try_from(index)
            .ok()
            .and_then(|i| self.users_model.row_data(i))
        else {
            return;
        };
        let session = self.menu_session();
        if row.is_channel {
            let Some(channel) = u16::try_from(row.channel_id)
                .ok()
                .and_then(|id| self.session.channels.iter().find(|c| c.id == id))
            else {
                return;
            };
            let facts = menu::ChannelFacts {
                id: channel.id,
                owned: channel.owned,
                from_config: channel.from_config,
            };
            let entries = menu::channel_menu(&session, &facts);
            self.show_menu(entries, MenuTarget::Channel(facts.id), x, y);
        } else {
            let channel_id = u16::try_from(row.channel_id).unwrap_or(0);
            // Nothing applies to ourselves.
            let (outranked, role_editable, channel_kickable) = if row.is_us {
                (false, false, false)
            } else {
                let theirs =
                    self.rank_of_client(roster::join_id(row.client_id_lo, row.client_id_hi));
                let owns_their_channel = self
                    .session
                    .channels
                    .iter()
                    .any(|c| c.id == channel_id && c.owned);
                (
                    theirs < session.our_rank,
                    theirs <= session.our_rank,
                    p::has(self.session.permissions, p::MANAGE_CHANNELS)
                        || (owns_their_channel
                            && p::has(self.session.permissions, p::MANAGE_OWN_CHANNELS)),
                )
            };
            let facts = menu::UserFacts {
                is_us: row.is_us,
                role: Role::from_int(row.role).unwrap_or_default(),
                sharing: row.sharing,
                local_muted: row.local_muted,
                outranked,
                is_admin: row.role == 4,
                role_editable,
                channel_kickable,
                channel_id,
                blocked: self
                    .user_id_at(index)
                    .is_some_and(|c| self.client_blocked(c)),
            };
            let channels: Vec<menu::ChannelChoice> = self
                .session
                .channels
                .iter()
                .filter(|c| !c.hidden)
                .map(|c| menu::ChannelChoice {
                    id: c.id,
                    name: c.name.clone(),
                })
                .collect();
            let held: Vec<client::RoleId> = self
                .user_id_at(index)
                .and_then(|id| self.session.details.get(&id))
                .map(|(_, roles)| roles.clone())
                .unwrap_or_default();
            let mut roles: Vec<menu::RoleChoice> = self
                .session
                .roles
                .values()
                .map(|r| menu::RoleChoice {
                    id: r.id,
                    name: r.name.clone(),
                    rank: r.rank,
                    builtin: r
                        .builtin
                        .then(|| Role::from_int(i32::try_from(r.id).unwrap_or(0)))
                        .flatten(),
                    held: held.contains(&r.id),
                })
                .collect();
            roles.sort_by_key(|r| (r.rank, r.id));
            let entries = menu::user_menu(&session, &facts, &channels, &roles);
            // The slider row starts at their current level.
            let volume = self
                .user_id_at(index)
                .and_then(|id| self.session.users.get(&id))
                .map_or(1.0, |u| u.audio.volume);
            self.main.global::<State>().set_menu_volume(volume);
            if let Some(client_id) = self.user_id_at(index) {
                self.show_menu(entries, MenuTarget::User(client_id), x, y);
            }
        }
    }

    /// The slider in a user's menu moved: every move reaches the
    /// audio engine so it is heard at once; the release also persists
    /// it and refreshes the roster (the badge). The menu stays open.
    pub(super) fn menu_volume(&mut self, volume: f32, released: bool) {
        let Some(MenuTarget::User(client_id)) = self.menu_target else {
            return;
        };
        if self.roster_index_of(client_id).is_none() {
            return;
        }
        if released {
            let username = self.username_of(client_id);
            self.handle_set_user_volume(client_id, &username, volume);
            return;
        }
        let mut audio = self
            .session
            .users
            .get(&client_id)
            .map(|u| peer_audio_to(u.audio))
            .unwrap_or_default();
        audio.volume = volume;
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(client_id, audio))
            .ok();
    }

    /// A chat row asked for its menu; `selecting` is whether a styled
    /// body is currently in its plain, selectable view.
    pub(super) fn open_row_menu(&mut self, row: &ChatRow, selecting: bool, x: f32, y: f32) {
        let kind = match row.kind {
            0 => menu::MessageKind::Text,
            2 => menu::MessageKind::Image,
            3 => menu::MessageKind::Code,
            5 => menu::MessageKind::File,
            // System lines and day dividers have no menu.
            _ => return,
        };
        let msg_id = roster::join_id(row.msg_id_lo, row.msg_id_hi);
        // Someone else's stored message, in General or a DM (not the
        // admin channel's lines): reportable, its author blockable.
        let reportable = self.session.safety.on
            && !row.own
            && !row.pending
            && self.session.chat.active_dm != Some(ADMIN_CHANNEL_KEY)
            && self
                .author_of_message(msg_id)
                .is_some_and(|(author, _)| Some(author) != self.session.our_user_id);
        let facts = menu::MessageFacts {
            kind,
            own: row.own,
            deleted: row.deleted,
            styled: row.styled,
            selecting,
            in_dm: self.session.chat.active_dm.is_some(),
            delete_any: client::permissions::has(
                self.session.permissions,
                client::permissions::DELETE_MESSAGES,
            ),
            reportable,
            copy_image: crate::host::HAS_IMAGE_CLIPBOARD,
        };
        let target = MenuTarget::Message {
            row_id: row.row_id,
            msg_id,
            body: row.body.to_string(),
        };
        self.show_menu(menu::message_menu(&facts), target, x, y);
    }

    /// Open the overlay with `entries` for `target` at window
    /// coordinates; an empty menu opens nothing.
    pub(super) fn show_menu(
        &mut self,
        entries: Vec<menu::Entry>,
        target: MenuTarget,
        x: f32,
        y: f32,
    ) {
        if entries.is_empty() {
            return;
        }
        let (main, submenus, actions) = ui::convert::menu_entries(&entries, &menu_label);
        let submenus: Vec<ModelRc<ui::MenuEntry>> = submenus
            .into_iter()
            .map(|column| ModelRc::new(VecModel::from(column)))
            .collect();
        let s = self.main.global::<State>();
        s.set_menu_entries(ModelRc::new(VecModel::from(main)));
        s.set_menu_submenus(ModelRc::new(VecModel::from(submenus)));
        s.set_menu_x(x);
        s.set_menu_y(y);
        s.set_menu_open(true);
        self.menu_target = Some(target);
        self.menu_actions = actions;
    }

    /// A person's Actions menu in the admin panel's Users tab, under
    /// the button at window coordinates `x`, `y`.
    pub(crate) fn open_admin_user_menu(&mut self, user_id: String, online: bool, x: f32, y: f32) {
        self.show_menu(
            menu::admin_user_menu(online),
            MenuTarget::AdminUser(user_id),
            x,
            y,
        );
    }

    /// Escape, a click outside, or a pick: the overlay goes, and the
    /// composer takes back the focus the overlay's key scope held.
    pub(super) fn close_menu(&mut self) {
        self.menu_target = None;
        self.menu_actions.clear();
        let s = self.main.global::<State>();
        if s.get_menu_open() {
            s.set_menu_open(false);
            if s.get_phase() == Phase::Main {
                self.main.invoke_focus_composer();
            }
        }
    }

    /// An entry was picked: resolve it against the menu's target.
    pub(super) fn menu_activated(&mut self, id: i32) {
        use menu::Action as A;
        let target = self.menu_target.take();
        let action = usize::try_from(id)
            .ok()
            .and_then(|id| self.menu_actions.get(id).copied());
        self.close_menu();
        let (Some(target), Some(action)) = (target, action) else {
            return;
        };
        match (target, action) {
            (MenuTarget::User(client_id), action) => {
                // Gone since the menu opened: nothing to act on.
                if let Some(index) = self.roster_index_of(client_id) {
                    self.user_menu_action(index, action);
                }
            }
            (MenuTarget::Channel(id), A::ChannelEdit(_)) => self.channel_edit(id),
            (MenuTarget::Channel(id), A::ChannelDelete(_)) => self.channel_delete(id),
            (MenuTarget::Message { body, .. }, A::CopyText) => {
                crate::clipboard::set_clipboard_text(&body);
            }
            (MenuTarget::Message { row_id, .. }, A::SelectText(on)) => {
                self.main
                    .global::<State>()
                    .set_selecting_row(if on { row_id } else { -1 });
            }
            (MenuTarget::Message { msg_id, .. }, A::EditMessage) => self.row_edit(msg_id),
            (MenuTarget::Message { msg_id, .. }, A::DeleteMessage) => self.row_delete(msg_id),
            (MenuTarget::Message { row_id, .. }, A::OpenImage) => {
                crate::viewers::open_gallery(self, row_id);
            }
            (MenuTarget::Message { row_id, .. }, A::SaveImage) => {
                media::save_image_dialog(self, row_id);
            }
            (MenuTarget::Message { row_id, .. }, A::CopyImage) => {
                media::copy_image(self, row_id);
            }
            (MenuTarget::Message { row_id, .. }, A::SaveFile) => self.file_save(row_id),
            (MenuTarget::Message { msg_id, body, .. }, A::ReportMessage) => {
                self.report_message(msg_id, &body);
            }
            (MenuTarget::Message { msg_id, .. }, A::BlockAuthor) => self.block_author(msg_id),
            (MenuTarget::Composer { cursor, anchor }, A::Cut) => {
                self.composer_take(cursor, anchor, true);
            }
            (MenuTarget::Composer { cursor, anchor }, A::Copy) => {
                self.composer_take(cursor, anchor, false);
            }
            (MenuTarget::Composer { .. }, A::Paste) => media::paste_into_chat(self),
            (MenuTarget::AdminUser(user_id), action) => {
                crate::adminui::user_action(self, &user_id, action);
            }
            _ => {}
        }
    }

    /// The composer menu's Copy, and its Cut when `cut`, on the
    /// selection the right-click found. The field's own `copy()` and
    /// `cut()` have nothing left to work on by now, and its cut would
    /// call back into the app from inside this call (review P2-16).
    fn composer_take(&mut self, cursor: usize, anchor: usize, cut: bool) {
        use app_core::composer;
        let s = self.main.global::<State>();
        let draft = s.get_draft();
        let picked = composer::selected(&draft, cursor, anchor);
        if picked.is_empty() {
            return;
        }
        crate::clipboard::set_clipboard_text(picked);
        if cut {
            let (rest, caret) = composer::cut(&draft, cursor, anchor);
            s.set_draft(rest.into());
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            self.main.invoke_composer_set_cursor(caret as i32);
        }
    }

    /// The audio, message and moderation entries of a user row.
    pub(super) fn user_menu_action(&mut self, index: i32, action: menu::Action) {
        use menu::Action as A;
        match action {
            A::MuteForMe => {
                if let Some(id) = self.user_id_at(index) {
                    let username = self.username_of(id);
                    let muted = self
                        .session
                        .users
                        .get(&id)
                        .is_some_and(|u| u.audio.is_muted);
                    self.handle_mute_user(id, &username, !muted);
                }
            }
            A::ResetAudio => {
                if let Some(id) = self.user_id_at(index) {
                    let username = self.username_of(id);
                    self.handle_reset_peer_audio(id, &username);
                }
            }
            A::Watch => {
                if let Some(id) = self.user_id_at(index) {
                    self.handle_watch_peer(id);
                }
            }
            A::Message => self.dm_from_roster(index),
            A::Block => self.block_at(index, true),
            A::Unblock => self.block_at(index, false),
            A::Report => self.report_user_at(index),
            A::SetRole(role) => {
                // One role is the whole set: the built-ins nest, and a
                // custom role is defined as a complete set.
                if let Some(target) = self.admin_target_at(index) {
                    self.send_admin(
                        AdminCommand::SetRoles {
                            target,
                            roles: vec![role],
                        },
                        PendingAdmin::Show,
                    );
                }
            }
            A::MoveTo(channel_id) => {
                if let Some(target) = self.admin_target_at(index) {
                    self.send_admin(
                        AdminCommand::MoveUser { target, channel_id },
                        PendingAdmin::Show,
                    );
                }
            }
            A::ChannelKick => self.channel_kick_at(index),
            A::MuteVoice => self.moderate(index, ModAction::MuteVoice),
            A::MuteChat => self.moderate(index, ModAction::MuteChat),
            A::UnmuteVoice => self.moderate(index, ModAction::UnmuteVoice),
            A::UnmuteChat => self.moderate(index, ModAction::UnmuteChat),
            A::Disconnect => self.moderate(index, ModAction::Disconnect),
            A::Kick => self.moderate(index, ModAction::Kick),
            A::Ban => self.moderate(index, ModAction::Ban),
            // Volume is the slider row, not a pick: its moves come
            // through `menu_volume`. The rest are not user actions.
            _ => {}
        }
    }

    /// "Send to the default channel" for the user at roster row
    /// `index` (the owner's channel kick, spec §5).
    pub(super) fn channel_kick_at(&mut self, index: i32) {
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        let (Some((user_id, _)), Some(user)) = (
            self.session.details.get(&client_id),
            self.session.users.get(&client_id),
        ) else {
            return;
        };
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::ChannelKick {
                channel_id: user.channel_id,
                user_id: *user_id,
            }))
            .ok();
    }

    /// A moderation entry was picked for the user at row `index`: the
    /// unmutes go straight out; everything else opens the dialog for a
    /// duration and a reason (spec §4.1).
    pub(super) fn moderate(&mut self, index: i32, action: ModAction) {
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        let Some(target) = self.admin_target_at(index) else {
            return;
        };
        let kind_for = |a: ModAction| match a {
            ModAction::MuteVoice | ModAction::UnmuteVoice => client::RestrictionKind::Voice,
            _ => client::RestrictionKind::GeneralText,
        };
        match action {
            ModAction::UnmuteVoice | ModAction::UnmuteChat => {
                let kind = kind_for(action);
                self.send_admin(AdminCommand::Unmute { target, kind }, PendingAdmin::Show);
                return;
            }
            ModAction::MuteVoice
            | ModAction::MuteChat
            | ModAction::Disconnect
            | ModAction::Kick
            | ModAction::Ban => {}
        }
        let name = self.username_of(client_id);
        let (title, hint, confirm) = match action {
            ModAction::MuteVoice => (
                "mod-title-mute-voice",
                "mod-hint-mute-voice",
                "mod-confirm-mute",
            ),
            ModAction::MuteChat => (
                "mod-title-mute-chat",
                "mod-hint-mute-chat",
                "mod-confirm-mute",
            ),
            ModAction::Disconnect => (
                "mod-title-disconnect",
                "mod-hint-disconnect",
                "mod-confirm-disconnect",
            ),
            ModAction::Kick => ("mod-title-kick", "mod-hint-kick", "mod-confirm-kick"),
            ModAction::Ban | ModAction::UnmuteVoice | ModAction::UnmuteChat => {
                ("mod-title-ban", "mod-hint-ban", "mod-confirm-ban")
            }
        };
        // Durations: a mute defaults to half an hour, a ban to a day;
        // disconnect and kick have none.
        let (durations, default): (&[(&str, Option<u64>)], i32) = match action {
            ModAction::MuteVoice | ModAction::MuteChat => (
                &[
                    ("duration-10m", Some(10 * 60)),
                    ("duration-30m", Some(30 * 60)),
                    ("duration-2h", Some(2 * 3600)),
                    ("duration-24h", Some(24 * 3600)),
                    ("duration-forever", None),
                ],
                1,
            ),
            ModAction::Ban => (
                &[
                    ("duration-24h", Some(24 * 3600)),
                    ("duration-7d", Some(7 * 86_400)),
                    ("duration-30d", Some(30 * 86_400)),
                    ("duration-forever", None),
                ],
                0,
            ),
            _ => (&[], 0),
        };
        let labels: Vec<slint::SharedString> =
            durations.iter().map(|(k, _)| t!(k).into()).collect();
        self.session.moderation = Some(Moderation {
            action,
            client_id,
            duration_secs: durations.iter().map(|(_, secs)| *secs).collect(),
        });
        let s = self.main.global::<State>();
        s.set_moderation_action(action);
        s.set_moderation_title(t!(title, "name" => name.as_str()).into());
        s.set_moderation_hint(t!(hint).into());
        s.set_moderation_confirm_label(t!(confirm, "name" => name.as_str()).into());
        s.set_moderation_durations(ModelRc::new(VecModel::from(labels)));
        s.set_moderation_duration(default);
        s.set_moderation_reason("".into());
        s.set_moderation_open(true);
    }

    pub(super) fn confirm_moderation(&mut self) {
        let Some(m) = self.session.moderation.take() else {
            self.cancel_moderation();
            return;
        };
        let s = self.main.global::<State>();
        let reason = s.get_moderation_reason().trim().to_string();
        let duration_secs = usize::try_from(s.get_moderation_duration())
            .ok()
            .and_then(|i| m.duration_secs.get(i).copied())
            .flatten();
        s.set_moderation_open(false);
        s.set_moderation_reason("".into());
        // The person may have left while the dialog was open.
        let Some((user_id, _)) = self.session.details.get(&m.client_id) else {
            return;
        };
        let target = AdminTarget::User(*user_id);
        let command = match m.action {
            ModAction::MuteVoice | ModAction::MuteChat => AdminCommand::Restrict {
                target,
                kind: if m.action == ModAction::MuteVoice {
                    client::RestrictionKind::Voice
                } else {
                    client::RestrictionKind::GeneralText
                },
                duration_secs,
                reason,
            },
            ModAction::Disconnect => AdminCommand::Disconnect { target, reason },
            ModAction::Kick => AdminCommand::Kick { target, reason },
            ModAction::Ban => AdminCommand::Ban {
                target,
                duration_secs,
                reason,
            },
            ModAction::UnmuteVoice | ModAction::UnmuteChat => return,
        };
        self.send_admin(command, PendingAdmin::Show);
        self.main.invoke_focus_composer();
    }

    pub(super) fn cancel_moderation(&mut self) {
        self.session.moderation = None;
        let s = self.main.global::<State>();
        s.set_moderation_open(false);
        s.set_moderation_reason("".into());
    }

    /// File a person's volume and mute under who they are (their user
    /// id, once `UserDetails` said it), not under what they are called
    /// today (review P2-24).
    fn save_peer_audio(
        &mut self,
        client_id: u64,
        username: &str,
        audio: Option<PeerAudioSettings>,
    ) {
        let user = self.session.details.get(&client_id).map(|(user, _)| *user);
        self.settings.set_peer_audio(user.as_ref(), username, audio);
    }

    pub(super) fn handle_mute_user(&mut self, client_id: u64, username: &str, muted: bool) {
        if let Some(user) = self.session.users.get_mut(&client_id) {
            user.audio.is_muted = muted;
            let audio = peer_audio_to(user.audio);
            self.save_peer_audio(client_id, username, Some(audio));
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.session
                    .users
                    .get(&client_id)
                    .map(|u| peer_audio_to(u.audio))
                    .unwrap_or_default(),
            ))
            .ok();
        self.save_settings();
        self.update_user_list();
    }

    pub(super) fn handle_set_user_volume(&mut self, client_id: u64, username: &str, volume: f32) {
        if let Some(user) = self.session.users.get_mut(&client_id) {
            user.audio.volume = volume;
            let audio = peer_audio_to(user.audio);
            self.save_peer_audio(client_id, username, Some(audio));
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.session
                    .users
                    .get(&client_id)
                    .map(|u| peer_audio_to(u.audio))
                    .unwrap_or_default(),
            ))
            .ok();
        self.save_settings();
        self.update_user_list();
    }

    pub(super) fn handle_reset_peer_audio(&mut self, client_id: u64, username: &str) {
        self.save_peer_audio(client_id, username, None);
        if let Some(user) = self.session.users.get_mut(&client_id) {
            user.audio = PeerAudio::default();
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                PeerAudioSettings::default(),
            ))
            .ok();
        self.save_settings();
        self.update_user_list();
    }

    /// The peer whose stream the main window's viewer page shows
    /// (single-window hosts, ANDROID-SPEC.md D23); `None` on the
    /// desktop, where viewers are windows.
    pub(crate) fn page_viewer_id(&self) -> Option<u64> {
        self.viewers
            .iter()
            .find(|(_, v)| v.is_page())
            .map(|(id, _)| *id)
    }

    pub(super) fn handle_watch_peer(&mut self, client_id: u64) {
        if self.viewers.contains_key(&client_id) {
            // Already watching: bring the viewer forward, on the event
            // loop like a fresh open (window ordering can dispatch
            // events synchronously on macOS).
            let _ = slint::invoke_from_event_loop(move || {
                with_current(move |app| {
                    if let Some(viewer) = app.viewers.get(&client_id) {
                        viewer.raise();
                    }
                });
            });
            return;
        }
        let username = self.username_of(client_id);
        // Open at the sharer's announced dimensions so a 1080p stream
        // doesn't flash open as a 720p window before the first frame.
        let (share_w, share_h) = self
            .session
            .active_sharers
            .get(&client_id)
            .copied()
            .unwrap_or((1280, 720));
        info!("Watch {username} ({client_id}): opening viewer at {share_w}x{share_h}");
        // Window creation + show() runs OUTSIDE the app borrow: on
        // macOS, ordering a window front can dispatch events
        // synchronously, and a callback re-entering the app state
        // while it is borrowed would abort the process.
        let _ = slint::invoke_from_event_loop(move || {
            with_current(move |app| {
                if app.viewers.contains_key(&client_id) {
                    return;
                }
                // One page, one stream: on a single-window host the
                // new stream replaces whatever was being watched.
                if super::SINGLE_WINDOW_HOST {
                    let others: Vec<u64> = app.viewers.keys().copied().collect();
                    for other in others {
                        app.handle_unwatch_peer(other);
                    }
                }
                match Viewer::open(
                    client_id,
                    &username,
                    share_w,
                    share_h,
                    app.settings.ui.text_scale,
                    &app.main,
                ) {
                    Ok(viewer) => {
                        viewer.set_self_audio(app.session.is_muted, app.session.is_deafened);
                        app.viewers.insert(client_id, viewer);
                        // The screen thread first, so the stream's
                        // frames are wanted before the server sends any.
                        app.screen.tx.send(ScreenControl::Watch { client_id }).ok();
                        app.network
                            .tx
                            .send(NetworkControl::Send(ClientMessage::SubscribeScreen {
                                target_id: client_id,
                            }))
                            .ok();
                        app.push_activity(&t!("chat-watching", "name" => username.as_str()));
                        info!("Viewer open + subscribed to {client_id}");
                    }
                    Err(e) => {
                        warn!("Viewer open failed: {e}");
                        app.show_error(&e.to_string());
                    }
                }
            });
        });
    }

    pub fn handle_unwatch_peer(&mut self, client_id: u64) {
        if let Some(viewer) = self.viewers.remove(&client_id) {
            viewer.close();
        }
        self.screen
            .tx
            .send(ScreenControl::StopWatching { client_id })
            .ok();
        self.network
            .tx
            .send(NetworkControl::UnsubscribeScreen {
                target_id: client_id,
            })
            .ok();
        let username = self.username_of(client_id);
        self.push_activity(&t!("chat-stopped-watching", "name" => username.as_str()));
    }

    pub fn set_stream_volume(&mut self, client_id: u64, volume: f32) {
        self.screen
            .tx
            .send(ScreenControl::SetStreamVolume { client_id, volume })
            .ok();
    }

    pub fn set_stream_muted(&mut self, client_id: u64, muted: bool) {
        self.screen
            .tx
            .send(ScreenControl::MuteStream { client_id, muted })
            .ok();
    }

    // ════════════════════════════════════════════════════════════════
    //  DEVICES
    // ════════════════════════════════════════════════════════════════
}
