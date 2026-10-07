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
        self.users.get(&client_id).map_or_else(
            || t!("fallback-username", "id" => client_id.to_string()),
            |u| u.username.clone(),
        )
    }

    /// The server-side identity behind roster row `index`, as an admin
    /// command names it.
    pub(super) fn admin_target_at(&self, index: i32) -> Option<AdminTarget> {
        let client_id = self.user_id_at(index)?;
        let (user_id, _) = self.details.get(&client_id)?;
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
        let current = self.users.get(&client_id).map(|u| u.channel_id);
        (current != Some(channel_id)).then_some((client_id, channel_id))
    }

    /// The drop: the same `MoveUser` the "Move to" submenu sends,
    /// the server judging permission and the target's gates.
    pub(super) fn move_user_to(&mut self, client_id: u64, channel_id: u16) {
        let Some((user_id, _)) = self.details.get(&client_id) else {
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
        let has = |bit: u64| p::has(self.permissions, bit);
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
            our_rank: self.our_rank(),
        }
    }

    /// A roster row asked for its menu (a right-click, or a left-click
    /// on a user row). The decorated row already carries the rank
    /// rule and the channel powers.
    pub(super) fn open_user_menu(&mut self, index: i32, x: f32, y: f32) {
        let Some(row) = usize::try_from(index)
            .ok()
            .and_then(|i| self.users_model.row_data(i))
        else {
            return;
        };
        let session = self.menu_session();
        if row.is_channel {
            let Ok(id) = u16::try_from(row.channel_id) else {
                return;
            };
            let facts = menu::ChannelFacts {
                id,
                owned: row.owned,
                from_config: row.from_config,
            };
            let entries = menu::channel_menu(&session, &facts);
            self.show_menu(entries, MenuTarget::Channel(id), x, y);
        } else {
            let facts = menu::UserFacts {
                is_us: row.is_us,
                role: Role::from_int(row.role).unwrap_or_default(),
                sharing: row.sharing,
                local_muted: row.local_muted,
                outranked: row.outranked,
                is_admin: row.role == 4,
                role_editable: row.role_editable,
                channel_kickable: row.channel_kickable,
                channel_id: u16::try_from(row.channel_id).unwrap_or(0),
            };
            let channels: Vec<menu::ChannelChoice> = self
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
                .and_then(|id| self.details.get(&id))
                .map(|(_, roles)| roles.clone())
                .unwrap_or_default();
            let mut roles: Vec<menu::RoleChoice> = self
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
                .and_then(|id| self.users.get(&id))
                .map_or(1.0, |u| u.audio.volume);
            self.main.global::<State>().set_menu_volume(volume);
            self.show_menu(entries, MenuTarget::User(index), x, y);
        }
    }

    /// The slider in a user's menu moved: every move reaches the
    /// audio engine so it is heard at once; the release also persists
    /// it and refreshes the roster (the badge). The menu stays open.
    pub(super) fn menu_volume(&mut self, volume: f32, released: bool) {
        let Some(MenuTarget::User(index)) = self.menu_target else {
            return;
        };
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        if released {
            let username = self.username_of(client_id);
            self.handle_set_user_volume(client_id, &username, volume);
            return;
        }
        let mut audio = self
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
        let facts = menu::MessageFacts {
            kind,
            own: row.own,
            deleted: row.deleted,
            styled: row.styled,
            selecting,
            in_dm: self.active_dm.is_some(),
            delete_any: client::permissions::has(
                self.permissions,
                client::permissions::DELETE_MESSAGES,
            ),
        };
        let target = MenuTarget::Message {
            row_id: row.row_id,
            msg_id: roster::join_id(row.msg_id_lo, row.msg_id_hi),
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
        let (main, submenus) = ui::convert::menu_entries(&entries, &menu_label);
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
    }

    /// Escape, a click outside, or a pick: the overlay goes, and the
    /// composer takes back the focus the overlay's key scope held.
    pub(super) fn close_menu(&mut self) {
        self.menu_target = None;
        let s = self.main.global::<State>();
        if s.get_menu_open() {
            s.set_menu_open(false);
            if s.get_phase() == Phase::Main {
                self.main.invoke_focus_composer();
            }
        }
    }

    /// An entry was picked: resolve it against the menu's target.
    pub(super) fn menu_activated(&mut self, action: MenuAction, arg: i32) {
        use menu::Action as A;
        let target = self.menu_target.take();
        self.close_menu();
        let (Some(target), Some(action)) = (target, ui::convert::menu_action_back(action, arg))
        else {
            return;
        };
        match (target, action) {
            (MenuTarget::User(index), action) => self.user_menu_action(index, action),
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
            (MenuTarget::Composer, A::Cut) => self.main.invoke_composer_cut(),
            (MenuTarget::Composer, A::Copy) => self.main.invoke_composer_copy(),
            (MenuTarget::Composer, A::Paste) => media::paste_into_chat(self),
            _ => {}
        }
    }

    /// The audio, message and moderation entries of a user row.
    pub(super) fn user_menu_action(&mut self, index: i32, action: menu::Action) {
        use menu::Action as A;
        match action {
            A::MuteForMe => {
                if let Some(id) = self.user_id_at(index) {
                    let username = self.username_of(id);
                    let muted = self.users.get(&id).is_some_and(|u| u.audio.is_muted);
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
        let (Some((user_id, _)), Some(user)) =
            (self.details.get(&client_id), self.users.get(&client_id))
        else {
            return;
        };
        self.network
            .tx
            .send(NetworkControl::ChannelKick {
                channel_id: user.channel_id,
                user_id: *user_id,
            })
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
        self.moderation = Some(Moderation {
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
        let Some(m) = self.moderation.take() else {
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
        let Some((user_id, _)) = self.details.get(&m.client_id) else {
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
        self.moderation = None;
        let s = self.main.global::<State>();
        s.set_moderation_open(false);
        s.set_moderation_reason("".into());
    }

    pub(super) fn handle_mute_user(&mut self, client_id: u64, username: &str, muted: bool) {
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio.is_muted = muted;
            self.settings
                .peer_settings
                .insert(peer_key(username), peer_audio_to(user.audio));
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.users
                    .get(&client_id)
                    .map(|u| peer_audio_to(u.audio))
                    .unwrap_or_default(),
            ))
            .ok();
        self.settings.save();
        self.update_user_list();
    }

    pub(super) fn handle_set_user_volume(&mut self, client_id: u64, username: &str, volume: f32) {
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio.volume = volume;
            self.settings
                .peer_settings
                .insert(peer_key(username), peer_audio_to(user.audio));
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                self.users
                    .get(&client_id)
                    .map(|u| peer_audio_to(u.audio))
                    .unwrap_or_default(),
            ))
            .ok();
        self.settings.save();
        self.update_user_list();
    }

    pub(super) fn handle_reset_peer_audio(&mut self, client_id: u64, username: &str) {
        self.settings.peer_settings.remove(&peer_key(username));
        if let Some(user) = self.users.get_mut(&client_id) {
            user.audio = PeerAudio::default();
        }
        self.voice
            .tx
            .send(VoiceControl::SetPeerAudio(
                client_id,
                PeerAudioSettings::default(),
            ))
            .ok();
        self.settings.save();
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
                        viewer.set_self_audio(app.is_muted, app.is_deafened);
                        app.viewers.insert(client_id, viewer);
                        app.network
                            .tx
                            .send(NetworkControl::SubscribeScreen {
                                target_id: client_id,
                            })
                            .ok();
                        app.push_chat_system(&t!("chat-watching", "name" => username.as_str()));
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
        self.network
            .tx
            .send(NetworkControl::UnsubscribeScreen {
                target_id: client_id,
            })
            .ok();
        let username = self.username_of(client_id);
        self.push_chat_system(&t!("chat-stopped-watching", "name" => username.as_str()));
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
