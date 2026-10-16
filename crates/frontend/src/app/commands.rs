//! The connect button, slash commands, the channel dialog, admin commands and their results, the invite dialog.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

/// How many rows a typed listing (`/users`, `/bans`, `/invites`,
/// `/reports`) prints into the chat. The server sends a long listing
/// whole since 1.2.1 (review P0-1); the panel shows all of it, and a
/// chat that keeps 500 rows should not lose them to one command.
const COMMAND_LIST_ROWS: usize = 150;

impl AppState {
    pub(super) fn handle_connect(&mut self, nickname: &str) {
        self.reconnect_timer.stop();
        self.settings.user.nickname = nickname.to_string();
        self.main.global::<State>().set_connect_notice("".into());

        // A code (or a whole link) typed into the invite box counts —
        // clicking Connect means "use what I typed". It is stored on
        // the entry so a restart before admission still has it.
        let s = self.main.global::<State>();
        if s.get_invite_prompt() {
            let typed = s.get_invite_code().to_string();
            if let Some(server) = self.settings.default_server_mut()
                && !server.set_invite_input(&typed)
            {
                self.show_error(&t!("error-invite-code-malformed"));
                return;
            }
        }
        self.save_settings();
        crate::settingsui::refresh_servers(self);

        let Some(client_key) = self.client_key.clone() else {
            let path = client::config::client_key_path().display().to_string();
            self.show_error(&t!("error-no-identity", "path" => path.as_str()));
            return;
        };
        let Some(server) = self.settings.default_server().cloned() else {
            self.show_error(&t!("error-invalid-address"));
            return;
        };
        let Ok(addr) = server.address.trim().parse::<SocketAddr>() else {
            self.show_error(&t!("error-invalid-address"));
            return;
        };
        // Which entry this session is: the default may change while
        // it runs, and an account deletion clears this one.
        self.session_server = Some(server.name.clone());

        self.network
            .tx
            .send(NetworkControl::Connect {
                addr,
                nickname: nickname.to_string(),
                invite_code: server.invite_code,
                fingerprint: server.fingerprint,
                client_key,
                portable: client::config::is_portable_install(),
            })
            .ok();
    }

    /// Dismiss the invite box and the sticky verdict line.
    pub fn clear_connect_verdicts(&mut self) {
        let s = self.main.global::<State>();
        s.set_invite_prompt(false);
        s.set_invite_prompt_text("".into());
        s.set_invite_code("".into());
        s.set_connect_notice("".into());
    }

    /// One slash command from the composer. Moderation commands map
    /// onto `AdminCommand` (spec §8): the server checks them, a list
    /// or a refusal lands in chat, and a plain "done" in the activity
    /// log under what was typed ([`Self::send_admin`]).
    pub(super) fn handle_slash_command(&mut self, command: &str, args: &str) {
        let shown = args.trim();
        self.typed_command = Some(if shown.is_empty() {
            format!("/{command}")
        } else {
            format!("/{command} {shown}")
        });
        self.run_slash_command(command, args);
        // Not every command asks the server anything.
        self.typed_command = None;
    }

    fn run_slash_command(&mut self, command: &str, args: &str) {
        // Output goes back where the command was typed.
        self.command_conv = self.session.chat.active_dm;
        match command {
            "invite" => self.handle_invite_command(args),
            "help" | "commands" => {
                let commands = self.commands_available();
                let line = t!("chat-commands-help", "commands" => commands.as_str());
                self.push_command_output(&line);
            }
            "leave" => self.handle_disconnect(),
            "quit" | "exit" if super::HOST_CAN_QUIT => self.leave_app(super::Leaving::Quit),
            "quit" | "exit" => self.push_command_output(&t!("quit-ios")),
            "users" => self.send_admin(AdminCommand::ListUsers, PendingAdmin::Show),
            "restrictions" | "bans" => {
                self.send_admin(AdminCommand::ListRestrictions, PendingAdmin::Show);
            }
            "invites" => self.send_admin(AdminCommand::ListInvites, PendingAdmin::Show),
            // 1.2: the open reports, where the panel is not (phones).
            "reports" => self.send_admin(
                AdminCommand::ListReports {
                    resolved: args.trim() == "all",
                },
                PendingAdmin::Show,
            ),
            "resolve" => match args.trim().trim_start_matches('#').parse::<u64>() {
                Ok(id) => self.send_admin(AdminCommand::ResolveReport { id }, PendingAdmin::Show),
                Err(_) => self.push_command_output(&t!("chat-command-usage-resolve")),
            },
            "telemetry" => {
                let target = self.target_for(args.trim());
                self.send_admin(AdminCommand::LastTelemetry { target }, PendingAdmin::Show);
            }
            "history" => {
                let limit = args.trim().parse::<u16>().unwrap_or(20).clamp(1, 200);
                self.load_earlier(limit);
            }
            "mem" => {
                let report = self.memory_report();
                info!("memory report:\n{report}");
                self.push_command_output(&report);
            }
            "dm" | "msg" => {
                let (name, text) = match args.trim().split_once(char::is_whitespace) {
                    Some((n, t)) if !t.trim().is_empty() => (n.trim(), t.trim().to_string()),
                    _ => {
                        self.push_command_output(&t!("chat-command-usage-dm"));
                        return;
                    }
                };
                let found = self
                    .session
                    .users
                    .iter()
                    .find(|(_, u)| u.username.eq_ignore_ascii_case(name))
                    .map(|(c, u)| (*c, u.username.clone()));
                let Some((client_id, shown)) = found else {
                    self.push_command_output(&t!("chat-dm-unknown", "name" => name));
                    return;
                };
                let Some(user_id) = self.session.details.get(&client_id).map(|(u, _)| *u) else {
                    return;
                };
                self.open_dm(user_id, &shown);
                self.send_dm(user_id, text);
            }
            "channel" => self.handle_channel_command(args),
            "kick" | "ban" | "unban" | "disconnect" | "mute" | "unmute" | "grant" | "revoke"
            | "move" => self.handle_user_command(command, args),
            "revoke-invite" => match client::invite::normalize_code(args) {
                Some(code) => {
                    self.send_admin(AdminCommand::RevokeInvite { code }, PendingAdmin::Show);
                }
                None => self.push_command_output(&t!("chat-command-usage-revoke-invite")),
            },
            "unlink" => match parse_hex_id(args.trim()) {
                Some(key_hash) => {
                    self.send_admin(AdminCommand::UnlinkKey { key_hash }, PendingAdmin::Show);
                }
                None => self.push_command_output(&t!("chat-command-usage-unlink")),
            },
            "link" => {
                let mut words = args.split_whitespace();
                match (words.next(), words.next().and_then(parse_hex_id)) {
                    (Some(name), Some(key_hash)) => {
                        let target = self.target_for(name);
                        self.send_admin(
                            AdminCommand::LinkKey { target, key_hash },
                            PendingAdmin::Show,
                        );
                    }
                    _ => self.push_command_output(&t!("chat-command-usage-link")),
                }
            }
            _ => self.push_command_output(&t!("chat-command-unknown", "command" => command)),
        }
    }

    /// The commands about a person:
    /// `/kick name [reason]` · `/ban name [7d] [reason]` · `/unban name`
    /// · `/disconnect name [reason]` · `/mute [voice|text] name [30m]
    /// [reason]` · `/unmute [voice|text] name` · `/grant name role` ·
    /// `/revoke name role` · `/move name channel`.
    pub(super) fn handle_user_command(&mut self, command: &str, args: &str) {
        let mut words: Vec<&str> = args.split_whitespace().collect();
        // `/mute` and `/unmute` take an optional kind before the name.
        let mut kind = client::RestrictionKind::Voice;
        if matches!(command, "mute" | "unmute")
            && let Some(first) = words.first()
            && let Some(k) = client::RestrictionKind::parse(first)
            && matches!(
                k,
                client::RestrictionKind::Voice | client::RestrictionKind::GeneralText
            )
        {
            kind = k;
            words.remove(0);
        }
        if words.is_empty() {
            self.push_command_output(&t!("chat-command-usage", "command" => command));
            return;
        }
        let name = words.remove(0);
        let target = self.target_for(name);
        let rest = words;
        // An optional duration right after the name; the rest is the reason.
        let (duration_secs, reason_words) = match rest.first().and_then(|w| parse_duration_secs(w))
        {
            Some(secs) => (Some(secs), &rest[1..]),
            None => (None, &rest[..]),
        };
        let reason = reason_words.join(" ");
        let admin = match command {
            "kick" => AdminCommand::Kick { target, reason },
            "disconnect" => AdminCommand::Disconnect { target, reason },
            "ban" => AdminCommand::Ban {
                target,
                duration_secs,
                reason,
            },
            "unban" => AdminCommand::Unban { target },
            "mute" => AdminCommand::Restrict {
                target,
                kind,
                duration_secs,
                reason,
            },
            "unmute" => AdminCommand::Unmute { target, kind },
            "grant" | "revoke" => {
                let Some(role) = rest.first().and_then(|w| self.parse_role_name(w)) else {
                    self.push_command_output(&t!("chat-command-usage", "command" => command));
                    return;
                };
                // Their current roles, known while they are online.
                let mut roles: Vec<client::RoleId> = match &target {
                    AdminTarget::User(user_id) => self
                        .session_of(user_id)
                        .and_then(|c| self.session.details.get(&c))
                        .map(|(_, r)| r.clone())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                if command == "grant" {
                    if !roles.contains(&role) {
                        roles.push(role);
                    }
                } else {
                    roles.retain(|r| *r != role);
                }
                AdminCommand::SetRoles { target, roles }
            }
            "move" => {
                let wanted = rest.join(" ");
                let Some(channel_id) = self
                    .session
                    .channels
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(wanted.trim()))
                    .and_then(|i| u16::try_from(i).ok())
                else {
                    self.push_command_output(&t!("chat-command-usage", "command" => command));
                    return;
                };
                AdminCommand::MoveUser { target, channel_id }
            }
            _ => return,
        };
        self.send_admin(admin, PendingAdmin::Show);
    }

    /// The commands this session may use, from `Session.permissions`
    /// (spec §8): a member sees what a member can do, not what an
    /// admin can.
    pub(super) fn commands_available(&self) -> String {
        use client::permissions as p;
        let has = |bit: u64| p::has(self.session.permissions, bit);
        let mut out: Vec<&str> = vec!["/help", "/history [n]", "/channel list", "/mem", "/leave"];
        if super::HOST_CAN_QUIT {
            out.push("/quit");
        }
        if has(p::CREATE_INVITES) {
            out.push("/invite [role] [24h] [uses]");
        }
        if has(p::CREATE_CHANNELS) {
            out.push("/channel create <name> [password]");
        }
        if has(p::MANAGE_OWN_CHANNELS) || has(p::MANAGE_CHANNELS) {
            out.push("/channel keep <name>");
            out.push("/channel delete <name>");
            out.push("/channel kick <user>");
        }
        if has(p::MANAGE_CHANNELS) {
            out.push("/move <user> <channel>");
        }
        if has(p::MUTE) || has(p::RESTRICT) {
            out.push("/mute [voice|text] <user> [30m] [reason]");
            out.push("/unmute [voice|text] <user>");
            out.push("/bans");
        }
        if has(p::RESTRICT) && self.session.safety.on {
            out.push("/reports [all]");
            out.push("/resolve <id>");
        }
        if has(p::DISCONNECT) {
            out.push("/disconnect <user> [reason]");
        }
        if has(p::KICK) {
            out.push("/kick <user> [reason]");
        }
        if has(p::BAN) {
            out.push("/ban <user> [7d] [reason]");
            out.push("/unban <user>");
        }
        if has(p::MANAGE_ROLES) {
            out.push("/grant <user> <role>");
            out.push("/revoke <user> <role>");
        }
        if has(p::MANAGE_INVITES) {
            out.push("/invites");
            out.push("/revoke-invite <code>");
        }
        if has(p::MANAGE_USERS) {
            out.push("/users");
            out.push("/telemetry <user>");
            out.push("/link <user> <keyhash>");
            out.push("/unlink <keyhash>");
        }
        out.join(" · ")
    }

    /// `/channel create <name> [password]` · `/channel keep <name>` ·
    /// `/channel delete <name>` · `/channel kick <user>` (spec §5).
    /// The dialog comes with the UI step; this is enough to try it.
    pub(super) fn handle_channel_command(&mut self, args: &str) {
        let mut words = args.split_whitespace();
        let Some(verb) = words.next() else {
            self.push_command_output(&t!("chat-command-usage-channel"));
            return;
        };
        match verb {
            "list" => {
                let lines: Vec<String> = self
                    .session
                    .channels
                    .iter()
                    .map(|c| {
                        let facts = self.session.channel_details.get(&c.id);
                        let owner = facts
                            .and_then(|f| f.owner)
                            .and_then(|o| {
                                self.session_of(&o)
                                    .and_then(|client_id| self.session.users.get(&client_id))
                                    .map(|u| u.username.clone())
                            })
                            .unwrap_or_else(|| t!("channel-owner-server"));
                        let flags: Vec<String> = [
                            facts
                                .is_some_and(|f| f.is_afk)
                                .then_some("channel-flag-afk"),
                            facts
                                .is_some_and(|f| !f.permanent)
                                .then_some("channel-flag-temporary"),
                            c.locked.then_some("channel-flag-password"),
                        ]
                        .into_iter()
                        .flatten()
                        .map(|key| t!(key))
                        .collect();
                        format!(
                            "#{} {} — {} — {}{}",
                            facts.map_or(0, |f| f.stable_id),
                            c.name,
                            owner,
                            if flags.is_empty() {
                                t!("channel-flag-permanent")
                            } else {
                                flags.join(", ")
                            },
                            c.user_limit.map_or(String::new(), |l| format!(
                                " — {}",
                                t!("channel-list-limit", "limit" => l)
                            ))
                        )
                    })
                    .collect();
                for line in lines {
                    self.push_command_output(&line);
                }
            }
            "create" => {
                let Some(name) = words.next() else {
                    self.push_command_output(&t!("chat-command-usage-channel"));
                    return;
                };
                let password = words.next().map(str::to_string);
                self.create_channel(client::ChannelSpec {
                    name: name.to_string(),
                    password,
                    user_limit: None,
                    min_role: None,
                    permanent: false,
                    afk_after_min: None,
                });
            }
            "keep" | "delete" => {
                let wanted = words.collect::<Vec<_>>().join(" ");
                let Some(channel_id) = self.channel_id_named(&wanted) else {
                    self.push_command_output(&t!("chat-command-usage-channel"));
                    return;
                };
                if verb == "delete" {
                    self.network
                        .tx
                        .send(NetworkControl::Send(ClientMessage::DeleteChannel {
                            channel_id,
                        }))
                        .ok();
                } else if let Some(mut spec) = self.current_spec(channel_id) {
                    spec.permanent = true;
                    self.edit_channel(channel_id, spec);
                }
            }
            "kick" => {
                let Some(name) = words.next() else {
                    self.push_command_output(&t!("chat-command-usage-channel"));
                    return;
                };
                let (Some(channel_id), AdminTarget::User(user_id)) =
                    (self.our_channel(), self.target_for(name))
                else {
                    self.push_command_output(&t!("chat-command-no-such-user", "name" => name));
                    return;
                };
                self.network
                    .tx
                    .send(NetworkControl::Send(ClientMessage::ChannelKick {
                        channel_id,
                        user_id,
                    }))
                    .ok();
            }
            _ => self.push_command_output(&t!("chat-command-usage-channel")),
        }
    }

    /// Ask the server for a channel; remember its password locally so
    /// the creator joins it without a prompt and can edit it later
    /// (the server never sends passwords back).
    pub(super) fn create_channel(&mut self, spec: client::ChannelSpec) {
        if let Some(password) = spec.password.clone()
            && let Some(server) = self.settings.default_server_mut()
        {
            server.channel_passwords.insert(spec.name.clone(), password);
            self.save_settings();
        }
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::CreateChannel(spec)))
            .ok();
    }

    /// Send a whole new spec for a channel (the server replaces every
    /// field), keeping the local password copy in step.
    pub(super) fn edit_channel(&mut self, channel_id: u16, spec: client::ChannelSpec) {
        if let Some(server) = self.settings.default_server_mut() {
            let old_name = self
                .session
                .channels
                .iter()
                .find(|c| c.id == channel_id)
                .map(|c| c.name.clone());
            if let Some(old) = old_name
                && old != spec.name
            {
                server.channel_passwords.remove(&old);
            }
            match &spec.password {
                Some(p) => {
                    server
                        .channel_passwords
                        .insert(spec.name.clone(), p.clone());
                }
                None => {
                    server.channel_passwords.remove(&spec.name);
                }
            }
            self.save_settings();
        }
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::EditChannel {
                channel_id,
                spec,
            }))
            .ok();
    }

    /// A channel's settings as this client knows them: the wire info,
    /// the facts, and the password only if it was saved locally (the
    /// server never sends it, so an edit without it clears it).
    pub(super) fn current_spec(&self, channel_id: u16) -> Option<client::ChannelSpec> {
        let info = self.session.channel_infos.get(usize::from(channel_id))?;
        let facts = self.session.channel_details.get(&channel_id);
        let password = if info.has_password {
            self.settings
                .default_server()
                .and_then(|server| server.channel_passwords.get(&info.name).cloned())
        } else {
            None
        };
        Some(client::ChannelSpec {
            name: info.name.clone(),
            password,
            user_limit: info.user_limit,
            min_role: facts.and_then(|f| f.min_role),
            permanent: facts.is_none_or(|f| f.permanent),
            afk_after_min: facts.and_then(|f| f.afk_after_min),
        })
    }

    /// The `+`: an empty dialog.
    pub(super) fn channel_create(&mut self) {
        self.session.chat.channel_dialog = Some(ChannelDialog {
            editing: None,
            min_role: None,
            pending: false,
        });
        let s = self.main.global::<State>();
        s.set_channel_dialog_editing(false);
        s.set_channel_dialog_title(t!("channel-dialog-title-new").into());
        s.set_channel_dialog_name("".into());
        s.set_channel_dialog_password("".into());
        s.set_channel_dialog_password_note("".into());
        s.set_channel_dialog_limit("".into());
        s.set_channel_dialog_afk("".into());
        s.set_channel_dialog_keep(false);
        s.set_channel_dialog_members_only(false);
        s.set_channel_dialog_error("".into());
        s.set_channel_dialog_open(true);
    }

    /// "Edit channel…": the dialog filled with what this client knows
    /// (spec §5; the password only when it was saved here).
    pub(crate) fn channel_edit(&mut self, channel_id: u16) {
        let Some(spec) = self.current_spec(channel_id) else {
            return;
        };
        let has_password = self
            .session
            .channel_infos
            .get(usize::from(channel_id))
            .is_some_and(|c| c.has_password);
        self.session.chat.channel_dialog = Some(ChannelDialog {
            editing: Some(channel_id),
            min_role: spec.min_role,
            pending: false,
        });
        let s = self.main.global::<State>();
        s.set_channel_dialog_editing(true);
        s.set_channel_dialog_title(
            t!("channel-dialog-title-edit", "name" => spec.name.as_str()).into(),
        );
        s.set_channel_dialog_name(spec.name.into());
        s.set_channel_dialog_password_note(if has_password && spec.password.is_none() {
            t!("channel-password-unknown").into()
        } else {
            "".into()
        });
        s.set_channel_dialog_password(spec.password.unwrap_or_default().into());
        s.set_channel_dialog_limit(
            spec.user_limit
                .map_or(String::new(), |l| l.to_string())
                .into(),
        );
        s.set_channel_dialog_afk(
            spec.afk_after_min
                .map_or(String::new(), |m| m.to_string())
                .into(),
        );
        s.set_channel_dialog_keep(spec.permanent);
        s.set_channel_dialog_members_only(spec.min_role.is_some());
        s.set_channel_dialog_error("".into());
        s.set_channel_dialog_open(true);
    }

    /// "Delete channel…": confirm with the name, then send.
    pub(super) fn channel_delete(&mut self, channel_id: u16) {
        let name = self.channel_name(channel_id);
        self.session.chat.pending_confirm = Some(Confirm::DeleteChannel { channel_id });
        let s = self.main.global::<State>();
        s.set_confirm_title(t!("confirm-delete-channel-title", "name" => name.as_str()).into());
        s.set_confirm_body(t!("confirm-delete-channel-body").into());
        s.set_confirm_label(t!("confirm-delete-channel").into());
        s.set_confirm_open(true);
    }

    /// Save on the dialog: validate what the server would refuse as
    /// `Invalid` here, with a sentence per field, then send the spec.
    pub(super) fn channel_dialog_save(&mut self) {
        let Some(dialog) = self.session.chat.channel_dialog.as_mut() else {
            return;
        };
        if dialog.pending {
            return;
        }
        let s = self.main.global::<State>();
        let name = s.get_channel_dialog_name().trim().to_string();
        if name.is_empty() || name.chars().count() > 40 {
            s.set_channel_dialog_error(t!("channel-error-name").into());
            return;
        }
        let Ok(user_limit) = parse_optional_count(s.get_channel_dialog_limit().as_str()) else {
            s.set_channel_dialog_error(t!("channel-error-limit").into());
            return;
        };
        let Ok(afk_after_min) = parse_optional_count(s.get_channel_dialog_afk().as_str()) else {
            s.set_channel_dialog_error(t!("channel-error-afk").into());
            return;
        };
        let password = s.get_channel_dialog_password().trim().to_string();
        let min_role = s
            .get_channel_dialog_members_only()
            .then(|| dialog.min_role.unwrap_or(client::permissions::ROLE_MEMBER));
        let spec = client::ChannelSpec {
            name,
            password: (!password.is_empty()).then_some(password),
            user_limit,
            min_role,
            permanent: s.get_channel_dialog_keep(),
            afk_after_min,
        };
        s.set_channel_dialog_error("".into());
        dialog.pending = true;
        let editing = dialog.editing;
        match editing {
            Some(channel_id) => self.edit_channel(channel_id, spec),
            None => self.create_channel(spec),
        }
    }

    pub(super) fn channel_dialog_cancel(&mut self) {
        self.session.chat.channel_dialog = None;
        self.main.global::<State>().set_channel_dialog_open(false);
    }

    /// The wire id of a channel by name, case-insensitive.
    pub(super) fn channel_id_named(&self, name: &str) -> Option<u16> {
        self.session
            .channels
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name.trim()))
            .and_then(|i| u16::try_from(i).ok())
    }

    /// Whom a name refers to: the id when they are in the roster right
    /// now, else the name for the server to resolve (someone offline —
    /// a ban to lift, a kicked person's roles).
    pub(super) fn target_for(&self, name: &str) -> AdminTarget {
        let name = name.trim_start_matches('@');
        self.session
            .users
            .iter()
            .find(|(_, u)| u.username.eq_ignore_ascii_case(name))
            .and_then(|(client_id, _)| self.session.details.get(client_id))
            .map_or_else(
                || AdminTarget::Name(name.to_string()),
                |(user_id, _)| AdminTarget::User(*user_id),
            )
    }

    pub(crate) fn send_admin(&mut self, mut command: AdminCommand, then: PendingAdmin) {
        // What the server keeps of a reason (it cuts the same way); a
        // longer one could not even be sent as one message.
        command.clip_reason();
        // A printed answer lands in the conversation shown when it was
        // asked for — a menu pick as much as a typed command.
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        if matches!(then, PendingAdmin::Show) {
            self.command_conv = self.session.chat.active_dm;
            // A typed command is named in the activity log when the
            // server says it is done. A menu pick is not: what it did
            // shows where it was picked.
            if let Some(typed) = self.typed_command.take() {
                self.session.admin_typed.insert(request_id, typed);
            }
        }
        self.session.pending_admin.insert(request_id, then);
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::Admin {
                request_id,
                command,
            }))
            .ok();
    }

    /// The line under a typed listing that was cut to
    /// [`COMMAND_LIST_ROWS`]: how many more there are, and where.
    fn push_list_rest(&mut self, total: usize) {
        if let Some(rest) = total
            .checked_sub(COMMAND_LIST_ROWS)
            .filter(|rest| *rest > 0)
        {
            self.push_command_output(&t!("chat-admin-list-more", "count" => rest));
        }
    }

    pub(super) fn handle_admin_result(
        &mut self,
        request_id: u32,
        result: Result<client::AdminOutcome, client::AdminError>,
    ) {
        let then = self
            .session
            .pending_admin
            .remove(&request_id)
            .unwrap_or(PendingAdmin::Show);
        let typed = self.session.admin_typed.remove(&request_id);
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                if let PendingAdmin::Telemetry(client_id) = then {
                    // A tooltip fetch that was refused is a log line,
                    // not a chat notice.
                    log::info!("telemetry for {client_id:#x} refused: {}", e.describe());
                    return;
                }
                let why = describe_admin_error(&e);
                if matches!(then, PendingAdmin::Panel(_) | PendingAdmin::AuditMore) {
                    crate::adminui::take_error(self, &why);
                    return;
                }
                if matches!(then, PendingAdmin::Channel) {
                    let line = t!("chat-admin-refused", "why" => why.as_str());
                    self.with_conversation(Some(ADMIN_CHANNEL_KEY), |app| {
                        app.push_system_here(&line);
                    });
                    return;
                }
                self.push_command_output(&t!("chat-admin-refused", "why" => why.as_str()));
                return;
            }
        };
        match then {
            PendingAdmin::Panel(tab) => {
                crate::adminui::take_outcome(self, tab, outcome);
                return;
            }
            PendingAdmin::AuditMore => {
                crate::adminui::take_outcome(self, -1, outcome);
                return;
            }
            PendingAdmin::Channel => {
                if let client::AdminOutcome::Audit(entries) = outcome {
                    // Newest first on the wire; oldest first in a chat.
                    for e in entries.iter().rev() {
                        let line = audit_line_text(e);
                        self.with_conversation(Some(ADMIN_CHANNEL_KEY), |app| {
                            app.push_system_here(&line);
                        });
                    }
                }
                return;
            }
            PendingAdmin::Show | PendingAdmin::Telemetry(_) => {}
        }
        if let PendingAdmin::Telemetry(client_id) = then {
            if let client::AdminOutcome::Telemetry(sample) = outcome {
                let text = match sample {
                    Some(t) => {
                        let ago = describe_ago(t.ts);
                        format!(
                            "{}\n{}",
                            t!("tooltip-telemetry-age", "ago" => ago.as_str()),
                            t.line.replace(" | ", "\n")
                        )
                    }
                    None => t!("tooltip-no-telemetry"),
                };
                self.session.telemetry_notes.insert(
                    client_id,
                    TelemetryNote {
                        fetched: Instant::now(),
                        text,
                    },
                );
                self.update_user_list();
            }
            return;
        }
        match outcome {
            // Nothing to show but that it worked: not a line in
            // whatever conversation is open (it read "Done." under
            // every kick, mute and move), but one in the activity log
            // for a command that was typed, which has no other answer.
            client::AdminOutcome::Done => {
                if let Some(typed) = typed {
                    self.push_activity(&t!("activity-command-done", "command" => typed.as_str()));
                }
            }
            client::AdminOutcome::Users(users) => {
                let total = users.len();
                self.push_command_output(&t!("chat-admin-users-header", "count" => total));
                for u in users.into_iter().take(COMMAND_LIST_ROWS) {
                    let roles: Vec<String> =
                        u.roles.iter().map(|r| self.role_name_of(*r)).collect();
                    // Online is said in words: no "●", which Android's
                    // system font cannot draw.
                    let line = format!(
                        "{} — {} — {} — {}",
                        u.name,
                        if roles.is_empty() {
                            t!("admin-list-no-roles")
                        } else {
                            roles.join(", ")
                        },
                        short_id(&u.user_id),
                        if u.client_id.is_some() {
                            t!("admin-now")
                        } else {
                            describe_ago(u.last_seen)
                        }
                    );
                    self.push_command_output(&line);
                }
                self.push_list_rest(total);
            }
            client::AdminOutcome::Restrictions(list) => {
                let total = list.len();
                self.push_command_output(&t!(
                    "chat-admin-restrictions-header",
                    "count" => total
                ));
                for r in list.into_iter().take(COMMAND_LIST_ROWS) {
                    let who = r
                        .user_name
                        .clone()
                        .or_else(|| {
                            r.key_hash.map(|k| {
                                let id = short_id(&k);
                                t!("admin-list-key", "id" => id.as_str())
                            })
                        })
                        .or_else(|| r.address.clone())
                        .unwrap_or_default();
                    let line = format!(
                        "#{} {} {} — {} — {} — {}",
                        r.id,
                        r.kind.name(),
                        who,
                        describe_expiry(r.until),
                        r.reason,
                        r.by
                    );
                    self.push_command_output(&line);
                }
                self.push_list_rest(total);
            }
            client::AdminOutcome::Telemetry(sample) => match sample {
                Some(t) => {
                    let line = format!("{} — {}", describe_ago(t.ts), t.line);
                    self.push_command_output(&line);
                }
                None => self.push_command_output(&t!("chat-admin-no-telemetry")),
            },
            client::AdminOutcome::Invites(list) => {
                let total = list.len();
                self.push_command_output(&t!("chat-admin-invites-header", "count" => total));
                for i in list.into_iter().take(COMMAND_LIST_ROWS) {
                    let uses = match i.max_uses {
                        Some(max) => format!("{}/{max}", i.uses),
                        None => i.uses.to_string(),
                    };
                    let state = if i.revoked {
                        format!(" ({})", t!("admin-revoked"))
                    } else {
                        String::new()
                    };
                    let line = format!(
                        "{} — {} — {} — {}{state}",
                        client::invite::display_code(&i.code),
                        self.role_name_of(i.grants_role),
                        describe_expiry(i.expires_at),
                        t!("admin-list-uses", "uses" => uses.as_str())
                    );
                    self.push_command_output(&line);
                }
                self.push_list_rest(total);
            }
            client::AdminOutcome::Reports(list) => {
                let total = list.len();
                self.push_command_output(&t!("chat-admin-reports-header", "count" => total));
                for r in list.into_iter().take(COMMAND_LIST_ROWS) {
                    let mut line = format!(
                        "#{} {} — {} — {}",
                        r.id,
                        format_wall_stamp(r.ts),
                        r.target_name,
                        reason_label(r.reason)
                    );
                    if !r.excerpt.is_empty() {
                        let short: String = r.excerpt.chars().take(120).collect();
                        line.push_str(" — “");
                        line.push_str(&short);
                        line.push('”');
                    }
                    if !r.note.is_empty() {
                        line.push_str(" — ");
                        line.push_str(&r.note);
                    }
                    if let Some(by) = &r.reporter {
                        line.push_str(" (");
                        line.push_str(by);
                        line.push(')');
                    }
                    if r.resolved {
                        line.push_str(" — ");
                        line.push_str(&t!("chat-admin-report-handled"));
                    }
                    self.push_command_output(&line);
                }
                self.push_list_rest(total);
            }
            // Answers to what only the admin panel asks, and it takes
            // its own (`adminui::take_outcome`): nothing typed leads here.
            client::AdminOutcome::RoleCreated(_)
            | client::AdminOutcome::Stats(_)
            | client::AdminOutcome::Blobs(_)
            | client::AdminOutcome::Audit(_) => {}
        }
    }

    // ── Invite dialog (spec §3.1) ───────────────────────────────────
    // The picks are indices into option lists the dialog shows; the
    // wire carries what the slash command would send for the same
    // words, so the server applies the same rules (a member's expiry
    // is capped at the default, roles at or above admin refused).

    /// "Invite users": open the dialog with the defaults (`/invite`'s:
    /// guest, a day, unlimited).
    pub(crate) fn open_invite_dialog(&mut self) {
        let s = self.main.global::<State>();
        let strings = |keys: &[&str]| -> ModelRc<slint::SharedString> {
            let items: Vec<slint::SharedString> = keys.iter().map(|k| t!(k).into()).collect();
            ModelRc::new(VecModel::from(items))
        };
        s.set_invite_role_options(strings(&["role-guest", "role-member", "role-organizer"]));
        s.set_invite_expiry_options(strings(&[
            "invite-expiry-1h",
            "invite-expiry-1d",
            "invite-expiry-7d",
            "invite-expiry-30d",
        ]));
        s.set_invite_uses_options(strings(&[
            "invite-uses-1",
            "invite-uses-5",
            "invite-uses-10",
            "invite-uses-unlimited-option",
        ]));
        s.set_invite_dialog_role(0);
        s.set_invite_dialog_expiry(1);
        s.set_invite_dialog_uses(3);
        s.set_invite_dialog_code("".into());
        s.set_invite_dialog_summary("".into());
        s.set_invite_dialog_error("".into());
        s.set_invite_dialog_open(true);
    }

    /// "Create invite": mint with the picks. The answer lands in the
    /// dialog (`InviteCreated` / `InviteDenied`).
    pub(super) fn invite_dialog_create(&mut self) {
        let s = self.main.global::<State>();
        let grants_role = match s.get_invite_dialog_role() {
            1 => Some(client::permissions::ROLE_MEMBER),
            2 => Some(client::permissions::ROLE_ORGANIZER),
            _ => None,
        };
        let expires_secs = match s.get_invite_dialog_expiry() {
            0 => Some(3_600),
            2 => Some(7 * 86_400),
            3 => Some(30 * 86_400),
            _ => Some(86_400),
        };
        let max_uses = match s.get_invite_dialog_uses() {
            0 => Some(1),
            1 => Some(5),
            2 => Some(10),
            _ => None,
        };
        s.set_invite_dialog_error("".into());
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::CreateInvite {
                grants_role,
                expires_secs,
                max_uses,
            }))
            .ok();
    }

    /// The code, again, onto the clipboard (it went there when minted).
    pub(super) fn invite_dialog_copy(&mut self) {
        let code = self.main.global::<State>().get_invite_dialog_code();
        if code.is_empty() {
            return;
        }
        crate::clipboard::set_clipboard_text(&code);
        self.show_toast(ToastKind::Success, &t!("invite-copied"));
    }

    pub(super) fn handle_invite_command(&mut self, args: &str) {
        let mut grants_role = None;
        let mut expires_secs = None;
        let mut max_uses = None;
        for word in args.split_whitespace() {
            let lower = word.to_ascii_lowercase();
            if let Some(role) = client::permissions::BUILTIN_ROLES
                .iter()
                .find(|r| r.name == lower)
            {
                grants_role = Some(role.id);
            } else if let Some(secs) = parse_duration_secs(&lower) {
                expires_secs = Some(secs);
            } else if let Ok(n) = lower.parse::<u32>() {
                max_uses = Some(n);
            } else {
                self.push_command_output(&t!("chat-invite-usage"));
                return;
            }
        }
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::CreateInvite {
                grants_role,
                expires_secs,
                max_uses,
            }))
            .ok();
    }
}
