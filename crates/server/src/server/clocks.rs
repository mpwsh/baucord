//! The channel timer (1.5, feature bit `CHANNEL_CLOCK`): how long a
//! voice channel has had someone in it, from the first join into an
//! empty channel to the last leave.
//!
//! The server owns the start and sends elapsed milliseconds on each
//! edge; a client counts on from its own monotonic clock, so a wrong
//! wall clock on either side changes nothing. Kept by the channel's
//! name — the one thing a renumbering list change keeps — and never
//! persisted: a restart drops every session, so every room empties.

use protocol::{ClientId, ServerMessage};

use super::ChatServer;

impl ChatServer {
    /// The channels a timer runs for: not the lobby, not AFK.
    fn timed(&self, channel_id: u16) -> bool {
        channel_id != self.default_channel && self.afk_channel() != Some(channel_id)
    }

    fn channel_key(&self, channel_id: u16) -> Option<String> {
        self.channel_list
            .get(usize::from(channel_id))
            .map(|c| c.name.trim().to_string())
    }

    /// The channel's timer as of now: `None` when it is empty.
    pub(super) fn running_ms(&self, channel_id: u16) -> Option<u64> {
        let since = self.occupied_since.get(&self.channel_key(channel_id)?)?;
        Some(u64::try_from(self.now.duration_since(*since).as_millis()).unwrap_or(u64::MAX))
    }

    /// After `room_members` changed: start the timers of channels that
    /// just filled, stop those that just emptied, and say so to every
    /// session. Called from `index_rooms`, the one place membership
    /// lands.
    pub(super) fn update_channel_clocks(&mut self) {
        let mut occupied: Vec<(String, u16)> = Vec::new();
        for channel_id in self.room_members.keys().copied() {
            if self.timed(channel_id)
                && let Some(key) = self.channel_key(channel_id)
            {
                occupied.push((key, channel_id));
            }
        }
        let mut edges = Vec::new();
        for (key, channel_id) in &occupied {
            if !self.occupied_since.contains_key(key) {
                self.occupied_since.insert(key.clone(), self.now);
                edges.push(ServerMessage::ChannelClock {
                    channel_id: *channel_id,
                    running_ms: Some(0),
                });
            }
        }
        let gone: Vec<String> = self
            .occupied_since
            .keys()
            .filter(|k| !occupied.iter().any(|(o, _)| o == *k))
            .cloned()
            .collect();
        for key in gone {
            self.occupied_since.remove(&key);
            // A channel that left the list has no id to name; the list
            // update that removed it says enough.
            if let Some(pos) = self.channel_list.iter().position(|c| c.name.trim() == key)
                && let Ok(channel_id) = u16::try_from(pos)
            {
                edges.push(ServerMessage::ChannelClock {
                    channel_id,
                    running_ms: None,
                });
            }
        }
        if edges.is_empty() {
            return;
        }
        let everyone = self.clients();
        for msg in &edges {
            self.send_to(&everyone, msg);
        }
    }

    /// Every running timer, to one session: after `Init`, and after a
    /// `ChannelListUpdated` (which renumbers). An empty channel needs
    /// no line: a client starts with none running.
    pub(super) fn send_channel_clocks(&mut self, client: ClientId) {
        let running: Vec<ServerMessage> = (0..self.channel_list.len())
            .filter_map(|i| u16::try_from(i).ok())
            .filter_map(|channel_id| {
                self.running_ms(channel_id)
                    .map(|ms| ServerMessage::ChannelClock {
                        channel_id,
                        running_ms: Some(ms),
                    })
            })
            .collect();
        for msg in &running {
            self.send_msg(client, msg);
        }
    }
}
