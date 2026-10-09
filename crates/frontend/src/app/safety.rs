//! Blocking, reporting and deleting one's own account (1.2, the server's
//! `features::SAFETY`): the list of blocked people and what it hides,
//! the report dialog, and the account deletion's ending.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

use client::protocol::{ReportReason, ReportRefusal, ReportTarget};

/// What the report dialog is about while it is open.
pub(super) struct PendingReport {
    target: ReportTarget,
    /// Whose it is: "Also block them" blocks this account.
    author: Option<client::UserId>,
}

/// The localized name of a report reason, for the dialog's picker.
fn reason_label(reason: ReportReason) -> String {
    t!(match reason {
        ReportReason::Spam => "report-reason-spam",
        ReportReason::Harassment => "report-reason-harassment",
        ReportReason::Hate => "report-reason-hate",
        ReportReason::Sexual => "report-reason-sexual",
        ReportReason::Violence => "report-reason-violence",
        ReportReason::SelfHarm => "report-reason-self-harm",
        ReportReason::Illegal => "report-reason-illegal",
        ReportReason::Other => "report-reason-other",
    })
}

/// The reason a report's row in the admin panel shows.
pub(crate) fn reason_text(reason: ReportReason) -> String {
    reason_label(reason)
}

impl AppState {
    // ── Who is blocked ──────────────────────────────────────────────

    pub(super) fn is_blocked(&self, user: &client::UserId) -> bool {
        self.blocked.contains_key(user)
    }

    /// The account behind a session, when the server said
    /// (`UserDetails`, or `Authors` for someone in the lobby).
    pub(super) fn account_of(&self, client_id: u64) -> Option<client::UserId> {
        self.details
            .get(&client_id)
            .map(|(user, _)| *user)
            .or_else(|| self.lurkers.get(&client_id).copied())
    }

    /// Whether a session belongs to someone we blocked.
    pub(crate) fn client_blocked(&self, client_id: u64) -> bool {
        self.account_of(client_id)
            .is_some_and(|user| self.is_blocked(&user))
    }

    /// A message from a blocked person stays out; its id is kept so an
    /// edit or a deletion of it later says nothing either.
    pub(super) fn hide_message(&mut self, msg_id: u64) {
        self.hidden_messages.insert(msg_id);
    }

    /// `Blocks`: the whole list, on join and after every change (from
    /// this device or another of ours).
    pub(super) fn take_blocks(&mut self, users: Vec<client::protocol::BlockedUser>) {
        let fresh: HashMap<client::UserId, String> =
            users.into_iter().map(|b| (b.user_id, b.name)).collect();
        let added: Vec<client::UserId> = fresh
            .keys()
            .filter(|u| !self.blocked.contains_key(*u))
            .copied()
            .collect();
        let removed: Vec<(client::UserId, String)> = self
            .blocked
            .iter()
            .filter(|(u, _)| !fresh.contains_key(*u))
            .map(|(u, name)| (*u, name.clone()))
            .collect();
        let announce = self.blocks_known;
        self.blocks_known = true;
        self.blocked = fresh;
        for user in &added {
            self.hide_everything_of(user);
        }
        for (user, _) in &removed {
            self.show_conversation_again(user);
        }
        if announce {
            for user in &added {
                let name = self.blocked.get(user).cloned().unwrap_or_default();
                self.show_toast(
                    ToastKind::Info,
                    &t!("toast-blocked", "name" => name.as_str()),
                );
            }
            for (_, name) in &removed {
                self.show_toast(
                    ToastKind::Info,
                    &t!("toast-unblocked", "name" => name.as_str()),
                );
            }
        }
        self.update_user_list();
        self.rebuild_dms_model();
        crate::settingsui::refresh_safety(self);
    }

    /// Someone was just blocked: their rows leave every conversation,
    /// their DM leaves the list (it comes back on unblock), and their
    /// stream stops if we were watching it.
    fn hide_everything_of(&mut self, user: &client::UserId) {
        // General, every parked DM, and whatever is shown.
        let mut keys: Vec<Option<client::UserId>> = self.parked.keys().copied().collect();
        if !keys.contains(&self.active_dm) {
            keys.push(self.active_dm);
        }
        if !keys.contains(&None) {
            keys.push(None);
        }
        for key in keys {
            self.with_conversation(key, |app| app.drop_rows_of(user));
        }
        if self.active_dm == Some(*user) {
            self.switch_conversation(None);
        }
        // Their DM: out of the list, its rows dropped; it is fetched
        // again if they are unblocked and it is opened.
        self.parked.remove(&Some(*user));
        self.dm_order.retain(|u| u != user);
        if let Some(meta) = self.dm_meta.get_mut(user) {
            meta.fetched = false;
            meta.unread = 0;
        }
        let watched: Vec<u64> = self
            .viewers
            .keys()
            .copied()
            .filter(|c| self.account_of(*c).as_ref() == Some(user))
            .collect();
        for client_id in watched {
            self.handle_unwatch_peer(client_id);
        }
    }

    /// Drop the shown conversation's rows by `user`, mirroring the
    /// removals back to front so every other row keeps its id and
    /// picture.
    fn drop_rows_of(&mut self, user: &client::UserId) {
        let ids: Vec<u64> = self
            .history
            .rows()
            .iter()
            .filter_map(app_core::chat::ChatItem::meta)
            .filter(|m| m.author.as_ref() == Some(user))
            .map(|m| m.id)
            .collect();
        let removed = self.history.remove_author(user);
        if removed.is_empty() {
            return;
        }
        self.hidden_messages.extend(ids);
        for &index in removed.iter().rev() {
            if index < self.chat_model.row_count() {
                self.evict_row(index);
            }
        }
        for index in 0..self.chat_model.row_count() {
            self.refresh_grouping(index);
        }
    }

    /// Unblocked: their DM goes back in the list where its last
    /// message puts it. Rows hidden earlier this session stay hidden
    /// (the toast says so); new ones show.
    fn show_conversation_again(&mut self, user: &client::UserId) {
        let Some(last_ts) = self.dm_meta.get(user).map(|m| m.last_ts) else {
            return;
        };
        if self.dm_order.contains(user) || self.dm_hidden(user) {
            return;
        }
        let at = self
            .dm_order
            .iter()
            .position(|u| {
                *u != ADMIN_CHANNEL_KEY && self.dm_meta.get(u).is_none_or(|m| m.last_ts < last_ts)
            })
            .unwrap_or(self.dm_order.len());
        self.dm_order.insert(at, *user);
    }

    /// Block or unblock an account; the server answers with the whole
    /// list (`Blocks`), which does the rest.
    pub(super) fn set_blocked(&mut self, user: client::UserId, block: bool) {
        if !self.safety || Some(user) == self.our_user_id || user == ADMIN_CHANNEL_KEY {
            return;
        }
        self.network
            .tx
            .send(if block {
                NetworkControl::Block(user)
            } else {
                NetworkControl::Unblock(user)
            })
            .ok();
    }

    /// Settings' Unblock.
    pub(crate) fn unblock(&mut self, user: client::UserId) {
        self.set_blocked(user, false);
    }

    /// The roster row's account, for its menu's Block and Report.
    pub(super) fn block_at(&mut self, index: i32, block: bool) {
        if let Some(user) = self.user_id_at(index).and_then(|c| self.account_of(c)) {
            self.set_blocked(user, block);
        }
    }

    // ── Reports ─────────────────────────────────────────────────────

    /// "Report…" on a roster row.
    pub(super) fn report_user_at(&mut self, index: i32) {
        let Some(client_id) = self.user_id_at(index) else {
            return;
        };
        let Some(user) = self.account_of(client_id) else {
            return;
        };
        let name = self.username_of(client_id);
        self.open_report(
            ReportTarget::User { user },
            Some(user),
            &t!("report-title-user", "name" => name.as_str()),
            "",
        );
    }

    /// The author (account and name) of message `msg_id` in the shown
    /// conversation.
    pub(super) fn author_of_message(&self, msg_id: u64) -> Option<(client::UserId, String)> {
        self.history
            .rows()
            .iter()
            .filter_map(app_core::chat::ChatItem::meta)
            .find(|m| m.id == msg_id)
            .and_then(|m| m.author.map(|a| (a, m.user.clone())))
    }

    /// "Report message…" on a chat row of the shown conversation.
    pub(super) fn report_message(&mut self, msg_id: u64, body: &str) {
        let Some((author, name)) = self.author_of_message(msg_id) else {
            return;
        };
        let conv = match self.active_dm {
            None => client::ConvRef::General,
            Some(partner) => client::ConvRef::Dm(partner),
        };
        let excerpt: String = body.chars().take(280).collect();
        self.open_report(
            ReportTarget::Message { conv, msg_id },
            Some(author),
            &t!("report-title-message", "name" => name.as_str()),
            &excerpt,
        );
    }

    /// "Block sender" on a chat row: the author, whether or not they
    /// are still here.
    pub(super) fn block_author(&mut self, msg_id: u64) {
        if let Some((author, _)) = self.author_of_message(msg_id) {
            self.set_blocked(author, true);
        }
    }

    fn open_report(
        &mut self,
        target: ReportTarget,
        author: Option<client::UserId>,
        title: &str,
        excerpt: &str,
    ) {
        if !self.safety {
            return;
        }
        let reasons: Vec<slint::SharedString> = ReportReason::ALL
            .iter()
            .map(|r| reason_label(*r).into())
            .collect();
        let s = self.main.global::<State>();
        s.set_report_title(title.into());
        s.set_report_excerpt(excerpt.into());
        s.set_report_reasons(ModelRc::new(VecModel::from(reasons)));
        s.set_report_reason(-1);
        s.set_report_note("".into());
        s.set_report_block(false);
        s.set_report_block_offered(author.is_some_and(|a| !self.is_blocked(&a)));
        s.set_report_open(true);
        self.report = Some(PendingReport { target, author });
    }

    /// Send: the report, then the block if asked for.
    pub(super) fn report_send(&mut self) {
        let s = self.main.global::<State>();
        let reason = usize::try_from(s.get_report_reason())
            .ok()
            .and_then(|i| ReportReason::ALL.get(i).copied());
        let Some(reason) = reason else {
            // Send waits for a reason; nothing to do yet.
            return;
        };
        let Some(report) = self.report.take() else {
            s.set_report_open(false);
            return;
        };
        let note = client::protocol::truncate_on_char_boundary(
            s.get_report_note().trim(),
            client::protocol::MAX_REPORT_NOTE_BYTES,
        );
        let also_block = s.get_report_block();
        s.set_report_open(false);
        s.set_report_note("".into());
        self.network
            .tx
            .send(NetworkControl::Report {
                target: report.target,
                reason,
                note,
            })
            .ok();
        if also_block && let Some(author) = report.author {
            self.set_blocked(author, true);
        }
        self.main.invoke_focus_composer();
    }

    pub(super) fn report_cancel(&mut self) {
        self.report = None;
        let s = self.main.global::<State>();
        s.set_report_open(false);
        s.set_report_note("".into());
        self.main.invoke_focus_composer();
    }

    /// `ReportAck`: filed, or why not.
    pub(super) fn take_report_ack(&mut self, result: Result<(), ReportRefusal>) {
        match result {
            Ok(()) => self.show_toast(ToastKind::Success, &t!("report-sent")),
            Err(refusal) => {
                let key = match refusal {
                    ReportRefusal::NoSuchMessage => "report-refused-no-message",
                    ReportRefusal::NoSuchUser => "report-refused-no-user",
                    ReportRefusal::Yourself => "report-refused-yourself",
                    ReportRefusal::TooMany => "report-refused-too-many",
                };
                self.show_toast(ToastKind::Warning, &t!(key));
            }
        }
    }

    // ── Deleting the account ────────────────────────────────────────

    /// The Settings button, armed: ask the server.
    pub(crate) fn delete_account(&mut self) {
        if !self.connected || !self.safety {
            return;
        }
        info!("Asking the server to delete our account");
        self.network.tx.send(NetworkControl::DeleteAccount).ok();
    }

    /// `AccountDeleted`: the account is gone on the server; the session
    /// ends right after. What this device kept for it there goes too:
    /// the channel passwords on the server entry. The entry itself
    /// stays (the address is still good; joining again needs a new
    /// invite, and the connect card says so).
    pub(super) fn take_account_deleted(&mut self) {
        self.account_deleted = true;
        let name = self.session_server.clone();
        if let Some(entry) = name
            .as_deref()
            .and_then(|n| self.settings.servers.iter_mut().find(|e| e.name == n))
        {
            entry.channel_passwords.clear();
            entry.invite_code = None;
        }
        self.settings.save();
        crate::settingsui::refresh_servers(self);
        crate::settingsui::refresh_safety(self);
    }

    /// `AccountDeleteRefused`.
    pub(super) fn take_account_delete_refused(&mut self, reason: client::protocol::DeleteRefusal) {
        let line = match reason {
            client::protocol::DeleteRefusal::LastAdmin => t!("account-delete-last-admin"),
        };
        crate::settingsui::set_account_status(self, &line);
        self.show_toast(ToastKind::Warning, &line);
    }

    /// What the connect card says once the session that deleted the
    /// account has ended.
    pub(super) fn account_deleted_notice(&self) -> String {
        let server = self.session_server.clone().unwrap_or_default();
        t!("account-deleted", "server" => server.as_str())
    }

    /// Forget this session's safety state (teardown).
    pub(super) fn reset_safety(&mut self) {
        self.safety = false;
        self.blocked.clear();
        self.blocks_known = false;
        self.hidden_messages.clear();
        self.report = None;
        self.admin_reports.clear();
        let s = self.main.global::<State>();
        s.set_report_open(false);
        crate::settingsui::refresh_safety(self);
    }
}
