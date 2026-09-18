// clipboard.rs — native clipboard readers for what FLTK can't see.
//
// FLTK's clipboard layer speaks exactly two languages: plain text and
// one bitmap flavor per platform (TIFF/PDF on macOS, CF_DIB on
// Windows). Real clipboards carry more:
//
//   - Copied *files* (Finder/Explorer) arrive as file URLs / CF_HDROP —
//     FLTK either can't see them or, on macOS, offers the file's *icon*
//     as the image, which is how a copied photo pastes as a PNG badge.
//   - Some sources (macOS screenshots, Chromium's "PNG" format) put
//     PNG-only data on the clipboard, invisible to FLTK's TIFF-side.
//
// This module answers two questions natively, in priority order used
// by the paste path: "is there a copied file?" then "is there PNG
// data?". Everything else falls back to FLTK's own paste.
//
// Linux needs no code here: file managers deliver copies/drops as
// file:// URIs through the text path, which chat.rs sniffs directly.

/// First file on the clipboard as a filesystem path, if any.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn clipboard_file() -> Option<std::path::PathBuf> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};

    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: extern statics from AppKit; reading a framework constant.
    let data = pb.dataForType(unsafe { NSPasteboardTypeFileURL })?;
    let url = String::from_utf8(data.to_vec()).ok()?;
    let path = app_core::images::file_url_to_path(url.trim())?;
    Some(path)
}

/// Raw PNG bytes on the clipboard, if any (covers PNG-only sources
/// FLTK's TIFF/PDF-side reader misses, e.g. macOS screenshot copies).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn clipboard_png() -> Option<Vec<u8>> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};

    let pb = NSPasteboard::generalPasteboard();
    // SAFETY: extern static from AppKit; reading a framework constant.
    let data = pb.dataForType(unsafe { NSPasteboardTypePNG })?;
    let bytes = data.to_vec();
    bytes
        .starts_with(&[0x89, b'P', b'N', b'G'])
        .then_some(bytes)
}

// The Windows readers are hand-declared like resources.rs: two
// syscall sequences, layouts from winuser.h/shellapi.h, no new deps.
// The workspace otherwise avoids `unsafe`; this is the deliberate
// exception alongside resources.rs.
/// First file on the clipboard (`CF_HDROP`), if any.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
#[must_use]
pub fn clipboard_file() -> Option<std::path::PathBuf> {
    use core::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;

    const CF_HDROP: u32 = 15;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn DragQueryFileW(hdrop: *mut c_void, index: u32, out: *mut u16, cch: u32) -> u32;
    }

    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 || OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        // From here on, every exit must CloseClipboard.
        let result = (|| {
            let hdrop = GetClipboardData(CF_HDROP);
            if hdrop.is_null() {
                return None;
            }
            let len = DragQueryFileW(hdrop, 0, std::ptr::null_mut(), 0);
            if len == 0 {
                return None;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let copied = DragQueryFileW(hdrop, 0, buf.as_mut_ptr(), len + 1);
            if copied == 0 {
                return None;
            }
            buf.truncate(copied as usize);
            Some(std::path::PathBuf::from(std::ffi::OsString::from_wide(
                &buf,
            )))
        })();
        CloseClipboard();
        result
    }
}

/// Raw PNG bytes on the clipboard via the registered "PNG" format
/// (written by Chromium-family browsers), if any.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
#[must_use]
pub fn clipboard_png() -> Option<Vec<u8>> {
    use core::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalLock(hmem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(hmem: *mut c_void) -> i32;
        fn GlobalSize(hmem: *mut c_void) -> usize;
    }

    // "PNG\0" as UTF-16.
    let name: [u16; 4] = [u16::from(b'P'), u16::from(b'N'), u16::from(b'G'), 0];

    unsafe {
        let format = RegisterClipboardFormatW(name.as_ptr());
        if format == 0
            || IsClipboardFormatAvailable(format) == 0
            || OpenClipboard(std::ptr::null_mut()) == 0
        {
            return None;
        }
        let result = (|| {
            let hmem = GetClipboardData(format);
            if hmem.is_null() {
                return None;
            }
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return None;
            }
            let size = GlobalSize(hmem);
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
            GlobalUnlock(hmem);
            bytes
                .starts_with(&[0x89, b'P', b'N', b'G'])
                .then_some(bytes)
        })();
        CloseClipboard();
        result
    }
}

/// Linux: nothing to do natively — file copies/drops arrive as
/// file:// URIs through FLTK's text paste, sniffed in chat.rs.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn clipboard_file() -> Option<std::path::PathBuf> {
    None
}

/// Linux: FLTK's X11 image paste already covers the common cases.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn clipboard_png() -> Option<Vec<u8>> {
    None
}

// ── Plain text (all platforms, via arboard) ─────────────────────────────

/// Read the clipboard as plain text.
#[must_use]
pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Put plain text on the clipboard (the "Copy message"/"Copy link"
/// menu actions).
pub fn set_clipboard_text(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new()
        && let Err(e) = cb.set_text(text)
    {
        log::warn!("Clipboard write failed: {e}");
    }
}

/// Put a picture on the clipboard as pixels (the "Copy image" menu
/// action): decoded here from the PNG/JPEG/GIF bytes the row holds
/// (a GIF's first frame), then handed to the OS as RGBA. Bounded by
/// the same decoder that renders chat images.
pub fn set_clipboard_image(bytes: &[u8]) {
    let decoded = match app_core::images::decode_bounded(bytes) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            log::warn!("Copy image: decode failed: {e:?}");
            return;
        }
    };
    let (width, height) = (decoded.width() as usize, decoded.height() as usize);
    let data = arboard::ImageData {
        width,
        height,
        bytes: std::borrow::Cow::Owned(decoded.into_raw()),
    };
    if let Ok(mut cb) = arboard::Clipboard::new()
        && let Err(e) = cb.set_image(data)
    {
        log::warn!("Clipboard image write failed: {e}");
    }
}
