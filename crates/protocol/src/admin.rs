//! Moderation and administration (spec §4.1, §4.2, §8): the restriction
//! kinds, the admin commands, and what they answer with. Everything an
//! admin can do from the client goes through [`AdminCommand`], is
//! checked by the server like any other message, and is audited there.

use serde::{Deserialize, Serialize};

use crate::{
    messages::{ClientId, UserId},
    permissions::RoleId,
};

/// What a restriction takes away (spec §4.2). The discriminants are the
/// database encoding; never renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RestrictionKind {
    /// Cannot join at all (kick, ban).
    Server = 0,
    /// Voice datagrams dropped (mute).
    Voice = 1,
    GeneralText = 2,
    Dm = 3,
    Upload = 4,
    Share = 5,
    Profile = 6,
}

impl RestrictionKind {
    #[must_use]
    pub fn from_db(value: i64) -> Option<Self> {
        Some(match value {
            0 => Self::Server,
            1 => Self::Voice,
            2 => Self::GeneralText,
            3 => Self::Dm,
            4 => Self::Upload,
            5 => Self::Share,
            6 => Self::Profile,
            _ => return None,
        })
    }

    #[must_use]
    pub fn to_db(self) -> i64 {
        self as i64
    }

    /// The name used in commands (`/restrict name voice 30m`) and logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Voice => "voice",
            Self::GeneralText => "text",
            Self::Dm => "dm",
            Self::Upload => "upload",
            Self::Share => "share",
            Self::Profile => "profile",
        }
    }

    /// The inverse of [`Self::name`], forgiving about aliases.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "server" | "join" => Self::Server,
            "voice" | "mute" => Self::Voice,
            "text" | "chat" | "general_text" | "general" => Self::GeneralText,
            "dm" | "dms" => Self::Dm,
            "upload" | "uploads" => Self::Upload,
            "share" | "sharing" => Self::Share,
            "profile" => Self::Profile,
            _ => return None,
        })
    }
}

/// Whom a command is about. A client that has the person in its roster
/// sends the id; for someone offline (a ban to lift, a kick to undo)
/// it sends the name, and the server resolves it — a name of record,
/// case-insensitive, or a hex id prefix of at least 8 digits. An
/// ambiguous name is refused with the candidates listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminTarget {
    User(UserId),
    /// A device key hash (spec §2.1), whoever holds it.
    Key(UserId),
    /// A source IP address, in text.
    Address(String),
    /// A user by name of record or id prefix, resolved by the server.
    Name(String),
}

/// One administrative action (spec §8). `reason` texts are shown to the
/// affected user and kept in the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminCommand {
    ListUsers,
    ListRestrictions,
    ListInvites,
    /// Replace a user's role set. Needs `MANAGE_ROLES`; no role may
    /// rank above the actor's own, admins may edit each other, and the
    /// last admin keeps `admin`.
    SetRoles {
        target: AdminTarget,
        roles: Vec<RoleId>,
    },
    /// Add a restriction (needs `RESTRICT`, or `MUTE` for `voice`).
    /// `duration_secs: None` = until lifted. `/mute` is this with
    /// `voice` or `text`.
    Restrict {
        target: AdminTarget,
        kind: RestrictionKind,
        duration_secs: Option<u64>,
        reason: String,
    },
    /// Remove a restriction by id (needs `RESTRICT`, or `MUTE` for a
    /// `voice` one).
    Lift {
        restriction_id: u64,
    },
    /// Remove every restriction of `kind` on a user (`/unmute`). Same
    /// permission as `Restrict`.
    Unmute {
        target: AdminTarget,
        kind: RestrictionKind,
    },
    /// Close the session; access unchanged (needs `DISCONNECT`).
    Disconnect {
        target: AdminTarget,
        reason: String,
    },
    /// Close the session and forget the person's device keys and
    /// roles: they need a new invite to come back, and start over as
    /// whatever it grants (needs `KICK`). Nothing to lift.
    Kick {
        target: AdminTarget,
        reason: String,
    },
    /// Close the session and block the user, their key and their
    /// address until `duration_secs` runs out or `Unban` (needs `BAN`).
    Ban {
        target: AdminTarget,
        duration_secs: Option<u64>,
        reason: String,
    },
    /// Remove every `server` restriction on a user (needs `BAN`).
    Unban {
        target: AdminTarget,
    },
    /// Put a user in another voice channel (needs `MANAGE_CHANNELS`).
    MoveUser {
        target: AdminTarget,
        channel_id: u16,
    },
    /// Stop a code from admitting anyone else (needs `MANAGE_INVITES`).
    RevokeInvite {
        code: String,
    },
    /// Attach a device key to a user — a retired key, or one seen in the
    /// log (needs `MANAGE_USERS`).
    LinkKey {
        target: AdminTarget,
        key_hash: UserId,
    },
    /// Retire a device key: that device is refused until re-linked
    /// (needs `MANAGE_USERS`).
    UnlinkKey {
        key_hash: UserId,
    },
    /// Remove anyone's message (needs `RESTRICT`).
    DeleteMessage {
        msg_id: u64,
    },
    /// Delete a person: their row, keys, roles, restrictions, progress
    /// and messages. The audit keeps a line (needs `MANAGE_USERS`).
    PurgeUser {
        target: AdminTarget,
    },
    /// The last telemetry sample a user sent, summarized (needs
    /// `MANAGE_USERS`): what the roster tooltip shows.
    LastTelemetry {
        target: AdminTarget,
    },
    /// Clear a user's profile (spec §7.1; needs `RESTRICT`); with
    /// `lock`, a `profile` restriction keeps it cleared until lifted.
    ResetProfile {
        target: AdminTarget,
        lock: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySummary {
    pub key_hash: UserId,
    pub retired: bool,
    /// Milliseconds since the Unix epoch.
    pub last_seen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSummary {
    pub user_id: UserId,
    pub name: String,
    pub roles: Vec<RoleId>,
    /// The session id while online.
    pub client_id: Option<ClientId>,
    pub last_seen: u64,
    pub keys: Vec<KeySummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestrictionSummary {
    pub id: u64,
    pub user_id: Option<UserId>,
    /// The restricted user's name of record, when there is one.
    pub user_name: Option<String>,
    pub key_hash: Option<UserId>,
    pub address: Option<String>,
    pub kind: RestrictionKind,
    pub until: Option<u64>,
    pub reason: String,
    /// Who did it, by name; `server` when automatic.
    pub by: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteSummary {
    pub code: String,
    pub grants_role: RoleId,
    pub created_by: Option<String>,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub revoked: bool,
}

/// What a command produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminOutcome {
    Done,
    Users(Vec<UserSummary>),
    Restrictions(Vec<RestrictionSummary>),
    Invites(Vec<InviteSummary>),
    /// `None`: nothing received from that user this run.
    Telemetry(Option<TelemetrySummary>),
}

/// A user's last telemetry, as the server logs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetrySummary {
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    pub line: String,
}

/// Why a command was refused. The client renders one line each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminError {
    /// The actor lacks the permission the command needs.
    NoPermission,
    NoSuchUser,
    NoSuchRestriction,
    NoSuchInvite,
    NoSuchRole,
    NoSuchChannel,
    /// The target ranks at or above the actor (spec §8: nobody acts on
    /// an equal or higher rank), or a role would outrank the actor.
    Outranked,
    /// The last admin keeps `admin`.
    LastAdmin,
    /// You cannot do that to yourself (kick, ban, disconnect).
    Yourself,
    /// A malformed command (reason attached).
    Invalid(String),
}

impl AdminError {
    /// Human-readable form for the chat line.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NoPermission => "you may not do that".to_string(),
            Self::NoSuchUser => "no such user".to_string(),
            Self::NoSuchRestriction => "no such restriction".to_string(),
            Self::NoSuchInvite => "no such invite".to_string(),
            Self::NoSuchRole => "no such role".to_string(),
            Self::NoSuchChannel => "no such channel".to_string(),
            Self::Outranked => "they rank at or above you".to_string(),
            Self::LastAdmin => "the last admin keeps admin".to_string(),
            Self::Yourself => "not on yourself".to_string(),
            Self::Invalid(why) => why.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restriction_kinds_round_trip_the_db_encoding_and_names() {
        for kind in [
            RestrictionKind::Server,
            RestrictionKind::Voice,
            RestrictionKind::GeneralText,
            RestrictionKind::Dm,
            RestrictionKind::Upload,
            RestrictionKind::Share,
            RestrictionKind::Profile,
        ] {
            assert_eq!(RestrictionKind::from_db(kind.to_db()), Some(kind));
            assert_eq!(RestrictionKind::parse(kind.name()), Some(kind));
        }
        assert_eq!(RestrictionKind::from_db(7), None);
        assert_eq!(RestrictionKind::parse("mute"), Some(RestrictionKind::Voice));
        assert_eq!(RestrictionKind::parse("nope"), None);
    }
}
