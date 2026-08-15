//! The baucord UI crate.
//!
//! `.slint` sources live in `slint/`; `build.rs` compiles them and this
//! crate re-exports the generated components (`MainWindow`,
//! `SettingsWindow`, `StreamViewerWindow`, `GalleryWindow`) plus the
//! `State` / `Logic` / `Tr` / `SettingsState` / `SettingsLogic`
//! globals. The [`convert`] module maps `app-core` view-model types
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
    use app_core::roster::{split_id, RosterRow};

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
                kind: 4,
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
            ChatItem::Link { url } => ChatRow {
                kind: 3,
                ts: "".into(),
                user: "".into(),
                body: url.as_str().into(),
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
}
