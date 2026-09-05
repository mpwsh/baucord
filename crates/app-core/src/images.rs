//! The bounded image pipeline — paste/attach/drop → protocol-ready bytes.
//!
//! Ported from the FLTK build's `ui/chat.rs` helpers, with two changes:
//! the wire byte cap is a parameter (so this crate doesn't depend on
//! `client`; the shell passes `client::MAX_CHAT_IMAGE_BYTES`), and
//! failures are typed ([`ImageError`]) instead of pre-localized strings
//! — the shell maps them to fluent messages.

use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

/// First-pass bound for a pasted image's longest edge.
pub const MAX_IMAGE_DIM: u32 = 1280;

/// Dimension cap for *animated* GIFs, tighter than for stills: every
/// viewer that plays one decodes W×H×4 bytes per frame, so an 800px
/// 50-frame GIF is already ~128 MB of transient frame data.
pub const MAX_ANIM_GIF_DIM: u32 = 800;

/// Extensions the attach/paste/drop pipeline accepts.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];

/// Why an image couldn't be prepared for the wire.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    /// Animated GIF exceeds the wire byte budget (`.0` = budget in bytes).
    #[error("animated GIF is over the {0}-byte budget")]
    GifTooLarge(usize),
    /// Animated GIF exceeds [`MAX_ANIM_GIF_DIM`] on its longest edge.
    #[error("animated GIF is larger than {MAX_ANIM_GIF_DIM}px")]
    GifTooBig,
    /// Even after downscaling to the 320px floor the encoding is over
    /// budget (`.0` = resulting size in KB).
    #[error("image is too large to send ({0} KB after downscaling)")]
    TooLargeAfterDownscale(usize),
    /// Decode/encode failure (corrupt file, unsupported flavor, bomb
    /// limits hit, I/O).
    #[error("{0}")]
    Codec(String),
}

/// Encode to PNG, downscaling until the result fits `max_bytes`.
/// Shared by the clipboard-paste and attach-from-disk paths.
///
/// # Errors
/// [`ImageError::Codec`] on encode failure, or
/// [`ImageError::TooLargeAfterDownscale`] if even the 320px floor is
/// over budget.
pub fn encode_bounded_png(buf: &image::RgbImage, max_bytes: usize) -> Result<Vec<u8>, ImageError> {
    let mut bound = MAX_IMAGE_DIM;
    loop {
        let scaled = if buf.width().max(buf.height()) > bound {
            image::imageops::resize(
                buf,
                (buf.width() * bound / buf.width().max(buf.height())).max(1),
                (buf.height() * bound / buf.width().max(buf.height())).max(1),
                image::imageops::FilterType::Triangle,
            )
        } else {
            buf.clone()
        };

        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(scaled)
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| ImageError::Codec(e.to_string()))?;
        let bytes = out.into_inner();

        if bytes.len() <= max_bytes {
            return Ok(bytes);
        }
        if bound <= 320 {
            return Err(ImageError::TooLargeAfterDownscale(bytes.len() / 1024));
        }
        bound /= 2;
    }
}

/// Turn raw image bytes of any supported format into protocol-ready
/// bytes.
///
/// - Animated GIFs pass through untouched (animation can't survive a
///   PNG transcode) — enforced against `max_bytes` and the tighter
///   [`MAX_ANIM_GIF_DIM`], read from the GIF header without decoding.
/// - PNGs already inside both budgets pass through untouched.
/// - Everything else (JPEG/WebP/BMP/TIFF, oversized or single-frame
///   GIFs, oversized PNGs) decodes — behind explicit limits so a
///   crafted file can't OOM us — and goes through the bounded-PNG
///   pipeline. The dimension check matters even for small files:
///   receivers decode at full resolution.
///
/// # Errors
/// The GIF budget errors, or [`ImageError::Codec`] for anything the
/// decoder/encoder refuses.
pub fn load_image_bytes(bytes: Vec<u8>, max_bytes: usize) -> Result<Vec<u8>, ImageError> {
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        // Logical screen size sits at bytes 6..10 of the header,
        // u16 LE each — no decode needed.
        if bytes.len() >= 10 {
            let w = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
            let h = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
            if gif_is_animated(&bytes) {
                if bytes.len() > max_bytes {
                    return Err(ImageError::GifTooLarge(max_bytes));
                }
                if w.max(h) > MAX_ANIM_GIF_DIM {
                    return Err(ImageError::GifTooBig);
                }
                return Ok(bytes);
            }
        }
        // Single-frame (or unparseable) GIF: fall through to the
        // static pipeline below.
    }

    let format = image::guess_format(&bytes).ok();
    let is_png = format == Some(image::ImageFormat::Png);
    let is_jpeg = format == Some(image::ImageFormat::Jpeg);

    let decoded = decode_bounded(&bytes)?;

    // Already-encoded PNGs and JPEGs inside both budgets pass through
    // untouched — receivers sniff the magic and decode either.
    if (is_png || is_jpeg)
        && bytes.len() <= max_bytes
        && decoded.width().max(decoded.height()) <= MAX_IMAGE_DIM
    {
        return Ok(bytes);
    }

    // Photographs (anything that arrived as JPEG) re-encode as JPEG:
    // a photo transcoded to PNG balloons 5–10×, which used to turn a
    // 500 KB camera shot into a multi-MB upload that stalled voice for
    // everyone. Screenshots and everything else keep the sharp-edged
    // PNG path.
    if is_jpeg {
        encode_bounded_jpeg(&decoded.to_rgb8(), max_bytes)
    } else {
        encode_bounded_png(&decoded.to_rgb8(), max_bytes)
    }
}

/// [`encode_bounded_png`]'s photographic sibling: JPEG at quality 85,
/// halving dimensions until the budget fits. Only used for sources
/// that were already JPEG — lossy re-encoding is fair game there.
///
/// # Errors
/// [`ImageError::TooLargeAfterDownscale`] below 320 px, or
/// [`ImageError::Codec`] from the encoder.
pub fn encode_bounded_jpeg(buf: &image::RgbImage, max_bytes: usize) -> Result<Vec<u8>, ImageError> {
    let mut bound = MAX_IMAGE_DIM;
    loop {
        let scaled = if buf.width().max(buf.height()) > bound {
            image::imageops::resize(
                buf,
                (buf.width() * bound / buf.width().max(buf.height())).max(1),
                (buf.height() * bound / buf.width().max(buf.height())).max(1),
                image::imageops::FilterType::Triangle,
            )
        } else {
            buf.clone()
        };

        let mut out = Cursor::new(Vec::new());
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        image::DynamicImage::ImageRgb8(scaled)
            .write_with_encoder(encoder)
            .map_err(|e| ImageError::Codec(e.to_string()))?;
        let bytes = out.into_inner();

        if bytes.len() <= max_bytes {
            return Ok(bytes);
        }
        if bound <= 320 {
            return Err(ImageError::TooLargeAfterDownscale(bytes.len() / 1024));
        }
        bound /= 2;
    }
}

/// Decode with explicit limits so a crafted file (e.g. a 30000×30000
/// bomb that compresses to nothing) can't allocate gigabytes before
/// any of our dimension checks run.
///
/// # Errors
/// [`ImageError::Codec`] on unreadable/oversized input.
pub fn decode_bounded(bytes: &[u8]) -> Result<image::DynamicImage, ImageError> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);

    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::Codec(e.to_string()))?;
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| ImageError::Codec(e.to_string()))
}

/// Size of the profile avatar the client keeps and uploads (spec §7.1).
pub const AVATAR_SIDE: u32 = 128;

/// Turn any picked image into the avatar the profile keeps: decoded
/// with the usual bounds, center-cropped to a square, scaled to
/// [`AVATAR_SIDE`], encoded as PNG.
///
/// # Errors
/// The decode errors of [`decode_bounded`], or [`ImageError::Codec`]
/// if the PNG encoder fails.
pub fn avatar_png(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let decoded = decode_bounded(bytes)?;
    let square = decoded.resize_to_fill(
        AVATAR_SIDE,
        AVATAR_SIDE,
        image::imageops::FilterType::Lanczos3,
    );
    let mut out = Cursor::new(Vec::new());
    square
        .to_rgba8()
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| ImageError::Codec(e.to_string()))?;
    Ok(out.into_inner())
}

/// Does this GIF contain more than one image descriptor? Scans for the
/// second `0x2C` image-separator at block level — cheap and
/// false-positive-free compared to decoding. Conservative on parse
/// confusion: treats "can't tell" as animated so we never strip frames.
#[must_use]
pub fn gif_is_animated(bytes: &[u8]) -> bool {
    // Skip header (6) + logical screen descriptor (7).
    let Some(mut i) = 13usize.checked_add(gif_gct_len(bytes)) else {
        return true;
    };
    let mut images = 0u32;
    while i < bytes.len() {
        match bytes[i] {
            0x3B => return false, // trailer before a 2nd image
            0x2C => {
                images += 1;
                if images >= 2 {
                    return true;
                }
                // Image descriptor: 10 bytes + optional local color table.
                if i + 9 >= bytes.len() {
                    return true;
                }
                let flags = bytes[i + 9];
                let lct = if flags & 0x80 != 0 {
                    3usize << ((flags & 0x07) + 1)
                } else {
                    0
                };
                i += 10 + lct + 1; // + LZW min-code-size byte
                i = skip_gif_subblocks(bytes, i);
            }
            0x21 => {
                // Extension: label byte then sub-blocks.
                i += 2;
                i = skip_gif_subblocks(bytes, i);
            }
            _ => return true, // lost sync — assume animated
        }
    }
    images >= 2
}

/// Global color table length from the logical screen descriptor.
pub(crate) fn gif_gct_len(bytes: &[u8]) -> usize {
    match bytes.get(10) {
        Some(&flags) if flags & 0x80 != 0 => 3usize << ((flags & 0x07) + 1),
        _ => 0,
    }
}

/// Advance past a chain of GIF data sub-blocks (len byte + payload,
/// terminated by a zero-length block). Returns an out-of-range index
/// on truncation, which callers treat as "assume animated".
pub(crate) fn skip_gif_subblocks(bytes: &[u8], mut i: usize) -> usize {
    while let Some(&len) = bytes.get(i) {
        i += 1 + len as usize;
        if len == 0 {
            return i;
        }
    }
    bytes.len() + 1
}

// ── File paths from pastes and drops ────────────────────────────────────

/// Does this path look like an image we can load?
#[must_use]
pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// `file://` URI or plain path → filesystem path. Percent-decodes URIs
/// (X11 drops encode spaces as `%20`), rejects remote `file://host/`
/// forms, and only answers for paths that exist.
#[must_use]
pub fn file_url_to_path(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    let path = if let Some(rest) = s.strip_prefix("file://") {
        // Strip an authority component; only localhost/empty accepted.
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') {
            return None;
        }
        PathBuf::from(percent_decode(rest))
    } else if s.starts_with('/') || (s.len() > 2 && s.as_bytes()[1] == b':') {
        // Absolute POSIX path (macOS drops) or Windows drive path.
        PathBuf::from(s)
    } else {
        return None;
    };
    path.is_file().then_some(path)
}

/// Minimal %XX decoder for file URIs (UTF-8 paths).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First image file among newline-separated dropped/pasted paths or
/// URIs, if any. The single sniffer serving drag&drop on all three
/// desktop platforms and Linux clipboard file copies.
#[must_use]
pub fn first_image_path_in(text: &str) -> Option<PathBuf> {
    text.lines()
        .filter_map(file_url_to_path)
        .find(|p| is_image_path(p))
}

/// The pasted text as a single http(s) image URL, if that's what it
/// is: one token, no whitespace, an image extension on the URL path
/// (query/fragment ignored).
#[must_use]
pub fn image_url_in(text: &str) -> Option<String> {
    let t = text.trim();
    if !(t.starts_with("https://") || t.starts_with("http://")) || t.contains(char::is_whitespace) {
        return None;
    }
    let path = t.split(['?', '#']).next().unwrap_or(t);
    is_image_path(Path::new(path)).then(|| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_url_in_accepts_plain_png_url() {
        assert_eq!(
            image_url_in("https://x.test/cat.png"),
            Some("https://x.test/cat.png".into())
        );
    }

    #[test]
    fn image_url_in_ignores_query_when_matching_extension() {
        assert!(image_url_in("https://x.test/cat.gif?frame=2").is_some());
    }

    #[test]
    fn image_url_in_rejects_non_image_and_multi_token() {
        assert!(image_url_in("https://x.test/page.html").is_none());
        assert!(image_url_in("look https://x.test/cat.png").is_none());
    }

    #[test]
    fn is_image_path_is_case_insensitive() {
        assert!(is_image_path(Path::new("/tmp/A.PNG")));
        assert!(!is_image_path(Path::new("/tmp/a.txt")));
    }

    #[test]
    fn single_frame_gif_is_not_animated() {
        // Header + LSD (no GCT) + one image descriptor + trailer.
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&[1, 0, 1, 0, 0x00, 0, 0]); // LSD, no GCT
        g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x00]); // image desc
        g.push(0x02); // LZW min code size
        g.extend_from_slice(&[0x01, 0x00, 0x00]); // one data sub-block + terminator
        g.push(0x3B); // trailer
        assert!(!gif_is_animated(&g));
    }

    #[test]
    fn jpeg_within_budgets_passes_through_untouched() {
        let img = image::RgbImage::from_fn(640, 480, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 100])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .unwrap();
        let jpeg = out.into_inner();

        let wire = load_image_bytes(jpeg.clone(), 10 * 1024 * 1024).expect("accepted");
        assert_eq!(wire, jpeg, "small jpeg must not be transcoded");
        assert!(wire.starts_with(&[0xFF, 0xD8, 0xFF]), "still a jpeg");
    }

    #[test]
    fn oversized_jpeg_reencodes_as_jpeg_not_png() {
        // 4000 px photo-like input: over MAX_IMAGE_DIM, must come back
        // downscaled AND still JPEG — the whole point is that photos
        // never balloon through a PNG transcode again.
        let img = image::RgbImage::from_fn(4000, 3000, |x, y| {
            image::Rgb([
                (x * 7 % 256) as u8,
                (y * 3 % 256) as u8,
                ((x + y) % 256) as u8,
            ])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .unwrap();
        let jpeg = out.into_inner();

        let wire = load_image_bytes(jpeg, 10 * 1024 * 1024).expect("accepted");
        assert!(wire.starts_with(&[0xFF, 0xD8, 0xFF]), "photo stays jpeg");
        let back = decode_bounded(&wire).unwrap();
        assert!(back.width().max(back.height()) <= MAX_IMAGE_DIM);
    }
}
