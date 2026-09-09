//! Permission bits, the four built-in roles, and role ranks (spec §4).
//!
//! The server decides everything from these; the client only *renders*
//! what `Session.permissions` says it may do. Bits are allocated here
//! and never reused, for the same reason as [`crate::features`]: the
//! bitset crosses the wire, and a 1.1.0 client must read a 1.1.3
//! server's set correctly.
//!
//! Role *ranks* order roles for `min_role` channel gates ("members and
//! up"); the gaps between the built-in ranks are for custom roles.

/// Move between voice channels.
pub const JOIN_CHANNELS: u64 = 1 << 0;
/// Send voice (a `voice` restriction subtracts this).
pub const SPEAK: u64 = 1 << 1;
/// Write in the general chat.
pub const TEXT_GENERAL: u64 = 1 << 2;
/// Send and receive direct messages.
pub const DM: u64 = 1 << 3;
/// Share the screen.
pub const SHARE: u64 = 1 << 4;
/// Upload files and images.
pub const UPLOAD: u64 = 1 << 5;
/// Mint invite codes (guest codes; admins may grant more).
pub const CREATE_INVITES: u64 = 1 << 6;
/// Create channels from the client, up to the per-user cap.
pub const CREATE_CHANNELS: u64 = 1 << 7;
/// Edit, kick from, and delete channels you created.
pub const MANAGE_OWN_CHANNELS: u64 = 1 << 8;
/// Edit or delete any user channel; rename config channels.
pub const MANAGE_CHANNELS: u64 = 1 << 9;
/// Close a user's connection with a reason (they may rejoin at once).
pub const DISCONNECT: u64 = 1 << 10;
/// Revoke a user's access until lifted.
pub const KICK: u64 = 1 << 11;
/// Revoke access and block the key and address.
pub const BAN: u64 = 1 << 12;
/// Mute a user's voice.
pub const MUTE: u64 = 1 << 13;
/// Apply any restriction kind.
pub const RESTRICT: u64 = 1 << 14;
/// Grant and revoke roles; create and edit custom roles.
pub const MANAGE_ROLES: u64 = 1 << 15;
/// Link and unlink keys, reset profiles, purge users.
pub const MANAGE_USERS: u64 = 1 << 16;
/// See and revoke every invite; mint member codes.
pub const MANAGE_INVITES: u64 = 1 << 17;
/// Ignore the sharer/viewer caps and file quotas.
pub const BYPASS_LIMITS: u64 = 1 << 18;
/// "Is an administrator": the admin channel, the last-admin guard.
/// Granted only by the built-in `admin` role.
pub const ADMIN: u64 = 1 << 19;
/// Wear the gold name band (spec §7.1). In the admin set, and a
/// custom role may hold or lack it — "everything but gold" is a role.
pub const GOLD_NAME: u64 = 1 << 20;
/// Open the admin panel (spec §8): the chip, the audit log, the
/// server's counters. Each tab's data still needs its own
/// permission; this is the door, not the rooms.
pub const PANEL: u64 = 1 << 21;
/// See the admin channel (spec §8): the audit as it happens, in a
/// conversation, with the slash commands at hand.
pub const ADMIN_CHANNEL: u64 = 1 << 22;

/// What a guest may do.
pub const GUEST_PERMISSIONS: u64 = JOIN_CHANNELS | SPEAK | TEXT_GENERAL;
/// What a member may do.
pub const MEMBER_PERMISSIONS: u64 = GUEST_PERMISSIONS | DM | SHARE | UPLOAD | CREATE_INVITES;
/// What an organizer may do: a member who can make channels.
pub const ORGANIZER_PERMISSIONS: u64 = MEMBER_PERMISSIONS | CREATE_CHANNELS | MANAGE_OWN_CHANNELS;
/// What an admin may do: everything.
pub const ADMIN_PERMISSIONS: u64 = ORGANIZER_PERMISSIONS
    | MANAGE_CHANNELS
    | DISCONNECT
    | KICK
    | BAN
    | MUTE
    | RESTRICT
    | MANAGE_ROLES
    | MANAGE_USERS
    | MANAGE_INVITES
    | BYPASS_LIMITS
    | GOLD_NAME
    | PANEL
    | ADMIN_CHANNEL
    | ADMIN;

/// Identifies a role on the wire and in the database.
pub type RoleId = u32;

/// Orders roles for `min_role` gates. Higher outranks lower.
pub type Rank = u16;

pub const ROLE_GUEST: RoleId = 1;
pub const ROLE_MEMBER: RoleId = 2;
pub const ROLE_ORGANIZER: RoleId = 3;
pub const ROLE_ADMIN: RoleId = 4;

pub const RANK_GUEST: Rank = 0;
pub const RANK_MEMBER: Rank = 10;
pub const RANK_ORGANIZER: Rank = 20;
pub const RANK_ADMIN: Rank = 30;

/// One of the four undeletable roles every server has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinRole {
    pub id: RoleId,
    pub name: &'static str,
    pub permissions: u64,
    pub rank: Rank,
}

/// The built-in roles, in rank order. The server re-syncs these into
/// its role table at every startup, so a permission added here reaches
/// existing databases without a migration.
pub const BUILTIN_ROLES: [BuiltinRole; 4] = [
    BuiltinRole {
        id: ROLE_GUEST,
        name: "guest",
        permissions: GUEST_PERMISSIONS,
        rank: RANK_GUEST,
    },
    BuiltinRole {
        id: ROLE_MEMBER,
        name: "member",
        permissions: MEMBER_PERMISSIONS,
        rank: RANK_MEMBER,
    },
    BuiltinRole {
        id: ROLE_ORGANIZER,
        name: "organizer",
        permissions: ORGANIZER_PERMISSIONS,
        rank: RANK_ORGANIZER,
    },
    BuiltinRole {
        id: ROLE_ADMIN,
        name: "admin",
        permissions: ADMIN_PERMISSIONS,
        rank: RANK_ADMIN,
    },
];

/// Whether `bit` is in `permissions`.
#[must_use]
pub const fn has(permissions: u64, bit: u64) -> bool {
    permissions & bit != 0
}

/// Every permission with the name the CLI and the panel use for it,
/// in bit order. [`ADMIN`] is listed so it can be *printed*; a custom
/// role can never hold it ([`CUSTOM_ROLE_PERMISSIONS`]).
pub const PERMISSION_NAMES: [(&str, u64); 23] = [
    ("join", JOIN_CHANNELS),
    ("speak", SPEAK),
    ("text", TEXT_GENERAL),
    ("dm", DM),
    ("share", SHARE),
    ("upload", UPLOAD),
    ("invite", CREATE_INVITES),
    ("create-channels", CREATE_CHANNELS),
    ("manage-own-channels", MANAGE_OWN_CHANNELS),
    ("manage-channels", MANAGE_CHANNELS),
    ("disconnect", DISCONNECT),
    ("kick", KICK),
    ("ban", BAN),
    ("mute", MUTE),
    ("restrict", RESTRICT),
    ("manage-roles", MANAGE_ROLES),
    ("manage-users", MANAGE_USERS),
    ("manage-invites", MANAGE_INVITES),
    ("bypass-limits", BYPASS_LIMITS),
    ("gold", GOLD_NAME),
    ("panel", PANEL),
    ("admin-channel", ADMIN_CHANNEL),
    ("admin", ADMIN),
];

/// The glyphs a role may wear (spec §4), by lucide name; `""` is
/// none. Clients map the name to the icon, so the list is closed.
pub const ROLE_ICONS: [&str; 16] = [
    "star", "crown", "shield", "wrench", "zap", "flame", "heart", "gem", "trophy", "sword",
    "music", "gamepad", "video", "mic", "eye", "bot",
];

/// Whether `icon` is one of [`ROLE_ICONS`] or none.
#[must_use]
pub fn valid_role_icon(icon: &str) -> bool {
    icon.is_empty() || ROLE_ICONS.contains(&icon)
}

/// What a custom role may be given: everything but [`ADMIN`], which
/// only the built-in `admin` role grants (the gold band, the
/// last-admin guard).
pub const CUSTOM_ROLE_PERMISSIONS: u64 = ADMIN_PERMISSIONS & !ADMIN;

/// A custom role's rank follows from its permissions: the tier of the
/// highest built-in whose whole set it contains, placed just above
/// that tier — everything a member has sits above members and below
/// organizers; everything an organizer has (an administrator without
/// the `admin` bit) sits above organizers and below admins. Nothing
/// custom reaches the admin rank, and the operator never picks a
/// number: what a role can do decides where it stands for the
/// moderation rank rule and the channel gates.
#[must_use]
pub fn derived_rank(permissions: u64) -> Rank {
    BUILTIN_ROLES
        .iter()
        .filter(|r| r.rank < RANK_ADMIN && permissions & r.permissions == r.permissions)
        .map(|r| r.rank + 5)
        .max()
        .unwrap_or(RANK_GUEST + 5)
}

/// The bit for a permission name (`"share"`), or `None`.
#[must_use]
pub fn permission_by_name(name: &str) -> Option<u64> {
    let lower = name.trim().to_ascii_lowercase();
    PERMISSION_NAMES
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, bit)| *bit)
}

/// The names in `permissions`, in bit order: `"join, speak, text"`.
#[must_use]
pub fn permission_names(permissions: u64) -> Vec<&'static str> {
    PERMISSION_NAMES
        .iter()
        .filter(|(_, bit)| has(permissions, *bit))
        .map(|(name, _)| *name)
        .collect()
}

/// Whether `name` may name a custom role: 1–32 characters of
/// lowercase ASCII letters, digits and hyphens, not a built-in's.
#[must_use]
pub fn valid_custom_role_name(name: &str) -> bool {
    let ok_chars = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    ok_chars && !BUILTIN_ROLES.iter().any(|r| r.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_distinct() {
        let all = [
            JOIN_CHANNELS,
            SPEAK,
            TEXT_GENERAL,
            DM,
            SHARE,
            UPLOAD,
            CREATE_INVITES,
            CREATE_CHANNELS,
            MANAGE_OWN_CHANNELS,
            MANAGE_CHANNELS,
            DISCONNECT,
            KICK,
            BAN,
            MUTE,
            RESTRICT,
            MANAGE_ROLES,
            MANAGE_USERS,
            MANAGE_INVITES,
            BYPASS_LIMITS,
            ADMIN,
            GOLD_NAME,
            PANEL,
            ADMIN_CHANNEL,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.count_ones(), 1);
            for b in &all[i + 1..] {
                assert_eq!(a & b, 0);
            }
        }
    }

    #[test]
    fn permission_names_round_trip() {
        for (name, bit) in PERMISSION_NAMES {
            assert_eq!(permission_by_name(name), Some(bit));
            assert_eq!(permission_names(bit), vec![name]);
        }
        assert_eq!(permission_by_name("fly"), None);
        assert_eq!(
            permission_names(GUEST_PERMISSIONS),
            vec!["join", "speak", "text"]
        );
        assert!(!has(CUSTOM_ROLE_PERMISSIONS, ADMIN));
        assert!(has(CUSTOM_ROLE_PERMISSIONS, GOLD_NAME));
        assert_eq!(CUSTOM_ROLE_PERMISSIONS | ADMIN, ADMIN_PERMISSIONS);
    }

    #[test]
    fn custom_role_names_are_strict() {
        assert!(valid_custom_role_name("streamer"));
        assert!(valid_custom_role_name("mod-2"));
        assert!(!valid_custom_role_name(""));
        assert!(!valid_custom_role_name("Streamer"));
        assert!(!valid_custom_role_name("-mod"));
        assert!(!valid_custom_role_name("member"));
        assert!(!valid_custom_role_name(&"a".repeat(33)));
    }

    #[test]
    fn a_custom_roles_rank_follows_its_permissions() {
        // Less than a guest: just above guests all the same.
        assert_eq!(derived_rank(0), RANK_GUEST + 5);
        assert_eq!(derived_rank(GUEST_PERMISSIONS | KICK), RANK_GUEST + 5);
        assert_eq!(derived_rank(MEMBER_PERMISSIONS | SHARE), RANK_MEMBER + 5);
        assert_eq!(derived_rank(ORGANIZER_PERMISSIONS), RANK_ORGANIZER + 5);
        // Everything but gold: above organizers, below admins.
        let steward = CUSTOM_ROLE_PERMISSIONS & !GOLD_NAME;
        assert!(derived_rank(steward) > RANK_ORGANIZER);
        assert!(derived_rank(steward) < RANK_ADMIN);
        // Even the full custom set never reaches the admin rank.
        assert!(derived_rank(CUSTOM_ROLE_PERMISSIONS) < RANK_ADMIN);
    }

    #[test]
    fn builtin_roles_nest_by_rank() {
        for pair in BUILTIN_ROLES.windows(2) {
            let (lower, higher) = (pair[0], pair[1]);
            assert!(lower.rank < higher.rank);
            assert_eq!(
                lower.permissions & higher.permissions,
                lower.permissions,
                "{} must hold everything {} holds",
                higher.name,
                lower.name
            );
        }
    }

    #[test]
    fn only_admins_carry_the_admin_bit() {
        for role in BUILTIN_ROLES {
            assert_eq!(has(role.permissions, ADMIN), role.id == ROLE_ADMIN);
        }
    }

    #[test]
    fn guests_cannot_dm_share_or_upload() {
        assert!(!has(GUEST_PERMISSIONS, DM));
        assert!(!has(GUEST_PERMISSIONS, SHARE));
        assert!(!has(GUEST_PERMISSIONS, UPLOAD));
    }
}
