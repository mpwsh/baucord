// ui/sidebar.rs
//
// The user list updates incrementally: a full rebuild only happens when
// users join or leave. Speaking indicators, mute state, volume badges,
// and "Live" badges are updated in-place on existing widgets, preserving
// scroll position.

use fltk::{enums::Font, image::SvgImage, prelude::*, *};
use renet::ClientId;
use std::collections::{HashMap, HashSet};

use crate::app::UserState;
use crate::ui::messages::UiMessage;
use ui::theme;
use ui::widgets::controls;

// ── Embedded SVG icons for remote user mute/deafen state ────────────────

const SVG_SELF_MUTED: &str = include_str!("../../../../assets/icons/mic-muted.svg");
const SVG_SELF_DEAFENED: &str = include_str!("../../../../assets/icons/headphones-off.svg");

/// Create a scaled SVG image from embedded data.
fn svg_icon(data: &str, size: i32) -> Option<SvgImage> {
    SvgImage::from_data(data).ok().map(|mut img| {
        img.scale(size, size, true, true);
        img
    })
}

/// Handles for a single user row — kept around to update in-place.
struct UserRow {
    client_id: ClientId,
    container: group::Flex,
    indicator: frame::Frame,
    name_label: frame::Frame,
    /// Icon frame between name and action button — shows muted/deafened SVG.
    status_icon: Option<frame::Frame>,
    /// The `…` / `Live` button. For self rows this only shows "Live"
    /// (no "…" menu). For other users it shows "…" on hover or "Live".
    action_btn: Option<button::Button>,
    is_us: bool,
}

pub struct Sidebar {
    sender: app::Sender<UiMessage>,

    // UI elements
    group: group::Flex,
    user_count_label: frame::Frame,
    user_list_scroll: group::Scroll,
    user_list_pack: group::Pack,
    stream_stats_label: frame::Frame,
    connection_status: frame::Frame,
    latency_label: frame::Frame,
    mute_btn: button::Button,
    deafen_btn: button::Button,
    share_btn: button::Button,

    /// Live user row widgets, keyed by client ID for in-place updates.
    user_rows: Vec<UserRow>,

    /// Set of client IDs currently sharing — passed in from App on each update.
    active_sharers: HashSet<ClientId>,

    /// Currently open per-user audio popup (if any).
    audio_popup: Option<window::Window>,

    /// Local mute/deafen state — used to show icons on our own user row.
    /// Updated by set_mute_state() / set_deafen_state().
    local_muted: bool,
    local_deafened: bool,
}

impl Sidebar {
    pub fn new(sender: app::Sender<UiMessage>) -> Self {
        let mut sidebar = group::Flex::default().column();
        sidebar.set_color(theme::BG_SECONDARY);
        sidebar.set_frame(enums::FrameType::FlatBox);
        sidebar.set_margins(10, 10, 10, 10);
        sidebar.set_spacing(10);

        // User count label
        let mut user_count_label = frame::Frame::default()
            .with_label("Users: 0")
            .with_align(enums::Align::Left | enums::Align::Inside);
        sidebar.fixed(&user_count_label, 25);
        user_count_label.set_label_color(theme::TEXT_SECONDARY);
        user_count_label.set_label_size(13);

        // User list scroll area — fills the space between header and status panel
        let mut user_list_scroll = group::Scroll::default();
        user_list_scroll.set_type(group::ScrollType::None);
        user_list_scroll.set_color(theme::BG_SECONDARY);
        user_list_scroll.set_frame(enums::FrameType::FlatBox);

        let mut user_list_pack = group::Pack::default();
        user_list_pack.set_type(group::PackType::Vertical);
        user_list_pack.set_spacing(5);
        user_list_pack.end();

        user_list_scroll.end();
        user_list_scroll.resizable(&user_list_pack);

        // ── Stream stats line (hidden when not streaming) ─────────────
        let mut stream_stats_label = frame::Frame::default()
            .with_label("")
            .with_align(enums::Align::Left | enums::Align::Inside);
        sidebar.fixed(&stream_stats_label, 0); // 0 height = hidden
        stream_stats_label.set_label_color(theme::TEXT_MUTED);
        stream_stats_label.set_label_size(10);
        stream_stats_label.hide();

        // ── Connection status panel ─────────────────────────────────
        let mut status_col = group::Flex::default().column();
        status_col.set_spacing(2);
        sidebar.fixed(&status_col, 50);

        let mut connection_status = frame::Frame::default().with_label("\u{25cf} Connected");
        status_col.fixed(&connection_status, 20);
        connection_status.set_label_color(theme::SUCCESS);
        connection_status.set_label_size(12);
        connection_status.set_align(enums::Align::Left | enums::Align::Inside);

        let mut latency_label =
            frame::Frame::default().with_label("  \u{1f50a} --ms  \u{1f310} --ms");
        status_col.fixed(&latency_label, 16);
        latency_label.set_label_color(theme::TEXT_MUTED);
        latency_label.set_label_size(10);
        latency_label.set_align(enums::Align::Left | enums::Align::Inside);

        status_col.end();

        // Mute / Deafen row
        let mut audio_row = group::Flex::default().row();
        audio_row.set_spacing(5);
        sidebar.fixed(&audio_row, 35);

        let mute_btn = controls::button("Mute");
        let deafen_btn = controls::button("Deafen");

        audio_row.end();

        // Share Screen button — full width, toggles to "Stop Sharing"
        let mut share_btn = controls::button("Share Screen");
        sidebar.fixed(&share_btn, 35);

        // Settings button
        let mut settings_btn = controls::button("Settings");
        sidebar.fixed(&settings_btn, 35);

        // Disconnect button
        let mut disconnect_btn = controls::accent_button("Disconnect", theme::DANGER);
        sidebar.fixed(&disconnect_btn, 35);

        sidebar.end();

        // ── Callbacks ───────────────────────────────────────────────

        let s = sender;
        mute_btn.clone().set_callback(move |_| {
            s.send(UiMessage::ToggleMute);
        });

        let s = sender;
        deafen_btn.clone().set_callback(move |_| {
            s.send(UiMessage::ToggleDeafen);
        });

        let s = sender;
        share_btn.clone().set_callback(move |_| {
            s.send(UiMessage::StartShareScreen);
        });

        let s = sender;
        settings_btn.set_callback(move |_| {
            s.send(UiMessage::OpenSettings);
        });

        let s = sender;
        disconnect_btn.set_callback(move |_| {
            s.send(UiMessage::Disconnect);
        });

        Self {
            sender,
            group: sidebar,
            user_count_label,
            user_list_scroll,
            user_list_pack,
            stream_stats_label,
            connection_status,
            latency_label,
            mute_btn,
            deafen_btn,
            share_btn,
            user_rows: Vec::new(),
            active_sharers: HashSet::new(),
            audio_popup: None,
            local_muted: false,
            local_deafened: false,
        }
    }

    pub fn create_ui(&mut self) -> group::Flex {
        self.group.clone()
    }

    /// Update the set of active sharers. Called from App whenever
    /// ScreenShareStarted/Stopped arrives.
    pub fn set_active_sharers(&mut self, sharers: &HashMap<ClientId, (u32, u32)>) {
        self.active_sharers = sharers.keys().copied().collect();
    }

    /// Update the share button to reflect current sharing state.
    pub fn set_share_state(&mut self, is_sharing: bool) {
        if is_sharing {
            self.share_btn.set_label("Stop Sharing");
            self.share_btn.set_color(theme::DANGER);
            self.share_btn.set_label_color(enums::Color::White);
        } else {
            self.share_btn.set_label("Share Screen");
            self.share_btn.set_color(theme::BG_TERTIARY);
            self.share_btn.set_label_color(theme::TEXT_PRIMARY);
        }
    }

    /// Update the stream stats line shown above the connection status.
    ///
    /// Call this on a timer (e.g. every second) while we're sharing.
    /// Pass `None` to hide the stats (when sharing stops).
    ///
    /// `resolution` — e.g. "720p30"
    /// `avg_frame_kb` — average encoded frame size in KB
    /// `encode_ms` — average encode time in milliseconds
    pub fn update_stream_stats(&mut self, stats: Option<(&str, f32, f32)>) {
        match stats {
            Some((resolution, avg_kb, encode_ms)) => {
                let label = format!(
                    "  Stream: {resolution} | {avg_kb:.0}KB | {encode_ms:.1}ms enc"
                );
                self.stream_stats_label.set_label(&label);
                if !self.stream_stats_label.visible() {
                    self.stream_stats_label.show();
                    self.group.fixed(&self.stream_stats_label, 16);
                    self.group.layout();
                }
            }
            None => {
                if self.stream_stats_label.visible() {
                    self.stream_stats_label.hide();
                    self.group.fixed(&self.stream_stats_label, 0);
                    self.group.layout();
                }
            }
        }
    }

    fn close_audio_popup(&mut self) {
        if let Some(mut w) = self.audio_popup.take() {
            w.hide();
        }
    }

    fn open_audio_popup(
        &mut self,
        client_id: ClientId,
        username: &str,
        current_volume: f32,
        is_muted: bool,
        is_sharing: bool,
        click_x: i32,
        click_y: i32,
    ) {
        self.close_audio_popup();

        let sender = self.sender;
        let popup_w = 200;
        let popup_h = if is_sharing { 165 } else { 130 };

        let x = click_x;
        let y = click_y.saturating_sub(popup_h);

        let mut wind = window::Window::default()
            .with_size(popup_w, popup_h)
            .with_pos(x, y);
        wind.set_border(false);
        wind.set_color(theme::BG_PRIMARY);
        wind.set_frame(enums::FrameType::FlatBox);

        let mut col = group::Flex::default_fill().column();
        col.set_margins(12, 8, 12, 8);
        col.set_spacing(4);

        if is_sharing {
            let mut watch_btn = controls::accent_button("Watch Stream", theme::ACCENT);
            col.fixed(&watch_btn, 26);

            let mut wind_watch = wind.clone();
            watch_btn.set_callback(move |_| {
                sender.send(UiMessage::WatchPeer { client_id });
                wind_watch.hide();
            });

            let mut sep_watch = frame::Frame::default();
            sep_watch.set_frame(enums::FrameType::FlatBox);
            sep_watch.set_color(theme::BG_TERTIARY);
            col.fixed(&sep_watch, 1);
        }

        let mut vol_header = frame::Frame::default()
            .with_label("User Volume")
            .with_align(enums::Align::Left | enums::Align::Inside);
        vol_header.set_label_color(theme::TEXT_PRIMARY);
        vol_header.set_label_size(12);
        vol_header.set_label_font(enums::Font::HelveticaBold);
        col.fixed(&vol_header, 18);

        let mut slider_row = group::Flex::default().row();
        slider_row.set_spacing(6);
        col.fixed(&slider_row, 22);

        let mut slider = valuator::HorSlider::default();
        slider.set_minimum(0.0);
        slider.set_maximum(2.0);
        slider.set_value(current_volume as f64);
        slider.set_step(0.05, 1);
        slider.set_selection_color(theme::ACCENT);

        let mut pct_label =
            frame::Frame::default().with_label(&format!("{}%", (current_volume * 100.0) as i32));
        pct_label.set_label_color(theme::TEXT_SECONDARY);
        pct_label.set_label_size(11);
        slider_row.fixed(&pct_label, 36);

        slider_row.end();

        let mut sep = frame::Frame::default();
        sep.set_frame(enums::FrameType::FlatBox);
        sep.set_color(theme::BG_TERTIARY);
        col.fixed(&sep, 1);

        let mute_label = if is_muted { "Unmute" } else { "Mute" };
        let mut mute_btn = if is_muted {
            controls::accent_button(mute_label, theme::DANGER)
        } else {
            controls::button(mute_label)
        };
        col.fixed(&mute_btn, 26);

        let mut sep2 = frame::Frame::default();
        sep2.set_frame(enums::FrameType::FlatBox);
        sep2.set_color(theme::BG_TERTIARY);
        col.fixed(&sep2, 1);

        let mut reset_btn = controls::button("Reset Audio");
        col.fixed(&reset_btn, 26);

        col.end();
        wind.end();
        wind.show();

        // ── Callbacks ───────────────────────────────────────────────

        let mut pct_lbl = pct_label.clone();
        slider.set_callback(move |s| {
            let vol = s.value() as f32;
            pct_lbl.set_label(&format!("{}%", (vol * 100.0) as i32));
        });

        let username_slide = username.to_string();
        let mut wind_slide = wind.clone();
        let slider_release = slider.clone();
        slider.handle(move |_, ev| {
            if ev == enums::Event::Released {
                let vol = slider_release.value() as f32;
                sender.send(UiMessage::SetUserVolume {
                    client_id,
                    username: username_slide.clone(),
                    volume: vol,
                });
                wind_slide.hide();
                true
            } else {
                false
            }
        });

        let username_mute = username.to_string();
        let mut wind_mute = wind.clone();
        mute_btn.set_callback(move |_| {
            sender.send(UiMessage::MuteUser {
                client_id,
                username: username_mute.clone(),
                muted: !is_muted,
            });
            wind_mute.hide();
        });

        let username_reset = username.to_string();
        let mut wind_reset = wind.clone();
        reset_btn.set_callback(move |_| {
            sender.send(UiMessage::ResetUserAudio {
                client_id,
                username: username_reset.clone(),
            });
            wind_reset.hide();
        });

        let username_close = username.to_string();
        let slider_close = slider.clone();
        wind.handle(move |w, ev| {
            if ev == enums::Event::Unfocus {
                let vol = slider_close.value() as f32;
                if (vol - current_volume).abs() > 0.001 {
                    sender.send(UiMessage::SetUserVolume {
                        client_id,
                        username: username_close.clone(),
                        volume: vol,
                    });
                }
                w.hide();
                true
            } else {
                false
            }
        });

        self.audio_popup = Some(wind);
    }

    pub fn handle_open_audio_popup(
        &mut self,
        client_id: ClientId,
        username: &str,
        current_volume: f32,
        is_muted: bool,
        click_x: i32,
        click_y: i32,
    ) {
        let is_sharing = self.active_sharers.contains(&client_id);
        self.open_audio_popup(
            client_id,
            username,
            current_volume,
            is_muted,
            is_sharing,
            click_x,
            click_y,
        );
    }

    // ════════════════════════════════════════════════════════════════════
    //  USER LIST — incremental updates
    // ════════════════════════════════════════════════════════════════════

    pub fn update_users(&mut self, users: &HashMap<ClientId, UserState>, our_id: Option<ClientId>) {
        self.user_count_label
            .set_label(&format!("Users: {}", users.len()));

        let mut current_ids: Vec<ClientId> = users.keys().copied().collect();
        current_ids.sort();
        let mut existing_ids: Vec<ClientId> = self.user_rows.iter().map(|r| r.client_id).collect();
        existing_ids.sort();

        if current_ids != existing_ids {
            self.rebuild_user_list(users, our_id);
        } else {
            self.refresh_user_rows(users);
        }
    }

    /// Full rebuild — only called when users join or leave.
    fn rebuild_user_list(
        &mut self,
        users: &HashMap<ClientId, UserState>,
        our_id: Option<ClientId>,
    ) {
        self.user_list_pack.clear();
        self.user_rows.clear();

        let mut sorted_users: Vec<_> = users.iter().collect();
        sorted_users.sort_by(|(_, state_a), (_, state_b)| state_a.username.cmp(&state_b.username));

        let item_height = 35;
        let total_height = sorted_users.len() as i32 * item_height;

        let scroll_h = self.user_list_scroll.h();
        if total_height > scroll_h {
            self.user_list_scroll.set_type(group::ScrollType::Vertical);
        } else {
            self.user_list_scroll.set_type(group::ScrollType::None);
        }

        let scroll_w = (self.user_list_scroll.w() - 16 - 10).max(100);

        self.user_list_pack.resize(
            self.user_list_scroll.x() + 5,
            self.user_list_scroll.y() + 5,
            scroll_w,
            total_height.max(10),
        );

        for (client_id, state) in &sorted_users {
            let is_us = Some(**client_id) == our_id;
            self.create_user_item(**client_id, state, is_us, scroll_w);
        }

        self.user_list_pack.redraw();
        self.user_list_scroll.redraw();
        self.group.redraw();
    }

    /// In-place update — only changes labels, colors, icons, and Live badges,
    /// preserving layout and scroll.
    fn refresh_user_rows(&mut self, users: &HashMap<ClientId, UserState>) {
        for row in &mut self.user_rows {
            let Some(state) = users.get(&row.client_id) else {
                continue;
            };

            // Speaking indicator
            if state.audio_settings.is_muted {
                row.indicator.set_label("\u{2298}");
                row.indicator.set_label_color(theme::TEXT_MUTED);
            } else if state.is_speaking {
                row.indicator.set_label("\u{25cf}");
                row.indicator.set_label_color(theme::SUCCESS);
            } else {
                row.indicator.set_label("\u{25cb}");
                row.indicator.set_label_color(theme::TEXT_MUTED);
            }

            // Display name (volume badge for non-default volume)
            let display_name = if !row.is_us && (state.audio_settings.volume - 1.0).abs() > 0.01 {
                format!(
                    "{} ({}%)",
                    state.username,
                    (state.audio_settings.volume * 100.0) as i32
                )
            } else {
                state.username.clone()
            };
            row.name_label.set_label(&display_name);

            // Name color
            if row.is_us {
                row.name_label.set_label_color(theme::ACCENT);
            } else if state.audio_settings.is_muted {
                row.name_label.set_label_color(theme::DANGER);
            } else {
                row.name_label.set_label_color(theme::TEXT_PRIMARY);
            }

            // Row background tint
            if state.audio_settings.is_muted {
                row.container.set_color(enums::Color::from_rgb(50, 30, 30));
            } else if (state.audio_settings.volume - 1.0).abs() > 0.01 {
                row.container.set_color(enums::Color::from_rgb(40, 40, 50));
            } else {
                row.container.set_color(theme::BG_SECONDARY);
            }

            // Mute/deafen icon — for self rows use local state (which is
            // always up to date), for remote rows use broadcast state.
            if let Some(icon_frame) = &mut row.status_icon {
                let (show_deafened, show_muted) = if row.is_us {
                    (self.local_deafened, self.local_muted)
                } else {
                    (state.is_self_deafened, state.is_self_muted)
                };

                if show_deafened {
                    if let Some(img) = svg_icon(SVG_SELF_DEAFENED, 14) {
                        icon_frame.set_image(Some(img));
                    }
                } else if show_muted {
                    if let Some(img) = svg_icon(SVG_SELF_MUTED, 14) {
                        icon_frame.set_image(Some(img));
                    }
                } else {
                    icon_frame.set_image(None::<SvgImage>);
                }
                icon_frame.redraw();
            }

            // "Live" badge — applies to both self and non-self rows now
            if let Some(btn) = &mut row.action_btn {
                let is_sharing = self.active_sharers.contains(&row.client_id);
                if is_sharing {
                    btn.set_label("Live");
                    btn.set_label_size(11);
                    btn.set_label_font(Font::HelveticaBold);
                    btn.set_color(theme::DANGER);
                    btn.set_label_color(enums::Color::White);
                } else if row.is_us {
                    // Self row, not sharing — hide the badge entirely
                    btn.set_label("");
                    btn.set_color(theme::BG_SECONDARY);
                    btn.set_label_color(theme::BG_SECONDARY);
                } else {
                    // Remote row, not sharing — revert to hidden "…"
                    btn.set_label("\u{2026}");
                    btn.set_label_size(16);
                    btn.set_label_font(Font::Helvetica);
                    btn.set_color(row.container.color());
                    btn.set_label_color(row.container.color());
                }
                btn.redraw();
            }
        }

        self.user_list_pack.redraw();
    }

    fn create_user_item(
        &mut self,
        client_id: ClientId,
        state: &UserState,
        is_us: bool,
        width: i32,
    ) {
        let item_height = 30;

        let mut user_item = group::Flex::default().with_size(width, item_height);
        user_item.set_type(group::FlexType::Row);
        user_item.set_margins(5, 2, 5, 2);
        user_item.set_frame(enums::FrameType::FlatBox);

        if state.audio_settings.is_muted {
            user_item.set_color(enums::Color::from_rgb(50, 30, 30));
        } else if (state.audio_settings.volume - 1.0).abs() > 0.01 {
            user_item.set_color(enums::Color::from_rgb(40, 40, 50));
        } else {
            user_item.set_color(theme::BG_SECONDARY);
        }

        // Speaking indicator
        let mut indicator = frame::Frame::default();
        user_item.fixed(&indicator, 20);

        if state.audio_settings.is_muted {
            indicator.set_label("\u{2298}");
        } else if state.is_speaking {
            indicator.set_label("\u{25cf}");
        } else {
            indicator.set_label("\u{25cb}");
        }

        indicator.set_label_size(14);
        indicator.set_label_font(Font::Helvetica);
        if state.is_speaking && !state.audio_settings.is_muted {
            indicator.set_label_color(theme::SUCCESS);
        } else {
            indicator.set_label_color(theme::TEXT_MUTED);
        }

        // Username label — takes remaining space
        let display_name = if !is_us && (state.audio_settings.volume - 1.0).abs() > 0.01 {
            format!(
                "{} ({}%)",
                state.username,
                (state.audio_settings.volume * 100.0) as i32
            )
        } else {
            state.username.clone()
        };

        let mut name_label = frame::Frame::default().with_label(&display_name);
        name_label.set_align(enums::Align::Left | enums::Align::Inside);

        if is_us {
            name_label.set_label_color(theme::ACCENT);
        } else if state.audio_settings.is_muted {
            name_label.set_label_color(theme::DANGER);
        } else {
            name_label.set_label_color(theme::TEXT_PRIMARY);
        }
        name_label.set_label_size(13);

        // Status icon frame — shows self-muted/deafened SVG for all users.
        // For self: use local_muted/local_deafened. For remote: use broadcast state.
        let mut icon_frm = frame::Frame::default();
        user_item.fixed(&icon_frm, 18);

        let (show_deafened, show_muted) = if is_us {
            (self.local_deafened, self.local_muted)
        } else {
            (state.is_self_deafened, state.is_self_muted)
        };

        if show_deafened {
            if let Some(img) = svg_icon(SVG_SELF_DEAFENED, 14) {
                icon_frm.set_image(Some(img));
            }
        } else if show_muted {
            if let Some(img) = svg_icon(SVG_SELF_MUTED, 14) {
                icon_frm.set_image(Some(img));
            }
        }

        let status_icon = Some(icon_frm);

        // Action button: "Live" badge for all users, "…" menu for non-self.
        // Both self and non-self get the button so refresh_user_rows can
        // toggle the "Live" badge uniformly.
        let is_sharing = self.active_sharers.contains(&client_id);

        let mut options_btn = button::Button::default();
        options_btn.set_frame(enums::FrameType::FlatBox);
        user_item.fixed(&options_btn, 32);

        if is_sharing {
            options_btn.set_label("Live");
            options_btn.set_label_size(11);
            options_btn.set_label_font(Font::HelveticaBold);
            options_btn.set_color(theme::DANGER);
            options_btn.set_label_color(enums::Color::White);
        } else if is_us {
            // Self, not sharing — empty invisible slot
            options_btn.set_label("");
            options_btn.set_color(theme::BG_SECONDARY);
            options_btn.set_label_color(theme::BG_SECONDARY);
        } else {
            // Remote, not sharing — hidden "…" (matches parent bg)
            options_btn.set_label("\u{2026}");
            options_btn.set_label_size(16);
            options_btn.set_color(theme::BG_SECONDARY);
            options_btn.set_label_color(theme::BG_SECONDARY);
        }

        if !is_us {
            // Non-self: clicking opens audio popup
            let sender = self.sender;
            let username = state.username.clone();
            let client_id_copy = client_id;

            options_btn.set_callback(move |_| {
                sender.send(UiMessage::OpenUserAudioPopup {
                    client_id: client_id_copy,
                    username: username.clone(),
                    click_x: app::event_x_root(),
                    click_y: app::event_y_root(),
                });
            });

            // Hover: reveal "…" + tint row
            let mut btn_clone = options_btn.clone();
            let mut container = user_item.clone();
            let base_color = user_item.color();
            let hover_color = enums::Color::from_rgb(55, 55, 65);
            let was_hovering = std::rc::Rc::new(std::cell::RefCell::new(false));
            let hovering = was_hovering.clone();

            let sender_rc = self.sender;
            let client_id_rc = client_id;
            let username_rc = state.username.clone();

            user_item.handle(move |w, ev| {
                match ev {
                    enums::Event::Enter | enums::Event::Move => {
                        if !*hovering.borrow() {
                            *hovering.borrow_mut() = true;

                            let is_live = btn_clone.label() == "Live";
                            if !is_live {
                                btn_clone.set_label_color(theme::TEXT_PRIMARY);
                                btn_clone.set_color(hover_color);
                                btn_clone.redraw();
                            }
                            container.set_color(hover_color);
                            container.redraw();
                        }
                        true
                    }
                    enums::Event::Leave => {
                        let mx = app::event_x();
                        let my = app::event_y();
                        let wx = w.x();
                        let wy = w.y();
                        if mx < wx || mx > wx + w.w() || my < wy || my > wy + w.h() {
                            *hovering.borrow_mut() = false;

                            let is_live = btn_clone.label() == "Live";
                            if !is_live {
                                btn_clone.set_label_color(base_color);
                                btn_clone.set_color(base_color);
                                btn_clone.redraw();
                            }
                            container.set_color(base_color);
                            container.redraw();
                        }
                        true
                    }
                    enums::Event::Push if app::event_button() == 3 => {
                        sender_rc.send(UiMessage::OpenUserAudioPopup {
                            client_id: client_id_rc,
                            username: username_rc.clone(),
                            click_x: app::event_x_root(),
                            click_y: app::event_y_root(),
                        });
                        true
                    }
                    _ => false,
                }
            });
        }

        let action_btn_handle = Some(options_btn);

        user_item.end();
        self.user_list_pack.add(&user_item);

        self.user_rows.push(UserRow {
            client_id,
            container: user_item,
            indicator,
            name_label,
            status_icon,
            action_btn: action_btn_handle,
            is_us,
        });
    }

    pub fn set_connection_status(&mut self, connected: bool) {
        if connected {
            self.connection_status.set_label("\u{25cf} Connected");
            self.connection_status.set_label_color(theme::SUCCESS);
        } else {
            self.connection_status.set_label("\u{25cf} Disconnected");
            self.connection_status.set_label_color(theme::DANGER);
        }
    }

    pub fn update_latency(&mut self, rtt_ms: f64, audio_latency_ms: Option<u32>) {
        let status_label = if rtt_ms < 1.0 {
            "Disconnected".to_string()
        } else {
            "Connected".to_string()
        };
        self.connection_status.set_label(&status_label);

        if rtt_ms < 80.0 {
            self.connection_status.set_label_color(theme::SUCCESS);
        } else if rtt_ms < 150.0 {
            self.connection_status.set_label_color(theme::WARNING);
        } else {
            self.connection_status.set_label_color(theme::DANGER);
        }

        let net_str = if rtt_ms < 1.0 {
            "<1ms".to_string()
        } else {
            format!("{rtt_ms:.0}ms")
        };
        let audio_str = match audio_latency_ms {
            Some(ms) => format!("{ms}ms"),
            None => "--".to_string(),
        };
        self.latency_label
            .set_label(&format!("Audio: {audio_str} | Network: {net_str}"));
    }

    pub fn handle_disconnected(&mut self) {
        self.close_audio_popup();
        self.set_connection_status(false);
        self.latency_label.set_label("Audio: -- | Network: --");
        self.user_list_pack.clear();
        self.user_rows.clear();
        self.user_count_label.set_label("Users: 0");
        self.active_sharers.clear();
        self.set_share_state(false);
        self.update_stream_stats(None);
    }

    pub fn set_mute_state(&mut self, muted: bool) {
        self.local_muted = muted;
        if muted {
            self.mute_btn.set_label("Unmute");
            self.mute_btn.set_color(theme::DANGER);
            self.mute_btn.set_label_color(enums::Color::White);
        } else {
            self.mute_btn.set_label("Mute");
            self.mute_btn.set_color(theme::BG_TERTIARY);
            self.mute_btn.set_label_color(theme::TEXT_PRIMARY);
        }
        self.update_self_status_icon();
    }

    pub fn set_deafen_state(&mut self, deafened: bool) {
        self.local_deafened = deafened;
        if deafened {
            self.deafen_btn.set_label("Undeafen");
            self.deafen_btn.set_color(theme::WARNING);
            self.deafen_btn.set_label_color(enums::Color::White);
        } else {
            self.deafen_btn.set_label("Deafen");
            self.deafen_btn.set_color(theme::BG_TERTIARY);
            self.deafen_btn.set_label_color(theme::TEXT_PRIMARY);
        }
        self.update_self_status_icon();
    }

    /// Update the mute/deafen icon on our own user row from local state.
    fn update_self_status_icon(&mut self) {
        for row in &mut self.user_rows {
            if !row.is_us {
                continue;
            }
            if let Some(icon_frame) = &mut row.status_icon {
                if self.local_deafened {
                    if let Some(img) = svg_icon(SVG_SELF_DEAFENED, 14) {
                        icon_frame.set_image(Some(img));
                    }
                } else if self.local_muted {
                    if let Some(img) = svg_icon(SVG_SELF_MUTED, 14) {
                        icon_frame.set_image(Some(img));
                    }
                } else {
                    icon_frame.set_image(None::<SvgImage>);
                }
                icon_frame.redraw();
            }
            break;
        }
    }
}