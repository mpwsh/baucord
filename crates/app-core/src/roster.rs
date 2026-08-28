//! Roster state — the user list a shell renders.
//!
//! `ClientId` is a plain `u64` here (renet's `ClientId` is one); the
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
        }
    }
}

/// One voice channel as the roster needs it (mirror of the wire
/// `ChannelInfo` — this crate stays free of networking deps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMeta {
    pub id: u16,
    pub name: String,
    /// Joining requires a password (shown as a lock).
    pub locked: bool,
    /// Member cap, `None` = unlimited.
    pub user_limit: Option<u16>,
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
}

/// A channel header row for the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRow {
    pub id: u16,
    pub name: String,
    pub locked: bool,
    /// Current member count.
    pub members: u16,
    pub user_limit: Option<u16>,
    /// This is the channel we are in (highlighted; clicking is a no-op).
    pub is_ours: bool,
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
        }
    };

    let our_channel = our_id.and_then(|id| users.get(&id)).map(|u| u.channel_id);
    let mut sorted: Vec<_> = users.iter().collect();
    sorted.sort_by(|(_, a), (_, b)| a.username.cmp(&b.username));

    let mut items = Vec::with_capacity(channels.len() + users.len());
    for channel in channels {
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
        for id in [0u64, 1, u64::from(u32::MAX), u64::MAX, 0x8000_0000_0000_0001] {
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
        }
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
        let RosterItem::Channel(lobby) = &items[0] else { panic!("expected header") };
        assert_eq!(lobby.name, "Lobby");
        assert_eq!(lobby.members, 2);
        assert!(lobby.is_ours, "we are in the Lobby");
        assert!(matches!(&items[1], RosterItem::User(u) if u.name == "amy"));
        let RosterItem::User(us) = &items[2] else { panic!("expected user") };
        assert_eq!(us.name, "zoe");
        assert!(us.is_us);
        assert!(us.self_muted, "self row uses local state");
        let RosterItem::Channel(gaming) = &items[3] else { panic!("expected header") };
        assert_eq!(gaming.name, "Gaming");
        assert!(!gaming.is_ours);
        assert!(matches!(&items[4], RosterItem::User(u) if u.name == "bob"));
    }

    #[test]
    fn empty_channels_still_render_headers() {
        let channels = [meta(0, "Lobby"), meta(1, "AFK")];
        let mut users = HashMap::new();
        users.insert(1, UserState::new("amy", PeerAudio::default(), 0));
        let items = grouped_rows(&channels, &users, Some(1), &HashSet::default(), false, false);
        assert_eq!(items.len(), 3);
        let RosterItem::Channel(afk) = &items[2] else { panic!("expected header") };
        assert_eq!(afk.members, 0);
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
