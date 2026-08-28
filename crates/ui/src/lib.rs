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

    use super::{ChatRow, UserRow};
    use app_core::chat::ChatItem;
    use app_core::roster::{split_id, ChannelRow, RosterItem, RosterRow};

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
            channel_id: -1,
            locked: false,
            member_label: "".into(),
            is_our_channel: false,
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
        match item {
            ChatItem::Message { ts, user, body } => ChatRow {
                kind: 0,
                ts: ts.as_str().into(),
                user: user.as_str().into(),
                body: body.as_str().into(),
                styled: false,
                body_styled: Default::default(),
                image: slint::Image::default(),
                img_w: 0.0,
                img_h: 0.0,
                row_id,
                is_gif: false,
            },
            ChatItem::System { body } => ChatRow {
                kind: 1,
                ts: "".into(),
                user: "".into(),
                body: body.as_str().into(),
                styled: false,
                body_styled: Default::default(),
                image: slint::Image::default(),
                img_w: 0.0,
                img_h: 0.0,
                row_id,
                is_gif: false,
            },
            ChatItem::Code { ts, user, code } => ChatRow {
                kind: 3,
                ts: ts.as_str().into(),
                user: user.as_str().into(),
                body: code.as_str().into(),
                styled: false,
                body_styled: Default::default(),
                image: slint::Image::default(),
                img_w: 0.0,
                img_h: 0.0,
                row_id,
                is_gif: false,
            },
            #[allow(clippy::cast_precision_loss)]
            ChatItem::Image {
                ts,
                user,
                width,
                height,
                is_gif,
                ..
            } => ChatRow {
                kind: 2,
                ts: ts.as_str().into(),
                user: user.as_str().into(),
                body: "".into(),
                styled: false,
                body_styled: Default::default(),
                image: slint::Image::default(),
                img_w: *width as f32,
                img_h: *height as f32,
                row_id,
                is_gif: *is_gif,
            },
        }
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
        let mut buf =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
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

    /// Longest edge of a chat-row preview texture. Chat displays
    /// images in at most a 360×280 box (chat.slint), so 720 px covers
    /// a 2× hidpi scale exactly; the full-resolution decode happens
    /// only when the gallery viewer opens.
    pub const PREVIEW_MAX_EDGE: u32 = 720;

    /// Decode for a chat row: the returned dimensions are the
    /// *original* image's (the row's layout box is computed from
    /// them), but the texture is downscaled to
    /// [`PREVIEW_MAX_EDGE`] — a 1280² PNG is 6.5 MB of RGBA that
    /// would otherwise stay resident for the row's whole life just to
    /// be drawn at 360 px.
    pub fn decode_to_preview(
        bytes: &[u8],
    ) -> Result<(slint::Image, u32, u32), app_core::images::ImageError> {
        let decoded = app_core::images::decode_bounded(bytes)?;
        let (w, h) = (decoded.width(), decoded.height());
        let rgba = if w.max(h) > PREVIEW_MAX_EDGE {
            decoded
                .thumbnail(PREVIEW_MAX_EDGE, PREVIEW_MAX_EDGE)
                .to_rgba8()
        } else {
            decoded.to_rgba8()
        };
        let (tw, th) = (rgba.width(), rgba.height());
        Ok((rgba_image(rgba.as_raw(), tw, th), w, h))
    }
}
