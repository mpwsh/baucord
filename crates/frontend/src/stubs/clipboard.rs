//! `clipboard` without a desktop clipboard crate: nothing to read,
//! writes dropped with a log line. Slint's Android backend has its
//! own clipboard; the `Platform` trait routes to it later.

pub fn clipboard_file() -> Option<std::path::PathBuf> {
    None
}

pub fn clipboard_png() -> Option<Vec<u8>> {
    None
}

pub fn clipboard_text() -> Option<String> {
    None
}

pub fn set_clipboard_text(_text: &str) {
    log::info!("no clipboard on this host");
}

pub fn set_clipboard_image(_bytes: &[u8]) {
    log::info!("no clipboard on this host");
}
