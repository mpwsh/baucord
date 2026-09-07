//! The baucord UI crate.
//!
//! `.slint` sources live in `slint/`; `build.rs` compiles them and this
//! crate re-exports the generated components (`MainWindow`,
//! `SettingsWindow`, `StreamViewerWindow`, `BaucordTray`) plus the
//! `State` / `Logic` / `Tr` / `SettingsState` / `SettingsLogic`
//! globals. Icons come from the `@lucide` library that `build.rs`
//! mounts (the `lucide-slint` build dependency). The [`convert`] module maps `app-core` view-model types
//! onto the generated row structs.
//!
//! This crate compiles identically for desktop and the future
//! `mobile-android` / `mobile-ios` shells — nothing in here may touch
//! desktop-only APIs.

#![allow(clippy::all, clippy::pedantic)] // generated code below
slint::include_modules!();

pub mod convert {
    //! app-core → generated-struct conversions.

    use app_core::{
        chat::{ChatItem, Meta},
        menu,
        roster::{split_id, ChannelRow, RosterItem, RosterRow},
    };

    use super::{ChatRow, MenuAction, MenuEntry, UserRow};

    /// Menu entries → the overlay's rows: the main column and, in
    /// order of appearance, one column per submenu (`MenuEntry.submenu`
    /// indexes it). `label` localizes; the shell owns the catalog.
    #[must_use]
    pub fn menu_entries(
        entries: &[menu::Entry],
        label: &dyn Fn(&menu::Label) -> slint::SharedString,
    ) -> (Vec<MenuEntry>, Vec<Vec<MenuEntry>>) {
        let mut submenus: Vec<Vec<MenuEntry>> = Vec::new();
        let main = entries
            .iter()
            .map(|e| {
                let submenu = if e.submenu.is_empty() {
                    -1
                } else {
                    submenus.push(
                        e.submenu
                            .iter()
                            .map(|sub| menu_entry(sub, -1, label))
                            .collect(),
                    );
                    i32::try_from(submenus.len() - 1).unwrap_or(-1)
                };
                menu_entry(e, submenu, label)
            })
            .collect();
        (main, submenus)
    }

    fn menu_entry(
        e: &menu::Entry,
        submenu: i32,
        label: &dyn Fn(&menu::Label) -> slint::SharedString,
    ) -> MenuEntry {
        let (action, arg) = menu_action(e.action);
        MenuEntry {
            label: label(&e.label),
            action,
            arg,
            enabled: e.enabled,
            separator: e.separator,
            danger: e.danger,
            submenu,
        }
    }

    /// An action and its parameter, as the Slint enum + `arg` carry
    /// them; [`menu_action_back`] is the inverse.
    #[must_use]
    pub fn menu_action(action: menu::Action) -> (MenuAction, i32) {
        use menu::Action as A;
        match action {
            A::None => (MenuAction::None, 0),
            A::ChannelEdit(id) => (MenuAction::ChannelEdit, i32::from(id)),
            A::ChannelDelete(id) => (MenuAction::ChannelDelete, i32::from(id)),
            A::Volume => (MenuAction::Volume, 0),
            A::MuteForMe => (MenuAction::MuteForMe, 0),
            A::ResetAudio => (MenuAction::ResetAudio, 0),
            A::Watch => (MenuAction::Watch, 0),
            A::Message => (MenuAction::Message, 0),
            A::SetRole(role) => (MenuAction::SetRole, role.as_int()),
            A::MoveTo(id) => (MenuAction::MoveTo, i32::from(id)),
            A::ChannelKick => (MenuAction::ChannelKick, 0),
            A::MuteVoice => (MenuAction::MuteVoice, 0),
            A::MuteChat => (MenuAction::MuteChat, 0),
            A::UnmuteVoice => (MenuAction::UnmuteVoice, 0),
            A::UnmuteChat => (MenuAction::UnmuteChat, 0),
            A::Disconnect => (MenuAction::Disconnect, 0),
            A::Kick => (MenuAction::Kick, 0),
            A::Ban => (MenuAction::Ban, 0),
            A::CopyText => (MenuAction::CopyText, 0),
            A::SelectText(on) => (MenuAction::SelectText, i32::from(on)),
            A::EditMessage => (MenuAction::EditMessage, 0),
            A::DeleteMessage => (MenuAction::DeleteMessage, 0),
            A::OpenImage => (MenuAction::OpenImage, 0),
            A::SaveImage => (MenuAction::SaveImage, 0),
            A::Cut => (MenuAction::Cut, 0),
            A::Copy => (MenuAction::Copy, 0),
            A::Paste => (MenuAction::Paste, 0),
        }
    }

    /// The pick back into app-core's action. A parameter the UI could
    /// not have produced (a role outside 1…4, a channel above `u16`)
    /// yields `None`.
    #[must_use]
    pub fn menu_action_back(action: MenuAction, arg: i32) -> Option<menu::Action> {
        use menu::Action as A;
        let channel = || u16::try_from(arg).ok();
        Some(match action {
            MenuAction::None => A::None,
            MenuAction::ChannelEdit => A::ChannelEdit(channel()?),
            MenuAction::ChannelDelete => A::ChannelDelete(channel()?),
            MenuAction::Volume => A::Volume,
            MenuAction::MuteForMe => A::MuteForMe,
            MenuAction::ResetAudio => A::ResetAudio,
            MenuAction::Watch => A::Watch,
            MenuAction::Message => A::Message,
            MenuAction::SetRole => A::SetRole(app_core::roster::Role::from_int(arg)?),
            MenuAction::MoveTo => A::MoveTo(channel()?),
            MenuAction::ChannelKick => A::ChannelKick,
            MenuAction::MuteVoice => A::MuteVoice,
            MenuAction::MuteChat => A::MuteChat,
            MenuAction::UnmuteVoice => A::UnmuteVoice,
            MenuAction::UnmuteChat => A::UnmuteChat,
            MenuAction::Disconnect => A::Disconnect,
            MenuAction::Kick => A::Kick,
            MenuAction::Ban => A::Ban,
            MenuAction::CopyText => A::CopyText,
            MenuAction::SelectText => A::SelectText(arg != 0),
            MenuAction::EditMessage => A::EditMessage,
            MenuAction::DeleteMessage => A::DeleteMessage,
            MenuAction::OpenImage => A::OpenImage,
            MenuAction::SaveImage => A::SaveImage,
            MenuAction::Cut => A::Cut,
            MenuAction::Copy => A::Copy,
            MenuAction::Paste => A::Paste,
        })
    }

    /// Roster row → sidebar model row.
    #[must_use]
    pub fn user_row(r: &RosterRow) -> UserRow {
        let (lo, hi) = split_id(r.client_id);
        UserRow {
            client_id_lo: lo,
            client_id_hi: hi,
            name: r.name.as_str().into(),
            volume_pct: r.volume_pct,
            is_us: r.is_us,
            speaking: r.speaking,
            local_muted: r.local_muted,
            self_muted: r.self_muted,
            self_deafened: r.self_deafened,
            sharing: r.sharing,
            is_channel: false,
            channel_id: i32::from(r.channel_id),
            locked: false,
            member_label: "".into(),
            is_our_channel: false,
            role: r.role.as_int(),
            level: i32::from(r.level),
            // The shell fills these from what the session may do and
            // from the profiles it knows.
            level_label: "".into(),
            role_label: "".into(),
            xp_pct: -1,
            xp_line: "".into(),
            telemetry: "".into(),
            avatar: slint::Image::default(),
            has_avatar: false,
            initial: app_core::roster::initial_of(&r.name).into(),
            tile_tint: tint(&r.name),
            name_color: slint::Color::default(),
            has_name_color: false,
            name_style: 0,
            outranked: false,
            role_editable: false,
            channel_kickable: false,
            members_only: false,
            temporary: false,
            is_afk: false,
            owned: false,
            from_config: false,
        }
    }

    /// Channel header → sidebar model row. Only the channel fields
    /// carry meaning; user fields are inert defaults.
    #[must_use]
    pub fn channel_row(c: &ChannelRow) -> UserRow {
        UserRow {
            client_id_lo: 0,
            client_id_hi: 0,
            name: c.name.as_str().into(),
            volume_pct: 100,
            is_us: false,
            speaking: false,
            local_muted: false,
            self_muted: false,
            self_deafened: false,
            sharing: false,
            is_channel: true,
            channel_id: i32::from(c.id),
            locked: c.locked,
            member_label: match c.user_limit {
                Some(limit) => format!("{}/{limit}", c.members).into(),
                None => c.members.to_string().into(),
            },
            is_our_channel: c.is_ours,
            role: 0,
            level: 0,
            level_label: "".into(),
            role_label: "".into(),
            xp_pct: -1,
            xp_line: "".into(),
            telemetry: "".into(),
            avatar: slint::Image::default(),
            has_avatar: false,
            initial: "".into(),
            tile_tint: slint::Color::default(),
            name_color: slint::Color::default(),
            has_name_color: false,
            name_style: 0,
            outranked: false,
            role_editable: false,
            channel_kickable: false,
            members_only: c.members_only,
            temporary: c.temporary,
            is_afk: c.is_afk,
            owned: c.owned,
            from_config: c.from_config,
        }
    }

    /// Grouped roster item → sidebar model row.
    #[must_use]
    pub fn roster_item(item: &RosterItem) -> UserRow {
        match item {
            RosterItem::Channel(c) => channel_row(c),
            RosterItem::User(u) => user_row(u),
        }
    }

    /// Chat item → chat model row. Image rows get a placeholder image
    /// here; the shell installs the decoded frame (and drives GIF frame
    /// swaps) via `VecModel::set_row_data`.
    #[must_use]
    pub fn chat_row(item: &ChatItem, row_id: i32) -> ChatRow {
        // Every field a system row has no use for, in one place.
        let base = |kind: i32, meta: Option<&Meta>| {
            let (lo, hi) = split_id(meta.map_or(0, |m| m.id));
            ChatRow {
                kind,
                ts: meta.map_or("", |m| m.ts.as_str()).into(),
                user: meta.map_or("", |m| m.user.as_str()).into(),
                body: "".into(),
                styled: false,
                body_styled: Default::default(),
                image: slint::Image::default(),
                img_w: 0.0,
                img_h: 0.0,
                row_id,
                is_gif: false,
                msg_id_lo: lo,
                msg_id_hi: hi,
                own: meta.is_some_and(|m| m.own),
                edited: false,
                deleted: false,
                pending: meta.is_some_and(|m| m.pending),
                continuation: false,
                avatar: slint::Image::default(),
                has_avatar: false,
                initial: meta.map_or_else(
                    || "".into(),
                    |m| app_core::roster::initial_of(&m.user).into(),
                ),
                tile_tint: meta.map_or(slint::Color::default(), |m| tint(&m.user)),
                name_color: slint::Color::default(),
                has_name_color: false,
                name_style: 0,
            }
        };
        match item {
            ChatItem::Message {
                meta,
                body,
                edited,
                deleted,
            } => ChatRow {
                body: body.as_str().into(),
                edited: *edited,
                deleted: *deleted,
                ..base(0, Some(meta))
            },
            ChatItem::System { body } => ChatRow {
                body: body.as_str().into(),
                ..base(1, None)
            },
            ChatItem::DayMarker { label, .. } => ChatRow {
                body: label.as_str().into(),
                ..base(4, None)
            },
            ChatItem::Code { meta, code } => ChatRow {
                body: code.as_str().into(),
                ..base(3, Some(meta))
            },
            #[allow(clippy::cast_precision_loss)]
            ChatItem::Image {
                meta,
                width,
                height,
                is_gif,
            } => ChatRow {
                img_w: *width as f32,
                img_h: *height as f32,
                is_gif: *is_gif,
                ..base(2, Some(meta))
            },
        }
    }

    /// The fallback tile's color for a name.
    #[must_use]
    pub fn tint(name: &str) -> slint::Color {
        let [r, g, b] = app_core::roster::tile_tint(name);
        slint::Color::from_rgb_u8(r, g, b)
    }

    /// Packed RGBA bytes → `slint::Image` (shared-buffer copy).
    ///
    /// Length-guarded: a stride/size mismatch yields a blank image and
    /// a log line instead of a `copy_from_slice` panic — under the
    /// release profile's `panic = "abort"`, that panic would silently
    /// kill the whole app on the first bad frame.
    #[must_use]
    pub fn rgba_image(rgba: &[u8], width: u32, height: u32) -> slint::Image {
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            log::error!(
                "rgba_image: {}x{} needs {expected} bytes, got {} — dropping frame",
                width,
                height,
                rgba.len()
            );
            return slint::Image::default();
        }
        let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
        buf.make_mut_bytes().copy_from_slice(rgba);
        slint::Image::from_rgba8(buf)
    }

    /// Decode PNG/GIF-first-frame bytes → `slint::Image` + dimensions.
    /// Uses app-core's bounded decoder (bomb limits included).
    pub fn decode_to_image(
        bytes: &[u8],
    ) -> Result<(slint::Image, u32, u32), app_core::images::ImageError> {
        let decoded = app_core::images::decode_bounded(bytes)?;
        let rgba = decoded.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        Ok((rgba_image(rgba.as_raw(), w, h), w, h))
    }

    /// The chat-row image box's longest side, in logical pixels
    /// (chat.slint draws images in at most 360×280).
    pub const PREVIEW_BOX_EDGE: u32 = 360;

    /// Longest edge of a chat-row preview texture for a window at
    /// `scale_factor`: the box at that scale, so a 1× display keeps
    /// 360² per picture (0.5 MB) and a 2× display 720² (2 MB) — the
    /// old fixed 720 paid the hidpi price everywhere. Capped at 2×.
    #[must_use]
    pub fn preview_max_edge(scale_factor: f32) -> u32 {
        let scale = scale_factor.clamp(1.0, 2.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let edge = (PREVIEW_BOX_EDGE as f32 * scale).ceil() as u32;
        edge
    }

    /// Decode for a chat row: the returned dimensions are the
    /// *original* image's (the row's layout box is computed from
    /// them), but the texture is downscaled to `max_edge` (see
    /// [`preview_max_edge`]) — a 1280² PNG is 6.5 MB of RGBA that
    /// would otherwise stay resident for the row's whole life just to
    /// be drawn at 360 px. The full-resolution decode happens only
    /// while the gallery viewer is open.
    pub fn decode_to_preview(
        bytes: &[u8],
        max_edge: u32,
    ) -> Result<(slint::Image, u32, u32), app_core::images::ImageError> {
        let decoded = app_core::images::decode_bounded(bytes)?;
        let (w, h) = (decoded.width(), decoded.height());
        let rgba = if w.max(h) > max_edge {
            decoded.thumbnail(max_edge, max_edge).to_rgba8()
        } else {
            decoded.to_rgba8()
        };
        let (tw, th) = (rgba.width(), rgba.height());
        Ok((rgba_image(rgba.as_raw(), tw, th), w, h))
    }
}
