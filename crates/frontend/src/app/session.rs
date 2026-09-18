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
                self.pending_channel_join = None;
                self.show_toast(ToastKind::Warning, &t!("channel-full"));
            }
            client::ChannelDenyReason::NoSuchChannel | client::ChannelDenyReason::NotPermitted => {
                self.pending_channel_join = None;
                self.show_error(reason.describe());
            }
        }
    }

    /// Open the channel password prompt for `channel_id`.
    pub(super) fn open_channel_prompt(&mut self, channel_id: u16, wrong: bool) {
        self.pending_channel_join = Some(channel_id);
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
        let Some(meta) = self.channels.iter().find(|c| c.id == channel_id) else {
            return;
        };
        if !meta.locked {
            self.pending_channel_join = Some(channel_id);
            self.network
                .tx
                .send(NetworkControl::JoinChannel {
                    channel_id,
                    password: None,
                })
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
                self.pending_channel_join = Some(channel_id);
                self.network
                    .tx
                    .send(NetworkControl::JoinChannel {
                        channel_id,
                        password: Some(password),
                    })
                    .ok();
            }
            None => self.open_channel_prompt(channel_id, false),
        }
    }

    /// OK on the password prompt: save the password on the server
    /// entry, then retry the join. The prompt closes when the server
    /// echoes our move (or reopens on `WrongPassword`).
    pub(super) fn channel_password_ok(&mut self) {
        let Some(channel_id) = self.pending_channel_join else {
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
            self.settings.save();
        }
        self.main.global::<State>().set_channel_prompt_open(false);
        self.network
            .tx
            .send(NetworkControl::JoinChannel {
                channel_id,
                password: Some(password),
            })
            .ok();
    }

    pub(super) fn channel_password_cancel(&mut self) {
        self.pending_channel_join = None;
        let state = self.main.global::<State>();
        state.set_channel_prompt_open(false);
        state.set_channel_password("".into());
    }

    /// Shared teardown for both user-initiated and server-initiated
    /// disconnects.
    pub(super) fn teardown_connection(&mut self) {
        self.connected = false;
        self.permissions = 0;
        self.our_user_id = None;
        self.server_persistent = true;
        self.details.clear();
        self.pending_admin.clear();
        self.server_muted = None;
        self.text_muted = None;
        self.oldest_history_id = None;
        self.channel_details.clear();
        self.telemetry_notes.clear();
        self.moderation = None;
        self.profiles.clear();
        self.avatar_fetches.clear();
        self.own_avatar_blob = None;
        self.reset_chat_flow();
        self.is_testing_mic = false;
        self.channels.clear();
        self.channel_infos.clear();
        self.channel_options.set_vec(Vec::new());
        self.main.global::<State>().set_moderation_open(false);
        self.push_permissions();
        self.pending_channel_join = None;
        {
            let state = self.main.global::<State>();
            state.set_channel_prompt_open(false);
            state.set_channel_password("".into());
        }
        // Also clear the shared flag the voice loop reads — leaving it
        // set (disconnect during a mic test) kept the loop gating
        // outgoing audio after the next connect.
        self.is_testing_mic_shared.store(false, Ordering::Relaxed);

        // Play the leave sound for ourselves too. The server's UserLeft
        // broadcast is never sent to the leaver — and stopping the
        // engine immediately would cut the sound off, so the Stop is
        // deferred. Its closure skips the Stop if a new session or mic
        // test started in the meantime.
        self.play_chime(SoundType::UserLeft);
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            slint::Timer::single_shot(Duration::from_millis(900), move || {
                with_app(&weak, |app| {
                    if !app.connected && !app.is_testing_mic {
                        app.voice.tx.send(VoiceControl::Stop).ok();
                    }
                });
            });
        }

        // Stop our screen share if active
        if self.screen.shared.is_sharing.load(Ordering::Relaxed) {
            self.screen.tx.send(ScreenControl::StopSharing).ok();
        }
        self.active_sharers.clear();
        self.sharer_ids.clear();
        for (_, viewer) in self.viewers.drain() {
            viewer.close();
        }
        crate::viewers::close_gallery(self);
        self.gifs.clear();

        self.users.clear();
        self.our_client_id = None;
        self.is_muted = false;
        self.is_deafened = false;
        // A menu open on a row that is about to vanish.
        self.close_menu();
        self.roles = builtin_role_table();

        let s = self.main.global::<State>();
        s.set_muted(false);
        s.set_deafened(false);
        s.set_sharing(false);
        s.set_stream_stats("".into());
        s.set_status_text(format!("●  {}", t!("status-disconnected")).into());
        s.set_status_color(slint::Color::from_rgb_u8(0xE0, 0x1E, 0x5A));
        s.set_latency_line(
            t!("latency-line",
                "audio" => "--", "network" => "--", "up" => "--", "down" => "--")
            .into(),
        );
        s.set_resource_line(" ".into());
        s.set_users_count_label(t!("users-count", "count" => 0u32).into());
        self.users_model.set_vec(Vec::new());
    }

    pub(super) fn handle_disconnect(&mut self) {
        self.reconnect_timer.stop();
        self.reconnect_attempts = 0;
        self.rejoin_channel = None;
        self.network.tx.send(NetworkControl::Disconnect).ok();
        self.push_chat_system(&t!("chat-disconnected", "reason" => t!("disconnect-reason-user")));
        self.teardown_connection();
        self.show_connect_phase();
    }

    pub fn show_connect_phase(&mut self) {
        self.refresh_device_warning();
        self.main.global::<State>().set_phase(Phase::Connect);
        let first = !self.startup_done;
        self.startup_done = true;
        let stay_hidden = first && self.settings.startup.start_minimized && cfg!(feature = "tray");
        if !stay_hidden {
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

    pub(super) fn handle_toggle_mute(&mut self) {
        if self.is_muted && self.server_muted.is_some() {
            self.show_toast(ToastKind::Warning, &t!("chat-server-muted-locked"));
            return;
        }
        self.set_muted(!self.is_muted);
    }

    /// Set the mute state and tell the voice thread, the server and
    /// the UI.
    pub(super) fn set_muted(&mut self, muted: bool) {
        self.is_muted = muted;
        self.voice
            .tx
            .send(VoiceControl::SetMuted(self.is_muted))
            .ok();
        self.network
            .tx
            .send(NetworkControl::SendAudioState {
                muted: self.is_muted,
                deafened: self.is_deafened,
            })
            .ok();
        self.main.global::<State>().set_muted(self.is_muted);
        self.update_user_list();
    }

    pub(super) fn handle_toggle_deafen(&mut self) {
        self.is_deafened = !self.is_deafened;
        self.voice
            .tx
            .send(VoiceControl::SetDeafened(self.is_deafened))
            .ok();
        self.network
            .tx
            .send(NetworkControl::SendAudioState {
                muted: self.is_muted,
                deafened: self.is_deafened,
            })
            .ok();
        self.main.global::<State>().set_deafened(self.is_deafened);
        self.update_user_list();
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
        let voice_already_running = self.connected || self.is_testing_mic;

        self.is_testing_mic = enabled;
        self.is_testing_mic_shared.store(enabled, Ordering::Relaxed);

        if enabled {
            if !voice_already_running {
                self.voice
                    .tx
                    .send(VoiceControl::Start {
                        settings: Box::new(self.settings.clone()),
                        our_client_id: 0,
                    })
                    .ok();
            }
            self.voice.tx.send(VoiceControl::SetLoopback(true)).ok();
        } else {
            self.voice.tx.send(VoiceControl::SetLoopback(false)).ok();
            if !self.connected {
                self.voice.tx.send(VoiceControl::Stop).ok();
            }
        }
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
        if let Some(edit) = self.editing.take() {
            // Rewriting one of ours: an empty result is a cancel, not
            // a deletion.
            self.main.global::<State>().set_editing(false);
            if !text.is_empty() && self.own_texts.get(&edit.msg_id) != Some(&text) {
                self.network
                    .tx
                    .send(NetworkControl::EditMessage {
                        msg_id: edit.msg_id,
                        text,
                    })
                    .ok();
                self.pending_edit = Some(edit.msg_id);
                Self::arm_apply_timeout(edit.msg_id, false);
            }
            self.main.global::<State>().set_draft("".into());
            self.main.invoke_focus_composer();
            return;
        }
        if text.is_empty() {
            return;
        }
        if let Some(rest) = text.strip_prefix('/')
            && let Some((command, args)) = split_command(rest)
        {
            self.handle_slash_command(command, args);
        } else if let Some(to) = self.active_dm {
            self.send_dm(to, text);
            self.main.invoke_chat_pin_bottom();
        } else if let Some(TextMute { until }) = self.text_muted {
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
