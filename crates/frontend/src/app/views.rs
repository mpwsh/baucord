//! The rail and Channel view (1.5).
//!
//! A rail down the left of the window at every width: the channel list
//! or Channel view (in a call; joining one opens it) as the main
//! column, the chat beside it or not — one of the two on a narrow
//! window — or Messages (an inbox and the conversation picked in it).
//!
//! Channel view is a grid of the people in our channel — a tile each,
//! their picture or their camera — with the channel's name, timer and
//! recording state above it. Its tiles are built from the roster's own
//! rows, so the look, the speaking edge and the menu are the roster's.
//!
//! Cameras: this build receives them. A camera on in our channel is
//! watched only while Channel view is on screen; its frames come
//! through the present-prep thread under the stream id
//! `sender | CAMERA_TRACK_BIT` and land on the sender's tile.
//!
//! A child of `app`: everything private to `AppState` is in reach
//! through `super::*`, so these are the same methods, only filed.

use client::protocol::recording::CAMERA_TRACK_BIT;
use ui::StageTile;

#[allow(clippy::wildcard_imports)]
use super::*;

/// The rail's buttons, as `Logic.view-picked` numbers them.
const VIEW_LIST: i32 = 0;
const VIEW_STAGE: i32 = 1;
const VIEW_CHAT: i32 = 2;
const VIEW_MESSAGES: i32 = 3;

/// `State.view-main`: the main column.
const MAIN_NONE: i32 = 0;
const MAIN_LIST: i32 = 1;
const MAIN_STAGE: i32 = 2;

/// Channel view's per-session state.
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent facts, not a mode
pub(crate) struct Stage {
    /// Cameras on in our channel, by sender: the size they announced.
    pub cameras: HashMap<u64, (u16, u16)>,
    /// The cameras we asked the server for.
    pub watching: HashSet<u64>,
    /// Cameras whose first picture arrived since we started watching.
    pub pictured: HashSet<u64>,
    /// Channel view is on screen (`Logic.stage-shown`).
    pub shown: bool,
    /// Each tile's place in the model, by client id.
    pub tiles: HashMap<u64, usize>,
    /// The server relays cameras (`features::CAMERA`).
    pub server_cameras: bool,
    /// Our camera thread is running (asked to start, not yet stopped).
    pub sending: bool,
    /// The server was told our camera is on (`CameraStart` sent).
    pub announced: bool,
    /// Our tile shows the self-view.
    pub self_view: bool,
}

fn stream_of(client_id: u64) -> u64 {
    client_id | CAMERA_TRACK_BIT
}

impl AppState {
    // ── The places: Channel, Chat, Messages ─────────────────────────

    fn narrow(&self) -> bool {
        self.main.get_narrow()
    }

    /// In a voice room, so Channel view has a call to show.
    fn in_voice(&self) -> bool {
        self.our_channel().is_some() && !self.in_lobby()
    }

    /// The main column that shows (Channel view only in a call), and
    /// whether the chat does — the chat when there is no main column,
    /// and not beside one on a narrow window (main.slint draws the
    /// same).
    fn columns(&self) -> (i32, bool) {
        let s = self.main.global::<State>();
        let main = match s.get_view_main() {
            MAIN_STAGE if !self.in_voice() => MAIN_LIST,
            m => m,
        };
        let chat = main == MAIN_NONE || (s.get_view_chat() && !self.narrow());
        (main, chat)
    }

    /// A press on the rail. The channel list and Channel view (in a
    /// call) are one main column, either one: pressing the other swaps
    /// it, pressing the one showing hides it while the chat is there to
    /// take the room. Chat shows or hides the chat — on a narrow window,
    /// where one column shows, it swaps with the main one. From
    /// Messages, any of them goes back, to the one pressed; Messages
    /// turns the window to the inbox, and a second press back.
    pub(super) fn view_picked(&mut self, view: i32) {
        if view == VIEW_STAGE && !self.in_voice() {
            return;
        }
        if self.main.global::<State>().get_view_messages() {
            self.leave_messages();
            let s = self.main.global::<State>();
            match view {
                VIEW_LIST => s.set_view_main(MAIN_LIST),
                VIEW_STAGE => s.set_view_main(MAIN_STAGE),
                VIEW_CHAT if self.narrow() => {
                    self.last_main = s.get_view_main().max(MAIN_LIST);
                    s.set_view_main(MAIN_NONE);
                    s.set_view_chat(true);
                }
                VIEW_CHAT => s.set_view_chat(true),
                _ => {}
            }
            self.save_view_prefs();
            return;
        }
        let (main, chat) = self.columns();
        let narrow = self.narrow();
        let s = self.main.global::<State>();
        let pick = |m: i32| {
            if view == VIEW_LIST {
                m == MAIN_LIST
            } else {
                m == MAIN_STAGE
            }
        };
        match view {
            // The one showing: hidden, if the chat takes the room (else
            // the arms below set it again, which changes nothing).
            VIEW_LIST | VIEW_STAGE if pick(main) && !narrow && s.get_view_chat() => {
                s.set_view_main(MAIN_NONE);
            }
            VIEW_LIST => s.set_view_main(MAIN_LIST),
            VIEW_STAGE => s.set_view_main(MAIN_STAGE),
            VIEW_CHAT if narrow && chat => {
                s.set_view_main(self.last_main.max(MAIN_LIST));
            }
            VIEW_CHAT if narrow => {
                self.last_main = main;
                s.set_view_main(MAIN_NONE);
                s.set_view_chat(true);
            }
            VIEW_CHAT if chat && main != MAIN_NONE => s.set_view_chat(false),
            VIEW_CHAT if !chat => s.set_view_chat(true),
            VIEW_MESSAGES => self.enter_messages(),
            _ => {}
        }
        self.save_view_prefs();
    }

    /// Channel view's back arrow: the channel list instead of it.
    pub(super) fn stage_back(&mut self) {
        self.main.global::<State>().set_view_main(MAIN_LIST);
        self.save_view_prefs();
    }

    /// The chat's back arrow: the chat goes, and the main column comes
    /// back where there was none (narrow: the one it replaced).
    pub(super) fn chat_back(&mut self) {
        let (main, _) = self.columns();
        let s = self.main.global::<State>();
        if !self.narrow() {
            s.set_view_chat(false);
        }
        if main == MAIN_NONE {
            s.set_view_main(self.last_main.max(MAIN_LIST));
        }
        self.save_view_prefs();
    }

    /// Into Messages, with the conversation last open there (wide; a
    /// narrow window opens on the inbox).
    fn enter_messages(&mut self) {
        let s = self.main.global::<State>();
        s.set_view_messages(true);
        s.set_narrow_chat_open(false);
        if self.narrow() {
            return;
        }
        let last = self
            .session
            .chat
            .last_dm
            .filter(|u| self.session.chat.dm_order.contains(u));
        if let Some(user) = last {
            self.switch_conversation(Some(user));
        }
    }

    /// Out of Messages; the chat there is the server's.
    fn leave_messages(&mut self) {
        let s = self.main.global::<State>();
        s.set_view_messages(false);
        s.set_narrow_chat_open(false);
        if self.session.chat.active_dm.is_some() {
            self.session.chat.last_dm = self.session.chat.active_dm;
        }
        self.switch_conversation(None);
    }

    /// A conversation picked in the inbox: it opens beside it (wide) or
    /// slides in (narrow).
    pub(super) fn dm_opened(&mut self, user: client::UserId) {
        self.switch_conversation(Some(user));
        if self.narrow() {
            self.main.global::<State>().set_narrow_chat_open(true);
        }
    }

    /// A DM came on screen, however it was opened (the inbox, a menu's
    /// Message, `/dm`, a notice): the window goes to Messages.
    pub(super) fn panel_follows(&mut self, key: Option<client::UserId>) {
        let Some(user) = key else {
            return;
        };
        self.session.chat.last_dm = Some(user);
        self.main.global::<State>().set_view_messages(true);
    }

    /// We joined a voice channel ourselves: Channel view shows the call
    /// (its back arrow returns to the list).
    pub(super) fn joined_voice(&mut self) {
        self.main.global::<State>().set_view_main(MAIN_STAGE);
    }

    /// Whether the person can see `from`'s conversation, or the inbox,
    /// right now (the window aside): what a DM from them does not chime
    /// for.
    pub(super) fn messages_on_screen(&self, from: client::UserId) -> bool {
        // The inbox is on screen with any conversation in Messages,
        // except on a narrow window where another one slid over it;
        // that one is `from`'s or it chimes.
        let s = self.main.global::<State>();
        s.get_view_messages()
            && (!self.narrow()
                || !s.get_narrow_chat_open()
                || self.session.chat.active_dm == Some(from))
    }

    /// The panel's edge was let go beside Channel view: keep the width.
    pub(super) fn panel_resized(&mut self, width: f32) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let width = width.clamp(300.0, 1200.0).round() as u16;
        if width != self.settings.ui.panel_width {
            self.settings.ui.panel_width = width;
            self.save_settings();
        }
    }

    /// The kept layout, from the settings.
    pub(super) fn push_view_prefs(&self) {
        let s = self.main.global::<State>();
        let ui = &self.settings.ui;
        s.set_view_main(if ui.view_list { MAIN_LIST } else { MAIN_NONE });
        s.set_view_chat(ui.view_chat);
        s.set_panel_width(f32::from(ui.panel_width.clamp(300, 1200)));
    }

    fn save_view_prefs(&mut self) {
        let s = self.main.global::<State>();
        // A main column (the list, or Channel view in a call) or none.
        let (list, chat) = (s.get_view_main() != MAIN_NONE, s.get_view_chat());
        let ui = &mut self.settings.ui;
        if (ui.view_list, ui.view_chat) != (list, chat) {
            ui.view_list = list;
            ui.view_chat = chat;
            self.save_settings();
        }
    }

    // ── Channel view ────────────────────────────────────────────────

    /// The tiles, from the roster's rows of our channel: everyone else
    /// in roster order, then us. Called with every roster rebuild.
    pub(super) fn rebuild_stage(&mut self) {
        let ours = self.our_channel().filter(|_| !self.in_lobby());
        let mut tiles: Vec<StageTile> = Vec::new();
        let mut us: Option<StageTile> = None;
        if let Some(channel) = ours {
            for i in 0..self.users_model.row_count() {
                let Some(row) = self.users_model.row_data(i) else {
                    continue;
                };
                if row.is_channel || u16::try_from(row.channel_id).ok() != Some(channel) {
                    continue;
                }
                let tile = self.tile_of(&row, i);
                if row.is_us {
                    us = Some(tile);
                } else {
                    tiles.push(tile);
                }
            }
        }
        tiles.extend(us);
        self.session.stage.tiles = tiles
            .iter()
            .enumerate()
            .map(|(i, t)| (roster::join_id(t.client_id_lo, t.client_id_hi), i))
            .collect();
        if tiles.len() == self.stage_model.row_count() {
            for (i, tile) in tiles.into_iter().enumerate() {
                if self.stage_model.row_data(i).as_ref() != Some(&tile) {
                    self.stage_model.set_row_data(i, tile);
                }
            }
        } else {
            self.stage_model.set_vec(tiles);
        }
        self.refresh_stage_header();
    }

    fn tile_of(&self, row: &UserRow, index: usize) -> StageTile {
        let id = roster::join_id(row.client_id_lo, row.client_id_hi);
        let pictured = self.session.stage.pictured.contains(&id)
            || (row.is_us && self.session.stage.self_view);
        // A tile that already shows a camera keeps its last picture;
        // the next frame replaces it.
        let frame = if pictured {
            self.session
                .stage
                .tiles
                .get(&id)
                .and_then(|i| self.stage_model.row_data(*i))
                .map(|t| t.frame)
                .unwrap_or_default()
        } else {
            slint::Image::default()
        };
        StageTile {
            name: row.name.clone(),
            index: i32::try_from(index).unwrap_or(-1),
            client_id_lo: row.client_id_lo,
            client_id_hi: row.client_id_hi,
            avatar: row.avatar.clone(),
            has_avatar: row.has_avatar,
            initial: row.initial.clone(),
            tile_tint: row.tile_tint,
            name_color: row.name_color,
            has_name_color: row.has_name_color,
            speaking: row.speaking,
            self_muted: row.self_muted,
            self_deafened: row.self_deafened,
            sharing: row.sharing,
            is_us: row.is_us,
            camera: pictured,
            frame,
            role: row.role,
            role_icon: row.role_icon.clone(),
            role_color: row.role_color,
            has_role_color: row.has_role_color,
            level_short: self
                .session
                .users
                .get(&id)
                .filter(|u| u.level > 0)
                .map(|u| t!("tile-level", "level" => u32::from(u.level)))
                .unwrap_or_default()
                .into(),
            xp_pct: row.xp_pct,
        }
    }

    /// The header above the grid: our channel's name, timer and
    /// recording state, as its roster row has them.
    pub(super) fn refresh_stage_header(&self) {
        let s = self.main.global::<State>();
        let Some(channel) = self.our_channel().filter(|_| !self.in_lobby()) else {
            s.set_stage_channel(-1);
            s.set_stage_name("".into());
            s.set_stage_timer("".into());
            s.set_stage_recording(false);
            s.set_stage_can_stop(false);
            s.set_stage_rec_tip("".into());
            return;
        };
        let mut row = UserRow::default();
        self.decorate_channel_row(&mut row, channel);
        s.set_stage_channel(i32::from(channel));
        s.set_stage_name(self.channel_name(channel).into());
        s.set_stage_timer(row.timer_label);
        s.set_stage_recording(row.recording);
        s.set_stage_can_stop(row.can_stop_recording);
        s.set_stage_rec_tip(row.rec_tip);
    }

    /// A speaking or audio-state edge, on its tile alone.
    pub(super) fn refresh_stage_tile(
        &self,
        client_id: u64,
        speaking: bool,
        muted: bool,
        deafened: bool,
    ) {
        let Some(&i) = self.session.stage.tiles.get(&client_id) else {
            return;
        };
        let Some(mut tile) = self.stage_model.row_data(i) else {
            return;
        };
        if (tile.speaking, tile.self_muted, tile.self_deafened) == (speaking, muted, deafened) {
            return;
        }
        tile.speaking = speaking;
        tile.self_muted = muted;
        tile.self_deafened = deafened;
        self.stage_model.set_row_data(i, tile);
    }

    // ── Cameras ─────────────────────────────────────────────────────

    /// Channel view came on screen or left it: cameras are watched
    /// only while it can be seen.
    pub(super) fn stage_shown(&mut self, shown: bool) {
        self.session.stage.shown = shown;
        self.sync_camera_watch();
    }

    pub(super) fn take_camera_started(&mut self, client_id: u64, width: u16, height: u16) {
        if Some(client_id) == self.session.our_client_id {
            return;
        }
        info!(
            "{} turned their camera on ({width}x{height})",
            self.username_of(client_id)
        );
        self.session
            .stage
            .cameras
            .insert(client_id, (width, height));
        // The decoder is ready before any frame can come.
        self.screen
            .tx
            .send(ScreenControl::PeerStartedSharing {
                client_id: stream_of(client_id),
                width: u32::from(width),
                height: u32::from(height),
            })
            .ok();
        self.sync_camera_watch();
    }

    pub(super) fn take_camera_stopped(&mut self, client_id: u64) {
        // Ours, ended by the server: a move, the lobby, a permission
        // taken away. The thread stops too.
        if Some(client_id) == self.session.our_client_id {
            self.session.stage.announced = false;
            self.stop_own_camera(false);
            return;
        }
        if self.session.stage.cameras.remove(&client_id).is_none() {
            return;
        }
        // The server let go of our subscription itself.
        self.session.stage.watching.remove(&client_id);
        self.drop_camera(client_id);
    }

    pub(super) fn take_camera_denied(
        &mut self,
        target_id: Option<u64>,
        reason: client::protocol::DenyReason,
    ) {
        // Watching one was refused: the tile keeps the picture.
        if let Some(target) = target_id {
            info!("camera of {target:#x} refused: {}", reason.describe());
            if self.session.stage.watching.remove(&target) {
                self.screen
                    .tx
                    .send(ScreenControl::StopWatching {
                        client_id: stream_of(target),
                    })
                    .ok();
            }
        } else {
            info!("our camera refused: {}", reason.describe());
            self.session.stage.announced = false;
            self.stop_own_camera(false);
            self.show_toast(ToastKind::Warning, &t!("camera-denied"));
        }
    }

    /// We moved: the cameras of the room we left are no longer ours
    /// to see (the server already ended those we watched); the new
    /// room's arrive as `CameraStarted`.
    pub(super) fn cameras_after_our_move(&mut self) {
        let ours = self.our_channel();
        let gone: Vec<u64> = self
            .session
            .stage
            .cameras
            .keys()
            .copied()
            .filter(|id| self.session.users.get(id).map(|u| u.channel_id) != ours)
            .collect();
        for id in gone {
            self.session.stage.cameras.remove(&id);
            self.session.stage.watching.remove(&id);
            self.drop_camera(id);
        }
        self.sync_camera_watch();
    }

    /// Watch every camera of our channel while Channel view is seen;
    /// let all of them go when it is not.
    fn sync_camera_watch(&mut self) {
        let wanted: HashSet<u64> = if self.session.stage.shown && !self.in_lobby() {
            let ours = self.our_channel();
            self.session
                .stage
                .cameras
                .keys()
                .copied()
                .filter(|id| {
                    self.session.users.get(id).map(|u| u.channel_id) == ours
                        && !self.client_blocked(*id)
                })
                .collect()
        } else {
            HashSet::new()
        };
        let start: Vec<u64> = wanted
            .difference(&self.session.stage.watching)
            .copied()
            .collect();
        let stop: Vec<u64> = self
            .session
            .stage
            .watching
            .difference(&wanted)
            .copied()
            .collect();
        for id in start {
            self.session.stage.watching.insert(id);
            // The screen thread first, so the frames are wanted before
            // the server sends any.
            self.screen
                .tx
                .send(ScreenControl::Watch {
                    client_id: stream_of(id),
                })
                .ok();
            self.network
                .tx
                .send(NetworkControl::Send(ClientMessage::SubscribeCamera {
                    target_id: id,
                }))
                .ok();
        }
        for id in stop {
            self.session.stage.watching.remove(&id);
            self.session.stage.pictured.remove(&id);
            self.screen
                .tx
                .send(ScreenControl::StopWatching {
                    client_id: stream_of(id),
                })
                .ok();
            self.network
                .tx
                .send(NetworkControl::Send(ClientMessage::UnsubscribeCamera {
                    target_id: id,
                }))
                .ok();
            self.clear_tile_picture(id);
        }
    }

    /// A camera is gone: its decoder, and its tile goes back to the
    /// picture.
    fn drop_camera(&mut self, client_id: u64) {
        self.screen
            .tx
            .send(ScreenControl::PeerStoppedSharing {
                client_id: stream_of(client_id),
            })
            .ok();
        self.session.stage.pictured.remove(&client_id);
        self.clear_tile_picture(client_id);
    }

    fn clear_tile_picture(&self, client_id: u64) {
        let Some(&i) = self.session.stage.tiles.get(&client_id) else {
            return;
        };
        if let Some(mut tile) = self.stage_model.row_data(i)
            && tile.camera
        {
            tile.camera = false;
            tile.frame = slint::Image::default();
            self.stage_model.set_row_data(i, tile);
        }
    }

    /// The session ends: every camera the screen thread was told of
    /// goes, as viewers do.
    pub(super) fn forget_cameras(&mut self) {
        if self.session.stage.sending {
            self.camera
                .tx
                .send(client::camera::CameraControl::Stop)
                .ok();
            self.main.global::<State>().set_camera_on(false);
        }
        let ids: Vec<u64> = self.session.stage.cameras.keys().copied().collect();
        for id in ids {
            if self.session.stage.watching.contains(&id) {
                self.screen
                    .tx
                    .send(ScreenControl::StopWatching {
                        client_id: stream_of(id),
                    })
                    .ok();
            }
            self.screen
                .tx
                .send(ScreenControl::PeerStoppedSharing {
                    client_id: stream_of(id),
                })
                .ok();
        }
        self.session.stage = Stage::default();
    }

    /// One prepared camera frame: onto its sender's tile.
    pub(super) fn camera_frame(&mut self, f: crate::present::PreparedFrame) {
        let sender = f.meta.client_id & !CAMERA_TRACK_BIT;
        if !self.session.stage.watching.contains(&sender) {
            return;
        }
        self.session.stage.pictured.insert(sender);
        let Some(&i) = self.session.stage.tiles.get(&sender) else {
            return;
        };
        if let Some(mut tile) = self.stage_model.row_data(i) {
            tile.camera = true;
            tile.frame = slint::Image::from_rgba8(f.buffer);
            self.stage_model.set_row_data(i, tile);
        }
    }
}

// ── Our camera (1.5) ────────────────────────────────────────────────────

impl AppState {
    /// Whether the camera button is offered: a desktop build, a server
    /// that relays cameras, and the `camera` permission. The phones get
    /// it with their capture backends.
    pub(super) fn sync_camera_button(&self) {
        let can = cfg!(not(any(target_os = "android", target_os = "ios")))
            && self.session.stage.server_cameras
            && client::permissions::has(self.session.permissions, client::permissions::CAMERA);
        self.main.global::<State>().set_camera_available(can);
    }

    /// The camera button: on, or off.
    pub(super) fn toggle_camera(&mut self) {
        if self.session.stage.sending {
            self.stop_own_camera(true);
            return;
        }
        if self.in_lobby() || self.our_channel().is_none() {
            self.show_toast(ToastKind::Info, &t!("camera-join-first"));
            return;
        }
        self.session.stage.sending = true;
        self.start_camera_thread(true);
        // On at once, so a second press stops it; the server hears
        // when the first frame is encoded (`CameraEvent::Started`).
        self.main.global::<State>().set_camera_on(true);
    }

    /// Open the camera Settings → Camera names: sending, or for the
    /// preview alone.
    pub(crate) fn start_camera_thread(&self, send: bool) {
        let cam = &self.settings.camera;
        self.camera
            .tx
            .send(client::camera::CameraControl::Start {
                device: cam.device.clone(),
                encoder: video::EncoderPreference::from_key(&self.settings.share.encoder)
                    .unwrap_or_default(),
                send,
                mirror: cam.mirror,
                quality: self.camera_quality(),
            })
            .ok();
    }

    /// The camera size Settings → Camera asks for.
    pub(crate) fn camera_quality(&self) -> video::Quality {
        video::Quality::from_key(&self.settings.camera.quality)
            .filter(|q| video::Quality::CAMERA.contains(q))
            .unwrap_or(video::Quality::CAMERA_DEFAULT)
    }

    /// Whether our camera is on in a call.
    pub(crate) fn session_camera_sending(&self) -> bool {
        self.session.stage.sending
    }

    /// Ask what a camera can deliver (`CameraEvent::Heights`).
    pub(crate) fn probe_camera(&self, device: Option<String>) {
        self.camera
            .tx
            .send(client::camera::CameraControl::Probe { device })
            .ok();
    }

    /// Settings → Camera's preview off, with no call sending.
    pub(crate) fn stop_camera_preview_thread(&self) {
        self.camera
            .tx
            .send(client::camera::CameraControl::Stop)
            .ok();
    }

    /// Settings → Camera's mirror or data saver changed: the open
    /// camera follows at once.
    pub(crate) fn camera_options_changed(&mut self) {
        self.camera
            .tx
            .send(client::camera::CameraControl::Options {
                mirror: self.settings.camera.mirror,
                quality: self.camera_quality(),
            })
            .ok();
    }

    /// Our camera off: the thread, the room (if it was told), the tile.
    pub(super) fn stop_own_camera(&mut self, tell_server: bool) {
        if self.session.stage.sending {
            self.camera
                .tx
                .send(client::camera::CameraControl::Stop)
                .ok();
            crate::settingsui::camera_preview_ended(self);
        }
        if self.session.stage.announced {
            if tell_server {
                self.network.tx.send(NetworkControl::CameraStop).ok();
            }
            self.network
                .tx
                .send(NetworkControl::TelemetryEvent(
                    client::protocol::TelemetryEvent::CameraStopped,
                ))
                .ok();
        }
        self.session.stage.sending = false;
        self.session.stage.announced = false;
        self.session.stage.self_view = false;
        self.main.global::<State>().set_camera_on(false);
        if let Some(us) = self.session.our_client_id {
            self.clear_tile_picture(us);
        }
    }

    /// What the camera thread said.
    pub(super) fn handle_camera_event(&mut self, evt: client::camera::CameraEvent) {
        use client::camera::{CameraEvent, CameraFailure};
        match evt {
            CameraEvent::Started {
                width,
                height,
                camera,
                mode,
                encoder,
            } => {
                if !self.session.stage.sending {
                    return;
                }
                self.session.stage.announced = true;
                self.network
                    .tx
                    .send(NetworkControl::CameraStart { width, height })
                    .ok();
                // The camera's model for the server's telemetry (when
                // it is allowed, and the server reads it).
                self.network
                    .tx
                    .send(NetworkControl::TelemetryEvent(
                        client::protocol::TelemetryEvent::CameraStarted {
                            camera,
                            mode,
                            encoder,
                        },
                    ))
                    .ok();
            }
            CameraEvent::Stopped => {}
            CameraEvent::Heights(heights) => {
                crate::settingsui::camera_heights(self, &heights);
            }
            CameraEvent::Error(failure) => {
                let was_on =
                    self.session.stage.sending || crate::settingsui::camera_previewing(self);
                self.stop_own_camera(true);
                crate::settingsui::camera_preview_ended(self);
                if was_on {
                    let key = match failure {
                        CameraFailure::NoCamera => "camera-none",
                        CameraFailure::NotAllowed => "camera-not-allowed",
                        CameraFailure::Unsupported | CameraFailure::NoEncoder => {
                            "camera-unsupported"
                        }
                        CameraFailure::Other(e) => {
                            warn!("camera failed: {e}");
                            "camera-failed"
                        }
                    };
                    self.show_toast(ToastKind::Warning, &t!(key));
                }
            }
        }
    }

    /// A viewer of ours asked for a keyframe; the relay's health line.
    pub(super) fn camera_keyframe_requested(&self) {
        self.camera
            .tx
            .send(client::camera::CameraControl::ForceKeyframe)
            .ok();
    }

    pub(super) fn camera_health(&self, worst_loss: f32, viewers: u16) {
        self.camera
            .tx
            .send(client::camera::CameraControl::Health {
                worst_loss,
                viewers,
            })
            .ok();
    }

    /// The self-view's newest picture onto our tile, if one came.
    pub(super) fn take_camera_preview(&mut self) {
        let Some((pixels, w, h)) = self
            .camera
            .shared
            .preview
            .lock()
            .ok()
            .and_then(|mut p| p.take())
        else {
            return;
        };
        crate::settingsui::camera_preview(self, &pixels, w, h);
        if !self.session.stage.sending {
            return;
        }
        self.session.stage.self_view = true;
        let Some(us) = self.session.our_client_id else {
            return;
        };
        let Some(&i) = self.session.stage.tiles.get(&us) else {
            return;
        };
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
        let bytes = buffer.make_mut_bytes();
        if bytes.len() != pixels.len() {
            return;
        }
        bytes.copy_from_slice(&pixels);
        if let Some(mut tile) = self.stage_model.row_data(i) {
            tile.camera = true;
            tile.frame = slint::Image::from_rgba8(buffer);
            self.stage_model.set_row_data(i, tile);
        }
    }
}
