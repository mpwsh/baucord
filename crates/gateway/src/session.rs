//! One bot's session with a server, as a state machine over a
//! [`transport::Driver`]: the same `Hello`, lanes and messages the
//! desktop client speaks (`client/src/network.rs`), cut down to what
//! a bot does — the Control lane in both directions and voice out —
//! and translated into the JSON of [`crate::wire`].
//!
//! No I/O here: the UDP loop (`net.rs`) and the in-process network of
//! the tests both pump the driver and call [`Session::tick`]. What the
//! session wants the bot to know collects in an outbox the loop drains.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use protocol::{
    features, Auth, ChatMessage, ClientId, ClientKeyProof, ClientMessage, ConvRef, Lane,
    ServerMessage, UserId,
};
use serde_json::{json, Value};
use transport::identity::{CLIENT_AUTH_EXPORTER_BYTES, CLIENT_AUTH_EXPORTER_LABEL};
use transport::{
    ClientKey, DisconnectReason, Driver, DriverHost, Event, Observation, PeerId, Trust,
};

use crate::wire::{conv_json, hex, Action, Outbound};

/// One Opus frame: 20 ms at 48 kHz.
pub const VOICE_FRAME_SAMPLES: u32 = 960;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The QUIC handshake is in flight.
    Connecting,
    /// `Hello` sent; the verdict is `Welcome` or `Refused`.
    AwaitingWelcome,
    /// Admitted; the roster follows in `Init`.
    Admitted,
    Closed,
}

/// Why the session ended, for the loop that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    /// A fresh connection with the same parameters is the sensible
    /// next step (the link dropped); otherwise the server said no and
    /// trying again changes nothing until an operator acts.
    pub transient: bool,
    pub reason: String,
}

/// Someone on the server, as the roster knows them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    /// `None` in the lobby.
    pub channel: Option<u16>,
    pub user_id: Option<UserId>,
    pub roles: Vec<u32>,
    pub level: u16,
}

struct PendingHello {
    name: String,
    invite: Option<String>,
    key: Arc<ClientKey>,
}

pub struct Session {
    driver: Driver,
    server: PeerId,
    phase: Phase,
    hello: Option<PendingHello>,
    /// What the handshake saw of the server's identity.
    pub identity: Option<Observation>,
    /// The id the server gave this session.
    pub me: Option<ClientId>,
    pub user_id: Option<UserId>,
    pub permissions: u64,
    server_name: String,
    greeting: String,
    channels: Vec<protocol::ChannelInfo>,
    roster: HashMap<ClientId, Member>,
    /// Names of people not on the roster (history authors, lobby
    /// posters), from `Authors`.
    authors: HashMap<UserId, String>,
    outbox: Vec<Outbound>,
    ended: Option<End>,
    voice_seq: u32,
    voice_ts: u32,
    voice_buf: Vec<u8>,
    speaking: bool,
    /// `Init` arrived: the READY snapshot is complete.
    ready: bool,
}

impl DriverHost for Session {
    fn driver(&mut self) -> &mut Driver {
        &mut self.driver
    }
}

impl Session {
    /// Wrap a driver whose `connect` to the server was just called.
    pub fn new(
        driver: Driver,
        server: PeerId,
        name: &str,
        invite: Option<&str>,
        key: Arc<ClientKey>,
    ) -> Self {
        Self {
            driver,
            server,
            phase: Phase::Connecting,
            hello: Some(PendingHello {
                name: name.to_string(),
                invite: invite.map(str::to_string),
                key,
            }),
            identity: None,
            me: None,
            user_id: None,
            permissions: 0,
            server_name: String::new(),
            greeting: String::new(),
            channels: Vec::new(),
            roster: HashMap::new(),
            authors: HashMap::new(),
            outbox: Vec::new(),
            ended: None,
            voice_seq: 0,
            voice_ts: 0,
            voice_buf: Vec::with_capacity(protocol::VOICE_MTU_BUDGET),
            speaking: false,
            ready: false,
        }
    }

    /// The session is over, and why.
    #[must_use]
    pub fn ended(&self) -> Option<&End> {
        self.ended.as_ref()
    }

    /// Admitted and past `Init`: the bot can act.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready && self.phase == Phase::Admitted
    }

    /// What the session wants the bot told, oldest first.
    pub fn take_outbox(&mut self) -> Vec<Outbound> {
        std::mem::take(&mut self.outbox)
    }

    /// Drive the state machine: the driver's events, then every
    /// control message it has. The caller has fed it datagrams and
    /// timeouts for `now` already.
    pub fn tick(&mut self, now: Instant) {
        if self.phase == Phase::Closed {
            return;
        }
        let events: Vec<Event> = self.driver.drain_events().collect();
        for event in events {
            match event {
                Event::Connected { identity, .. } => {
                    self.identity = identity;
                    if let Some(Observation::Accepted { fingerprint, trust }) = &self.identity {
                        info!("transport up; server identity {fingerprint} ({trust:?})");
                        if matches!(trust, Trust::FirstUse) {
                            warn!("no pin: trusting {fingerprint} on first use (--pin {fingerprint} to insist on it)");
                        }
                    }
                    self.send_hello(now);
                }
                Event::Disconnected { reason, .. } => self.on_disconnected(&reason),
                Event::DatagramsBlocked { .. } => {
                    warn!("voice send buffer full; the oldest packets are being dropped");
                }
            }
        }
        if self.phase == Phase::Closed {
            return;
        }
        for lane in [Lane::Control, Lane::Signal, Lane::Media] {
            while let Some(raw) = self.driver.recv(self.server, lane) {
                let Some(msg) = protocol::decode_server_message(&raw) else {
                    debug!(
                        "skipped an undecodable {lane} message ({} bytes)",
                        raw.len()
                    );
                    continue;
                };
                self.handle(now, msg);
                if self.phase == Phase::Closed {
                    return;
                }
            }
        }
    }

    fn send_hello(&mut self, now: Instant) {
        let Some(hello) = self.hello.take() else {
            return;
        };
        let mut exported = [0u8; CLIENT_AUTH_EXPORTER_BYTES];
        let client_key = self
            .driver
            .export_keying_material(self.server, CLIENT_AUTH_EXPORTER_LABEL, b"", &mut exported)
            .map(|()| ClientKeyProof {
                public_key: hello.key.public_key(),
                signature: hello.key.sign(&exported),
            });
        if client_key.is_none() {
            warn!("no TLS exporter after the handshake; the server will refuse the Hello");
        }
        let auth = match hello.invite {
            Some(code) => Auth::Invite { code },
            None => Auth::Key,
        };
        self.send(
            now,
            &ClientMessage::Hello {
                username: hello.name,
                auth,
                client_key,
                features: features::CLIENT_1_1,
                client_version: protocol::CLIENT_VERSION.to_string(),
            },
        );
        self.phase = Phase::AwaitingWelcome;
    }

    fn on_disconnected(&mut self, reason: &DisconnectReason) {
        if self.phase == Phase::Closed {
            return;
        }
        let was_up = self.phase == Phase::Admitted;
        self.phase = Phase::Closed;
        let transient = reason.is_transient() || matches!(reason, DisconnectReason::Refused);
        // A refusal at the door ("full") is worth another try later,
        // like a dropped link; an identity or version mismatch is not.
        let reason = reason.to_string();
        info!(
            "disconnected ({reason}){}",
            if transient { "; will reconnect" } else { "" }
        );
        if self.ended.is_none() {
            self.ended = Some(End {
                transient,
                reason: reason.clone(),
            });
        }
        self.push(Outbound::status(
            "DISCONNECTED",
            json!({ "reason": reason, "was_connected": was_up, "reconnecting": transient }),
        ));
    }

    fn push(&mut self, out: Outbound) {
        self.outbox.push(out);
    }

    fn send(&mut self, now: Instant, msg: &ClientMessage) {
        let Some(encoded) = protocol::encode_client_message(msg) else {
            warn!("could not encode a {msg:?}");
            return;
        };
        if let Err(e) = self.driver.send(now, self.server, msg.lane(), &encoded) {
            warn!("send failed: {e}");
        }
    }

    /// Do what the bot asked. Refused with a reason before the session
    /// is ready; the words go to the bot in an `error` frame.
    ///
    /// # Errors
    /// The session is not ready, or the action cannot be sent.
    pub fn act(&mut self, now: Instant, action: Action) -> Result<(), String> {
        if !self.is_ready() {
            return Err("not connected to the server yet".to_string());
        }
        let msg = match action {
            Action::CreateMessage { text } => ClientMessage::Chat(text),
            Action::CreateDm {
                to,
                text,
                client_ref,
            } => ClientMessage::DirectMessage {
                to,
                text,
                client_ref,
            },
            Action::EditMessage { id, text } => ClientMessage::EditMessage { msg_id: id, text },
            Action::DeleteMessage { id } => ClientMessage::DeleteMessage { msg_id: id },
            Action::JoinChannel {
                channel_id,
                password,
            } => ClientMessage::JoinChannel {
                channel_id,
                password,
            },
            Action::Speaking(on) => {
                self.set_speaking(now, on);
                return Ok(());
            }
            Action::AudioState { muted, deafened } => ClientMessage::AudioState { muted, deafened },
            Action::SetNickname { name } => ClientMessage::SetNickname { name },
            Action::FetchHistory {
                conv,
                before,
                limit,
            } => ClientMessage::FetchHistory {
                conv,
                before,
                limit,
            },
            Action::MarkRead { conv, id } => ClientMessage::MarkRead { conv, msg_id: id },
        };
        self.send(now, &msg);
        Ok(())
    }

    /// The speaking light, sent only when it changes.
    pub fn set_speaking(&mut self, now: Instant, on: bool) {
        if self.speaking == on || !self.is_ready() {
            return;
        }
        self.speaking = on;
        self.send(now, &ClientMessage::Speaking(on));
    }

    /// One encoded 20 ms Opus frame onto the Voice lane. The sequence
    /// and the 48 kHz clock advance by one frame each; a frame that is
    /// not sent (not ready, too large) still advances them, so the
    /// listeners' jitter buffers see the gap for what it is.
    pub fn send_voice(&mut self, now: Instant, opus: &[u8]) {
        let seq = self.voice_seq;
        let ts = self.voice_ts;
        self.voice_seq = self.voice_seq.wrapping_add(1);
        self.voice_ts = self.voice_ts.wrapping_add(VOICE_FRAME_SAMPLES);
        if !self.is_ready() {
            return;
        }
        if !protocol::pack_voice_into(&mut self.voice_buf, seq, ts, 0, opus) {
            debug!("voice frame seq={seq} dropped: {} bytes", opus.len());
            return;
        }
        if let Err(e) = self
            .driver
            .send(now, self.server, Lane::Voice, &self.voice_buf)
        {
            debug!("voice frame seq={seq} not sent: {e}");
        }
    }

    /// Close the connection ourselves (the gateway is shutting down).
    pub fn close(&mut self, now: Instant) {
        if self.phase != Phase::Closed {
            self.phase = Phase::Closed;
            self.driver.close(now, self.server, "bye");
        }
    }

    /// Where the server's keep-alive and the driver's own timers want
    /// the loop back.
    pub fn next_deadline(&mut self) -> Option<Instant> {
        self.driver.next_deadline()
    }

    /// The `READY` data, for a bot that connects to the gateway after
    /// the session came up.
    #[must_use]
    pub fn ready_snapshot(&self) -> Option<Value> {
        self.is_ready().then(|| self.ready_json())
    }

    fn ready_json(&self) -> Value {
        let mut members: Vec<Value> = self
            .roster
            .iter()
            .map(|(id, m)| member_json(*id, m))
            .collect();
        members.sort_by(|a, b| a["client_id"].as_str().cmp(&b["client_id"].as_str()));
        json!({
            "client_id": client_json(self.me.unwrap_or(0)),
            "user_id": self.user_id.map(|u| hex(&u)),
            "permissions": self.permissions,
            "server_name": self.server_name,
            "greeting": self.greeting,
            "channels": self.channels.iter().map(channel_json).collect::<Vec<_>>(),
            "members": members,
        })
    }

    fn name_of(&self, client_id: ClientId) -> Option<&str> {
        self.roster.get(&client_id).map(|m| m.name.as_str())
    }

    fn message_json(&self, msg: &ChatMessage, user_id: Option<UserId>, conv: &ConvRef) -> Value {
        let user_id = user_id.or_else(|| self.roster.get(&msg.client_id).and_then(|m| m.user_id));
        let name = self
            .name_of(msg.client_id)
            .map(str::to_string)
            .or_else(|| user_id.and_then(|u| self.authors.get(&u).cloned()));
        json!({
            "id": msg.id,
            "ts": msg.ts,
            "client_id": client_json(msg.client_id),
            "user_id": user_id.map(|u| hex(&u)),
            "name": name,
            "text": msg.text,
            "conv": conv_json(conv),
            "own": self.me == Some(msg.client_id),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, now: Instant, msg: ServerMessage) {
        match msg {
            ServerMessage::Welcome {
                client_id,
                user_id,
                features: server_features,
            } => {
                if self.phase != Phase::AwaitingWelcome {
                    return;
                }
                self.phase = Phase::Admitted;
                self.me = Some(client_id);
                self.user_id = user_id;
                info!("admitted as {client_id:#x} (server features {server_features:#x})");
                self.push(Outbound::status(
                    "CONNECTED",
                    json!({
                        "client_id": client_json(client_id),
                        "user_id": user_id.map(|u| hex(&u)),
                        "server_features": server_features,
                        "fingerprint": match &self.identity {
                            Some(Observation::Accepted { fingerprint, .. }) => Some(fingerprint.to_string()),
                            _ => None,
                        },
                    }),
                ));
            }
            ServerMessage::Refused { reason } => {
                let why = reason.describe().to_string();
                warn!("the server refused us: {why}");
                self.phase = Phase::Closed;
                self.ended = Some(End {
                    transient: matches!(
                        reason,
                        protocol::RefusedReason::ServerFull | protocol::RefusedReason::RateLimited
                    ),
                    reason: why.clone(),
                });
                self.push(Outbound::status(
                    "REFUSED",
                    json!({ "reason": why, "kind": format!("{reason:?}") }),
                ));
                self.driver.close(now, self.server, "refused");
            }
            ServerMessage::Init {
                usernames,
                client_id,
                server_name,
                greeting,
                channels,
                memberships,
            } => {
                self.me = Some(client_id);
                self.server_name = server_name;
                self.greeting = greeting;
                self.channels = channels;
                self.roster.clear();
                for (id, name) in usernames {
                    self.roster.insert(
                        id,
                        Member {
                            name,
                            channel: memberships.get(&id).copied(),
                            ..Member::default()
                        },
                    );
                }
                self.ready = true;
                let ready = self.ready_json();
                self.push(Outbound::dispatch("READY", ready));
            }
            ServerMessage::Session {
                user_id,
                permissions,
                persistent,
            } => {
                self.user_id = Some(user_id);
                self.permissions = permissions;
                self.push(Outbound::dispatch(
                    "SESSION",
                    json!({
                        "user_id": hex(&user_id),
                        "permissions": permissions,
                        "persistent": persistent,
                    }),
                ));
            }
            ServerMessage::UserJoined {
                client_id,
                username,
                channel_id,
            } => {
                let member = Member {
                    name: username,
                    channel: Some(channel_id),
                    ..Member::default()
                };
                let d = member_json(client_id, &member);
                self.roster.insert(client_id, member);
                self.push(Outbound::dispatch("MEMBER_ADD", d));
            }
            ServerMessage::UserLeft { client_id } => {
                let name = self.roster.remove(&client_id).map(|m| m.name);
                self.push(Outbound::dispatch(
                    "MEMBER_REMOVE",
                    json!({ "client_id": client_json(client_id), "name": name }),
                ));
            }
            ServerMessage::UserMovedChannel {
                client_id,
                channel_id,
            } => {
                if let Some(m) = self.roster.get_mut(&client_id) {
                    m.channel = Some(channel_id);
                }
                self.push(Outbound::dispatch(
                    "VOICE_STATE",
                    json!({ "client_id": client_json(client_id), "channel_id": channel_id }),
                ));
            }
            ServerMessage::UserRenamed { client_id, name } => {
                if let Some(m) = self.roster.get_mut(&client_id) {
                    m.name.clone_from(&name);
                }
                self.push(Outbound::dispatch(
                    "MEMBER_UPDATE",
                    json!({ "client_id": client_json(client_id), "name": name }),
                ));
            }
            ServerMessage::UserDetails {
                client_id,
                user_id,
                roles,
                level,
            } => {
                let m = self.roster.entry(client_id).or_default();
                m.user_id = Some(user_id);
                m.roles.clone_from(&roles);
                m.level = level;
                self.push(Outbound::dispatch(
                    "MEMBER_UPDATE",
                    json!({
                        "client_id": client_json(client_id),
                        "user_id": hex(&user_id),
                        "roles": roles,
                        "level": level,
                    }),
                ));
            }
            ServerMessage::ChannelListUpdated {
                channels,
                memberships,
            } => {
                self.channels = channels;
                for (id, m) in &mut self.roster {
                    m.channel = memberships.get(id).copied();
                }
                self.push(Outbound::dispatch(
                    "CHANNELS",
                    json!({
                        "channels": self.channels.iter().map(channel_json).collect::<Vec<_>>(),
                        "memberships": memberships.iter()
                            .map(|(id, ch)| json!({ "client_id": client_json(*id), "channel_id": ch }))
                            .collect::<Vec<_>>(),
                    }),
                ));
            }
            ServerMessage::JoinChannelDenied { channel_id, reason } => {
                self.push(Outbound::dispatch(
                    "JOIN_DENIED",
                    json!({ "channel_id": channel_id, "reason": reason.describe() }),
                ));
            }
            ServerMessage::Chat(msg) => {
                let d = self.message_json(&msg, None, &ConvRef::General);
                self.push(Outbound::dispatch("MESSAGE_CREATE", d));
            }
            ServerMessage::ChatFrom { from, msg } => {
                let d = self.message_json(&msg, Some(from), &ConvRef::General);
                self.push(Outbound::dispatch("MESSAGE_CREATE", d));
            }
            ServerMessage::Direct { from, msg } => {
                let mut d = self.message_json(&msg, Some(from), &ConvRef::Dm(from));
                if d["name"].is_null()
                    && let Some(name) = self.authors.get(&from)
                {
                    d["name"] = Value::String(name.clone());
                }
                self.push(Outbound::dispatch("MESSAGE_CREATE", d));
            }
            ServerMessage::FileMessage {
                conv,
                from,
                msg,
                attachment,
            } => {
                let mut d = self.message_json(&msg, Some(from), &conv);
                d["attachment"] = json!({
                    "blob_id": hex(&attachment.blob_id),
                    "name": attachment.name,
                    "mime": attachment.mime,
                    "size": attachment.size,
                });
                self.push(Outbound::dispatch("MESSAGE_CREATE", d));
            }
            ServerMessage::MessageEdited { msg_id, text } => {
                self.push(Outbound::dispatch(
                    "MESSAGE_UPDATE",
                    json!({ "id": msg_id, "text": text }),
                ));
            }
            ServerMessage::MessageDeleted { msg_id } => {
                self.push(Outbound::dispatch(
                    "MESSAGE_DELETE",
                    json!({ "id": msg_id }),
                ));
            }
            ServerMessage::MessageAck { client_ref, msg_id } => {
                self.push(Outbound::dispatch(
                    "MESSAGE_ACK",
                    json!({ "client_ref": client_ref, "id": msg_id }),
                ));
            }
            ServerMessage::MessageFailed { client_ref, reason } => {
                self.push(Outbound::dispatch(
                    "MESSAGE_FAILED",
                    json!({ "client_ref": client_ref, "reason": format!("{reason:?}") }),
                ));
            }
            ServerMessage::ChatRateLimited { retry_ms } => {
                self.push(Outbound::dispatch(
                    "RATE_LIMITED",
                    json!({ "retry_ms": retry_ms }),
                ));
            }
            ServerMessage::History {
                conv,
                messages,
                more,
            } => {
                let items: Vec<Value> = messages
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.id, "ts": m.ts, "user_id": hex(&m.user_id),
                            "name": m.name, "text": m.text, "edited": m.edited,
                        })
                    })
                    .collect();
                self.push(Outbound::dispatch(
                    "HISTORY",
                    json!({ "conv": conv_json(&conv), "messages": items, "more": more }),
                ));
            }
            ServerMessage::HistoryUnavailable { conv, retry_secs } => {
                self.push(Outbound::dispatch(
                    "HISTORY_UNAVAILABLE",
                    json!({ "conv": conv_json(&conv), "retry_secs": retry_secs }),
                ));
            }
            ServerMessage::Authors { authors } => {
                for a in authors {
                    self.authors.insert(a.user_id, a.name);
                }
            }
            ServerMessage::Speaking {
                client_id,
                is_speaking,
            } => {
                self.push(Outbound::dispatch(
                    "SPEAKING",
                    json!({ "client_id": client_json(client_id), "speaking": is_speaking }),
                ));
            }
            ServerMessage::AudioState {
                client_id,
                muted,
                deafened,
            } => {
                self.push(Outbound::dispatch(
                    "AUDIO_STATE",
                    json!({ "client_id": client_json(client_id), "muted": muted, "deafened": deafened }),
                ));
            }
            ServerMessage::ScreenShareStarted {
                client_id,
                width,
                height,
                scope,
            } => {
                self.push(Outbound::dispatch(
                    "SHARE_START",
                    json!({
                        "client_id": client_json(client_id), "width": width, "height": height,
                        "everyone": matches!(scope, protocol::ShareScope::Everyone),
                    }),
                ));
            }
            ServerMessage::ScreenShareStopped { client_id } => {
                self.push(Outbound::dispatch(
                    "SHARE_STOP",
                    json!({ "client_id": client_json(client_id) }),
                ));
            }
            // 1.5: the channel timer and recordings. A bot is recorded
            // like anyone in the room, and told so.
            ServerMessage::ChannelClock {
                channel_id,
                running_ms,
            } => {
                self.push(Outbound::dispatch(
                    "CHANNEL_CLOCK",
                    json!({ "channel_id": channel_id, "running_ms": running_ms }),
                ));
            }
            ServerMessage::RecordingStarted {
                channel_id,
                rec_id,
                elapsed_ms,
                by_name,
                ..
            } => {
                self.push(Outbound::dispatch(
                    "RECORDING_START",
                    json!({
                        "channel_id": channel_id,
                        "rec_id": rec_id,
                        "elapsed_ms": elapsed_ms,
                        "by": by_name,
                    }),
                ));
            }
            ServerMessage::RecordingStopped {
                channel_id,
                rec_id,
                duration_ms,
                reason,
                by_name,
            } => {
                self.push(Outbound::dispatch(
                    "RECORDING_STOP",
                    json!({
                        "channel_id": channel_id,
                        "rec_id": rec_id,
                        "duration_ms": duration_ms,
                        "reason": reason.describe(),
                        "by": by_name,
                    }),
                ));
            }
            ServerMessage::RecordingReady { item } => {
                self.push(Outbound::dispatch(
                    "RECORDING_READY",
                    json!({
                        "rec_id": item.rec_id,
                        "channel": item.channel,
                        "started_at": item.started_at,
                        "duration_ms": item.duration_ms,
                        "file_name": item.file_name,
                        "size": item.size,
                        "blob_id": hex(&item.blob_id),
                    }),
                ));
            }
            ServerMessage::VoiceReceiverReport {
                received,
                expected,
                lost,
                late,
                rtt_ms,
            } => {
                self.push(Outbound::dispatch(
                    "VOICE_REPORT",
                    json!({
                        "received": received, "expected": expected,
                        "lost": lost, "late": late, "rtt_ms": rtt_ms,
                    }),
                ));
            }
            ServerMessage::Removed { kind, reason, by } => {
                let kind = format!("{kind:?}");
                warn!("removed by {by}: {kind} ({reason})");
                self.push(Outbound::status(
                    "REMOVED",
                    json!({ "kind": kind, "reason": reason, "by": by }),
                ));
                // The server closes the connection next; whatever it
                // says, this is not a link to re-dial.
                self.ended = Some(End {
                    transient: false,
                    reason: format!("removed: {kind} ({reason})"),
                });
            }
            ServerMessage::Restricted {
                kind,
                until,
                reason,
            } => {
                self.push(Outbound::dispatch(
                    "RESTRICTED",
                    json!({ "kind": format!("{kind:?}"), "until": until, "reason": reason }),
                ));
            }
            ServerMessage::RestrictionLifted { kind } => {
                self.push(Outbound::dispatch(
                    "RESTRICTION_LIFTED",
                    json!({ "kind": format!("{kind:?}") }),
                ));
            }
            ServerMessage::RoleList { roles } => {
                let items: Vec<Value> = roles
                    .iter()
                    .map(|r| {
                        json!({
                            "id": r.id, "name": r.name, "rank": r.rank,
                            "permissions": r.permissions, "builtin": r.builtin,
                        })
                    })
                    .collect();
                self.push(Outbound::dispatch("ROLES", json!({ "roles": items })));
            }
            ServerMessage::RenameDenied { reason } => {
                self.push(Outbound::error(reason.describe(), Some("SET_NICKNAME")));
            }
            ServerMessage::MovedIdle { after_min } => {
                self.push(Outbound::dispatch(
                    "MOVED_IDLE",
                    json!({ "after_min": after_min }),
                ));
            }
            other => debug!("ignored a {}", variant_name(&other)),
        }
    }
}

/// A client id as the JSON carries it: decimal digits in a string,
/// since JavaScript numbers lose the top bits of a `u64`.
#[must_use]
pub fn client_json(id: ClientId) -> Value {
    Value::String(id.to_string())
}

fn channel_json(c: &protocol::ChannelInfo) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "has_password": c.has_password,
        "user_limit": c.user_limit,
    })
}

fn member_json(id: ClientId, m: &Member) -> Value {
    json!({
        "client_id": client_json(id),
        "name": m.name,
        "channel_id": m.channel,
        "user_id": m.user_id.map(|u| hex(&u)),
        "roles": m.roles,
        "level": m.level,
    })
}

/// The variant's name for a debug line (`Debug` would print the body).
fn variant_name(msg: &ServerMessage) -> String {
    let text = format!("{msg:?}");
    text.split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("message")
        .to_string()
}

/// How long a voice frame is, for a pacer.
pub const VOICE_FRAME: Duration = Duration::from_millis(20);
