//! What changes by the second in a voice channel (1.5): the channel
//! timer, recordings, and the "Server" conversation the recordings
//! arrive in.
//!
//! The server sends a channel timer's elapsed time on each edge and a
//! recording's on start; this side counts on from its own monotonic
//! clock. One 1 s timer rewrites the channel rows' timer text in place,
//! and runs only while a timer runs and the main screen is drawn.
//!
//! A recording the user was in arrives from Server: a file card in a
//! conversation of its own, pinned first among the DMs. Its Save is a
//! streamed download written to disk as it comes, not a blob held in
//! memory — an hour is a third of a gigabyte.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

use client::protocol::recording::{
    clock_label, RecordingDenyReason, RecordingItem, RecordingStopReason,
};

#[allow(clippy::wildcard_imports)]
use super::*;

/// The "Server" conversation's key: no account has it.
pub(crate) const SERVER_CONV_KEY: client::UserId = [0xFE; 32];

/// A recording running in a channel, as this session knows it.
#[derive(Debug, Clone)]
pub(crate) struct Running {
    pub since: Instant,
    pub by: String,
}

/// The per-session state of this module.
#[derive(Debug, Default)]
pub(crate) struct Live {
    /// When each occupied channel's timer started, on our clock.
    pub clocks: HashMap<u16, Instant>,
    /// Recordings running, by channel.
    pub recording: HashMap<u16, Running>,
    /// Our recordings, by their file.
    pub items: HashMap<[u8; 32], RecordingItem>,
    /// Older recordings exist on the server.
    pub more: bool,
    /// Streamed downloads in progress: the file and where its card is.
    pub downloads: HashMap<[u8; 32], (String, u64)>,
}

fn since(elapsed_ms: u64) -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_millis(elapsed_ms))
        .unwrap_or(now)
}

fn ms_since(at: Instant) -> u64 {
    u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl AppState {
    // ── The channel timer ───────────────────────────────────────────

    pub(super) fn take_channel_clock(&mut self, channel_id: u16, running_ms: Option<u64>) {
        match running_ms {
            Some(ms) => {
                self.session.live.clocks.insert(channel_id, since(ms));
            }
            None => {
                self.session.live.clocks.remove(&channel_id);
            }
        }
        self.update_user_list();
        self.sync_clock_timer();
    }

    /// A channel's timer text, `HH:MM:SS`, or nothing.
    pub(super) fn timer_label(&self, channel_id: u16) -> String {
        self.session
            .live
            .clocks
            .get(&channel_id)
            .map(|at| clock_label(ms_since(*at)))
            .unwrap_or_default()
    }

    /// What a channel's row shows of this module: its timer, whether
    /// it is recording, and what we may do about that.
    pub(super) fn decorate_channel_row(&self, row: &mut UserRow, channel_id: u16) {
        row.timer_label = self.timer_label(channel_id).into();
        let may_record =
            client::permissions::has(self.session.permissions, client::permissions::RECORD);
        match self.session.live.recording.get(&channel_id) {
            Some(rec) => {
                row.recording = true;
                row.can_stop_recording = may_record;
                row.rec_tip = t!(
                    "recording-tip",
                    "name" => rec.by.as_str(),
                    "elapsed" => clock_label(ms_since(rec.since))
                )
                .into();
            }
            None => {
                row.can_record = may_record
                    && self.our_channel() == Some(channel_id)
                    && !self.in_lobby()
                    && !self
                        .session
                        .channels
                        .iter()
                        .any(|c| c.id == channel_id && c.is_afk);
            }
        }
    }

    /// Run the 1 s timer while there is a count to show and someone to
    /// see it; stop it otherwise.
    pub(super) fn sync_clock_timer(&mut self) {
        let wanted = (!self.session.live.clocks.is_empty()
            || !self.session.live.recording.is_empty())
            && self.main_on_screen();
        if wanted == self.clock_timer.running() {
            return;
        }
        if wanted {
            self.clock_timer
                .start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
                    with_current(AppState::tick_clocks)
                });
        } else {
            self.clock_timer.stop();
        }
    }

    /// Once a second: the channel rows' timer text and recording tips,
    /// in place; nothing else is rebuilt.
    fn tick_clocks(&mut self) {
        if !self.main_on_screen() {
            self.sync_clock_timer();
            return;
        }
        for i in 0..self.users_model.row_count() {
            let Some(mut row) = self.users_model.row_data(i) else {
                continue;
            };
            if !row.is_channel {
                continue;
            }
            let Ok(channel_id) = u16::try_from(row.channel_id) else {
                continue;
            };
            let before = row.clone();
            self.decorate_channel_row(&mut row, channel_id);
            if row != before {
                self.users_model.set_row_data(i, row);
            }
        }
        self.refresh_stage_header();
    }

    // ── Recordings in channels ──────────────────────────────────────

    pub(super) fn take_recording_started(
        &mut self,
        channel_id: u16,
        elapsed_ms: u64,
        by_name: &str,
    ) {
        let fresh = elapsed_ms < 5_000;
        let name = self.channel_name(channel_id);
        self.session.live.recording.insert(
            channel_id,
            Running {
                since: since(elapsed_ms),
                by: by_name.to_string(),
            },
        );
        if fresh {
            self.push_activity(&t!(
                "activity-recording-started",
                "name" => by_name,
                "channel" => name.as_str()
            ));
            if self.our_channel() == Some(channel_id) {
                self.play_chime(SoundType::RecordingStarted);
            }
        }
        self.update_user_list();
        self.sync_clock_timer();
        // A recording hears us even alone: the microphone sends now.
        self.sync_voice_room();
    }

    pub(super) fn take_recording_stopped(
        &mut self,
        channel_id: u16,
        duration_ms: u64,
        reason: RecordingStopReason,
        by_name: Option<String>,
    ) {
        self.session.live.recording.remove(&channel_id);
        let name = self.channel_name(channel_id);
        let duration = clock_label(duration_ms);
        let why = t!(stop_reason_key(reason));
        let line = if let (RecordingStopReason::Stopped, Some(by)) = (reason, by_name) {
            t!(
                "activity-recording-stopped-by",
                "name" => by.as_str(),
                "channel" => name.as_str(),
                "duration" => duration.as_str()
            )
        } else {
            t!(
                "activity-recording-ended",
                "channel" => name.as_str(),
                "duration" => duration.as_str(),
                "reason" => why.as_str()
            )
        };
        self.push_activity(&line);
        if self.our_channel() == Some(channel_id) {
            self.play_chime(SoundType::RecordingStopped);
        }
        self.update_user_list();
        self.sync_clock_timer();
        self.sync_voice_room();
    }

    pub(super) fn take_recording_denied(&mut self, reason: RecordingDenyReason) {
        self.show_toast(ToastKind::Warning, &t!(deny_reason_key(reason)));
    }

    /// We moved into `channel_id`: say so if it is recording.
    pub(super) fn arrived_in(&mut self, channel_id: u16) {
        if self.session.live.recording.contains_key(&channel_id) {
            self.show_toast(ToastKind::Info, &t!("recording-joined-toast"));
            self.play_chime(SoundType::RecordingStarted);
        }
    }

    /// The record button, the menu entry, `/record`.
    pub(super) fn start_recording(&mut self, channel_id: u16) {
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::StartRecording {
                channel_id,
            }))
            .ok();
    }

    /// The REC label's stop, the menu entry: asked first. `/record stop`
    /// is not (it was typed).
    pub(super) fn ask_stop_recording(&mut self, channel_id: u16) {
        let name = self.channel_name(channel_id);
        self.session.chat.pending_confirm = Some(Confirm::StopRecording { channel_id });
        let s = self.main.global::<State>();
        s.set_confirm_title(t!("recording-stop-title", "channel" => name.as_str()).into());
        s.set_confirm_body(t!("recording-stop-body").into());
        s.set_confirm_label(t!("recording-stop").into());
        s.set_confirm_open(true);
    }

    pub(super) fn stop_recording(&mut self, channel_id: u16) {
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::StopRecording {
                channel_id,
            }))
            .ok();
    }

    /// `/record`, `/record stop`: in our channel.
    pub(super) fn handle_record_command(&mut self, args: &str) {
        let Some(channel_id) = self.our_channel().filter(|_| !self.in_lobby()) else {
            self.push_command_output(&t!("recording-not-in-channel"));
            return;
        };
        if args.trim().eq_ignore_ascii_case("stop") {
            self.stop_recording(channel_id);
        } else {
            self.start_recording(channel_id);
        }
    }

    // ── The "Server" conversation ───────────────────────────────────

    /// Its row exists, first among the DMs, never fetched from the DM
    /// store.
    pub(super) fn ensure_server_conv(&mut self) {
        self.session
            .chat
            .dm_meta
            .entry(SERVER_CONV_KEY)
            .or_insert_with(|| DmMeta {
                name: t!("server-conv"),
                unread: 0,
                last_ts: 0,
                last_msg_id: 0,
                fetched: true,
                ..Default::default()
            });
        if !self.session.chat.dm_order.contains(&SERVER_CONV_KEY) {
            self.session.chat.dm_order.insert(0, SERVER_CONV_KEY);
            self.with_conversation(Some(SERVER_CONV_KEY), |app| {
                app.push_system_here(&t!("server-conv-lead"));
            });
        }
    }

    fn push_recording_card(&mut self, item: &RecordingItem) {
        let meta = Meta {
            id: item.rec_id,
            ts: format_wall_time(item.started_at),
            ts_ms: item.started_at,
            day: local_day(item.started_at),
            user: t!("server-conv"),
            author: None,
            own: false,
            pending: false,
        };
        let file = app_core::chat::FileMeta {
            blob: item.blob_id,
            name: item.file_name.clone(),
            mime: "audio/wav".to_string(),
            size: u32::try_from(item.size).unwrap_or(u32::MAX),
        };
        self.session.live.items.insert(item.blob_id, item.clone());
        if let Some(conv) = self.session.chat.dm_meta.get_mut(&SERVER_CONV_KEY)
            && item.started_at >= conv.last_ts
        {
            conv.last_ts = item.started_at;
            conv.preview = item.file_name.clone();
            conv.preview_mine = false;
        }
        self.with_conversation(Some(SERVER_CONV_KEY), |app| {
            let line = t!(
                "recording-card",
                "channel" => item.channel.as_str(),
                "duration" => clock_label(item.duration_ms),
                "name" => item.started_by.as_str()
            );
            app.push_system_here(&line);
            app.push_file_here(&meta, file);
        });
    }

    fn note_unseen(&mut self, items: &[RecordingItem]) {
        let unseen = u32::try_from(items.iter().filter(|i| !i.seen).count()).unwrap_or(0);
        let newest = items.iter().map(|i| i.rec_id).max().unwrap_or(0);
        let open = self.session.chat.active_dm == Some(SERVER_CONV_KEY);
        if let Some(meta) = self.session.chat.dm_meta.get_mut(&SERVER_CONV_KEY) {
            meta.last_msg_id = meta.last_msg_id.max(newest);
            if !open {
                meta.unread += unseen;
            }
        }
        if open && unseen > 0 {
            self.mark_recordings_seen(newest);
        }
        self.rebuild_dms_model();
    }

    /// Our recordings, newest first, after `Session` (or an older page).
    pub(super) fn take_recordings(&mut self, items: &[RecordingItem], more: bool) {
        if items.is_empty() && !self.session.chat.dm_order.contains(&SERVER_CONV_KEY) {
            return;
        }
        self.ensure_server_conv();
        self.session.live.more = more;
        for item in items.iter().rev() {
            if !self.session.live.items.contains_key(&item.blob_id) {
                self.push_recording_card(item);
            }
        }
        self.note_unseen(items);
    }

    /// A recording we were in is ready.
    pub(super) fn take_recording_ready(&mut self, item: &RecordingItem) {
        self.ensure_server_conv();
        self.push_recording_card(item);
        self.note_unseen(std::slice::from_ref(item));
        let notice = t!("recording-ready", "channel" => item.channel.as_str());
        self.show_toast(ToastKind::Success, &notice);
    }

    pub(super) fn mark_recordings_seen(&mut self, up_to: u64) {
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::MarkRecordingsSeen {
                up_to,
            }))
            .ok();
    }

    /// "Remove from my list" on a recording's card.
    pub(super) fn remove_recording(&mut self, blob: [u8; 32]) {
        let Some(item) = self.session.live.items.remove(&blob) else {
            return;
        };
        self.network
            .tx
            .send(NetworkControl::Send(ClientMessage::RemoveRecording {
                rec_id: item.rec_id,
            }))
            .ok();
        self.with_conversation(Some(SERVER_CONV_KEY), |app| {
            if let Some(delta) = app.history.delete_message(item.rec_id) {
                app.sync_range(&delta);
            }
        });
    }

    /// The file a chat row on screen carries, if it is a card.
    pub(super) fn file_blob_of_row(&self, row_id: i32) -> Option<[u8; 32]> {
        let index = (0..self.chat_model.row_count()).find(|&i| {
            self.chat_model
                .row_data(i)
                .is_some_and(|r| r.row_id == row_id)
        })?;
        self.history.file_at(index).map(|f| f.blob)
    }

    /// Whether `blob` is one of our recordings (its card saves through
    /// a streamed download).
    pub(super) fn is_recording(&self, blob: &[u8; 32]) -> bool {
        self.session.live.items.contains_key(blob)
    }

    // ── Streamed saves ──────────────────────────────────────────────

    /// A recording card's Save: written to a partial file as it comes,
    /// then moved into Downloads.
    pub(super) fn save_recording(
        &mut self,
        blob: [u8; 32],
        conv: Option<client::UserId>,
        msg_id: u64,
    ) {
        let Some((name, size)) = self
            .session
            .live
            .items
            .get(&blob)
            .map(|i| (i.file_name.clone(), i.size))
        else {
            return;
        };
        if self.session.live.downloads.contains_key(&blob) {
            return;
        }
        let path = client::config::partial_downloads_dir().join(format!("{}.part", hex(&blob)));
        self.session
            .live
            .downloads
            .insert(blob, (name.clone(), msg_id));
        self.session
            .transfers
            .pending_downloads
            .insert(blob, PendingDownload { name, conv, msg_id });
        self.set_transfer(conv, msg_id, Some((2, 0)));
        self.network
            .tx
            .send(NetworkControl::DownloadBlob {
                blob_id: blob,
                size,
                path,
            })
            .ok();
    }

    pub(super) fn download_progress(&mut self, blob: [u8; 32], pct: u8) {
        if let Some(p) = self.session.transfers.pending_downloads.get(&blob) {
            let (conv, msg_id) = (p.conv, p.msg_id);
            self.set_transfer(conv, msg_id, Some((2, i32::from(pct))));
        }
    }

    pub(super) fn download_done(&mut self, blob: [u8; 32], path: std::path::PathBuf) {
        self.session.live.downloads.remove(&blob);
        let Some(PendingDownload { name, conv, msg_id }) =
            self.session.transfers.pending_downloads.remove(&blob)
        else {
            std::fs::remove_file(&path).ok();
            return;
        };
        self.set_transfer(conv, msg_id, Some((2, 100)));
        let shown_name = name.clone();
        let spawned = std::thread::Builder::new()
            .name("save-download".into())
            .spawn(move || {
                let result = media::move_into_downloads(&name, &path);
                let _ = slint::invoke_from_event_loop(move || {
                    with_current(|app| app.download_saved(blob, &name, conv, msg_id, result));
                });
            });
        if let Err(e) = spawned {
            let error = e.to_string();
            self.show_error(&t!(
                "file-save-error",
                "name" => shown_name.as_str(),
                "error" => error.as_str()
            ));
        }
    }

    pub(super) fn download_failed(&mut self, blob: [u8; 32]) {
        self.session.live.downloads.remove(&blob);
        if let Some(PendingDownload { name, conv, msg_id }) =
            self.session.transfers.pending_downloads.remove(&blob)
        {
            self.set_transfer(conv, msg_id, None);
            self.show_toast(
                ToastKind::Error,
                &t!("download-failed", "name" => name.as_str()),
            );
        }
        let path = client::config::partial_downloads_dir().join(format!("{}.part", hex(&blob)));
        std::fs::remove_file(path).ok();
    }
}

fn stop_reason_key(reason: RecordingStopReason) -> &'static str {
    match reason {
        RecordingStopReason::Stopped => "recording-reason-stopped",
        RecordingStopReason::Empty => "recording-reason-empty",
        RecordingStopReason::TimeLimit => "recording-reason-time-limit",
        RecordingStopReason::StorageFull => "recording-reason-storage",
        RecordingStopReason::ChannelGone => "recording-reason-channel-gone",
        RecordingStopReason::ServerRestart => "recording-reason-restart",
        RecordingStopReason::Interrupted => "recording-reason-interrupted",
    }
}

fn deny_reason_key(reason: RecordingDenyReason) -> &'static str {
    match reason {
        RecordingDenyReason::NoPermission => "recording-denied-permission",
        RecordingDenyReason::NotInChannel => "recording-denied-channel",
        RecordingDenyReason::AlreadyRecording => "recording-denied-already",
        RecordingDenyReason::NotRecording => "recording-denied-not-recording",
        RecordingDenyReason::Unavailable => "recording-denied-unavailable",
        RecordingDenyReason::Busy => "recording-denied-busy",
        RecordingDenyReason::StorageFull => "recording-denied-storage",
        RecordingDenyReason::OldClients => "recording-denied-old-clients",
    }
}
