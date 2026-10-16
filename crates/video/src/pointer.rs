//! The sharer's mouse pointer, drawn into a captured frame.
//!
//! Desktop Duplication hands the desktop over without the pointer (it
//! is a hardware overlay) and describes it beside the picture: where
//! it is, and a bitmap in one of three kinds. A share showed no pointer
//! at all (review P2-25), which is most of what a viewer follows when
//! someone points at something. `dxgi.rs` reads the description; this
//! is the drawing, with no Windows in it, so the three kinds and the
//! clipping are tested where the tests run.
//!
//! The frame is tightly packed BGRA, top-down; so is a colour pointer.

/// How a pointer's bitmap is to be read: the three kinds
/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One bit a pixel, two masks one above the other: AND, then XOR.
    /// AND 0 draws black or white (XOR 0 or 1); AND 1 leaves the screen
    /// (XOR 0) or inverts it (XOR 1) — the text caret.
    Monochrome,
    /// BGRA with alpha: blended over the screen.
    Color,
    /// BGRA whose fourth byte is a mask, not alpha: 0 replaces the
    /// screen pixel with the colour, anything else XORs the colour into
    /// it.
    MaskedColor,
}

/// A rectangle of a frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The largest pointer taken seriously, a side. Windows' largest
/// accessibility pointer is 256; anything far beyond is not a pointer.
const MAX_SIDE: u32 = 1024;

/// A pointer's bitmap, checked once so that drawing cannot read past
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    kind: Kind,
    width: u32,
    /// Of the picture as drawn (a monochrome buffer is twice as tall).
    height: u32,
    /// Bytes from one row to the next.
    pitch: usize,
    data: Vec<u8>,
}

impl Shape {
    /// From the platform's description: `height` and `pitch` as it
    /// reports them (for [`Kind::Monochrome`] the height of both masks
    /// together). `None` for a description that does not fit its own
    /// buffer, which is then not drawn at all.
    #[must_use]
    pub fn new(kind: Kind, width: u32, height: u32, pitch: u32, data: &[u8]) -> Option<Self> {
        let pitch = pitch as usize;
        let (drawn_height, row_bytes) = match kind {
            Kind::Monochrome => (height / 2, (width as usize).div_ceil(8)),
            Kind::Color | Kind::MaskedColor => (height, width as usize * 4),
        };
        let rows = match kind {
            Kind::Monochrome => drawn_height as usize * 2,
            Kind::Color | Kind::MaskedColor => drawn_height as usize,
        };
        if width == 0
            || drawn_height == 0
            || width > MAX_SIDE
            || drawn_height > MAX_SIDE
            || pitch < row_bytes
            // The last row need not be padded out to the pitch.
            || data.len() < pitch * (rows - 1) + row_bytes
        {
            return None;
        }
        Some(Self {
            kind,
            width,
            height: drawn_height,
            pitch,
            data: data.to_vec(),
        })
    }

    /// Draw the pointer with its top-left corner at (`x`, `y`) — which
    /// may be off the frame on any side — into a `frame_w` × `frame_h`
    /// BGRA frame. Returns the part of the frame it covers, or `None`
    /// if that is nothing (or the frame is not the size it says).
    pub fn draw(
        &self,
        frame: &mut [u8],
        frame_w: u32,
        frame_h: u32,
        x: i32,
        y: i32,
    ) -> Option<Rect> {
        if frame.len() < frame_w as usize * frame_h as usize * 4 {
            return None;
        }
        // The part of the pointer that lands on the frame, in the
        // frame's coordinates and in the pointer's own.
        let covered = self.covered(frame_w, frame_h, x, y)?;
        let (left, top) = (covered.x as usize, covered.y as usize);
        let (right, bottom) = (left + covered.width as usize, top + covered.height as usize);
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // left ≥ x, top ≥ y
        let (skip_x, skip_y) = (
            (i64::from(covered.x) - i64::from(x)) as usize,
            (i64::from(covered.y) - i64::from(y)) as usize,
        );

        let stride = frame_w as usize * 4;
        for row in 0..bottom - top {
            let sy = skip_y + row;
            let line =
                &mut frame[(top + row) * stride + left * 4..(top + row) * stride + right * 4];
            for (col, px) in line.chunks_exact_mut(4).enumerate() {
                let sx = skip_x + col;
                match self.kind {
                    Kind::Color => {
                        let s = &self.data[sy * self.pitch + sx * 4..][..4];
                        blend(px, s);
                    }
                    Kind::MaskedColor => {
                        let s = &self.data[sy * self.pitch + sx * 4..][..4];
                        if s[3] == 0 {
                            px[..3].copy_from_slice(&s[..3]);
                        } else {
                            px[0] ^= s[0];
                            px[1] ^= s[1];
                            px[2] ^= s[2];
                        }
                    }
                    Kind::Monochrome => {
                        let and = self.bit(sx, sy);
                        let xor = self.bit(sx, sy + self.height as usize);
                        match (and, xor) {
                            (false, false) => px[..3].fill(0),
                            (false, true) => px[..3].fill(255),
                            (true, false) => {}
                            (true, true) => {
                                px[0] = !px[0];
                                px[1] = !px[1];
                                px[2] = !px[2];
                            }
                        }
                    }
                }
            }
        }
        Some(covered)
    }

    /// [`Self::draw`], keeping what the pointer covers first: a pointer
    /// that moves on over an unchanged desktop is drawn again into the
    /// same frame, with the desktop put back where it was ([`Under`])
    /// — instead of reading the whole desktop out once more (review
    /// 2026-10, P3-35).
    pub fn draw_keeping(
        &self,
        frame: &mut [u8],
        frame_w: u32,
        frame_h: u32,
        x: i32,
        y: i32,
    ) -> Option<Under> {
        if frame.len() < frame_w as usize * frame_h as usize * 4 {
            return None;
        }
        // Where it lands, from the drawing's own clipping: drawn into
        // nothing, it says so.
        let rect = self.covered(frame_w, frame_h, x, y)?;
        let stride = frame_w as usize * 4;
        let (left, width) = (rect.x as usize * 4, rect.width as usize * 4);
        let mut pixels = Vec::with_capacity(width * rect.height as usize);
        for row in rect.y as usize..(rect.y + rect.height) as usize {
            pixels.extend_from_slice(&frame[row * stride + left..][..width]);
        }
        self.draw(frame, frame_w, frame_h, x, y);
        Some(Under { rect, pixels })
    }

    /// The part of a `frame_w` × `frame_h` frame the pointer covers with
    /// its top-left corner at (`x`, `y`): [`Self::draw`]'s clipping.
    fn covered(&self, frame_w: u32, frame_h: u32, x: i32, y: i32) -> Option<Rect> {
        let (x, y) = (i64::from(x), i64::from(y));
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + i64::from(self.width)).min(i64::from(frame_w));
        let bottom = (y + i64::from(self.height)).min(i64::from(frame_h));
        if left >= right || top >= bottom {
            return None;
        }
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // clamped above
        Some(Rect {
            x: left as u32,
            y: top as u32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        })
    }

    /// The pointer for a frame read back at half size
    /// (`dxgi.rs` halves a large desktop on the GPU): every other pixel
    /// of every other row, of the same kind.
    #[must_use]
    pub fn halved(&self) -> Self {
        let (width, height) = (self.width.div_ceil(2), self.height.div_ceil(2));
        let (w, h) = (width as usize, height as usize);
        match self.kind {
            Kind::Color | Kind::MaskedColor => {
                let mut data = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    for x in 0..w {
                        data.extend_from_slice(&self.data[2 * y * self.pitch + 2 * x * 4..][..4]);
                    }
                }
                Self {
                    kind: self.kind,
                    width,
                    height,
                    pitch: w * 4,
                    data,
                }
            }
            Kind::Monochrome => {
                let pitch = w.div_ceil(8);
                let mut data = vec![0u8; pitch * h * 2];
                // The AND mask, then the XOR mask under it.
                for (mask, from) in [(0, 0), (h, self.height as usize)] {
                    for y in 0..h {
                        for x in 0..w {
                            if self.bit(2 * x, from + 2 * y) {
                                data[(mask + y) * pitch + x / 8] |= 0x80 >> (x % 8);
                            }
                        }
                    }
                }
                Self {
                    kind: Kind::Monochrome,
                    width,
                    height,
                    pitch,
                    data,
                }
            }
        }
    }

    /// One bit of a monochrome mask: rows of bytes, the leftmost pixel
    /// in the highest bit.
    fn bit(&self, x: usize, row: usize) -> bool {
        self.data[row * self.pitch + x / 8] & (0x80 >> (x % 8)) != 0
    }
}

/// The desktop under a drawn pointer, as it was before the drawing
/// ([`Shape::draw_keeping`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Under {
    /// The part of the frame the pointer covers.
    pub rect: Rect,
    /// Its pixels, row after row.
    pixels: Vec<u8>,
}

impl Under {
    /// Put the desktop back into the `frame_w`-wide frame it was taken
    /// from. A frame of another size is left alone.
    pub fn restore(&self, frame: &mut [u8], frame_w: u32) {
        let r = self.rect;
        let stride = frame_w as usize * 4;
        let width = r.width as usize * 4;
        if r.x + r.width > frame_w || frame.len() < (r.y + r.height) as usize * stride {
            return;
        }
        for (i, row) in self.pixels.chunks_exact(width).enumerate() {
            let at = (r.y as usize + i) * stride + r.x as usize * 4;
            frame[at..at + width].copy_from_slice(row);
        }
    }
}

/// `src` (BGRA, straight alpha) over `dst`; the frame's own fourth byte
/// is left as the desktop had it.
fn blend(dst: &mut [u8], src: &[u8]) {
    let a = u32::from(src[3]);
    match a {
        0 => {}
        255 => dst[..3].copy_from_slice(&src[..3]),
        _ => {
            for c in 0..3 {
                let mixed = u32::from(src[c]) * a + u32::from(dst[c]) * (255 - a);
                #[allow(clippy::cast_possible_truncation)] // ≤ 255 by construction
                {
                    dst[c] = ((mixed + 127) / 255) as u8;
                }
            }
        }
    }
}

/// Whether two BGRA frames of `width` × `height` are the same picture
/// outside `except`. A frame with the pointer drawn in is compared with
/// a screenshot that never has one: the pointer is not a change.
#[must_use]
pub fn same_outside(a: &[u8], b: &[u8], width: u32, height: u32, except: Option<Rect>) -> bool {
    let stride = width as usize * 4;
    if a.len() != b.len() || a.len() != stride * height as usize {
        return false;
    }
    let Some(r) = except else {
        return a == b;
    };
    let (x0, x1) = (
        (r.x as usize).min(width as usize) * 4,
        (r.x as usize + r.width as usize).min(width as usize) * 4,
    );
    let (y0, y1) = (r.y as usize, r.y as usize + r.height as usize);
    let rows_above = y0.min(height as usize) * stride;
    let rows_from = y1.min(height as usize) * stride;
    // Whole rows above and below in one compare each; the rows the
    // pointer is on, left and right of it.
    if a[..rows_above] != b[..rows_above] || a[rows_from..] != b[rows_from..] {
        return false;
    }
    a[rows_above..rows_from]
        .chunks_exact(stride)
        .zip(b[rows_above..rows_from].chunks_exact(stride))
        .all(|(ra, rb)| ra[..x0] == rb[..x0] && ra[x1..] == rb[x1..])
}

/// A `width` × `height` BGRA frame at half the size, each pixel the
/// average of the four it covers (rounded): what the GPU makes of a
/// desktop it halves (`dxgi.rs`), give or take a step of rounding, for
/// a screenshot to be compared with.
#[must_use]
pub fn halve(frame: &[u8], width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (hw, hh) = (w / 2, h / 2);
    let mut out = Vec::with_capacity(hw * hh * 4);
    if frame.len() < w * h * 4 {
        return out;
    }
    let stride = w * 4;
    for y in 0..hh {
        let (top, bottom) = (&frame[2 * y * stride..], &frame[(2 * y + 1) * stride..]);
        for x in 0..hw {
            for c in 0..4 {
                let at = 2 * x * 4 + c;
                let sum = u16::from(top[at])
                    + u16::from(top[at + 4])
                    + u16::from(bottom[at])
                    + u16::from(bottom[at + 4]);
                #[allow(clippy::cast_possible_truncation)] // ≤ 255
                out.push(((sum + 2) / 4) as u8);
            }
        }
    }
    out
}

/// [`same_outside`], allowing each byte to be off by `tolerance`: a
/// screenshot halved on the CPU against a desktop halved on the GPU
/// round differently.
#[must_use]
pub fn close_outside(
    a: &[u8],
    b: &[u8],
    width: u32,
    height: u32,
    except: Option<Rect>,
    tolerance: u8,
) -> bool {
    let stride = width as usize * 4;
    if a.len() != b.len() || a.len() != stride * height as usize {
        return false;
    }
    let close = |ra: &[u8], rb: &[u8]| ra.iter().zip(rb).all(|(x, y)| x.abs_diff(*y) <= tolerance);
    a.chunks_exact(stride)
        .zip(b.chunks_exact(stride))
        .enumerate()
        .all(|(y, (ra, rb))| match except {
            Some(r) if (r.y..r.y + r.height).contains(&(y as u32)) => {
                let x0 = (r.x as usize).min(width as usize) * 4;
                let x1 = (r.x as usize + r.width as usize).min(width as usize) * 4;
                close(&ra[..x0], &rb[..x0]) && close(&ra[x1..], &rb[x1..])
            }
            _ => close(ra, rb),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w` × `h` frame of one colour.
    fn frame(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    fn px(frame: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * w + x) * 4) as usize;
        frame[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn a_halved_pointer_keeps_every_other_pixel() {
        // 3 × 2 colour: columns 0 and 2 of row 0 survive.
        let data: Vec<u8> = (0..24).collect();
        let half = Shape::new(Kind::Color, 3, 2, 12, &data).unwrap().halved();
        assert_eq!((half.width, half.height, half.pitch), (2, 1, 8));
        assert_eq!(half.data, [0, 1, 2, 3, 8, 9, 10, 11]);
        // 4 × 2 monochrome: AND 1010/0000, XOR 0110/0000 → 11 and 01.
        let mono = [0b1010_0000, 0, 0b0110_0000, 0];
        let half = Shape::new(Kind::Monochrome, 4, 4, 1, &mono)
            .unwrap()
            .halved();
        assert_eq!((half.width, half.height, half.pitch), (2, 1, 1));
        assert_eq!(half.data, [0b1100_0000, 0b0100_0000]);
        // It is still a shape that draws.
        let mut f = frame(2, 1, [9, 9, 9, 255]);
        assert!(half.draw(&mut f, 2, 1, 0, 0).is_some());
    }

    #[test]
    fn halving_averages_and_compares_within_rounding() {
        // 4 × 2 → 2 × 1.
        let mut f = Vec::new();
        for v in [0u8, 10, 20, 30, 1, 11, 21, 31] {
            f.extend_from_slice(&[v, v, v, 255]);
        }
        let half = halve(&f, 4, 2);
        assert_eq!(half, [6, 6, 6, 255, 26, 26, 26, 255]);
        let mut gpu = half.clone();
        gpu[0] = 5; // rounded the other way
        assert!(close_outside(&half, &gpu, 2, 1, None, 2));
        gpu[4] = 40; // a change
        assert!(!close_outside(&half, &gpu, 2, 1, None, 2));
        // …unless it is under the pointer.
        let pointer = Rect {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        };
        assert!(close_outside(&half, &gpu, 2, 1, Some(pointer), 2));
    }

    #[test]
    fn a_kept_desktop_is_put_back() {
        let data = [0, 0, 255, 255].repeat(9);
        let shape = Shape::new(Kind::Color, 3, 3, 12, &data).unwrap();
        // A frame with something in every pixel, so a wrong restore shows.
        let clean: Vec<u8> = (0..6 * 4 * 4).map(|i| i as u8).collect();
        let mut f = clean.clone();
        // Hanging off the right and bottom edges.
        let under = shape.draw_keeping(&mut f, 6, 4, 4, 2).unwrap();
        assert_eq!(
            under.rect,
            Rect {
                x: 4,
                y: 2,
                width: 2,
                height: 2
            }
        );
        assert_ne!(f, clean);
        under.restore(&mut f, 6);
        assert_eq!(f, clean);
        // Off the frame: nothing drawn, nothing kept.
        assert!(shape.draw_keeping(&mut f, 6, 4, 6, 0).is_none());
        assert!(shape.draw_keeping(&mut f, 6, 4, -3, 0).is_none());
        // Drawn again somewhere else after the restore: the same as a
        // fresh drawing there.
        let mut fresh = clean;
        shape.draw(&mut fresh, 6, 4, -1, -1);
        let under = shape.draw_keeping(&mut f, 6, 4, -1, -1).unwrap();
        assert_eq!(f, fresh);
        // A frame it does not fit is not written.
        let mut narrow = vec![7u8; 4 * 4];
        under.restore(&mut narrow, 1);
        assert_eq!(narrow, vec![7u8; 16]);
    }

    #[test]
    fn a_colour_pointer_is_blended_by_its_alpha() {
        // 2 × 1: opaque red, then half-transparent white.
        let data = [0, 0, 255, 255, 255, 255, 255, 128];
        let shape = Shape::new(Kind::Color, 2, 1, 8, &data).unwrap();
        let mut f = frame(4, 2, [0, 0, 0, 255]);
        assert_eq!(
            shape.draw(&mut f, 4, 2, 1, 1),
            Some(Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 1
            })
        );
        assert_eq!(px(&f, 4, 1, 1), [0, 0, 255, 255]);
        assert_eq!(px(&f, 4, 2, 1), [128, 128, 128, 255]);
        // Nothing else was touched.
        assert_eq!(px(&f, 4, 0, 1), [0, 0, 0, 255]);
        assert_eq!(px(&f, 4, 3, 1), [0, 0, 0, 255]);
        assert_eq!(px(&f, 4, 1, 0), [0, 0, 0, 255]);
        // A clear pixel leaves the screen as it is.
        let clear = Shape::new(Kind::Color, 1, 1, 4, &[9, 9, 9, 0]).unwrap();
        let mut f = frame(1, 1, [1, 2, 3, 255]);
        clear.draw(&mut f, 1, 1, 0, 0);
        assert_eq!(f, [1, 2, 3, 255]);
    }

    #[test]
    fn a_masked_pointer_replaces_or_inverts() {
        // Mask 0: the colour. Mask 255 with white: the screen inverted.
        // Mask 255 with black: the screen untouched.
        let data = [10, 20, 30, 0, 255, 255, 255, 255, 0, 0, 0, 255];
        let shape = Shape::new(Kind::MaskedColor, 3, 1, 12, &data).unwrap();
        let mut f = frame(3, 1, [1, 2, 3, 255]);
        shape.draw(&mut f, 3, 1, 0, 0);
        assert_eq!(px(&f, 3, 0, 0), [10, 20, 30, 255]);
        assert_eq!(px(&f, 3, 1, 0), [254, 253, 252, 255]);
        assert_eq!(px(&f, 3, 2, 0), [1, 2, 3, 255]);
    }

    #[test]
    fn a_monochrome_pointer_reads_both_masks() {
        // 4 × 1, a byte a row: AND 0011, XOR 0101 →
        // black, white, screen, inverted.
        let data = [0b0011_0000, 0b0101_0000];
        let shape = Shape::new(Kind::Monochrome, 4, 2, 1, &data).unwrap();
        let mut f = frame(4, 1, [10, 20, 30, 255]);
        shape.draw(&mut f, 4, 1, 0, 0);
        assert_eq!(px(&f, 4, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&f, 4, 1, 0), [255, 255, 255, 255]);
        assert_eq!(px(&f, 4, 2, 0), [10, 20, 30, 255]);
        assert_eq!(px(&f, 4, 3, 0), [245, 235, 225, 255]);
    }

    #[test]
    fn a_pointer_over_the_edge_is_cut_there() {
        // 3 × 3 opaque white, hanging off each corner of a 4 × 4 frame.
        let data = [255u8; 36];
        let shape = Shape::new(Kind::Color, 3, 3, 12, &data).unwrap();
        for (x, y, rect) in [
            (
                -2,
                -2,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            ),
            (
                3,
                -1,
                Rect {
                    x: 3,
                    y: 0,
                    width: 1,
                    height: 2,
                },
            ),
            (
                -1,
                2,
                Rect {
                    x: 0,
                    y: 2,
                    width: 2,
                    height: 2,
                },
            ),
            (
                2,
                2,
                Rect {
                    x: 2,
                    y: 2,
                    width: 2,
                    height: 2,
                },
            ),
        ] {
            let mut f = frame(4, 4, [0, 0, 0, 255]);
            assert_eq!(shape.draw(&mut f, 4, 4, x, y), Some(rect), "at {x},{y}");
            let white = f.chunks_exact(4).filter(|p| p[0] == 255).count();
            assert_eq!(white as u32, rect.width * rect.height, "at {x},{y}");
        }
        // Wholly off it, on every side: nothing drawn, nothing touched.
        for (x, y) in [
            (-3, 0),
            (4, 0),
            (0, -3),
            (0, 4),
            (i32::MIN, 0),
            (0, i32::MAX),
        ] {
            let mut f = frame(4, 4, [0, 0, 0, 255]);
            assert_eq!(shape.draw(&mut f, 4, 4, x, y), None, "at {x},{y}");
            assert_eq!(f, frame(4, 4, [0, 0, 0, 255]));
        }
        // A frame shorter than it says is left alone.
        let mut short = vec![0u8; 15];
        assert_eq!(shape.draw(&mut short, 2, 2, 0, 0), None);
    }

    #[test]
    fn a_description_that_does_not_fit_its_buffer_is_refused() {
        assert!(Shape::new(Kind::Color, 2, 2, 8, &[0; 16]).is_some());
        // Padded rows, the last one not padded out.
        assert!(Shape::new(Kind::Color, 2, 2, 12, &[0; 20]).is_some());
        assert!(Shape::new(Kind::Color, 2, 2, 8, &[0; 15]).is_none());
        assert!(Shape::new(Kind::Color, 2, 2, 4, &[0; 16]).is_none());
        assert!(Shape::new(Kind::Color, 0, 2, 8, &[0; 16]).is_none());
        assert!(Shape::new(Kind::Color, 5000, 1, 20000, &vec![0; 20000]).is_none());
        // Monochrome: 9 wide needs two bytes a row, and both masks.
        assert!(Shape::new(Kind::Monochrome, 9, 4, 2, &[0; 8]).is_some());
        assert!(Shape::new(Kind::Monochrome, 9, 4, 1, &[0; 8]).is_none());
        assert!(Shape::new(Kind::Monochrome, 9, 4, 2, &[0; 7]).is_none());
        assert!(Shape::new(Kind::Monochrome, 8, 1, 1, &[0; 1]).is_none());
    }

    #[test]
    fn the_pointer_is_not_a_change() {
        let clean = frame(6, 5, [7, 7, 7, 255]);
        let mut with = clean.clone();
        let shape = Shape::new(Kind::Color, 2, 2, 8, &[255; 16]).unwrap();
        let rect = shape.draw(&mut with, 6, 5, 3, 1).unwrap();
        assert!(!same_outside(&with, &clean, 6, 5, None));
        assert!(same_outside(&with, &clean, 6, 5, Some(rect)));
        // A change anywhere else is one: above, below, left, right.
        for (x, y) in [(0, 0), (5, 4), (2, 1), (5, 2)] {
            let mut moved = clean.clone();
            moved[((y * 6 + x) * 4) as usize] = 0;
            assert!(!same_outside(&with, &moved, 6, 5, Some(rect)), "at {x},{y}");
        }
        // Another size is another picture.
        assert!(!same_outside(&with, &clean[4..], 6, 5, Some(rect)));
        assert!(same_outside(&clean, &clean, 6, 5, None));
    }
}
