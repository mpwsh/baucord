// ui/mod.rs
pub mod chat;
pub mod connect;
pub mod messages;
pub mod screenshare;
pub mod settings;
pub mod sidebar;
pub mod splash;
pub mod update;
pub mod viewer;
use fltk::prelude::*;

/// Set the application icon on a window. Shared across all screens.
pub fn set_window_icon(wind: &mut fltk::window::Window) {
    if let Ok(icon) =
        fltk::image::PngImage::from_data(include_bytes!("../../../../assets/icons/icon.png"))
    {
        wind.set_icon(Some(icon));
    }
}
