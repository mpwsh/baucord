//! The baucord UI crate.
//!
//! `.slint` sources live in `slint/`; `build.rs` compiles them and this
//! crate re-exports the generated components (`MainWindow`, which
//! carries the Settings and admin pages, `StreamViewerWindow`,
//! `BaucordTray`) plus the `State` / `Logic` / `Tr` / `SettingsState` /
//! `SettingsLogic` / `AdminState` / `AdminLogic` globals. Icons come from the `@lucide` library that `build.rs`
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

    use super::{ChatRow, MenuEntry, UserRow};

    /// Menu entries → the overlay's rows: the main column and, in
    /// order of appearance, one column per submenu (`MenuEntry.submenu`
    /// indexes it). `label` localizes; the shell owns the catalog.
    ///
    /// The third part is what each row does, by `MenuEntry.id`: the
    /// overlay hands that id back on a pick and never learns what an
    /// action is (review P2-57 — it used to carry a Slint copy of
    /// app-core's enum, with a converter each way).
    #[must_use]
    pub fn menu_entries(
        entries: &[menu::Entry],
        label: &dyn Fn(&menu::Label) -> slint::SharedString,
    ) -> (Vec<MenuEntry>, Vec<Vec<MenuEntry>>, Vec<menu::Action>) {
        let mut submenus: Vec<Vec<MenuEntry>> = Vec::new();
        let mut actions: Vec<menu::Action> = Vec::new();
        let mut row = |e: &menu::Entry, submenu: i32| {
            actions.push(e.action);
            MenuEntry {
                label: label(&e.label),
                id: i32::try_from(actions.len() - 1).unwrap_or(-1),
                slider: matches!(e.action, menu::Action::Volume),
                enabled: e.enabled,
                separator: e.separator,
                danger: e.danger,
                submenu,
            }
        };
        let main = entries
            .iter()
            .map(|e| {
                let submenu = if e.submenu.is_empty() {
                    -1
                } else {
                    submenus.push(e.submenu.iter().map(|sub| row(sub, -1)).collect());
                    i32::try_from(submenus.len() - 1).unwrap_or(-1)
                };
                row(e, submenu)
            })
            .collect();
        (main, submenus, actions)
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
            channel_id: i32::from(r.channel_id),
            role: r.role.as_int(),
            initial: app_core::roster::initial_of(&r.name).into(),
            tile_tint: tint(&r.name),
            // No level known. That and everything left at its default
            // — the labels, the role's and the profile's look, the
            // telemetry — the shell fills from what the session may do
            // and from the profiles it knows.
            xp_pct: -1,
            ..Default::default()
        }
    }

    /// Channel header → sidebar model row. Only the channel fields
    /// carry meaning; user fields are inert defaults.
    #[must_use]
    pub fn channel_row(c: &ChannelRow) -> UserRow {
        UserRow {
            name: c.name.as_str().into(),
            volume_pct: 100,
            is_channel: true,
            channel_id: i32::from(c.id),
            locked: c.locked,
            member_label: match c.user_limit {
                Some(limit) => format!("{}/{limit}", c.members).into(),
                None => c.members.to_string().into(),
            },
            is_our_channel: c.is_ours,
            xp_pct: -1,
            members_only: c.members_only,
            temporary: c.temporary,
            ..Default::default()
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

    /// Chat item → chat model row. An image row's picture is not in
    /// the row: the pane asks for it (`Logic.chat-picture`) while the
    /// row is on screen, and `image` carries only a playing GIF's
    /// frame (the shell swaps it via `VecModel::set_row_data`).
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
                image_rev: 0,
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
                file_kind: "".into(),
                file_meta: "".into(),
                transfer_kind: 0,
                transfer_pct: 0,
                file_saved: false,
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
            ChatItem::File { meta, file } => ChatRow {
                body: file.name.as_str().into(),
                file_kind: file.kind_text().into(),
                file_meta: file.size_text().into(),
                ..base(5, Some(meta))
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
        // One copy: `new` would zero the buffer first, only to have it
        // overwritten (review 2026-10, P3-6).
        slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
            rgba, width, height,
        ))
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
        scaled_edge(PREVIEW_BOX_EDGE, scale_factor)
    }

    /// The send preview's picture box at its widest (main.slint: a
    /// 520 px card, 300 px tall inside its padding).
    pub const SEND_PREVIEW_BOX_EDGE: u32 = 480;

    /// Longest edge of the send preview's texture: its box at the
    /// window's scale. The dialog used to hold the full-resolution
    /// decode — up to 1280², 6.5 MB — to draw it at a third of that
    /// (review P3-41).
    #[must_use]
    pub fn send_preview_max_edge(scale_factor: f32) -> u32 {
        scaled_edge(SEND_PREVIEW_BOX_EDGE, scale_factor)
    }

    fn scaled_edge(logical: u32, scale_factor: f32) -> u32 {
        let scale = scale_factor.clamp(1.0, 2.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let edge = (logical as f32 * scale).ceil() as u32;
        edge
    }

    /// Decode for a picture box: the returned dimensions are the
    /// *original* image's (a chat row's layout box is computed from
    /// them), but the texture is downscaled to `max_edge` (see
    /// [`preview_max_edge`]) — a 1280² PNG is 6.5 MB of RGBA, drawn at
    /// 360 px. The full resolution is decoded only while the gallery
    /// viewer is open. Wire bytes only — a received picture, or one of
    /// ours already through `load_image_bytes` — so it takes app-core's
    /// received-picture decoder: bomb limits, and PNG, JPEG and GIF
    /// only (review P1-20).
    pub fn decode_to_preview(
        bytes: &[u8],
        max_edge: u32,
    ) -> Result<(slint::Image, u32, u32), app_core::images::ImageError> {
        let (pixels, w, h) = decode_preview_pixels(bytes, max_edge)?;
        Ok((slint::Image::from_rgba8(pixels), w, h))
    }

    /// [`decode_to_preview`]'s pixels, before they become an image: a
    /// `SharedPixelBuffer` is `Send`, so a worker thread can decode and
    /// hand the buffer to the UI thread, which wraps it (a chat row's
    /// picture, `pictures.rs`). Also the original dimensions.
    pub fn decode_preview_pixels(
        bytes: &[u8],
        max_edge: u32,
    ) -> Result<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, u32, u32), app_core::images::ImageError>
    {
        let decoded = app_core::images::decode_received(bytes)?;
        let (w, h) = (decoded.width(), decoded.height());
        let rgba = if w.max(h) > max_edge {
            decoded.thumbnail(max_edge, max_edge).into_rgba8()
        } else {
            decoded.into_rgba8()
        };
        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
        );
        Ok((pixels, w, h))
    }
}
