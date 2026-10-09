//! General's activity view (1.2): who came, went, moved and shared,
//! apart from the conversation, so it does not bury the messages.
//!
//! In memory and for this session only: emptied on every connect and
//! disconnect, capped at [`ACTIVITY_MAX`] lines, never written to disk
//! or the log. It shows what the roster already shows live; it is not
//! a record of who was online.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Lines the activity view keeps: a session's comings and goings, not
/// a history of them.
const ACTIVITY_MAX: usize = 200;

impl AppState {
    /// A line into the activity view, stamped with the local time.
    pub(super) fn push_activity(&mut self, text: &str) {
        let now = u64::try_from(chrono::Local::now().timestamp_millis()).unwrap_or(0);
        let line = format!("{}  {text}", format_wall_time(now));
        self.next_row_id += 1;
        // Row ids only need to be unique in a window; see `chat_row_at`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let row_id = self.next_row_id as i32;
        let row = ui::convert::chat_row(&app_core::chat::ChatItem::System { body: line }, row_id);
        self.activity_model.push(row);
        while self.activity_model.row_count() > ACTIVITY_MAX {
            self.activity_model.remove(0);
        }
        // Nothing marks it as new when it is not shown: the toggle in
        // General's header is there to look, not to nag.
        if self.activity_shown() {
            self.main.invoke_chat_follow_appended();
        }
    }

    /// The activity view is on screen: General's, toggled to it.
    pub(super) fn activity_shown(&self) -> bool {
        self.showing_activity && self.active_dm.is_none()
    }

    /// General's header toggle: the conversation, or the activity.
    pub(super) fn set_general_view(&mut self, activity: bool) {
        if self.active_dm.is_some() {
            self.switch_conversation(None);
        }
        if activity == self.showing_activity {
            return;
        }
        self.showing_activity = activity;
        if activity {
            self.freeze_chat_gifs();
        }
        let s = self.main.global::<State>();
        s.set_activity_open(activity);
        s.set_chat(if activity {
            ModelRc::from(self.activity_model.clone())
        } else {
            ModelRc::from(self.chat_model.clone())
        });
        self.main.invoke_chat_pin_bottom();
    }

    /// Back to the conversation view (a conversation switch, a send):
    /// the pane shows `chat_model` again.
    pub(super) fn leave_activity_view(&mut self) {
        if !self.showing_activity {
            return;
        }
        self.showing_activity = false;
        let s = self.main.global::<State>();
        s.set_activity_open(false);
        s.set_chat(ModelRc::from(self.chat_model.clone()));
    }

    /// Forget the activity (a new session, or none).
    pub(super) fn clear_activity(&mut self) {
        self.leave_activity_view();
        self.activity_model.set_vec(Vec::new());
    }
}
