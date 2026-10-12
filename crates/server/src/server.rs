//! Chat server — admission, message routing, media relay.
//!
//! Wire types, the lane table, and serialization all come from the
//! shared `protocol` crate; the QUIC transport and identity pinning from
//! `transport`. This file owns policy: who may join (the shared
//! password, rate-limited), who may share and watch (limits), and when
//! to tell clients how their media is doing (receiver reports, stream
//! health).
//!
//! The server is sans-IO: [`ChatServer::update`] takes `now` and works
//! on the embedded [`Driver`]; the binary's loop and the test harness
//! move datagrams in and out around it.
//!
//! The tick never touches disk: everything that persists goes through
//! [`Persist`] (spec §0, rule 1) — fire-and-forget writes that drop and
//! count under backpressure, and reads polled per tick against a
//! deadline. Wall time comes from a [`Clock`] anchored to the tick's
//! `Instant`, so the simulator's fake clock advances it too.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use log::{debug, info, warn};
use protocol::{
    features,
    permissions::{self, RoleId, RANK_ADMIN, RANK_GUEST, ROLE_ADMIN},
    AdminCommand, Auth, ChannelDenyReason, ChannelInfo, ChatMessage, ClientId, ClientKeyProof,
    ClientMessage, DenyReason, InviteDenyReason, InviteInvalidReason, Lane, NeedInviteReason,
    RefusedReason, RemovedKind, RestrictionKind, ServerMessage, ShareScope, TelemetryEvent,
    TelemetrySample, UserId,
};
use transport::{
    identity::{verify_ed25519, CLIENT_AUTH_EXPORTER_BYTES, CLIENT_AUTH_EXPORTER_LABEL},
    Budget, DisconnectReason, Driver, DriverHost, Event, Fingerprint, LinkStats, ServerIdentity,
    Shared, WaitingRoom,
};

mod admin;
mod channels;
mod dms;
mod files;
mod history;
mod moderation;
mod profiles;
mod safety;

use crate::{
    clock::{Clock, WallMs},
    config::{channel_infos_of, validate_channel_list, ChannelSettings, Config},
    link_stats::VoiceWindow,
    persistence::{
        AskId, AuditRow, Command, MessageRow, Notice, Persist, ProgressDelta, TelemetryEventRow,
        TelemetryRow, GENERAL_CONV,
    },
    policy::{
        self, InviteRecord, KeyHash, KeyLookup, Policy, Restriction, BOOTSTRAP_INVITE_TTL_MS,
        DEFAULT_INVITE_TTL_MS,
    },
    relay::{ShareInfo, VideoRelay, VoiceRelay},
    server::{dms::PendingDm, profiles::AnnouncedUpload},
};

/// Announced share dimensions must fall in this range (per-chunk
/// dimensions are additionally validated by the shared header parser).
const MIN_SHARE_DIMENSION: u16 = 16;
const MAX_SHARE_DIMENSION: u16 = 4096;

/// How often to sample viewer health and report to sharers.
const STREAM_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// How often each client gets its voice receiver report (spec §6.1).
const VOICE_REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// How often restrictions are checked for expiry.
const RESTRICTION_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// How often idle members are moved to the AFK channel.
const CHANNEL_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// How often accrued progress is flushed (spec §9.1).
const PROGRESS_INTERVAL: Duration = Duration::from_mins(1);

/// The main loop's longest sleep with nothing due (review P1-2): a stop
/// request and the persistence thread's notices (a CLI change, a prune)
/// wait at most this long. Datagrams wake the loop at once.
const MAX_IDLE_WAIT: Duration = Duration::from_secs(1);
/// The main loop's longest sleep while a database read is out: its
/// answer comes on a channel the socket wait cannot see.
const ASK_POLL: Duration = Duration::from_millis(2);

/// How often to stat the config file for hot reload. A stat every few
/// seconds is free; anything file-watcher-fancier buys nothing here.
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// A connection that has not sent `Hello` this long after the
/// handshake is closed. The handshake itself already proved the peer
/// is a real client; on a link losing most of its packets the Hello
/// stream data needs several retransmissions on top (seen: handshake
/// up after 9 s, Hello still missing at +5 s). A pending slot for ten
/// seconds costs nothing.
const HELLO_DEADLINE: Duration = Duration::from_secs(10);

/// Failed admissions per source address in [`ADMISSION_FAILURE_WINDOW`]
/// before further attempts are refused outright.
const ADMISSION_FAILURE_LIMIT: usize = 5;

/// How many messages one admin answer may take: 64 of 16 KiB, a
/// megabyte on the control lane (200 reports at their longest take
/// 34). A listing past that is cut, and the log says by how much.
const ADMIN_ANSWER_PAGES: usize = 64;
const ADMISSION_FAILURE_WINDOW: Duration = Duration::from_mins(1);

/// Time given for a `Refused` message to reach the peer before the
/// connection is closed under it.
const REFUSED_CLOSE_DELAY: Duration = Duration::from_millis(300);

/// Uploads whose sender stalled this long are forgotten.
const UPLOAD_STALL: Duration = Duration::from_mins(2);

/// One log upload per client per this interval (spec §9.1).
const LOG_UPLOAD_MIN_INTERVAL: Duration = Duration::from_mins(10);

/// Telemetry samples are coalesced per user on the tick and handed to
/// the persistence thread in one batch this often (spec §9).
const TELEMETRY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

// ── Stats ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct ServerStats {
    messages_count: u64,
    bytes_sent: u64,
    bytes_received: u64,
    admitted: u64,
    refused: u64,
}

// ── ChatServer ──────────────────────────────────────────────────────────

/// One client's in-flight chunked upload (image or log).
struct IncomingUpload {
    upload_id: u32,
    count: u32,
    received: u32,
    bytes: usize,
    chunks: Vec<Option<Vec<u8>>>,
    started: Instant,
}

/// A connection past the handshake, waiting for its `Hello`.
struct PendingPeer {
    since: Instant,
    remote: SocketAddr,
}

/// What a read from the store was for, kept until its answer comes
/// back or its deadline passes. One table, keyed by the ask: there were
/// seven maps, searched one after another for whose answer it was — and
/// an answer none of them knew fell through to the last, as a history
/// page (review P2-52).
enum Waiting {
    /// A history page: who asked, for what, how much.
    History {
        client: ClientId,
        conv: protocol::ConvRef,
        limit: u16,
    },
    /// A DM being stored before anyone sees it (spec §6).
    Dm(Box<PendingDm>),
    /// The DM list of a session.
    Conversations(ClientId),
    /// An audit page for the panel, with the command's request id.
    Audit { client: ClientId, request_id: u32 },
    /// A report waiting for the store to find its message.
    Report(safety::PendingReport),
    /// The Reports tab, with the command's request id.
    ReportList { client: ClientId, request_id: u32 },
    /// A file's bytes from disk; who wants them is in `pending_blobs`.
    Blob(protocol::BlobId),
}

pub struct ChatServer {
    config: Config,
    driver: Driver,
    fingerprint: Fingerprint,
    /// The clock of the current tick; every timestamp in this file
    /// comes from here so the simulation harness controls it.
    now: Instant,
    /// When this process started (the panel's uptime).
    started: Instant,
    /// Wall time, derived from `now` (spec §10).
    clock: Clock,
    /// The persistence thread's handle (spec §1). Never blocks.
    persist: Persist,
    /// Users, keys, roles, invites, restrictions — what admission and
    /// every permission check consult (spec §0 rule 4).
    policy: Policy,
    /// The one-shot admin code printed at startup when nobody is admin
    /// yet (spec §1.1).
    bootstrap_invite: Option<String>,
    /// The user each admitted session is logged in as.
    user_of: HashMap<ClientId, UserId>,
    /// The device key each admitted session came in with.
    key_of: HashMap<ClientId, KeyHash>,
    /// Telemetry samples waiting for the next flush, one per user — a
    /// newer sample replaces an older one (last wins, never a backlog).
    pending_telemetry: HashMap<UserId, TelemetryRow>,
    last_telemetry_flush: Instant,
    pending: HashMap<ClientId, PendingPeer>,
    /// Recent failed admissions per source ([`failure_source`]).
    admission_failures: HashMap<IpAddr, VecDeque<Instant>>,
    usernames: HashMap<ClientId, String>,
    /// The keys of `usernames` — every admitted session — kept as a
    /// list: the relays and every broadcast walk it, and it was built
    /// again from the map on each tick (review P2-31).
    roster: Vec<ClientId>,
    /// Negotiated feature bits per admitted client.
    features_of: HashMap<ClientId, u64>,
    /// Latest self-reported audio state: client → (muted, deafened).
    audio_states: HashMap<ClientId, (bool, bool)>,
    /// Clients currently deafened (voice relay skips them).
    deafened: HashSet<ClientId>,
    /// Sessions the relay drops voice from (spec §4.2): a `voice`
    /// restriction, or a role set without `SPEAK`. Recomputed from the
    /// policy whenever it changes and once a second for expiries.
    muted: HashSet<ClientId>,
    /// Last expiry sweep of the restrictions.
    last_restriction_sweep: Instant,
    /// The newest general-chat messages (spec §6): replayed on join,
    /// and the set an author may still edit or delete. Primed from the
    /// database at startup.
    recent: VecDeque<MessageRow>,
    /// Chat rate limit (`limits.chat`): tokens left and when they were
    /// last topped up, per session.
    chat_tokens: HashMap<ClientId, (f64, Instant)>,
    /// Reads from the store in flight, each with what it was for.
    waiting: HashMap<AskId, Waiting>,
    /// Progress accrued this minute (spec §9.1), flushed once a minute.
    progress: HashMap<UserId, ProgressDelta>,
    /// Sessions that spoke since the last progress minute.
    spoke: HashSet<ClientId>,
    last_progress_minute: Instant,
    /// Last activity per session (chat, speaking, joining, sharing),
    /// for auto-AFK (spec §5).
    last_activity: HashMap<ClientId, Instant>,
    last_channel_sweep: Instant,
    /// The last telemetry line per user, for the admin tooltip.
    last_telemetry: HashMap<UserId, (WallMs, String)>,
    /// The channel list clients see: config channels, then UI-created
    /// ones. `channel_infos` is its wire form.
    channel_list: Vec<ChannelSettings>,
    /// Voice-channel membership: client → channel id (index into
    /// `channel_infos`). Every admitted client has an entry.
    channel_of: HashMap<ClientId, u16>,
    /// The channel list announced to clients, built once from config.
    channel_infos: Vec<ChannelInfo>,
    /// Where new clients land (config `default: true`).
    default_channel: u16,
    /// Config file to watch for hot reload (None = watching disabled,
    /// e.g. in tests that drive `apply_channel_config` directly).
    config_path: Option<PathBuf>,
    /// Last observed mtime of `config_path`.
    config_mtime: Option<SystemTime>,
    last_config_check: Instant,
    /// Per-client (time, bytes) of recently *accepted* chat images,
    /// pruned to the last minute — the sliding window behind
    /// `limits.max_transfer_kb_per_minute`.
    image_bytes: HashMap<ClientId, Vec<(Instant, usize)>>,
    /// In-progress image uploads, at most one per client — a chunk
    /// with a fresh `upload_id` abandons the old partial.
    uploads: HashMap<ClientId, IncomingUpload>,
    /// Blob uploads in flight (spec §7), announced first.
    blob_uploads: HashMap<ClientId, IncomingUpload>,
    announced_uploads: HashMap<ClientId, AnnouncedUpload>,
    /// Files someone other than their first uploader sent in full this
    /// run: proof they hold the bytes, so they may post them too
    /// (review P0-4). Known by hash alone is never enough.
    proven_uploads: HashSet<(UserId, protocol::BlobId)>,
    /// Each user's upload volume today (spec §7's daily allowance).
    uploads_today: HashMap<UserId, files::DailyUpload>,
    /// Blob bytes by hash: the only copy without persistence, a cache
    /// of the disk with it (bounded, least recently used out first:
    /// `cache_blob`).
    blob_bytes: HashMap<protocol::BlobId, std::sync::Arc<Vec<u8>>>,
    /// When each cached blob was last stored or served, on
    /// `blob_clock`.
    blob_used: HashMap<protocol::BlobId, u64>,
    blob_clock: u64,
    /// Who waits for which file's bytes: one disk read
    /// ([`Waiting::Blob`]) serves them all.
    pending_blobs: HashMap<protocol::BlobId, Vec<ClientId>>,
    /// Profile and rename rate limits (spec §7.1).
    profile_changed_at: HashMap<UserId, Instant>,
    renamed_at: HashMap<ClientId, Instant>,
    /// (sender, receiver) sessions the relay keeps voice from: the
    /// receiver's user blocked the sender's (`protocol::safety`).
    /// Recomputed with `muted`.
    blocked_pairs: HashSet<(ClientId, ClientId)>,
    /// When each account last filed reports (rate limit).
    report_times: HashMap<UserId, VecDeque<Instant>>,
    /// When each account last changed its blocks, for their limit.
    block_times: HashMap<UserId, VecDeque<Instant>>,
    /// In-progress log uploads, same shape.
    log_uploads: HashMap<ClientId, IncomingUpload>,
    /// When each client last completed a log upload.
    last_log_upload: HashMap<ClientId, Instant>,
    /// Server-wide window of accepted image bytes — the aggregate
    /// budget the per-client one can't provide.
    image_bytes_global: Vec<(Instant, usize)>,
    /// Server-assigned chat message ids, unique for this run.
    next_message_id: u64,
    /// Per-client transport counters at the client's last `link:`
    /// line: the line covers everything since, not the last 2 s.
    link_baseline: HashMap<ClientId, LinkStats>,
    /// Per-client voice counts since that line, summed over the 2 s
    /// report windows in between.
    link_voice: HashMap<ClientId, VoiceWindow>,
    last_stats: Instant,
    last_health: Instant,
    last_voice_report: Instant,
    /// Last per-client `link:` log line (see `logging.link_stats_interval_secs`).
    last_link_log: Instant,
    stats: ServerStats,
    voice: VoiceRelay,
    video: VideoRelay,
}

impl DriverHost for ChatServer {
    fn driver(&mut self) -> &mut Driver {
        &mut self.driver
    }
}

impl ChatServer {
    /// Build the server around `identity`. Binds nothing: the caller
    /// owns the socket (or the simulated network) and pumps
    /// [`DriverHost::driver`] around [`Self::update`].
    ///
    /// # Errors
    /// Invalid channel, authentication or policy configuration, a
    /// database that cannot be opened or migrated, or a TLS setup
    /// failure.
    pub fn new(
        config: Config,
        identity: &ServerIdentity,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        config
            .validate_policy()
            .map_err(|e| format!("invalid server.yaml: {e}"))?;
        if config.authentication.secure || config.authentication.password.is_some() {
            warn!(
                "server.yaml has an `authentication:` section; since 1.1 it is ignored — \
                 invites are the only door (see the bootstrap invite below)"
            );
        }

        // ── Persistence and policy ──────────────────────────────────
        // Opened (and migrated) here, before the socket binds: a broken
        // database fails startup, never a running server.
        let (persist, startup) = Persist::spawn(&config.persistence)?;
        let mut policy = startup.policy;
        let now = Instant::now();
        let clock = Clock::system(now);
        let bootstrap_invite = mint_bootstrap_invite(
            &mut policy,
            clock.wall_ms(now),
            &config.public_endpoint(),
            &identity.fingerprint(),
        );

        if config.server.port == 5000 {
            warn!(
                "server.port is 5000: some ISP gateways police UDP/5000 (users hear each \
                 other at 16 kbps). The default is 51000 since 1.1.5; publish both ports \
                 while clients migrate (README, \"Running the server\")"
            );
        }

        // ── Voice channels ──────────────────────────────────────────
        let default_channel = config
            .validate_channels()
            .map_err(|e| format!("invalid server.yaml: {e}"))?;
        let channel_infos = config.channel_infos();
        info!(
            "Voice channels: {} (plus the lobby everyone connects into)",
            channel_infos
                .iter()
                .filter(|c| c.name != crate::config::LOBBY_NAME)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );

        // ── Admission ───────────────────────────────────────────────
        info!(
            "Admission: device keys log in, invites are the door; {} known users, {} invites",
            policy.users.len(),
            policy.invites.len()
        );

        // ── Transport ───────────────────────────────────────────────
        let mut budget = Budget::server();
        budget.media_pace_bytes_per_sec = config.limits.media_relay_kb_per_sec.max(64) * 1024;
        budget.socket_buffer_bytes = send_buffer_target(config.network.max_clients);
        let driver = Driver::new_server(identity, config.network.max_clients, budget)?
            .with_waiting_room(waiting_room(config.network.max_clients));

        let config_channels_for_list = crate::config::with_lobby(&config.channels);
        let mut server = Self {
            config,
            driver,
            fingerprint: identity.fingerprint(),
            now,
            started: now,
            clock,
            persist,
            policy,
            bootstrap_invite,
            user_of: HashMap::new(),
            key_of: HashMap::new(),
            pending_telemetry: HashMap::new(),
            last_telemetry_flush: now,
            pending: HashMap::new(),
            admission_failures: HashMap::new(),
            usernames: HashMap::new(),
            roster: Vec::new(),
            features_of: HashMap::new(),
            audio_states: HashMap::new(),
            deafened: HashSet::new(),
            muted: HashSet::new(),
            last_restriction_sweep: now,
            recent: startup.recent.into_iter().collect(),
            chat_tokens: HashMap::new(),
            waiting: HashMap::new(),
            progress: HashMap::new(),
            spoke: HashSet::new(),
            last_progress_minute: now,
            last_activity: HashMap::new(),
            last_channel_sweep: now,
            last_telemetry: HashMap::new(),
            channel_list: config_channels_for_list,
            channel_of: HashMap::new(),
            channel_infos,
            default_channel,
            config_path: None,
            config_mtime: None,
            last_config_check: now,
            image_bytes: HashMap::new(),
            uploads: HashMap::new(),
            blob_uploads: HashMap::new(),
            announced_uploads: HashMap::new(),
            proven_uploads: HashSet::new(),
            uploads_today: HashMap::new(),
            blob_bytes: HashMap::new(),
            blob_used: HashMap::new(),
            blob_clock: 0,
            pending_blobs: HashMap::new(),
            profile_changed_at: HashMap::new(),
            renamed_at: HashMap::new(),
            blocked_pairs: HashSet::new(),
            report_times: HashMap::new(),
            block_times: HashMap::new(),
            log_uploads: HashMap::new(),
            last_log_upload: HashMap::new(),
            image_bytes_global: Vec::new(),
            next_message_id: startup.next_message_id,
            link_baseline: HashMap::new(),
            link_voice: HashMap::new(),
            last_stats: now,
            last_health: now,
            last_voice_report: now,
            last_link_log: now,
            stats: ServerStats::default(),
            voice: VoiceRelay::new(),
            video: VideoRelay::new(),
        };
        if let Some(code) = server.bootstrap_invite.clone()
            && let Some(invite) = server.policy.invites.get(&code).cloned()
        {
            server.persist.send(Command::InsertInvite { code, invite });
        }
        // UI-created channels from the store join the list.
        server.rebuild_channel_list()?;
        if !server.policy.channels.is_empty() {
            info!(
                "UI channels: {}",
                server
                    .policy
                    .channels
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(server)
    }

    /// The identity clients pin.
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The transport budget in force (the binary sizes the socket
    /// buffers from it).
    #[must_use]
    pub fn budget(&self) -> &Budget {
        self.driver.budget()
    }

    /// The persistence handle — for the stats line and for tests that
    /// stage a stalled disk.
    #[must_use]
    pub fn persistence(&self) -> &Persist {
        &self.persist
    }

    /// The in-memory policy (users, keys, roles, invites, restrictions).
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The one-shot admin code printed at startup, while it exists.
    #[must_use]
    pub fn bootstrap_invite(&self) -> Option<&str> {
        self.bootstrap_invite.as_deref()
    }

    /// The user a session is logged in as.
    #[must_use]
    pub fn user_of(&self, client: ClientId) -> Option<UserId> {
        self.user_of.get(&client).copied()
    }

    /// The voice channel a session is in.
    #[must_use]
    pub fn channel_of(&self, client: ClientId) -> Option<u16> {
        self.channel_of.get(&client).copied()
    }

    /// How many sessions are sharing their screen right now.
    #[must_use]
    pub fn video_sharer_count(&self) -> usize {
        self.video.sharer_count()
    }

    /// Mint an invite (spec §3.1). One implementation behind the
    /// bootstrap code, `ClientMessage::CreateInvite`, and the tests.
    /// The caller has already decided the actor may grant `role`.
    ///
    /// # Errors
    /// `role` does not exist, or is the admin role (admin codes come
    /// from the CLI only).
    pub fn mint_invite(
        &mut self,
        created_by: Option<UserId>,
        role: RoleId,
        ttl_ms: Option<u64>,
        max_uses: Option<u32>,
    ) -> Result<(String, InviteRecord), InviteDenyReason> {
        let rank = self
            .policy
            .roles
            .get(&role)
            .map(|r| r.rank)
            .ok_or(InviteDenyReason::RoleNotAllowed)?;
        if rank >= RANK_ADMIN {
            return Err(InviteDenyReason::RoleNotAllowed);
        }
        let now = self.wall_now();
        let code = policy::generate_invite_code();
        let invite = InviteRecord {
            created_by,
            created_at: now,
            expires_at: ttl_ms.map(|ttl| now.saturating_add(ttl)),
            max_uses,
            uses: 0,
            grants_role: role,
            revoked_at: None,
        };
        self.policy.add_invite(code.clone(), invite.clone());
        self.persist.send(Command::InsertInvite {
            code: code.clone(),
            invite: invite.clone(),
        });
        let uses = max_uses.map_or_else(|| "unlimited uses".to_string(), |n| format!("{n} uses"));
        let ttl = ttl_ms.map_or_else(
            || "never expires".to_string(),
            |ms| {
                format!(
                    "expires in {}",
                    crate::server::files::human_duration(ms / 1000)
                )
            },
        );
        self.audit(
            created_by,
            "invite.create",
            Some(code.clone()),
            Some(format!(
                "as {}, {uses}, {ttl}",
                self.role_name_for_audit(role)
            )),
        );
        Ok((code, invite))
    }

    /// Add a restriction (the ban family, spec §4.1–§4.2): in memory
    /// first, then the row. A `server` restriction on a live session
    /// removes it at once. Returns the stored restriction (with id).
    pub fn restrict(&mut self, restriction: Restriction, by_name: &str) -> Restriction {
        let stored = self.policy.add_restriction(restriction);
        self.persist
            .send(Command::InsertRestriction(stored.clone()));
        self.audit(
            stored.by,
            "restrict",
            stored
                .user_id
                .map(|u| hex(&u[..8]))
                .or_else(|| stored.key_hash.map(|k| hex(&k[..8]))),
            Some(format!(
                "{} {}{}",
                stored.kind.name(),
                stored.until.map_or_else(
                    || "indefinitely".to_string(),
                    |until| format!("until {}", wall_stamp(until))
                ),
                if stored.reason.is_empty() {
                    String::new()
                } else {
                    format!(": {}", stored.reason)
                }
            )),
        );
        if stored.kind == RestrictionKind::Server {
            // An address or key match can reach people the actor never
            // named: never the actor, never anyone at or above their
            // rank (the rank rule, spec §8, applied to the fallout).
            // The person named goes whatever their rank: the command
            // decided who may be named (`check_not_admin`: an equal or
            // a higher rank short of an administrator), and admission
            // refuses them from here on. Skipped with the fallout, a
            // banned equal stayed in the room until they next
            // reconnected.
            let actor_rank = stored.by.map(|a| self.policy.rank_of(&a));
            let hit: Vec<ClientId> = self
                .user_of
                .iter()
                .filter(|(client, user)| {
                    if stored.by.as_ref() == Some(*user) {
                        return false;
                    }
                    if stored.user_id.as_ref() == Some(*user) {
                        return true;
                    }
                    if actor_rank.is_some_and(|rank| self.policy.rank_of(user) >= rank) {
                        return false;
                    }
                    let key = self.key_of.get(*client);
                    let ip = self.driver.remote_address(**client).map(|a| a.ip());
                    (key.is_some() && stored.key_hash.as_ref() == key)
                        || (ip.is_some() && stored.ip == ip)
                })
                .map(|(client, _)| *client)
                .collect();
            // A `server` restriction is a ban (a kick is not a
            // restriction at all; it forgets the keys instead).
            let kind = RemovedKind::Banned {
                until: stored.until,
            };
            for client in hit {
                self.remove_session(client, kind.clone(), &stored.reason, by_name);
            }
        } else if let Some(user) = stored.user_id {
            let msg = ServerMessage::Restricted {
                kind: stored.kind,
                until: stored.until,
                reason: stored.reason.clone(),
            };
            for client in self.sessions_of(&user) {
                self.send_msg(client, &msg);
            }
        }
        self.refresh_enforcement();
        stored
    }

    /// Stop a code from admitting anyone else (spec §3.1). Nobody who
    /// already used it is affected.
    pub fn revoke_invite(&mut self, code: &str, by: Option<UserId>) -> bool {
        let now = self.wall_now();
        if !self.policy.revoke_invite(code, now) {
            return false;
        }
        self.persist.send(Command::RevokeInvite {
            code: code.to_string(),
            ts: now,
        });
        self.audit(by, "invite.revoke", Some(code.to_string()), None);
        true
    }

    /// Unlink a device key from its user (spec §2.1). The hash stays
    /// taken: that device is refused with `KeyRetired` until re-linked.
    /// A session on that key is disconnected.
    pub fn unlink_key(&mut self, key_hash: &KeyHash) -> bool {
        let now = self.wall_now();
        let Some(key) = self.policy.keys.get_mut(key_hash) else {
            return false;
        };
        if key.retired_at.is_some() {
            return false;
        }
        key.retired_at = Some(now);
        self.persist_key(key_hash);
        self.audit(None, "key.unlink", Some(hex(&key_hash[..8])), None);
        let live: Vec<ClientId> = self
            .key_of
            .iter()
            .filter(|(_, k)| *k == key_hash)
            .map(|(c, _)| *c)
            .collect();
        for client in live {
            self.remove_session(
                client,
                RemovedKind::Disconnected,
                "this device was unlinked from your user",
                "server",
            );
        }
        true
    }

    /// Lift a restriction by id. A live session it applied to is told.
    pub fn lift_restriction(&mut self, id: u64, by: Option<UserId>) -> bool {
        let Some(gone) = self
            .policy
            .restrictions
            .iter()
            .find(|r| r.id == id)
            .cloned()
        else {
            return false;
        };
        self.policy.remove_restriction(id);
        self.persist.send(Command::DeleteRestriction { id });
        self.audit(by, "restriction.lift", Some(id.to_string()), None);
        if let Some(user) = gone.user_id
            && gone.kind != RestrictionKind::Server
        {
            let msg = ServerMessage::RestrictionLifted { kind: gone.kind };
            for client in self.sessions_of(&user) {
                self.send_msg(client, &msg);
            }
        }
        self.refresh_enforcement();
        true
    }

    /// Wall time of the current tick.
    fn wall_now(&self) -> WallMs {
        self.clock.wall_ms(self.now)
    }

    fn audit(
        &mut self,
        actor: Option<UserId>,
        action: &str,
        target: Option<String>,
        detail: Option<String>,
    ) {
        let ts = self.wall_now();
        self.persist.send(Command::Audit(AuditRow {
            ts,
            actor,
            action: action.to_string(),
            target: target.clone(),
            detail: detail.clone(),
        }));
        // The admin channel sees it as it happens (spec §8).
        let line = ServerMessage::AuditLine {
            entry: protocol::AuditEntry {
                id: 0,
                ts,
                actor: actor.map(|a| self.name_of_user(&a)),
                target: target.as_deref().map(|t| self.audit_target_name(action, t)),
                action: action.to_string(),
                detail,
            },
        };
        let readers: Vec<ClientId> = self
            .user_of
            .iter()
            .filter(|(_, u)| {
                permissions::has(self.effective_permissions(u), permissions::ADMIN_CHANNEL)
            })
            .map(|(c, _)| *c)
            .collect();
        for peer in readers {
            self.send_msg(peer, &line);
        }
    }

    /// Mutable access to the persistence handle, for this file's tests.
    #[cfg(test)]
    fn persistence_mut(&mut self) -> &mut Persist {
        &mut self.persist
    }

    /// Replace the wall clock. Tests pin it so timestamps, invite
    /// expiry and restrictions are deterministic on the fake clock.
    pub fn set_clock(&mut self, clock: Clock) {
        self.clock = clock;
    }

    /// The wall clock in force.
    #[must_use]
    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Drive one tick: transport timers, admission, control messages,
    /// media relay, periodic reports.
    pub fn update(&mut self, now: Instant) {
        self.now = now;
        self.driver.handle_timeouts(now);

        self.process_events();
        // Who has anything to read: the only peers asked, for control
        // messages here and for media below. Every client used to be
        // asked for every lane on every tick (review P2-31).
        let readable = self.driver.readable();
        self.process_control_messages(&readable);
        self.expire_pending();
        self.poll_persistence();
        if now.duration_since(self.last_restriction_sweep) >= RESTRICTION_SWEEP_INTERVAL {
            self.last_restriction_sweep = now;
            self.sweep_restrictions();
            self.prune_admission_failures();
        }
        if now.duration_since(self.last_progress_minute) >= PROGRESS_INTERVAL {
            self.last_progress_minute = now;
            self.flush_progress();
        }
        if now.duration_since(self.last_channel_sweep) >= CHANNEL_SWEEP_INTERVAL {
            self.last_channel_sweep = now;
            self.sweep_channels();
        }

        let senders: Vec<ClientId> = readable
            .into_iter()
            .filter(|peer| self.usernames.contains_key(peer))
            .collect();
        let afk = self.afk_channel();
        self.voice.process(
            &mut self.driver,
            now,
            &crate::relay::Room {
                clients: &self.roster,
                senders: &senders,
                deafened: &self.deafened,
                muted: &self.muted,
                blocked: &self.blocked_pairs,
                afk_channel: afk,
                lobby: self.default_channel,
                channel_of: &self.channel_of,
            },
        );
        self.video.process(&mut self.driver, now, &senders);
        self.video
            .process_system_audio(&mut self.driver, now, &senders);

        if now.duration_since(self.last_health) >= STREAM_HEALTH_INTERVAL {
            self.send_stream_health();
            self.last_health = now;
        }

        if now.duration_since(self.last_voice_report) >= VOICE_REPORT_INTERVAL {
            self.send_voice_reports();
            self.last_voice_report = now;
        }

        if now.duration_since(self.last_telemetry_flush) >= TELEMETRY_FLUSH_INTERVAL {
            self.flush_telemetry();
            self.last_telemetry_flush = now;
        }

        if now.duration_since(self.last_config_check) >= CONFIG_CHECK_INTERVAL {
            self.check_config_reload();
            // Same lazy cadence: drop uploads whose sender stalled
            // mid-pace for two minutes (crashed, gave up, vanished).
            self.uploads
                .retain(|_, u| now.duration_since(u.started) < UPLOAD_STALL);
            self.log_uploads
                .retain(|_, u| now.duration_since(u.started) < UPLOAD_STALL);
            self.last_config_check = now;
        }

        if now.duration_since(self.last_stats)
            >= Duration::from_secs(self.config.logging.stats_interval_secs)
        {
            self.print_stats();
            self.last_stats = now;
        }
    }

    /// When the next tick is due if no datagram comes first (review
    /// P1-2): the transport's next deadline, a Hello's deadline, the
    /// periodic jobs of [`Self::update`], at most [`MAX_IDLE_WAIT`] away
    /// — [`ASK_POLL`] while a database read is out. The main loop waits
    /// on its socket until then, so a datagram is relayed the moment it
    /// lands instead of after a 1, 5 or 20 ms sleep, and an idle server
    /// wakes a few times a second instead of 50 to 1,000.
    #[must_use]
    pub fn next_wake(&mut self) -> Instant {
        let cap = if self.persist.reads_in_flight() > 0 || self.persist.reload_in_flight() {
            ASK_POLL
        } else {
            MAX_IDLE_WAIT
        };
        let mut due = [
            self.now + cap,
            self.last_restriction_sweep + RESTRICTION_SWEEP_INTERVAL,
            self.last_progress_minute + PROGRESS_INTERVAL,
            self.last_channel_sweep + CHANNEL_SWEEP_INTERVAL,
            self.last_health + STREAM_HEALTH_INTERVAL,
            self.last_voice_report + VOICE_REPORT_INTERVAL,
            self.last_telemetry_flush + TELEMETRY_FLUSH_INTERVAL,
            self.last_config_check + CONFIG_CHECK_INTERVAL,
        ]
        .into_iter()
        .min()
        .unwrap_or(self.now);
        // A zero interval prints on every tick; it is no deadline.
        let stats = Duration::from_secs(self.config.logging.stats_interval_secs);
        if !stats.is_zero() {
            due = due.min(self.last_stats + stats);
        }
        if let Some(hello) = self
            .pending
            .values()
            .map(|p| p.since + HELLO_DEADLINE)
            .min()
        {
            due = due.min(hello);
        }
        if let Some(deadline) = self.driver.next_deadline() {
            due = due.min(deadline);
        }
        due
    }

    /// Close every connection with `reason` (a deploy or operator
    /// stop). Clients treat "server restarting" as transient and come
    /// back on their own in a couple of seconds, instead of discovering
    /// the restart through the idle timeout. The caller pumps the
    /// transport so the close packets leave, and only then calls
    /// [`Self::finish_shutdown`]: the database drain can take seconds,
    /// and with the close frames queued behind it a slow drain outlasted
    /// Docker's stop timeout, the process was killed, and every client
    /// sat out its 30 s idle timeout (review P2-5).
    pub fn begin_shutdown(&mut self, now: Instant, reason: &str) {
        info!(
            "Shutting down: closing {} connections ({reason})",
            self.driver.peer_count()
        );
        for peer in self.driver.peers() {
            self.driver.close(now, peer, reason);
        }
        self.flush_telemetry();
    }

    /// Have the persistence thread write what is queued and stop
    /// (bounded). The last step of a shutdown.
    pub fn finish_shutdown(&mut self) {
        self.persist.shutdown();
    }

    /// Both steps at once, for a caller with no transport to pump.
    pub fn shutdown(&mut self, now: Instant, reason: &str) {
        self.begin_shutdown(now, reason);
        self.finish_shutdown();
    }

    /// Admitted clients, in no particular order.
    #[must_use]
    pub fn clients(&self) -> Vec<ClientId> {
        self.roster.clone()
    }

    // ── Transport events ────────────────────────────────────────────

    fn process_events(&mut self) {
        let events: Vec<Event> = self.driver.drain_events().collect();
        for event in events {
            match event {
                Event::Connected { peer, remote, .. } => {
                    debug!("connection {peer:#x} from {remote} — waiting for Hello");
                    self.pending.insert(
                        peer,
                        PendingPeer {
                            since: self.now,
                            remote,
                        },
                    );
                }
                Event::Disconnected { peer, reason } => {
                    if self.pending.remove(&peer).is_some() {
                        debug!("connection {peer:#x} left before Hello: {reason}");
                    } else if self.usernames.contains_key(&peer) {
                        self.handle_client_disconnected(peer, &reason);
                    }
                }
                Event::DatagramsBlocked { peer } => {
                    debug!("datagram send buffer full toward {peer:#x}; dropping oldest");
                }
            }
        }
    }

    /// Close connections that never said Hello.
    fn expire_pending(&mut self) {
        let now = self.now;
        let late: Vec<ClientId> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.since) > HELLO_DEADLINE)
            .map(|(id, _)| *id)
            .collect();
        for peer in late {
            if let Some(p) = self.pending.remove(&peer) {
                info!(
                    "connection {peer:#x} from {} sent no Hello; closing",
                    p.remote
                );
            }
            self.driver.close(now, peer, "no hello");
        }
    }

    // ── Admission ───────────────────────────────────────────────────

    /// Handle the first Control frame of a pending connection: the
    /// admission decision (spec §3.2–§3.3). In order: the failure rate
    /// limit; the 1.0 password variant (refused as `BadHello`, which a
    /// 1.0 client renders as "update baucord"); a valid key proof;
    /// bans; who the key is (a known key logs in, a first visit needs
    /// a code); the name rules; the room cap; then the user's own older
    /// session, if any, yields to this one.
    #[allow(clippy::too_many_lines)]
    fn handle_hello(&mut self, peer: ClientId, raw: &[u8]) {
        let Some(pending) = self.pending.remove(&peer) else {
            return;
        };
        let remote = pending.remote;
        let ip = remote.ip();
        self.stats.bytes_received += raw.len() as u64;

        let Some(ClientMessage::Hello {
            username,
            auth,
            client_key,
            features: client_features,
            client_version,
        }) = protocol::decode_client_message(raw)
        else {
            warn!("connection {peer:#x} from {remote}: first message was not Hello");
            self.refuse(peer, ip, RefusedReason::BadHello, false);
            return;
        };

        if self.recent_failures(ip) >= ADMISSION_FAILURE_LIMIT {
            warn!("connection {peer:#x} from {ip}: too many failed admissions; refusing");
            self.refuse(peer, ip, RefusedReason::RateLimited, false);
            return;
        }

        let invite_code = match auth {
            Auth::Shared { .. } => {
                info!(
                    "connection {peer:#x} from {remote}: 1.0 client ({client_version}) sent the \
                     password variant; refused — it will show \"update baucord\""
                );
                self.refuse(peer, ip, RefusedReason::BadHello, false);
                return;
            }
            Auth::Key => None,
            Auth::Invite { code } => Some(code),
        };

        // The device key is the identity (spec §3): no proof, no admission.
        let Some(key_hash) = client_key
            .as_ref()
            .and_then(|proof| self.verify_client_key(peer, proof))
        else {
            warn!("connection {peer:#x} from {remote}: no valid client key proof");
            self.refuse(peer, ip, RefusedReason::BadHello, false);
            return;
        };

        let now = self.wall_now();
        let lookup = self.policy.resolve_key(&key_hash);
        let known_user = match lookup {
            KeyLookup::User(user) => Some(user),
            KeyLookup::Unknown | KeyLookup::Retired => None,
        };

        // Bans before anything else (spec §3.2): by user, key or address.
        if let Some(ban) = self
            .policy
            .server_ban(known_user.as_ref(), &key_hash, Some(ip), now)
        {
            let reason = RefusedReason::Banned {
                until: ban.until,
                reason: ban.reason.clone(),
            };
            info!(
                "connection {peer:#x} from {remote}: banned ({}), key {}",
                ban.reason,
                hex(&key_hash[..8])
            );
            self.refuse(peer, ip, reason, false);
            return;
        }

        let mut username = protocol::sanitize_username(&username);
        if username.is_empty() {
            username = format!("User{peer:x}");
        }

        // Who is this? A known key is the login — a code it sends anyway
        // is ignored and no use is burned. A first visit needs a code.
        let (user_id, admitted_with) = match lookup {
            KeyLookup::User(user_id) => (user_id, None),
            KeyLookup::Retired => {
                info!(
                    "connection {peer:#x} from {remote}: retired key {}; needs a re-link",
                    hex(&key_hash[..8])
                );
                self.refuse(
                    peer,
                    ip,
                    RefusedReason::NeedInvite {
                        why: NeedInviteReason::KeyRetired,
                    },
                    false,
                );
                return;
            }
            KeyLookup::Unknown => {
                let Some(raw_code) = invite_code else {
                    info!(
                        "connection {peer:#x} from {remote}: unknown key {} and no invite",
                        hex(&key_hash[..8])
                    );
                    self.refuse(
                        peer,
                        ip,
                        RefusedReason::NeedInvite {
                            why: NeedInviteReason::UnknownKey,
                        },
                        false,
                    );
                    return;
                };
                let checked = protocol::invite::normalize_code(&raw_code)
                    .ok_or(InviteInvalidReason::Unknown)
                    .and_then(|code| self.policy.check_invite(&code, now).map(|_| code));
                match checked {
                    Ok(code) => (key_hash, Some(code)),
                    Err(why) => {
                        warn!("connection {peer:#x} from {remote}: invite refused ({why:?})");
                        self.refuse(peer, ip, RefusedReason::InviteInvalid { why }, true);
                        return;
                    }
                }
            }
        };

        // This user's own live sessions (auto-reconnect after a blackout
        // makes this routine): they never count against the name or the
        // room cap, and they yield below.
        let own_sessions: Vec<ClientId> = self
            .user_of
            .iter()
            .filter(|(_, user)| **user == user_id)
            .map(|(client, _)| *client)
            .collect();

        // Names (spec §10): unique among the users online; and a guest
        // cannot take a member's name while that member is offline.
        let arriving_rank = match &admitted_with {
            Some(code) => self
                .policy
                .invites
                .get(code)
                .and_then(|i| self.policy.roles.get(&i.grants_role))
                .map_or(RANK_GUEST, |r| r.rank),
            None => self.policy.rank_of(&user_id),
        };
        let taken_online = self.usernames.iter().any(|(client, name)| {
            !own_sessions.contains(client) && name.eq_ignore_ascii_case(&username)
        });
        if taken_online
            || (arriving_rank == RANK_GUEST && self.policy.name_held_by_member(&username, &user_id))
        {
            info!("connection {peer:#x} from {remote}: name '{username}' is in use");
            self.refuse(peer, ip, RefusedReason::NameInUse, false);
            return;
        }

        if self.usernames.len().saturating_sub(own_sessions.len())
            >= self.config.network.max_clients
        {
            info!("connection {peer:#x} from {remote}: server full");
            self.refuse(peer, ip, RefusedReason::ServerFull, false);
            return;
        }

        for old in own_sessions {
            info!("{username} ({old:#x}) replaced by a newer connection ({peer:#x})");
            self.remove_session(
                old,
                RemovedKind::Replaced,
                "signed in from a newer connection",
                "server",
            );
        }

        // Create the user on a first visit; refresh the name of record
        // and last-seen otherwise. In memory first, then the rows.
        if let Some(code) = &admitted_with {
            let role = self
                .policy
                .invites
                .get(code)
                .map_or(permissions::ROLE_GUEST, |i| i.grants_role);
            self.policy
                .create_user(user_id, &username, now, Some(code.clone()));
            self.policy.link_key(key_hash, user_id, now);
            self.policy.grant_role(&user_id, role);
            self.policy.use_invite(code);
            // One event, one transaction: the user, the key, the role,
            // the invite's use and the audit line each took a commit of
            // their own, and a crash between two left a user without a
            // role or an invite used by nobody (review P3-9).
            self.persist.begin_group();
            self.persist_user(&user_id);
            self.persist_key(&key_hash);
            self.persist.send(Command::GrantRole {
                user_id,
                role_id: role,
                granted_by: self.policy.invites.get(code).and_then(|i| i.created_by),
                ts: now,
            });
            self.persist.send(Command::UseInvite { code: code.clone() });
            self.audit(
                Some(user_id),
                "user.join",
                Some(hex(&user_id[..8])),
                Some(format!(
                    "with invite {code} as {}",
                    self.role_name_for_audit(role)
                )),
            );
            self.persist.end_group();
            let role_name = self
                .policy
                .roles
                .get(&role)
                .map_or("?", |r| r.name.as_str());
            if self.bootstrap_invite.as_deref() == Some(code.as_str()) {
                self.bootstrap_invite = None;
                warn!("Bootstrap invite redeemed: {username} is now admin");
            }
            info!(
                "{username} ({peer:#x}) joined with invite {code} as {role_name}; user id {}",
                hex(&user_id[..8])
            );
        } else {
            self.policy.touch(&user_id, &key_hash, &username, now);
            self.persist_user(&user_id);
            self.persist_key(&key_hash);
        }

        let negotiated = features::negotiate(features::SERVER_1_1, client_features);
        self.features_of.insert(peer, negotiated);
        self.user_of.insert(peer, user_id);
        self.key_of.insert(peer, key_hash);
        // Out of the waiting room: its seat is the next arrival's.
        self.driver.admit(peer);
        self.stats.admitted += 1;
        info!(
            "{username} ({peer:#x}) admitted from {remote} (client {client_version}, features \
             {negotiated:#x}, user id {})",
            hex(&user_id[..8])
        );

        self.send_msg(
            peer,
            &ServerMessage::Welcome {
                client_id: peer,
                user_id: Some(user_id),
                features: features::SERVER_1_1,
            },
        );
        if negotiated & features::INVITES != 0 {
            self.send_msg(
                peer,
                &ServerMessage::Session {
                    user_id,
                    permissions: self.effective_permissions(&user_id),
                    persistent: self.persist.is_persistent(),
                },
            );
        }
        if negotiated & features::ROLE_LIST != 0 {
            let roles = self.role_list();
            self.send_msg(peer, &roles);
        }
        // Before any history: the client hides blocked people's
        // messages from the first replay on.
        self.send_blocks(peer);
        self.handle_client_connected(peer, username);
    }

    /// Verify a client identity proof against this connection's TLS
    /// exporter and return the key hash (spec §2.1).
    fn verify_client_key(&self, peer: ClientId, proof: &ClientKeyProof) -> Option<KeyHash> {
        let mut exported = [0u8; CLIENT_AUTH_EXPORTER_BYTES];
        self.driver
            .export_keying_material(peer, CLIENT_AUTH_EXPORTER_LABEL, b"", &mut exported)?;
        if verify_ed25519(&proof.public_key, &exported, &proof.signature) {
            Some(transport::sha256::digest(&proof.public_key))
        } else {
            warn!("connection {peer:#x} sent a client key proof that does not verify");
            None
        }
    }

    /// Queue the current user row for the store.
    fn persist_user(&mut self, user_id: &UserId) {
        if let Some(user) = self.policy.users.get(user_id).cloned() {
            self.persist.send(Command::UpsertUser {
                user_id: *user_id,
                user,
            });
        }
    }

    /// Queue the current key row for the store.
    fn persist_key(&mut self, key_hash: &KeyHash) {
        if let Some(key) = self.policy.keys.get(key_hash).cloned() {
            self.persist.send(Command::UpsertKey {
                key_hash: *key_hash,
                key,
            });
        }
    }

    /// End a session by the server's decision (spec §4.1): tell the
    /// client why, close 300 ms later so the message lands, and take
    /// the session out of the rosters now. The transport's later
    /// `Disconnected` event finds nothing left to do.
    fn remove_session(&mut self, client: ClientId, kind: RemovedKind, reason: &str, by: &str) {
        if !self.usernames.contains_key(&client) {
            return;
        }
        info!(
            "{} ({client:#x}) removed ({kind:?}) by {by}: {reason}",
            self.get_username(client)
        );
        self.send_msg(
            client,
            &ServerMessage::Removed {
                kind,
                reason: reason.to_string(),
                by: by.to_string(),
            },
        );
        self.driver
            .close_after(self.now, client, REFUSED_CLOSE_DELAY);
        self.handle_client_disconnected(client, &DisconnectReason::LocallyClosed);
    }

    fn recent_failures(&mut self, ip: IpAddr) -> usize {
        let now = self.now;
        let source = failure_source(ip);
        let Some(times) = self.admission_failures.get_mut(&source) else {
            return 0;
        };
        while times
            .front()
            .is_some_and(|t| now.duration_since(*t) > ADMISSION_FAILURE_WINDOW)
        {
            times.pop_front();
        }
        if times.is_empty() {
            self.admission_failures.remove(&source);
            return 0;
        }
        times.len()
    }

    /// Forget sources whose failures all aged out. An entry used to go
    /// only when the same address tried again, so every address that
    /// ever failed once stayed for the life of the process (review
    /// P2-7).
    fn prune_admission_failures(&mut self) {
        let now = self.now;
        self.admission_failures.retain(|_, times| {
            while times
                .front()
                .is_some_and(|t| now.duration_since(*t) > ADMISSION_FAILURE_WINDOW)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
    }

    /// Send `Refused`, then close once it had time to land.
    fn refuse(&mut self, peer: ClientId, ip: IpAddr, reason: RefusedReason, count: bool) {
        if count {
            self.admission_failures
                .entry(failure_source(ip))
                .or_default()
                .push_back(self.now);
        }
        self.stats.refused += 1;
        self.send_msg(peer, &ServerMessage::Refused { reason });
        self.driver.close_after(self.now, peer, REFUSED_CLOSE_DELAY);
    }

    // ── Message send helpers ────────────────────────────────────────

    fn send_msg(&mut self, client_id: ClientId, msg: &ServerMessage) {
        debug_assert!(
            !msg.is_upload_chunk(),
            "image chunks go through relay_image"
        );
        // A session gets only what it said it can decode. Checked here,
        // once, for every message: the table is the protocol's own
        // (`ServerMessage::required_feature`), where each caller used
        // to name a bit by hand and they did not all agree (review
        // P3-6). A connection still at its `Hello` has negotiated
        // nothing and gets the 1.0 messages only — `Refused`.
        let needs = msg.required_feature();
        if needs != 0 && !self.has_feature(client_id, needs) {
            return;
        }
        if let Some(encoded) = protocol::encode_server_message(msg) {
            self.stats.bytes_sent += encoded.len() as u64;
            if let Err(e) = self.driver.send(self.now, client_id, msg.lane(), &encoded) {
                debug!("send to {client_id:#x} on {} failed: {e}", msg.lane());
            }
        } else {
            // Too large for the wire (or unencodable): say so here — the
            // client would have dropped it without a word.
            warn!(
                "a {} message for {client_id:#x} exceeds {} bytes and was not sent",
                msg.lane(),
                protocol::MAX_CONTROL_MESSAGE_BYTES
            );
        }
    }

    /// An admin answer. A listing longer than one message goes out as
    /// several to a session that joins them (`ADMIN_PAGES`): the rows
    /// before the last page as `AdminRows`, then the `AdminResult`
    /// that closes the answer. Any other session gets the first page
    /// alone — a shipped client drops a larger message unread, so its
    /// panel came back empty (review P0-1).
    pub(super) fn send_admin_result(
        &mut self,
        client_id: ClientId,
        request_id: u32,
        result: Result<protocol::AdminOutcome, protocol::AdminError>,
    ) {
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                self.send_msg(
                    client_id,
                    &ServerMessage::AdminResult {
                        request_id,
                        result: Err(e),
                    },
                );
                return;
            }
        };
        let total = outcome.rows();
        let mut pages = outcome.into_pages(protocol::LIST_ITEMS_BUDGET);
        let limit = if self.has_feature(client_id, features::ADMIN_PAGES) {
            ADMIN_ANSWER_PAGES
        } else {
            1
        };
        pages.truncate(limit);
        let sent: usize = pages.iter().map(protocol::AdminOutcome::rows).sum();
        if sent < total {
            info!(
                "admin answer for {client_id:#x} cut to {} message(s) ({} rows left out)",
                pages.len(),
                total - sent
            );
        }
        let Some(last) = pages.pop() else {
            return;
        };
        for rows in pages {
            self.send_msg(client_id, &ServerMessage::AdminRows { request_id, rows });
        }
        self.send_msg(
            client_id,
            &ServerMessage::AdminResult {
                request_id,
                result: Ok(last),
            },
        );
    }

    /// One message to many sessions, encoded and framed once: the
    /// sends share the bytes. Each recipient used to get its own
    /// encoding and its own copy (review P2-32).
    fn send_to(&mut self, recipients: &[ClientId], msg: &ServerMessage) {
        debug_assert!(
            !msg.is_upload_chunk(),
            "image chunks go through relay_image"
        );
        let Some(encoded) = protocol::encode_server_message(msg) else {
            warn!(
                "a {} message for {} sessions exceeds {} bytes and was not sent",
                msg.lane(),
                recipients.len(),
                protocol::MAX_CONTROL_MESSAGE_BYTES
            );
            return;
        };
        let needs = msg.required_feature();
        let shared = Shared::new(msg.lane(), &encoded);
        for &client in recipients {
            if needs != 0 && !self.has_feature(client, needs) {
                continue;
            }
            self.stats.bytes_sent += encoded.len() as u64;
            if let Err(e) = self.driver.send_shared(self.now, client, &shared) {
                debug!("send to {client:#x} on {} failed: {e}", msg.lane());
            }
        }
    }

    fn broadcast_msg(&mut self, msg: &ServerMessage) {
        let everyone = std::mem::take(&mut self.roster);
        self.send_to(&everyone, msg);
        self.roster = everyone;
    }

    fn broadcast_msg_except(&mut self, except: ClientId, msg: &ServerMessage) {
        let others: Vec<ClientId> = self
            .roster
            .iter()
            .copied()
            .filter(|client| *client != except)
            .collect();
        self.send_to(&others, msg);
    }

    /// The next server-assigned chat message id and its wall-clock
    /// timestamp (ms since the Unix epoch). The id counter is rebased
    /// on the database's watermark at startup, so ids stay unique
    /// across restarts with persistence on.
    fn stamp(&mut self) -> (u64, u64) {
        let id = self.next_message_id;
        self.next_message_id += 1;
        (id, self.clock.wall_ms(self.now))
    }

    // ── Persistence ─────────────────────────────────────────────────

    /// Collect answers and notices from the persistence thread. Reads
    /// that outlived their deadline resolve here as timed out.
    fn poll_persistence(&mut self) {
        let polled = self.persist.poll(self.now);
        // Notices first: a reloaded policy was checked against the writes
        // queued up to this poll, and an answer handled below may queue
        // another (review P1-22).
        for notice in polled.notices {
            match notice {
                Notice::Policy { policy, .. } => {
                    info!(
                        "Policy reloaded from disk: {} users, {} invites, {} restrictions",
                        policy.users.len(),
                        policy.invites.len(),
                        policy.restrictions.len()
                    );
                    self.apply_reloaded_policy(*policy);
                }
                Notice::RefsReleased(released) => self.release_blob_refs(&released),
                Notice::Pruned {
                    history_cutoff,
                    released,
                    files,
                } => self.apply_prune(history_cutoff, &released, &files),
            }
        }
        for (id, outcome) in polled.answers {
            // An answer to a read nobody noted is nobody's: it used to
            // be taken for a history page.
            let Some(waiting) = self.waiting.remove(&id) else {
                continue;
            };
            match waiting {
                Waiting::History {
                    client,
                    conv,
                    limit,
                } => self.resolve_history_answer(client, conv, limit, outcome),
                Waiting::Dm(pending) => self.resolve_dm_answer(&pending, outcome),
                Waiting::Conversations(client) => {
                    self.resolve_conversations_answer(client, outcome);
                }
                Waiting::Audit { client, request_id } => {
                    self.resolve_audit_answer(client, request_id, outcome);
                }
                Waiting::Report(pending) => self.resolve_report_answer(pending, outcome),
                Waiting::ReportList { client, request_id } => {
                    self.resolve_report_list_answer(client, request_id, outcome);
                }
                Waiting::Blob(blob_id) => self.resolve_blob_answer(blob_id, outcome),
            }
        }
    }

    /// Hand the coalesced telemetry samples to the persistence thread.
    /// A full queue gives them back; they stay pending and a newer
    /// sample for the same user replaces them — last wins.
    fn flush_telemetry(&mut self) {
        if self.pending_telemetry.is_empty() {
            return;
        }
        let rows: Vec<TelemetryRow> = self.pending_telemetry.drain().map(|(_, r)| r).collect();
        if let Err(returned) = self.persist.send_telemetry(rows) {
            for row in returned {
                self.pending_telemetry.entry(row.user_id).or_insert(row);
            }
        }
    }

    // ── Uploads ─────────────────────────────────────────────────────

    /// Fold one upload chunk into the sender's in-flight image; on
    /// completion, validate (magic, size, bandwidth budgets) and hand
    /// the image to the paced relay. Bounds mirror the codec's, plus:
    /// one upload per client (a fresh `upload_id` abandons the old
    /// partial), a projected-size check on the claimed chunk count so
    /// an absurd upload dies on its first chunk, and a running byte
    /// cap so lying `count` values can't overshoot.
    fn handle_image_chunk(
        &mut self,
        client_id: ClientId,
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    ) {
        let Some(image) = fold_upload(
            &mut self.uploads,
            client_id,
            upload_id,
            index,
            count,
            data,
            protocol::MAX_IMAGE_CHUNKS,
            protocol::MAX_CHAT_IMAGE_BYTES,
            self.now,
        ) else {
            return;
        };
        let image = match image {
            Folded::Complete(image) => image,
            Folded::Malformed => {
                warn!(
                    "Dropping malformed image chunk from {} (count {count}, index {index})",
                    self.get_username(client_id),
                );
                return;
            }
            Folded::Oversized(bytes) => {
                warn!(
                    "Dropping oversized image upload from {} ({} KB and counting)",
                    self.get_username(client_id),
                    bytes / 1024,
                );
                return;
            }
        };

        // Relay only what claims to be a PNG, JPEG, or GIF — the
        // desktop encodes exactly these, so anything else is a
        // modified client wasting everyone's bandwidth. Receivers
        // sniff the same magic to pick a decoder. Checked *before*
        // the bandwidth budget so junk never consumes anyone's window.
        let is_png = image.starts_with(&PNG_MAGIC);
        let is_jpeg = image.starts_with(&[0xFF, 0xD8, 0xFF]);
        let is_gif = image.starts_with(b"GIF87a") || image.starts_with(b"GIF89a");
        if !is_png && !is_jpeg && !is_gif {
            warn!(
                "Dropping non-PNG/JPEG/GIF chat image ({} bytes) from {}",
                image.len(),
                self.get_username(client_id),
            );
            return;
        }
        if !self.allow_image(client_id, image.len()) {
            return;
        }
        info!(
            "[{}] sent an image ({} KB)",
            self.get_username(client_id),
            image.len() / 1024,
        );
        self.stats.messages_count += 1;
        self.relay_image(client_id, upload_id, &image);
    }

    /// Queue a validated image for relay: re-chunk, pre-encode each
    /// chunk *once*, and open one Media stream per receiver carrying
    /// all of them. The transport paces each stream at
    /// `limits.media_relay_kb_per_sec` so a big image never floods a
    /// receiver's downlink queue — voice shares that queue.
    fn relay_image(&mut self, client_id: ClientId, upload_id: u32, image: &[u8]) {
        let (id, ts) = self.stamp();
        let hash = transport::sha256::digest(image);
        #[allow(clippy::cast_possible_truncation)] // bounded by MAX_IMAGE_CHUNKS
        let count = image.len().div_ceil(protocol::IMAGE_CHUNK_BYTES) as u32;
        let mut encoded: Vec<protocol::Bytes> = Vec::with_capacity(count as usize);
        for (i, chunk) in image.chunks(protocol::IMAGE_CHUNK_BYTES).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let msg = ServerMessage::ChatImageChunk {
                client_id,
                upload_id,
                index: i as u32,
                count,
                id,
                ts,
                hash,
                data: chunk.to_vec(),
            };
            let Some(bytes) = protocol::encode_server_message(&msg) else {
                warn!("Image relay chunk failed to encode; dropping image");
                return;
            };
            encoded.push(protocol::Bytes::from(bytes));
        }
        // Not to whoever blocked the sender; and who sent it first,
        // if the audience was never told (a picture from the lobby).
        let audience = match self.user_of.get(&client_id).copied() {
            Some(author) => {
                let audience = self.audience_of(&author);
                self.introduce_author(client_id, &author, &audience);
                audience
            }
            None => self.clients(),
        };
        for peer in audience {
            self.stats.bytes_sent += image.len() as u64;
            if let Err(e) = self
                .driver
                .send_media(self.now, peer, encoded.iter().cloned())
            {
                debug!("image relay to {peer:#x} failed: {e}");
            }
        }
    }

    /// Sliding-window *bandwidth* budget for chat images: admit the
    /// image only if the last minute's accepted bytes plus this one
    /// stay within both the per-client and server-wide budgets; bytes
    /// are recorded only for accepted images. Dropped images cost the
    /// sender nothing here (their upload already crossed the wire —
    /// that part is physics), but they are never multiplied into
    /// `clients ×` relay egress.
    pub(crate) fn allow_image(&mut self, client_id: ClientId, len: usize) -> bool {
        let per_client = self.config.limits.max_transfer_kb_per_minute * 1024;
        let global = self.config.limits.max_transfer_kb_per_minute_global * 1024;
        if per_client == 0 || global == 0 {
            return false;
        }
        let now = self.now;
        let window = Duration::from_mins(1);

        let entries = self.image_bytes.entry(client_id).or_default();
        entries.retain(|(t, _)| now.duration_since(*t) < window);
        let client_used: usize = entries.iter().map(|(_, b)| b).sum();
        if client_used + len > per_client {
            warn!(
                "Image bandwidth limit for {}: {} KB used of {} KB/min, dropping a {} KB image",
                self.usernames
                    .get(&client_id)
                    .map_or("<unknown>", String::as_str),
                client_used / 1024,
                per_client / 1024,
                len / 1024,
            );
            return false;
        }

        self.image_bytes_global
            .retain(|(t, _)| now.duration_since(*t) < window);
        let global_used: usize = self.image_bytes_global.iter().map(|(_, b)| b).sum();
        if global_used + len > global {
            warn!(
                "Server-wide image bandwidth limit: {} KB used of {} KB/min, \
                 dropping a {} KB image from {}",
                global_used / 1024,
                global / 1024,
                len / 1024,
                self.usernames
                    .get(&client_id)
                    .map_or("<unknown>", String::as_str),
            );
            return false;
        }

        self.image_bytes
            .entry(client_id)
            .or_default()
            .push((now, len));
        self.image_bytes_global.push((now, len));
        true
    }

    /// A "send my log to this server" upload (§9.1): reassembled like an
    /// image, capped at [`protocol::MAX_LOG_UPLOAD_BYTES`], one per
    /// client per ten minutes, stored under `<identity.dir>/logs/`.
    fn handle_log_chunk(
        &mut self,
        client_id: ClientId,
        upload_id: u32,
        index: u32,
        count: u32,
        data: Vec<u8>,
    ) {
        if index == 0
            && self
                .last_log_upload
                .get(&client_id)
                .is_some_and(|t| self.now.duration_since(*t) < LOG_UPLOAD_MIN_INTERVAL)
        {
            debug!(
                "Ignoring log upload from {}: one per {} minutes",
                self.get_username(client_id),
                LOG_UPLOAD_MIN_INTERVAL.as_secs() / 60
            );
            self.log_uploads.remove(&client_id);
            return;
        }
        let Some(folded) = fold_upload(
            &mut self.log_uploads,
            client_id,
            upload_id,
            index,
            count,
            data,
            protocol::MAX_LOG_UPLOAD_CHUNKS,
            protocol::MAX_LOG_UPLOAD_BYTES,
            self.now,
        ) else {
            return;
        };
        let log = match folded {
            Folded::Complete(log) => log,
            Folded::Malformed | Folded::Oversized(_) => {
                warn!(
                    "Dropping malformed or oversized log upload from {}",
                    self.get_username(client_id)
                );
                return;
            }
        };
        self.last_log_upload.insert(client_id, self.now);
        let username = self.get_username(client_id);
        let dir = PathBuf::from(&self.config.identity.dir).join("logs");
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let safe = safety::log_safe_name(&username);
        // The account's id in the name: its deletion finds them
        // (`delete_uploaded_logs`).
        let user = self
            .user_of
            .get(&client_id)
            .map_or_else(|| "0".repeat(16), |u| hex(&u[..8]));
        let path = dir.join(format!("{safe}-{user}-{client_id:x}-{stamp}.log"));
        match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &log)) {
            Ok(()) => info!(
                "[{username}] uploaded their log ({} KB) → {}",
                log.len() / 1024,
                path.display()
            ),
            Err(e) => warn!(
                "[{username}] uploaded their log ({} KB) but it could not be stored at {}: {e}",
                log.len() / 1024,
                path.display()
            ),
        }
    }

    // ── Connection events ───────────────────────────────────────────

    fn handle_client_connected(&mut self, client_id: ClientId, username: String) {
        info!("{username} ({client_id:#x}) connected (in the lobby)");
        if self.usernames.insert(client_id, username).is_none() {
            self.roster.push(client_id);
        }
        // Everyone lands in the default channel (config guarantees it
        // is open and unlimited) — the lobby (spec §12, 1.1.5): nobody
        // is told until they join a channel, which is when `announce`
        // does everything a connect used to.
        self.channel_of.insert(client_id, self.default_channel);

        // Full state snapshot for the new client: user list, channel
        // roster and memberships, active shares, and current audio
        // states — so a client joining an in-progress session renders
        // it correctly instead of assuming everyone is unmuted, nobody
        // is streaming, and the world is one big room. Lobby members
        // other than the newcomer are left out: they are not there.
        let visible: HashSet<ClientId> = self
            .clients()
            .into_iter()
            .filter(|c| *c == client_id || !self.in_lobby(*c))
            .collect();
        let init = ServerMessage::Init {
            usernames: self
                .usernames
                .iter()
                .filter(|(c, _)| visible.contains(c))
                .map(|(c, n)| (*c, n.clone()))
                .collect(),
            client_id,
            server_name: self.config.server.name.clone(),
            greeting: self.config.server.greeting_message.clone(),
            channels: self.channel_infos.clone(),
            memberships: self
                .channel_of
                .iter()
                .filter(|(c, _)| visible.contains(c))
                .map(|(c, ch)| (*c, *ch))
                .collect(),
        };
        self.send_msg(client_id, &init);

        // No streams are announced here: a session arrives in the
        // lobby, which sees none (spec §12); `announce` tells it about
        // the ones it can watch when it joins a channel.

        let states: Vec<(ClientId, (bool, bool))> = self
            .audio_states
            .iter()
            .filter(|(id, _)| !self.in_lobby(**id))
            .map(|(id, s)| (*id, *s))
            .collect();
        for (peer_id, (muted, deafened)) in states {
            self.send_msg(
                client_id,
                &ServerMessage::AudioState {
                    client_id: peer_id,
                    muted,
                    deafened,
                },
            );
        }

        self.last_activity.insert(client_id, self.now);
        self.send_channel_details(client_id);
        self.replay_history(client_id);
        self.send_conversations(client_id);

        // Who everyone is, for clients that render roles (spec §8):
        // the whole roster to the newcomer, the newcomer to everyone.
        let details: Vec<ServerMessage> = self
            .user_of
            .iter()
            .filter(|(c, _)| **c != client_id && visible.contains(c))
            .map(|(c, u)| self.user_details(*c, u))
            .collect();
        for msg in details {
            self.send_msg(client_id, &msg);
        }
        // Everyone's look, ours included (the server's copy is the
        // one that follows the key), for sessions that render it.
        let profiles: Vec<ServerMessage> = self
            .user_of
            .iter()
            .filter(|(c, _)| visible.contains(c))
            .map(|(c, u)| self.user_profile(*c, u))
            .collect();
        for msg in profiles {
            self.send_msg(client_id, &msg);
        }
        // Everyone's progress, ours included (the newcomer's own bar
        // needs its xp too), for sessions that render it.
        let progress: Vec<ServerMessage> = self
            .user_of
            .iter()
            .filter(|(c, _)| visible.contains(c))
            .map(|(c, u)| self.user_progress(*c, u))
            .collect();
        for msg in progress {
            self.send_msg(client_id, &msg);
        }
        self.refresh_enforcement();
    }

    /// A session became visible: it left the lobby for a room. What a
    /// connect used to broadcast happens now — the join (carrying the
    /// room, so clients render "joined <channel>"), who they are,
    /// their look, their progress, their audio state — and the
    /// newcomer learns of the streams it may now watch. Its own
    /// `UserMovedChannel` still goes to it alone: the others have the
    /// room from the join.
    fn announce(&mut self, client_id: ClientId, channel_id: u16) {
        let username = self.get_username(client_id);
        self.broadcast_msg_except(
            client_id,
            &ServerMessage::UserJoined {
                client_id,
                username,
                channel_id,
            },
        );
        if let Some(user_id) = self.user_of.get(&client_id).copied() {
            let msg = self.user_details(client_id, &user_id);
            for peer in self.clients() {
                self.send_msg(peer, &msg);
            }
            let msg = self.user_profile(client_id, &user_id);
            for peer in self.clients() {
                self.send_msg(peer, &msg);
            }
            let msg = self.user_progress(client_id, &user_id);
            for peer in self.clients() {
                self.send_msg(peer, &msg);
            }
        }
        if let Some((muted, deafened)) = self.audio_states.get(&client_id).copied() {
            self.broadcast_msg_except(
                client_id,
                &ServerMessage::AudioState {
                    client_id,
                    muted,
                    deafened,
                },
            );
        }
        self.send_msg(
            client_id,
            &ServerMessage::UserMovedChannel {
                client_id,
                channel_id,
            },
        );
        // Streams for everyone appear now; channel-scoped ones in the
        // new room come from `revalidate_shares_after_move`.
        let sharers: Vec<(ClientId, ShareInfo)> = self
            .video
            .sharers()
            .iter()
            .filter(|(id, info)| **id != client_id && info.scope == ShareScope::Everyone)
            .map(|(id, info)| (*id, *info))
            .collect();
        for (sharer_id, info) in sharers {
            self.send_msg(
                client_id,
                &ServerMessage::ScreenShareStarted {
                    client_id: sharer_id,
                    width: info.width,
                    height: info.height,
                    scope: info.scope,
                },
            );
        }
    }

    /// A session went back to the lobby (an admin move, a channel
    /// kick): to everyone else it left. Its own stream stops, it stops
    /// watching, and every stream disappears from its view.
    fn withdraw(&mut self, client_id: ClientId) {
        if self.video.is_sharing(client_id) {
            self.video
                .stop_sharing(&mut self.driver, self.now, client_id);
            self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
        }
        let sharers: Vec<ClientId> = self.video.sharers().keys().copied().collect();
        for sharer in sharers {
            if self.video.is_subscribed(client_id, sharer) {
                self.video
                    .unsubscribe(&mut self.driver, self.now, client_id, sharer);
            }
            self.send_msg(
                client_id,
                &ServerMessage::ScreenShareStopped { client_id: sharer },
            );
        }
        self.broadcast_msg_except(client_id, &ServerMessage::UserLeft { client_id });
        self.send_msg(
            client_id,
            &ServerMessage::UserMovedChannel {
                client_id,
                channel_id: self.default_channel,
            },
        );
    }

    /// In the lobby: the default channel, where a session is not
    /// there yet (spec §12, 1.1.5).
    fn in_lobby(&self, client: ClientId) -> bool {
        self.channel_of.get(&client).copied() == Some(self.default_channel)
    }

    /// Who is told about `subject`'s session: everyone once it is in a
    /// room, only the session itself while it sits in the lobby, where
    /// nobody has heard of it. Every broadcast about one session goes
    /// through here; speaking, audio state, a rename and a role change
    /// used to reach every client, which told them a lobby session was
    /// there (review P2-1). What changed meanwhile reaches the others
    /// with [`Self::announce`], when it joins a room.
    pub(crate) fn audience_about(&self, subject: ClientId) -> Vec<ClientId> {
        if self.in_lobby(subject) {
            vec![subject]
        } else {
            self.clients()
        }
    }

    fn handle_client_disconnected(&mut self, client_id: ClientId, reason: &DisconnectReason) {
        let username = self
            .usernames
            .remove(&client_id)
            .unwrap_or_else(|| format!("User{client_id:x}"));
        self.roster.retain(|c| *c != client_id);
        self.features_of.remove(&client_id);
        self.chat_tokens.remove(&client_id);
        self.last_activity.remove(&client_id);
        self.spoke.remove(&client_id);
        self.blob_uploads.remove(&client_id);
        self.announced_uploads.remove(&client_id);
        self.renamed_at.remove(&client_id);
        if let Some(user_id) = self.user_of.remove(&client_id) {
            let key_hash = self.key_of.remove(&client_id);
            let now = self.wall_now();
            if let Some(u) = self.policy.users.get_mut(&user_id) {
                u.last_seen = now;
            }
            if let Some(k) = key_hash.and_then(|k| self.policy.keys.get_mut(&k)) {
                k.last_seen = now;
            }
            self.persist_user(&user_id);
            if let Some(key_hash) = key_hash {
                self.persist_key(&key_hash);
            }
        }
        self.audio_states.remove(&client_id);
        self.deafened.remove(&client_id);
        let was_visible = self
            .channel_of
            .remove(&client_id)
            .is_some_and(|ch| ch != self.default_channel);
        self.image_bytes.remove(&client_id);
        self.uploads.remove(&client_id);
        self.log_uploads.remove(&client_id);
        self.last_log_upload.remove(&client_id);
        self.link_baseline.remove(&client_id);
        self.link_voice.remove(&client_id);

        match reason {
            DisconnectReason::ApplicationClosed { .. } | DisconnectReason::LocallyClosed => {
                info!("{username} ({client_id:#x}) disconnected: {reason}");
            }
            DisconnectReason::TimedOut => {
                warn!(
                    "{username} ({client_id:#x}) timed out — no packets for the idle \
                     timeout (client crashed, network gone, or a NAT dropped the mapping)"
                );
            }
            other => warn!("{username} ({client_id:#x}) disconnected: {other}"),
        }

        let was_sharing = self.video.is_sharing(client_id);
        self.video
            .handle_disconnect(&mut self.driver, self.now, client_id);
        self.voice.handle_disconnect(client_id);
        if was_sharing {
            self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
        }

        // A lobby member was never announced, so nobody hears it go.
        if was_visible {
            self.broadcast_msg(&ServerMessage::UserLeft { client_id });
        }
    }

    // ── Control-plane messages ──────────────────────────────────────

    fn process_control_messages(&mut self, readable: &[ClientId]) {
        // Pending connections: exactly one Control frame matters — the
        // Hello. Anything else they send before admission is discarded.
        let pending: Vec<ClientId> = readable
            .iter()
            .copied()
            .filter(|peer| self.pending.contains_key(peer))
            .collect();
        for peer in pending {
            if let Some(raw) = self.driver.recv(peer, Lane::Control) {
                self.handle_hello(peer, &raw);
            }
            for lane in [Lane::Signal, Lane::Media, Lane::Voice, Lane::SystemAudio] {
                while self.driver.recv(peer, lane).is_some() {}
            }
            while self.driver.recv_video(peer).is_some() {}
        }

        for &client_id in readable {
            // Admitted — just now, by the Hello above, counts.
            if !self.usernames.contains_key(&client_id) {
                continue;
            }
            for lane in [Lane::Control, Lane::Signal, Lane::Media] {
                while let Some(message) = self.driver.recv(client_id, lane) {
                    self.stats.bytes_received += message.len() as u64;
                    match protocol::decode_client_message(&message) {
                        Some(msg) => self.handle_client_message(client_id, msg),
                        None => debug!(
                            "Undecodable {lane} message ({} bytes) from {client_id:#x}",
                            message.len()
                        ),
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_client_message(&mut self, client_id: ClientId, message: ClientMessage) {
        match message {
            ClientMessage::Hello { .. } => {
                debug!("{client_id:#x} sent a second Hello; ignored");
            }
            ClientMessage::Chat(text) => {
                if !self.allowed(
                    client_id,
                    permissions::TEXT_GENERAL,
                    RestrictionKind::GeneralText,
                ) {
                    return;
                }
                if !self.chat_allowed_by_rate(client_id) {
                    return;
                }
                let text =
                    protocol::truncate_on_char_boundary(&text, self.config.limits.max_chat_bytes);
                let username = self.get_username(client_id);
                info!("[{username}]: {text}");
                self.stats.messages_count += 1;
                self.note_activity(client_id);
                let (id, ts) = self.stamp();
                let msg = ChatMessage {
                    id,
                    ts,
                    client_id,
                    text: text.clone(),
                };
                // With its author's account for sessions that can block
                // and report; nothing to whoever blocked the author.
                match self.user_of.get(&client_id).copied() {
                    Some(author) => {
                        let rich = ServerMessage::ChatFrom {
                            from: author,
                            msg: msg.clone(),
                        };
                        let plain = ServerMessage::Chat(msg);
                        let audience = self.audience_of(&author);
                        self.introduce_author(client_id, &author, &audience);
                        for peer in audience {
                            if self.has_feature(peer, features::SAFETY) {
                                self.send_msg(peer, &rich);
                            } else {
                                self.send_msg(peer, &plain);
                            }
                        }
                    }
                    None => self.broadcast_msg(&ServerMessage::Chat(msg)),
                }
                // Delivery never waits for the disk: broadcast first,
                // then enqueue the insert. Under backpressure this is a
                // hole in history, never a lost message.
                if let Some(user_id) = self.user_of.get(&client_id).copied() {
                    let row = MessageRow {
                        msg_id: id,
                        conv_id: GENERAL_CONV,
                        ts,
                        user_id,
                        body: text,
                        name: username,
                        edited: false,
                        blob_id: None,
                    };
                    self.remember_message(&row);
                    self.persist.send(Command::InsertMessage(row));
                    self.progress.entry(user_id).or_default().messages += 1;
                }
            }
            ClientMessage::FetchHistory {
                conv,
                before,
                limit,
            } => self.handle_fetch_history(client_id, conv, before, limit),
            ClientMessage::EditMessage { msg_id, text } => {
                self.handle_edit_message(client_id, msg_id, text);
            }
            ClientMessage::DeleteMessage { msg_id } => {
                self.handle_delete_message(client_id, msg_id, false);
            }
            ClientMessage::CreateChannel(spec) => self.handle_create_channel(client_id, spec),
            ClientMessage::EditChannel { channel_id, spec } => {
                self.handle_edit_channel(client_id, channel_id, spec);
            }
            ClientMessage::DeleteChannel { channel_id } => {
                self.handle_delete_channel(client_id, channel_id);
            }
            ClientMessage::ChannelKick {
                channel_id,
                user_id,
            } => self.handle_channel_kick(client_id, channel_id, user_id),
            ClientMessage::SetProfile(profile) => self.handle_set_profile(client_id, profile),
            ClientMessage::SetNickname { name } => self.handle_set_nickname(client_id, &name),
            ClientMessage::UploadBegin {
                upload_id,
                name,
                mime,
                size,
                sha256,
            } => self.handle_upload_begin(client_id, upload_id, name, mime, size, sha256),
            ClientMessage::UploadChunk {
                upload_id,
                index,
                count,
                data,
            } => self.handle_upload_chunk(client_id, upload_id, index, count, data),
            ClientMessage::FetchBlob { blob_id } => self.handle_fetch_blob(client_id, blob_id),
            ClientMessage::DirectMessage {
                to,
                text,
                client_ref,
            } => self.handle_direct_message(client_id, to, text, client_ref),
            ClientMessage::MarkRead { conv, msg_id } => {
                self.handle_mark_read(client_id, conv, msg_id);
            }
            ClientMessage::SendFile {
                conv,
                blob_id,
                client_ref,
            } => self.handle_send_file(client_id, conv, blob_id, client_ref),
            ClientMessage::ChatImageChunk {
                upload_id,
                index,
                count,
                data,
            } => {
                // An image is a chat message too: a text mute silences
                // it and it spends a rate-limit token, on top of needing
                // `UPLOAD`. Told once per upload (on its first chunk),
                // then the rest of it is dropped quietly.
                let permitted = if index == 0 {
                    self.allowed(client_id, permissions::UPLOAD, RestrictionKind::Upload)
                        && self.allowed(
                            client_id,
                            permissions::TEXT_GENERAL,
                            RestrictionKind::GeneralText,
                        )
                        && self.chat_allowed_by_rate(client_id)
                } else {
                    self.has_permission(client_id, permissions::UPLOAD)
                        && self.has_permission(client_id, permissions::TEXT_GENERAL)
                };
                if !permitted {
                    return;
                }
                self.handle_image_chunk(client_id, upload_id, index, count, data);
            }
            ClientMessage::Speaking(is_speaking) => {
                if is_speaking {
                    self.note_activity(client_id);
                    self.spoke.insert(client_id);
                }
                // To everyone who knows the session, the sender
                // included: older clients treat the echo as
                // authoritative for their own tile.
                let msg = ServerMessage::Speaking {
                    client_id,
                    is_speaking,
                };
                for peer in self.audience_about(client_id) {
                    self.send_msg(peer, &msg);
                }
            }
            ClientMessage::AudioState { muted, deafened } => {
                self.audio_states.insert(client_id, (muted, deafened));
                if deafened {
                    self.deafened.insert(client_id);
                } else {
                    self.deafened.remove(&client_id);
                }
                let msg = ServerMessage::AudioState {
                    client_id,
                    muted,
                    deafened,
                };
                for peer in self.audience_about(client_id) {
                    if peer != client_id {
                        self.send_msg(peer, &msg);
                    }
                }
            }

            // ── Screen share signaling ──────────────────────────────
            ClientMessage::ScreenShareStart {
                width,
                height,
                scope,
            } => {
                self.note_activity(client_id);
                self.handle_share_start(client_id, width, height, scope);
            }
            ClientMessage::ScreenShareStop => {
                let username = self.get_username(client_id);
                info!("{username} stopped screen share");
                self.video
                    .stop_sharing(&mut self.driver, self.now, client_id);
                self.broadcast_msg(&ServerMessage::ScreenShareStopped { client_id });
            }
            ClientMessage::SubscribeScreen { target_id } => {
                self.note_activity(client_id);
                self.handle_subscribe(client_id, target_id);
            }
            ClientMessage::UnsubscribeScreen { target_id } => {
                self.video
                    .unsubscribe(&mut self.driver, self.now, client_id, target_id);
            }
            ClientMessage::RequestKeyframe { target_id } => {
                // Only honored from subscribed viewers, so a misbehaving
                // client can't force arbitrary encoders to spam IDRs.
                // The per-sharer gate is the single authoritative rate
                // limit for all keyframe request paths.
                if self.video.is_subscribed(client_id, target_id)
                    && self.video.allow_keyframe_request(self.now, target_id)
                {
                    self.send_keyframe_request(target_id, client_id);
                }
            }

            // ── Voice channels ──────────────────────────────────────
            ClientMessage::JoinChannel {
                channel_id,
                password,
            } => {
                self.note_activity(client_id);
                self.handle_join_channel(client_id, channel_id, password.as_deref());
            }

            // ── Invites (spec §3.1) ─────────────────────────────────
            ClientMessage::CreateInvite {
                grants_role,
                expires_secs,
                max_uses,
            } => {
                let Some(user_id) = self.user_of.get(&client_id).copied() else {
                    return;
                };
                let perms = self.effective_permissions(&user_id);
                let role = grants_role.unwrap_or(permissions::ROLE_GUEST);
                let manage = permissions::has(perms, permissions::MANAGE_INVITES);
                if !permissions::has(perms, permissions::CREATE_INVITES)
                    || (role != permissions::ROLE_GUEST && !manage)
                {
                    self.send_msg(
                        client_id,
                        &ServerMessage::InviteDenied {
                            reason: InviteDenyReason::NoPermission,
                        },
                    );
                    return;
                }
                // Members get the default expiry at most; admins choose.
                let ttl = expires_secs
                    .filter(|s| *s > 0)
                    .map_or(DEFAULT_INVITE_TTL_MS, |s| s.saturating_mul(1000));
                let ttl = if manage {
                    ttl
                } else {
                    ttl.min(DEFAULT_INVITE_TTL_MS)
                };
                let answer = match self.mint_invite(Some(user_id), role, Some(ttl), max_uses) {
                    Ok((code, invite)) => {
                        info!(
                            "{} ({client_id:#x}) created invite {code} (role {role})",
                            self.get_username(client_id)
                        );
                        ServerMessage::InviteCreated {
                            code,
                            grants_role: role,
                            expires_at: invite.expires_at,
                            max_uses,
                        }
                    }
                    Err(reason) => ServerMessage::InviteDenied { reason },
                };
                self.send_msg(client_id, &answer);
            }

            // ── Administration (spec §8) ────────────────────────────
            ClientMessage::Admin {
                request_id,
                command,
            } => {
                // The audit log is a disk read: answered when the store
                // does (`resolve_audit_answer`), everything else now.
                if let AdminCommand::AuditLog { before, limit } = command {
                    self.handle_audit_log(client_id, request_id, before, limit);
                    return;
                }
                // The reports likewise (1.2).
                if let AdminCommand::ListReports { resolved } = command {
                    self.handle_list_reports(client_id, request_id, resolved);
                    return;
                }
                // What one command writes — a kick is the keys, each
                // role and the audit line — is one transaction.
                self.persist.begin_group();
                let result = if let AdminCommand::ResolveReport { id } = command {
                    self.handle_resolve_report(client_id, id)
                } else {
                    self.handle_admin(client_id, command)
                };
                self.persist.end_group();
                self.send_admin_result(client_id, request_id, result);
            }

            // ── Telemetry ───────────────────────────────────────────
            ClientMessage::Telemetry(sample) => {
                if !self.accepts_telemetry(client_id) {
                    return;
                }
                if self.config.logging.telemetry {
                    info!(
                        "telemetry: {} ({client_id:#x}): {}",
                        self.get_username(client_id),
                        summarize_telemetry(&sample)
                    );
                }
                if let Some(user_id) = self.user_of.get(&client_id).copied() {
                    let line = summarize_telemetry(&sample);
                    self.last_telemetry
                        .insert(user_id, (self.clock.wall_ms(self.now), line));
                }
                if self.persist.is_persistent()
                    && let Some(user_id) = self.user_of.get(&client_id).copied()
                    && let Some(key_hash) = self.key_of.get(&client_id).copied()
                    && let Some(row) = TelemetryRow::from_sample(
                        self.clock.wall_ms(self.now),
                        user_id,
                        key_hash,
                        &sample,
                    )
                {
                    self.pending_telemetry.insert(user_id, row);
                }
            }
            ClientMessage::TelemetryEvent(event) => {
                if !self.accepts_telemetry(client_id) {
                    return;
                }
                info!(
                    "telemetry event: {} ({client_id:#x}): {}",
                    self.get_username(client_id),
                    describe_event(&event)
                );
                if self.persist.is_persistent()
                    && let Some(user_id) = self.user_of.get(&client_id).copied()
                    && let Some(key_hash) = self.key_of.get(&client_id).copied()
                    && let Some(encoded) = protocol::encode_telemetry_event(&event)
                {
                    self.persist
                        .send(Command::TelemetryEvent(TelemetryEventRow {
                            ts: self.clock.wall_ms(self.now),
                            user_id,
                            key_hash,
                            kind: event_kind(&event).to_string(),
                            event: encoded,
                        }));
                }
            }
            ClientMessage::LogUploadChunk {
                upload_id,
                index,
                count,
                data,
            } => {
                self.handle_log_chunk(client_id, upload_id, index, count, data);
            }

            // ── Block, report, delete (1.2, protocol::safety) ───────
            ClientMessage::Block { user } => self.handle_block(client_id, user, true),
            ClientMessage::Unblock { user } => self.handle_block(client_id, user, false),
            ClientMessage::Report {
                target,
                reason,
                note,
            } => self.handle_report(client_id, target, reason, &note),
            ClientMessage::DeleteAccount => self.handle_delete_account(client_id),
        }
    }

    fn accepts_telemetry(&self, client_id: ClientId) -> bool {
        self.features_of
            .get(&client_id)
            .is_some_and(|f| features::has(*f, features::TELEMETRY))
    }

    fn handle_share_start(
        &mut self,
        client_id: ClientId,
        width: u16,
        height: u16,
        scope: ShareScope,
    ) {
        let dims_ok = (MIN_SHARE_DIMENSION..=MAX_SHARE_DIMENSION).contains(&width)
            && (MIN_SHARE_DIMENSION..=MAX_SHARE_DIMENSION).contains(&height);
        if !dims_ok {
            warn!("{client_id:#x} announced share with invalid dimensions {width}x{height}");
            self.send_msg(
                client_id,
                &ServerMessage::ShareDenied {
                    reason: DenyReason::InvalidDimensions,
                },
            );
            return;
        }

        if !self.allowed(client_id, permissions::SHARE, RestrictionKind::Share)
            || self.in_lobby(client_id)
        {
            self.send_msg(
                client_id,
                &ServerMessage::ShareDenied {
                    reason: DenyReason::NotPermitted,
                },
            );
            return;
        }

        // Admission control: cap concurrent shares. Re-announcing an
        // existing share (e.g. quality change) is always allowed;
        // `BYPASS_LIMITS` ignores the cap.
        if !self.video.is_sharing(client_id)
            && self.video.sharer_count() >= self.config.limits.max_sharers
            && !self.has_permission(client_id, permissions::BYPASS_LIMITS)
        {
            info!(
                "Share denied for {client_id:#x}: {} of {} share slots in use",
                self.video.sharer_count(),
                self.config.limits.max_sharers
            );
            self.send_msg(
                client_id,
                &ServerMessage::ShareDenied {
                    reason: DenyReason::ShareLimitReached,
                },
            );
            return;
        }

        // A re-announce that narrows scope Everyone → Channel must
        // evict out-of-channel viewers, exactly like a channel move.
        let previous_scope = self.video.share_info(client_id).map(|info| info.scope);

        let username = self.get_username(client_id);
        info!("{username} started screen share ({width}x{height}, {scope:?})");
        self.video.start_sharing(
            client_id,
            ShareInfo {
                width,
                height,
                scope,
            },
        );

        let started = ServerMessage::ScreenShareStarted {
            client_id,
            width,
            height,
            scope,
        };
        match scope {
            ShareScope::Everyone => {
                for peer in self.clients() {
                    if !self.in_lobby(peer) {
                        self.send_msg(peer, &started);
                    }
                }
            }
            ShareScope::Channel => {
                // Only the sharer's channel learns about the stream —
                // for everyone else it doesn't exist. The sharer is a
                // member of their own channel, so this covers them too.
                let channel = self.channel_of.get(&client_id).copied();
                for peer in self.peers_in_channel(channel) {
                    self.send_msg(peer, &started);
                }
                if previous_scope == Some(ShareScope::Everyone) {
                    self.evict_out_of_channel_viewers(client_id, channel);
                    // Other channels saw this stream while it was
                    // public; tell them it's gone from their view.
                    let stopped = ServerMessage::ScreenShareStopped { client_id };
                    let outside: Vec<ClientId> = self
                        .clients()
                        .into_iter()
                        .filter(|peer| {
                            *peer != client_id && self.channel_of.get(peer).copied() != channel
                        })
                        .collect();
                    for peer in outside {
                        self.send_msg(peer, &stopped);
                    }
                }
            }
        }
    }
    // ── Voice channels ──────────────────────────────────────────────

    /// Watch `path` for changes and hot-apply the `channels` section.
    /// Called once from main after startup; everything else in the
    /// file requires a restart (and says so in the log).
    pub fn watch_config_file(&mut self, path: PathBuf) {
        self.config_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        info!(
            "Watching {} for channel changes (checked every {}s; only the \
             [channels] section hot-applies)",
            path.display(),
            CONFIG_CHECK_INTERVAL.as_secs(),
        );
        self.config_path = Some(path);
    }

    /// If the watched config file's mtime moved, re-read it and apply
    /// the channel list. A file that fails to parse or validate is
    /// logged and ignored — a live server never dies or degrades from
    /// a bad edit.
    fn check_config_reload(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;

        let fresh = match Config::from_file(&path) {
            Ok(fresh) => fresh,
            Err(e) => {
                warn!(
                    "Config reload: {} is not valid YAML — keeping current config: {e}",
                    path.display()
                );
                return;
            }
        };
        // What the next start checks. A file this server would refuse to
        // boot with (the AFK room taken out of the list while
        // `user_channels.afk_channel` still names it) must not be half
        // applied to the running one: it kept working, and the container
        // crash-looped on its next restart (review P2-4).
        if let Err(e) = fresh.validate_policy() {
            warn!(
                "Config reload: {} would not start a server — keeping current config: {e}",
                path.display()
            );
            return;
        }
        if fresh.channels == self.config.channels {
            info!(
                "Config reload: {} changed but the [channels] section did not — \
                 other sections require a restart to apply",
                path.display(),
            );
            return;
        }
        match self.apply_channel_config(fresh.channels) {
            Ok(()) => info!(
                "Config reload: channel list applied from {}",
                path.display()
            ),
            Err(e) => warn!("Config reload: invalid [channels] — keeping current list: {e}"),
        }
    }

    /// Swap in a new channel list on a live server.
    ///
    /// Channels are matched **by name**: a channel that keeps its name
    /// keeps its members (even if it moved position, or its password /
    /// user limit changed — nobody is ever kicked by a reload). Members
    /// of a removed or renamed channel are moved to the new default.
    /// Clients get one authoritative [`ServerMessage::ChannelListUpdated`]
    /// (ids are list indexes, so a reload renumbers them and clients
    /// must resync wholesale), and channel-scoped share visibility is
    /// re-derived with exactly-once corrective Started/Stopped messages.
    ///
    /// # Errors
    /// The list failed validation; nothing was changed.
    pub fn apply_channel_config(&mut self, channels: Vec<ChannelSettings>) -> Result<(), String> {
        validate_channel_list(&channels)?;
        let previous = std::mem::replace(&mut self.config.channels, channels);
        if let Err(e) = self.rebuild_channel_list() {
            self.config.channels = previous;
            return Err(e);
        }
        Ok(())
    }

    /// The renumbering itself, for the full list (config channels then
    /// UI ones); see `apply_channel_config` for the semantics.
    pub(crate) fn apply_channel_list(
        &mut self,
        channels: Vec<ChannelSettings>,
    ) -> Result<(), String> {
        let new_default = validate_channel_list(&channels)?;
        let channels = crate::config::with_lobby(&channels);

        #[allow(clippy::cast_possible_truncation)] // validated ≤ MAX_CHANNELS
        let new_id_of: HashMap<String, u16> = channels
            .iter()
            .enumerate()
            .map(|(i, ch)| (ch.name.trim().to_string(), i as u16))
            .collect();

        // Snapshot who-was-where by *name* before renumbering.
        let old_name_of: HashMap<ClientId, String> = self
            .channel_of
            .iter()
            .map(|(client, id)| {
                let name = self
                    .channel_infos
                    .get(usize::from(*id))
                    .map(|info| info.name.clone())
                    .unwrap_or_default();
                (*client, name)
            })
            .collect();

        let mut moved_to_default = 0usize;
        let new_channel_of: HashMap<ClientId, u16> = old_name_of
            .iter()
            .map(|(client, name)| {
                let new_id = new_id_of.get(name).copied().unwrap_or_else(|| {
                    moved_to_default += 1;
                    new_default
                });
                (*client, new_id)
            })
            .collect();

        // Whoever a removed room dumps into the lobby is withdrawn
        // below, exactly as an admin's move there would: their stream
        // stops, they stop watching, and the others see them leave.
        let old_lobby = self.default_channel;
        let demoted: Vec<ClientId> = new_channel_of
            .iter()
            .filter(|(client, new_ch)| {
                **new_ch == new_default
                    && self
                        .channel_of
                        .get(*client)
                        .is_some_and(|old| *old != old_lobby)
            })
            .map(|(client, _)| *client)
            .collect();

        self.channel_list = channels;
        self.channel_infos = channel_infos_of(&self.channel_list);
        self.default_channel = new_default;
        self.channel_of = new_channel_of;

        info!(
            "Channel list updated: {} (plus the lobby); {} member(s) moved to the lobby",
            self.channel_infos
                .iter()
                .filter(|c| c.name != crate::config::LOBBY_NAME)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            moved_to_default,
        );

        // Memberships per recipient: lobby members other than the
        // recipient itself stay out, as in `Init`.
        for client in self.clients() {
            let memberships: HashMap<ClientId, u16> = self
                .channel_of
                .iter()
                .filter(|(c, _)| **c == client || !self.in_lobby(**c))
                .map(|(c, ch)| (*c, *ch))
                .collect();
            self.send_msg(
                client,
                &ServerMessage::ChannelListUpdated {
                    channels: self.channel_infos.clone(),
                    memberships,
                },
            );
        }

        for client in demoted {
            self.withdraw(client);
        }

        // ── Re-derive channel-scoped share visibility ───────────────
        // For each channel-scoped stream, compare each client's view
        // before (same channel *name* as the sharer) and after (same
        // new channel id): gained → Started, lost → Stopped, exactly
        // once. Out-of-channel subscriptions are dropped silently here;
        // the delta loop delivers their Stopped.
        let sharers: Vec<(ClientId, ShareInfo)> = self
            .video
            .sharers()
            .iter()
            .filter(|(_, info)| info.scope == ShareScope::Channel)
            .map(|(id, info)| (*id, *info))
            .collect();
        for (sharer, info) in sharers {
            let Some(sharer_channel) = self.channel_of.get(&sharer).copied() else {
                continue;
            };
            for viewer in self.video.subscribers_of(sharer) {
                if self.channel_of.get(&viewer).copied() != Some(sharer_channel) {
                    self.video
                        .unsubscribe(&mut self.driver, self.now, viewer, sharer);
                }
            }

            let sharer_old_name = old_name_of.get(&sharer).cloned().unwrap_or_default();
            let started = ServerMessage::ScreenShareStarted {
                client_id: sharer,
                width: info.width,
                height: info.height,
                scope: info.scope,
            };
            let stopped = ServerMessage::ScreenShareStopped { client_id: sharer };
            for peer in self.clients() {
                if peer == sharer {
                    continue;
                }
                let saw_before = old_name_of.get(&peer) == Some(&sharer_old_name);
                let sees_now = self.channel_of.get(&peer).copied() == Some(sharer_channel);
                if sees_now && !saw_before {
                    self.send_msg(peer, &started);
                } else if saw_before && !sees_now {
                    self.send_msg(peer, &stopped);
                }
            }
        }
        Ok(())
    }

    /// Validate and execute a channel move. Password and capacity
    /// checks run here; on success the move is broadcast (including to
    /// the mover) and channel-scoped share visibility is re-validated.
    fn handle_join_channel(
        &mut self,
        client_id: ClientId,
        channel_id: u16,
        password: Option<&str>,
    ) {
        let Some(target) = self.channel_list.get(usize::from(channel_id)).cloned() else {
            self.deny_join(client_id, channel_id, ChannelDenyReason::NoSuchChannel);
            return;
        };
        let old_channel = self.channel_of.get(&client_id).copied();
        if old_channel == Some(channel_id) {
            return; // already there — nothing to do
        }
        if !self.has_permission(client_id, permissions::JOIN_CHANNELS)
            || !self.join_gate_allows(client_id, channel_id)
        {
            self.deny_join(client_id, channel_id, ChannelDenyReason::NotPermitted);
            return;
        }
        if let Some(expected) = target.password.as_deref()
            && password != Some(expected)
        {
            info!(
                "{} denied joining '{}': bad or missing password",
                self.get_username(client_id),
                target.name,
            );
            self.deny_join(client_id, channel_id, ChannelDenyReason::WrongPassword);
            return;
        }
        if let Some(limit) = target.user_limit
            && self.members_in(channel_id) >= usize::from(limit)
        {
            self.deny_join(client_id, channel_id, ChannelDenyReason::ChannelFull);
            return;
        }

        self.place_in_channel(client_id, channel_id);
    }

    /// Move a session to `channel_id` with no checks: the join path
    /// has done them, an admin `MoveUser` bypasses them.
    pub(crate) fn place_in_channel(&mut self, client_id: ClientId, channel_id: u16) {
        let old_channel = self.channel_of.get(&client_id).copied();
        if old_channel == Some(channel_id) {
            return;
        }
        info!(
            "{} moved to channel '{}'",
            self.get_username(client_id),
            self.channel_infos
                .get(usize::from(channel_id))
                .map_or("?", |c| c.name.as_str()),
        );
        let lobby = self.default_channel;
        let was_visible = old_channel.is_some_and(|c| c != lobby);
        self.channel_of.insert(client_id, channel_id);
        match (was_visible, channel_id != lobby) {
            (false, true) => self.announce(client_id, channel_id),
            (true, false) => self.withdraw(client_id),
            _ => self.broadcast_msg(&ServerMessage::UserMovedChannel {
                client_id,
                channel_id,
            }),
        }
        self.revalidate_shares_after_move(client_id, old_channel, channel_id);
    }

    fn deny_join(&mut self, client_id: ClientId, channel_id: u16, reason: ChannelDenyReason) {
        self.send_msg(
            client_id,
            &ServerMessage::JoinChannelDenied { channel_id, reason },
        );
    }

    /// Members currently in `channel_id`.
    fn members_in(&self, channel_id: u16) -> usize {
        self.channel_of
            .values()
            .filter(|c| **c == channel_id)
            .count()
    }

    /// Admitted clients in `channel` (None never matches anyone).
    fn peers_in_channel(&self, channel: Option<u16>) -> Vec<ClientId> {
        self.clients()
            .into_iter()
            .filter(|peer| channel.is_some() && self.channel_of.get(peer).copied() == channel)
            .collect()
    }

    /// Unsubscribe every viewer of `sharer` who is not in `channel`,
    /// telling each their view of the stream ended.
    fn evict_out_of_channel_viewers(&mut self, sharer: ClientId, channel: Option<u16>) {
        let stopped = ServerMessage::ScreenShareStopped { client_id: sharer };
        for viewer in self.video.subscribers_of(sharer) {
            if self.channel_of.get(&viewer).copied() != channel {
                self.video
                    .unsubscribe(&mut self.driver, self.now, viewer, sharer);
                self.send_msg(viewer, &stopped);
            }
        }
    }

    /// Channel-scoped share visibility follows membership. After
    /// `mover` goes `old` → `new`:
    /// - if the mover is sharing channel-scoped: the stream leaves the
    ///   old channel (badge cleared, viewers evicted) and appears in
    ///   the new one;
    /// - any channel-scoped stream the mover was watching or could see
    ///   in the old channel disappears for them, and streams in the new
    ///   channel are announced to them.
    ///
    /// Everyone-scoped streams are unaffected throughout.
    fn revalidate_shares_after_move(&mut self, mover: ClientId, old: Option<u16>, new: u16) {
        // ── The mover as a sharer ───────────────────────────────────
        if let Some(info) = self.video.share_info(mover)
            && info.scope == ShareScope::Channel
        {
            // Old channel: stream gone (this also reaches evicted
            // viewers, closing their windows).
            let stopped = ServerMessage::ScreenShareStopped { client_id: mover };
            for peer in self.peers_in_channel(old) {
                if peer != mover {
                    self.send_msg(peer, &stopped);
                }
            }
            self.evict_out_of_channel_viewers(mover, Some(new));
            // New channel: stream appears.
            let started = ServerMessage::ScreenShareStarted {
                client_id: mover,
                width: info.width,
                height: info.height,
                scope: info.scope,
            };
            for peer in self.peers_in_channel(Some(new)) {
                if peer != mover {
                    self.send_msg(peer, &started);
                }
            }
        }

        // ── The mover as a viewer / potential viewer ────────────────
        let sharers: Vec<(ClientId, ShareInfo)> = self
            .video
            .sharers()
            .iter()
            .map(|(id, info)| (*id, *info))
            .collect();
        for (sharer, info) in sharers {
            if sharer == mover || info.scope != ShareScope::Channel {
                continue;
            }
            let sharer_channel = self.channel_of.get(&sharer).copied();
            if sharer_channel == Some(new) {
                // Snapshot semantics, like connecting: a stream lives
                // in the channel the mover just entered.
                self.send_msg(
                    mover,
                    &ServerMessage::ScreenShareStarted {
                        client_id: sharer,
                        width: info.width,
                        height: info.height,
                        scope: info.scope,
                    },
                );
            } else if sharer_channel == old {
                // The mover left this stream behind.
                if self.video.is_subscribed(mover, sharer) {
                    self.video
                        .unsubscribe(&mut self.driver, self.now, mover, sharer);
                }
                self.send_msg(
                    mover,
                    &ServerMessage::ScreenShareStopped { client_id: sharer },
                );
            }
        }
    }

    fn handle_subscribe(&mut self, client_id: ClientId, target_id: ClientId) {
        let Some(info) = self.video.share_info(target_id) else {
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::NotSharing,
                },
            );
            return;
        };

        // Channel-scoped streams are watchable only from the sharer's
        // channel. (The stream is normally invisible from elsewhere —
        // this closes the modified-client path.)
        if info.scope == ShareScope::Channel
            && self.channel_of.get(&client_id) != self.channel_of.get(&target_id)
        {
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::NotInChannel,
                },
            );
            return;
        }

        // Admission control: cap viewers per stream. Worst-case egress
        // is max_sharers × max_viewers_per_stream × bitrate — the knob
        // that actually protects the uplink.
        if !self.video.is_subscribed(client_id, target_id)
            && self.video.viewer_count(target_id) >= self.config.limits.max_viewers_per_stream
            && !self.has_permission(client_id, permissions::BYPASS_LIMITS)
        {
            info!(
                "Subscribe denied for {client_id:#x} → {target_id:#x}: viewer limit {} reached",
                self.config.limits.max_viewers_per_stream
            );
            self.send_msg(
                client_id,
                &ServerMessage::SubscribeDenied {
                    target_id,
                    reason: DenyReason::ViewerLimitReached,
                },
            );
            return;
        }

        self.video.subscribe(client_id, target_id);

        // Kick the sharer for an IDR so the new viewer gets a first
        // picture in one RTT instead of waiting out the keyframe
        // interval. Same gate as explicit requests.
        if self.video.allow_keyframe_request(self.now, target_id) {
            self.send_keyframe_request(target_id, client_id);
        }
    }

    // ── Reports ─────────────────────────────────────────────────────

    /// Sample viewer health per stream and report to sharers.
    fn send_stream_health(&mut self) {
        let reports = self.video.stream_health(&self.driver);
        for report in reports {
            self.send_msg(
                report.sharer,
                &ServerMessage::StreamHealth {
                    worst_loss: report.worst_loss,
                    worst_rtt_ms: report.worst_rtt_ms,
                    viewers: report.viewers,
                },
            );
        }
    }

    /// Every two seconds, tell each client what the relay actually
    /// received of its voice (spec §6.1) — the only loss figure its
    /// audio ladder may act on — and, when enabled, log the `link:`
    /// line for the same interval.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn send_voice_reports(&mut self) {
        let link_interval = Duration::from_secs(self.config.logging.link_stats_interval_secs);
        let logging = !link_interval.is_zero();
        let since_line = self.now.duration_since(self.last_link_log);
        let log_due = logging && since_line >= link_interval;
        if log_due {
            self.last_link_log = self.now;
        }
        // The line's own interval: its rates and counts cover the time
        // since the last line. They used to divide by this 2 s report
        // interval against a baseline moved every report, so a 10 s
        // line described its last 2 s (review P3-7).
        let interval_secs = since_line
            .as_secs_f64()
            .max(VOICE_REPORT_INTERVAL.as_secs_f64());

        for client_id in self.clients() {
            let Some(stats) = self.driver.stats(client_id) else {
                continue;
            };
            let voice = self.voice.seq.take_window(client_id).unwrap_or_default();
            let rtt_ms = stats.rtt.as_millis().min(u128::from(u32::MAX)) as u32;
            // Nothing of its voice was due: a report of nothing, to a
            // session that said it can do without (`QUIET_REPORTS`), is
            // a packet every two seconds for as long as a phone sits in
            // the lobby — and its radio never rests (review P2-49).
            let quiet = voice.expected == 0
                && voice.received == 0
                && self.has_feature(client_id, features::QUIET_REPORTS);
            if !quiet {
                self.send_msg(
                    client_id,
                    &ServerMessage::VoiceReceiverReport {
                        received: voice.received,
                        expected: voice.expected,
                        lost: voice.lost(),
                        late: voice.late,
                        rtt_ms,
                    },
                );
            }
            if logging {
                self.link_voice.entry(client_id).or_default().add(&voice);
            }

            if log_due {
                let voice = self.link_voice.remove(&client_id).unwrap_or_default();
                let base = self
                    .link_baseline
                    .get(&client_id)
                    .copied()
                    .unwrap_or_default();
                let username = self.get_username(client_id);
                let tx_kbit = (stats.bytes_tx.saturating_sub(base.bytes_tx)) as f64 * 8.0
                    / 1000.0
                    / interval_secs;
                let rx_kbit = (stats.bytes_rx.saturating_sub(base.bytes_rx)) as f64 * 8.0
                    / 1000.0
                    / interval_secs;
                let sup = stats
                    .groups_reset_superseded
                    .saturating_sub(base.groups_reset_superseded);
                let dl = stats
                    .groups_reset_deadline
                    .saturating_sub(base.groups_reset_deadline);
                let ack = stats
                    .groups_reset_unacked
                    .saturating_sub(base.groups_reset_unacked);
                let upstream = stats
                    .groups_reset_upstream
                    .saturating_sub(base.groups_reset_upstream);
                let behind = stats.groups_reset.saturating_sub(base.groups_reset);
                let cause = if behind > 0 || upstream > 0 {
                    format!(
                        " (superseded {sup}, deadline {dl}, unacked {ack}, upstream {upstream})"
                    )
                } else {
                    String::new()
                };
                info!(
                    "link: {username} ({client_id:#x}): rtt {rtt_ms} ms, cwnd {} KB, path loss {:.1}%, \
                     tx {tx_kbit:.0} kbit/s, rx {rx_kbit:.0} kbit/s, mtu {}, dgram drops {}, \
                     groups behind {behind}/{}{cause} | voice rx {} pkt, expected {}, lost {} ({:.1}%), late {}{}",
                    stats.cwnd / 1024,
                    stats.path_loss_pct_since(&base),
                    stats.current_mtu,
                    stats.datagram_send_drops - base.datagram_send_drops,
                    stats.groups_started - base.groups_started,
                    voice.received,
                    voice.expected,
                    voice.lost(),
                    voice.loss_pct(),
                    voice.late,
                    if voice.resyncs > 0 {
                        format!(", resyncs {}", voice.resyncs)
                    } else {
                        String::new()
                    },
                );
                self.link_baseline.insert(client_id, stats);
            }
        }
    }

    /// Forward a keyframe request to `sharer` on behalf of `viewer`.
    fn send_keyframe_request(&mut self, sharer: ClientId, viewer: ClientId) {
        debug!("Forwarding keyframe request: viewer {viewer:#x} → sharer {sharer:#x}");
        self.send_msg(
            sharer,
            &ServerMessage::KeyframeRequested { viewer_id: viewer },
        );
    }

    // ── Utilities ───────────────────────────────────────────────────

    fn get_username(&self, client_id: ClientId) -> String {
        self.usernames
            .get(&client_id)
            .cloned()
            .unwrap_or_else(|| format!("User{client_id:x}"))
    }

    #[allow(clippy::cast_precision_loss)]
    fn print_stats(&self) {
        info!("=== {} Statistics ===", self.config.server.name);
        info!(
            "Connected clients: {} (admitted {}, refused {}, transport-refused {}, \
             address checks {})",
            self.usernames.len(),
            self.stats.admitted,
            self.stats.refused,
            self.driver.refused_count(),
            self.driver.retried_count(),
        );
        info!("Text messages: {}", self.stats.messages_count);
        info!("{}", self.persist.stats());

        self.voice.print_stats();
        self.video.print_stats();

        info!(
            "Control data sent: {:.2} MB, received: {:.2} MB",
            self.stats.bytes_sent as f64 / 1_048_576.0,
            self.stats.bytes_received as f64 / 1_048_576.0
        );

        for client_id in self.clients() {
            if let Some(stats) = self.driver.stats(client_id) {
                let username = self.get_username(client_id);
                info!(
                    "  {username} ({client_id:#x}): rtt {} ms, cwnd {} KB, lost {}/{} pkt, tx {:.2} MB, rx {:.2} MB, groups behind {}/{} (superseded {}, deadline {}, unacked {}, upstream {})",
                    stats.rtt.as_millis(),
                    stats.cwnd / 1024,
                    stats.lost_packets,
                    stats.sent_packets,
                    stats.bytes_tx as f64 / 1_048_576.0,
                    stats.bytes_rx as f64 / 1_048_576.0,
                    stats.groups_reset,
                    stats.groups_started,
                    stats.groups_reset_superseded,
                    stats.groups_reset_deadline,
                    stats.groups_reset_unacked,
                    stats.groups_reset_upstream,
                );
            }
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Outcome of folding one chunk into an upload.
enum Folded {
    Complete(Vec<u8>),
    Malformed,
    Oversized(usize),
}

/// Fold one chunk into `uploads[client]`, creating the entry on a fresh
/// `upload_id` (which abandons any older partial). `None` while the
/// upload is still incomplete (or the chunk was a duplicate).
#[allow(clippy::too_many_arguments)]
fn fold_upload(
    uploads: &mut HashMap<ClientId, IncomingUpload>,
    client_id: ClientId,
    upload_id: u32,
    index: u32,
    count: u32,
    data: Vec<u8>,
    max_chunks: u32,
    max_bytes: usize,
    now: Instant,
) -> Option<Folded> {
    if count == 0
        || count > max_chunks
        || index >= count
        || (count as usize - 1) * protocol::IMAGE_CHUNK_BYTES > max_bytes
    {
        return Some(Folded::Malformed);
    }

    let stale = uploads
        .get(&client_id)
        .is_some_and(|u| u.upload_id != upload_id);
    if stale {
        uploads.remove(&client_id);
    }
    let upload = uploads.entry(client_id).or_insert_with(|| IncomingUpload {
        upload_id,
        count,
        received: 0,
        bytes: 0,
        chunks: vec![None; count as usize],
        started: now,
    });
    if upload.count != count || upload.chunks[index as usize].is_some() {
        return None; // inconsistent or duplicate — drop silently
    }
    upload.bytes += data.len();
    if upload.bytes > max_bytes {
        let bytes = upload.bytes;
        uploads.remove(&client_id);
        return Some(Folded::Oversized(bytes));
    }
    upload.chunks[index as usize] = Some(data);
    upload.received += 1;
    if upload.received < upload.count {
        return None;
    }

    let upload = uploads.remove(&client_id)?;
    let mut whole = Vec::with_capacity(upload.bytes);
    for chunk in upload.chunks {
        whole.extend_from_slice(&chunk?);
    }
    Some(Folded::Complete(whole))
}

/// One-line rendering of a telemetry sample for the log.
#[allow(clippy::format_push_string)] // a log line; readability over allocation
fn summarize_telemetry(s: &TelemetrySample) -> String {
    let mut out = format!(
        "v{} {}/{}{} | path rtt {} ms, loss {:.1}%, cwnd {} KB, up {:.0} kbit/s, down {:.0} kbit/s, dgram drops {}, blocked {}",
        s.client.version,
        s.client.os,
        s.client.arch,
        if s.client.portable { " portable" } else { "" },
        s.path.rtt_ms,
        s.path.loss_pct,
        s.path.cwnd / 1024,
        s.path.kbps_up,
        s.path.kbps_down,
        s.path.datagram_send_drops,
        s.path.datagrams_blocked,
    );
    if let Some(a) = &s.audio {
        out.push_str(&format!(
            " | audio {} kbit/s {} {} ms{} fec {}% loss {:.1}%{} sent {} enc {} µs over/under {}/{} in '{}' out '{}' | rx {} lost {} fec {} red {} plc {} jitter {}/{} ms dec {} µs",
            a.bitrate_bps / 1000,
            a.band,
            a.frame_ms,
            if a.red { " RED" } else { "" },
            a.fec_hint_pct,
            a.smoothed_loss_pct,
            if a.exercised { "" } else { " (idle)" },
            a.packets_sent,
            a.encode_us,
            a.input_overruns,
            a.output_underruns,
            a.input_device,
            a.output_device,
            a.packets_received,
            a.packets_lost,
            a.fec_recoveries,
            a.red_recoveries,
            a.plc_frames,
            a.jitter_ms,
            a.jitter_target_ms,
            a.decode_us,
        ));
    }
    if let Some(v) = &s.sharing {
        out.push_str(&format!(
            " | sharing {} ({}) rung {}% fps {:.1} scale {}% forced IDR {} kf req {} groups reset {}",
            v.preset,
            v.encoder,
            v.rung_pct,
            v.fps_encoded,
            v.fps_scale_pct,
            v.forced_keyframes,
            v.keyframe_requests,
            v.groups_reset,
        ));
    }
    for v in &s.viewing {
        out.push_str(&format!(
            " | viewing {:#x}: decoded {} dropped {} kf req {} dec {} µs",
            v.sharer, v.frames_decoded, v.frames_dropped, v.keyframe_requests_sent, v.decode_us,
        ));
    }
    out
}

/// Short stable name of an event, for the `kind` column.
fn event_kind(e: &TelemetryEvent) -> &'static str {
    match e {
        TelemetryEvent::RateRungChanged { .. } => "rate_rung_changed",
        TelemetryEvent::IdentityPromptShown => "identity_prompt_shown",
        TelemetryEvent::DeviceChanged { .. } => "device_changed",
        TelemetryEvent::InputDiscontinuityBurst { .. } => "input_discontinuity_burst",
        TelemetryEvent::SendBufferPressure => "send_buffer_pressure",
        TelemetryEvent::ShareStarted { .. } => "share_started",
        TelemetryEvent::ShareStopped => "share_stopped",
        TelemetryEvent::Disconnected { .. } => "disconnected",
    }
}

fn describe_event(e: &TelemetryEvent) -> String {
    match e {
        TelemetryEvent::RateRungChanged {
            level,
            bitrate_bps,
            red,
        } => format!(
            "audio rung → {level} ({} kbit/s{})",
            bitrate_bps / 1000,
            if *red { ", RED" } else { "" }
        ),
        TelemetryEvent::IdentityPromptShown => "identity prompt shown".to_string(),
        TelemetryEvent::DeviceChanged { description } => format!("device changed: {description}"),
        TelemetryEvent::InputDiscontinuityBurst { count } => {
            format!("input discontinuity burst ({count})")
        }
        TelemetryEvent::SendBufferPressure => "send buffer pressure".to_string(),
        TelemetryEvent::ShareStarted { preset, encoder } => {
            format!("share started ({preset}, {encoder})")
        }
        TelemetryEvent::ShareStopped => "share stopped".to_string(),
        TelemetryEvent::Disconnected { reason } => format!("disconnected: {reason}"),
    }
}

/// Nobody is admin — always with persistence off, only on an empty
/// database with it on — so mint a one-shot admin code and print it
/// (spec §1.1). Whoever runs the server has the log; nobody else does.
fn mint_bootstrap_invite(
    policy: &mut Policy,
    now: WallMs,
    endpoint: &str,
    fingerprint: &Fingerprint,
) -> Option<String> {
    if policy.has_admin() {
        return None;
    }
    let code = policy::generate_invite_code();
    policy.add_invite(
        code.clone(),
        InviteRecord {
            created_by: None,
            created_at: now,
            expires_at: Some(now + BOOTSTRAP_INVITE_TTL_MS),
            max_uses: Some(1),
            uses: 0,
            grants_role: ROLE_ADMIN,
            revoked_at: None,
        },
    );
    warn!(
        "No admin yet. Bootstrap invite (admin, 1 use, 1 h): code {} for {endpoint} \
         (identity {fingerprint})",
        protocol::invite::display_code(&code)
    );
    Some(code)
}

/// A wall-clock millisecond stamp as `2026-09-08 14:06`, for audit lines.
fn wall_stamp(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(i64::try_from(ms).unwrap_or(0)).map_or_else(
        || ms.to_string(),
        |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// OS send-buffer target for the relay socket: the worst burst is one
/// full-size chat image (up to [`protocol::MAX_CHAT_IMAGE_BYTES`],
/// 10 MiB) fanned out to every client at once, so scale with
/// `max_clients`. Floor: 8 MiB. Ceiling: 64 MiB — the buffer doesn't
/// have to hold the entire fan-out, only the gap between enqueue and
/// NIC drain; past 64 MiB, per-connection pacing spreads the rest.
fn send_buffer_target(max_clients: usize) -> usize {
    (max_clients * protocol::MAX_CHAT_IMAGE_BYTES).clamp(8 * 1024 * 1024, 64 * 1024 * 1024)
}

#[cfg(test)]
mod wake_tests {
    use std::time::{Duration, Instant};

    use transport::ServerIdentity;

    use super::{ChatServer, ASK_POLL, MAX_IDLE_WAIT};
    use crate::{config::Config, persistence::Query};

    /// An idle server sleeps until its next periodic job, not on a
    /// millisecond tick; a database read out shortens the sleep to the
    /// poll that picks its answer up (review P1-2).
    #[test]
    fn an_idle_server_sleeps_until_its_next_job() {
        let mut config = Config::default();
        config.persistence.enabled = false;
        let mut server = ChatServer::new(config, &ServerIdentity::generate()).expect("server");
        let now = Instant::now();
        server.update(now);
        let wait = server.next_wake().saturating_duration_since(now);
        assert!(
            wait >= Duration::from_millis(500) && wait <= MAX_IDLE_WAIT,
            "an idle server wakes in {wait:?}"
        );

        let _ = server.persistence_mut().ask(
            Query::Audit {
                before: None,
                limit: 1,
            },
            now + Duration::from_secs(5),
        );
        let wait = server.next_wake().saturating_duration_since(now);
        assert!(wait <= ASK_POLL, "with a read out it wakes in {wait:?}");
        server.update(Instant::now());
        let wait = server.next_wake().saturating_duration_since(now);
        assert!(wait > ASK_POLL, "the answer was collected: {wait:?}");
    }
}

/// Connections one address may hold in the waiting room at once: a
/// household behind one NAT reconnecting together after a restart
/// fits; one host opening connections in a loop does not fill the room.
const WAITING_PER_ADDRESS: usize = 8;

/// The seats for connections that have not said `Hello` yet, beside
/// `max_clients` for the people who are in. The transport used to
/// count both against `max_clients`: a full server refused the next
/// handshake outright, so `ServerFull` never reached anyone, a user
/// returning after a blackout could not replace their own stale
/// session, and connections that never spoke took the seats of people
/// (review P3-5, plan SEC-2). A quarter again, between 16 and 64.
fn waiting_room(max_clients: usize) -> WaitingRoom {
    WaitingRoom {
        seats: (max_clients / 4).clamp(16, 64),
        per_address: WAITING_PER_ADDRESS,
    }
}

/// What failed admissions are counted under: an IPv4 address as it is,
/// an IPv6 address by its /64. One subscriber holds a whole /64, so
/// counting each address apart gave a guesser a fresh allowance per
/// address and the table one entry per address tried (review P2-7).
fn failure_source(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(
            || IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
            IpAddr::V4,
        ),
    }
}

#[cfg(test)]
mod failure_source_tests {
    use std::net::IpAddr;

    use super::failure_source;

    #[test]
    fn ipv6_counts_by_its_64_and_ipv4_by_address() {
        let ip = |s: &str| s.parse::<IpAddr>().expect("address");
        assert_eq!(
            failure_source(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            failure_source(ip("2001:db8:1:2::1")),
        );
        assert_ne!(
            failure_source(ip("2001:db8:1:2::1")),
            failure_source(ip("2001:db8:1:3::1")),
        );
        assert_eq!(failure_source(ip("192.0.2.7")), ip("192.0.2.7"));
        assert_ne!(
            failure_source(ip("192.0.2.7")),
            failure_source(ip("192.0.2.8"))
        );
        // A v4 client on a dual-stack socket is still that v4 address.
        assert_eq!(failure_source(ip("::ffff:192.0.2.7")), ip("192.0.2.7"));
    }
}

#[cfg(test)]
mod reload_tests {
    use std::time::{Duration, Instant, SystemTime};

    use transport::ServerIdentity;

    use super::{ChatServer, CONFIG_CHECK_INTERVAL};
    use crate::config::Config;

    fn yaml(channels: &[&str]) -> String {
        let mut yaml = String::from(
            "persistence:\n  enabled: false\nuser_channels:\n  afk_channel: AFK\nchannels:\n",
        );
        for name in channels {
            yaml.push_str("  - name: ");
            yaml.push_str(name);
            yaml.push('\n');
        }
        yaml
    }

    fn names(server: &ChatServer) -> Vec<String> {
        server
            .config
            .channels
            .iter()
            .map(|ch| ch.name.clone())
            .collect()
    }

    /// A saved file the next start would refuse is not applied to the
    /// running server (review P2-4); a valid one is.
    #[test]
    fn a_reload_the_next_start_would_refuse_is_not_applied() {
        let dir = std::env::temp_dir().join(format!(
            "baucord-reload-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("server.yaml");
        std::fs::write(&path, yaml(&["General", "AFK"])).expect("write");
        let config = Config::from_file(&path).expect("config");
        let mut server = ChatServer::new(config, &ServerIdentity::generate()).expect("server");
        server.watch_config_file(path.clone());
        let mut now = Instant::now();
        server.update(now);
        let mut mtime = SystemTime::now();
        let mut save = |server: &mut ChatServer, channels: &[&str]| {
            std::fs::write(&path, yaml(channels)).expect("write");
            // A fresh mtime whatever the filesystem's granularity.
            mtime += Duration::from_secs(2);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(mtime))
                .expect("mtime");
            now += CONFIG_CHECK_INTERVAL + Duration::from_millis(1);
            server.update(now);
        };

        // The AFK room removed while `afk_channel` still names it.
        save(&mut server, &["General", "Games"]);
        assert_eq!(names(&server), ["General", "AFK"], "kept as it was");

        save(&mut server, &["General", "Games", "AFK"]);
        assert_eq!(names(&server), ["General", "Games", "AFK"], "applied");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod socket_tests {
    use super::send_buffer_target;

    #[test]
    fn send_buffer_scales_with_clients() {
        // A solo room keeps the 8 MiB floor.
        assert_eq!(send_buffer_target(0), 8 * 1024 * 1024);
        // Images are 10 MiB units: even a 4-user room wants 40 MiB.
        assert_eq!(send_buffer_target(4), 40 * 1024 * 1024);
        // 12 users — the room size that broke for real — hits the
        // ceiling, as does anything larger.
        assert_eq!(send_buffer_target(12), 64 * 1024 * 1024);
        assert_eq!(send_buffer_target(64), 64 * 1024 * 1024);
    }
}
