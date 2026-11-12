//! Cameras (1.5, feature bit `CAMERA`): someone's camera, seen by the
//! people in their channel.
//!
//! A camera is relayed exactly as a channel-scoped screen share is —
//! groups per keyframe, a subscription per viewer, the keyframe gate —
//! by a second [`crate::relay::VideoRelay`] whose viewer tracks carry
//! [`protocol::recording::CAMERA_TRACK_BIT`]. It reaches the server on
//! the uplink track [`protocol::recording::CAMERA_UPLINK_TRACK`]. A
//! camera is seen by its channel only, and a move or a return to the
//! lobby turns it off.

use log::info;
use protocol::{
    features, permissions, ClientId, DenyReason, RestrictionKind, ServerMessage, ShareScope,
};

use super::ChatServer;
use crate::relay::ShareInfo;

/// The camera sizes a client may announce.
const MIN_CAMERA_SIDE: u16 = 16;
const MAX_CAMERA_SIDE: u16 = 1920;

impl ChatServer {
    /// The people in `client`'s channel who can see cameras, `client`
    /// left out.
    fn camera_audience(&self, client: ClientId) -> Vec<ClientId> {
        let Some(ch) = self.channel_of.get(&client).copied() else {
            return Vec::new();
        };
        self.room_members
            .get(&ch)
            .map(|m| {
                m.iter()
                    .copied()
                    .filter(|p| *p != client && self.has_feature(*p, features::CAMERA))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn deny_camera(&mut self, client: ClientId, target_id: Option<ClientId>, reason: DenyReason) {
        self.send_msg(client, &ServerMessage::CameraDenied { target_id, reason });
    }

    /// `CameraStart`.
    pub(super) fn handle_camera_start(&mut self, client: ClientId, width: u16, height: u16) {
        if !self.allowed(client, permissions::CAMERA, RestrictionKind::Share) {
            self.deny_camera(client, None, DenyReason::NotPermitted);
            return;
        }
        let Some(ch) = self.channel_of.get(&client).copied() else {
            return;
        };
        if ch == self.default_channel || self.afk_channel() == Some(ch) {
            self.deny_camera(client, None, DenyReason::NotInChannel);
            return;
        }
        if !(MIN_CAMERA_SIDE..=MAX_CAMERA_SIDE).contains(&width)
            || !(MIN_CAMERA_SIDE..=MAX_CAMERA_SIDE).contains(&height)
        {
            self.deny_camera(client, None, DenyReason::InvalidDimensions);
            return;
        }
        let on_here = self.room_members.get(&ch).map_or(0, |m| {
            m.iter().filter(|p| self.cameras.is_sharing(**p)).count()
        });
        if !self.cameras.is_sharing(client)
            && on_here >= self.config.limits.max_cameras_per_channel
            && !self.has_permission(client, permissions::BYPASS_LIMITS)
        {
            self.deny_camera(client, None, DenyReason::ShareLimitReached);
            return;
        }
        self.note_activity(client);
        self.cameras.start_sharing(
            client,
            ShareInfo {
                width,
                height,
                scope: ShareScope::Channel,
            },
        );
        info!(
            "{} ({client:#x}) turned their camera on ({width}x{height})",
            self.username_of(client)
        );
        let audience = self.camera_audience(client);
        self.send_to(
            &audience,
            &ServerMessage::CameraStarted {
                client_id: client,
                width,
                height,
            },
        );
    }

    /// `CameraStop`, and every way a camera ends: its viewers' tracks
    /// close and the channel is told.
    pub(super) fn stop_camera(&mut self, client: ClientId, audience: &[ClientId]) {
        if !self.cameras.is_sharing(client) {
            return;
        }
        self.cameras
            .stop_sharing(&mut self.driver, self.now, client);
        info!("{} ({client:#x}) camera off", self.username_of(client));
        let msg = ServerMessage::CameraStopped { client_id: client };
        self.send_to(audience, &msg);
        self.send_msg(client, &msg);
    }

    /// `CameraStop`.
    pub(super) fn handle_camera_stop(&mut self, client: ClientId) {
        let audience = self.camera_audience(client);
        self.stop_camera(client, &audience);
    }

    /// `SubscribeCamera`.
    pub(super) fn handle_subscribe_camera(&mut self, client: ClientId, target: ClientId) {
        if self.cameras.share_info(target).is_none() {
            self.deny_camera(client, Some(target), DenyReason::NotSharing);
            return;
        }
        if self.in_lobby(client) || self.channel_of.get(&client) != self.channel_of.get(&target) {
            self.deny_camera(client, Some(target), DenyReason::NotInChannel);
            return;
        }
        if !self.cameras.is_subscribed(client, target)
            && self.cameras.viewer_count(target) >= self.config.limits.max_viewers_per_stream
            && !self.has_permission(client, permissions::BYPASS_LIMITS)
        {
            self.deny_camera(client, Some(target), DenyReason::ViewerLimitReached);
            return;
        }
        self.cameras.subscribe(client, target);
        if self.cameras.allow_keyframe_request(self.now, target) {
            self.send_msg(
                target,
                &ServerMessage::CameraKeyframeRequested { viewer_id: client },
            );
        }
    }

    /// `UnsubscribeCamera`.
    pub(super) fn handle_unsubscribe_camera(&mut self, client: ClientId, target: ClientId) {
        self.cameras
            .unsubscribe(&mut self.driver, self.now, client, target);
    }

    /// `RequestCameraKeyframe`: honoured from a subscriber, through the
    /// one gate per camera.
    pub(super) fn handle_camera_keyframe(&mut self, client: ClientId, target: ClientId) {
        if self.cameras.is_subscribed(client, target)
            && self.cameras.allow_keyframe_request(self.now, target)
        {
            self.send_msg(
                target,
                &ServerMessage::CameraKeyframeRequested { viewer_id: client },
            );
        }
    }

    /// A session moved from `old` to `new`: its camera goes off (its old
    /// room is told), it stops watching the old room's cameras, and it
    /// is shown the new room's.
    pub(super) fn cameras_after_move(&mut self, mover: ClientId, old: Option<u16>, new: u16) {
        if old == Some(new) {
            return;
        }
        if self.cameras.is_sharing(mover) {
            let old_room: Vec<ClientId> = old
                .and_then(|o| self.room_members.get(&o))
                .map(|m| {
                    m.iter()
                        .copied()
                        .filter(|p| *p != mover && self.has_feature(*p, features::CAMERA))
                        .collect()
                })
                .unwrap_or_default();
            // Whoever followed into the new room was told it started.
            let mut told = old_room;
            told.extend(self.camera_audience(mover));
            told.sort_unstable();
            told.dedup();
            self.stop_camera(mover, &told);
        }
        let watched: Vec<ClientId> = self
            .cameras
            .sharers()
            .keys()
            .copied()
            .filter(|s| self.cameras.is_subscribed(mover, *s))
            .collect();
        for sharer in watched {
            if self.channel_of.get(&sharer) != Some(&new) {
                self.cameras
                    .unsubscribe(&mut self.driver, self.now, mover, sharer);
                self.send_msg(mover, &ServerMessage::CameraStopped { client_id: sharer });
            }
        }
        if !self.has_feature(mover, features::CAMERA) || new == self.default_channel {
            return;
        }
        let here: Vec<(ClientId, ShareInfo)> = self
            .cameras
            .sharers()
            .iter()
            .filter(|(s, _)| **s != mover && self.channel_of.get(s) == Some(&new))
            .map(|(s, i)| (*s, *i))
            .collect();
        for (sharer, info) in here {
            self.send_msg(
                mover,
                &ServerMessage::CameraStarted {
                    client_id: sharer,
                    width: info.width,
                    height: info.height,
                },
            );
        }
    }

    /// A session ended: its camera with it.
    pub(super) fn cameras_after_disconnect(&mut self, client: ClientId, room: &[ClientId]) {
        let was_on = self.cameras.is_sharing(client);
        self.cameras
            .handle_disconnect(&mut self.driver, self.now, client);
        if was_on {
            let audience: Vec<ClientId> = room
                .iter()
                .copied()
                .filter(|p| *p != client && self.has_feature(*p, features::CAMERA))
                .collect();
            self.send_to(
                &audience,
                &ServerMessage::CameraStopped { client_id: client },
            );
        }
    }

    /// Once a second: each camera's viewers, for its sender's ladder.
    pub(super) fn send_camera_health(&mut self) {
        for report in self.cameras.stream_health(&self.driver) {
            self.send_msg(
                report.sharer,
                &ServerMessage::CameraHealth {
                    worst_loss: report.worst_loss,
                    worst_rtt_ms: report.worst_rtt_ms,
                    viewers: report.viewers,
                },
            );
        }
    }
}
