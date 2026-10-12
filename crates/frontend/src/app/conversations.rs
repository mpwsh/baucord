//! History paging, the General/DM conversation switch, DM rows and sending, the row model in sync with the history.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    /// Mirror a history delta into the `VecModel` (drop from front, append
    /// the converted new rows).
    pub(super) fn sync_history_tail(&mut self, delta: &app_core::chat::HistoryDelta) {
        for _ in 0..delta.dropped_front {
            if self.chat_model.row_count() > 0 {
                self.evict_row(0);
            }
        }
        if delta.dropped_front > 0 {
            self.refresh_grouping(0);
            // "Load earlier" goes on from the oldest message still
            // shown, not from where the last page began: with the old
            // cursor the trimmed rows in between were never fetched
            // again, a gap in the middle of the history (review P2-23).
            if let Some(oldest) = self.history.oldest_message_id() {
                self.session.oldest_history_id = Some(oldest);
                if self.session.server_persistent && !self.session.chat.history_more {
                    self.session.chat.history_more = true;
                    if !self.borrowed_conversation {
                        self.main.global::<State>().set_history_more(true);
                    }
                }
            }
        }
        let len = self.history.rows().len();
        for i in (len - delta.appended)..len {
            let row = self.chat_row_at(i);
            self.chat_model.push(row);
        }
        if delta.appended == 0 || self.borrowed_conversation {
            return;
        }
        // A reader at the bottom follows the new rows once the list has
        // measured them (`ChatPane.follow-appended`); one scrolled up
        // is told how many messages from others they have not seen.
        let following = self.main.invoke_chat_follow_appended();
        // Nothing is trimmed above a reader who is not at the newest
        // rows; back there, the list returns to its cap.
        self.history.hold_front(!following);
        let fresh: HashSet<u64> = self.history.rows()[len - delta.appended..]
            .iter()
            .filter_map(app_core::chat::ChatItem::meta)
            .filter(|m| !m.own && !m.pending)
            .map(|m| m.id)
            .collect();
        if !following && !fresh.is_empty() && !self.activity_shown() {
            let s = self.main.global::<State>();
            let count = usize::try_from(s.get_chat_new_count()).unwrap_or(0) + fresh.len();
            s.set_chat_new_count(i32::try_from(count).unwrap_or(i32::MAX));
            s.set_chat_new_label(t!("chat-new-messages", "count" => count).into());
        }
    }

    /// A history page for conversation `key`; the active fields hold
    /// that conversation while this runs. Oldest first. A page we
    /// asked for goes in front of the list; the general replay right
    /// after the roster is appended (nothing live precedes it).
    pub(super) fn take_history_page(
        &mut self,
        key: Option<client::UserId>,
        messages: &[client::HistoryMessage],
        more: bool,
    ) {
        if let Some(first) = messages.first() {
            self.session.oldest_history_id = Some(first.id);
        }
        // A blocked person's messages stay out of every page; their ids
        // are kept so later edits and deletions of them say nothing.
        let hidden: Vec<u64> = messages
            .iter()
            .filter(|m| self.is_blocked(&m.user_id))
            .map(|m| m.id)
            .collect();
        self.session.safety.hidden_messages.extend(hidden);
        if key.is_none() {
            for m in messages {
                if Some(m.user_id) == self.session.our_user_id {
                    self.session.chat.own_texts.insert(m.id, m.text.clone());
                }
            }
        }
        let prepared: Vec<(Meta, String, bool)> = messages
            .iter()
            .filter(|m| !self.is_blocked(&m.user_id))
            .map(|m| {
                let meta = Meta {
                    id: m.id,
                    ts: format_wall_time(m.ts),
                    ts_ms: m.ts,
                    day: local_day(m.ts),
                    user: m.name.clone(),
                    author: Some(m.user_id),
                    own: Some(m.user_id) == self.session.our_user_id,
                    pending: false,
                };
                (meta, app_core::emoji::replace_shortcodes(&m.text), m.edited)
            })
            .collect();
        if key == self.session.chat.active_dm {
            let s = self.main.global::<State>();
            s.set_history_more(more);
            s.set_history_loading(false);
        }
        self.session.chat.history_more = more;
        // DM pages are always asked for; the general replay after
        // `Init` is the one page that is appended.
        let page = self.session.chat.history_page_pending == Some(key) || key.is_some();
        if page {
            if self.session.chat.history_page_pending == Some(key) {
                self.session.chat.history_page_pending = None;
            }
            if prepared.is_empty() {
                if key.is_none() {
                    self.push_system_here(&t!("chat-no-earlier"));
                }
                return;
            }
            // Dividers where the day changes within the page, and one
            // before its first message.
            let mut rows = Vec::new();
            let mut day = None;
            for (meta, body, edited) in &prepared {
                if day != Some(meta.day) {
                    rows.push(app_core::chat::ChatItem::DayMarker {
                        label: day_label(meta.day),
                        day: meta.day,
                    });
                    day = Some(meta.day);
                }
                rows.extend(ChatHistory::rows_for(meta, body, *edited));
            }
            let inserted = self.history.prepend_rows(rows);
            self.sync_prepend(inserted);
            // The divider that used to open the list is redundant when
            // the page ends on the same day.
            let redundant = matches!(
                self.history.rows().get(inserted),
                Some(app_core::chat::ChatItem::DayMarker { day: d, .. }) if Some(*d) == day
            );
            if redundant && let Some(delta) = self.history.remove_row(inserted) {
                self.sync_range(&delta);
            }
        } else {
            for (meta, body, edited) in &prepared {
                self.push_message_here(meta, body, *edited);
            }
            if !prepared.is_empty() {
                let line = t!(
                    "chat-history-loaded",
                    "count" => prepared.len(),
                    "more" => if more { "yes" } else { "no" }
                );
                self.push_system_here(&line);
            }
        }
    }

    // ── Conversations (spec §6) ─────────────────────────────────────

    /// Run `f` with conversation `key` in the active fields, swapping
    /// a parked one in and back out around it. The pane keeps showing
    /// the real active conversation: only `switch_conversation` moves
    /// `State.chat`.
    pub(super) fn with_conversation<R>(
        &mut self,
        key: Option<client::UserId>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if key == self.session.chat.active_dm {
            return f(self);
        }
        let active = self.session.chat.active_dm;
        let borrowed = std::mem::replace(&mut self.borrowed_conversation, true);
        self.swap_in(key);
        let out = f(self);
        self.swap_in(active);
        self.borrowed_conversation = borrowed;
        out
    }

    /// Exchange the active fields with the parked conversation `key`
    /// (created empty if new), parking the current one under its key.
    pub(super) fn swap_in(&mut self, key: Option<client::UserId>) {
        if key == self.session.chat.active_dm {
            return;
        }
        let mut target = self
            .session
            .chat
            .parked
            .remove(&key)
            .unwrap_or_else(|| Conversation {
                history: ChatHistory::new(),
                model: Rc::new(VecModel::default()),
                oldest_history_id: None,
                more: false,
            });
        std::mem::swap(&mut self.history, &mut target.history);
        std::mem::swap(&mut self.chat_model, &mut target.model);
        std::mem::swap(
            &mut self.session.oldest_history_id,
            &mut target.oldest_history_id,
        );
        std::mem::swap(&mut self.session.chat.history_more, &mut target.more);
        let old_key = self.session.chat.active_dm;
        self.session.chat.parked.insert(old_key, target);
        self.session.chat.active_dm = key;
    }

    /// Show a conversation in the chat pane: General (`None`) or the
    /// DM with `user`. A DM opens with its history fetched once and
    /// its unread cleared (`MarkRead`).
    pub(super) fn switch_conversation(&mut self, key: Option<client::UserId>) {
        // Any switch lands on a conversation, not General's activity.
        self.leave_activity_view();
        if key != self.session.chat.active_dm {
            self.freeze_chat_gifs();
            self.swap_in(key);
            let s = self.main.global::<State>();
            s.set_chat(ModelRc::from(self.chat_model.clone()));
            s.set_history_more(self.session.chat.history_more);
            s.set_history_loading(false);
            self.session.chat.history_page_pending = None;
        }
        let s = self.main.global::<State>();
        match key {
            None => {
                s.set_active_dm(-1);
                s.set_conv_name("".into());
                self.session.chat.general_unread = 0;
                s.set_general_unread(0);
            }
            Some(user) => {
                Self::clear_dm_notice(&user);
                let index = self.session.chat.dm_order.iter().position(|u| *u == user);
                s.set_active_dm(index.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1));
                let name = self
                    .session
                    .chat
                    .dm_meta
                    .get(&user)
                    .map(|m| m.name.clone())
                    .unwrap_or_default();
                s.set_conv_name(name.into());
                let (fetch, mark) =
                    self.session
                        .chat
                        .dm_meta
                        .get_mut(&user)
                        .map_or((false, None), |meta| {
                            let mark = (meta.unread > 0).then_some(meta.last_msg_id);
                            meta.unread = 0;
                            (!meta.fetched && self.session.server_persistent, mark)
                        });
                if fetch {
                    self.load_earlier(50);
                }
                if let Some(msg_id) = mark
                    && user != ADMIN_CHANNEL_KEY
                {
                    self.network
                        .tx
                        .send(NetworkControl::Send(ClientMessage::MarkRead {
                            conv: client::ConvRef::Dm(user),
                            msg_id,
                        }))
                        .ok();
                }
            }
        }
        self.rebuild_dms_model();
        self.main.invoke_chat_pin_bottom();
        self.main.invoke_focus_composer();
    }

    /// Hide a DM from the list. Nothing is deleted: the server keeps
    /// the conversation, and the next message brings it back.
    pub(super) fn close_dm(&mut self, user: client::UserId) {
        if self.session.chat.active_dm == Some(user) {
            self.switch_conversation(None);
        }
        self.session.chat.parked.remove(&Some(user));
        self.session.chat.dm_meta.remove(&user);
        self.session.chat.dm_order.retain(|u| *u != user);
        self.set_dm_hidden(user, true);
        self.rebuild_dms_model();
    }

    /// Remember (or forget) that a conversation was closed, so the
    /// next login's `Conversations` does not bring it back.
    pub(super) fn set_dm_hidden(&mut self, user: client::UserId, hidden: bool) {
        if user == ADMIN_CHANNEL_KEY {
            return;
        }
        let key = hex(&user);
        let list = &mut self.settings.ui.hidden_dms;
        let present = list.contains(&key);
        if hidden && !present {
            list.push(key);
        } else if !hidden && present {
            list.retain(|h| *h != key);
        } else {
            return;
        }
        self.settings.save();
    }

    pub(super) fn dm_hidden(&self, user: &client::UserId) -> bool {
        let key = hex(user);
        self.settings.ui.hidden_dms.contains(&key)
    }

    /// Open (or start) the DM with `user`, named `name`.
    pub fn open_dm(&mut self, user: client::UserId, name: &str) {
        self.set_dm_hidden(user, false);
        let meta = self
            .session
            .chat
            .dm_meta
            .entry(user)
            .or_insert_with(|| DmMeta {
                name: name.to_string(),
                unread: 0,
                last_ts: 0,
                last_msg_id: 0,
                // Nothing to fetch without a store.
                fetched: false,
            });
        if meta.name.is_empty() {
            meta.name = name.to_string();
        }
        if !self.session.chat.dm_order.contains(&user) {
            self.session.chat.dm_order.insert(0, user);
        }
        self.switch_conversation(Some(user));
        // From a roster menu or `/dm`: the narrow layout turns to the
        // chat page, as a tap on the conversation's row would.
        self.main.global::<State>().set_narrow_chat_open(true);
    }

    /// The admin channel's row exists (spec §8): a conversation under
    /// the reserved key, first in the Messages list, never fetched
    /// from the DM store.
    pub(super) fn ensure_admin_channel(&mut self) {
        self.session
            .chat
            .dm_meta
            .entry(ADMIN_CHANNEL_KEY)
            .or_insert_with(|| DmMeta {
                name: t!("admin-channel"),
                unread: 0,
                last_ts: 0,
                last_msg_id: 0,
                fetched: true,
            });
        if !self.session.chat.dm_order.contains(&ADMIN_CHANNEL_KEY) {
            self.session.chat.dm_order.insert(0, ADMIN_CHANNEL_KEY);
        }
    }

    /// The shield row: open the admin channel; the last page of the
    /// audit becomes its history the first time.
    pub(super) fn open_admin_channel(&mut self) {
        self.ensure_admin_channel();
        let empty =
            self.with_conversation(Some(ADMIN_CHANNEL_KEY), |app| app.history.rows().is_empty());
        self.switch_conversation(Some(ADMIN_CHANNEL_KEY));
        if empty {
            let lead = t!("admin-channel-lead");
            self.with_conversation(Some(ADMIN_CHANNEL_KEY), |app| app.push_system_here(&lead));
            self.send_admin(
                AdminCommand::AuditLog {
                    before: None,
                    limit: 50,
                },
                PendingAdmin::Channel,
            );
        }
    }

    /// The Messages section's rows: newest conversation first, with
    /// unread counts, online rings and the partner's look.
    pub(super) fn rebuild_dms_model(&mut self) {
        let mut total = 0;
        let rows: Vec<DmRow> = self
            .session
            .chat
            .dm_order
            .iter()
            .filter_map(|user| {
                let meta = self.session.chat.dm_meta.get(user)?;
                total += meta.unread;
                let client = self
                    .session
                    .details
                    .iter()
                    .find(|(_, (u, _))| u == user)
                    .map(|(c, _)| *c);
                let name = client
                    .and_then(|c| self.session.users.get(&c))
                    .map_or(meta.name.clone(), |u| u.username.clone());
                let mut row = DmRow {
                    name: name.as_str().into(),
                    unread: i32::try_from(meta.unread).unwrap_or(i32::MAX),
                    active: self.session.chat.active_dm == Some(*user),
                    online: client.is_some(),
                    avatar: slint::Image::default(),
                    has_avatar: false,
                    initial: app_core::roster::initial_of(&name).into(),
                    tile_tint: ui::convert::tint(&name),
                    admin: *user == ADMIN_CHANNEL_KEY,
                };
                // Online: their session's look; offline: what the server
                // said of them with the list (`Authors`).
                if let Some(image) = client
                    .and_then(|c| self.session.profiles.get(&c))
                    .or_else(|| self.session.author_looks.get(user).map(|l| &l.profile))
                    .and_then(|p| p.avatar)
                    .and_then(|blob| self.avatar_images.get(&blob))
                {
                    row.avatar = image.clone();
                    row.has_avatar = true;
                }
                Some(row)
            })
            .collect();
        self.dms_model.set_vec(rows);
        let s = self.main.global::<State>();
        s.set_dm_unread_total(i32::try_from(total).unwrap_or(i32::MAX));
        if let Some(user) = self.session.chat.active_dm {
            let index = self.session.chat.dm_order.iter().position(|u| *u == user);
            s.set_active_dm(index.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1));
        }
    }

    /// A live DM arrived (spec §6): into its conversation — shown at
    /// once if open (and marked read), counted as unread otherwise —
    /// the list moves it to the top, the chime plays.
    pub(super) fn take_direct(
        &mut self,
        from: client::UserId,
        username: &str,
        id: u64,
        ts: u64,
        body: Incoming<'_>,
    ) {
        // The server refuses DMs between blocked people both ways; a
        // stray one (sent just before the block) stays out.
        if self.is_blocked(&from) {
            self.hide_message(id);
            return;
        }
        // A new message brings a closed conversation back.
        self.set_dm_hidden(from, false);
        let is_open = self.session.chat.active_dm == Some(from);
        let meta = self
            .session
            .chat
            .dm_meta
            .entry(from)
            .or_insert_with(|| DmMeta {
                name: username.to_string(),
                unread: 0,
                last_ts: 0,
                last_msg_id: 0,
                // Never opened: with a store the page will bring this one
                // too, so it is not pushed twice.
                fetched: false,
            });
        meta.name = username.to_string();
        meta.last_ts = ts;
        meta.last_msg_id = meta.last_msg_id.max(id);
        let show_now = is_open || meta.fetched || !self.session.server_persistent;
        if !self.session.server_persistent {
            meta.fetched = true;
        }
        if !is_open {
            meta.unread += 1;
        }
        self.session.chat.dm_order.retain(|u| *u != from);
        self.session.chat.dm_order.insert(0, from);
        if show_now {
            let meta = Meta {
                id,
                ts: format_wall_time(ts),
                ts_ms: ts,
                day: local_day(ts),
                user: username.to_string(),
                author: Some(from),
                own: false,
                pending: false,
            };
            self.with_conversation(Some(from), |app| {
                match &body {
                    Incoming::Text(text) => {
                        let text = app_core::emoji::replace_shortcodes(text);
                        app.push_message_here(&meta, &text, false);
                    }
                    Incoming::File(file) => app.push_file_here(&meta, file.clone()),
                }
                app.session.oldest_history_id.get_or_insert(id);
            });
        }
        if is_open {
            self.network
                .tx
                .send(NetworkControl::Send(ClientMessage::MarkRead {
                    conv: client::ConvRef::Dm(from),
                    msg_id: id,
                }))
                .ok();
        }
        // The chime on screen; a notification off screen on a phone.
        let text = match &body {
            Incoming::Text(text) => app_core::emoji::replace_shortcodes(text),
            Incoming::File(file) => t!("notif-dm-file", "name" => file.name.as_str()),
        };
        self.alert_direct_message(from, username, &text);
        self.rebuild_dms_model();
    }

    /// Send a DM from the composer: the row appears at once, muted,
    /// under a placeholder id; `MessageAck` confirms it, `MessageFailed`
    /// removes it.
    pub(super) fn send_dm(&mut self, to: client::UserId, text: String) {
        if to == ADMIN_CHANNEL_KEY {
            // Commands only here; they are handled before this point.
            let hint = t!("admin-channel-commands-only");
            self.with_conversation(Some(to), |app| app.push_system_here(&hint));
            return;
        }
        let client_ref = self.next_client_ref;
        self.next_client_ref = self.next_client_ref.wrapping_add(1).max(1);
        let placeholder = u64::MAX - u64::from(client_ref);
        let now = u64::try_from(chrono::Local::now().timestamp_millis()).unwrap_or(0);
        let meta = Meta {
            id: placeholder,
            ts: format_wall_time(now),
            ts_ms: now,
            day: local_day(now),
            user: self.settings.user.nickname.clone(),
            author: self.session.our_user_id,
            own: true,
            pending: true,
        };
        let body = app_core::emoji::replace_shortcodes(&text);
        self.push_message_here(&meta, &body, false);
        self.session
            .chat
            .pending_dm_sends
            .insert(client_ref, (Some(to), placeholder));
        if let Some(m) = self.session.chat.dm_meta.get_mut(&to) {
            m.last_ts = now;
        }
        self.session.chat.dm_order.retain(|u| *u != to);
        self.session.chat.dm_order.insert(0, to);
        self.rebuild_dms_model();
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::DirectMessage {
                to,
                text,
                client_ref,
            }))
            .ok();
    }

    /// The user behind roster row `index`, for "Message …".
    pub(super) fn dm_from_roster(&mut self, index: i32) {
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        let Some(user_id) = self.session.details.get(&client_id).map(|(u, _)| *u) else {
            return;
        };
        let name = self.username_of(client_id);
        self.open_dm(user_id, &name);
    }

    /// Mirror a splice (an edit or a deletion) into the model: the
    /// replaced rows go, the new ones take their place, and the rest
    /// of the list — and the scroll position — stay where they were.
    pub(super) fn sync_range(&mut self, delta: &app_core::chat::RangeDelta) {
        for _ in 0..delta.removed {
            if delta.start < self.chat_model.row_count() {
                self.evict_row(delta.start);
            }
        }
        for offset in 0..delta.inserted {
            let index = delta.start + offset;
            let row = self.chat_row_at(index);
            self.chat_model.insert(index, row);
        }
        self.refresh_grouping(delta.start + delta.inserted);
    }

    /// Re-derive one row's continuation flag after its neighbourhood
    /// changed (a splice, a prepend, a front drop). Only the flag is
    /// touched: GIF/image state keyed by the row id stays intact.
    pub(super) fn refresh_grouping(&self, index: usize) {
        if let Some(mut row) = self.chat_model.row_data(index) {
            let flag = self.history.is_continuation(index);
            if row.continuation != flag {
                row.continuation = flag;
                self.chat_model.set_row_data(index, row);
            }
        }
    }

    /// Remove model row `index` and free any GIF/image state it owned.
    pub(super) fn evict_row(&mut self, index: usize) {
        let old = self.chat_model.row_data(index);
        self.chat_model.remove(index);
        if let Some(old) = old {
            self.gifs.remove(&old.row_id);
            self.images.remove(&old.row_id);
        }
    }

    /// The model row for history row `index`, with a fresh row id.
    pub(crate) fn chat_row_at(&mut self, index: usize) -> ChatRow {
        // Truncating u64→i32 is deliberate: ids only need uniqueness
        // within the 500-row window, and 2^31 messages per session is
        // out of scope by ~six orders of magnitude.
        self.next_row_id += 1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let row_id = self.next_row_id as i32;
        let mut row = ui::convert::chat_row(&self.history.rows()[index], row_id);
        row.continuation = self.history.is_continuation(index);
        // An image row rebuilt in place keeps its row id and picture:
        // the texture is installed once by `push_image_row`, and the
        // `images`/`gifs` maps are keyed by that id. A fresh id here
        // orphaned the GIF player and blanked the picture.
        if row.kind == 2
            && let Some(old) = self.chat_model.row_data(index)
            && old.kind == 2
            && old.msg_id_lo == row.msg_id_lo
            && old.msg_id_hi == row.msg_id_hi
        {
            row.row_id = old.row_id;
            row.image = old.image;
        }
        if row.kind == 5 || row.kind == 2 {
            if let Some(id) = self.history.rows()[index].message_id()
                && let Some((kind, pct)) = self
                    .session
                    .transfers
                    .shown
                    .get(&(self.session.chat.active_dm, id))
            {
                row.transfer_kind = *kind;
                row.transfer_pct = *pct;
            }
            if let Some(file) = self.history.file_at(index) {
                row.file_saved = self.downloaded.contains_key(&file.blob);
            }
        }
        if row.kind != 1 && row.kind != 4 {
            let own = row.own;
            let name = row.user.to_string();
            let author = self.history.rows()[index].meta().and_then(|m| m.author);
            self.apply_profile_look(&name, own, author.as_ref(), &mut row);
        }
        // Message rows carrying WhatsApp-style markup (*bold*,
        // _italic_, `code`) get a pre-parsed StyledText; plain
        // ones skip it and stay mouse-selectable. to_commonmark
        // escapes everything else, so from_markdown can only fail
        // on pathological input — which then renders plain.
        if row.kind == 0 && !row.deleted {
            // Tint matches Theme.mono-text in theme.slint.
            let (cm, styled) =
                app_core::markdown::to_commonmark(row.body.as_str(), Some("#86AA90"));
            if styled && let Ok(st) = slint::StyledText::from_markdown(&cm) {
                row.body_styled = st;
                row.styled = true;
            }
        }
        row
    }

    /// Public mirror-sync for media.rs (image rows).
    pub fn sync_delta(&mut self, delta: &app_core::chat::HistoryDelta) {
        self.sync_history_tail(delta);
    }
}
