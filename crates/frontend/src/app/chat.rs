//! Pushing rows (system, message, file, command output), pictures and files in and out, edit/delete, GIFs and the preview.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

// A child module of `app`: its parent's private items and imports are
// its own, which is the whole point of the file split.
#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    /// A system line into the general chat, whichever conversation
    /// is open (notices are about the room).
    /// A system line into General, stamped with the local time: who
    /// came, went or moved, and when.
    pub fn push_chat_system(&mut self, text: &str) {
        let now = u64::try_from(chrono::Local::now().timestamp_millis()).unwrap_or(0);
        let line = format!("{}  {text}", format_wall_time(now));
        self.with_conversation(None, |app| app.push_system_here(&line));
    }

    /// A message into the general chat, whichever conversation is
    /// open: a DM being read must not swallow the room's traffic.
    pub fn push_chat_message(&mut self, meta: &Meta, body: &str, edited: bool) {
        self.with_conversation(None, |app| app.push_message_here(meta, body, edited));
        if self.active_dm.is_some() && !meta.own && !meta.pending {
            self.general_unread += 1;
            self.main
                .global::<State>()
                .set_general_unread(i32::try_from(self.general_unread).unwrap_or(i32::MAX));
        }
    }

    /// Into whatever conversation the active fields hold right now —
    /// the DM paths, and pages already routed by `with_conversation`.
    pub(super) fn push_system_here(&mut self, text: &str) {
        let delta = self.history.push_system(text);
        self.sync_history_tail(&delta);
    }

    pub(super) fn push_message_here(&mut self, meta: &Meta, body: &str, edited: bool) {
        self.mark_day(meta.day);
        let delta = self.history.push_message(meta, body, edited);
        self.sync_history_tail(&delta);
    }

    /// A file card into the shown conversation (spec §7).
    pub(super) fn push_file_here(&mut self, meta: &Meta, file: app_core::chat::FileMeta) {
        self.mark_day(meta.day);
        let delta = self.history.push_file(meta, file);
        self.sync_history_tail(&delta);
    }

    /// A command's output, into the conversation it was typed in.
    pub(super) fn push_command_output(&mut self, text: &str) {
        let conv = self.command_conv;
        self.with_conversation(conv, |app| app.push_system_here(text));
    }

    /// A file card into General, whichever conversation is shown.
    pub(super) fn push_chat_file(&mut self, meta: &Meta, file: app_core::chat::FileMeta) {
        self.with_conversation(None, |app| app.push_file_here(meta, file));
        if self.active_dm.is_some() && !meta.own {
            self.general_unread += 1;
            self.main
                .global::<State>()
                .set_general_unread(i32::try_from(self.general_unread).unwrap_or(i32::MAX));
        }
    }

    /// A card that is a picture fetches its bytes on arrival (spec §7:
    /// pictures in DMs ride the blob path) — when it is small enough
    /// to show inline; bigger ones stay a card with Save.
    pub(super) fn want_picture(
        &mut self,
        a: &client::protocol::Attachment,
        conv: Option<client::UserId>,
        msg_id: u64,
    ) {
        let inline = a.mime.starts_with("image/")
            && usize::try_from(a.size).is_ok_and(|s| s <= client::MAX_CHAT_IMAGE_BYTES);
        if !inline {
            return;
        }
        let fresh = !self.pending_pictures.contains_key(&a.blob_id);
        self.pending_pictures
            .entry(a.blob_id)
            .or_default()
            .push((conv, msg_id));
        if fresh {
            self.network
                .tx
                .send(NetworkControl::FetchBlob { blob_id: a.blob_id })
                .ok();
        }
    }

    /// Send an image into General with a card that shows it going:
    /// the picture itself, muted, with the ring and a ✕ (the same
    /// look as a file on its way). The server relays `ImageMessage`
    /// back; our own echo replaces the card.
    pub(super) fn send_image_with_card(&mut self, png: Vec<u8>) {
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
            own: true,
            pending: true,
        };
        self.transfers.insert((None, placeholder), (1, 0));
        self.uploading = Some((None, placeholder));
        self.image_upload = Some(placeholder);
        let shown =
            self.with_conversation(None, |app| media::push_image_row(app, &meta, png.clone()));
        if !shown {
            self.transfers.remove(&(None, placeholder));
            self.uploading = None;
            self.image_upload = None;
        }
        self.network.tx.send(NetworkControl::SendImage(png)).ok();
    }

    /// Our image came back from the server: the card it replaces goes
    /// (its picture and player with it) before the real row is pushed.
    pub(super) fn drop_image_card(&mut self) {
        let Some(placeholder) = self.image_upload.take() else {
            return;
        };
        self.uploading = None;
        self.transfers.remove(&(None, placeholder));
        self.with_conversation(None, |app| {
            let at = app
                .history
                .rows()
                .iter()
                .position(|r| r.message_id() == Some(placeholder));
            if let Some(i) = at
                && let Some(row) = app.chat_model.row_data(i)
            {
                app.images.remove(&row.row_id);
                app.gifs.remove(&row.row_id);
            }
            if let Some(delta) = app.history.discard_message(placeholder) {
                app.sync_range(&delta);
            }
        });
    }

    // ── Files (spec §7) ─────────────────────────────────────────────

    /// Announce a picked file to the server; the answer arrives as
    /// `UploadDone` (→ `file_uploaded`) or `UploadRejected`.
    pub fn begin_file_upload(&mut self, name: String, mime: String, bytes: Vec<u8>) {
        let upload_id = self.next_file_upload_id;
        self.next_file_upload_id = self
            .next_file_upload_id
            .wrapping_add(1)
            .max(FILE_UPLOAD_BASE);
        let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        // The card shows at once, at 0 %, and fills as the bytes go;
        // the same placeholder is confirmed by the post's ack later.
        let client_ref = self.next_client_ref;
        self.next_client_ref = self.next_client_ref.wrapping_add(1).max(1);
        let placeholder = u64::MAX - u64::from(client_ref);
        let conv = self.active_dm;
        let now = u64::try_from(chrono::Local::now().timestamp_millis()).unwrap_or(0);
        let meta = Meta {
            id: placeholder,
            ts: format_wall_time(now),
            ts_ms: now,
            day: local_day(now),
            user: self.settings.user.nickname.clone(),
            own: true,
            pending: true,
        };
        let file = app_core::chat::FileMeta {
            blob: [0u8; 32],
            name: name.clone(),
            mime: mime.clone(),
            size,
        };
        self.transfers.insert((conv, placeholder), (1, 0));
        self.uploading = Some((conv, placeholder));
        self.with_conversation(conv, |app| app.push_file_here(&meta, file));
        self.pending_uploads.insert(
            upload_id,
            PendingUpload {
                conv,
                name: name.clone(),
                placeholder,
            },
        );
        self.network
            .tx
            .send(NetworkControl::UploadBlob {
                upload_id,
                name,
                mime,
                data: bytes,
            })
            .ok();
    }

    /// The server has the bytes: post them into the conversation the
    /// file was picked for, under a placeholder row until the ack.
    pub(super) fn file_uploaded(
        &mut self,
        upload_id: u32,
        upload: PendingUpload,
        blob_id: client::protocol::BlobId,
    ) {
        let placeholder = upload.placeholder;
        #[allow(clippy::cast_possible_truncation)] // built from a u32
        let client_ref = (u64::MAX - placeholder) as u32;
        // Uploaded: the card knows its blob now (Save works once acked)
        // and the ring goes; the muted look stays until the ack.
        self.uploading = None;
        self.transfers.remove(&(upload.conv, placeholder));
        self.with_conversation(upload.conv, |app| {
            let at = app
                .history
                .rows()
                .iter()
                .position(|r| r.message_id() == Some(placeholder));
            if let Some(i) = at
                && let Some(existing) = app.history.file_at(i).cloned()
            {
                let file = app_core::chat::FileMeta {
                    blob: blob_id,
                    ..existing
                };
                app.history.set_file(i, file);
                let row = app.chat_row_at(i);
                app.chat_model.set_row_data(i, row);
            }
        });
        self.pending_files
            .insert(client_ref, (upload.conv, placeholder));
        // A picture: once acked, our own card shows it (no echo comes
        // back on a DM).
        if let Some(png) = self.pending_picture_uploads.remove(&upload_id) {
            self.picture_acks.insert(client_ref, png);
        }
        let conv = match upload.conv {
            None => client::ConvRef::General,
            Some(user) => client::ConvRef::Dm(user),
        };
        self.network
            .tx
            .send(NetworkControl::SendFile {
                conv,
                blob_id,
                client_ref,
            })
            .ok();
    }

    /// Show, move or clear a card's transfer ring: the state lives in
    /// `transfers` (rows are rebuilt at will) and the row is rebuilt
    /// from it.
    pub(super) fn set_transfer(
        &mut self,
        conv: Option<client::UserId>,
        msg_id: u64,
        state: Option<(i32, i32)>,
    ) {
        match state {
            Some(state) => {
                self.transfers.insert((conv, msg_id), state);
            }
            None => {
                self.transfers.remove(&(conv, msg_id));
            }
        }
        self.with_conversation(conv, |app| {
            if let Some(i) = app
                .history
                .rows()
                .iter()
                .position(|r| r.message_id() == Some(msg_id))
            {
                let row = app.chat_row_at(i);
                app.chat_model.set_row_data(i, row);
            }
        });
    }

    /// A saved card's Open: the file, with what the system uses for it.
    /// Gone from disk since → the card goes back to Save.
    pub(super) fn file_open(&mut self, row_id: i32) {
        let Some(index) = (0..self.chat_model.row_count()).find(|&i| {
            self.chat_model
                .row_data(i)
                .is_some_and(|r| r.row_id == row_id)
        }) else {
            return;
        };
        let Some(blob) = self.history.file_at(index).map(|f| f.blob) else {
            return;
        };
        let Some(path) = self.downloaded.get(&blob).cloned() else {
            return;
        };
        if !crate::host::target_exists(&path) {
            self.downloaded.remove(&blob);
            let row = self.chat_row_at(index);
            self.chat_model.set_row_data(index, row);
            self.show_toast(ToastKind::Warning, &t!("file-gone"));
            return;
        }
        if let Err(e) = crate::host::open_target(&path) {
            self.show_error(&t!("file-open-error", "error" => e));
        }
    }

    /// Stop a card's transfer: an upload's stream is reset and the
    /// pending card goes; a download's partial is dropped and the ring
    /// clears.
    pub(super) fn file_cancel(&mut self, row_id: i32) {
        let Some(index) = (0..self.chat_model.row_count()).find(|&i| {
            self.chat_model
                .row_data(i)
                .is_some_and(|r| r.row_id == row_id)
        }) else {
            return;
        };
        let Some(msg_id) = self.history.rows()[index].message_id() else {
            return;
        };
        let conv = self.active_dm;
        match self.transfers.get(&(conv, msg_id)).map(|t| t.0) {
            Some(1) if self.image_upload == Some(msg_id) => {
                self.network.tx.send(NetworkControl::CancelImage).ok();
                self.drop_image_card();
            }
            Some(1) => {
                if let Some((upload_id, _)) = self
                    .pending_uploads
                    .iter()
                    .find(|(_, u)| u.placeholder == msg_id)
                    .map(|(id, u)| (*id, u.conv))
                {
                    self.pending_uploads.remove(&upload_id);
                    self.uploading = None;
                    self.transfers.remove(&(conv, msg_id));
                    self.network
                        .tx
                        .send(NetworkControl::CancelUpload { upload_id })
                        .ok();
                    if let Some(delta) = self.history.discard_message(msg_id) {
                        self.sync_range(&delta);
                    }
                }
            }
            Some(2) => {
                let blob = self
                    .pending_downloads
                    .iter()
                    .find(|(_, d)| d.msg_id == msg_id && d.conv == conv)
                    .map(|(b, _)| *b);
                if let Some(blob) = blob {
                    self.pending_downloads.remove(&blob);
                    self.network
                        .tx
                        .send(NetworkControl::CancelFetch { blob_id: blob })
                        .ok();
                }
                self.set_transfer(conv, msg_id, None);
            }
            _ => {}
        }
    }

    /// A card's Save: fetch the blob; `Blob` writes it to Downloads.
    pub(super) fn file_save(&mut self, row_id: i32) {
        let Some(index) = (0..self.chat_model.row_count()).find(|&i| {
            self.chat_model
                .row_data(i)
                .is_some_and(|r| r.row_id == row_id)
        }) else {
            return;
        };
        let Some(file) = self.history.file_at(index).cloned() else {
            return;
        };
        let Some(msg_id) = self.history.rows()[index].message_id() else {
            return;
        };
        if self.pending_downloads.contains_key(&file.blob) {
            return;
        }
        let conv = self.active_dm;
        self.pending_downloads.insert(
            file.blob,
            PendingDownload {
                name: file.name.clone(),
                conv,
                msg_id,
            },
        );
        self.set_transfer(conv, msg_id, Some((2, 0)));
        self.network
            .tx
            .send(NetworkControl::FetchBlob { blob_id: file.blob })
            .ok();
    }

    /// A divider before the next message when its day differs from
    /// the last one shown.
    pub fn mark_day(&mut self, day: i64) {
        if self.history.last_day() != Some(day) {
            let delta = self.history.push_day_marker(&day_label(day), day);
            self.sync_history_tail(&delta);
        }
    }

    /// Mirror `inserted` rows put at the front of the history.
    pub(super) fn sync_prepend(&mut self, inserted: usize) {
        for i in 0..inserted {
            let row = self.chat_row_at(i);
            self.chat_model.insert(i, row);
        }
        self.refresh_grouping(inserted);
    }

    /// Ask the server for the page before the oldest message shown.
    pub(super) fn load_earlier(&mut self, limit: u16) {
        if self.history_page_pending.is_some() {
            return;
        }
        self.history_page_pending = Some(self.active_dm);
        self.main.global::<State>().set_history_loading(true);
        self.network
            .tx
            .send(NetworkControl::FetchHistory {
                conv: self
                    .active_dm
                    .map_or(client::ConvRef::General, client::ConvRef::Dm),
                before: self.oldest_history_id,
                limit,
            })
            .ok();
    }

    /// "Edit message" on one of ours: the composer starts from its
    /// full text; the draft that was there comes back on cancel.
    pub(super) fn row_edit(&mut self, msg_id: u64) {
        let Some(text) = self.own_texts.get(&msg_id).cloned() else {
            return;
        };
        let s = self.main.global::<State>();
        let previous_draft = if self.editing.is_some() {
            // Switching from one edit to another keeps the first
            // draft, not the half-edited text.
            self.editing
                .take()
                .map(|e| e.previous_draft)
                .unwrap_or_default()
        } else {
            s.get_draft().to_string()
        };
        self.editing = Some(Editing {
            msg_id,
            previous_draft,
        });
        s.set_draft(text.into());
        s.set_editing(true);
        self.main.invoke_focus_composer();
    }

    pub(super) fn edit_cancel(&mut self) {
        let s = self.main.global::<State>();
        if let Some(edit) = self.editing.take() {
            s.set_draft(edit.previous_draft.into());
        }
        s.set_editing(false);
        self.main.invoke_focus_composer();
    }

    /// "Delete message": ours through the author path, anyone's
    /// through the admin path when we hold `RESTRICT`; both confirm
    /// first.
    pub(super) fn row_delete(&mut self, msg_id: u64) {
        // Anything with a `meta` is ours to delete when we wrote it:
        // text, code, a picture, a file. Only someone else's needs
        // the permission.
        let own = self.history.rows().iter().any(|r| match r {
            app_core::chat::ChatItem::Message { meta, .. }
            | app_core::chat::ChatItem::Code { meta, .. }
            | app_core::chat::ChatItem::Image { meta, .. }
            | app_core::chat::ChatItem::File { meta, .. } => meta.id == msg_id && meta.own,
            _ => false,
        });
        let by_admin = !own;
        if by_admin
            && !client::permissions::has(self.permissions, client::permissions::DELETE_MESSAGES)
        {
            return;
        }
        self.pending_confirm = Some(Confirm::DeleteMessage { msg_id, by_admin });
        let s = self.main.global::<State>();
        s.set_confirm_title(t!("confirm-delete-message-title").into());
        s.set_confirm_body(
            t!(if by_admin {
                "confirm-delete-message-body-admin"
            } else {
                "confirm-delete-message-body"
            })
            .into(),
        );
        s.set_confirm_label(t!("confirm-delete-message").into());
        s.set_confirm_open(true);
    }

    pub(super) fn confirm_accept(&mut self) {
        let s = self.main.global::<State>();
        s.set_confirm_open(false);
        match self.pending_confirm.take() {
            Some(Confirm::DeleteMessage { msg_id, by_admin }) => {
                if by_admin {
                    self.send_admin(AdminCommand::DeleteMessage { msg_id }, PendingAdmin::Show);
                } else {
                    self.network
                        .tx
                        .send(NetworkControl::DeleteMessage { msg_id })
                        .ok();
                    self.pending_delete = Some(msg_id);
                    Self::arm_apply_timeout(msg_id, true);
                }
            }
            Some(Confirm::DeleteChannel { channel_id }) => {
                self.network
                    .tx
                    .send(NetworkControl::DeleteChannel { channel_id })
                    .ok();
            }
            None => {}
        }
        self.main.invoke_focus_composer();
    }

    pub(super) fn confirm_cancel(&mut self) {
        self.pending_confirm = None;
        self.main.global::<State>().set_confirm_open(false);
        self.main.invoke_focus_composer();
    }

    /// The server answers an edit or a deletion only by echoing it;
    /// when it refuses (the message left its ring) it says nothing.
    /// After three seconds without the echo, say so here.
    pub(super) fn arm_apply_timeout(msg_id: u64, delete: bool) {
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        let Some(weak) = weak else {
            return;
        };
        slint::Timer::single_shot(Duration::from_secs(3), move || {
            with_app(&weak, |app| {
                let still = if delete {
                    app.pending_delete.take_if(|id| *id == msg_id).is_some()
                } else {
                    app.pending_edit.take_if(|id| *id == msg_id).is_some()
                };
                if still {
                    app.show_toast(
                        ToastKind::Warning,
                        &t!(if delete {
                            "chat-delete-not-applied"
                        } else {
                            "chat-edit-not-applied"
                        }),
                    );
                }
            });
        });
    }

    /// Forget every in-flight chat interaction (a fresh session, or
    /// none): edits, confirms, paging.
    pub(super) fn reset_chat_flow(&mut self) {
        self.own_texts.clear();
        self.editing = None;
        self.pending_edit = None;
        self.pending_delete = None;
        self.pending_confirm = None;
        self.channel_dialog = None;
        self.history_page_pending = None;
        self.pending_dm_sends.clear();
        self.parked.clear();
        self.dm_order.clear();
        self.dm_meta.clear();
        self.dms_model.set_vec(Vec::new());
        self.active_dm = None;
        self.history_more = false;
        self.general_unread = 0;
        let s = self.main.global::<State>();
        s.set_general_unread(0);
        s.set_active_dm(-1);
        s.set_conv_name("".into());
        s.set_dm_unread_total(0);
        let s = self.main.global::<State>();
        s.set_editing(false);
        s.set_confirm_open(false);
        s.set_channel_dialog_open(false);
        s.set_history_more(false);
        s.set_history_loading(false);
    }

    /// Media-row plumbing lives in `media.rs`; these expose the model
    /// internals it needs.
    pub fn chat_model(&self) -> &Rc<VecModel<ChatRow>> {
        &self.chat_model
    }

    pub fn history_mut(&mut self) -> &mut ChatHistory {
        &mut self.history
    }

    pub fn gifs_mut(&mut self) -> &mut HashMap<i32, GifPlayer> {
        &mut self.gifs
    }

    pub fn set_pending_preview(&mut self, png: Vec<u8>) {
        self.pending_preview = Some(png);
    }

    /// Close the send preview and let go of its full-resolution
    /// texture: a property keeps its last image, and a 4K screenshot
    /// previewed once is 32 MB that would otherwise stay until the
    /// next one.
    pub fn close_preview(&mut self) {
        let s = self.main.global::<State>();
        s.set_preview_open(false);
        s.set_preview_image(slint::Image::default());
    }

    pub(super) fn gif_hover(&mut self, row_id: i32, entering: bool) {
        let needs_ticker = self
            .gifs
            .get_mut(&row_id)
            .is_some_and(|player| player.hover(entering));
        if needs_ticker {
            self.ensure_gif_ticker();
        }
    }

    /// Stop every chat-row GIF and the in-window preview overlay,
    /// restoring row previews and releasing frame streams. Called when
    /// the main window hides to the tray (there is nothing to animate)
    /// and from the hidden-window guard in
    /// [`gif_tick`](Self::gif_tick).
    pub fn freeze_chat_gifs(&mut self) {
        let mut restores: Vec<(i32, slint::Image)> = Vec::new();
        for (&row_id, player) in &mut self.gifs {
            if let Some(first) = player.force_freeze() {
                restores.push((row_id, first));
            }
        }
        for (row_id, image) in restores {
            for i in 0..self.chat_model.row_count() {
                if let Some(mut row) = self.chat_model.row_data(i)
                    && row.row_id == row_id
                {
                    row.image = image.clone();
                    self.chat_model.set_row_data(i, row);
                    break;
                }
            }
        }
        self.preview_gif = None;
    }

    /// Wipe the local chat: history, rows, image bytes, GIF players,
    /// textures. Purely local — everyone clears their own scrollback
    /// (people were reconnecting just to get an empty chat). Also the
    /// bluntest memory-release lever the user has.
    pub fn clear_chat(&mut self) {
        self.history_mut().clear();
        self.chat_model.set_vec(Vec::new());
        self.images.clear();
        self.gifs.clear();
        self.gif_ticker.stop();
    }

    /// Start the 33 ms animation ticker if it isn't already running.
    pub fn ensure_gif_ticker(&mut self) {
        if self.gif_ticker.running() {
            return;
        }
        let weak = APP.with(|a| a.borrow().as_ref().map(Rc::downgrade));
        if let Some(weak) = weak {
            self.gif_ticker.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(33),
                move || with_app(&weak, AppState::gif_tick),
            );
        }
    }

    /// One animation tick: advance every playing chat GIF and the
    /// overlay GIF; stop the ticker when nothing is playing.
    pub(super) fn gif_tick(&mut self) {
        use crate::media::GifStep;
        const DT_MS: f64 = 33.0;

        // (fn defined at top of gif_tick to satisfy items-after-statements)
        fn step_overlay(g: &mut crate::viewers::GalleryGif, dt_ms: f64) -> Option<slint::Image> {
            g.in_frame_ms += dt_ms;
            let mut advanced = false;
            for _ in 0..g.anim.frames.len() {
                let delay = f64::from(g.anim.frames[g.idx].delay_ms);
                if g.in_frame_ms < delay {
                    break;
                }
                g.in_frame_ms -= delay;
                g.idx = (g.idx + 1) % g.anim.frames.len();
                advanced = true;
            }
            advanced.then(|| {
                let frame = &g.anim.frames[g.idx];
                ui::convert::rgba_image(&frame.rgba, g.anim.width, g.anim.height)
            })
        }

        // A phone app in the background: nothing is on screen, the
        // gallery page included. Stop ticking (every frame would be a
        // redraw nobody sees); `host_shown` restarts the ticker, and
        // the players carry on from where they were.
        if !host_visible() {
            self.gif_ticker.stop();
            return;
        }
        // Tray/minimized: chat rows and the in-window preview are
        // invisible — stop paying decode for them. The gallery viewer
        // is its own window and may still be on screen, so it keeps
        // animating; the ticker dies below once nothing plays.
        if !self.main.window().is_visible() {
            self.freeze_chat_gifs();
        }

        let mut any_playing = false;
        let mut blits: Vec<(i32, slint::Image)> = Vec::new();
        for (&row_id, player) in &mut self.gifs {
            match player.step(DT_MS) {
                GifStep::Idle => {}
                GifStep::Blit {
                    rgba,
                    width,
                    height,
                } => blits.push((row_id, ui::convert::rgba_image(&rgba, width, height))),
                GifStep::Freeze { first } => blits.push((row_id, first)),
            }
            any_playing |= player.playing();
        }
        for (row_id, image) in blits {
            for i in 0..self.chat_model.row_count() {
                if let Some(mut row) = self.chat_model.row_data(i)
                    && row.row_id == row_id
                {
                    row.image = image.clone();
                    self.chat_model.set_row_data(i, row);
                    break;
                }
            }
        }

        if let Some(g) = &mut self.gallery_gif {
            any_playing = true;
            if let Some(img) = step_overlay(g, DT_MS) {
                self.main.global::<State>().set_viewer_image(img);
            }
        }
        if let Some(g) = &mut self.preview_gif {
            any_playing = true;
            if let Some(img) = step_overlay(g, DT_MS) {
                self.main.global::<State>().set_preview_image(img);
            }
        }

        if !any_playing {
            self.gif_ticker.stop();
        }
    }

    // ════════════════════════════════════════════════════════════════
    //  ROSTER / STATUS
    // ════════════════════════════════════════════════════════════════
}
