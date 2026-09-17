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
/// Progress: xp and levels (`UserDetails.level`, `ProgressUpdate`).
/// Bit 5 was reserved for achievements, which no server ever
/// announced; progress replaces them.
pub const PROGRESS: u64 = 1 << 5;
/// Content-addressed blob fetches (files, avatars).
pub const BLOBS: u64 = 1 << 6;
/// Invite codes as the door: `Auth::Key` / `Auth::Invite`, the
/// `NeedInvite` / `InviteInvalid` / `Banned` refusals, `Session`.
pub const INVITES: u64 = 1 << 7;
/// Direct messages.
pub const DM: u64 = 1 << 8;
/// Channels created and managed from the client.
pub const CHANNELS_UI: u64 = 1 << 9;
/// Admin commands over the wire and the admin channel.
pub const ADMIN: u64 = 1 << 10;
/// Everyone's xp on the wire (`UserProgress`), for the card's bar.
/// Its own bit rather than [`PROGRESS`]: a 1.1.0 client negotiated
/// that one and cannot decode the variant appended in 1.1.2.
pub const PROGRESS_ALL: u64 = 1 << 11;
/// The role table on the wire (`RoleList`): custom roles by name.
pub const ROLE_LIST: u64 = 1 << 12;
/// Files in conversations (spec §7): `SendFile`, `FileMessage`,
/// `HistoryAttachments`, and the upload path open to any type.
pub const FILES: u64 = 1 << 13;
/// The admin panel's commands (spec §8): `AuditLog`, roles over the
/// wire, `ListBlobs`/`DeleteBlob`, `ServerStats`.
pub const ADMIN_PANEL: u64 = 1 << 14;
/// The audit as it happens (`AuditLine`), for the admin channel.
pub const ADMIN_FEED: u64 = 1 << 15;
/// The default channel is a lobby (spec §12, 1.1.5): a session there
/// is announced to nobody until it joins a channel. The server
/// behaves this way for every session; the bit tells a client that
/// it may hide the lobby row and render "joined <channel>".
pub const LOBBY: u64 = 1 << 16;

/// What a 1.0 client can do.
pub const CLIENT_1_0: u64 = TELEMETRY | IDENTITY;
/// What a 1.0 server offers: it logs telemetry; it only *logs* client
/// keys, so it does not announce [`IDENTITY`].
pub const SERVER_1_0: u64 = TELEMETRY;
/// What a 1.1 client can do: it logs in by key, presents invites,
/// renders roles and restrictions, replays history, shows progress,
/// and makes channels.
pub const CLIENT_1_1: u64 = CLIENT_1_0
    | INVITES
    | ROLES
    | HISTORY
    | PROGRESS
    | CHANNELS_UI
    | PROFILES
    | BLOBS
    | DM
    | PROGRESS_ALL
    | ROLE_LIST
    | FILES
    | ADMIN_PANEL
    | ADMIN_FEED
    | LOBBY;
/// What a 1.1 server offers: keys are the login, invites the door,
/// roles decide what a session may do, and it keeps history.
pub const SERVER_1_1: u64 = TELEMETRY
    | IDENTITY
    | INVITES
    | ROLES
    | HISTORY
    | PROGRESS
    | CHANNELS_UI
    | PROFILES
    | BLOBS
    | DM
    | PROGRESS_ALL
    | ROLE_LIST
    | FILES
    | ADMIN_PANEL
    | ADMIN_FEED
    | LOBBY;

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
            PROGRESS,
            BLOBS,
            INVITES,
            DM,
            CHANNELS_UI,
            ADMIN,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.count_ones(), 1);
            for b in &all[i + 1..] {
                assert_eq!(a & b, 0);
            }
        }
    }
}
