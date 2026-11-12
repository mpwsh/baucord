//! Voice channel recording on the tick (1.5): start, stop, who was
//! recorded, and delivery by Server.
//!
//! The tick decides and announces; the recorder thread
//! ([`crate::recorder`]) does the decoding and the disk. A recording is
//! kept by its channel's *name*, the one thing a renumbering list change
//! keeps. Everyone placed in the channel while it runs is a participant
//! and gets an access row at once (so a crash loses nobody); when the
//! file is ready each participant online is sent `RecordingReady`, and
//! the others find it in their list on their next visit. The file's
//! bytes are served to its participants alone
//! ([`ChatServer::may_fetch`]).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use log::{info, warn};
use protocol::recording::{RecordingDenyReason, RecordingItem, RecordingStopReason};
use protocol::{features, permissions, BlobId, ClientId, ServerMessage, UserId};

use super::{ChatServer, Waiting};
use crate::persistence::{Command, Query, RecordingEnd, RecordingFile, RecordingRow};
use crate::policy::BlobMeta;
use crate::recorder::RecEvent;

/// How long a recordings list may take to read.
const LIST_DEADLINE: Duration = Duration::from_secs(2);
/// Recordings in one page of the list.
const LIST_PAGE: u16 = 50;

/// A recording running in a channel.
#[derive(Debug, Clone)]
pub(super) struct ActiveRecording {
    pub rec_id: u64,
    /// The channel's name: the key across renumbering.
    pub channel: String,
    pub started: Instant,
    pub started_wall: u64,
    pub by: UserId,
    pub by_name: String,
    pub participants: HashSet<UserId>,
}

/// A recording that ended and waits for its file.
#[derive(Debug, Clone)]
pub(super) struct FinishingRecording {
    pub channel: String,
    pub started_wall: u64,
    pub by_name: String,
    pub duration_ms: u64,
    pub reason: RecordingStopReason,
    pub stopped_by: Option<UserId>,
    pub participants: HashSet<UserId>,
}

impl ChatServer {
    /// The wire id of the channel named `name`, if it is still listed.
    fn channel_id_named(&self, name: &str) -> Option<u16> {
        self.channel_list
            .iter()
            .position(|c| c.name.trim() == name)
            .and_then(|i| u16::try_from(i).ok())
    }

    fn channel_name_of(&self, channel_id: u16) -> Option<String> {
        self.channel_list
            .get(usize::from(channel_id))
            .map(|c| c.name.trim().to_string())
    }

    /// The recording running in `channel_id`, if any.
    pub(super) fn recording_in(&self, channel_id: u16) -> Option<&ActiveRecording> {
        let name = self.channel_name_of(channel_id)?;
        self.recordings.values().find(|r| r.channel == name)
    }

    /// The wire ids of the channels recording now: the relay's tap.
    pub(super) fn refresh_recording_tap(&mut self) {
        self.tapped.clear();
        let ids: Vec<u16> = self
            .recordings
            .values()
            .filter_map(|r| self.channel_id_named(&r.channel))
            .collect();
        self.tapped.extend(ids);
    }

    fn deny_recording(&mut self, client: ClientId, channel_id: u16, reason: RecordingDenyReason) {
        info!(
            "{} ({client:#x}): recording in channel {channel_id} refused: {}",
            self.username_of(client),
            reason.describe()
        );
        self.send_msg(
            client,
            &ServerMessage::RecordingDenied { channel_id, reason },
        );
    }

    /// The bytes recordings hold on disk now.
    fn recording_bytes(&self) -> u64 {
        self.recording_access
            .keys()
            .filter_map(|b| self.policy.blobs.get(b))
            .map(|b| u64::from(b.size))
            .sum()
    }

    /// Why `client` may not start recording `channel_id`, if it may not.
    fn start_refusal(&self, client: ClientId, channel_id: u16) -> Option<RecordingDenyReason> {
        let cfg = self.config.recording;
        if !cfg.enabled || !self.persist.is_persistent() || self.spool_dir.is_none() {
            return Some(RecordingDenyReason::Unavailable);
        }
        if !self.has_permission(client, permissions::RECORD) {
            return Some(RecordingDenyReason::NoPermission);
        }
        if self.channel_of.get(&client) != Some(&channel_id)
            || channel_id == self.default_channel
            || self.afk_channel() == Some(channel_id)
        {
            return Some(RecordingDenyReason::NotInChannel);
        }
        if self.recording_in(channel_id).is_some() {
            return Some(RecordingDenyReason::AlreadyRecording);
        }
        if self.recordings.len() >= usize::try_from(cfg.max_concurrent).unwrap_or(usize::MAX) {
            return Some(RecordingDenyReason::Busy);
        }
        if self.recording_bytes() >= u64::from(cfg.total_cap_gb) << 30 {
            return Some(RecordingDenyReason::StorageFull);
        }
        let everyone_sees_the_label = self
            .room_members
            .get(&channel_id)
            .is_none_or(|m| m.iter().all(|c| self.has_feature(*c, features::RECORDING)));
        (!everyone_sees_the_label).then_some(RecordingDenyReason::OldClients)
    }

    /// `StartRecording`.
    pub(super) fn handle_start_recording(&mut self, client: ClientId, channel_id: u16) {
        let Some(by) = self.user_of.get(&client).copied() else {
            return;
        };
        if let Some(reason) = self.start_refusal(client, channel_id) {
            self.deny_recording(client, channel_id, reason);
            return;
        }
        let cfg = self.config.recording;
        let members: Vec<ClientId> = self
            .room_members
            .get(&channel_id)
            .cloned()
            .unwrap_or_default();
        let Some(channel) = self.channel_name_of(channel_id) else {
            return;
        };
        let Some(spool) = self.spool_dir.clone() else {
            return;
        };
        let rec_id = self.next_rec_id;
        self.next_rec_id += 1;
        let by_name = self.username_of(client);
        let started_wall = self.wall_now();
        let participants: HashSet<UserId> = members
            .iter()
            .filter_map(|m| self.user_of.get(m).copied())
            .collect();
        self.persist.begin_group();
        self.persist.send(Command::InsertRecording(RecordingRow {
            rec_id,
            channel: channel.clone(),
            started_at: started_wall,
            started_by: Some(by),
            starter: by_name.clone(),
            sample_rate: cfg.sample_rate_hz,
        }));
        self.persist.send(Command::GrantRecording {
            rec_id,
            users: participants.iter().copied().collect(),
        });
        self.persist.end_group();
        self.recorder.open(
            rec_id,
            crate::recorder::spool_path(&spool, rec_id),
            cfg.sample_rate_hz,
            self.now,
        );
        info!(
            "{by_name} ({client:#x}) started recording '{channel}' (#{rec_id}, {} Hz, {} people)",
            cfg.sample_rate_hz,
            participants.len()
        );
        self.recordings.insert(
            rec_id,
            ActiveRecording {
                rec_id,
                channel: channel.clone(),
                started: self.now,
                started_wall,
                by,
                by_name: by_name.clone(),
                participants,
            },
        );
        self.refresh_recording_tap();
        self.audit(
            Some(by),
            "recording.start",
            Some(channel),
            Some(format!("#{rec_id}")),
        );
        let msg = ServerMessage::RecordingStarted {
            channel_id,
            rec_id,
            elapsed_ms: 0,
            by,
            by_name,
        };
        let everyone = self.clients();
        self.send_to(&everyone, &msg);
    }

    /// `StopRecording`: any holder of `record`, from anywhere.
    pub(super) fn handle_stop_recording(&mut self, client: ClientId, channel_id: u16) {
        let Some(by) = self.user_of.get(&client).copied() else {
            return;
        };
        if !self.has_permission(client, permissions::RECORD) {
            self.deny_recording(client, channel_id, RecordingDenyReason::NoPermission);
            return;
        }
        let Some(rec_id) = self.recording_in(channel_id).map(|r| r.rec_id) else {
            self.deny_recording(client, channel_id, RecordingDenyReason::NotRecording);
            return;
        };
        let name = self.username_of(client);
        self.end_recording(rec_id, RecordingStopReason::Stopped, Some((by, name)));
    }

    /// End a running recording: the tap goes, everyone is told, and the
    /// recorder finishes the file (or, at shutdown, parks it).
    pub(super) fn end_recording(
        &mut self,
        rec_id: u64,
        reason: RecordingStopReason,
        by: Option<(UserId, String)>,
    ) {
        let Some(rec) = self.recordings.remove(&rec_id) else {
            return;
        };
        self.refresh_recording_tap();
        let duration_ms =
            u64::try_from(self.now.duration_since(rec.started).as_millis()).unwrap_or(u64::MAX);
        let parking = reason == RecordingStopReason::ServerRestart;
        self.recorder.close(
            rec_id,
            self.now,
            if parking {
                None
            } else {
                self.blobs_dir.clone()
            },
        );
        info!(
            "recording #{rec_id} of '{}' ended after {} ({}{})",
            rec.channel,
            protocol::recording::clock_label(duration_ms),
            reason.describe(),
            by.as_ref()
                .map_or(String::new(), |(_, n)| format!(", by {n}"))
        );
        self.audit(
            by.as_ref().map(|(u, _)| *u),
            "recording.stop",
            Some(rec.channel.clone()),
            Some(format!(
                "#{rec_id}, {}, {}",
                protocol::recording::clock_label(duration_ms),
                reason.describe()
            )),
        );
        if parking {
            // The next start finishes the file from its spool, under
            // this reason (`repair_recordings`).
            self.persist
                .send(Command::FinishRecording(Box::new(RecordingEnd {
                    rec_id,
                    duration_ms,
                    stopped_by: by.as_ref().map(|(u, _)| *u),
                    reason,
                    file: None,
                })));
        }
        if let Some(channel_id) = self.channel_id_named(&rec.channel) {
            let msg = ServerMessage::RecordingStopped {
                channel_id,
                rec_id,
                duration_ms,
                reason,
                by_name: by.as_ref().map(|(_, n)| n.clone()),
            };
            let everyone = self.clients();
            self.send_to(&everyone, &msg);
        }
        self.finishing.insert(
            rec_id,
            FinishingRecording {
                channel: rec.channel,
                started_wall: rec.started_wall,
                by_name: rec.by_name,
                duration_ms,
                reason,
                stopped_by: by.map(|(u, _)| u),
                participants: rec.participants,
            },
        );
    }

    /// A session was placed in `channel_id`: recorded from now on if
    /// it is recording.
    pub(super) fn note_recording_participant(&mut self, client: ClientId, channel_id: u16) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        let Some(name) = self.channel_name_of(channel_id) else {
            return;
        };
        let Some(rec) = self.recordings.values_mut().find(|r| r.channel == name) else {
            return;
        };
        if rec.participants.insert(user) {
            let rec_id = rec.rec_id;
            self.persist.send(Command::GrantRecording {
                rec_id,
                users: vec![user],
            });
        }
    }

    /// Whether a session may join a channel that is recording: only a
    /// client that can show the label.
    pub(super) fn may_join_recording(&self, client: ClientId, channel_id: u16) -> bool {
        self.recording_in(channel_id).is_none() || self.has_feature(client, features::RECORDING)
    }

    /// After membership or the list changed: a recording whose channel
    /// emptied or went away ends.
    pub(super) fn check_recordings(&mut self) {
        if self.recordings.is_empty() {
            return;
        }
        let ended: Vec<(u64, RecordingStopReason)> = self
            .recordings
            .values()
            .filter_map(|r| match self.channel_id_named(&r.channel) {
                None => Some((r.rec_id, RecordingStopReason::ChannelGone)),
                Some(id) if !self.room_members.contains_key(&id) => {
                    Some((r.rec_id, RecordingStopReason::Empty))
                }
                Some(_) => None,
            })
            .collect();
        for (rec_id, reason) in ended {
            self.end_recording(rec_id, reason, None);
        }
        self.refresh_recording_tap();
    }

    /// Once a second: the time limit.
    pub(super) fn sweep_recordings(&mut self) {
        let limit = Duration::from_secs(u64::from(self.config.recording.max_minutes) * 60);
        let over: Vec<u64> = self
            .recordings
            .values()
            .filter(|r| self.now.duration_since(r.started) >= limit)
            .map(|r| r.rec_id)
            .collect();
        for rec_id in over {
            self.end_recording(rec_id, RecordingStopReason::TimeLimit, None);
        }
    }

    /// Every tick: the relay's copies to the recorder, and the
    /// recorder's news.
    pub(super) fn pump_recordings(&mut self) {
        let tapped = std::mem::take(&mut self.voice.tapped);
        for (channel_id, sender, bytes) in tapped {
            if let Some(rec_id) = self.recording_in(channel_id).map(|r| r.rec_id) {
                self.recorder.packet(rec_id, sender, self.now, bytes);
            }
        }
        if !self.recorder.running() {
            return;
        }
        for event in self.recorder.poll() {
            self.recorder_event(event);
        }
    }

    fn recorder_event(&mut self, event: RecEvent) {
        match event {
            RecEvent::Finished {
                rec_id,
                blob_id,
                size,
                duration_ms,
            } => self.deliver_recording(rec_id, blob_id, size, duration_ms),
            RecEvent::WriteFailed { rec_id, error } => {
                warn!("recording #{rec_id}: writing failed ({error}); it ends here");
                self.end_recording(rec_id, RecordingStopReason::StorageFull, None);
            }
            RecEvent::Failed { rec_id, error } => {
                warn!("recording #{rec_id} failed: {error}");
                self.audit(
                    None,
                    "recording.fail",
                    None,
                    Some(format!("#{rec_id}: {error}")),
                );
                if self.recordings.contains_key(&rec_id) {
                    self.end_recording(rec_id, RecordingStopReason::StorageFull, None);
                }
                if let Some(f) = self.finishing.remove(&rec_id) {
                    self.persist
                        .send(Command::FinishRecording(Box::new(RecordingEnd {
                            rec_id,
                            duration_ms: f.duration_ms,
                            stopped_by: f.stopped_by,
                            reason: f.reason,
                            file: None,
                        })));
                }
            }
            RecEvent::Parked { .. } => {}
        }
    }

    /// The file is in the blob store: its row closes, its people may
    /// fetch it, and those online hear from Server.
    fn deliver_recording(&mut self, rec_id: u64, blob_id: BlobId, size: u64, duration_ms: u64) {
        let Some(f) = self.finishing.remove(&rec_id) else {
            return;
        };
        let file_name = protocol::recording::file_name(&f.channel, f.started_wall, duration_ms);
        self.persist
            .send(Command::FinishRecording(Box::new(RecordingEnd {
                rec_id,
                duration_ms,
                stopped_by: f.stopped_by,
                reason: f.reason,
                file: Some(RecordingFile {
                    blob_id,
                    size,
                    name: file_name.clone(),
                    created_at: self.wall_now(),
                }),
            })));
        let meta = self
            .policy
            .blobs
            .entry(blob_id)
            .or_insert_with(|| BlobMeta {
                mime: "audio/wav".to_string(),
                size: u32::try_from(size).unwrap_or(u32::MAX),
                name: file_name.clone(),
                created_by: None,
                created_at: self.clock.wall_ms(self.now),
                refs: 0,
            });
        meta.refs = meta.refs.saturating_add(1);
        self.recording_access
            .entry(blob_id)
            .or_default()
            .extend(f.participants.iter().copied());
        self.recording_blob_of.insert(rec_id, blob_id);
        info!(
            "recording #{rec_id} of '{}' is ready ({}); sent to {} people",
            f.channel,
            crate::server::files::human_size(size),
            f.participants.len()
        );
        let item = RecordingItem {
            rec_id,
            channel: f.channel,
            started_at: f.started_wall,
            duration_ms,
            started_by: f.by_name,
            reason: f.reason,
            blob_id,
            file_name,
            size,
            sample_rate: self.config.recording.sample_rate_hz,
            seen: false,
        };
        let msg = ServerMessage::RecordingReady { item };
        let sessions: Vec<ClientId> = f
            .participants
            .iter()
            .flat_map(|u| self.sessions_of(u))
            .collect();
        self.send_to(&sessions, &msg);
    }

    /// Whether `client` may have `blob_id`'s bytes: anything but a
    /// recording; a recording only for its participants.
    pub(super) fn may_fetch(&self, client: ClientId, blob_id: &BlobId) -> bool {
        let Some(allowed) = self.recording_access.get(blob_id) else {
            return true;
        };
        self.user_of
            .get(&client)
            .is_some_and(|u| allowed.contains(u))
    }

    // ── The list in the "Server" conversation ───────────────────────

    /// After `Session`, and on `FetchRecordings`: a page of this user's
    /// recordings.
    pub(super) fn handle_fetch_recordings(&mut self, client: ClientId, before: Option<u64>) {
        if !self.has_feature(client, features::RECORDING) || !self.persist.is_persistent() {
            return;
        }
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        let ask = self.persist.ask(
            Query::Recordings {
                user,
                before,
                limit: LIST_PAGE,
            },
            self.now + LIST_DEADLINE,
        );
        self.waiting.insert(ask, Waiting::Recordings(client));
    }

    /// The store answered a list.
    pub(super) fn resolve_recordings(
        &mut self,
        client: ClientId,
        mut items: Vec<RecordingItem>,
        more: bool,
    ) {
        let kept = items.len();
        protocol::keep_prefix_that_fits(&mut items, protocol::LIST_ITEMS_BUDGET);
        let more = more || items.len() < kept;
        self.send_msg(client, &ServerMessage::Recordings { items, more });
    }

    /// `RemoveRecording`: off this user's list, and out of their reach.
    pub(super) fn handle_remove_recording(&mut self, client: ClientId, rec_id: u64) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        if let Some(blob) = self.recording_blob_of.get(&rec_id)
            && let Some(set) = self.recording_access.get_mut(blob)
        {
            set.remove(&user);
        }
        self.persist
            .send(Command::RemoveRecordingAccess { rec_id, user });
    }

    /// `MarkRecordingsSeen`.
    pub(super) fn handle_recordings_seen(&mut self, client: ClientId, up_to: u64) {
        let Some(user) = self.user_of.get(&client).copied() else {
            return;
        };
        self.persist.send(Command::RecordingsSeen { user, up_to });
    }

    /// After `Init`: what is recording now, for the REC labels.
    pub(super) fn send_recordings_running(&mut self, client: ClientId) {
        let running: Vec<ServerMessage> = self
            .recordings
            .values()
            .filter_map(|r| {
                Some(ServerMessage::RecordingStarted {
                    channel_id: self.channel_id_named(&r.channel)?,
                    rec_id: r.rec_id,
                    elapsed_ms: u64::try_from(self.now.duration_since(r.started).as_millis())
                        .unwrap_or(u64::MAX),
                    by: r.by,
                    by_name: r.by_name.clone(),
                })
            })
            .collect();
        for msg in &running {
            self.send_msg(client, msg);
        }
    }

    /// The server is stopping: every recording ends, parked for the
    /// next start to finish.
    pub(super) fn park_recordings(&mut self) {
        let ids: Vec<u64> = self.recordings.keys().copied().collect();
        for rec_id in ids {
            self.end_recording(rec_id, RecordingStopReason::ServerRestart, None);
        }
    }
}
