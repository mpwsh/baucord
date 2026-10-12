//! `clipboard` without a desktop clipboard crate (the phones): nothing
//! to read; text goes to the host's clipboard through its hooks
//! (`crate::host::copy_text` — Android's `ClipboardManager`, iOS's
//! general pasteboard), pictures are dropped with a log line.

pub fn clipboard_file() -> Option<std::path::PathBuf> {
    None
}

pub fn clipboard_png() -> Option<Vec<u8>> {
    None
}

pub fn clipboard_pixels() -> Option<(usize, usize, Vec<u8>)> {
    None
}

pub fn clipboard_text() -> Option<String> {
    None
}

/// Put plain text on the host's clipboard, saying whether it worked.
///
/// # Errors
/// The host's own message, or no host clipboard at all.
pub fn copy_text(text: &str) -> Result<(), String> {
    crate::host::copy_text(text)
}

pub fn set_clipboard_text(text: &str) {
    if let Err(e) = copy_text(text) {
        log::warn!("Clipboard write failed: {e}");
    }
}

pub fn set_clipboard_image(_bytes: &[u8]) {
    log::info!("no clipboard on this host");
}
