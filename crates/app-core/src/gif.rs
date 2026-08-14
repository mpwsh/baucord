//! GIF playback support.
//!
//! Slint's `Image` has no animation, so the shell drives GIFs itself:
//! decode the frames once (bounded — the wire pipeline already capped
//! animated GIFs at 800px / the byte budget), then swap the row's image
//! on a `slint::Timer` using each frame's own delay.
//!
//! Autoplay policy is unchanged from the FLTK build: play ~3 loops
//! (from [`loop_secs`]) but never more than 30 s, freeze to the first
//! frame, replay on hover.

use crate::images::{gif_gct_len, skip_gif_subblocks, ImageError};

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

/// Decode every frame of an animated GIF to RGBA. `image`'s GIF decoder
/// composites disposal/partial frames onto the logical screen, so each
/// returned frame is complete — exactly what an `Image` swap needs.
///
/// # Errors
/// [`ImageError::Codec`] on undecodable input.
pub fn decode_animation(bytes: &[u8]) -> Result<GifAnimation, ImageError> {
    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;

    let decoder = GifDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|e| ImageError::Codec(e.to_string()))?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|e| ImageError::Codec(e.to_string()))?;

    let (mut width, mut height) = (0u32, 0u32);
    let mut out = Vec::with_capacity(frames.len());
    for frame in frames {
        let (num, den) = frame.delay().numer_denom_ms();
        // Browser convention (matched by Fl_Anim_GIF too): a zero delay
        // plays as ~100 ms.
        let ms = if den == 0 { 100 } else { num / den };
        let delay_ms = if ms == 0 { 100 } else { ms };
        let buf = frame.into_buffer();
        width = buf.width();
        height = buf.height();
        out.push(GifFrame {
            rgba: buf.into_raw(),
            delay_ms,
        });
    }
    Ok(GifAnimation {
        width,
        height,
        frames: out,
    })
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
