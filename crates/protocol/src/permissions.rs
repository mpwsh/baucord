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
/// "Is an administrator": the gold name band, the admin channel, the
/// last-admin guard. Granted only by the built-in `admin` role.
pub const ADMIN: u64 = 1 << 19;

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
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.count_ones(), 1);
            for b in &all[i + 1..] {
                assert_eq!(a & b, 0);
            }
        }
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
