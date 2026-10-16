//! Roster state — the user list a shell renders.
//!
//! `ClientId` is a plain `u64` here (the transport's peer id is one); the
//! crate stays free of networking deps.

use std::collections::{HashMap, HashSet};

pub type ClientId = u64;

/// Per-peer audio settings mirror (`audio::PeerAudioSettings` shape).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeerAudio {
    pub volume: f32,
    pub is_muted: bool,
}

impl Default for PeerAudio {
    fn default() -> Self {
        Self {
            volume: 1.0,
            is_muted: false,
        }
    }
}

/// The first letter of a name, upper-cased, for the avatar tile when
/// there is no picture; `?` for an empty name.
#[must_use]
pub fn initial_of(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map_or_else(|| "?".to_string(), |c| c.to_uppercase().collect())
}

/// A muted, deterministic tint for a name's fallback tile: the same
/// person gets the same square everywhere, and none of the tints is
/// loud enough to compete with a real name color. Hue from a hash of
/// the name, fixed saturation and lightness (HSL 45% / 32%).
#[must_use]
pub fn tile_tint(name: &str) -> [u8; 3] {
    let mut h: u32 = 2_166_136_261;
    for b in name.to_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(16_777_619);
    }
    let hue = f64::from(h % 360);
    hsl_to_rgb(hue, 0.45, 0.32)
}

// h, s, l, c, x and r, g, b are the names the HSL→RGB formula is
// written with everywhere; longer ones would hide it.
#[expect(clippy::many_single_char_names)]
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    ]
}

/// The highest built-in role a user holds, as the roster marks it.
/// The server's tables decide it (`UserDetails`); the client only
/// draws it. `Unknown` until the details arrive, or on a server that
/// does not send them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Role {
    #[default]
    Unknown,
    Guest,
    Member,
    Organizer,
    Admin,
}

impl Role {
    /// The value a Slint `int` field carries: 0 unknown … 4 admin.
    #[must_use]
    pub fn as_int(self) -> i32 {
        match self {
            Role::Unknown => 0,
            Role::Guest => 1,
            Role::Member => 2,
            Role::Organizer => 3,
            Role::Admin => 4,
        }
    }

    /// The inverse of [`Role::as_int`] for the four real roles; `None`
    /// for anything else (a pick the UI could not have made).
    #[must_use]
    pub fn from_int(value: i32) -> Option<Self> {
        match value {
            1 => Some(Role::Guest),
            2 => Some(Role::Member),
            3 => Some(Role::Organizer),
            4 => Some(Role::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserState {
    pub username: String,
    pub is_speaking: bool,
    pub audio: PeerAudio,
    /// Remote user's self-reported mute state (distinct from local per-user mute).
    pub is_self_muted: bool,
    pub is_self_deafened: bool,
    /// Voice channel this user is in (server-authoritative).
    pub channel_id: u16,
    /// From `UserDetails`; `Role::Unknown` until it arrives.
    pub role: Role,
    /// From `UserDetails`; 0 until it arrives.
    pub level: u16,
    /// From `UserProgress` (spec §9.1); `None` until it arrives.
    pub xp: Option<u64>,
}

impl UserState {
    pub fn new(username: impl Into<String>, audio: PeerAudio, channel_id: u16) -> Self {
        Self {
            username: username.into(),
            is_speaking: false,
            audio,
            is_self_muted: false,
            is_self_deafened: false,
            channel_id,
            role: Role::Unknown,
            level: 0,
            xp: None,
        }
    }
}

/// One voice channel as the roster needs it (mirror of the wire
/// `ChannelInfo` plus `ChannelDetails` — this crate stays free of
/// networking deps).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // display facts, not a state machine
pub struct ChannelMeta {
    pub id: u16,
    pub name: String,
    /// Joining requires a password (shown as a lock).
    pub locked: bool,
    /// Member cap, `None` = unlimited.
    pub user_limit: Option<u16>,
    /// A `min_role` gate exists (members-only, staff-only).
    pub members_only: bool,
    /// Deleted after a while empty (a user channel without "keep").
    pub temporary: bool,
    /// The server's AFK channel: voice goes nowhere there.
    pub is_afk: bool,
    /// This session's user created it (edit/delete in its menu).
    pub owned: bool,
    /// Defined in the server's config file: cannot be deleted from
    /// the client, nor renamed.
    pub from_config: bool,
    /// Gated above this session's rank (spec §12): the header and its
    /// members are left out of the roster entirely.
    pub hidden: bool,
}

/// Snapshot row for the UI, in display order.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // display flags, not a state machine
pub struct RosterRow {
    pub client_id: ClientId,
    pub name: String,
    /// 100 = default; the UI shows a badge when != 100.
    pub volume_pct: i32,
    pub is_us: bool,
    pub speaking: bool,
    pub local_muted: bool,
    pub self_muted: bool,
    pub self_deafened: bool,
    pub sharing: bool,
    pub role: Role,
    pub level: u16,
    /// Total xp, when the server said (the card's bar).
    pub xp: Option<u64>,
    /// The channel they are in (the "Move to" menu greys it out).
    pub channel_id: u16,
}

/// A channel header row for the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // display flags, not a state machine
pub struct ChannelRow {
    pub id: u16,
    pub name: String,
    pub locked: bool,
    /// Current member count.
    pub members: u16,
    pub user_limit: Option<u16>,
    /// This is the channel we are in (highlighted; clicking is a no-op).
    pub is_ours: bool,
    pub members_only: bool,
    pub temporary: bool,
    pub is_afk: bool,
    pub owned: bool,
    pub from_config: bool,
}

/// One sidebar row: a channel header or a user beneath one.
#[derive(Debug, Clone, PartialEq)]
pub enum RosterItem {
    Channel(ChannelRow),
    User(RosterRow),
}

/// Project channels + users into the TeamSpeak-style grouped sidebar:
/// every channel (occupied or not, in config order) as a header row,
/// its members sorted by username beneath it. `our_id` marks the self
/// row; for it, `local_*` state substitutes the broadcast state (which
/// is always stale for ourselves).
///
/// A `hidden` channel (gated above this session's rank) is skipped
/// along with everyone in it — a members-only room is one guests never
/// see, people included.
///
/// Users pointing at a channel id with no metadata (a server bug, but
/// hiding people would make it worse) are appended at the end without
/// a header.
#[allow(clippy::cast_possible_truncation)]
// implicit_hasher: the shell hands us its std maps; genericizing the
// hasher buys nothing here.
#[allow(clippy::implicit_hasher)]
#[must_use]
pub fn grouped_rows(
    channels: &[ChannelMeta],
    users: &HashMap<ClientId, UserState>,
    our_id: Option<ClientId>,
    sharers: &HashSet<ClientId>,
    local_muted: bool,
    local_deafened: bool,
) -> Vec<RosterItem> {
    let user_row = |id: ClientId, st: &UserState| {
        let is_us = Some(id) == our_id;
        let (self_muted, self_deafened) = if is_us {
            (local_muted, local_deafened)
        } else {
            (st.is_self_muted, st.is_self_deafened)
        };
        RosterRow {
            client_id: id,
            name: st.username.clone(),
            volume_pct: (st.audio.volume * 100.0).round() as i32,
            is_us,
            speaking: st.is_speaking,
            local_muted: st.audio.is_muted,
            self_muted,
            self_deafened,
            sharing: sharers.contains(&id),
            role: st.role,
            level: st.level,
            xp: st.xp,
            channel_id: st.channel_id,
        }
    };

    let our_channel = our_id.and_then(|id| users.get(&id)).map(|u| u.channel_id);
    // By name as a person reads it ("ana" before "Zoe"; bytes put every
    // capital first), then by client id: two of the same name kept no
    // order of their own, the map's from one rebuild to the next (review
    // 2026-10, P3-50).
    let mut sorted: Vec<_> = users.iter().collect();
    sorted.sort_by_cached_key(|(id, st)| (st.username.to_lowercase(), **id));

    let mut items = Vec::with_capacity(channels.len() + users.len());
    for channel in channels {
        if channel.hidden {
            continue;
        }
        let members: Vec<_> = sorted
            .iter()
            .filter(|(_, st)| st.channel_id == channel.id)
            .collect();
        items.push(RosterItem::Channel(ChannelRow {
            id: channel.id,
            name: channel.name.clone(),
            locked: channel.locked,
            members: members.len() as u16,
            user_limit: channel.user_limit,
            is_ours: our_channel == Some(channel.id),
            members_only: channel.members_only,
            temporary: channel.temporary,
            is_afk: channel.is_afk,
            owned: channel.owned,
            from_config: channel.from_config,
        }));
        for (id, st) in members {
            items.push(RosterItem::User(user_row(**id, st)));
        }
    }

    // Defensive: users in a channel the metadata doesn't know.
    let known: HashSet<u16> = channels.iter().map(|c| c.id).collect();
    for (id, st) in &sorted {
        if !known.contains(&st.channel_id) {
            items.push(RosterItem::User(user_row(**id, st)));
        }
    }
    items
}

/// Split a u64 client id into two i32 halves for a Slint struct (which
/// has no 64-bit integer type) and back, losslessly.
#[must_use]
pub fn split_id(id: ClientId) -> (i32, i32) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    {
        ((id & 0xFFFF_FFFF) as u32 as i32, (id >> 32) as u32 as i32)
    }
}

#[must_use]
pub fn join_id(lo: i32, hi: i32) -> ClientId {
    #[allow(clippy::cast_sign_loss)]
    {
        (u64::from(hi as u32) << 32) | u64::from(lo as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_split_roundtrips_at_extremes() {
        for id in [
            0u64,
            1,
            u64::from(u32::MAX),
            u64::MAX,
            0x8000_0000_0000_0001,
        ] {
            let (lo, hi) = split_id(id);
            assert_eq!(join_id(lo, hi), id);
        }
    }

    fn meta(id: u16, name: &str) -> ChannelMeta {
        ChannelMeta {
            id,
            name: name.to_string(),
            locked: false,
            user_limit: None,
            members_only: false,
            temporary: false,
            is_afk: false,
            owned: false,
            from_config: true,
            hidden: false,
        }
    }

    /// Names sort as people read them, and two of a name by client id
    /// (review 2026-10, P3-50).
    #[test]
    fn members_sort_by_name_ignoring_case_then_by_id() {
        let channels = [meta(0, "Lobby")];
        let mut users = HashMap::new();
        users.insert(9, UserState::new("ana", PeerAudio::default(), 0));
        users.insert(4, UserState::new("Zoe", PeerAudio::default(), 0));
        users.insert(7, UserState::new("bo", PeerAudio::default(), 0));
        users.insert(5, UserState::new("bo", PeerAudio::default(), 0));
        let items = grouped_rows(&channels, &users, None, &HashSet::default(), true, false);
        let order: Vec<(String, u64)> = items
            .iter()
            .filter_map(|item| match item {
                RosterItem::User(u) => Some((u.name.clone(), u.client_id)),
                RosterItem::Channel(_) => None,
            })
            .collect();
        let expected =
            [("ana", 9), ("bo", 5), ("bo", 7), ("Zoe", 4)].map(|(name, id)| (name.to_string(), id));
        assert_eq!(order, expected);
    }

    #[test]
    fn grouped_rows_order_channels_then_sorted_members() {
        let channels = [meta(0, "Lobby"), meta(1, "Gaming")];
        let mut users = HashMap::new();
        users.insert(2, UserState::new("zoe", PeerAudio::default(), 0));
        users.insert(1, UserState::new("amy", PeerAudio::default(), 0));
        users.insert(3, UserState::new("bob", PeerAudio::default(), 1));
        let items = grouped_rows(&channels, &users, Some(2), &HashSet::default(), true, false);

        // Lobby header, amy, zoe(us), Gaming header, bob.
        assert_eq!(items.len(), 5);
        let RosterItem::Channel(lobby) = &items[0] else {
            panic!("expected header")
        };
        assert_eq!(lobby.name, "Lobby");
        assert_eq!(lobby.members, 2);
        assert!(lobby.is_ours, "we are in the Lobby");
        assert!(matches!(&items[1], RosterItem::User(u) if u.name == "amy"));
        let RosterItem::User(us) = &items[2] else {
            panic!("expected user")
        };
        assert_eq!(us.name, "zoe");
        assert!(us.is_us);
        assert!(us.self_muted, "self row uses local state");
        let RosterItem::Channel(gaming) = &items[3] else {
            panic!("expected header")
        };
        assert_eq!(gaming.name, "Gaming");
        assert!(!gaming.is_ours);
        assert!(matches!(&items[4], RosterItem::User(u) if u.name == "bob"));
    }

    #[test]
    fn empty_channels_still_render_headers() {
        let channels = [meta(0, "Lobby"), meta(1, "AFK")];
        let mut users = HashMap::new();
        users.insert(1, UserState::new("amy", PeerAudio::default(), 0));
        let items = grouped_rows(
            &channels,
            &users,
            Some(1),
            &HashSet::default(),
            false,
            false,
        );
        assert_eq!(items.len(), 3);
        let RosterItem::Channel(afk) = &items[2] else {
            panic!("expected header")
        };
        assert_eq!(afk.members, 0);
    }

    #[test]
    fn tiles_are_stable_and_muted() {
        assert_eq!(tile_tint("Alice"), tile_tint("alice"));
        assert_ne!(tile_tint("alice"), tile_tint("bob"));
        for name in ["a", "bob", "carol", "zed"] {
            let [r, g, b] = tile_tint(name);
            let max = r.max(g).max(b);
            assert!(max < 150, "{name}: {r},{g},{b} is louder than a tint");
        }
        assert_eq!(initial_of("  émile"), "É");
        assert_eq!(initial_of(""), "?");
    }

    #[test]
    fn hidden_channels_take_their_members_with_them() {
        let mut staff = meta(1, "Staff");
        staff.hidden = true;
        let channels = [meta(0, "Lobby"), staff];
        let mut users = HashMap::new();
        users.insert(1, UserState::new("amy", PeerAudio::default(), 0));
        users.insert(2, UserState::new("boss", PeerAudio::default(), 1));
        let items = grouped_rows(
            &channels,
            &users,
            Some(1),
            &HashSet::default(),
            false,
            false,
        );
        // Lobby header + amy; no Staff header, no boss.
        assert_eq!(items.len(), 2);
        assert!(items
            .iter()
            .all(|i| !matches!(i, RosterItem::User(u) if u.name == "boss")));
    }

    #[test]
    fn unknown_channel_users_are_not_hidden() {
        let channels = [meta(0, "Lobby")];
        let mut users = HashMap::new();
        users.insert(1, UserState::new("ghost", PeerAudio::default(), 42));
        let items = grouped_rows(&channels, &users, None, &HashSet::default(), false, false);
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[1], RosterItem::User(u) if u.name == "ghost"));
    }
}
