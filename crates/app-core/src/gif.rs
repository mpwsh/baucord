//! GIF playback support.
//!
//! Slint's `Image` has no animation, so the shell drives GIFs itself:
//! decode the frames once, then swap the row's image on a
//! `slint::Timer` using each frame's own delay.
//!
//! The wire pipeline caps animated GIFs at 800px / the byte budget,
//! but neither caps the *frame count* — and every frame decodes to a
//! full logical-screen RGBA buffer. A legitimate 1 MB, 480px,
//! 150-frame GIF is ~140 MB of frames and seconds of synchronous
//! decode on every receiving client at once (which also stalls each
//! client's event loop long enough for the server to start
//! retransmitting at them). [`decode_animation`] therefore enforces
//! its own ceilings — frame count, total RGBA bytes, and wall-clock
//! decode time — and simply truncates: the animation loops over the
//! decoded prefix instead of the whole file.
//!
//! Autoplay policy is unchanged from the FLTK build: play ~3 loops
//! (from [`loop_secs`]) but never more than 30 s, freeze to the first
//! frame, replay on hover.

use std::{rc::Rc, time::Instant};

use crate::images::{gif_gct_len, skip_gif_subblocks, ImageError};

/// Most frames a decoded animation may hold. At a typical 100 ms/frame
/// this is ~24 s of loop — beyond any chat reaction.
pub const MAX_GIF_FRAMES: usize = 240;

/// Total decoded RGBA budget across all frames. 48 MB ≈ 52 frames at
/// the 800px wire cap, or 150+ frames of a typical 320–480px GIF.
pub const MAX_GIF_RGBA_BYTES: usize = 48 * 1024 * 1024;

/// Wall-clock budget for one animation decode. This runs on the UI
/// thread when a GIF arrives (or is hovered), and every connected
/// client decodes the same GIF at the same moment — a long stall here
/// freezes the whole room and stops the network thread's consumer,
/// which is exactly the failure this bounds.
pub const MAX_GIF_DECODE_MS: u64 = 150;

/// One decoded frame: packed RGBA at the GIF's logical screen size.
pub struct GifFrame {
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}

/// A fully decoded animation.
pub struct GifAnimation {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<GifFrame>,
}

impl GifAnimation {
    /// Autoplay budget: ~3 loops, capped at 30 s — a duration misread
    /// (or a pathologically long GIF) must not turn into minutes of
    /// animation. Hover still replays after the freeze.
    #[must_use]
    pub fn autoplay_budget_secs(bytes: &[u8]) -> f64 {
        (3.0 * loop_secs(bytes) + 0.25).min(30.0)
    }
}

/// Decode an animated GIF to RGBA frames, within hard ceilings.
///
/// `image`'s GIF decoder composites disposal/partial frames onto the
/// logical screen, so each returned frame is complete — exactly what an
/// `Image` swap needs. Frames are pulled *incrementally* and decoding
/// stops at [`MAX_GIF_FRAMES`], [`MAX_GIF_RGBA_BYTES`], or
/// [`MAX_GIF_DECODE_MS`], whichever comes first: the animation then
/// loops over the decoded prefix. A decode error after at least one
/// good frame also just ends the animation early rather than dropping
/// the GIF entirely.
///
/// # Errors
/// [`ImageError::Codec`] on undecodable input (no frame decoded).
pub fn decode_animation(bytes: &[u8]) -> Result<GifAnimation, ImageError> {
    use image::{codecs::gif::GifDecoder, AnimationDecoder, ImageDecoder};

    let mut decoder = GifDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|e| ImageError::Codec(e.to_string()))?;
    // Same bomb limits as the still-image path: dimension and
    // per-allocation caps inside the codec itself.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    decoder
        .set_limits(limits)
        .map_err(|e| ImageError::Codec(e.to_string()))?;

    let started = Instant::now();
    let (mut width, mut height) = (0u32, 0u32);
    let mut out = Vec::new();
    let mut rgba_total = 0usize;

    for frame in decoder.into_frames() {
        let frame = match frame {
            Ok(frame) => frame,
            // Mid-stream decode error: play what we have.
            Err(e) if !out.is_empty() => {
                log::warn!("GIF frame {} undecodable, truncating: {e}", out.len());
                break;
            }
            Err(e) => return Err(ImageError::Codec(e.to_string())),
        };
        let (num, den) = frame.delay().numer_denom_ms();
        // Browser convention (matched by Fl_Anim_GIF too): a zero delay
        // plays as ~100 ms.
        let ms = num.checked_div(den).unwrap_or(100);
        let delay_ms = if ms == 0 { 100 } else { ms };
        let buf = frame.into_buffer();
        width = buf.width();
        height = buf.height();
        let rgba = buf.into_raw();
        rgba_total += rgba.len();
        out.push(GifFrame { rgba, delay_ms });

        if out.len() >= MAX_GIF_FRAMES
            || rgba_total >= MAX_GIF_RGBA_BYTES
            || started.elapsed().as_millis() >= u128::from(MAX_GIF_DECODE_MS)
        {
            log::debug!(
                "GIF decode truncated at {} frames / {} MB / {:?}",
                out.len(),
                rgba_total / 1_048_576,
                started.elapsed(),
            );
            break;
        }
    }
    Ok(GifAnimation {
        width,
        height,
        frames: out,
    })
}

/// `Cursor` needs `AsRef<[u8]>`; this lets one refcounted buffer back
/// any number of sequential decoders without copying the compressed
/// bytes per playback.
struct RcBytes(Rc<Vec<u8>>);
impl AsRef<[u8]> for RcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A GIF being decoded *frame by frame* during playback, instead of
/// all frames upfront: memory is one frame at a time (a playing 800px
/// GIF holds ~2.5 MB instead of the 48 MB budget), at the cost of
/// ~1–4 ms of decode per frame advance — comfortably inside a 33 ms
/// animation tick. Looping reopens the decoder over the same
/// refcounted bytes.
pub struct GifStream {
    bytes: Rc<Vec<u8>>,
    frames: image::Frames<'static>,
    width: u32,
    height: u32,
}

impl GifStream {
    /// Open a stream over encoded GIF bytes. Applies the same codec
    /// bomb limits as every other decode path.
    ///
    /// # Errors
    /// [`ImageError::Codec`] when the header/limits are refused.
    pub fn open(bytes: Rc<Vec<u8>>) -> Result<Self, ImageError> {
        let frames = Self::reopen(&bytes)?;
        Ok(Self {
            bytes,
            frames,
            width: 0,
            height: 0,
        })
    }

    fn reopen(bytes: &Rc<Vec<u8>>) -> Result<image::Frames<'static>, ImageError> {
        use image::{codecs::gif::GifDecoder, AnimationDecoder, ImageDecoder};

        let mut decoder = GifDecoder::new(std::io::Cursor::new(RcBytes(Rc::clone(bytes))))
            .map_err(|e| ImageError::Codec(e.to_string()))?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        limits.max_alloc = Some(512 * 1024 * 1024);
        decoder
            .set_limits(limits)
            .map_err(|e| ImageError::Codec(e.to_string()))?;
        Ok(decoder.into_frames())
    }

    /// Decode the next frame, composited to the full logical screen.
    /// `None` at end of stream — and on a mid-stream decode error,
    /// which truncates playback the same way [`decode_animation`]
    /// truncates its prefix.
    pub fn next_frame(&mut self) -> Option<GifFrame> {
        let frame = match self.frames.next()? {
            Ok(frame) => frame,
            Err(e) => {
                log::warn!("GIF stream frame undecodable, ending loop early: {e}");
                return None;
            }
        };
        let (num, den) = frame.delay().numer_denom_ms();
        let ms = num.checked_div(den).unwrap_or(100);
        let delay_ms = if ms == 0 { 100 } else { ms };
        let buf = frame.into_buffer();
        self.width = buf.width();
        self.height = buf.height();
        Some(GifFrame {
            rgba: buf.into_raw(),
            delay_ms,
        })
    }

    /// Restart from frame 0 for the next loop. `false` when the bytes
    /// no longer open (shouldn't happen — they opened once).
    pub fn rewind(&mut self) -> bool {
        match Self::reopen(&self.bytes) {
            Ok(frames) => {
                self.frames = frames;
                true
            }
            Err(e) => {
                log::warn!("GIF stream rewind failed: {e}");
                false
            }
        }
    }

    /// Logical screen size, known after the first decoded frame
    /// (`(0, 0)` before that).
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Total duration of one animation loop, from the GIF's own Graphic
/// Control Extension delays (centiseconds) — no pixel decoding. Delays
/// of 0 render as ~100 ms everywhere (browser convention), so they
/// count as such; small real delays play as-is (clamping them up made
/// the 3-loop budget 3–5× too long for fast GIFs). Falls back to 3 s
/// if the parse loses sync.
#[must_use]
pub fn loop_secs(bytes: &[u8]) -> f64 {
    let mut i = 13 + gif_gct_len(bytes);
    let mut total_cs: u64 = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x21 => {
                let is_gce = bytes.get(i + 1) == Some(&0xF9);
                if is_gce && i + 4 < bytes.len() {
                    let d = u64::from(u16::from_le_bytes([bytes[i + 3], bytes[i + 4]]));
                    total_cs += if d == 0 { 10 } else { d };
                }
                i += 2;
                i = skip_gif_subblocks(bytes, i);
            }
            0x2C => {
                if i + 9 >= bytes.len() {
                    break;
                }
                let flags = bytes[i + 9];
                let lct = if flags & 0x80 != 0 {
                    3usize << ((flags & 0x07) + 1)
                } else {
                    0
                };
                i += 10 + lct + 1;
                i = skip_gif_subblocks(bytes, i);
            }
            // 0x3B trailer, or lost sync — either way the walk is done.
            _ => break,
        }
    }
    if total_cs == 0 {
        3.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            total_cs as f64 / 100.0
        }
    }
}

#[cfg(test)]
mod decode_tests {
    use image::{codecs::gif::GifEncoder, Delay, Frame, RgbaImage};

    use super::*;

    /// Encode `n` frames of `size`×`size` — alternating solid colors, so
    /// LZW crushes them and the file stays far under the 1 MB wire
    /// budget while the *decoded* form is n·size²·4 bytes.
    fn synth_gif(size: u32, n: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut bytes);
            enc.set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            for i in 0..n {
                let c = if i % 2 == 0 {
                    [40, 40, 60, 255]
                } else {
                    [200, 120, 40, 255]
                };
                let img = RgbaImage::from_pixel(size, size, image::Rgba(c));
                let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(40, 1));
                enc.encode_frame(frame).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn small_gif_decodes_fully() {
        let bytes = synth_gif(64, 5);
        let anim = decode_animation(&bytes).expect("decodes");
        assert_eq!(anim.frames.len(), 5);
        assert_eq!((anim.width, anim.height), (64, 64));
        assert!(anim.frames.iter().all(|f| f.delay_ms == 40));
    }

    #[test]
    fn pathological_gif_is_truncated_within_budgets() {
        // 480×480 × 200 frames = ~184 MB decoded if unbounded — the
        // exact shape that took real clients to ~150 MB and froze every
        // event loop in the room at once. Wire size stays tiny (solid
        // frames), so every sender-side check passes.
        let bytes = synth_gif(480, 200);
        assert!(bytes.len() < 1024 * 1024, "must pass the wire budget");

        let anim = decode_animation(&bytes).expect("decodes");
        let total: usize = anim.frames.iter().map(|f| f.rgba.len()).sum();
        assert!(!anim.frames.is_empty(), "keeps a playable prefix");
        assert!(anim.frames.len() < 200, "must not decode every frame");
        assert!(
            total <= MAX_GIF_RGBA_BYTES + 480 * 480 * 4,
            "decoded bytes stay within budget (+ at most the frame that crossed it): {total}"
        );
        assert!(anim.frames.len() <= MAX_GIF_FRAMES);
    }

    #[test]
    fn frame_count_ceiling_holds_for_tiny_frames() {
        // Tiny frames never hit the byte or time budget — the frame
        // ceiling must still stop the decode.
        let bytes = synth_gif(16, 600);
        let anim = decode_animation(&bytes).expect("decodes");
        assert_eq!(anim.frames.len(), MAX_GIF_FRAMES);
    }

    #[test]
    fn stream_decodes_frame_by_frame_and_rewinds() {
        let bytes = std::rc::Rc::new(synth_gif(64, 5));
        let mut stream = GifStream::open(std::rc::Rc::clone(&bytes)).expect("opens");
        let mut n = 0;
        while let Some(frame) = stream.next_frame() {
            assert_eq!(frame.delay_ms, 40);
            assert_eq!(frame.rgba.len(), 64 * 64 * 4);
            n += 1;
        }
        assert_eq!(n, 5, "all frames stream out");
        assert_eq!(stream.dimensions(), (64, 64));
        assert!(stream.next_frame().is_none(), "exhausted until rewound");

        assert!(stream.rewind(), "rewind reopens");
        let mut again = 0;
        while stream.next_frame().is_some() {
            again += 1;
        }
        assert_eq!(again, 5, "full second loop after rewind");
    }

    #[test]
    fn stream_truncated_bytes_end_early_not_panic() {
        let full = synth_gif(64, 10);
        // Chop the file mid-stream: some frames must still come out,
        // then a clean end instead of an error or panic.
        let cut = std::rc::Rc::new(full[..full.len() * 2 / 3].to_vec());
        let mut stream = GifStream::open(cut).expect("header intact");
        let mut n = 0;
        while stream.next_frame().is_some() {
            n += 1;
            assert!(n <= 10, "cannot yield more than encoded");
        }
        assert!(n >= 1, "prefix frames still play");
        assert!(n < 10, "truncation must lose the tail");
    }
}
