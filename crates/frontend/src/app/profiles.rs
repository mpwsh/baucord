//! The roster rows' decoration, avatars and profile looks, renames, the hover cards.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

/// An account's name and look, kept by account (`author_looks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuthorLook {
    pub name: String,
    pub profile: client::protocol::Profile,
}

impl AppState {
    pub fn update_user_list(&mut self) {
        self.main.global::<State>().set_in_lobby(self.in_lobby());
        // Every presence change lands here: a phone's microphone follows
        // the room (ANDROID-SPEC.md D53), and so does the type of what
        // keeps it alive off screen (D63).
        self.sync_voice_room();
        self.sync_keep_alive(false);
        let items = roster::grouped_rows(
            &self.channels,
            &self.users,
            self.our_client_id,
            &self.sharer_ids,
            self.is_muted,
            self.is_deafened,
        );
        // In-place update: write rows positionally, growing/shrinking
        // only when membership changed — preserves the ListView's
        // scroll offset exactly like the FLTK incremental path.
        let converted: Vec<UserRow> = items
            .iter()
            .map(|item| {
                let mut row = ui::convert::roster_item(item);
                if let roster::RosterItem::User(user) = item {
                    self.decorate_user_row(&mut row, user);
                }
                row
            })
            .collect();
        let current = self.users_model.row_count();
        for (i, row) in converted.iter().enumerate() {
            if i < current {
                self.users_model.set_row_data(i, row.clone());
            } else {
                self.users_model.push(row.clone());
            }
        }
        for i in (converted.len()..current).rev() {
            self.users_model.remove(i);
        }
        self.main
            .global::<State>()
            .set_users_count_label(t!("users-count", "count" => self.users.len()).into());
    }

    /// What the roster menu and tooltip need beyond the view model:
    /// the rank rule against us (spec §8) and the session's channel
    /// powers, plus the tooltip text.
    pub(super) fn decorate_user_row(&self, row: &mut UserRow, user: &roster::RosterRow) {
        use client::permissions as p;
        let account = self.account_of(user.client_id);
        self.apply_profile_look(&user.name, user.is_us, account.as_ref(), row);
        if user.level > 0 {
            row.level_label = t!("tooltip-level", "level" => u32::from(user.level)).into();
        }
        row.role_label = self.top_role_name(user.client_id).into();
        if let Some(info) = self.top_role_info(user.client_id) {
            row.role_icon = info.icon.as_str().into();
            if info.color != 0 {
                row.role_color = slint::Color::from_argb_encoded(0xFF00_0000 | info.color);
                row.has_role_color = true;
            }
        }
        if let Some((pct, line)) = xp_progress(user.level, user.xp) {
            row.xp_pct = pct;
            row.xp_line = line.into();
        }
        if user.is_us {
            return;
        }
        // Blocked: their voice never reaches us and their stream is
        // not ours to watch, so neither shows; the name is muted.
        if self.client_blocked(user.client_id) {
            row.speaking = false;
            row.sharing = false;
            row.name_color = slint::Color::from_rgb_u8(0x6B, 0x72, 0x80);
            row.has_name_color = true;
        }
        let ours = self.our_rank();
        let theirs = self.rank_of_client(user.client_id);
        row.outranked = theirs < ours;
        row.role_editable = theirs <= ours;
        let owns_their_channel = self
            .channels
            .iter()
            .any(|c| c.id == user.channel_id && c.owned);
        row.channel_kickable = p::has(self.permissions, p::MANAGE_CHANNELS)
            || (owns_their_channel && p::has(self.permissions, p::MANAGE_OWN_CHANNELS));
        row.telemetry = self.user_telemetry(user).into();
    }

    /// The last telemetry for sessions that may see it — or a note
    /// that none arrived yet; empty for everyone else.
    pub(super) fn user_telemetry(&self, user: &roster::RosterRow) -> String {
        if !client::permissions::has(self.permissions, client::permissions::MANAGE_USERS)
            || user.is_us
        {
            return String::new();
        }
        match self.telemetry_notes.get(&user.client_id) {
            Some(note) => note.text.clone(),
            None => t!("tooltip-no-telemetry"),
        }
    }

    /// Decode `avatar.png` (if the profile has one) for the rows.
    pub fn reload_own_avatar(&mut self) {
        self.own_avatar = self
            .settings
            .profile
            .avatar
            .then(|| std::fs::read(client::config::avatar_path()).ok())
            .flatten()
            .and_then(|bytes| app_core::images::decode_bounded(&bytes).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                ui::convert::rgba_image(rgba.as_raw(), rgba.width(), rgba.height())
            });
    }

    /// The profile look for a row: ours from the settings, everyone
    /// else's the defaults until the profile wire (1.1.2) fills them.
    /// `UserRow` and `ChatRow` carry the same fields (`ProfileLook`).
    /// `author` is the row's account when known: it finds the look of
    /// someone offline or in the lobby, and is not fooled by a name
    /// someone else wears now.
    pub(super) fn apply_profile_look(
        &self,
        name: &str,
        is_us: bool,
        author: Option<&client::UserId>,
        row: &mut dyn ProfileLook,
    ) {
        // The server's copy when it has one (that is what everyone
        // else sees); our settings before the server answered, or
        // when it does not carry profiles.
        let from_server = self.server_look(name, is_us, author);
        let ours = is_us || name == self.settings.user.nickname;
        let (color, style, avatar) = match from_server {
            Some(p) => (
                p.name_color,
                match p.name_style {
                    client::protocol::NameStyle::Normal => client::config::NameStyle::Normal,
                    client::protocol::NameStyle::Bold => client::config::NameStyle::Bold,
                    client::protocol::NameStyle::Italic => client::config::NameStyle::Italic,
                },
                p.avatar
                    .and_then(|blob| self.avatar_images.get(&blob).cloned())
                    .or_else(|| ours.then(|| self.own_avatar.clone()).flatten()),
            ),
            None if ours => (
                self.settings.profile.name_color,
                self.settings.profile.name_style,
                self.own_avatar.clone(),
            ),
            None => (None, client::config::NameStyle::Normal, None),
        };
        if let Some(rgb) = color {
            let [_, r, g, b] = rgb.to_be_bytes();
            row.set_name_color(slint::Color::from_rgb_u8(r, g, b));
        }
        row.set_name_style(match style {
            client::config::NameStyle::Normal => 0,
            client::config::NameStyle::Bold => 1,
            client::config::NameStyle::Italic => 2,
        });
        if let Some(image) = avatar {
            row.set_avatar(image);
        }
    }

    /// What the server says a row's author looks like: by account (their
    /// session's look while online, else what `Authors` said), else by
    /// name among the sessions online, else by name among the accounts
    /// introduced (a hover card knows only the name).
    fn server_look(
        &self,
        name: &str,
        is_us: bool,
        author: Option<&client::UserId>,
    ) -> Option<&client::protocol::Profile> {
        if let Some(user) = author {
            let online = self
                .details
                .iter()
                .find(|(_, (u, _))| u == user)
                .and_then(|(c, _)| self.profiles.get(c));
            if let Some(look) = online.or_else(|| self.author_looks.get(user).map(|l| &l.profile)) {
                return Some(look);
            }
        }
        self.users
            .iter()
            .find(|(id, u)| (is_us && Some(**id) == self.our_client_id) || u.username == name)
            .and_then(|(id, _)| self.profiles.get(id))
            .or_else(|| {
                (!is_us)
                    .then(|| self.author_looks.values().find(|l| l.name == name))
                    .flatten()
                    .map(|l| &l.profile)
            })
    }

    /// Our look as the server should have it (spec §7.1): sent right
    /// after `Session` and on every change. With a picture the blob
    /// goes first; `UploadDone` calls back here with its hash.
    pub fn send_own_profile(&mut self) {
        if !self.connected {
            return;
        }
        if self.settings.profile.avatar
            && self.own_avatar_blob.is_none()
            && let Ok(bytes) = std::fs::read(client::config::avatar_path())
        {
            self.network
                .tx
                .send(NetworkControl::UploadBlob {
                    upload_id: AVATAR_UPLOAD_ID,
                    name: "avatar.png".to_string(),
                    mime: "image/png".to_string(),
                    data: bytes,
                })
                .ok();
            return;
        }
        let profile = client::protocol::Profile {
            name_color: self.settings.profile.name_color,
            name_style: match self.settings.profile.name_style {
                client::config::NameStyle::Normal => client::protocol::NameStyle::Normal,
                client::config::NameStyle::Bold => client::protocol::NameStyle::Bold,
                client::config::NameStyle::Italic => client::protocol::NameStyle::Italic,
            },
            avatar: if self.settings.profile.avatar {
                self.own_avatar_blob
            } else {
                None
            },
        };
        self.network
            .tx
            .send(NetworkControl::SetProfile(profile))
            .ok();
    }

    /// Someone changed their nickname: every row they authored, in
    /// every conversation, is rebuilt under the new name.
    pub(super) fn rename_author(&mut self, old: &str, new: &str) {
        if old == new || old.is_empty() {
            return;
        }
        let keys: Vec<Option<client::UserId>> = std::iter::once(self.active_dm)
            .chain(self.parked.keys().copied())
            .collect();
        for key in keys {
            self.with_conversation(key, |app| {
                for i in app.history.rename_author(old, new) {
                    let row = app.chat_row_at(i);
                    app.chat_model.set_row_data(i, row);
                }
            });
        }
    }

    /// A profile changed: every chat row by that author, in every
    /// conversation, takes the new look (the roster rebuilds anyway).
    pub(super) fn refresh_author_look(&mut self, name: &str) {
        self.refresh_looks(&|user, _| user == name);
    }

    /// The same for an account: its rows by account, and by name the
    /// rows from before the session knew whose they were.
    pub(super) fn refresh_account_look(&mut self, account: &client::UserId, name: &str) {
        self.refresh_looks(&|user, author| author == Some(account) || user == name);
    }

    /// Re-apply the look of every chat row `matches` picks (its author's
    /// name, its account when known), in every conversation.
    fn refresh_looks(&mut self, matches: &dyn Fn(&str, Option<&client::UserId>) -> bool) {
        let keys: Vec<Option<client::UserId>> = std::iter::once(self.active_dm)
            .chain(self.parked.keys().copied())
            .collect();
        for key in keys {
            self.with_conversation(key, |app| {
                for i in 0..app.chat_model.row_count() {
                    let Some(mut row) = app.chat_model.row_data(i) else {
                        continue;
                    };
                    if row.kind == 1 || row.kind == 4 {
                        continue;
                    }
                    let author = app
                        .history
                        .rows()
                        .get(i)
                        .and_then(|r| r.meta())
                        .and_then(|m| m.author);
                    let name = row.user.to_string();
                    if !matches(&name, author.as_ref()) {
                        continue;
                    }
                    // Reset before re-applying: a removed picture or
                    // color must clear too.
                    row.has_avatar = false;
                    row.avatar = slint::Image::default();
                    row.has_name_color = false;
                    row.name_style = 0;
                    let own = row.own;
                    app.apply_profile_look(&name, own, author.as_ref(), &mut row);
                    app.chat_model.set_row_data(i, row);
                }
            });
        }
    }

    /// The picture changed locally: the server's hash for it no longer
    /// applies (the next `send_own_profile` uploads the new one).
    pub fn forget_avatar_blob(&mut self) {
        self.own_avatar_blob = None;
    }

    /// Our own look changed in Settings: our rows follow at once.
    pub fn refresh_own_rows(&mut self) {
        let name = self.settings.user.nickname.clone();
        self.refresh_author_look(&name);
    }

    /// Ask the server to rename this session (spec §7.1).
    pub fn request_rename(&mut self, name: String) {
        self.network.tx.send(NetworkControl::SetNickname(name)).ok();
    }

    /// A session's look arrived: keep it (by account too, so it stays
    /// once they leave), fetch a picture we do not have (disk cache
    /// first), redraw.
    pub(super) fn take_profile(&mut self, client_id: u64, profile: client::protocol::Profile) {
        self.want_avatar(profile.avatar);
        let name = self.username_of(client_id);
        let account = self.account_of(client_id);
        if let Some(user) = account {
            self.author_looks.insert(
                user,
                AuthorLook {
                    name: name.clone(),
                    profile: profile.clone(),
                },
            );
        }
        self.profiles.insert(client_id, profile);
        self.update_user_list();
        self.rebuild_dms_model();
        match account {
            Some(user) => self.refresh_account_look(&user, &name),
            None => self.refresh_author_look(&name),
        }
    }

    /// `Authors`: who wrote what is arriving, for authors without a
    /// roster row. A lobby session's earlier picture (pictures ride
    /// another stream and can beat this) takes its real name.
    pub(super) fn take_authors(&mut self, authors: Vec<client::protocol::AuthorInfo>) {
        for author in authors {
            if let Some(client_id) = author.client_id {
                self.lurkers.insert(client_id, author.user_id);
                let fallback = client::network::fallback_username(client_id);
                self.rename_author(&fallback, &author.name);
            }
            self.take_author_look(author.user_id, author.name, author.profile);
        }
    }

    /// An account's look, from `Authors` or a `ProfileUpdated` for
    /// someone not online: kept, its picture wanted, its rows redrawn
    /// when anything changed.
    pub(super) fn take_author_look(
        &mut self,
        user: client::UserId,
        name: String,
        profile: client::protocol::Profile,
    ) {
        self.want_avatar(profile.avatar);
        let look = AuthorLook { name, profile };
        if self.author_looks.get(&user) == Some(&look) {
            return;
        }
        let name = look.name.clone();
        self.author_looks.insert(user, look);
        self.rebuild_dms_model();
        self.refresh_account_look(&user, &name);
    }

    /// A picture someone's look names: decoded from the disk cache, or
    /// asked of the server once.
    fn want_avatar(&mut self, avatar: Option<client::protocol::BlobId>) {
        let Some(blob) = avatar else {
            return;
        };
        if self.avatar_images.contains_key(&blob) {
            return;
        }
        let cached = client::config::avatar_cache_dir().join(hex(&blob));
        match std::fs::read(&cached) {
            Ok(bytes) => self.take_avatar_bytes(blob, &bytes, false),
            Err(_) => {
                if self.avatar_fetches.insert(blob) {
                    self.network
                        .tx
                        .send(NetworkControl::FetchBlob { blob_id: blob })
                        .ok();
                }
            }
        }
    }

    /// Decode a picture into the image map (and onto the disk cache
    /// when it just arrived from the wire).
    pub(super) fn take_avatar_bytes(
        &mut self,
        blob: client::protocol::BlobId,
        bytes: &[u8],
        cache: bool,
    ) {
        let Ok(decoded) = app_core::images::decode_bounded(bytes) else {
            return;
        };
        // Ours are 128² at pick time; a picture from another client is
        // whatever the server's KB cap let through, and it is drawn at
        // 64 px at most. Never keep more than 128² of it resident.
        let side = app_core::images::AVATAR_SIDE;
        let rgba = if decoded.width().max(decoded.height()) > side {
            decoded.thumbnail(side, side).to_rgba8()
        } else {
            decoded.to_rgba8()
        };
        let image = ui::convert::rgba_image(rgba.as_raw(), rgba.width(), rgba.height());
        self.avatar_images.insert(blob, image);
        if cache {
            let dir = client::config::avatar_cache_dir();
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = std::fs::write(dir.join(hex(&blob)), bytes);
            }
        }
        self.update_user_list();
        self.rebuild_dms_model();
        // Whoever's picture this is, their rows can show it now: by
        // account (everyone the session has one for), else by name.
        let online: Vec<String> = self
            .profiles
            .iter()
            .filter(|(c, p)| p.avatar == Some(blob) && self.account_of(**c).is_none())
            .filter_map(|(c, _)| self.users.get(c).map(|u| u.username.clone()))
            .collect();
        let accounts: Vec<(client::UserId, String)> = self
            .author_looks
            .iter()
            .filter(|(_, l)| l.profile.avatar == Some(blob))
            .map(|(u, l)| (*u, l.name.clone()))
            .collect();
        for name in online {
            self.refresh_author_look(&name);
        }
        for (user, name) in accounts {
            self.refresh_account_look(&user, &name);
        }
    }

    /// The pointer entered or left an author's picture in chat: the
    /// card wants a roster row, so the name is looked up among the
    /// people online (someone who left gets no card).
    pub(super) fn author_hovered(&mut self, name: &str, hovering: bool) {
        let found = (0..self.users_model.row_count()).find(|&i| {
            self.users_model
                .row_data(i)
                .is_some_and(|row| !row.is_channel && row.name == name)
        });
        let s = self.main.global::<State>();
        match (hovering, found) {
            (true, Some(index)) => {
                let index = i32::try_from(index).unwrap_or(-1);
                s.set_card_guest_open(false);
                s.set_card_index(index);
                self.user_hovered(index);
            }
            // Gone since they wrote: a card from what we still know.
            (true, None) => {
                s.set_card_index(-1);
                let guest = self.guest_card_row(name);
                let s = self.main.global::<State>();
                s.set_card_guest(guest);
                s.set_card_guest_open(true);
            }
            (false, _) => {
                let shown = usize::try_from(s.get_card_index()).ok();
                if shown == found {
                    s.set_card_index(-1);
                }
                s.set_card_guest_open(false);
            }
        }
    }

    /// The card for a chat author who is no longer connected: name
    /// and look from the profile cache, "Offline" where the level
    /// would go, nothing the roster alone could tell.
    pub(super) fn guest_card_row(&self, name: &str) -> UserRow {
        let initial: String = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect())
            .unwrap_or_default();
        let mut row = UserRow {
            name: name.into(),
            initial: initial.into(),
            tile_tint: ui::convert::tint(name),
            level_label: t!("card-offline").into(),
            xp_pct: -1,
            ..UserRow::default()
        };
        self.apply_profile_look(name, false, None, &mut row);
        row
    }

    /// The pointer rests on a user row: fetch their last telemetry
    /// for the tooltip when we may, at most once every ten seconds.
    pub(super) fn user_hovered(&mut self, index: i32) {
        if !client::permissions::has(self.permissions, client::permissions::MANAGE_USERS) {
            return;
        }
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        if Some(client_id) == self.our_client_id {
            return;
        }
        let fresh = self
            .telemetry_notes
            .get(&client_id)
            .is_some_and(|n| n.fetched.elapsed() < Duration::from_secs(10));
        if fresh {
            return;
        }
        let Some((user_id, _)) = self.details.get(&client_id) else {
            return;
        };
        let target = AdminTarget::User(*user_id);
        self.send_admin(
            AdminCommand::LastTelemetry { target },
            PendingAdmin::Telemetry(client_id),
        );
    }
}
