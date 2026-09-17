//! Server configuration.
//!
//! Deliberately absent compared to earlier versions: `protocol_id` and
//! any channel IDs/buffer sizes. Those are wire-protocol facts owned by
//! the `protocol` crate — making them configurable only created ways for
//! a deployment to disagree with every client. What remains configurable
//! is genuinely deployment-specific: addresses, capacity, limits, auth.

use std::{fs, net::SocketAddr, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerSettings,
    pub network: NetworkSettings,
    pub limits: LimitSettings,
    /// Legacy (1.0). Still parsed so old files load; never written.
    #[serde(skip_serializing)]
    pub authentication: AuthSettings,
    pub identity: IdentitySettings,
    pub persistence: PersistenceSettings,
    pub logging: LoggingSettings,
    /// Voice channels (rooms). Omitting the key gives a single open
    /// "Lobby" default channel, so pre-channels configs keep working.
    /// This list is the hot-reloaded *base*; channels people create
    /// from the client live in the database (spec §5).
    pub channels: Vec<ChannelSettings>,
    /// Policy for channels created from the client.
    pub user_channels: UserChannelSettings,
}

/// One voice channel (room). Designed to grow: every field except
/// `name` is `#[serde(default)]`, so future per-channel options (topic,
/// bitrate caps, ACLs…) can be added without breaking existing YAML.
///
/// `PartialEq` backs hot-reload: the watcher only re-applies when the
/// channel list actually changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSettings {
    pub name: String,
    /// The channel new clients land in on connect. Exactly one channel
    /// must set this, and it may carry neither `password` nor
    /// `user_limit` — everyone must be able to land there.
    #[serde(default)]
    pub default: bool,
    /// Optional join password. Never sent to clients — they only learn
    /// that one is required.
    #[serde(default)]
    pub password: Option<String>,
    /// Maximum members; absent = unlimited.
    #[serde(default)]
    pub user_limit: Option<u16>,
}

/// Hard cap on configured channels: keeps the `Init` payload bounded
/// and the sidebar sane. (Wire ids are u16, so the format allows more;
/// this is policy, not protocol.)
pub const MAX_CHANNELS: usize = 256;

fn default_channels() -> Vec<ChannelSettings> {
    vec![ChannelSettings {
        name: "General".to_string(),
        default: false,
        password: None,
        user_limit: None,
    }]
}

/// The lobby's name on the wire. Empty: a config channel cannot be
/// empty-named, so it never collides, and a client that knows the
/// lobby (feature bit `LOBBY`) hides the row by this name.
pub const LOBBY_NAME: &str = "";

/// A channel list with the lobby appended (spec §12, 1.1.5): the
/// room everyone connects into, announced to nobody, not in the
/// config. Always last, so config and UI channels keep their ids.
#[must_use]
pub fn with_lobby(channels: &[ChannelSettings]) -> Vec<ChannelSettings> {
    let mut all = channels.to_vec();
    all.push(ChannelSettings {
        name: LOBBY_NAME.to_string(),
        default: false,
        password: None,
        user_limit: None,
    });
    all
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub name: String,
    /// Address to bind the UDP socket on.
    pub host: String,
    pub port: u16,
    /// The address clients reach this server at (`"host:port"`). Only
    /// used to print the invite string (`host:port#SHA256:…`) at
    /// startup; the transport itself does not need it.
    pub public_address: Option<String>,
    pub greeting_message: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    /// Connections admitted at once; the transport refuses the rest
    /// during the handshake. (`bytes_per_tick` from the 0.5 line is
    /// gone: QUIC's congestion controller and the per-lane budgets
    /// replace it, and an old key is ignored.)
    pub max_clients: usize,
}

/// Admission control. These are the real bandwidth ceiling: worst-case
/// server egress ≈ `max_sharers × max_viewers_per_stream × stream bitrate`
/// (plus voice, which is negligible). Defaults: 8 × 16 × ~4 Mbit/s ≈
/// 512 Mbit/s at full tilt — a hosted relay's uplink; size these to
/// yours instead of discovering the limit live mid-movie-night. Admins
/// bypass them (`BYPASS_LIMITS`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitSettings {
    /// Concurrent screen shares allowed.
    pub max_sharers: usize,
    /// Viewers allowed per stream.
    pub max_viewers_per_stream: usize,
    /// Chat messages longer than this (bytes) are truncated.
    pub max_chat_bytes: usize,
    /// Per-client *transfer* budget: KiB of accepted uploads — files,
    /// pictures, and the legacy chat-image relay — per sliding minute.
    /// Bytes are the resource: a 4 MiB picture costs as much as forty
    /// 100 KiB ones. Uploads that would cross the budget are refused
    /// outright; `0` disables uploads entirely. (Was
    /// `max_image_kb_per_minute`; the old name still reads.)
    #[serde(alias = "max_image_kb_per_minute")]
    pub max_transfer_kb_per_minute: usize,
    /// Server-wide transfer budget, KiB per sliding minute, over all
    /// clients; `0` disables uploads entirely. (Was
    /// `max_image_kb_per_minute_global`.)
    #[serde(alias = "max_image_kb_per_minute_global")]
    pub max_transfer_kb_per_minute_global: usize,
    /// Relay pacing for chat images, KiB per second *per receiver*.
    /// Images are re-chunked and trickled out at this rate so a big
    /// image never floods a receiver's downlink queue — that queue is
    /// shared with voice, and flooding it is what made calls stutter
    /// during uploads. Values below 64 are treated as 64. At the
    /// default 1024, a 2.5 MB image takes ~2.5 s per receiver.
    pub media_relay_kb_per_sec: usize,
    /// Per-user chat rate limit: at most `burst` messages in any
    /// `per_secs` window. A person typing fast fits; a paste loop
    /// does not.
    pub chat: ChatLimit,
}

/// Sliding-window chat rate limit (spec §1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatLimit {
    pub burst: u32,
    pub per_secs: u64,
}

impl Default for ChatLimit {
    fn default() -> Self {
        Self {
            burst: 5,
            per_secs: 5,
        }
    }
}

/// The 1.0 password door. Admission is by invite and device key
/// since 1.1 (spec §3); this section is read for compatibility and
/// warned about.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// 1.0: clients had to send this shared password. Ignored since
    /// 1.1 (with a warning at startup) — invites are the only door.
    pub secure: bool,
    pub password: Option<String>,
}

/// Where the server keeps its Ed25519 identity key and certificate.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentitySettings {
    /// Directory for `identity.key`, `identity.pem`, `retired/` and
    /// uploaded client logs (`logs/`). Created on first start. Keep it
    /// on persistent storage: the key *is* the server's identity, and
    /// every client pins it.
    pub dir: String,
}

/// The `SQLite` store (spec §1). `enabled: false` keeps everything in
/// memory for the session: roles, invites, bans and channels still
/// work until the process exits, and history, DMs, files and the
/// admin CLI do not exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistenceSettings {
    pub enabled: bool,
    /// The database file. Blobs live in a `blobs/` directory next to
    /// it. Keep both with the identity key: `server backup` archives
    /// them together.
    pub path: String,
    pub history: HistorySettings,
    pub files: FileSettings,
    pub avatars: AvatarSettings,
    pub telemetry: TelemetryRetention,
    /// Depth of the write queue between the relay tick and the
    /// persistence thread. Writes past it are dropped and counted
    /// rather than ever blocking the tick. Not written to the config
    /// file: tests lower it to provoke drops; leave the default.
    #[serde(skip_serializing)]
    pub write_queue: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    /// General-chat messages replayed to a joining client.
    pub replay: u16,
    /// Messages older than this are pruned hourly.
    pub retain_days: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct FileSettings {
    /// Per-file size cap.
    pub max_mb: u32,
    /// Per-user upload volume per day.
    pub per_user_daily_mb: u32,
    /// Total blob storage; oldest unreferenced blobs are evicted
    /// first, then uploads are refused.
    pub total_cap_gb: u32,
    /// Unreferenced blobs older than this are deleted.
    pub retain_days: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct AvatarSettings {
    pub max_kb: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryRetention {
    /// Telemetry rows older than this are pruned hourly.
    pub retain_days: u32,
}

/// Policy for channels created from the client (spec §5). The base
/// list stays in `channels:` and hot-reloads as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserChannelSettings {
    /// UI-created channels one organizer may own at a time (admins are
    /// exempt).
    pub max_per_user: u16,
    /// A UI-created channel empty for this long is deleted unless its
    /// creator marked it permanent.
    pub temporary_after_min: u32,
    /// The config channel idle users are moved to. Omit to disable
    /// auto-AFK; when set, it must name a channel in `channels:`.
    pub afk_channel: Option<String>,
    /// Default idle timeout before the move; a channel may set its own.
    pub afk_after_min: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingSettings {
    pub level: String,
    pub stats_interval_secs: u64,
    /// One `link:` line per connected client every this many seconds
    /// (0 = off): the relay's view of that client's link — QUIC path
    /// RTT, congestion window, transport-level loss, bytes in/out, video
    /// groups abandoned because that client fell behind (with the cause:
    /// superseded by the next keyframe, deadline, unacknowledged finish —
    /// `upstream` resets mirror the sharer's own and are not the
    /// client's fault), and the voice packets received vs. the
    /// sender's sequence numbers (what listeners actually miss). Meant
    /// to be lined up by timestamp against the client's own `link:`
    /// lines and its `telemetry:` samples. ~9 clients at 2 s is a few
    /// hundred lines a minute; raise it or set 0 outside diagnosis.
    pub link_stats_interval_secs: u64,
    /// Log the periodic `telemetry:` samples clients send (§9.1). Off
    /// still accepts the messages; they are just not written out.
    pub telemetry: bool,
    /// Log file, rotated at 5 MiB, in addition to stderr. Empty
    /// disables it — the right setting in a container, where the
    /// runtime captures stderr and the working directory need not be
    /// writable. An unwritable path is a warning, never a failure.
    pub file: String,
}

impl Config {
    /// # Errors
    /// Unreadable file or invalid YAML.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&contents)?)
    }

    /// Write this config to a YAML file.
    ///
    /// # Errors
    /// Serialization or write failure.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn std::error::Error>> {
        let contents = serde_yaml::to_string(self)?;
        fs::write(path, contents)?;
        Ok(())
    }

    /// # Errors
    /// `server.host` is not an IP address.
    pub fn socket_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.server.host, self.server.port).parse()
    }

    /// Check the limits and user-channel policy for values that would
    /// silently disable a feature or divide by zero.
    ///
    /// # Errors
    /// A human-readable description of the first problem found.
    pub fn validate_policy(&self) -> Result<(), String> {
        let chat = self.limits.chat;
        if chat.burst == 0 || chat.per_secs == 0 {
            return Err(format!(
                "limits.chat: burst {} per {} s would refuse every message — \
                 both must be at least 1",
                chat.burst, chat.per_secs
            ));
        }
        if self.user_channels.afk_after_min == 0 {
            return Err("user_channels.afk_after_min must be at least 1".to_string());
        }
        if self.user_channels.temporary_after_min == 0 {
            return Err("user_channels.temporary_after_min must be at least 1".to_string());
        }
        if let Some(afk) = self.user_channels.afk_channel.as_deref() {
            let afk = afk.trim();
            if !self.channels.iter().any(|ch| ch.name.trim() == afk) {
                return Err(format!(
                    "user_channels.afk_channel '{afk}' is not in the channels list — \
                     add it there, or omit the key to disable auto-AFK"
                ));
            }
        }
        if self.persistence.enabled && self.persistence.path.trim().is_empty() {
            return Err("persistence.path is empty".to_string());
        }
        if self.persistence.write_queue == 0 {
            return Err("persistence.write_queue must be at least 1".to_string());
        }
        Ok(())
    }

    /// The `host:port` clients should dial, for the invite string.
    #[must_use]
    pub fn public_endpoint(&self) -> String {
        self.server.public_address.clone().unwrap_or_else(|| {
            let host = if self.server.host == "0.0.0.0" || self.server.host == "::" {
                "<public-ip>".to_string()
            } else {
                self.server.host.clone()
            };
            format!("{host}:{}", self.server.port)
        })
    }

    /// Validate the channel list and return the lobby's index (the
    /// entry [`with_lobby`] appends).
    ///
    /// Enforced here (at startup, loudly) rather than tolerated at join
    /// time: empty/duplicate names, `user_limit: 0`, and more than
    /// [`MAX_CHANNELS`] channels are all config mistakes the operator
    /// should fix, not states the server should limp through.
    ///
    /// # Errors
    /// A human-readable description of the first problem found.
    pub fn validate_channels(&self) -> Result<u16, String> {
        validate_channel_list(&self.channels)
    }

    /// The channel list as announced to clients, lobby included
    /// (passwords reduced to a `has_password` flag; they never cross
    /// the wire).
    #[must_use]
    pub fn channel_infos(&self) -> Vec<protocol::ChannelInfo> {
        channel_infos_of(&with_lobby(&self.channels))
    }
}

/// The wire form of a channel list: ids are positions.
#[must_use]
pub fn channel_infos_of(channels: &[ChannelSettings]) -> Vec<protocol::ChannelInfo> {
    channels
        .iter()
        .enumerate()
        .map(|(i, ch)| protocol::ChannelInfo {
            #[allow(clippy::cast_possible_truncation)] // validated ≤ MAX_CHANNELS
            id: i as u16,
            name: ch.name.trim().to_string(),
            has_password: ch.password.is_some(),
            user_limit: ch.user_limit,
        })
        .collect()
}

/// Validate a candidate channel list (config and UI channels, without
/// the lobby) and return the lobby's index, i.e. where [`with_lobby`]
/// puts it. Shared by startup ([`Config::validate_channels`]) and hot
/// reload, which must vet a new list *before* swapping it in — a bad
/// edit to a running server is rejected here and never applied.
/// `default: true` is accepted and ignored (pre-1.1.5 files carry
/// it): connecting lands in the lobby, whatever the file says.
///
/// # Errors
/// A human-readable description of the first problem found.
pub fn validate_channel_list(channels: &[ChannelSettings]) -> Result<u16, String> {
    if channels.is_empty() {
        return Err(
            "channels: list is empty — omit the key entirely for a single 'General'".to_string(),
        );
    }
    if channels.len() > MAX_CHANNELS {
        return Err(format!(
            "channels: {} channels configured, maximum is {MAX_CHANNELS}",
            channels.len()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut flagged_default = false;
    for (i, ch) in channels.iter().enumerate() {
        let name = ch.name.trim();
        if name.is_empty() {
            return Err(format!("channels[{i}]: name must not be empty"));
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("channels[{i}]: duplicate channel name '{name}'"));
        }
        if ch.user_limit == Some(0) {
            return Err(format!(
                "channels[{i}] '{name}': user_limit 0 makes the channel unjoinable — \
                     omit the key for unlimited"
            ));
        }
        if ch.default && !flagged_default {
            flagged_default = true;
            log::warn!(
                "channels[{i}] '{name}': 'default: true' is ignored since 1.1.5 — everyone \
                 connects into the lobby and picks a channel; drop the key"
            );
        }
    }
    #[allow(clippy::cast_possible_truncation)] // len ≤ MAX_CHANNELS ≤ u16::MAX
    Ok(channels.len() as u16)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerSettings::default(),
            network: NetworkSettings::default(),
            limits: LimitSettings::default(),
            authentication: AuthSettings::default(),
            identity: IdentitySettings::default(),
            persistence: PersistenceSettings::default(),
            logging: LoggingSettings::default(),
            channels: default_channels(),
            user_channels: UserChannelSettings::default(),
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            name: "Baucord Server".to_string(),
            host: "0.0.0.0".to_string(),
            // 51000 since 1.1.5: some ISP gateways classify UDP/5000
            // into a policed class (the Movistar case in the session
            // guide); the 50000+ range is where voice apps live.
            port: 51000,
            public_address: None,
            greeting_message: "Welcome to the server!".to_string(),
        }
    }
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self { max_clients: 64 }
    }
}

impl Default for LimitSettings {
    fn default() -> Self {
        Self {
            max_sharers: 8,
            max_viewers_per_stream: 16,
            max_chat_bytes: 2000,
            // 4 MiB/min per client (four full-size images or a pile of
            // stickers), 16 MiB/min for the room — bursty enough for a
            // meme volley, bounded enough that a handful of spammers
            // can't multiply into hundreds of MB/min of relay egress.
            max_transfer_kb_per_minute: 4096,
            max_transfer_kb_per_minute_global: 16_384,
            media_relay_kb_per_sec: 1024,
            chat: ChatLimit::default(),
        }
    }
}

impl Default for PersistenceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "data/baucord/baucord.db".to_string(),
            history: HistorySettings::default(),
            files: FileSettings::default(),
            avatars: AvatarSettings::default(),
            telemetry: TelemetryRetention::default(),
            write_queue: 4096,
        }
    }
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            replay: 50,
            retain_days: 90,
        }
    }
}

impl Default for FileSettings {
    fn default() -> Self {
        Self {
            max_mb: 50,
            per_user_daily_mb: 500,
            total_cap_gb: 20,
            retain_days: 30,
        }
    }
}

impl Default for AvatarSettings {
    fn default() -> Self {
        Self { max_kb: 256 }
    }
}

impl Default for TelemetryRetention {
    fn default() -> Self {
        Self { retain_days: 30 }
    }
}

impl Default for UserChannelSettings {
    fn default() -> Self {
        Self {
            max_per_user: 2,
            temporary_after_min: 30,
            afk_channel: None,
            afk_after_min: 30,
        }
    }
}

impl Default for IdentitySettings {
    fn default() -> Self {
        Self {
            dir: "data/baucord".to_string(),
        }
    }
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            stats_interval_secs: 30,
            link_stats_interval_secs: 2,
            telemetry: true,
            file: "server.log".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass_policy_validation() {
        Config::default()
            .validate_policy()
            .expect("defaults are valid");
    }

    #[test]
    fn afk_channel_must_exist_in_the_channel_list() {
        let mut config = Config::default();
        config.user_channels.afk_channel = Some("AFK".to_string());
        let err = config.validate_policy().expect_err("missing AFK channel");
        assert!(err.contains("afk_channel"), "{err}");
        config.channels.push(ChannelSettings {
            name: "AFK".to_string(),
            default: false,
            password: None,
            user_limit: None,
        });
        config.validate_policy().expect("AFK channel present");
    }

    #[test]
    fn a_zero_chat_limit_is_rejected() {
        let mut config = Config::default();
        config.limits.chat.burst = 0;
        assert!(config.validate_policy().is_err());
    }

    #[test]
    fn new_sections_round_trip_through_yaml() {
        let mut config = Config::default();
        config.persistence.enabled = false;
        config.persistence.history.replay = 25;
        config.user_channels.max_per_user = 3;
        config.limits.chat.burst = 9;
        let yaml = serde_yaml::to_string(&config).expect("serialize");
        assert!(
            !yaml.contains("write_queue"),
            "tuning knob must stay out of the file"
        );
        let back: Config = serde_yaml::from_str(&yaml).expect("parse");
        assert!(!back.persistence.enabled);
        assert_eq!(back.persistence.history.replay, 25);
        assert_eq!(back.persistence.write_queue, 4096);
        assert_eq!(back.user_channels.max_per_user, 3);
        assert_eq!(back.limits.chat.burst, 9);
    }

    #[test]
    fn a_pre_1_1_config_loads_with_the_new_defaults() {
        let yaml = "server:\n  name: old\nchannels:\n  - name: Lobby\n    default: true\n";
        let config: Config = serde_yaml::from_str(yaml).expect("parse");
        assert!(config.persistence.enabled);
        assert_eq!(config.user_channels.max_per_user, 2);
        assert_eq!(config.limits.chat.per_secs, 5);
        assert_eq!(config.limits.max_sharers, 8);
    }
}
