//! Capability bits exchanged in `Hello.features` / `Welcome.features`.
//!
//! `postcard` is positional and not self-describing: adding a field to a
//! struct, or sending an enum variant the peer does not know, breaks
//! decoding. So within one ALPN version every addition is a *new
//! message* that a peer sends only if the other side announced the bit.
//! Bits are allocated here and **never reused**.
//!
//! A peer's effective feature set is the intersection of what it
//! announced and what the other side announced ([`negotiate`]).

/// Client → server telemetry samples and events (§9.1).
pub const TELEMETRY: u64 = 1 << 0;
/// The server honors `Hello.client_key` and persists identities.
pub const IDENTITY: u64 = 1 << 1;
/// Profiles and avatars.
pub const PROFILES: u64 = 1 << 2;
/// Chat history replay.
pub const HISTORY: u64 = 1 << 3;
/// Roles and permissions.
pub const ROLES: u64 = 1 << 4;
/// Achievements.
pub const ACHIEVEMENTS: u64 = 1 << 5;
/// Content-addressed blob fetches.
pub const BLOBS: u64 = 1 << 6;

/// What a 1.0 client can do.
pub const CLIENT_1_0: u64 = TELEMETRY | IDENTITY;
/// What a 1.0 server offers: it logs telemetry; it only *logs* client
/// keys, so it does not announce [`IDENTITY`].
pub const SERVER_1_0: u64 = TELEMETRY;

/// Bits both sides announced.
#[must_use]
pub const fn negotiate(ours: u64, theirs: u64) -> u64 {
    ours & theirs
}

/// Whether `bit` is in `features`.
#[must_use]
pub const fn has(features: u64, bit: u64) -> bool {
    features & bit != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_is_the_intersection() {
        assert_eq!(negotiate(CLIENT_1_0, SERVER_1_0), TELEMETRY);
        assert!(has(negotiate(CLIENT_1_0, SERVER_1_0), TELEMETRY));
        assert!(!has(negotiate(CLIENT_1_0, SERVER_1_0), IDENTITY));
    }

    #[test]
    fn bits_are_distinct() {
        let all = [
            TELEMETRY,
            IDENTITY,
            PROFILES,
            HISTORY,
            ROLES,
            ACHIEVEMENTS,
            BLOBS,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.count_ones(), 1);
            for b in &all[i + 1..] {
                assert_eq!(a & b, 0);
            }
        }
    }
}
