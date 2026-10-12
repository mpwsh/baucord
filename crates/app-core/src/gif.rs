//! GIF playback support.
//!
//! Slint's `Image` has no animation, so the shell drives GIFs itself:
//! it swaps the image on a `slint::Timer`, using each frame's own
//! delay.
//!
//! The wire pipeline caps animated GIFs at 800px / the byte budget,
//! but neither caps the *frame count* — and every frame decodes to a
//! full logical-screen RGBA buffer. A legitimate 1 MB, 480px,
//! 150-frame GIF is ~140 MB of frames and seconds of synchronous
//! decode if taken whole. So nothing takes one whole: every player —
//! a chat row, the gallery, the send preview — pulls frames from a
//! [`GifStream`] as they are shown, and holds one at a time. (The
//! gallery and the preview used to decode up to 240 frames, 48 MB,
//! up front; review P2-59.)
//!
//! Autoplay policy is unchanged from the FLTK build: play ~3 loops
//! (from [`loop_secs`]) but never more than 30 s, freeze to the first
//! frame, replay on hover.

use std::rc::Rc;

use crate::images::{gif_gct_len, skip_gif_subblocks, ImageError};

/// One decoded frame: packed RGBA at the GIF's logical screen size.
pub struct GifFrame {
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}

/// Autoplay budget: ~3 loops, capped at 30 s — a duration misread (or
/// a pathologically long GIF) must not turn into minutes of animation.
/// Hover still replays after the freeze.
#[must_use]
pub fn autoplay_budget_secs(bytes: &[u8]) -> f64 {
    (3.0 * loop_secs(bytes) + 0.25).min(30.0)
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
    /// which ends the loop there: the frames before it still play.
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
                // 21 F9 04 <packed> <delay lo> <delay hi> <transparent> 00.
                // The delay used to be read one byte early — the packed
                // field as its low byte, the real low byte as its high
                // one — which made a 40 ms frame ten seconds and put
                // nearly every GIF's budget at the 30 s cap.
                if is_gce && i + 5 < bytes.len() {
                    let d = u64::from(u16::from_le_bytes([bytes[i + 4], bytes[i + 5]]));
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
    fn a_pathological_gif_costs_one_frame_at_a_time() {
        // 480×480 × 200 frames = ~184 MB if decoded whole — the shape
        // that took real clients to ~150 MB and froze every event loop
        // in the room at once. Wire size stays tiny (solid frames), so
        // every sender-side check passes. A stream hands out one frame
        // and keeps none.
        let bytes = synth_gif(480, 200);
        assert!(bytes.len() < 1024 * 1024, "must pass the wire budget");
        let mut stream = GifStream::open(std::rc::Rc::new(bytes)).expect("opens");
        for _ in 0..3 {
            let frame = stream.next_frame().expect("a frame");
            assert_eq!(frame.rgba.len(), 480 * 480 * 4);
        }
    }

    #[test]
    fn the_autoplay_budget_is_three_loops_and_never_over_thirty_seconds() {
        // 5 frames × 40 ms = 0.2 s a loop.
        let short = synth_gif(16, 5);
        assert!(
            (loop_secs(&short) - 0.2).abs() < 0.001,
            "a loop reads as {} s",
            loop_secs(&short)
        );
        assert!((autoplay_budget_secs(&short) - 0.85).abs() < 0.01);
        // 600 frames × 40 ms = 24 s a loop: three would be 72 s.
        let long = synth_gif(16, 600);
        assert!((autoplay_budget_secs(&long) - 30.0).abs() < f64::EPSILON);
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
