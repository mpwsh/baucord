//! Joining channels, teardown and reconnect, mute/deafen, sharing, the microphone test, sending the composer.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    pub(super) fn handle_channel_join_denied(
        &mut self,
        channel_id: u16,
        reason: client::ChannelDenyReason,
    ) {
        match reason {
            client::ChannelDenyReason::WrongPassword => {
                // A saved password can go stale; whatever was stored is
                // wrong now, so re-prompt (and overwrite on OK).
                self.open_channel_prompt(channel_id, true);
            }
            client::ChannelDenyReason::ChannelFull => {
                self.session.pending_channel_join = None;
                self.show_toast(ToastKind::Warning, &t!("channel-full"));
            }
            client::ChannelDenyReason::NoSuchChannel => {
                self.session.pending_channel_join = None;
                self.show_error(&t!("channel-deny-missing"));
            }
            client::ChannelDenyReason::NotPermitted => {
                self.session.pending_channel_join = None;
                self.show_error(&t!("channel-deny-not-permitted"));
            }
        }
    }

    /// Open the channel password prompt for `channel_id`.
    pub(super) fn open_channel_prompt(&mut self, channel_id: u16, wrong: bool) {
        self.session.pending_channel_join = Some(channel_id);
        let name = self.channel_name(channel_id);
        let state = self.main.global::<State>();
        state.set_channel_prompt_name(name.into());
        state.set_channel_prompt_error(if wrong {
            t!("channel-wrong-password").into()
        } else {
            "".into()
        });
        state.set_channel_password("".into());
        state.set_channel_prompt_open(true);
    }

    /// A channel header was clicked: join directly, or via saved /
    /// prompted password for locked channels.
    pub(super) fn channel_clicked(&mut self, channel_id: u16) {
        if Some(channel_id) == self.our_channel() {
            return;
        }
        let Some(meta) = self.session.channels.iter().find(|c| c.id == channel_id) else {
            return;
        };
        if !meta.locked {
            self.session.pending_channel_join = Some(channel_id);
            self.network
                .tx
                .send(NetworkControl::Send(ClientMessage::JoinChannel {
                    channel_id,
                    password: None,
                }))
                .ok();
            return;
        }
        // Locked: try the saved password first; the server's
        // WrongPassword answer reopens the prompt if it went stale.
        let name = meta.name.clone();
        let saved = self
            .settings
            .default_server()
            .and_then(|server| server.channel_passwords.get(&name).cloned());
        match saved {
            Some(password) => {
                self.session.pending_channel_join = Some(channel_id);
                self.network
                    .tx
                    .send(NetworkControl::Send(ClientMessage::JoinChannel {
                        channel_id,
                        password: Some(password),
                    }))
                    .ok();
            }
            None => self.open_channel_prompt(channel_id, false),
        }
    }

    /// OK on the password prompt: save the password on the server
    /// entry, then retry the join. The prompt closes when the server
    /// echoes our move (or reopens on `WrongPassword`).
    pub(super) fn channel_password_ok(&mut self) {
        let Some(channel_id) = self.session.pending_channel_join else {
            self.main.global::<State>().set_channel_prompt_open(false);
            return;
        };
        let password = self
            .main
            .global::<State>()
            .get_channel_password()
            .to_string();
        if password.is_empty() {
            return;
        }
        let name = self.channel_name(channel_id);
        if let Some(server) = self.settings.default_server_mut() {
            server.channel_passwords.insert(name, password.clone());
            self.save_settings();
        }
        self.main.global::<State>().set_channel_prompt_open(false);
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::JoinChannel {
                channel_id,
                password: Some(password),
            }))
            .ok();
    }

    pub(super) fn channel_password_cancel(&mut self) {
        self.session.pending_channel_join = None;
        let state = self.main.global::<State>();
        state.set_channel_prompt_open(false);
        state.set_channel_password("".into());
    }

    /// Shared teardown for both user-initiated and server-initiated
    /// disconnects.
    pub(super) fn teardown_connection(&mut self) {
        self.connected = false;
        // The session got as far as an engine of its own (`Init`).
        let session_engine = self.session.our_client_id.is_some();
        // No session, no keybinds: the key watcher stops (review P1-3).
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.set_connected(false);
        }
        // Before the conversations go: their cards are repainted.
        self.clear_transfers();
        self.reset_chat_flow();
        // Everything else the session held, in one step (review P2-51):
        // who was here, what we may do, the server's channels and
        // roles, our own mute and deafen. What follows only tells the
        // window, the voice engine and the screen thread.
        self.session = Session::default();
        // The pictures of the session's people: the next one decodes
        // whoever it shows again, from the cache on disk.
        self.avatar_images.clear();
        self.avatar_decoding.clear();
        self.paused_watch = None;
        self.reset_safety();
        // A test ends with the session it may have started in.
        if std::mem::take(&mut self.is_testing_mic) {
            crate::settingsui::test_ended(self);
        }
        self.main.global::<State>().set_moderation_open(false);
        self.push_permissions();
        {
            let state = self.main.global::<State>();
            state.set_channel_prompt_open(false);
            state.set_channel_password("".into());
        }
        // Also bring the shared flag the voice loop reads up to date —
        // leaving it set (disconnect during a mic test) kept the loop
        // gating outgoing audio after the next connect.
        self.sync_voice_room();
        // The Audio tab's meter is showing: it goes on, on an engine of
        // its own — the session's would carry the session's mute into
        // a meter with nothing left to mute for.
        if self.audio_tab_open && session_engine {
            self.start_sessionless_engine();
        }

        // Play the leave sound for ourselves too. The server's UserLeft
        // broadcast is never sent to the leaver — and stopping the
        // engine immediately would cut the sound off, so the Stop is
        // deferred. Its closure skips the Stop if a new session, a mic
        // test or the Audio tab's meter started in the meantime.
        self.play_chime(SoundType::UserLeft);
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            slint::Timer::single_shot(Duration::from_millis(900), move || {
                with_app(&weak, |app| {
                    if !app.connected && !app.mic_previewing() {
                        app.voice.tx.send(VoiceControl::Stop).ok();
                    }
                });
            });
        }

        // Stop our screen share if active
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
        }
        // Closing a viewer only hides it: the screen thread must be told
        // too, or it keeps the stream's decoder, buffers and audio for
        // the rest of the process — the sharer's id is gone with the
        // session, so nothing else would ever end it.
        for (client_id, viewer) in self.viewers.drain() {
            viewer.close();
            self.screen
                .tx
                .send(ScreenControl::StopWatching { client_id })
                .ok();
        }
        crate::viewers::close_gallery(self);
        self.gifs.clear();
        // Every picture of the session: the next one brings its own.
        crate::pictures::clear();

        Self::clear_call_notice();
        // A menu open on a row that is about to vanish.
        self.close_menu();

        let s = self.main.global::<State>();
        s.set_muted(false);
        s.set_deafened(false);
        s.set_sharing(false);
        s.set_stream_stats("".into());
        s.set_status_text(t!("status-disconnected").into());
        s.set_status_color(slint::Color::from_rgb_u8(0xE0, 0x1E, 0x5A));
        s.set_latency_line(t!("latency-line", "audio" => "--", "network" => "--").into());
        s.set_latency_up("--".into());
        s.set_latency_down("--".into());
        s.set_resource_line(" ".into());
        s.set_users_count_label(t!("users-count", "count" => 0u32).into());
        self.users_model.set_vec(Vec::new());
        self.sync_tray();
    }

    /// Transfers end with the session: nothing more arrives for them.
    /// Left in place, Save did nothing for that file for the rest of
    /// the run (it read as already under way), pictures caught in
    /// flight never loaded after the reconnect, and cards kept a ring
    /// that would never move (review P2-13).
    fn clear_transfers(&mut self) {
        // The cards first: each loses its ring where it is shown.
        let shown: Vec<(Option<client::UserId>, u64)> =
            self.session.transfers.shown.keys().copied().collect();
        for (conv, msg_id) in shown {
            self.set_transfer(conv, msg_id, None);
        }
        self.session.transfers = Transfers::default();
    }

    pub(super) fn handle_disconnect(&mut self) {
        self.reconnect_timer.stop();
        self.reconnect_attempts = 0;
        self.rejoin_channel = None;
        self.network.tx.send(NetworkControl::Disconnect).ok();
        // No "disconnected" line: the connect page hides the chat, and
        // the next session's `Init` clears it.
        self.teardown_connection();
        self.show_connect_phase();
    }

    pub fn show_connect_phase(&mut self) {
        self.refresh_device_warning();
        self.main.global::<State>().set_phase(Phase::Connect);
        // The admin panel is the session's: it does not outlive it
        // (Settings may stay, the connect page opens it too).
        crate::adminui::close(self);
        // The session is over (unless a reconnect is on its way): the
        // host may let the app sleep again.
        if !std::mem::take(&mut self.reconnect_hold) {
            crate::host::keep_alive(None, false);
        }
        self.kept_alive = None;
        let first = !self.startup_done;
        self.startup_done = true;
        // Shown at launch only (unless that starts in the tray). A
        // session ending later — a lost connection, a kick — changes
        // the view where the window is and takes no focus from
        // whatever the user is in (review P2-17).
        let stay_hidden = self.settings.startup.start_minimized && cfg!(feature = "tray");
        if first && !stay_hidden {
            self.show_main();
        }
        if first && self.settings.startup.auto_connect {
            let nickname = self.settings.user.nickname.trim().to_string();
            if !nickname.is_empty() {
                self.handle_connect(&nickname);
            }
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  VOICE CONTROLS
    // ════════════════════════════════════════════════════════════════

    pub(crate) fn handle_toggle_mute(&mut self) {
        if self.session.is_muted && self.session.server_muted.is_some() {
            self.show_toast(ToastKind::Warning, &t!("chat-server-muted-locked"));
            return;
        }
        self.set_muted(!self.session.is_muted);
        self.toggle_tone(self.session.is_muted);
    }

    /// The soft note for our own mute or deafen, from wherever it was
    /// toggled (the button, a keybind, the tray, a stream's bar): lower
    /// going quiet, higher coming back. It plays where the room plays —
    /// a phone in the lobby has no playback and stays silent.
    fn toggle_tone(&self, quiet: bool) {
        if self.settings.sounds.mute_toggle_on() {
            let sound = if quiet {
                SoundType::WentQuiet
            } else {
                SoundType::BackLive
            };
            self.voice.tx.send(VoiceControl::PlaySound(sound)).ok();
        }
    }

    /// Set the mute state and tell the voice thread, the server and
    /// the UI.
    pub(super) fn set_muted(&mut self, muted: bool) {
        self.session.is_muted = muted;
        self.voice
            .tx
            .send(VoiceControl::SetMuted(self.session.is_muted))
            .ok();
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::AudioState {
                muted: self.session.is_muted,
                deafened: self.session.is_deafened,
            }))
            .ok();
        self.main.global::<State>().set_muted(self.session.is_muted);
        self.push_self_audio();
        self.update_user_list();
    }

    /// Mute and deafen to every stream window's own buttons (D43).
    fn push_self_audio(&self) {
        for viewer in self.viewers.values() {
            viewer.set_self_audio(self.session.is_muted, self.session.is_deafened);
        }
    }

    pub(crate) fn handle_toggle_deafen(&mut self) {
        self.session.is_deafened = !self.session.is_deafened;
        self.voice
            .tx
            .send(VoiceControl::SetDeafened(self.session.is_deafened))
            .ok();
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::AudioState {
                muted: self.session.is_muted,
                deafened: self.session.is_deafened,
            }))
            .ok();
        self.main
            .global::<State>()
            .set_deafened(self.session.is_deafened);
        self.push_self_audio();
        self.update_user_list();
        self.toggle_tone(self.session.is_deafened);
    }

    pub(super) fn handle_toggle_screenshare(&mut self) {
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
            return;
        }
        // Hotkey UX: when the user is in another app (typically a
        // game), don't steal focus with a picker — start streaming
        // immediately with the saved settings. The dialog shows only
        // when Baucord itself is focused (which covers the in-app
        // share button — clicking it focuses us) or when focus can't
        // be determined (never start a stream on a guess).
        let baucord_focused = video::capture::focused_pid().map(|pid| pid == std::process::id());
        match baucord_focused {
            Some(false) => self.start_share_with_saved(),
            _ => self.open_share_dialog(),
        }
    }

    /// Start sharing immediately with the last-used settings (falls
    /// back to primary monitor / FHD30 / channel-only). Hotkey path
    /// while another app is focused.
    pub(super) fn start_share_with_saved(&mut self) {
        let monitors = video::capture::list_monitors().unwrap_or_default();
        if monitors.is_empty() {
            self.show_error(&t!("error-no-monitors"));
            return;
        }
        let monitor_index = self.saved_or_primary_monitor(&monitors);
        let quality =
            video::Quality::from_key(&self.settings.share.quality).unwrap_or(video::Quality::FHD30);
        let everyone = self.settings.share.everyone;
        log::info!(
            "share hotkey pressed outside Baucord — starting immediately with saved \
             settings: {quality}, monitor {monitor_index}, everyone={everyone}"
        );
        video::capture::set_pointer_shown(self.settings.share.pointer);
        self.screen
            .tx
            .send(ScreenControl::StartSharing {
                quality,
                monitor_index,
                scope: if everyone {
                    client::ShareScope::Everyone
                } else {
                    client::ShareScope::Channel
                },
            })
            .ok();
    }

    /// Open the share picker, pre-filled with the last-used settings.
    pub(super) fn open_share_dialog(&mut self) {
        let monitors = video::capture::list_monitors().unwrap_or_default();
        if monitors.is_empty() {
            self.show_error(&t!("error-no-monitors"));
            return;
        }
        let labels: Vec<slint::SharedString> = monitors
            .iter()
            .map(|m| {
                let primary = if m.is_primary {
                    format!(" {}", t!("monitor-primary"))
                } else {
                    String::new()
                };
                format!("{} — {}x{}{}", m.name, m.width, m.height, primary).into()
            })
            .collect();
        let qualities: Vec<slint::SharedString> = video::Quality::ALL
            .iter()
            .map(|q| q.to_string().into())
            .collect();
        let default_monitor = self.saved_or_primary_monitor(&monitors);
        let saved_quality =
            video::Quality::from_key(&self.settings.share.quality).unwrap_or(video::Quality::FHD30);
        let default_q = video::Quality::ALL
            .iter()
            .position(|q| *q == saved_quality)
            .unwrap_or(0);
        let s = self.main.global::<State>();
        s.set_monitors(ModelRc::new(VecModel::from(labels)));
        s.set_qualities(ModelRc::new(VecModel::from(qualities)));
        s.set_default_monitor(i32::try_from(default_monitor).unwrap_or(0));
        s.set_default_quality(i32::try_from(default_q).unwrap_or(0));
        s.set_default_scope(i32::from(self.settings.share.everyone));
        self.main.invoke_open_share_dialog();
    }

    /// The saved monitor index if that monitor still exists, otherwise
    /// the primary (monitors come and go between sessions).
    pub(super) fn saved_or_primary_monitor(
        &self,
        monitors: &[video::capture::MonitorInfo],
    ) -> usize {
        let primary_idx = monitors.iter().position(|m| m.is_primary).unwrap_or(0);
        self.settings
            .share
            .monitor
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < monitors.len())
            .unwrap_or(primary_idx)
    }

    pub fn handle_test_microphone(&mut self, enabled: bool) {
        // Capture this BEFORE flipping is_testing_mic — checking
        // afterwards made the "start the engine for a mic test while
        // disconnected" branch unreachable.
        let voice_already_running = self.engine_wanted();
        self.is_testing_mic = enabled;
        // A test needs the microphone, in the lobby too.
        self.mic_preview_changed(voice_already_running);
        // Behind a Start on the same channel; nothing to tell an engine
        // that was just stopped.
        self.voice.tx.send(VoiceControl::SetLoopback(enabled)).ok();
        self.sync_keep_alive(false);
    }

    /// Settings' Audio tab came on screen or left it. While it shows
    /// the microphone is open for its meter wherever we are — not
    /// connected, in the lobby, on a phone that otherwise records
    /// nothing there — so the microphone can be set up before there is
    /// anyone to talk to. Out of a voice room nothing it hears leaves
    /// the device; in one, nothing changes. Not connected, mute and
    /// push-to-talk do not hold it shut either (an engine with no
    /// session, `VoiceControl::Start`); in a session they do, as in a
    /// room. Unlike a test it never outlives the screen: a hidden app
    /// or a closed tab ends it.
    pub fn set_audio_tab_open(&mut self, open: bool) {
        if open == self.audio_tab_open {
            return;
        }
        let voice_already_running = self.engine_wanted();
        self.audio_tab_open = open;
        log::info!(
            "settings: audio tab {}",
            if open { "on screen" } else { "off screen" }
        );
        self.mic_preview_changed(voice_already_running);
    }

    /// The microphone is open for this device's own sake: a test, or
    /// the Audio tab's meter.
    pub(super) fn mic_previewing(&self) -> bool {
        self.is_testing_mic || self.audio_tab_open
    }

    /// An engine should be running: a session's, which it gets with
    /// its client id (`Init`) — not on `Connected`, a moment earlier,
    /// when there is none to hand the microphone to yet — or one for a
    /// preview without a session.
    fn engine_wanted(&self) -> bool {
        self.session.our_client_id.is_some() || self.mic_previewing()
    }

    /// A test or the Audio tab's meter began or ended: start the engine
    /// for it where no session has one running, stop it when the last
    /// reason for it went, and tell a running one where it stands.
    fn mic_preview_changed(&mut self, was_running: bool) {
        let wanted = self.engine_wanted();
        self.sync_voice_room();
        // One line a change, each saying which engine and why: with
        // the voice thread's own (`Voice started`, `Voice stopped`) a
        // pasted log shows which of the two did not do its part.
        let (tab, test) = (self.audio_tab_open, self.is_testing_mic);
        match (was_running, wanted) {
            (false, true) => {
                log::info!(
                    "microphone: no engine yet, starting one for this device alone \
                     (Audio tab {tab}, test {test})"
                );
                self.start_sessionless_engine();
            }
            (true, false) => {
                log::info!("microphone: nothing needs it, stopping the engine");
                self.voice.tx.send(VoiceControl::Stop).ok();
            }
            (true, true) if self.session.our_client_id.is_some() => log::info!(
                "microphone: the session's engine has it (in a voice room {}, Audio tab {tab}, \
                 test {test})",
                self.in_voice_room()
            ),
            (true, true) => log::info!(
                "microphone: the engine started for this device alone carries on \
                 (Audio tab {tab}, test {test})"
            ),
            (false, false) => {}
        }
    }

    /// An engine for the microphone alone, with no session: what it
    /// hears is never sent.
    fn start_sessionless_engine(&mut self) {
        self.voice_in_room = true;
        self.voice
            .tx
            .send(VoiceControl::Start {
                config: Box::new(self.engine_config()),
                our_client_id: None,
                in_room: true,
            })
            .ok();
    }

    /// Tell the host what keeps the session alive off screen, when that
    /// changes: connected in a voice room (the microphone open) or in
    /// the lobby (D63). `force` says it again as it is — on a phone
    /// brought back to the screen, where a room's microphone type that
    /// was refused in the background can now be granted.
    pub(super) fn sync_keep_alive(&mut self, force: bool) {
        if !self.connected || self.session.our_client_id.is_none() {
            return;
        }
        // The AFK room is out of a room too: the relay drops its voice,
        // so its microphone service type and room wake lock were a
        // battery cost for nothing (review 2026-10, P3-13; D53).
        let in_room = !self.out_of_voice() || self.is_testing_mic;
        if force || self.kept_alive != Some(in_room) {
            self.kept_alive = Some(in_room);
            crate::host::keep_alive(Some(&self.server_name), in_room);
        }
    }

    /// Tell the voice engine whether we are in a voice room (or testing
    /// the microphone, or showing its meter), when that changes. On a
    /// phone the microphone and the call path go with it: nothing
    /// records in the lobby (ANDROID-SPEC.md D53, IOS-SPEC.md D51). The
    /// desktop keeps its streams and drops what the microphone hears,
    /// so nothing is encoded and sent for the relay to throw away
    /// (review P1-6).
    ///
    /// And where what it hears may go: a test keeps it on this device
    /// anywhere, the meter does wherever there is no room to talk into
    /// — the lobby and the AFK room, where the relay would drop it too,
    /// and nothing lights us up as speaking for it.
    pub(super) fn sync_voice_room(&mut self) {
        self.sync_mic_local();
        // No session's engine, no test and no meter: there is none yet
        // (`Connected`, before `Init` starts it with where we are), or
        // it is on its way out (the deferred Stop). Leave it as it is.
        if !self.engine_wanted() {
            return;
        }
        let in_room = self.voice_room_wanted();
        if in_room != self.voice_in_room {
            self.voice_in_room = in_room;
            self.voice.tx.send(VoiceControl::SetInRoom(in_room)).ok();
        }
        // Someone to hear it: a voice room with somebody else in it,
        // and no test keeping it here. Otherwise the encoder gets
        // nothing — it encoded 50 frames a second for nobody (review
        // 2026-10, P2-37); the light and the meters go on.
        let transmit = self.in_voice_room() && !self.is_testing_mic && self.room_has_company();
        if transmit != self.voice_transmit {
            self.voice_transmit = transmit;
            self.voice.tx.send(VoiceControl::SetTransmit(transmit)).ok();
        }
    }

    /// Anyone else in our room.
    fn room_has_company(&self) -> bool {
        let (Some(us), Some(room)) = (self.session.our_client_id, self.our_channel()) else {
            return false;
        };
        self.session
            .users
            .iter()
            .any(|(id, user)| *id != us && user.channel_id == room)
    }

    /// What the network thread reads before it sends a voice packet:
    /// what the microphone hears stays here during a test, and while
    /// the Audio tab's meter has it open out of a voice room.
    pub(super) fn sync_mic_local(&self) {
        let local = self.is_testing_mic || (self.audio_tab_open && !self.in_voice_room());
        self.is_testing_mic_shared.store(local, Ordering::Relaxed);
    }

    /// Send the composer draft: shortcodes rendered, control chars
    /// scrubbed, char cap enforced. The server echoes `TextMessage` back
    /// to everyone (sender included), so there is no local append here.
    pub(super) fn send_current_message(&mut self) {
        let raw = self.main.global::<State>().get_draft().to_string();
        let rendered =
            app_core::emoji::replace_shortcodes(&app_core::chat::scrub_control_chars(raw.trim()));
        let text: String = rendered
            .chars()
            .take(app_core::chat::MAX_MESSAGE_CHARS)
            .collect();
        if let Some(edit) = self.session.chat.editing.take() {
            // Rewriting one of ours: an empty result is a cancel, not
            // a deletion.
            self.main.global::<State>().set_editing(false);
            if !text.is_empty() && self.session.chat.own_texts.get(&edit.msg_id) != Some(&text) {
                self.network
                    .tx
                    .send(NetworkControl::Send(ClientMessage::EditMessage {
                        msg_id: edit.msg_id,
                        text,
                    }))
                    .ok();
                self.session.chat.pending_edit = Some(edit.msg_id);
                Self::arm_apply_timeout(edit.msg_id, false);
            }
            self.main.global::<State>().set_draft("".into());
            self.main.invoke_focus_composer();
            return;
        }
        if text.is_empty() {
            return;
        }
        // Written from General's activity view: what is sent, and any
        // command's answer, is in the conversation — show it.
        if self.activity_shown() {
            self.set_general_view(false);
        }
        if let Some(rest) = text.strip_prefix('/')
            && let Some((command, args)) = split_command(rest)
        {
            self.handle_slash_command(command, args);
        } else if let Some(to) = self.session.chat.active_dm {
            self.send_dm(to, text);
            self.main.invoke_chat_pin_bottom();
        } else if let Some(TextMute { until }) = self.session.text_muted {
            // The server would drop it anyway; say so here instead.
            let until = format!(" {}", describe_until(until));
            self.show_toast(
                ToastKind::Warning,
                &t!("restricted-text", "until" => until.as_str(), "reason" => ""),
            );
            return;
        } else {
            self.network.tx.send(NetworkControl::SendMessage(text)).ok();
            self.main.invoke_chat_pin_bottom();
        }
        self.main.global::<State>().set_draft("".into());
        self.main.invoke_focus_composer();
    }

    // ════════════════════════════════════════════════════════════════
    //  PER-USER AUDIO
    // ════════════════════════════════════════════════════════════════
}
