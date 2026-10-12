//! Worker events into the UI: the drain, voice and screen events, and the big `NetworkMessage` dispatch.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    /// Drain every worker channel. Bounded like the FLTK loop was, and
    /// `FrameReady` is coalesced per stream: if frames queued while the
    /// UI thread was busy, blitting each stale frame just replays the
    /// past — show the newest, skip the rest.
    pub(super) fn drain(app: &Rc<RefCell<AppState>>) {
        let mut st = app.borrow_mut();

        // Global hotkeys — collect first, then handle.
        let mut hotkey_actions = Vec::new();
        if let Some(hotkeys) = &st.hotkeys {
            while let Some(action) = hotkeys.poll() {
                hotkey_actions.push(action);
            }
        }
        for (action, pressed) in hotkey_actions {
            if !st.connected {
                continue;
            }
            match (action, pressed) {
                // A hold, not a toggle: both edges matter.
                (BindableAction::PushToTalk, held) => {
                    st.voice.tx.send(VoiceControl::PushToTalkHeld(held)).ok();
                    st.main.global::<State>().set_ptt_held(held);
                }
                (_, false) => {}
                (BindableAction::ToggleMute, true) => st.handle_toggle_mute(),
                (BindableAction::ToggleDeafen, true) => st.handle_toggle_deafen(),
                (BindableAction::ToggleScreenShare, true) => st.handle_toggle_screenshare(),
            }
        }

        for _ in 0..256 {
            match st.network.msg_rx.try_recv() {
                Ok(msg) => st.handle_network_message(msg),
                Err(_) => break,
            }
        }

        for _ in 0..32 {
            match st.voice.event_rx.try_recv() {
                Ok(evt) => st.handle_voice_event(evt),
                Err(_) => break,
            }
        }

        // Non-frame screen events, forwarded by the present-prep
        // thread (which owns screen.event_rx now — see present.rs).
        for _ in 0..64 {
            match st.ui_screen_rx.try_recv() {
                Ok(evt) => st.handle_screen_event(evt),
                Err(_) => break,
            }
        }
        // Frames arrive as ready-made SharedPixelBuffers: the copy was
        // paid on the prep thread; presenting is an Image wrap + a
        // property set. Newest-per-stream (older frames were coalesced
        // prep-side, before anyone paid for their conversion).
        let prepared: Vec<crate::present::PreparedFrame> = {
            let mut slots = st.present.slots.lock().unwrap();
            slots.drain().map(|(_, f)| f).collect()
        };
        for frame in prepared {
            st.present_frame(frame);
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  EVENT HANDLERS
    // ════════════════════════════════════════════════════════════════

    pub(super) fn handle_voice_event(&mut self, evt: VoiceEvent) {
        match evt {
            VoiceEvent::Started => {
                // No sound here: this event fires whenever the engine
                // boots, including for standalone mic tests. The join
                // ding is sent from handle_init_client instead.
                info!("Voice engine ready");
            }
            VoiceEvent::Error(e) => self.show_error(&e),
            // Our own speech shows from this local edge, at once; the
            // server's echo of it is ignored (`UserSpeaking`).
            VoiceEvent::SpeakingChanged(speaking) => {
                if let Some(id) = self.session.our_client_id {
                    if let Some(user) = self.session.users.get_mut(&id) {
                        user.is_speaking = speaking;
                    }
                    self.refresh_voice_row(id);
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    /// Present one prepared frame: wrap the (already-copied) pixel
    /// buffer in an Image handle and hand it to the viewer window. The
    /// heavy work happened on the prep thread; this path stays well
    /// under a millisecond, which is what lets the UI keep up with
    /// 60 fps streams.
    pub(super) fn present_frame(&mut self, f: crate::present::PreparedFrame) {
        let Some(viewer) = self.viewers.get_mut(&f.meta.client_id) else {
            return;
        };
        viewer.update_frame(f.buffer, &f.meta);
        // For the stream's summary line (client::viewlog): decoded →
        // handed to the UI, this side's whole share of a frame's way.
        let to_screen = u64::try_from(f.meta.decoded_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Ok(mut tallies) = self.screen.shared.present.lock() {
            let tally = tallies.entry(f.meta.client_id).or_default();
            tally.shown += 1;
            tally.to_screen_us = tally.to_screen_us.saturating_add(to_screen);
            tally.to_screen_max_us = tally.to_screen_max_us.max(to_screen);
        }
    }

    pub(super) fn handle_screen_event(&mut self, evt: ScreenEvent) {
        match evt {
            ScreenEvent::SharingStarted { quality } => {
                info!("Screen share started: {quality}");
                self.push_activity(&t!("chat-share-started", "quality" => quality.to_string()));
                self.main.global::<State>().set_sharing(true);
                // The stats line's 1-second timer, for as long as the
                // share lasts. (It was never stopped: one tick a second
                // for good after the first share, and one more timer
                // with every share after it — review P2-38.)
                let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
                if let Some(weak) = weak {
                    self.stream_stats_timer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_secs(1),
                        move || with_app(&weak, AppState::update_stream_stats),
                    );
                }
            }
            ScreenEvent::SharingStopped => {
                info!("Screen share stopped");
                self.push_activity(&t!("chat-share-stopped"));
                self.main.global::<State>().set_sharing(false);
                self.stop_stream_stats();
            }
            ScreenEvent::Error(e) => {
                self.show_error(&t!("error-screen-share", "error" => e.as_str()));
            }
            // Frames never reach this handler anymore: the
            // present-prep thread consumes them from screen.event_rx
            // and the UI receives PreparedFrames through the slot map
            // instead (see present.rs / present_frame below).
            ScreenEvent::FrameReady { .. } => {
                debug_assert!(false, "FrameReady is consumed by the present-prep thread");
            }
            ScreenEvent::PeerStartedSharing {
                client_id,
                width,
                height,
            } => {
                let username = self.username_of(client_id);
                info!("{username} started sharing ({width}x{height})");
                if !self.client_blocked(client_id) {
                    self.push_activity(&t!("chat-peer-sharing", "name" => username.as_str()));
                }
            }
            ScreenEvent::PeerStoppedSharing { client_id } => {
                let username = self.username_of(client_id);
                info!("{username} stopped sharing");
                if !self.client_blocked(client_id) {
                    self.push_activity(
                        &t!("chat-peer-stopped-sharing", "name" => username.as_str()),
                    );
                }
                if let Some(viewer) = self.viewers.remove(&client_id) {
                    viewer.close();
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_network_message(&mut self, msg: NetworkMessage) {
        match msg {
            NetworkMessage::Connected { lobby, safety } => {
                self.connected = true;
                // Keybinds act only in a session: watched from here to
                // `teardown_connection` (review P1-3).
                if let Some(hotkeys) = &mut self.hotkeys {
                    hotkeys.set_connected(true);
                }
                self.lobby = lobby;
                self.session.safety.on = safety;
                self.account_deleted = false;
                self.reconnect_attempts = 0;
                self.main.global::<State>().set_error_text("".into());
                self.clear_connect_verdicts();
                // Admitted: from now on the key is the login, so the
                // code has done its one job.
                if let Some(server) = self.settings.default_server_mut()
                    && server.invite_code.take().is_some()
                {
                    self.settings.save();
                    crate::settingsui::refresh_servers(self);
                }
                crate::settingsui::refresh_safety(self);
            }
            NetworkMessage::Session {
                user_id,
                permissions,
                persistent,
            } => {
                self.session.permissions = permissions;
                self.session.our_user_id = Some(user_id);
                self.session.server_persistent = persistent;
                log::info!(
                    "session: user {}, permissions {permissions:#x}, persistent {persistent}",
                    short_id(&user_id)
                );
                self.push_permissions();
                self.send_own_profile();
            }
            NetworkMessage::NeedInvite { why } => {
                let s = self.main.global::<State>();
                match why {
                    client::NeedInviteReason::UnknownKey => {
                        info!("Connect refused: this device needs an invite");
                        s.set_invite_prompt_text(t!("connect-need-invite").into());
                        s.set_invite_prompt(true);
                    }
                    client::NeedInviteReason::KeyRetired => {
                        info!("Connect refused: this device was unlinked");
                        s.set_invite_prompt(false);
                        s.set_connect_notice(t!("connect-key-retired").into());
                    }
                }
            }
            NetworkMessage::InviteInvalid { why } => {
                info!("Connect refused: invite {why:?}");
                // Whatever code we stored is no good; do not resend it.
                if let Some(server) = self.settings.default_server_mut() {
                    server.invite_code = None;
                }
                self.settings.save();
                let s = self.main.global::<State>();
                s.set_invite_prompt_text(
                    t!(match why {
                        client::InviteInvalidReason::Expired => "connect-invite-expired",
                        client::InviteInvalidReason::Revoked => "connect-invite-revoked",
                        client::InviteInvalidReason::Exhausted => "connect-invite-exhausted",
                        client::InviteInvalidReason::Unknown => "connect-invite-unknown",
                    })
                    .into(),
                );
                s.set_invite_code("".into());
                s.set_invite_prompt(true);
            }
            NetworkMessage::NameInUse => {
                info!("Connect refused: name in use");
                self.main
                    .global::<State>()
                    .set_connect_notice(t!("connect-name-in-use").into());
            }
            NetworkMessage::InviteCreated {
                code,
                grants_role,
                expires_at,
                max_uses,
            } => {
                // A newcomer needs the server address, the port and this
                // code, as plain text; the code goes on the clipboard.
                let shown = client::invite::display_code(&code);
                crate::clipboard::set_clipboard_text(&shown);
                let role = self.role_name_of(grants_role);
                let expiry = describe_expiry(expires_at);
                let uses = match max_uses {
                    Some(n) => t!("invite-uses", "count" => n),
                    None => t!("invite-uses-unlimited"),
                };
                let s = self.main.global::<State>();
                if s.get_invite_dialog_open() {
                    // Minted from the dialog: it shows there, to copy.
                    s.set_invite_dialog_code(shown.into());
                    s.set_invite_dialog_summary(format!("{role} · {expiry} · {uses}").into());
                    s.set_invite_dialog_error("".into());
                } else {
                    self.push_command_output(&t!(
                        "chat-invite-created",
                        "code" => shown.as_str(),
                        "role" => role.as_str(),
                        "expiry" => expiry.as_str(),
                        "uses" => uses.as_str()
                    ));
                }
            }
            NetworkMessage::InviteDenied { reason } => {
                let line = t!(match reason {
                    client::InviteDenyReason::NoPermission => "chat-invite-no-permission",
                    client::InviteDenyReason::RoleNotAllowed => "chat-invite-role-not-allowed",
                });
                let s = self.main.global::<State>();
                if s.get_invite_dialog_open() {
                    s.set_invite_dialog_error(line.into());
                } else {
                    self.push_command_output(&line);
                }
            }
            NetworkMessage::History {
                conv,
                messages,
                more,
            } => {
                // A page lands in its own conversation, active or
                // parked; a DM's is always one we asked for.
                let key = match conv {
                    client::ConvRef::General => None,
                    client::ConvRef::Dm(user) => Some(user),
                };
                if let Some(user) = key
                    && let Some(meta) = self.session.chat.dm_meta.get_mut(&user)
                {
                    meta.fetched = true;
                }
                self.with_conversation(key, |app| app.take_history_page(key, &messages, more));
            }
            NetworkMessage::HistoryUnavailable { retry_secs, .. } => {
                self.session.chat.history_page_pending = None;
                self.main.global::<State>().set_history_loading(false);
                self.push_chat_system(&t!("chat-history-unavailable", "secs" => retry_secs));
            }
            NetworkMessage::MessageEdited { msg_id, text } => {
                if self.session.chat.pending_edit == Some(msg_id) {
                    self.session.chat.pending_edit = None;
                }
                // A blocked person's message: not shown, not narrated.
                if self.session.safety.hidden_messages.contains(&msg_id) {
                    return;
                }
                if let Some(own) = self.session.chat.own_texts.get_mut(&msg_id) {
                    own.clone_from(&text);
                }
                let text = app_core::emoji::replace_shortcodes(&text);
                // In place when the message is on screen; a notice
                // when it scrolled out of the window (or predates it).
                let edited = self.with_conversation(None, |app| {
                    match app.history.edit_message(msg_id, &text) {
                        Some(delta) => {
                            app.sync_range(&delta);
                            true
                        }
                        None => false,
                    }
                });
                if !edited {
                    self.push_chat_system(&t!(
                        "chat-message-edited",
                        "id" => msg_id,
                        "text" => text.as_str()
                    ));
                }
            }
            NetworkMessage::MessageDeleted { msg_id } => {
                if self.session.chat.pending_delete == Some(msg_id) {
                    self.session.chat.pending_delete = None;
                }
                if self.session.safety.hidden_messages.remove(&msg_id) {
                    return;
                }
                self.session.chat.own_texts.remove(&msg_id);
                if self
                    .session
                    .chat
                    .editing
                    .as_ref()
                    .is_some_and(|e| e.msg_id == msg_id)
                {
                    self.edit_cancel();
                }
                // Off screen (scrolled out, or never shown): nothing to
                // say — an account's deletion takes up to 200 at once.
                self.with_conversation(None, |app| {
                    if let Some(delta) = app.history.delete_message(msg_id) {
                        app.sync_range(&delta);
                    }
                });
            }
            NetworkMessage::ChatRateLimited { retry_ms } => {
                let secs = retry_ms.div_ceil(1000);
                self.show_toast(ToastKind::Warning, &t!("chat-rate-limited", "secs" => secs));
            }
            NetworkMessage::ProgressUpdate { xp, level } => {
                if let Some(user) = self
                    .session
                    .our_client_id
                    .and_then(|id| self.session.users.get_mut(&id))
                {
                    user.xp = Some(xp);
                    user.level = level;
                }
                self.update_user_list();
                let line = t!("chat-level-up", "level" => level, "xp" => xp);
                self.push_activity(&line);
                self.show_toast(ToastKind::Success, &line);
            }
            NetworkMessage::UserProgress {
                client_id,
                xp,
                level,
            } => {
                if let Some(user) = self.session.users.get_mut(&client_id) {
                    user.xp = Some(xp);
                    user.level = level;
                    self.update_user_list();
                }
            }
            NetworkMessage::RoleList { roles } => {
                self.session.roles = roles.into_iter().map(|r| (r.id, r)).collect();
                // Custom names on the cards, and in the roster's menu.
                self.update_user_list();
            }
            NetworkMessage::AuditLine { entry } => {
                self.ensure_admin_channel();
                let line = audit_line_text(&entry);
                let open = self.session.chat.active_dm == Some(ADMIN_CHANNEL_KEY);
                self.with_conversation(Some(ADMIN_CHANNEL_KEY), |app| {
                    app.push_system_here(&line);
                });
                if !open && let Some(meta) = self.session.chat.dm_meta.get_mut(&ADMIN_CHANNEL_KEY) {
                    meta.unread += 1;
                    self.rebuild_dms_model();
                }
            }
            NetworkMessage::FileMessage {
                conv,
                from,
                username,
                id,
                ts,
                client_id,
                attachment,
            } => {
                if self.is_blocked(&from) {
                    self.hide_message(id);
                    return;
                }
                let key = match conv {
                    client::ConvRef::General => None,
                    client::ConvRef::Dm(_) => Some(from),
                };
                let file = app_core::chat::FileMeta {
                    blob: attachment.blob_id,
                    name: attachment.name.clone(),
                    mime: attachment.mime.clone(),
                    size: attachment.size,
                };
                match conv {
                    client::ConvRef::General => {
                        let meta = Meta {
                            id,
                            ts: format_wall_time(ts),
                            ts_ms: ts,
                            day: local_day(ts),
                            user: username,
                            author: Some(from),
                            own: Some(client_id) == self.session.our_client_id,
                            pending: false,
                        };
                        // Our own post came back: the ack already turned
                        // the placeholder into this message.
                        let already = self.with_conversation(None, |app| app.history.has(id));
                        if !already {
                            self.want_picture(&attachment, key, id);
                            self.push_chat_file(&meta, file);
                        }
                    }
                    client::ConvRef::Dm(_) => {
                        self.want_picture(&attachment, key, id);
                        self.take_direct(from, &username, id, ts, Incoming::File(file));
                    }
                }
            }
            NetworkMessage::HistoryAttachments { conv, items } => {
                let key = match conv {
                    client::ConvRef::General => None,
                    client::ConvRef::Dm(user) => Some(user),
                };
                for (msg_id, a) in &items {
                    self.want_picture(a, key, *msg_id);
                }
                self.with_conversation(key, |app| {
                    for (msg_id, a) in items {
                        let file = app_core::chat::FileMeta {
                            blob: a.blob_id,
                            name: a.name,
                            mime: a.mime,
                            size: a.size,
                        };
                        if let Some(i) = app.history.attach_file(msg_id, file) {
                            let row = app.chat_row_at(i);
                            app.chat_model.set_row_data(i, row);
                        }
                    }
                    // Cards are taller than the empty rows they replace.
                    app.main.invoke_chat_follow_appended();
                });
            }
            NetworkMessage::ChannelDetails {
                channel_id,
                stable_id,
                owner,
                min_role,
                permanent,
                afk_after_min,
                is_afk,
            } => {
                self.session.channel_details.insert(
                    channel_id,
                    ChannelFacts {
                        stable_id,
                        owner,
                        min_role,
                        permanent,
                        afk_after_min,
                        is_afk,
                    },
                );
                // One per channel after every list: cheap to re-derive
                // the sidebar each time, and the gates are right the
                // moment the last one lands.
                self.rebuild_channels();
                self.update_user_list();
            }
            NetworkMessage::ChannelDenied { reason } => {
                let what = t!(channel_denied_key(reason));
                if let Some(dialog) = self.session.chat.channel_dialog.as_mut()
                    && dialog.pending
                {
                    dialog.pending = false;
                    self.main
                        .global::<State>()
                        .set_channel_dialog_error(what.into());
                } else {
                    self.show_error(&what);
                    self.push_chat_system(&what);
                }
            }
            NetworkMessage::MovedIdle { after_min } => {
                let line = t!("chat-moved-idle", "min" => after_min);
                self.push_activity(&line);
                self.show_toast(ToastKind::Info, &line);
            }
            NetworkMessage::UserDetails {
                client_id,
                user_id,
                roles,
                level,
            } => {
                let before = self
                    .session
                    .details
                    .insert(client_id, (user_id, roles.clone()));
                if let Some(user) = self.session.users.get_mut(&client_id) {
                    user.role = role_of(&roles);
                    user.level = level;
                }
                self.apply_saved_peer_audio(client_id, &user_id);
                if self.session.chat.dm_meta.contains_key(&user_id) {
                    self.rebuild_dms_model();
                }
                if Some(client_id) == self.session.our_client_id {
                    // Our own rank decides which gated channels we see
                    // and which roles the menu may hand out.
                    self.rebuild_channels();
                    self.push_permissions();
                }
                self.update_user_list();
                if Some(client_id) == self.session.our_client_id
                    && let Some((_, old)) = before
                    && old != roles
                {
                    let names: Vec<String> = roles.iter().map(|r| self.role_name_of(*r)).collect();
                    let now = if names.is_empty() {
                        t!("roles-none")
                    } else {
                        names.join(", ")
                    };
                    let what = t!("chat-your-roles-changed", "roles" => now.as_str());
                    self.push_activity(&what);
                    self.show_toast(ToastKind::Info, &what);
                }
            }
            NetworkMessage::Restricted {
                kind,
                until,
                reason,
            } => {
                let until_ms = until;
                // "… for 2 h: spam" / "… indefinitely: spam" / a role gap
                // has neither: "… — not allowed by your role".
                let by_role = reason.is_empty() && until.is_none();
                let until = if by_role {
                    String::new()
                } else {
                    format!(" {}", describe_until(until))
                };
                let reason = if by_role {
                    format!(" — {}", t!("restricted-by-role"))
                } else if reason.is_empty() {
                    String::new()
                } else {
                    format!(": {reason}")
                };
                let what = t!(
                    restricted_key(kind),
                    "until" => until.as_str(),
                    "reason" => reason.as_str()
                );
                self.push_chat_system(&what);
                self.show_toast(ToastKind::Warning, &what);
                match kind {
                    // A server mute: stop sending (spec §4.2), and keep
                    // the button locked until it lifts.
                    client::RestrictionKind::Voice => {
                        if self.session.server_muted.is_none() {
                            self.session.server_muted = Some(self.session.is_muted);
                        }
                        if !self.session.is_muted {
                            self.set_muted(true);
                        }
                    }
                    client::RestrictionKind::GeneralText => {
                        self.session.text_muted = Some(TextMute { until: until_ms });
                    }
                    _ => {}
                }
            }
            NetworkMessage::RestrictionLifted { kind } => {
                self.push_chat_system(&t!(lifted_key(kind)));
                match kind {
                    client::RestrictionKind::Voice => {
                        if let Some(was_muted) = self.session.server_muted.take()
                            && !was_muted
                        {
                            self.set_muted(false);
                        }
                    }
                    client::RestrictionKind::GeneralText => self.session.text_muted = None,
                    _ => {}
                }
            }
            NetworkMessage::AdminResult { request_id, result } => {
                self.handle_admin_result(request_id, result);
            }
            NetworkMessage::ConnectionLost { kind, detail } => {
                const MAX_AUTO_RECONNECTS: u32 = 5;
                // Network, not credentials: come back on our own, with
                // backoff (2, 4, 8, 16, 30 s), instead of dumping the
                // user on the connect screen with a transport error.
                // The user sees one plain sentence per kind; the
                // transport's wording goes to the log only.
                log::warn!("connection lost ({kind:?}): {detail}");
                let what = t!(match kind {
                    LossKind::Timeout => "error-lost-timeout",
                    LossKind::Reset => "error-lost-reset",
                    LossKind::Unreachable => "error-lost-unreachable",
                    LossKind::ServerRestart => "error-lost-restart",
                });
                self.push_chat_system(&what);
                // Where to go back to, if the reconnect is automatic.
                // Only a live session knows: a retry that failed in turn
                // is not in any room, and used to overwrite the room
                // saved when the session was lost (review P2-14).
                if self.connected {
                    self.rejoin_channel = if self.lobby && !self.in_lobby() {
                        self.our_channel().map(|c| self.channel_name(c))
                    } else {
                        None
                    };
                }
                // In a room the host was holding the device awake.
                let was_in_room = self.kept_alive == Some(true);
                self.teardown_connection();
                // Retrying: the host keeps the process alive meanwhile
                // (on a phone in the background, the retry could not run
                // otherwise).
                self.reconnect_hold = self.reconnect_attempts < MAX_AUTO_RECONNECTS;
                if self.reconnect_hold && was_in_room {
                    // Said again, as it was: Android's wake lock for a
                    // room runs out by itself (review P2-28) and the
                    // audio streams that kept the device awake after
                    // that are closed now, so the retries get a new one.
                    crate::host::keep_alive(Some(&self.server_name), true);
                }
                self.show_connect_phase();
                if self.reconnect_attempts >= MAX_AUTO_RECONNECTS {
                    self.show_error(&format!("{what} {}", t!("error-lost-gave-up")));
                } else {
                    self.reconnect_attempts += 1;
                    let secs = (2u64 << (self.reconnect_attempts - 1)).min(30);
                    let secs_text = secs.to_string();
                    self.show_toast(
                        ToastKind::Warning,
                        &format!(
                            "{what} {}",
                            t!("error-reconnecting", "secs" => secs_text.as_str())
                        ),
                    );
                    let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
                    if let Some(weak) = weak {
                        self.reconnect_timer.start(
                            slint::TimerMode::SingleShot,
                            Duration::from_secs(secs),
                            move || {
                                with_app(&weak, |app| {
                                    if !app.connected {
                                        let nickname = app.settings.user.nickname.clone();
                                        app.handle_connect(&nickname);
                                    }
                                });
                            },
                        );
                    }
                }
            }
            NetworkMessage::ServerIdentity {
                fingerprint,
                first_use,
            } => {
                // Trust on first use (or a signed key rotation): pin it
                // on the server entry so a different key is refused
                // from now on.
                let changed = self
                    .settings
                    .default_server()
                    .is_none_or(|s| s.fingerprint.as_deref() != Some(fingerprint.as_str()));
                if changed {
                    if let Some(server) = self.settings.default_server_mut() {
                        server.fingerprint = Some(fingerprint.clone());
                    }
                    self.settings.save();
                    crate::settingsui::refresh_servers(self);
                    if first_use {
                        info!("Pinned server identity {fingerprint}");
                        self.push_activity(
                            &t!("chat-identity-pinned", "fingerprint" => fingerprint.as_str()),
                        );
                    }
                }
            }
            NetworkMessage::IdentityChanged { expected, observed } => {
                // Nothing was sent to that server. No silent accept: the
                // user decides, by editing (or clearing) the pin.
                let text = t!(
                    "error-identity-changed",
                    "expected" => expected.as_str(),
                    "observed" => observed.as_str()
                );
                self.push_chat_system(&text);
                self.teardown_connection();
                self.show_connect_phase();
                self.show_error(&text);
            }
            NetworkMessage::Disconnected { kind, detail } => {
                log::info!("disconnected ({kind:?}): {detail}");
                let what = match &kind {
                    DisconnectKind::UserRequested => t!("disconnect-reason-user"),
                    DisconnectKind::Refused(client::RefusedReason::ServerFull) => {
                        t!("error-refused-full")
                    }
                    DisconnectKind::Refused(client::RefusedReason::RateLimited) => {
                        t!("error-refused-rate-limited")
                    }
                    DisconnectKind::Refused(client::RefusedReason::Banned { until, reason }) => {
                        let until = describe_until(*until);
                        let reason = if reason.is_empty() {
                            t!("reason-none")
                        } else {
                            reason.clone()
                        };
                        t!(
                            "error-banned",
                            "reason" => reason.as_str(),
                            "until" => until.as_str()
                        )
                    }
                    DisconnectKind::Refused(_) | DisconnectKind::Outdated => {
                        t!("error-outdated")
                    }
                    DisconnectKind::Removed { kind, reason, by } => {
                        let reason = if reason.is_empty() {
                            t!("reason-none")
                        } else {
                            reason.clone()
                        };
                        let key = match kind {
                            client::RemovedKind::Disconnected => "removed-disconnected",
                            client::RemovedKind::Kicked => "removed-kicked",
                            client::RemovedKind::Banned { .. } => "removed-banned",
                            client::RemovedKind::Replaced => "removed-replaced",
                        };
                        let until = match kind {
                            client::RemovedKind::Banned { until } => describe_until(*until),
                            _ => String::new(),
                        };
                        t!(
                            key,
                            "by" => by.as_str(),
                            "reason" => reason.as_str(),
                            "until" => until.as_str()
                        )
                    }
                    DisconnectKind::LocalError => {
                        t!("error-local", "detail" => detail.as_str())
                    }
                    DisconnectKind::Other => t!("error-lost-generic"),
                };
                // The removal after our own account deletion is that,
                // not a kick: say what happened instead.
                let what = if std::mem::take(&mut self.account_deleted)
                    && matches!(kind, DisconnectKind::Removed { .. })
                {
                    self.account_deleted_notice()
                } else {
                    what
                };
                if self.connected {
                    self.push_chat_system(&t!("chat-disconnected", "reason" => what.as_str()));
                    self.teardown_connection();
                    self.show_connect_phase();
                } else {
                    // A retry that ended in a verdict: no reconnect is
                    // on its way any more, so the host lets go. The
                    // hold taken for the retry outlived it, and
                    // Android's "Connected" notification stayed up
                    // (review P2-14).
                    self.reconnect_hold = false;
                    self.kept_alive = None;
                    crate::host::keep_alive(None, false);
                }
                match kind {
                    DisconnectKind::UserRequested => {}
                    // Verdicts that stay on the card until the next try.
                    DisconnectKind::Refused(client::RefusedReason::Banned { .. })
                    | DisconnectKind::Removed { .. } => {
                        self.main.global::<State>().set_connect_notice(what.into());
                    }
                    // A full server is theirs, not ours: a warning.
                    DisconnectKind::Refused(client::RefusedReason::ServerFull) => {
                        self.show_toast(ToastKind::Warning, &what);
                    }
                    _ => self.show_error(&what),
                }
            }
            NetworkMessage::InitClient {
                users,
                client_id,
                server_name,
                greeting,
                channels,
                memberships,
            } => {
                self.handle_init_client(
                    client_id,
                    &users,
                    &server_name,
                    &greeting,
                    &channels,
                    &memberships,
                );
            }
            NetworkMessage::UserJoined {
                client_id,
                username,
                channel_id,
            } => self.handle_user_joined(client_id, username, channel_id),
            NetworkMessage::UserMovedChannel {
                client_id,
                channel_id,
            } => self.handle_user_moved(client_id, channel_id),
            NetworkMessage::ChannelJoinDenied { channel_id, reason } => {
                self.handle_channel_join_denied(channel_id, reason);
            }
            NetworkMessage::ChannelListUpdated {
                channels,
                memberships,
            } => self.handle_channels_updated(&channels, &memberships),
            NetworkMessage::UserLeft {
                client_id,
                username,
            } => self.handle_user_left(client_id, &username),
            NetworkMessage::TextMessage {
                client_id,
                id,
                ts,
                username,
                text,
                author,
            } => {
                // The server keeps a blocked person's posts from us;
                // this is the backstop (an older server, a race with a
                // block just made).
                let author = author.or_else(|| self.account_of(client_id));
                if author.is_some_and(|a| self.is_blocked(&a)) || self.client_blocked(client_id) {
                    self.hide_message(id);
                    return;
                }
                let meta = Meta {
                    id,
                    ts: format_wall_time(ts),
                    ts_ms: ts,
                    day: local_day(ts),
                    user: username,
                    author,
                    own: Some(client_id) == self.session.our_client_id,
                    pending: false,
                };
                if meta.own {
                    self.session.chat.own_texts.insert(id, text.clone());
                }
                // Render shortcodes from clients that send them raw.
                let text = app_core::emoji::replace_shortcodes(&text);
                self.push_chat_message(&meta, &text, false);
                // Ordinary messages are silent by design; a mention may
                // chime if the person asked for it.
                if !meta.own && self.settings.sounds.mention_on() && self.mentions_us(&text) {
                    self.alert_mention(&meta.user, &text);
                }
            }
            NetworkMessage::UploadProgress { upload, pct } => {
                // The ring on that upload's own card — a file's or an
                // image's (review P1-17). One that has no card (the
                // avatar) or whose card is gone shows nowhere.
                let card = match upload {
                    client::network::UploadKey::Image => {
                        self.session.transfers.image_upload.map(|id| (None, id))
                    }
                    client::network::UploadKey::Blob(upload_id) => self
                        .session
                        .transfers
                        .pending_uploads
                        .get(&upload_id)
                        .map(|u| (u.conv, u.placeholder)),
                };
                if let Some((conv, id)) = card {
                    self.set_transfer(conv, id, Some((1, i32::from(pct.min(99)))));
                }
            }
            NetworkMessage::ImageMessage {
                client_id,
                id,
                ts,
                username,
                data,
            } => {
                if self.client_blocked(client_id) {
                    self.hide_message(id);
                    return;
                }
                let meta = Meta {
                    id,
                    ts: format_wall_time(ts),
                    ts_ms: ts,
                    day: local_day(ts),
                    user: username.clone(),
                    author: self.account_of(client_id),
                    own: Some(client_id) == self.session.our_client_id,
                    pending: false,
                };
                if meta.own {
                    self.drop_image_card();
                }
                let shown =
                    self.with_conversation(None, |app| media::push_image_row(app, &meta, data));
                if !shown {
                    self.push_chat_system(
                        &t!("chat-image-undisplayable", "name" => username.as_str()),
                    );
                }
            }
            // Speaking and audio state touch one row, never the whole
            // roster (review P1-9).
            NetworkMessage::UserSpeaking {
                client_id,
                is_speaking,
            } => {
                // Ours is the local edge's (`SpeakingChanged`): handling
                // the echo too updated our row twice per edge.
                if Some(client_id) != self.session.our_client_id {
                    if let Some(user) = self.session.users.get_mut(&client_id) {
                        user.is_speaking = is_speaking;
                    }
                    self.refresh_voice_row(client_id);
                }
            }
            NetworkMessage::UserAudioState {
                client_id,
                muted,
                deafened,
            } => {
                if let Some(user) = self.session.users.get_mut(&client_id) {
                    user.is_self_muted = muted;
                    user.is_self_deafened = deafened;
                }
                self.refresh_voice_row(client_id);
            }
            NetworkMessage::NetworkStats {
                rtt_ms,
                loss_pct,
                kbps_up,
                kbps_down,
            } => {
                // Skip the label churn when nobody can see it (window in
                // the tray or minimized). Labels refresh at the next 2 s
                // sample once visible again.
                if self.main_on_screen() {
                    let audio_ms = self.voice.shared.jitter_buffer_ms();
                    self.update_latency(rtt_ms, loss_pct, Some(audio_ms), kbps_up, kbps_down);
                }
            }
            NetworkMessage::ShareDenied { reason } => {
                // The screen thread starts capturing optimistically
                // before the server answers — stop it so local state
                // matches the server's.
                if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
                    self.screen.tx.send(ScreenControl::StopSharing).ok();
                }
                let why = describe_deny(reason);
                self.show_error(&t!("share-denied", "why" => why.as_str()));
            }
            NetworkMessage::SubscribeDenied { target_id, reason } => {
                // Close the viewer window WatchPeer opened optimistically.
                if let Some(viewer) = self.viewers.remove(&target_id) {
                    viewer.close();
                }
                let username = self.username_of(target_id);
                let why = describe_deny(reason);
                self.push_chat_system(
                    &t!("watch-denied", "name" => username.as_str(), "why" => why.as_str()),
                );
            }
            NetworkMessage::ScreenShareStarted {
                client_id,
                width,
                height,
                scope: _,
            } => {
                self.session
                    .active_sharers
                    .insert(client_id, (width, height));
                self.update_user_list();
                // A stream going up in our channel — ours included: the
                // sharer hears that they are live, the same as the room.
                let in_our_channel = self
                    .session
                    .users
                    .get(&client_id)
                    .is_some_and(|u| Some(u.channel_id) == self.our_channel());
                if in_our_channel && self.settings.sounds.share_started_on() {
                    self.voice
                        .tx
                        .send(VoiceControl::PlaySound(SoundType::ShareStarted))
                        .ok();
                }
            }
            NetworkMessage::ScreenShareStopped { client_id } => {
                self.session.active_sharers.remove(&client_id);
                self.update_user_list();
            }
            // ── 1.1.2: profiles and blobs (spec §7, §7.1) ────────────
            NetworkMessage::UserProfile { client_id, profile } => {
                self.take_profile(client_id, profile);
            }
            NetworkMessage::ProfileUpdated { user_id, profile } => {
                let client = self
                    .session
                    .details
                    .iter()
                    .find(|(_, (u, _))| *u == user_id)
                    .map(|(c, _)| *c);
                if let Some(client_id) = client {
                    self.take_profile(client_id, profile);
                } else if let Some(name) = self
                    .session
                    .author_looks
                    .get(&user_id)
                    .map(|l| l.name.clone())
                {
                    // Someone not in the roster whose rows are on screen.
                    self.take_author_look(user_id, name, profile);
                }
            }
            NetworkMessage::Authors { authors } => self.take_authors(authors),
            NetworkMessage::ProfileDenied { reason } => {
                self.show_toast(ToastKind::Warning, &t!(profile_denied_key(reason)));
            }
            NetworkMessage::UserRenamed { client_id, name } => {
                let old = self.username_of(client_id);
                if let Some(user) = self.session.users.get_mut(&client_id) {
                    user.username.clone_from(&name);
                }
                // The Messages list and an open DM header follow a
                // partner's rename.
                let renamed_partner = self
                    .session
                    .details
                    .get(&client_id)
                    .map(|(u, _)| *u)
                    .filter(|u| self.session.chat.dm_meta.contains_key(u));
                if let Some(user_id) = renamed_partner {
                    if let Some(meta) = self.session.chat.dm_meta.get_mut(&user_id) {
                        meta.name.clone_from(&name);
                    }
                    self.rebuild_dms_model();
                    if self.session.chat.active_dm == Some(user_id) {
                        self.main
                            .global::<State>()
                            .set_conv_name(name.as_str().into());
                    }
                }
                // Every row they authored, in every conversation, takes
                // the new name (their look follows: profiles are keyed
                // by session, and the roster row now carries the name).
                self.rename_author(&old, &name);
                self.push_activity(&t!(
                    "chat-user-renamed",
                    "old" => old.as_str(),
                    "new" => name.as_str()
                ));
                self.update_user_list();
            }
            NetworkMessage::RenameDenied { reason } => {
                self.show_toast(ToastKind::Warning, &t!(rename_denied_key(reason)));
            }
            NetworkMessage::UploadDone { upload_id, blob_id } => {
                if let Some(upload) = self.session.transfers.pending_uploads.remove(&upload_id) {
                    self.file_uploaded(upload_id, upload, blob_id);
                } else if upload_id == AVATAR_UPLOAD_ID {
                    // The picture is on the server: now the profile
                    // can point at it.
                    self.session.own_avatar_blob = Some(blob_id);
                    self.avatar_images
                        .entry(blob_id)
                        .or_insert_with(|| self.own_avatar.clone().unwrap_or_default());
                    self.send_own_profile();
                }
            }
            NetworkMessage::UploadRejected { upload_id, reason } => {
                if let Some(upload) = self.session.transfers.pending_uploads.remove(&upload_id) {
                    self.session
                        .transfers
                        .pending_picture_uploads
                        .remove(&upload_id);
                    self.session
                        .transfers
                        .shown
                        .remove(&(upload.conv, upload.placeholder));
                    self.with_conversation(upload.conv, |app| {
                        if let Some(delta) = app.history.discard_message(upload.placeholder) {
                            app.sync_range(&delta);
                        }
                    });
                    let why = describe_upload_deny(reason);
                    let line = t!(
                        "file-upload-failed",
                        "name" => upload.name.as_str(),
                        "why" => why.as_str()
                    );
                    self.show_toast(ToastKind::Warning, &line);
                } else if upload_id == AVATAR_UPLOAD_ID {
                    let why = describe_upload_deny(reason);
                    self.show_toast(
                        ToastKind::Warning,
                        &t!("profile-avatar-refused", "why" => why.as_str()),
                    );
                    // The rest of the look still goes out.
                    self.session.own_avatar_blob = None;
                    self.send_own_profile();
                }
            }
            NetworkMessage::BlobProgress { blob_id, pct } => {
                if let Some(d) = self.session.transfers.pending_downloads.get(&blob_id) {
                    let (conv, msg_id) = (d.conv, d.msg_id);
                    self.set_transfer(conv, msg_id, Some((2, i32::from(pct))));
                }
            }
            NetworkMessage::BlobFailed { blob_id } => {
                if let Some(PendingDownload { name, conv, msg_id }) =
                    self.session.transfers.pending_downloads.remove(&blob_id)
                {
                    self.set_transfer(conv, msg_id, None);
                    self.show_toast(
                        ToastKind::Warning,
                        &t!("file-download-broken", "name" => name.as_str()),
                    );
                }
            }
            NetworkMessage::Blob { blob_id, data } => {
                if let Some(rows) = self.session.transfers.pending_pictures.remove(&blob_id) {
                    for (conv, msg_id) in rows {
                        let bytes = data.clone();
                        self.with_conversation(conv, |app| {
                            media::picture_from_file(app, msg_id, bytes);
                        });
                    }
                }
                if let Some(PendingDownload { name, conv, msg_id }) =
                    self.session.transfers.pending_downloads.remove(&blob_id)
                {
                    self.set_transfer(conv, msg_id, None);
                    self.save_download(blob_id, name, conv, msg_id, data);
                } else {
                    self.session.avatar_fetches.remove(&blob_id);
                    self.take_avatar_bytes(blob_id, &data, true);
                }
            }
            NetworkMessage::BlobMissing { blob_id } => {
                self.session.transfers.pending_pictures.remove(&blob_id);
                if let Some(PendingDownload { name, conv, msg_id }) =
                    self.session.transfers.pending_downloads.remove(&blob_id)
                {
                    self.set_transfer(conv, msg_id, None);
                    self.show_toast(
                        ToastKind::Warning,
                        &t!("file-download-failed", "name" => name.as_str()),
                    );
                } else {
                    self.session.avatar_fetches.remove(&blob_id);
                }
            }
            // ── 1.1.1: direct messages — the conversation UI is the
            // next drop; until then a DM lands in the general view
            // marked as such, and a refusal is a toast. ───────────
            NetworkMessage::Direct {
                from,
                username,
                id,
                ts,
                text,
            } => {
                self.take_direct(from, &username, id, ts, Incoming::Text(&text));
            }
            NetworkMessage::MessageAck { client_ref, msg_id } => {
                if let Some((key, placeholder)) = self
                    .session
                    .chat
                    .pending_dm_sends
                    .remove(&client_ref)
                    .or_else(|| self.session.transfers.pending_files.remove(&client_ref))
                {
                    self.session.transfers.shown.remove(&(key, placeholder));
                    let picture = self.session.transfers.picture_acks.remove(&client_ref);
                    self.with_conversation(key, |app| {
                        // Confirm first: the rebuild that follows keeps a
                        // picture only when the row's id stays the same,
                        // so the picture goes in under the final id.
                        if let Some(delta) = app.history.confirm_message(placeholder, msg_id) {
                            app.sync_range(&delta);
                        }
                        app.session.oldest_history_id.get_or_insert(msg_id);
                        if let Some(png) = picture {
                            media::picture_from_file(app, msg_id, png);
                        }
                    });
                    if let Some(user) = key
                        && let Some(meta) = self.session.chat.dm_meta.get_mut(&user)
                    {
                        meta.last_msg_id = meta.last_msg_id.max(msg_id);
                    }
                }
            }
            NetworkMessage::MessageFailed { client_ref, reason } => {
                if let Some((key, placeholder)) = self
                    .session
                    .chat
                    .pending_dm_sends
                    .remove(&client_ref)
                    .or_else(|| self.session.transfers.pending_files.remove(&client_ref))
                {
                    self.with_conversation(key, |app| {
                        if let Some(delta) = app.history.discard_message(placeholder) {
                            app.sync_range(&delta);
                        }
                    });
                }
                self.show_toast(ToastKind::Warning, &t!(dm_failed_key(reason)));
            }
            NetworkMessage::Conversations { dms } => {
                // The list from the store: names, unread counts, the
                // newest id per partner. Histories load when opened.
                self.session.chat.dm_order.clear();
                for dm in dms {
                    // Closed by its ✕ and nothing new since: stays out.
                    if dm.unread == 0 && self.dm_hidden(&dm.user_id) {
                        continue;
                    }
                    // Someone we blocked: out of the list until unblocked
                    // (`Blocks` came before this, on join).
                    let blocked = self.is_blocked(&dm.user_id);
                    if dm.unread > 0 {
                        self.set_dm_hidden(dm.user_id, false);
                    }
                    let meta = self
                        .session
                        .chat
                        .dm_meta
                        .entry(dm.user_id)
                        .or_insert_with(|| DmMeta {
                            name: dm.name.clone(),
                            unread: 0,
                            last_ts: 0,
                            last_msg_id: 0,
                            fetched: false,
                        });
                    meta.name = dm.name;
                    meta.unread = if blocked { 0 } else { dm.unread };
                    meta.last_ts = dm.last_ts;
                    meta.last_msg_id = dm.last_msg_id;
                    if !blocked {
                        self.session.chat.dm_order.push(dm.user_id);
                    }
                }
                // The admin channel is ours, not the store's: it keeps
                // its place at the top.
                if self.session.chat.dm_meta.contains_key(&ADMIN_CHANNEL_KEY) {
                    self.session.chat.dm_order.insert(0, ADMIN_CHANNEL_KEY);
                }
                self.rebuild_dms_model();
            }
            // ── 1.2: blocks, reports, deleting the account ──────────
            NetworkMessage::Blocks { users } => self.take_blocks(users),
            NetworkMessage::ReportAck { result } => self.take_report_ack(result),
            NetworkMessage::AccountDeleted => self.take_account_deleted(),
            NetworkMessage::AccountDeleteRefused { reason } => {
                self.take_account_delete_refused(reason);
            }
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  CONNECTION FLOW
    // ════════════════════════════════════════════════════════════════
}
