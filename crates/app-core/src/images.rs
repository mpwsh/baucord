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
pub(crate) const MAX_IMAGE_DIM: u32 = 1280;

/// Dimension cap for *animated* GIFs, tighter than for stills: every
/// viewer that plays one decodes W×H×4 bytes per frame, so an 800px
/// 50-frame GIF is already ~128 MB of transient frame data.
pub const MAX_ANIM_GIF_DIM: u32 = 800;

/// Extensions the attach/paste/drop pipeline accepts: BMP and TIFF on
/// a desktop only (`Cargo.toml`).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"];
/// Extensions the attach/paste/drop pipeline accepts.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

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
pub(crate) fn encode_bounded_png(
    buf: &image::RgbImage,
    max_bytes: usize,
) -> Result<Vec<u8>, ImageError> {
    encode_bounded(buf, max_bytes, |scaled, out| {
        scaled.write_to(out, image::ImageFormat::Png)
    })
}

/// The loop both encoders share: bound the longest edge, encode, and
/// halve the bound until the bytes fit or the 320 px floor is reached.
/// A picture already inside the bound is encoded where it is — it used
/// to be copied first, on every pass (review P3-41, P3-42).
fn encode_bounded(
    buf: &image::RgbImage,
    max_bytes: usize,
    encode: impl Fn(&image::RgbImage, &mut Cursor<Vec<u8>>) -> image::ImageResult<()>,
) -> Result<Vec<u8>, ImageError> {
    let longest = buf.width().max(buf.height());
    let mut bound = MAX_IMAGE_DIM;
    loop {
        let scaled = if longest > bound {
            std::borrow::Cow::Owned(image::imageops::resize(
                buf,
                (buf.width() * bound / longest).max(1),
                (buf.height() * bound / longest).max(1),
                image::imageops::FilterType::Triangle,
            ))
        } else {
            std::borrow::Cow::Borrowed(buf)
        };

        let mut out = Cursor::new(Vec::new());
        encode(&scaled, &mut out).map_err(|e| ImageError::Codec(e.to_string()))?;
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

    // Already-encoded PNGs and JPEGs inside both budgets pass through
    // untouched — receivers sniff the magic and decode either. Their
    // size is read from the header: every caller decodes the result for
    // the send preview next, which is where a broken file is found, so
    // decoding here as well was a second full decode only to measure
    // (review P3-41).
    // Not one that carries metadata, though: a camera's JPEG went out
    // with its EXIF — the GPS position included — to every member and
    // into history, and its orientation unapplied, so a portrait photo
    // arrived sideways (review 2026-10, P2-18). Re-encoded, it is drawn
    // the right way up and keeps nothing but the pixels.
    if (is_png || is_jpeg)
        && bytes.len() <= max_bytes
        && !carries_metadata(&bytes)
        && dimensions_of(&bytes).is_some_and(|(w, h)| w.max(h) <= MAX_IMAGE_DIM)
    {
        return Ok(bytes);
    }

    let decoded = decode_bounded(&bytes)?;

    // Photographs (anything that arrived as JPEG) re-encode as JPEG:
    // a photo transcoded to PNG balloons 5–10×, which used to turn a
    // 500 KB camera shot into a multi-MB upload that stalled voice for
    // everyone. Screenshots and everything else keep the sharp-edged
    // PNG path.
    if is_jpeg {
        encode_bounded_jpeg(&decoded.into_rgb8(), max_bytes)
    } else {
        encode_bounded_png(&decoded.into_rgb8(), max_bytes)
    }
}

/// Pixels with no file format around them — a system clipboard's
/// bitmap (`width` × `height`, RGBA, rows top to bottom) — into
/// protocol-ready bytes: straight into the bounded-PNG pipeline. Alpha
/// is dropped, as on every other road into it (and a clipboard bitmap's
/// is often all zeroes).
///
/// # Errors
/// [`ImageError::Codec`] when the buffer is not `width × height × 4`
/// bytes of something, or as [`encode_bounded_png`].
pub fn load_rgba(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    max_bytes: usize,
) -> Result<Vec<u8>, ImageError> {
    let pixels = u32::try_from(width)
        .ok()
        .zip(u32::try_from(height).ok())
        .filter(|(w, h)| *w > 0 && *h > 0)
        .and_then(|(w, h)| image::RgbaImage::from_raw(w, h, rgba))
        .ok_or_else(|| ImageError::Codec("not a picture".to_string()))?;
    encode_bounded_png(
        &image::DynamicImage::ImageRgba8(pixels).into_rgb8(),
        max_bytes,
    )
}

/// [`encode_bounded_png`]'s photographic sibling: JPEG at quality 85,
/// halving dimensions until the budget fits. Only used for sources
/// that were already JPEG — lossy re-encoding is fair game there.
///
/// # Errors
/// [`ImageError::TooLargeAfterDownscale`] below 320 px, or
/// [`ImageError::Codec`] from the encoder.
pub(crate) fn encode_bounded_jpeg(
    buf: &image::RgbImage,
    max_bytes: usize,
) -> Result<Vec<u8>, ImageError> {
    encode_bounded(buf, max_bytes, |scaled, out| {
        scaled.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(out, 85))
    })
}

/// The file extension a received picture's bytes are saved under: what
/// they are, not what most pictures are (a JPEG saved as `.png` opens
/// in a browser and nowhere else; review P3-31). PNG for anything the
/// sniff does not know — nothing else gets as far as a chat row.
#[must_use]
pub fn extension_for(bytes: &[u8]) -> &'static str {
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Jpeg) => "jpg",
        Ok(image::ImageFormat::Gif) => "gif",
        _ => "png",
    }
}

/// A picture's size from its header, without decoding it.
fn dimensions_of(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Decode with explicit limits so a crafted file (e.g. a 30000×30000
/// bomb that compresses to nothing) can't allocate gigabytes before
/// any of our dimension checks run.
///
/// Every compiled format is tried: this is for what the user picks,
/// pastes or drops. Bytes from the wire go through [`decode_received`].
///
/// # Errors
/// [`ImageError::Codec`] on unreadable/oversized input.
pub fn decode_bounded(bytes: &[u8]) -> Result<image::DynamicImage, ImageError> {
    use image::ImageDecoder as _;
    let codec = |e: image::ImageError| ImageError::Codec(e.to_string());
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::Codec(e.to_string()))?;
    reader.limits(decode_limits());
    let mut decoder = reader.into_decoder().map_err(codec)?;
    // The camera's orientation (EXIF), applied: the pixels are stored
    // as the sensor read them (P2-18).
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut picture = image::DynamicImage::from_decoder(decoder).map_err(codec)?;
    picture.apply_orientation(orientation);
    Ok(picture)
}

/// Whether a PNG or JPEG carries metadata beyond its pixels: EXIF
/// (a camera's, with the GPS position and the orientation), XMP or
/// IPTC. Those are re-encoded before they are sent ([`load_image_bytes`]).
#[must_use]
pub(crate) fn carries_metadata(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // JPEG segments up to the scan: APP1 holds EXIF or XMP, APP13
        // IPTC.
        let mut i = 2;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xFF {
                return false;
            }
            let marker = bytes[i + 1];
            match marker {
                0xFF => i += 1,
                0x01 | 0xD0..=0xD8 => i += 2,
                0xD9 | 0xDA => return false,
                0xE1 | 0xED => return true,
                _ => i += 2 + usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]])),
            }
        }
        false
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // PNG chunks before the pixels: eXIf, or XMP in an iTXt.
        let mut i = 8;
        while i + 8 <= bytes.len() {
            let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
            match &bytes[i + 4..i + 8] {
                b"eXIf" | b"iTXt" => return true,
                b"IDAT" | b"IEND" => return false,
                _ => i = i.saturating_add(12).saturating_add(len as usize),
            }
        }
        false
    } else {
        false
    }
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    limits
}

/// The format of a picture that came over the wire, by its magic
/// bytes: PNG, JPEG or GIF, the only ones a sender produces
/// ([`load_image_bytes`] passes those through and re-encodes the rest).
/// `None` for anything else.
#[must_use]
pub(crate) fn received_format(bytes: &[u8]) -> Option<image::ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(image::ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(image::ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(image::ImageFormat::Gif)
    } else {
        None
    }
}

/// The longest edge of a received picture that is decoded. Senders
/// make at most [`MAX_IMAGE_DIM`] (an animated GIF
/// [`MAX_ANIM_GIF_DIM`]); a picture attached as a file is sent as it
/// is, and a phone's 12 MP photo is 4032 px. A bigger one stays a file
/// card, to save.
pub const MAX_RECEIVED_DIM: u32 = 4096;

/// What decoding one received picture may allocate: a
/// [`MAX_RECEIVED_DIM`]² RGBA picture.
const MAX_RECEIVED_ALLOC: u64 = 64 * 1024 * 1024;

/// A progressive JPEG's decoder keeps every coefficient of the picture
/// (two bytes per sample) outside the image crate's count: at 4096² in
/// full colour that is 100 MB on top of the pixels. Received ones stop
/// at 8 MP (about 50 MB of coefficients); a camera writes baseline.
const MAX_RECEIVED_PROGRESSIVE_PIXELS: u64 = 1 << 23;

/// The limits for decoding bytes someone else sent: 16384² and 512 MB
/// (the local pipeline's) let a picture of a few KB freeze every client
/// that shows it, or get a phone killed (review 2026-10, P0-2).
fn received_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_RECEIVED_DIM);
    limits.max_image_height = Some(MAX_RECEIVED_DIM);
    limits.max_alloc = Some(MAX_RECEIVED_ALLOC);
    limits
}

/// The limits for playing a received animated GIF frame by frame
/// (`gif::GifStream`): its canvas at most 1024 px (senders stop at
/// [`MAX_ANIM_GIF_DIM`]), and what the player holds at once — the
/// canvas, the frame being decoded and the frame handed out — under
/// 16 MiB. Each frame's own rectangle is checked against both: a 1×1
/// canvas with an 11000² frame used to allocate 484 MB per frame.
pub(crate) fn received_gif_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(16 * 1024 * 1024);
    limits
}

/// [`decode_bounded`] for bytes someone else sent: chat pictures,
/// avatars, the gallery. Only PNG, JPEG and GIF reach a decoder, and
/// the one their magic names (review P1-20). A crafted WebP, BMP or
/// TIFF would otherwise reach those decoders on every receiver, and
/// history replays it to newcomers; with `panic = "abort"`, one
/// decoder panic would end every client that shows the picture. What
/// [`received_dimensions`] refuses is not decoded.
///
/// # Errors
/// [`ImageError::Codec`] for another format, a picture over the
/// received limits, or as [`decode_bounded`].
pub fn decode_received(bytes: &[u8]) -> Result<image::DynamicImage, ImageError> {
    let Some(format) = received_format(bytes) else {
        return Err(ImageError::Codec("not a PNG, JPEG or GIF picture".into()));
    };
    if received_dimensions(bytes).is_none() {
        return Err(ImageError::Codec("picture too large to show".into()));
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(received_limits());
    reader
        .decode()
        .map_err(|e| ImageError::Codec(e.to_string()))
}

/// A received picture's size from its header, without decoding it:
/// what a chat row lays out before its pixels are wanted. `None` for
/// anything [`decode_received`] would refuse by its magic, a header
/// that does not parse, or a picture over the received limits (longer
/// than [`MAX_RECEIVED_DIM`], or a progressive JPEG over 8 MP).
#[must_use]
pub fn received_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let format = received_format(bytes)?;
    let (w, h) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .ok()?;
    let progressive = format == image::ImageFormat::Jpeg && jpeg_is_progressive(bytes);
    received_size_fits(w, h, progressive).then_some((w, h))
}

/// Whether a received picture of `w`×`h` is decoded at all.
fn received_size_fits(w: u32, h: u32, progressive: bool) -> bool {
    w > 0
        && h > 0
        && w.max(h) <= MAX_RECEIVED_DIM
        && (!progressive || u64::from(w) * u64::from(h) <= MAX_RECEIVED_PROGRESSIVE_PIXELS)
}

/// Whether a JPEG's frame header is a progressive one (SOF2, SOF6,
/// SOF10 or SOF14), walking the markers before it. `false` for a
/// baseline JPEG, and for bytes that end or lose sync first.
fn jpeg_is_progressive(bytes: &[u8]) -> bool {
    let mut i = 2; // past SOI
    while i + 3 < bytes.len() {
        if bytes[i] != 0xFF {
            return false;
        }
        let marker = bytes[i + 1];
        match marker {
            // Fill bytes before a marker.
            0xFF => i += 1,
            // Markers without a length.
            0x01 | 0xD0..=0xD7 => i += 2,
            // A frame header: progressive or not.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return matches!(marker, 0xC2 | 0xC6 | 0xCA | 0xCE);
            }
            // The scan or the end before any frame header.
            0xDA | 0xD9 => return false,
            _ => {
                let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
                i += 2 + len;
            }
        }
    }
    false
}

/// Size of the profile avatar the client keeps and uploads (spec §7.1).
pub const AVATAR_SIDE: u32 = 128;

/// The longest edge of someone else's avatar that is decoded. Ours are
/// [`AVATAR_SIDE`]; a server takes up to 256 KB, enough for a picture
/// that unpacks to 4096² — every client would decode it for a 64 px
/// circle.
pub const MAX_RECEIVED_AVATAR_DIM: u32 = 512;

/// Someone else's avatar, as the rows draw it: decoded within the
/// received limits, refused over [`MAX_RECEIVED_AVATAR_DIM`], and
/// never more than [`AVATAR_SIDE`]² kept.
///
/// # Errors
/// [`ImageError::Codec`] for a picture over the limit, or as
/// [`decode_received`].
pub fn decode_avatar(bytes: &[u8]) -> Result<image::RgbaImage, ImageError> {
    let (w, h) =
        received_dimensions(bytes).ok_or_else(|| ImageError::Codec("not a picture".into()))?;
    if w.max(h) > MAX_RECEIVED_AVATAR_DIM {
        return Err(ImageError::Codec(format!("an avatar of {w}×{h}")));
    }
    let decoded = decode_received(bytes)?;
    Ok(if decoded.width().max(decoded.height()) > AVATAR_SIDE {
        decoded.thumbnail(AVATAR_SIDE, AVATAR_SIDE).into_rgba8()
    } else {
        decoded.into_rgba8()
    })
}

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
        .into_rgba8()
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

/// Minimal %XX decoder for file URIs (UTF-8 paths). Reads bytes, not
/// `str` slices: `%a` followed by a multi-byte character used to slice
/// through it and abort the app (PRODUCTION-PLAN CLI-10).
fn percent_decode(s: &str) -> String {
    let hex = |b: u8| char::from(b).to_digit(16);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(&[hi, lo]) = bytes.get(i + 1..i + 3)
            && let (Some(hi), Some(lo)) = (hex(hi), hex(lo))
            && let Ok(v) = u8::try_from(hi << 4 | lo)
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

    /// A JPEG of `w`×`h` with an EXIF APP1 saying `orientation`.
    fn jpeg_with_exif(w: u32, h: u32, orientation: u16) -> Vec<u8> {
        let picture = image::RgbImage::from_pixel(w, h, image::Rgb([200, 100, 50]));
        let mut plain = Cursor::new(Vec::new());
        picture
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut plain, 90,
            ))
            .expect("encode");
        let plain = plain.into_inner();
        // "Exif\0\0", a little-endian TIFF header, one IFD entry
        // (0x0112 Orientation, SHORT, 1), no next IFD.
        let mut exif = b"Exif\0\0II*\0".to_vec();
        exif.extend_from_slice(&8u32.to_le_bytes());
        exif.extend_from_slice(&1u16.to_le_bytes());
        exif.extend_from_slice(&0x0112u16.to_le_bytes());
        exif.extend_from_slice(&3u16.to_le_bytes());
        exif.extend_from_slice(&1u32.to_le_bytes());
        exif.extend_from_slice(&orientation.to_le_bytes());
        exif.extend_from_slice(&[0, 0]);
        exif.extend_from_slice(&0u32.to_le_bytes());
        let mut out = plain[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&exif);
        out.extend_from_slice(&plain[2..]);
        out
    }

    /// A photo with EXIF is not passed through: it goes out turned the
    /// way the camera says, and without its metadata (review 2026-10,
    /// P2-18). One without is passed through as before.
    #[test]
    fn a_photo_is_turned_upright_and_loses_its_exif() {
        let tagged = jpeg_with_exif(40, 20, 6);
        assert!(carries_metadata(&tagged));
        let sent = load_image_bytes(tagged.clone(), 1 << 20).expect("load");
        assert_ne!(sent, tagged, "re-encoded");
        assert!(!carries_metadata(&sent), "nothing but pixels");
        assert_eq!(dimensions_of(&sent), Some((20, 40)), "turned a quarter");

        let mut plain = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(40, 20, image::Rgb([1, 2, 3]))
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut plain, 90,
            ))
            .expect("encode");
        let plain = plain.into_inner();
        assert!(!carries_metadata(&plain));
        assert_eq!(
            load_image_bytes(plain.clone(), 1 << 20).expect("load"),
            plain
        );
    }

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
    fn clipboard_pixels_become_a_png_without_their_alpha() {
        // 2×1, both pixels fully transparent: the picture must still
        // show (a clipboard bitmap's alpha is often all zeroes).
        let rgba = vec![255, 0, 0, 0, 0, 0, 255, 0];
        let png = load_rgba(2, 1, rgba, 1 << 20).expect("encodes");
        let back = decode_bounded(&png).expect("decodes");
        assert_eq!((back.width(), back.height()), (2, 1));
        assert_eq!(back.to_rgba8().into_raw(), [255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn a_saved_picture_is_named_for_what_it_is() {
        assert_eq!(extension_for(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), "jpg");
        assert_eq!(extension_for(b"GIF89a\x01\x00\x01\x00"), "gif");
        assert_eq!(
            extension_for(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            "png"
        );
        assert_eq!(extension_for(b"?"), "png");
    }

    #[test]
    fn clipboard_pixels_of_the_wrong_size_are_refused() {
        assert!(load_rgba(2, 2, vec![0; 8], 1 << 20).is_err());
        assert!(load_rgba(0, 0, Vec::new(), 1 << 20).is_err());
    }

    #[test]
    fn a_big_clipboard_picture_is_scaled_into_the_bound() {
        let (w, h) = (2000usize, 500usize);
        let png = load_rgba(w, h, vec![40; w * h * 4], 1 << 20).expect("encodes");
        let back = decode_bounded(&png).expect("decodes");
        assert_eq!(back.width(), MAX_IMAGE_DIM);
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

    /// A 2×2 picture encoded as `format`.
    fn encoded(format: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, format)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn received_format_reads_only_png_jpeg_and_gif_magic() {
        use image::ImageFormat::{Gif, Jpeg, Png};
        assert_eq!(received_format(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Some(Png));
        assert_eq!(received_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Jpeg));
        assert_eq!(received_format(b"GIF87a\x01\0\x01\0"), Some(Gif));
        assert_eq!(received_format(b"GIF89a\x01\0\x01\0"), Some(Gif));
        // WebP, BMP, both TIFF byte orders, and cut-short headers.
        for other in [
            &b"RIFF\x1a\0\0\0WEBPVP8L"[..],
            b"BM\x3a\0\0\0\0\0",
            b"II*\0\x08\0\0\0",
            b"MM\0*\0\0\0\x08",
            b"\x89PNG",
            &[0xFF, 0xD8],
            b"GIF8",
            b"",
        ] {
            assert_eq!(received_format(other), None, "{other:?}");
        }
    }

    /// A GIF of a `screen` canvas whose one frame claims `frame`: two
    /// colours, one pixel of LZW data.
    fn gif_with(screen: (u16, u16), frame: (u16, u16)) -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&screen.0.to_le_bytes());
        g.extend_from_slice(&screen.1.to_le_bytes());
        g.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]); // a 2-colour table
        g.extend_from_slice(&[0x2C, 0, 0, 0, 0]);
        g.extend_from_slice(&frame.0.to_le_bytes());
        g.extend_from_slice(&frame.1.to_le_bytes());
        g.push(0);
        g.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00, 0x3B]);
        g
    }

    #[test]
    fn received_pictures_past_the_limits_are_not_decoded() {
        let png = |w, h| {
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        let edge = png(MAX_RECEIVED_DIM, 1);
        assert_eq!(received_dimensions(&edge), Some((MAX_RECEIVED_DIM, 1)));
        assert!(decode_received(&edge).is_ok());
        let over = png(MAX_RECEIVED_DIM + 1, 1);
        assert_eq!(received_dimensions(&over), None, "a file card, not a row");
        assert!(decode_received(&over).is_err());
        // A progressive JPEG stops at 8 MP; a baseline one at the edge.
        assert!(received_size_fits(4096, 2048, true));
        assert!(!received_size_fits(4096, 2049, true));
        assert!(received_size_fits(4096, 4096, false));
        assert!(!received_size_fits(0, 10, false));
    }

    #[test]
    fn an_avatar_is_kept_small_and_a_big_one_is_refused() {
        let png = |w, h| {
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        let ours = decode_avatar(&png(AVATAR_SIDE, AVATAR_SIDE)).expect("ours");
        assert_eq!(ours.dimensions(), (AVATAR_SIDE, AVATAR_SIDE));
        let bigger = decode_avatar(&png(MAX_RECEIVED_AVATAR_DIM, 256)).expect("at the limit");
        assert_eq!(bigger.dimensions(), (AVATAR_SIDE, AVATAR_SIDE / 2));
        assert!(decode_avatar(&png(MAX_RECEIVED_AVATAR_DIM + 1, 8)).is_err());
        assert!(decode_avatar(b"not a picture").is_err());
    }

    #[test]
    fn a_progressive_jpeg_is_told_from_a_baseline_one() {
        let baseline = encoded(image::ImageFormat::Jpeg);
        assert!(!jpeg_is_progressive(&baseline));
        let sof = baseline
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("a baseline frame header");
        let mut progressive = baseline.clone();
        progressive[sof + 1] = 0xC2;
        assert!(jpeg_is_progressive(&progressive));
        assert!(!jpeg_is_progressive(&baseline[..sof]), "cut before it");
    }

    #[test]
    fn a_gif_frame_larger_than_its_canvas_is_refused() {
        // A 1×1 canvas whose frame claims 11000²: 484 MB a frame under
        // the old limits, from a file of a few dozen bytes.
        let bomb = gif_with((1, 1), (11_000, 11_000));
        assert_eq!(received_dimensions(&bomb), Some((1, 1)));
        assert!(decode_received(&bomb).is_err());
        let mut stream = crate::gif::GifStream::open(bomb.into()).expect("the canvas opens");
        assert!(stream.next_frame().is_none(), "the frame ends the loop");
        // A canvas past what an animation may be does not open at all;
        // one inside it plays.
        let big = gif_with((2000, 2000), (1, 1));
        assert!(crate::gif::GifStream::open(big.into()).is_err());
        let small = gif_with((800, 800), (1, 1));
        let mut stream = crate::gif::GifStream::open(small.into()).expect("opens");
        assert!(stream.next_frame().is_some());
    }

    #[test]
    fn a_pasted_uri_with_a_broken_escape_decodes_without_a_panic() {
        assert_eq!(percent_decode("/tmp/a%20b"), "/tmp/a b");
        assert_eq!(percent_decode("/tmp/%a\u{e9}"), "/tmp/%a\u{e9}");
        assert_eq!(percent_decode("/tmp/%zz%4"), "/tmp/%zz%4");
        assert_eq!(percent_decode("/tmp/%C3%A9"), "/tmp/\u{e9}");
    }

    #[test]
    fn received_pictures_decode_only_as_png_jpeg_or_gif() {
        use image::ImageFormat::{Bmp, Gif, Jpeg, Png, Tiff, WebP};
        for format in [Png, Jpeg, Gif] {
            assert!(decode_received(&encoded(format)).is_ok(), "{format:?}");
        }
        for format in [WebP, Bmp, Tiff] {
            let bytes = encoded(format);
            // Still fine for a picture the user pastes or attaches…
            assert!(decode_bounded(&bytes).is_ok(), "{format:?}");
            // …never from the wire.
            assert!(decode_received(&bytes).is_err(), "{format:?}");
        }
        // The right magic on a broken body is an error, not a panic.
        assert!(decode_received(b"\x89PNG\r\n\x1a\n\0\0\0\0garbage").is_err());
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
