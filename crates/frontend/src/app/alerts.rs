//! Alerts on a phone (ANDROID-SPEC.md D52, IOS-SPEC.md D50): what reaches
//! the user while the app is in the background — a system notification,
//! which the phone's own modes govern — and how an alert chime plays at
//! all: through the voice engine where the ringer is on, not at all while
//! the phone is silenced, through the system on iOS (the voice session's
//! audio ignores the ring/silent switch, a notification's sound does not).
//! The desktop has no notifications: its chimes go through the engine as
//! they always did.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;
use crate::notice::{ChimeRoute, Notice, NoticeKind};

/// The notification key of the call starting.
const JOIN_KEY: &str = "join";

/// A call starting is announced at most this often: people step in and
/// out while they get settled, and one notice is the news.
const FIRST_JOIN_EVERY: Duration = Duration::from_mins(10);

/// The longest message preview a notification carries.
const PREVIEW_CHARS: usize = 240;

/// The notification key of the DM conversation with `user`.
fn dm_key(user: &client::UserId) -> String {
    format!("dm:{}", hex(user))
}

/// The user id `hex` spells (64 hex digits), as [`hex`] wrote it.
fn user_id_from_hex(text: &str) -> Option<client::UserId> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// `text` cut to a notification's length, on a char boundary.
fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(PREVIEW_CHARS).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

impl AppState {
    /// An alert chime (a DM, a mention), the way this host and the
    /// phone's sound mode allow. `notice` is what iOS posts to play it
    /// through the system.
    pub(super) fn play_alert_chime(&self, sound: SoundType, notice: Notice) {
        match crate::host::chime_route() {
            // Out of a voice room an Android engine plays nothing (no
            // stream is open, D53): the host plays the chime itself.
            ChimeRoute::App if !self.voice_in_room && crate::host::play_chime() => {}
            ChimeRoute::App => {
                self.voice.tx.send(VoiceControl::PlaySound(sound)).ok();
            }
            ChimeRoute::Muted => info!("alert chime skipped: the phone is silenced"),
            // Only on screen: the delegate that makes it sound-only runs
            // only then — in the background it would be a full banner
            // nobody asked for (a mention, a DM with notifications off).
            ChimeRoute::System if host_visible() => crate::host::notify(&Notice {
                on_screen: true,
                ..notice
            }),
            ChimeRoute::System => info!("alert chime skipped: in the background"),
        }
    }

    /// A DM arrived. On screen it chimes (if the sound is on); off
    /// screen on a phone it becomes a notification whose tap opens the
    /// conversation — the notification makes the sound then, so the
    /// engine does not chime under it.
    pub(super) fn alert_direct_message(&self, from: client::UserId, sender: &str, text: &str) {
        let sound = self.settings.sounds.direct_message_on();
        let notice = Notice {
            kind: NoticeKind::Message,
            key: dm_key(&from),
            channel: t!("notif-channel-messages"),
            title: sender.to_string(),
            body: preview(text),
            open_dm: Some(hex(&from)),
            sound,
            on_screen: false,
        };
        let notify = crate::host::notifies()
            && !host_visible()
            && self.settings.notifications.messages
            && crate::host::notices_allowed(NoticeKind::Message);
        if notify {
            crate::host::notify(&notice);
        } else if sound {
            // On screen; or the notification would not show (refused,
            // switched off) — the chime, as before notifications.
            self.play_alert_chime(SoundType::DirectMessage, notice);
        }
    }

    /// A message in General names us, and the mention sound is on.
    pub(super) fn alert_mention(&self, sender: &str, text: &str) {
        self.play_alert_chime(
            SoundType::Mention,
            Notice {
                kind: NoticeKind::Message,
                key: "mention".to_string(),
                channel: t!("notif-channel-messages"),
                title: sender.to_string(),
                body: preview(text),
                open_dm: None,
                sound: true,
                on_screen: true,
            },
        );
    }

    /// `joiner` came into the room `channel_id`. If the voice rooms were
    /// empty until now — someone is starting a call — and the app is in
    /// the background on a phone, say so: "Ana joined General".
    pub(super) fn alert_first_join(&mut self, joiner: u64, name: &str, channel_id: u16) {
        if !crate::host::notifies()
            || host_visible()
            || !self.settings.notifications.first_join
            || !crate::host::notices_allowed(NoticeKind::Activity)
            || Some(joiner) == self.our_client_id
            || self.is_quiet_room(channel_id)
        {
            return;
        }
        // Anyone already talking somewhere (us included) means the call
        // started earlier; this is just one more person.
        let busy = self
            .users
            .iter()
            .any(|(id, u)| *id != joiner && !self.is_quiet_room(u.channel_id));
        if busy
            || self
                .last_join_notice
                .is_some_and(|at| at.elapsed() < FIRST_JOIN_EVERY)
        {
            return;
        }
        self.last_join_notice = Some(Instant::now());
        let channel = self.channel_name(channel_id);
        crate::host::notify(&Notice {
            kind: NoticeKind::Activity,
            key: JOIN_KEY.to_string(),
            channel: t!("notif-channel-activity"),
            title: t!("notif-first-join-title", "name" => name, "channel" => channel.as_str()),
            body: t!("notif-first-join-text", "server" => self.server_name.as_str()),
            open_dm: None,
            sound: true,
            on_screen: false,
        });
    }

    /// A room that does not count: the lobby (where everyone waits
    /// unseen), the AFK room, and a room hidden from us (gated above our
    /// rank — its name and its people are not ours to announce). A room
    /// we do not know yet counts as quiet too.
    pub(super) fn is_quiet_room(&self, channel_id: u16) -> bool {
        let lobby = self
            .lobby_name
            .as_deref()
            .is_some_and(|lobby| self.channel_name(channel_id) == lobby);
        lobby
            || self
                .channel_details
                .get(&channel_id)
                .is_some_and(|f| f.is_afk)
            || self
                .channels
                .iter()
                .find(|c| c.id == channel_id)
                .is_none_or(|c| c.hidden || c.is_afk)
    }

    /// The DM with `user` is on screen: its notification is old news.
    pub(super) fn clear_dm_notice(user: &client::UserId) {
        crate::host::cancel_notice(&dm_key(user));
    }

    /// The session ended: a call notice would point at nothing.
    pub(super) fn clear_call_notice() {
        crate::host::cancel_notice(JOIN_KEY);
    }

    /// The app came back on screen: the call notice is old news, and so
    /// is the open conversation's.
    pub(super) fn clear_seen_notices(&self) {
        Self::clear_call_notice();
        if let Some(user) = self.active_dm {
            Self::clear_dm_notice(&user);
        }
    }

    /// A notification about the DM with `dm` (hex) was tapped: open that
    /// conversation, on the chat page.
    // Reached from the phones' `host` only (`stubs/host.rs`).
    #[cfg_attr(feature = "desktop", expect(dead_code, reason = "phones only"))]
    pub(super) fn open_noticed_dm(&mut self, dm: &str) {
        let Some(user) = user_id_from_hex(dm) else {
            info!("notice tapped with a malformed conversation id");
            return;
        };
        // The conversation may be gone from the list (a reconnect clears
        // it, a close hides it): its name from the record, else from
        // whoever is online with that id.
        let name = self
            .dm_meta
            .get(&user)
            .map(|m| m.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| {
                self.details
                    .iter()
                    .find(|(_, (id, _))| *id == user)
                    .map(|(client, _)| self.username_of(*client))
            })
            .unwrap_or_default();
        self.open_dm(user, &name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_id_survives_the_notice_round_trip() {
        let mut id = [0u8; 32];
        for (i, b) in id.iter_mut().enumerate() {
            *b = u8::try_from(i * 7).unwrap();
        }
        assert_eq!(user_id_from_hex(&hex(&id)), Some(id));
        assert_eq!(user_id_from_hex(""), None);
        assert_eq!(user_id_from_hex(&"zz".repeat(32)), None);
    }

    #[test]
    fn a_preview_is_cut_on_a_char_boundary() {
        let long = "é".repeat(PREVIEW_CHARS + 10);
        let cut = preview(&long);
        assert_eq!(cut.chars().count(), PREVIEW_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(preview("hi"), "hi");
    }
}
