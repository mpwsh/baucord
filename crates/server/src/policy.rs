//! The policy the tick consults: users, their keys, roles, invites and
//! restrictions (spec §0, rules 4 and 5).
//!
//! Always in memory, in both modes. With persistence on it is loaded
//! from the database at startup, every change is also queued to the
//! persistence thread, and an external change (the CLI, `sqlite3`)
//! arrives as a fresh [`Policy`] to swap in. With persistence off it
//! lives exactly as long as the process.
//!
//! Nothing here touches disk or the network: pure data and the rules
//! over it, so admission decisions are unit-testable without a wire.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    net::IpAddr,
};

pub use protocol::RestrictionKind;
use protocol::{
    permissions::{self, Rank, RoleId, BUILTIN_ROLES, RANK_GUEST},
    BlobId, InviteInvalidReason, Profile, UserId,
};

use crate::clock::WallMs;

/// SHA-256 of a raw Ed25519 public key (spec §2.1). Same shape as a
/// user id — a user's id *is* the hash of the key that created it.
pub type KeyHash = UserId;

/// Invite codes: 12 characters from an alphabet without `0/O/1/l/I`
/// (spec §3.1; the shape lives in `protocol::invite`). 31¹² ≈ 2⁵⁹ —
/// guessing is moot even before the rate limit.
pub const INVITE_CODE_LEN: usize = protocol::invite::CODE_LEN;
pub const INVITE_ALPHABET: &[u8; 31] = protocol::invite::ALPHABET;
/// A code lasts this long unless its creator says otherwise.
pub const DEFAULT_INVITE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// The one-shot admin code printed at startup (spec §1.1).
pub const BOOTSTRAP_INVITE_TTL_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    /// The name of record, refreshed from each `Hello`.
    pub name: String,
    pub created_at: WallMs,
    pub last_seen: WallMs,
    /// The code that admitted this user; `None` for the CLI or
    /// bootstrap.
    pub invited_with: Option<String>,
    pub roles: BTreeSet<RoleId>,
    /// How the name looks (spec §7.1); on the user row so it follows
    /// the key.
    pub profile: Profile,
}

/// What the server knows about a stored blob besides its bytes (spec
/// §7). Bytes live on disk (persistent) or in the server's memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    /// What the server sniffed.
    pub mime: String,
    pub size: u32,
    pub name: String,
    pub created_by: Option<UserId>,
    pub created_at: WallMs,
    /// Profiles pointing at it; zero means evictable.
    pub refs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    pub user_id: UserId,
    pub added_at: WallMs,
    pub last_seen: WallMs,
    /// Unlinked by an admin: the hash stays taken (spec §2.1).
    pub retired_at: Option<WallMs>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRecord {
    pub name: String,
    pub permissions: u64,
    pub rank: Rank,
    pub builtin: bool,
    /// A glyph from `permissions::ROLE_ICONS`, or `""` (spec §4).
    pub icon: String,
    /// RGB for the badge and the card, 0 = none.
    pub color: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRecord {
    pub created_by: Option<UserId>,
    pub created_at: WallMs,
    pub expires_at: Option<WallMs>,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub grants_role: RoleId,
    pub revoked_at: Option<WallMs>,
}

/// One row of the ban family. At least one of `user_id`, `key_hash`,
/// `ip` is set; a ban sets all it knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restriction {
    pub id: u64,
    pub user_id: Option<UserId>,
    pub key_hash: Option<KeyHash>,
    pub ip: Option<IpAddr>,
    pub kind: RestrictionKind,
    /// `None` = indefinite.
    pub until: Option<WallMs>,
    pub reason: String,
    pub by: Option<UserId>,
    pub created_at: WallMs,
}

impl Restriction {
    /// Whether this restriction is in force at `now`.
    #[must_use]
    pub fn active(&self, now: WallMs) -> bool {
        self.until.is_none_or(|until| until > now)
    }
}

/// How a device key resolves at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyLookup {
    /// Never seen: needs an invite.
    Unknown,
    /// Unlinked by an admin: needs a re-link, not a new user.
    Retired,
    /// Known: this is the login.
    User(UserId),
}

/// A channel made from the UI (spec §5). Config channels are not here;
/// they come from `server.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserChannelRecord {
    pub stable_id: u64,
    pub name: String,
    pub password: Option<String>,
    pub user_limit: Option<u16>,
    pub min_role: Option<RoleId>,
    pub permanent: bool,
    pub afk_after_min: Option<u16>,
    pub owner: Option<UserId>,
    pub created_at: WallMs,
    /// Since when nobody has been in it; the temporary sweep reads it.
    pub empty_since: Option<WallMs>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub users: HashMap<UserId, UserRecord>,
    pub keys: HashMap<KeyHash, KeyRecord>,
    pub roles: HashMap<RoleId, RoleRecord>,
    pub invites: HashMap<String, InviteRecord>,
    pub restrictions: Vec<Restriction>,
    /// Next restriction id; rebased on the database at load.
    pub next_restriction_id: u64,
    /// UI-created channels in creation order.
    pub channels: Vec<UserChannelRecord>,
    pub next_channel_id: u64,
    /// Total xp per user (spec §9.1); the level is derived.
    pub xp: HashMap<UserId, u64>,
    /// Stored blobs by hash (spec §7).
    pub blobs: HashMap<BlobId, BlobMeta>,
    /// Who each user blocked (`protocol::safety`, 1.2).
    pub blocks: HashMap<UserId, HashSet<UserId>>,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy {
    /// An empty policy with the four built-in roles.
    #[must_use]
    pub fn new() -> Self {
        let roles = BUILTIN_ROLES
            .iter()
            .map(|r| {
                (
                    r.id,
                    RoleRecord {
                        name: r.name.to_string(),
                        permissions: r.permissions,
                        rank: r.rank,
                        builtin: true,
                        icon: String::new(),
                        color: 0,
                    },
                )
            })
            .collect();
        Self {
            users: HashMap::new(),
            keys: HashMap::new(),
            roles,
            invites: HashMap::new(),
            restrictions: Vec::new(),
            next_restriction_id: 1,
            channels: Vec::new(),
            next_channel_id: 1,
            xp: HashMap::new(),
            blobs: HashMap::new(),
            blocks: HashMap::new(),
        }
    }

    /// A user's level from their xp (spec §9.1).
    #[must_use]
    pub fn level_of(&self, user: &UserId) -> u16 {
        protocol::level_for_xp(self.xp.get(user).copied().unwrap_or(0))
    }

    // ── Reads ───────────────────────────────────────────────────────

    /// Whether anyone holds the `ADMIN` permission. When nobody does,
    /// the server prints a bootstrap invite (spec §1.1).
    #[must_use]
    pub fn has_admin(&self) -> bool {
        self.users
            .keys()
            .any(|user| permissions::has(self.permissions_of(user), permissions::ADMIN))
    }

    /// The union of a user's roles. Restrictions are subtracted by the
    /// caller at check time.
    #[must_use]
    pub fn permissions_of(&self, user: &UserId) -> u64 {
        self.users.get(user).map_or(0, |u| {
            u.roles
                .iter()
                .filter_map(|id| self.roles.get(id))
                .fold(0, |acc, r| acc | r.permissions)
        })
    }

    /// The highest rank among a user's roles; guest rank for an
    /// unknown user.
    #[must_use]
    pub fn rank_of(&self, user: &UserId) -> Rank {
        self.users.get(user).map_or(RANK_GUEST, |u| {
            u.roles
                .iter()
                .filter_map(|id| self.roles.get(id))
                .map(|r| r.rank)
                .max()
                .unwrap_or(RANK_GUEST)
        })
    }

    #[must_use]
    pub fn resolve_key(&self, key_hash: &KeyHash) -> KeyLookup {
        match self.keys.get(key_hash) {
            None => KeyLookup::Unknown,
            Some(k) if k.retired_at.is_some() => KeyLookup::Retired,
            Some(k) => KeyLookup::User(k.user_id),
        }
    }

    /// Whether `code` (already normalized) admits right now.
    ///
    /// # Errors
    /// Which of the four ways it does not (spec §3.3).
    pub fn check_invite(
        &self,
        code: &str,
        now: WallMs,
    ) -> Result<&InviteRecord, InviteInvalidReason> {
        let invite = self.invites.get(code).ok_or(InviteInvalidReason::Unknown)?;
        if invite.revoked_at.is_some() {
            return Err(InviteInvalidReason::Revoked);
        }
        if invite.expires_at.is_some_and(|at| at <= now) {
            return Err(InviteInvalidReason::Expired);
        }
        if invite.max_uses.is_some_and(|max| invite.uses >= max) {
            return Err(InviteInvalidReason::Exhausted);
        }
        Ok(invite)
    }

    /// The `server` restriction that blocks this key, user or address
    /// right now, if any. Checked before anything else at admission.
    ///
    /// A ban on the user always holds. One that reaches a known user
    /// only through a key or an address is the fallout of banning
    /// someone else, and the rank rule (spec §8) applies to it as it
    /// does when the ban is issued: see [`Self::spared`].
    #[must_use]
    pub fn server_ban(
        &self,
        user: Option<&UserId>,
        key_hash: &KeyHash,
        ip: Option<IpAddr>,
        now: WallMs,
    ) -> Option<&Restriction> {
        self.restrictions.iter().find(|r| {
            if r.kind != RestrictionKind::Server || !r.active(now) {
                return false;
            }
            if user.is_some() && r.user_id.as_ref() == user {
                return true;
            }
            let reaches = r.key_hash.as_ref() == Some(key_hash) || (ip.is_some() && r.ip == ip);
            reaches && !user.is_some_and(|u| self.spared(r, u))
        })
    }

    /// Whether a key or address ban issued by a person passes `user`
    /// by: never an administrator, never the issuer, never anyone at or
    /// above the issuer's rank. Without this, someone allowed to ban
    /// could lock an administrator out by naming their address (review
    /// P2-10). A ban written by the operator (the CLI, `by` unset)
    /// spares nobody.
    fn spared(&self, r: &Restriction, user: &UserId) -> bool {
        r.by.as_ref().is_some_and(|by| {
            permissions::has(self.permissions_of(user), permissions::ADMIN)
                || (self.users.contains_key(by) && self.rank_of(user) >= self.rank_of(by))
        })
    }

    /// A restriction of `kind` on `user` in force now, if any.
    #[must_use]
    pub fn restriction_on(
        &self,
        user: &UserId,
        kind: RestrictionKind,
        now: WallMs,
    ) -> Option<&Restriction> {
        self.restrictions
            .iter()
            .find(|r| r.kind == kind && r.active(now) && r.user_id.as_ref() == Some(user))
    }

    /// Whether a user other than `except` who outranks a guest holds
    /// `name` (spec §10: a guest cannot take a member's name while that
    /// member is offline). Case-insensitive.
    #[must_use]
    pub fn name_held_by_member(&self, name: &str, except: &UserId) -> bool {
        self.users.iter().any(|(id, u)| {
            id != except && u.name.eq_ignore_ascii_case(name) && self.rank_of(id) > RANK_GUEST
        })
    }

    // ── Writes (the caller also queues the persistence command) ─────

    pub fn create_user(
        &mut self,
        user_id: UserId,
        name: &str,
        now: WallMs,
        invited_with: Option<String>,
    ) {
        self.users.insert(
            user_id,
            UserRecord {
                name: name.to_string(),
                created_at: now,
                last_seen: now,
                invited_with,
                roles: BTreeSet::new(),
                profile: Profile::default(),
            },
        );
    }

    pub fn link_key(&mut self, key_hash: KeyHash, user_id: UserId, now: WallMs) {
        self.keys.insert(
            key_hash,
            KeyRecord {
                user_id,
                added_at: now,
                last_seen: now,
                retired_at: None,
            },
        );
    }

    pub fn grant_role(&mut self, user_id: &UserId, role: RoleId) {
        if let Some(u) = self.users.get_mut(user_id) {
            u.roles.insert(role);
        }
    }

    /// Refresh the name of record and the last-seen stamps of a user
    /// and the key they came in with.
    pub fn touch(&mut self, user_id: &UserId, key_hash: &KeyHash, name: &str, now: WallMs) {
        if let Some(u) = self.users.get_mut(user_id) {
            u.name = name.to_string();
            u.last_seen = now;
        }
        if let Some(k) = self.keys.get_mut(key_hash) {
            k.last_seen = now;
        }
    }

    pub fn add_invite(&mut self, code: String, invite: InviteRecord) {
        self.invites.insert(code, invite);
    }

    /// Count one admission through `code`.
    pub fn use_invite(&mut self, code: &str) {
        if let Some(i) = self.invites.get_mut(code) {
            i.uses = i.uses.saturating_add(1);
        }
    }

    pub fn revoke_invite(&mut self, code: &str, now: WallMs) -> bool {
        match self.invites.get_mut(code) {
            Some(i) if i.revoked_at.is_none() => {
                i.revoked_at = Some(now);
                true
            }
            _ => false,
        }
    }

    /// Add a restriction, assigning its id. Returns the stored row.
    pub fn add_restriction(&mut self, mut restriction: Restriction) -> Restriction {
        restriction.id = self.next_restriction_id;
        self.next_restriction_id += 1;
        self.restrictions.push(restriction.clone());
        restriction
    }

    pub fn remove_restriction(&mut self, id: u64) -> bool {
        let before = self.restrictions.len();
        self.restrictions.retain(|r| r.id != id);
        self.restrictions.len() != before
    }

    /// Drop restrictions past their `until`.
    pub fn expire_restrictions(&mut self, now: WallMs) {
        self.restrictions.retain(|r| r.active(now));
    }
}

// ── Invite codes ────────────────────────────────────────────────────────

/// A fresh random code from [`INVITE_ALPHABET`].
#[must_use]
pub fn generate_invite_code() -> String {
    let mut code = String::with_capacity(INVITE_CODE_LEN);
    while code.len() < INVITE_CODE_LEN {
        // Rejection sampling keeps every symbol equally likely.
        let byte: u8 = rand::random();
        if usize::from(byte) < 8 * INVITE_ALPHABET.len() {
            code.push(char::from(
                INVITE_ALPHABET[usize::from(byte) % INVITE_ALPHABET.len()],
            ));
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use protocol::permissions::{ROLE_ADMIN, ROLE_GUEST, ROLE_MEMBER, ROLE_ORGANIZER};

    use super::*;

    fn id(n: u8) -> UserId {
        [n; 32]
    }

    #[test]
    fn a_fresh_policy_has_the_builtin_roles_and_no_admin() {
        let p = Policy::new();
        assert_eq!(p.roles.len(), 4);
        assert!(!p.has_admin());
    }

    #[test]
    fn permissions_are_the_union_of_roles() {
        let mut p = Policy::new();
        p.create_user(id(1), "alice", 0, None);
        assert_eq!(p.permissions_of(&id(1)), 0);
        p.grant_role(&id(1), ROLE_GUEST);
        assert_eq!(p.permissions_of(&id(1)), permissions::GUEST_PERMISSIONS);
        p.grant_role(&id(1), ROLE_ADMIN);
        assert!(permissions::has(
            p.permissions_of(&id(1)),
            permissions::ADMIN
        ));
        assert!(p.has_admin());
        assert_eq!(p.rank_of(&id(1)), protocol::permissions::RANK_ADMIN);
    }

    #[test]
    fn keys_resolve_to_users_unless_retired() {
        let mut p = Policy::new();
        assert_eq!(p.resolve_key(&id(9)), KeyLookup::Unknown);
        p.create_user(id(1), "alice", 0, None);
        p.link_key(id(9), id(1), 0);
        assert_eq!(p.resolve_key(&id(9)), KeyLookup::User(id(1)));
        p.keys.get_mut(&id(9)).unwrap().retired_at = Some(5);
        assert_eq!(p.resolve_key(&id(9)), KeyLookup::Retired);
    }

    fn invite(expires_at: Option<WallMs>, max_uses: Option<u32>) -> InviteRecord {
        InviteRecord {
            created_by: None,
            created_at: 0,
            expires_at,
            max_uses,
            uses: 0,
            grants_role: ROLE_GUEST,
            revoked_at: None,
        }
    }

    #[test]
    fn invites_fail_for_exactly_the_four_reasons() {
        let mut p = Policy::new();
        assert_eq!(
            p.check_invite("NOPE", 0).unwrap_err(),
            InviteInvalidReason::Unknown
        );
        p.add_invite("EXPIRED".into(), invite(Some(100), None));
        assert!(p.check_invite("EXPIRED", 99).is_ok());
        assert_eq!(
            p.check_invite("EXPIRED", 100).unwrap_err(),
            InviteInvalidReason::Expired
        );
        p.add_invite("ONCE".into(), invite(None, Some(1)));
        assert!(p.check_invite("ONCE", 0).is_ok());
        p.use_invite("ONCE");
        assert_eq!(
            p.check_invite("ONCE", 0).unwrap_err(),
            InviteInvalidReason::Exhausted
        );
        p.add_invite("GONE".into(), invite(None, None));
        assert!(p.revoke_invite("GONE", 5));
        assert!(!p.revoke_invite("GONE", 6));
        assert_eq!(
            p.check_invite("GONE", 10).unwrap_err(),
            InviteInvalidReason::Revoked
        );
    }

    #[test]
    fn server_bans_match_key_user_or_address_until_they_expire() {
        let mut p = Policy::new();
        let ip: IpAddr = "10.0.0.7".parse().unwrap();
        let stored = p.add_restriction(Restriction {
            id: 0,
            user_id: Some(id(1)),
            key_hash: Some(id(9)),
            ip: Some(ip),
            kind: RestrictionKind::Server,
            until: Some(1_000),
            reason: "spam".into(),
            by: None,
            created_at: 0,
        });
        assert_eq!(stored.id, 1);
        let other_ip: IpAddr = "10.0.0.8".parse().unwrap();
        assert!(p.server_ban(None, &id(9), Some(other_ip), 10).is_some());
        assert!(p
            .server_ban(Some(&id(1)), &id(2), Some(other_ip), 10)
            .is_some());
        assert!(p.server_ban(None, &id(2), Some(ip), 10).is_some());
        assert!(p.server_ban(None, &id(2), Some(other_ip), 10).is_none());
        assert!(p.server_ban(None, &id(2), None, 10).is_none());
        assert!(
            p.server_ban(None, &id(9), Some(ip), 1_000).is_none(),
            "expired"
        );
        p.expire_restrictions(1_000);
        assert!(p.restrictions.is_empty());
    }

    /// The fallout of a key or address ban follows the rank rule at
    /// admission too (review P2-10).
    #[test]
    fn a_key_or_address_ban_by_a_person_spares_admins_and_equal_ranks() {
        let mut p = Policy::new();
        let ip: IpAddr = "10.0.0.7".parse().unwrap();
        for (n, role) in [
            (1, ROLE_ADMIN),
            (2, ROLE_ORGANIZER),
            (3, ROLE_MEMBER),
            (4, ROLE_ORGANIZER),
        ] {
            p.create_user(id(n), "someone", 0, None);
            p.grant_role(&id(n), role);
        }
        let ban = |by| Restriction {
            id: 0,
            user_id: None,
            key_hash: Some(id(50)),
            ip: Some(ip),
            kind: RestrictionKind::Server,
            until: None,
            reason: String::new(),
            by,
            created_at: 0,
        };
        // An organizer bans an address.
        let stored = p.add_restriction(ban(Some(id(2))));
        let hit = |p: &Policy, user: u8| {
            p.server_ban(Some(&id(user)), &id(90), Some(ip), 10)
                .is_some()
        };
        assert!(!hit(&p, 1), "an administrator");
        assert!(!hit(&p, 2), "the one who issued it");
        assert!(!hit(&p, 4), "an equal rank");
        assert!(hit(&p, 3), "a lower rank");
        assert!(
            p.server_ban(None, &id(90), Some(ip), 10).is_some(),
            "a stranger"
        );
        assert!(
            p.server_ban(Some(&id(1)), &id(50), None, 10).is_none(),
            "the key match is fallout too"
        );

        // The operator's own ban (the CLI) spares nobody.
        assert!(p.remove_restriction(stored.id));
        p.add_restriction(ban(None));
        assert!(hit(&p, 1));
    }

    #[test]
    fn a_members_name_is_reserved_against_guests_only() {
        let mut p = Policy::new();
        p.create_user(id(1), "Alice", 0, None);
        p.grant_role(&id(1), ROLE_MEMBER);
        p.create_user(id(2), "bob", 0, None);
        p.grant_role(&id(2), ROLE_GUEST);
        assert!(p.name_held_by_member("alice", &id(3)), "case-insensitive");
        assert!(!p.name_held_by_member("alice", &id(1)), "your own name");
        assert!(
            !p.name_held_by_member("bob", &id(3)),
            "guests reserve nothing"
        );
    }

    #[test]
    fn codes_use_the_unambiguous_alphabet_and_normalize_on_input() {
        let code = generate_invite_code();
        assert_eq!(code.len(), INVITE_CODE_LEN);
        assert!(code.bytes().all(|b| INVITE_ALPHABET.contains(&b)));
        assert!(!code.contains(['0', 'O', '1', 'l', 'I']));
        assert_eq!(
            protocol::invite::normalize_code("k7fq-2mx9 abcd").as_deref(),
            Some("K7FQ2MX9ABCD")
        );
        assert_eq!(
            protocol::invite::normalize_code("K7FQ2MX9ABC0"),
            None,
            "0 is not in the alphabet"
        );
        assert_eq!(
            protocol::invite::normalize_code("K7FQ2MX9ABC"),
            None,
            "too short"
        );
        assert_ne!(generate_invite_code(), generate_invite_code());
    }
}
