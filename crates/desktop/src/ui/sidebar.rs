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
use i18n::t;
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

thread_local! {
    /// Parsed + scaled status icons, cached per thread (all UI work runs
    /// on the main thread). Re-parsing the SVGs on every user-list
    /// refresh was pure waste. Index 0 = muted, 1 = deafened.
    static STATUS_ICONS: std::cell::RefCell<[Option<SvgImage>; 2]> =
        const { std::cell::RefCell::new([None, None]) };
}

/// The mute/deafen status icon for a row, or `None` for neither.
/// Deafened takes precedence. Parsed once, cloned per use.
fn status_icon_image(deafened: bool, muted: bool) -> Option<SvgImage> {
    let idx = if deafened {
        1
    } else if muted {
        0
    } else {
        return None;
    };
    STATUS_ICONS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache[idx].is_none() {
            let data = if idx == 1 {
                SVG_SELF_DEAFENED
            } else {
                SVG_SELF_MUTED
            };
            cache[idx] = svg_icon(data, 14);
        }
        cache[idx].clone()
    })
}

// ── Shared row styling ──────────────────────────────────────────────────
// Used by BOTH create_user_item and refresh_user_rows so the two paths
// cannot drift apart (they already had, before this was factored out).

fn row_bg_color(state: &UserState) -> enums::Color {
    if state.audio_settings.is_muted {
        theme::ROW_MUTED_TINT
    } else if (state.audio_settings.volume - 1.0).abs() > 0.01 {
        theme::ROW_VOLUME_TINT
    } else {
        theme::BG_SECONDARY
    }
}

/// Speaking-indicator label + colour. Muted wins over speaking.
fn indicator_style(state: &UserState) -> (&'static str, enums::Color) {
    if state.audio_settings.is_muted {
        ("\u{2298}", theme::TEXT_MUTED)
    } else if state.is_speaking {
        ("\u{25cf}", theme::SUCCESS)
    } else {
        ("\u{25cb}", theme::TEXT_MUTED)
    }
}

/// Display name, with a volume badge for non-default remote volume.
fn display_name(state: &UserState, is_us: bool) -> String {
    if !is_us && (state.audio_settings.volume - 1.0).abs() > 0.01 {
        format!(
            "{} ({}%)",
            state.username,
            (state.audio_settings.volume * 100.0) as i32
        )
    } else {
        state.username.clone()
    }
}

fn name_color(state: &UserState, is_us: bool) -> enums::Color {
    if is_us {
        theme::ACCENT
    } else if state.audio_settings.is_muted {
        // Grey, not red: "muted" is a state you chose, not an error.
        // The tinted row, crossed indicator and icon carry the rest.
        theme::TEXT_MUTED
    } else {
        theme::TEXT_PRIMARY
    }
}

/// Style the row's action button: "Live" badge when sharing, invisible
/// slot for self, hidden "…" (revealed on hover) for remote users.
fn style_action_btn(btn: &mut button::Button, is_sharing: bool, is_us: bool, row_bg: enums::Color) {
    if is_sharing {
        btn.set_label(&t!("live"));
        btn.set_label_size(11);
        btn.set_label_font(Font::HelveticaBold);
        // Red, not accent: with a blue accent a blue "Live" reads
        // wrong, and red no longer collides with the accent color.
        btn.set_color(theme::DANGER);
        btn.set_label_color(theme::BUTTON_TEXT);
    } else if is_us {
        btn.set_label("");
        btn.set_color(row_bg);
        btn.set_label_color(row_bg);
    } else {
        btn.set_label("\u{2026}");
        btn.set_label_size(16);
        btn.set_label_font(Font::Helvetica);
        btn.set_color(row_bg);
        btn.set_label_color(row_bg);
    }
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

/// Live stream statistics for the sharer-side stats block.
pub struct StreamStats<'a> {
    pub quality: &'a str,
    pub avg_frame_kb: f32,
    pub encode_ms: f32,
    pub viewers: u32,
    /// 100 = full rate; below 100 the sender is shedding fps for lossy viewers.
    pub fps_scale_pct: u32,
    /// Worst viewer loss in percent (from StreamHealth).
    pub loss_pct: f32,
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
    resource_label: frame::Frame,
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
    /// Mirrors set_share_state, so relabel() picks the right label.
    sharing: bool,
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
            .with_label(&t!("users-count", "count" => 0))
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
        let mut stream_stats_label = frame::Frame::default().with_label("");
        stream_stats_label
            .set_align(enums::Align::Left | enums::Align::Inside | enums::Align::Clip);
        sidebar.fixed(&stream_stats_label, 0); // 0 height = hidden
        stream_stats_label.set_label_color(theme::TEXT_MUTED);
        stream_stats_label.set_label_size(10);
        stream_stats_label.hide();

        // ── Connection status panel ─────────────────────────────────
        let mut status_col = group::Flex::default().column();
        status_col.set_spacing(2);
        sidebar.fixed(&status_col, 66);

        let mut connection_status = frame::Frame::default().with_label(&t!("status-connecting"));
        status_col.fixed(&connection_status, 20);
        connection_status.set_label_color(theme::CYAN);
        connection_status.set_label_size(12);
        connection_status.set_align(enums::Align::Left | enums::Align::Inside);

        let mut latency_label = frame::Frame::default().with_label(" ");
        status_col.fixed(&latency_label, 16);
        latency_label.set_label_color(theme::TEXT_MUTED);
        latency_label.set_label_size(10);
        latency_label.set_align(enums::Align::Left | enums::Align::Inside | enums::Align::Clip);

        // Own-process CPU/memory. Blank until the first sample; only
        // refreshed while the sidebar is actually on screen (App gates
        // the sampling on `is_on_screen`).
        let mut resource_label = frame::Frame::default().with_label(" ");
        status_col.fixed(&resource_label, 14);
        resource_label.set_label_color(theme::TEXT_MUTED);
        resource_label.set_label_size(10);
        resource_label.set_align(enums::Align::Left | enums::Align::Inside | enums::Align::Clip);

        status_col.end();

        // Mute / Deafen row
        let mut audio_row = group::Flex::default().row();
        audio_row.set_spacing(5);
        sidebar.fixed(&audio_row, 35);

        let mute_btn = controls::button(&t!("mute"));
        let deafen_btn = controls::button(&t!("deafen"));

        audio_row.end();

        // Share Screen button — full width, toggles to "Stop Sharing"
        let share_btn = controls::button(&t!("share-screen"));
        sidebar.fixed(&share_btn, 35);

        // Settings button
        let mut settings_btn = controls::button(&t!("settings"));
        sidebar.fixed(&settings_btn, 35);

        // Disconnect button
        let mut disconnect_btn = controls::accent_button(&t!("disconnect"), theme::ACCENT);
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
            s.send(UiMessage::ToggleScreenShare);
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
            resource_label,
            mute_btn,
            deafen_btn,
            share_btn,
            user_rows: Vec::new(),
            active_sharers: HashSet::new(),
            audio_popup: None,
            local_muted: false,
            local_deafened: false,
            sharing: false,
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
        self.sharing = is_sharing;
        if is_sharing {
            self.share_btn.set_label(&t!("stop-sharing"));
            self.share_btn.set_color(theme::DANGER);
            self.share_btn.set_label_color(theme::BUTTON_TEXT);
        } else {
            self.share_btn.set_label(&t!("share-screen"));
            self.share_btn.set_color(theme::BG_TERTIARY);
            self.share_btn.set_label_color(theme::TEXT_PRIMARY);
        }
    }

    /// Update the two-line stream stats block shown above the connection
    /// status while sharing. Pass `None` to hide it.
    pub fn update_stream_stats(&mut self, stats: Option<&StreamStats>) {
        match stats {
            Some(s) => {
                let line1 = t!("stream-stats-quality",
                    "quality" => s.quality, "viewers" => s.viewers);
                let mut line2 = t!("stream-stats-frame",
                    "kb" => format!("{:.0}", s.avg_frame_kb),
                    "ms" => format!("{:.1}", s.encode_ms));
                let degraded = s.fps_scale_pct < 100;
                if degraded {
                    line2.push_str(" · ");
                    line2.push_str(&t!("stream-stats-degraded",
                        "fps" => s.fps_scale_pct,
                        "loss" => format!("{:.1}", s.loss_pct)));
                }
                // Leading pads are layout (left inset), not message text.
                self.stream_stats_label
                    .set_label(&format!("  {line1}\n  {line2}"));
                // The block turns warning-colored when adaptation kicked
                // in — the sharer should notice without reading numbers.
                self.stream_stats_label.set_label_color(if degraded {
                    theme::WARNING
                } else {
                    theme::TEXT_MUTED
                });
                if !self.stream_stats_label.visible() {
                    self.stream_stats_label.show();
                    self.group.fixed(&self.stream_stats_label, 30);
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

    pub fn close_audio_popup(&mut self) {
        if let Some(mut w) = self.audio_popup.take() {
            w.hide();
        }
    }
    #[allow(clippy::too_many_arguments)]
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

        let mut vol_header = frame::Frame::default()
            .with_label(&t!("user-volume"))
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

        let mute_label = if is_muted { t!("unmute") } else { t!("mute") };
        let mut mute_btn = if is_muted {
            controls::accent_button(&mute_label, theme::DANGER)
        } else {
            controls::button(&mute_label)
        };
        col.fixed(&mute_btn, 26);

        let mut sep2 = frame::Frame::default();
        sep2.set_frame(enums::FrameType::FlatBox);
        sep2.set_color(theme::BG_TERTIARY);
        col.fixed(&sep2, 1);

        let mut reset_btn = controls::button(&t!("reset-audio"));
        col.fixed(&reset_btn, 26);

        if is_sharing {
            let mut watch_btn = controls::accent_button(&t!("watch-stream"), theme::ACCENT);
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
            .set_label(&t!("users-count", "count" => users.len() as u32));

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

            let (ind_label, ind_color) = indicator_style(state);
            row.indicator.set_label(ind_label);
            row.indicator.set_label_color(ind_color);

            row.name_label.set_label(&display_name(state, row.is_us));
            row.name_label.set_label_color(name_color(state, row.is_us));

            let bg = row_bg_color(state);
            row.container.set_color(bg);

            // Mute/deafen icon — for self rows use local state (which is
            // always up to date), for remote rows use broadcast state.
            if let Some(icon_frame) = &mut row.status_icon {
                let (show_deafened, show_muted) = if row.is_us {
                    (self.local_deafened, self.local_muted)
                } else {
                    (state.is_self_deafened, state.is_self_muted)
                };
                icon_frame.set_image(status_icon_image(show_deafened, show_muted));
                icon_frame.redraw();
            }

            // "Live" badge — applies to both self and non-self rows
            if let Some(btn) = &mut row.action_btn {
                let is_sharing = self.active_sharers.contains(&row.client_id);
                style_action_btn(btn, is_sharing, row.is_us, bg);
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

        let row_bg = row_bg_color(state);
        user_item.set_color(row_bg);

        // Speaking indicator
        let mut indicator = frame::Frame::default();
        user_item.fixed(&indicator, 20);
        indicator.set_label_size(14);
        indicator.set_label_font(Font::Helvetica);
        let (ind_label, ind_color) = indicator_style(state);
        indicator.set_label(ind_label);
        indicator.set_label_color(ind_color);

        // Username label — takes remaining space
        let mut name_label = frame::Frame::default().with_label(&display_name(state, is_us));
        name_label.set_align(enums::Align::Left | enums::Align::Inside);
        name_label.set_label_color(name_color(state, is_us));
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
        icon_frm.set_image(status_icon_image(show_deafened, show_muted));

        let status_icon = Some(icon_frm);

        // Action button: "Live" badge for all users, "…" menu for non-self.
        // Both self and non-self get the button so refresh_user_rows can
        // toggle the "Live" badge uniformly.
        let is_sharing = self.active_sharers.contains(&client_id);

        let mut options_btn = button::Button::default();
        options_btn.set_frame(enums::FrameType::FlatBox);
        // 52px (was 32) so localized "Live" badges (e.g. "En vivo") fit.
        user_item.fixed(&options_btn, 52);
        style_action_btn(&mut options_btn, is_sharing, is_us, row_bg);

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
            let hover_color = theme::HOVER;
            let was_hovering = std::rc::Rc::new(std::cell::RefCell::new(false));
            let hovering = was_hovering.clone();

            let sender_rc = self.sender;
            let client_id_rc = client_id;
            let username_rc = state.username.clone();

            user_item.handle(move |w, ev| match ev {
                enums::Event::Enter | enums::Event::Move => {
                    if !*hovering.borrow() {
                        *hovering.borrow_mut() = true;

                        let is_live = btn_clone.label() == t!("live");
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

                        let is_live = btn_clone.label() == t!("live");
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
                enums::Event::Push if app::event_mouse_button() == app::MouseButton::Right => {
                    sender_rc.send(UiMessage::OpenUserAudioPopup {
                        client_id: client_id_rc,
                        username: username_rc.clone(),
                        click_x: app::event_x_root(),
                        click_y: app::event_y_root(),
                    });
                    true
                }
                _ => false,
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

    /// Re-set the static labels after a language change. Status and
    /// latency lines self-heal on their next periodic update; settings /
    /// disconnect buttons aren't stored and refresh on the next connect.
    pub fn relabel(&mut self) {
        // Route through the state setters so a language change while
        // muted/deafened keeps the toggled label ("Unmute"/"Undeafen"),
        // not the base one.
        self.set_mute_state(self.local_muted);
        self.set_deafen_state(self.local_deafened);
        self.share_btn.set_label(&t!(if self.sharing {
            "stop-sharing"
        } else {
            "share-screen"
        }));
        self.user_count_label
            .set_label(&t!("users-count", "count" => self.user_rows.len() as u32));
    }

    pub fn set_connection_status(&mut self, connected: bool) {
        if connected {
            self.connection_status
                .set_label(&format!("\u{25cf} {}", t!("status-connected")));
            self.connection_status.set_label_color(theme::SUCCESS);
        } else {
            self.connection_status
                .set_label(&format!("\u{25cf} {}", t!("status-disconnected")));
            self.connection_status.set_label_color(theme::DANGER);
        }
    }

    /// Whether the sidebar is actually being rendered: `visible_r`
    /// walks the parent chain, so this is false when the wizard shows
    /// the connect view, the window is hidden to the tray, or it is
    /// minimized (FLTK clears the window's visible flag on iconify on
    /// all three platforms). Stats updates and resource sampling are
    /// skipped while this is false.
    pub fn is_on_screen(&self) -> bool {
        self.group.visible_r()
    }

    /// Update the own-process resource line (CPU % of the whole
    /// machine, resident memory in MB).
    pub fn update_resources(&mut self, cpu_pct: f32, mem_mb: f64) {
        self.resource_label.set_label(&t!("resource-line",
            "cpu" => format!("{cpu_pct:.1}"),
            "mem" => format!("{mem_mb:.0}")));
    }

    pub fn update_latency(
        &mut self,
        rtt_ms: f64,
        loss_pct: f64,
        audio_latency_ms: Option<u32>,
        kbps_up: f64,
        kbps_down: f64,
    ) {
        // Color from RTT only. (The old `< 1ms ⇒ Disconnected` heuristic
        // showed "Disconnected" mid-call on any LAN — disconnection is a
        // real event handled by handle_disconnected, never inferred.)
        self.connection_status
            .set_label(&format!("\u{25cf} {}", t!("status-connected")));
        self.connection_status.set_label_color(if rtt_ms < 80.0 {
            theme::SUCCESS
        } else if rtt_ms < 150.0 {
            theme::WARNING
        } else {
            theme::DANGER
        });

        let net_str = if rtt_ms < 1.0 {
            "<1ms".to_string()
        } else if loss_pct >= 0.05 {
            // A lossy link is the #1 cause of bad streams and was
            // previously invisible — show it the moment it's nonzero.
            format!("{rtt_ms:.0}ms · {loss_pct:.1}% loss")
        } else {
            format!("{rtt_ms:.0}ms")
        };
        let audio_str = match audio_latency_ms {
            Some(ms) => format!("{ms}ms"),
            None => "--".to_string(),
        };
        self.latency_label.set_label(&t!("latency-line",
            "audio" => audio_str,
            "network" => net_str,
            "up" => format!("{:.1}", kbps_up / 1000.0),
            "down" => format!("{:.1}", kbps_down / 1000.0)));
    }

    pub fn handle_disconnected(&mut self) {
        self.close_audio_popup();
        self.set_connection_status(false);
        self.latency_label.set_label(&t!("latency-line",
            "audio" => "--", "network" => "--", "up" => "--", "down" => "--"));
        self.resource_label.set_label(" ");
        self.user_list_pack.clear();
        self.user_rows.clear();
        self.user_count_label
            .set_label(&t!("users-count", "count" => 0));
        self.active_sharers.clear();
        self.set_share_state(false);
        self.update_stream_stats(None);
    }

    pub fn set_mute_state(&mut self, muted: bool) {
        self.local_muted = muted;
        if muted {
            self.mute_btn.set_label(&t!("unmute"));
            self.mute_btn.set_color(theme::ACCENT_HOVER);
            self.mute_btn.set_label_color(theme::BUTTON_TEXT);
        } else {
            self.mute_btn.set_label(&t!("mute"));
            self.mute_btn.set_color(theme::BG_TERTIARY);
            self.mute_btn.set_label_color(theme::TEXT_PRIMARY);
        }
        self.update_self_status_icon();
    }

    pub fn set_deafen_state(&mut self, deafened: bool) {
        self.local_deafened = deafened;
        if deafened {
            self.deafen_btn.set_label(&t!("undeafen"));
            self.deafen_btn.set_color(theme::ACCENT_HOVER);
            self.deafen_btn.set_label_color(theme::BUTTON_TEXT);
        } else {
            self.deafen_btn.set_label(&t!("deafen"));
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
                icon_frame.set_image(status_icon_image(self.local_deafened, self.local_muted));
                icon_frame.redraw();
            }
            break;
        }
    }
}
