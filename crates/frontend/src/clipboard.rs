// clipboard.rs — the system clipboard, for the paste chain
// (`media::paste_into_chat`) and the "Copy …" menu actions.
//
// Slint has no clipboard API of its own beyond a TextInput's text.
// What a clipboard holds differs by platform and by who copied:
//
//   - Copied *files* (Finder/Explorer) are file URLs / CF_HDROP; on
//     Linux they are file:// URIs in the text.
//   - Screenshots and browsers put PNG data there (macOS's PNG type,
//     Chromium's registered "PNG" format on Windows).
//   - PrintScreen and paint programs leave only a bitmap.
//
// The chain asks in that order: `clipboard_file`, `clipboard_png`
// (both native, per platform), the text, and last `clipboard_pixels`
// (arboard's decoded bitmap, every platform).

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

/// Raw PNG bytes on the clipboard, if any (PNG-only sources, e.g.
/// macOS screenshot copies).
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

/// Linux: nothing to do natively — file copies arrive as file:// URIs
/// in the clipboard's text, which the paste chain sniffs.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn clipboard_file() -> Option<std::path::PathBuf> {
    None
}

/// Linux: no raw-PNG reader; [`clipboard_pixels`] reads its pictures.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn clipboard_png() -> Option<Vec<u8>> {
    None
}

// ── A picture as pixels (all platforms, via arboard) ────────────────────

/// The clipboard's picture as `(width, height, RGBA)`, in whatever
/// flavour the system keeps it: the bitmap the Print Screen key and
/// paint programs leave on Windows, TIFF on macOS, any image target on
/// Linux
/// — what the PNG readers above do not see (review P2-18). The paste
/// chain asks only when the clipboard has no text, so a spreadsheet's
/// cells stay the text they also are.
#[must_use]
pub fn clipboard_pixels() -> Option<(usize, usize, Vec<u8>)> {
    let image = with_clipboard(arboard::Clipboard::get_image).ok()?;
    Some((image.width, image.height, image.bytes.into_owned()))
}

// ── Plain text (all platforms, via arboard) ─────────────────────────────

/// Read the clipboard as plain text.
#[must_use]
pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

thread_local! {
    /// One clipboard per thread (in practice the UI thread's), kept for
    /// the life of the app. On Linux the copied data is served by the
    /// app itself and goes away with the `Clipboard` that set it, unless
    /// a clipboard manager takes it over: a fresh one dropped right after
    /// `set_text` could lose the copy. Elsewhere it costs nothing.
    static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> =
        const { std::cell::RefCell::new(None) };
}

fn with_clipboard<R>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>,
) -> Result<R, String> {
    CLIPBOARD.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        let cb = slot.as_mut().expect("set above");
        f(cb).map_err(|e| e.to_string())
    })
}

/// Put plain text on the clipboard, saying whether it worked
/// (Settings → Debug's "Copy log" tells the user).
///
/// # Errors
/// The system's clipboard refused, as text.
pub fn copy_text(text: &str) -> Result<(), String> {
    with_clipboard(|cb| cb.set_text(text))
}

/// Put plain text on the clipboard (the "Copy message"/"Copy link"
/// menu actions).
pub fn set_clipboard_text(text: &str) {
    if let Err(e) = copy_text(text) {
        log::warn!("Clipboard write failed: {e}");
    }
}

/// Put a picture on the clipboard as pixels (the "Copy image" menu
/// action): decoded here from the PNG/JPEG/GIF bytes the row holds
/// (a GIF's first frame), then handed to the OS as RGBA. Bounded by
/// the same decoder that renders chat images.
pub fn set_clipboard_image(bytes: &[u8]) {
    let decoded = match app_core::images::decode_received(bytes) {
        Ok(img) => img.into_rgba8(),
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
    if let Err(e) = with_clipboard(|cb| cb.set_image(data)) {
        log::warn!("Clipboard image write failed: {e}");
    }
}
