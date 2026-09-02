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
    pub authentication: AuthSettings,
    pub identity: IdentitySettings,
    pub logging: LoggingSettings,
    /// Voice channels (rooms). Omitting the key gives a single open
    /// "Lobby" default channel, so pre-channels configs keep working.
    pub channels: Vec<ChannelSettings>,
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
        name: "Lobby".to_string(),
        default: true,
        password: None,
        user_limit: None,
    }]
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
/// (plus voice, which is negligible). Defaults: 2 × 8 × ~4 Mbit/s ≈
/// 64 Mbit/s. Size these to your uplink instead of discovering the limit
/// live mid-movie-night.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitSettings {
    /// Concurrent screen shares allowed.
    pub max_sharers: usize,
    /// Viewers allowed per stream.
    pub max_viewers_per_stream: usize,
    /// Chat messages longer than this (bytes) are truncated.
    pub max_chat_bytes: usize,
    /// Per-client image *bandwidth* budget: KiB of accepted chat
    /// images per sliding minute. Bytes are the resource being
    /// protected — a burst of tiny stickers is harmless while two
    /// 1 MiB monsters back-to-back are not, so the budget weighs each
    /// image by its size instead of counting them. Images that would
    /// exceed the window are dropped with a warning. `0` disables
    /// image relay entirely. Note the amplification: every accepted
    /// KiB is relayed to every connected client, so per-client egress
    /// pressure ≈ this budget × connected clients. A budget smaller
    /// than an image rejects that image outright: the wire allows
    /// images up to 10 MiB (10240 KiB), so the default of 4096 also
    /// acts as a per-image size ceiling of ~4 MiB — raise it past
    /// 10240 if you want single full-size images through.
    pub max_image_kb_per_minute: usize,
    /// Same budget across the *whole server* (all senders combined,
    /// same sliding minute). The per-client budget bounds one chatty
    /// client; this bounds the coordinated case where several clients
    /// bursting together multiply into `clients × bytes` of relay
    /// egress. `0` disables image relay entirely.
    pub max_image_kb_per_minute_global: usize,
    /// Relay pacing for chat images, KiB per second *per receiver*.
    /// Images are re-chunked and trickled out at this rate so a big
    /// image never floods a receiver's downlink queue — that queue is
    /// shared with voice, and flooding it is what made calls stutter
    /// during uploads. Values below 64 are treated as 64. At the
    /// default 1024, a 2.5 MB image takes ~2.5 s per receiver.
    pub media_relay_kb_per_sec: usize,
}

/// Admission. Every connection is encrypted and the server's identity
/// is pinned by clients regardless of these settings; this is only
/// about who gets past `Hello`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// `true`: clients must send the shared `password` in `Hello`
    /// (compared in constant time; failures are rate-limited per
    /// source address). `false`: open mode for LAN/dev — anyone who
    /// can reach the UDP port can join with any name. The server logs
    /// loudly when running open.
    pub secure: bool,
    /// The room secret. Required when `secure` is true.
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingSettings {
    pub level: String,
    pub stats_interval_secs: u64,
    /// One `link:` line per connected client every this many seconds
    /// (0 = off): the relay's view of that client's link — QUIC path
    /// RTT, congestion window, transport-level loss, bytes in/out,
    /// abandoned video groups, and the voice packets received vs. the
    /// sender's sequence numbers (what listeners actually miss). Meant
    /// to be lined up by timestamp against the client's own `link:`
    /// lines and its `telemetry:` samples. ~9 clients at 2 s is a few
    /// hundred lines a minute; raise it or set 0 outside diagnosis.
    pub link_stats_interval_secs: u64,
    /// Log the periodic `telemetry:` samples clients send (§9.1). Off
    /// still accepts the messages; they are just not written out.
    pub telemetry: bool,
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

    /// Check the admission settings for contradictions.
    ///
    /// # Errors
    /// `secure: true` without a password.
    pub fn validate_authentication(&self) -> Result<(), String> {
        if self.authentication.secure
            && self
                .authentication
                .password
                .as_deref()
                .is_none_or(|p| p.trim().is_empty())
        {
            return Err(
                "authentication.secure is true but authentication.password is empty — \
                 set a password, or set secure: false for an open LAN server"
                    .to_string(),
            );
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

    /// Validate the channel list and return the default channel's index.
    ///
    /// Enforced here (at startup, loudly) rather than tolerated at join
    /// time: empty/duplicate names, zero or multiple `default: true`,
    /// a password or user limit on the default channel, `user_limit: 0`,
    /// and more than [`MAX_CHANNELS`] channels are all config mistakes
    /// the operator should fix, not states the server should limp
    /// through.
    ///
    /// # Errors
    /// A human-readable description of the first problem found.
    pub fn validate_channels(&self) -> Result<u16, String> {
        validate_channel_list(&self.channels)
    }

    /// The channel list as announced to clients (passwords reduced to
    /// a `has_password` flag; they never cross the wire).
    #[must_use]
    pub fn channel_infos(&self) -> Vec<protocol::ChannelInfo> {
        self.channels
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
}

/// Validate a candidate channel list and return the default channel's
/// index. Shared by startup ([`Config::validate_channels`]) and hot
/// reload, which must vet a new list *before* swapping it in — a bad
/// edit to a running server is rejected here and never applied.
///
/// # Errors
/// A human-readable description of the first problem found.
pub fn validate_channel_list(channels: &[ChannelSettings]) -> Result<u16, String> {
    if channels.is_empty() {
        return Err(
            "channels: list is empty — omit the key entirely for a single default Lobby"
                .to_string(),
        );
    }
    if channels.len() > MAX_CHANNELS {
        return Err(format!(
            "channels: {} channels configured, maximum is {MAX_CHANNELS}",
            channels.len()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut default_index: Option<usize> = None;
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
        if ch.default {
            if let Some(first) = default_index {
                return Err(format!(
                    "channels[{i}] '{name}': 'default: true' is already set on \
                         channels[{first}] '{}' — exactly one channel may be the default",
                    channels[first].name
                ));
            }
            if ch.password.is_some() {
                return Err(format!(
                    "channels[{i}] '{name}': the default channel cannot have a password — \
                         every client lands there on connect"
                ));
            }
            if ch.user_limit.is_some() {
                return Err(format!(
                    "channels[{i}] '{name}': the default channel cannot have a user_limit — \
                         a full default channel would block all new connections"
                ));
            }
            default_index = Some(i);
        }
    }
    let default_index = default_index.ok_or_else(|| {
        "channels: no channel has 'default: true' — mark exactly one (typically the Lobby)"
            .to_string()
    })?;
    #[allow(clippy::cast_possible_truncation)] // len ≤ MAX_CHANNELS ≤ u16::MAX
    Ok(default_index as u16)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerSettings::default(),
            network: NetworkSettings::default(),
            limits: LimitSettings::default(),
            authentication: AuthSettings::default(),
            identity: IdentitySettings::default(),
            logging: LoggingSettings::default(),
            channels: default_channels(),
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            name: "Baucord Server".to_string(),
            host: "0.0.0.0".to_string(),
            port: 5000,
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
            max_sharers: 2,
            max_viewers_per_stream: 8,
            max_chat_bytes: 2000,
            // 4 MiB/min per client (four full-size images or a pile of
            // stickers), 16 MiB/min for the room — bursty enough for a
            // meme volley, bounded enough that a handful of spammers
            // can't multiply into hundreds of MB/min of relay egress.
            max_image_kb_per_minute: 4096,
            max_image_kb_per_minute_global: 16_384,
            media_relay_kb_per_sec: 1024,
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
        }
    }
}
