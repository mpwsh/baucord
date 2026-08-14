//! Roster state — the user list a shell renders.
//!
//! `ClientId` is a plain `u64` here (renet's `ClientId` is one); the
//! crate stays free of networking deps.

use std::collections::{HashMap, HashSet};

pub type ClientId = u64;

/// Per-peer audio settings mirror (`audio::PeerAudioSettings` shape).
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // mirror of the wire shape
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
}

impl UserState {
    pub fn new(username: impl Into<String>, audio: PeerAudio) -> Self {
        Self {
            username: username.into(),
            is_speaking: false,
            audio,
            is_self_muted: false,
            is_self_deafened: false,
        }
    }
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

/// Project the user map into sorted display rows (by username, same
/// order the FLTK sidebar used). `our_id` marks the self row; for it,
/// `local_*` state substitutes the broadcast state (which is always
/// stale for ourselves).
#[allow(clippy::cast_possible_truncation)]
// implicit_hasher: the shell hands us its std maps; genericizing the
// hasher buys nothing here.
#[allow(clippy::implicit_hasher)]
#[must_use]
pub fn rows(
    users: &HashMap<ClientId, UserState>,
    our_id: Option<ClientId>,
    sharers: &HashSet<ClientId>,
    local_muted: bool,
    local_deafened: bool,
) -> Vec<RosterRow> {
    let mut sorted: Vec<_> = users.iter().collect();
    sorted.sort_by(|(_, a), (_, b)| a.username.cmp(&b.username));
    sorted
        .into_iter()
        .map(|(&id, st)| {
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
        })
        .collect()
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

    #[test]
    fn rows_sort_by_username_and_mark_self() {
        let mut users = HashMap::new();
        users.insert(2, UserState::new("zoe", PeerAudio::default()));
        users.insert(1, UserState::new("amy", PeerAudio::default()));
        let rows = rows(&users, Some(2), &HashSet::default(), true, false);
        assert_eq!(rows[0].name, "amy");
        assert_eq!(rows[1].name, "zoe");
        assert!(rows[1].is_us);
        assert!(rows[1].self_muted, "self row uses local state");
    }
}
