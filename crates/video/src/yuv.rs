//! YUV 4:2:0 → RGBA for decoders that hand back raw planes.
//!
//! `FFmpeg` converts through swscale; Android's `MediaCodec` (see
//! `mediacodec.rs`) returns the picture as it sits in the codec's output
//! buffer — three planes at vendor-chosen offsets and strides, described
//! either by a `MediaImage2` record (`image-data`) or by the classic
//! `color-format` + `stride` + `slice-height` trio. [`Yuv420Layout`]
//! names the common shape of both and [`to_rgba`] walks it.
//!
//! `VideoToolbox` (`videotoolbox.rs`, iOS) is asked for BGRA instead —
//! it converts on the GPU with the stream's own colour signalling — and
//! [`bgra_to_rgba`] only swaps two bytes per pixel and drops the row
//! padding.
//!
//! Pure code, no platform dependencies: the unit tests run on the host.

use std::fmt::Write as _;

/// One plane of a frame inside a decoder's output buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    /// Byte offset of the plane's first sample from the frame's start.
    pub offset: usize,
    /// Bytes between the starts of two rows.
    pub row_stride: usize,
    /// Bytes between two horizontally adjacent samples (1 planar, 2
    /// interleaved chroma as in NV12).
    pub pixel_stride: usize,
}

/// Where the Y, U and V samples of a 4:2:0 frame live in one buffer,
/// plus the visible rectangle (decoders pad to macroblock multiples:
/// 1080 lines come back as 1088 with `crop` saying which to show).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Yuv420Layout {
    pub y: Plane,
    pub u: Plane,
    pub v: Plane,
    /// Top-left of the visible rectangle, in luma pixels (even).
    pub crop_x: u32,
    pub crop_y: u32,
    /// Visible size — the [`to_rgba`] output's dimensions.
    pub width: u32,
    pub height: u32,
}

/// Byte size of Android's `MediaImage2` record (`image-data`).
pub const MEDIA_IMAGE2_LEN: usize = 104;

impl Yuv420Layout {
    /// I420: Y plane, then U, then V, each `stride`/`stride/2` wide and
    /// `slice_height`/`slice_height/2` rows tall
    /// (Android `COLOR_FormatYUV420Planar` = 19).
    #[must_use]
    pub fn planar(width: u32, height: u32, stride: usize, slice_height: usize) -> Self {
        let y_len = stride * slice_height;
        let c_stride = stride / 2;
        let c_len = c_stride * slice_height.div_ceil(2);
        Self {
            y: Plane {
                offset: 0,
                row_stride: stride,
                pixel_stride: 1,
            },
            u: Plane {
                offset: y_len,
                row_stride: c_stride,
                pixel_stride: 1,
            },
            v: Plane {
                offset: y_len + c_len,
                row_stride: c_stride,
                pixel_stride: 1,
            },
            crop_x: 0,
            crop_y: 0,
            width,
            height,
        }
    }

    /// NV12: Y plane, then interleaved U/V pairs at the same stride
    /// (Android `COLOR_FormatYUV420SemiPlanar` = 21).
    #[must_use]
    pub fn semi_planar(width: u32, height: u32, stride: usize, slice_height: usize) -> Self {
        let y_len = stride * slice_height;
        Self {
            y: Plane {
                offset: 0,
                row_stride: stride,
                pixel_stride: 1,
            },
            u: Plane {
                offset: y_len,
                row_stride: stride,
                pixel_stride: 2,
            },
            v: Plane {
                offset: y_len + 1,
                row_stride: stride,
                pixel_stride: 2,
            },
            crop_x: 0,
            crop_y: 0,
            width,
            height,
        }
    }

    /// Android's `MediaImage2` (the `image-data` entry of a
    /// `MediaCodec` output format), native-endian:
    ///
    /// ```text
    /// u32 type (1 = YUV)   u32 num_planes   u32 width   u32 height
    /// u32 bit_depth        u32 bit_depth_allocated
    /// 4 × { u32 offset  i32 col_inc  i32 row_inc  u32 horiz_sub  u32 vert_sub }
    /// ```
    ///
    /// The visible rectangle is the whole image; the caller applies the
    /// format's `crop` with [`Self::with_crop`].
    ///
    /// # Errors
    /// Too short, not 8-bit 4:2:0 YUV in three planes, or negative
    /// strides — each named in the message.
    pub fn from_media_image2(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < MEDIA_IMAGE2_LEN {
            return Err(format!(
                "image-data is {} bytes, MediaImage2 needs {MEDIA_IMAGE2_LEN}",
                bytes.len()
            ));
        }
        // Length checked above, so the slices never miss.
        let u32_at = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap_or([0; 4]));
        let i32_at = |i: usize| i32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap_or([0; 4]));
        let kind = u32_at(0);
        let planes = u32_at(4);
        let width = u32_at(8);
        let height = u32_at(12);
        let bit_depth = u32_at(16);
        let bit_depth_allocated = u32_at(20);
        if kind != 1 {
            return Err(format!("MediaImage2 type {kind} is not YUV"));
        }
        if planes != 3 {
            return Err(format!("MediaImage2 has {planes} planes, expected 3"));
        }
        if bit_depth != 8 || bit_depth_allocated != 8 {
            return Err(format!(
                "MediaImage2 is {bit_depth}-bit in {bit_depth_allocated}-bit samples, expected 8/8"
            ));
        }
        if width == 0 || height == 0 {
            return Err(format!("MediaImage2 is {width}x{height}"));
        }
        let plane = |n: usize, want_sub: u32| -> Result<Plane, String> {
            let base = 24 + n * 20;
            let offset = u32_at(base);
            let col_inc = i32_at(base + 4);
            let row_inc = i32_at(base + 8);
            let horiz = u32_at(base + 12);
            let vert = u32_at(base + 16);
            if col_inc <= 0 || row_inc <= 0 {
                return Err(format!(
                    "MediaImage2 plane {n} has col_inc {col_inc} / row_inc {row_inc} (negative strides unsupported)"
                ));
            }
            if horiz != want_sub || vert != want_sub {
                return Err(format!(
                    "MediaImage2 plane {n} subsampling {horiz}x{vert}, expected {want_sub}x{want_sub} (not 4:2:0)"
                ));
            }
            Ok(Plane {
                offset: offset as usize,
                row_stride: row_inc as usize,
                pixel_stride: col_inc as usize,
            })
        };
        Ok(Self {
            y: plane(0, 1)?,
            u: plane(1, 2)?,
            v: plane(2, 2)?,
            crop_x: 0,
            crop_y: 0,
            width,
            height,
        })
    }

    /// Restrict the visible rectangle. Odd offsets are rounded down to
    /// keep chroma aligned; a rectangle outside the image is clamped.
    #[must_use]
    pub fn with_crop(mut self, x: u32, y: u32, width: u32, height: u32) -> Self {
        let full_w = self.crop_x + self.width;
        let full_h = self.crop_y + self.height;
        let x = (x & !1).min(full_w);
        let y = (y & !1).min(full_h);
        self.crop_x = x;
        self.crop_y = y;
        self.width = width.min(full_w - x);
        self.height = height.min(full_h - y);
        self
    }

    /// Bytes a buffer must hold for every sample this layout reads.
    #[must_use]
    pub fn required_len(&self) -> usize {
        if self.width == 0 || self.height == 0 {
            return 0;
        }
        let last = |p: &Plane, last_row: usize, last_col: usize| {
            p.offset + last_row * p.row_stride + last_col * p.pixel_stride + 1
        };
        let (x0, y0) = (self.crop_x as usize, self.crop_y as usize);
        let (w, h) = (self.width as usize, self.height as usize);
        let luma = last(&self.y, y0 + h - 1, x0 + w - 1);
        // Chroma rows/cols cover ceil(n / 2) samples.
        let c_row = (y0 + h).div_ceil(2) - 1;
        let c_col = (x0 + w).div_ceil(2) - 1;
        luma.max(last(&self.u, c_row, c_col))
            .max(last(&self.v, c_row, c_col))
    }
}

/// `Y'CbCr` → `R'G'B'` coefficient set (the colour primaries' luma weights).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    /// What baucord's encoders signal in the VUI, and what any HD
    /// decoder assumes when nothing is signalled.
    #[default]
    Bt709,
    Bt2020,
}

/// Sample range: studio swing (Y 16..=235, C 16..=240) or full 0..=255.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Range {
    #[default]
    Limited,
    Full,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorSpec {
    pub matrix: Matrix,
    pub range: Range,
}

/// Fixed-point (Q14) conversion constants for one [`ColorSpec`].
#[derive(Clone, Copy, Debug)]
struct Coeffs {
    y_off: i32,
    cy: i32,
    crv: i32,
    cgu: i32,
    cgv: i32,
    cbu: i32,
}

const Q: i32 = 14;
const ROUND: i32 = 1 << (Q - 1);

impl Coeffs {
    fn new(spec: ColorSpec) -> Self {
        let (kr, kb) = match spec.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        // R = Y + 2(1-Kr)·Cr ; B = Y + 2(1-Kb)·Cb ;
        // G = Y − 2Kb(1-Kb)/Kg·Cb − 2Kr(1-Kr)/Kg·Cr
        let crv = 2.0 * (1.0 - kr);
        let cbu = 2.0 * (1.0 - kb);
        let cgu = -2.0 * kb * (1.0 - kb) / kg;
        let cgv = -2.0 * kr * (1.0 - kr) / kg;
        let (y_off, y_scale, c_scale) = match spec.range {
            Range::Limited => (16, 255.0 / 219.0, 255.0 / 224.0),
            Range::Full => (0, 1.0, 1.0),
        };
        let q = f64::from(1 << Q);
        let fix = |x: f64| (x * q).round() as i32;
        Self {
            y_off,
            cy: fix(y_scale),
            crv: fix(crv * c_scale),
            cgu: fix(cgu * c_scale),
            cgv: fix(cgv * c_scale),
            cbu: fix(cbu * c_scale),
        }
    }

    #[inline]
    #[allow(clippy::many_single_char_names)]
    fn px(&self, y: u8, u: u8, v: u8) -> [u8; 4] {
        let y = (i32::from(y) - self.y_off) * self.cy + ROUND;
        let u = i32::from(u) - 128;
        let v = i32::from(v) - 128;
        let r = (y + self.crv * v) >> Q;
        let g = (y + self.cgu * u + self.cgv * v) >> Q;
        let b = (y + self.cbu * u) >> Q;
        [
            r.clamp(0, 255) as u8,
            g.clamp(0, 255) as u8,
            b.clamp(0, 255) as u8,
            255,
        ]
    }
}

/// Convert the visible rectangle of `src` (laid out as `layout` says)
/// into tightly packed RGBA in `out`, which is resized to
/// `width × height × 4`.
///
/// # Errors
/// The buffer is too small for the layout (or a stride is zero) — an
/// error, not a panic, since the layout came from a vendor codec.
pub fn to_rgba(
    src: &[u8],
    layout: &Yuv420Layout,
    spec: ColorSpec,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let need = layout.required_len();
    if src.len() < need {
        return Err(format!(
            "{}x{} frame needs {need} bytes in its layout, decoder gave {}",
            layout.width,
            layout.height,
            src.len()
        ));
    }
    if layout.y.pixel_stride == 0 || layout.u.pixel_stride == 0 || layout.v.pixel_stride == 0 {
        return Err("zero pixel stride".into());
    }
    let width = layout.width as usize;
    let height = layout.height as usize;
    out.clear();
    out.resize(width * height * 4, 0);
    if width == 0 || height == 0 {
        return Ok(());
    }
    let co = Coeffs::new(spec);
    let (x0, y0) = (layout.crop_x as usize, layout.crop_y as usize);
    let chroma_cols = width.div_ceil(2);

    for (row, dst) in out.chunks_exact_mut(width * 4).enumerate() {
        let py = y0 + row;
        let pc = py / 2;
        let y_start = layout.y.offset + py * layout.y.row_stride + x0 * layout.y.pixel_stride;
        let u_start = layout.u.offset + pc * layout.u.row_stride + (x0 / 2) * layout.u.pixel_stride;
        let v_start = layout.v.offset + pc * layout.v.row_stride + (x0 / 2) * layout.v.pixel_stride;
        // Exactly the samples this row reads; required_len() proved
        // they exist.
        let y_row = &src[y_start..=y_start + (width - 1) * layout.y.pixel_stride];
        let u_row = &src[u_start..=u_start + (chroma_cols - 1) * layout.u.pixel_stride];
        let v_row = &src[v_start..=v_start + (chroma_cols - 1) * layout.v.pixel_stride];
        convert_row(
            &co,
            y_row,
            layout.y.pixel_stride,
            u_row,
            layout.u.pixel_stride,
            v_row,
            layout.v.pixel_stride,
            dst,
        );
    }
    Ok(())
}

/// One output row: two luma samples share each chroma pair.
#[allow(clippy::too_many_arguments)]
#[inline]
fn convert_row(
    co: &Coeffs,
    y_row: &[u8],
    y_step: usize,
    u_row: &[u8],
    u_step: usize,
    v_row: &[u8],
    v_step: usize,
    dst: &mut [u8],
) {
    let mut ys = y_row.iter().step_by(y_step);
    let us = u_row.iter().step_by(u_step);
    let vs = v_row.iter().step_by(v_step);
    let mut pairs = dst.chunks_exact_mut(8);
    for ((&u, &v), px) in us.zip(vs).zip(pairs.by_ref()) {
        let (Some(&y0), Some(&y1)) = (ys.next(), ys.next()) else {
            break;
        };
        px[..4].copy_from_slice(&co.px(y0, u, v));
        px[4..].copy_from_slice(&co.px(y1, u, v));
    }
    // Odd width: the last pixel reuses the last chroma pair.
    let tail = pairs.into_remainder();
    if let (4, Some(&y)) = (tail.len(), ys.next()) {
        let u = *u_row.iter().step_by(u_step).next_back().unwrap_or(&128);
        let v = *v_row.iter().step_by(v_step).next_back().unwrap_or(&128);
        tail.copy_from_slice(&co.px(y, u, v));
    }
}

/// Copy the top-left `width × height` pixels of a BGRA image whose rows
/// start `stride` bytes apart into tightly packed RGBA in `out` (resized
/// to fit). Alpha is set opaque: a decoded video frame has none, and
/// some decoders leave the byte undefined.
///
/// # Errors
/// The buffer is too small for the rectangle, or `stride` is shorter
/// than a row — an error, not a panic, since both come from the
/// decoder.
pub fn bgra_to_rgba(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    let row = w * 4;
    if w == 0 || h == 0 {
        out.clear();
        return Ok(());
    }
    if stride < row {
        out.clear();
        return Err(format!(
            "{stride} byte rows cannot hold {width} BGRA pixels"
        ));
    }
    let need = stride * (h - 1) + row;
    if src.len() < need {
        out.clear();
        return Err(format!(
            "{width}x{height} BGRA at stride {stride} needs {need} bytes, decoder gave {}",
            src.len()
        ));
    }
    // Every byte is written below: a buffer already this size (the
    // decoder reuses it frame after frame) is not zero-filled first.
    if out.len() != row * h {
        out.clear();
        out.resize(row * h, 0);
    }
    for (dst, src_row) in out.chunks_exact_mut(row).zip(src.chunks(stride)) {
        for (d, s) in dst.chunks_exact_mut(4).zip(src_row[..row].chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
    Ok(())
}

/// A one-line description for logs.
#[must_use]
pub fn describe(layout: &Yuv420Layout, spec: ColorSpec) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{}x{}+{}+{} Y@{} s{}/{} U@{} s{}/{} V@{} s{}/{} {:?} {:?}",
        layout.width,
        layout.height,
        layout.crop_x,
        layout.crop_y,
        layout.y.offset,
        layout.y.row_stride,
        layout.y.pixel_stride,
        layout.u.offset,
        layout.u.row_stride,
        layout.u.pixel_stride,
        layout.v.offset,
        layout.v.row_stride,
        layout.v.pixel_stride,
        spec.matrix,
        spec.range
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image2(width: u32, height: u32, planes: [(u32, i32, i32, u32, u32); 3]) -> Vec<u8> {
        let mut b = Vec::with_capacity(MEDIA_IMAGE2_LEN);
        for v in [1u32, 3, width, height, 8, 8] {
            b.extend_from_slice(&v.to_ne_bytes());
        }
        for (off, col, row, h, v) in planes {
            b.extend_from_slice(&off.to_ne_bytes());
            b.extend_from_slice(&col.to_ne_bytes());
            b.extend_from_slice(&row.to_ne_bytes());
            b.extend_from_slice(&h.to_ne_bytes());
            b.extend_from_slice(&v.to_ne_bytes());
        }
        // Unused fourth plane.
        b.extend_from_slice(&[0u8; 20]);
        assert_eq!(b.len(), MEDIA_IMAGE2_LEN);
        b
    }

    #[test]
    fn media_image2_semi_planar_parses() {
        let bytes = image2(
            1920,
            1088,
            [
                (0, 1, 1920, 1, 1),
                (1920 * 1088, 2, 1920, 2, 2),
                (1920 * 1088 + 1, 2, 1920, 2, 2),
            ],
        );
        let l = Yuv420Layout::from_media_image2(&bytes).unwrap();
        assert_eq!(l, Yuv420Layout::semi_planar(1920, 1088, 1920, 1088));
        let cropped = l.with_crop(0, 0, 1920, 1080);
        assert_eq!((cropped.width, cropped.height), (1920, 1080));
        assert_eq!(cropped.required_len(), 1920 * 1088 + 1920 * 539 + 1919 + 1);
    }

    #[test]
    fn media_image2_rejects_odd_shapes() {
        let mut bytes = image2(
            16,
            16,
            [(0, 1, 16, 1, 1), (256, 1, 8, 2, 2), (320, 1, 8, 2, 2)],
        );
        assert!(Yuv420Layout::from_media_image2(&bytes).is_ok());
        bytes[0..4].copy_from_slice(&3u32.to_ne_bytes()); // RGB
        assert!(Yuv420Layout::from_media_image2(&bytes).is_err());
        assert!(Yuv420Layout::from_media_image2(&bytes[..50]).is_err());
        let bytes = image2(
            16,
            16,
            [(0, 1, 16, 1, 1), (256, 1, 8, 1, 1), (320, 1, 8, 2, 2)],
        );
        assert!(
            Yuv420Layout::from_media_image2(&bytes).is_err(),
            "4:4:4 chroma"
        );
    }

    #[test]
    fn bgra_swaps_red_and_blue_and_drops_row_padding() {
        // 2×2 at a 12-byte stride (4 bytes of padding per row).
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 0xEE, 0xEE, 0xEE, 0xEE, //
            7, 8, 9, 0, 10, 11, 12, 0,
        ];
        let mut out = vec![0xAB; 3];
        bgra_to_rgba(&src, 12, 2, 2, &mut out).unwrap();
        assert_eq!(
            out,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }

    #[test]
    fn bgra_refuses_short_buffers_and_rows() {
        let mut out = Vec::new();
        assert!(
            bgra_to_rgba(&[0; 8], 4, 2, 1, &mut out).is_err(),
            "stride < row"
        );
        assert!(
            bgra_to_rgba(&[0; 15], 8, 2, 2, &mut out).is_err(),
            "one byte short"
        );
        assert!(bgra_to_rgba(&[0; 16], 8, 2, 2, &mut out).is_ok());
        assert!(bgra_to_rgba(&[], 0, 0, 0, &mut out).is_ok());
        assert!(out.is_empty());
    }

    #[test]
    fn crop_clamps_and_aligns() {
        let l = Yuv420Layout::planar(64, 48, 64, 48).with_crop(3, 5, 100, 100);
        assert_eq!((l.crop_x, l.crop_y, l.width, l.height), (2, 4, 62, 44));
    }

    /// A 4×2 I420 frame of known colours converts to the expected RGBA.
    #[test]
    fn planar_bt709_limited_converts_known_colours() {
        // Rows: white, black | grey columns don't matter: whole frame
        // split left (white) / right (black) so chroma pairs stay pure.
        let y = [235u8, 235, 16, 16, 235, 235, 16, 16];
        let u = [128u8, 128];
        let v = [128u8, 128];
        let mut src = Vec::new();
        src.extend_from_slice(&y);
        src.extend_from_slice(&u);
        src.extend_from_slice(&v);
        let layout = Yuv420Layout::planar(4, 2, 4, 2);
        let mut out = Vec::new();
        to_rgba(&src, &layout, ColorSpec::default(), &mut out).unwrap();
        assert_eq!(out.len(), 4 * 2 * 4);
        assert_eq!(&out[0..4], &[255, 255, 255, 255]);
        assert_eq!(&out[8..12], &[0, 0, 0, 255]);
        assert_eq!(&out[16..20], &[255, 255, 255, 255]);
        assert_eq!(&out[24..28], &[0, 0, 0, 255]);
    }

    #[test]
    fn semi_planar_red_is_red() {
        // BT.709 limited red: Y=63, Cb=102, Cr=240.
        let y = [63u8; 4];
        let uv = [102u8, 240];
        let mut src = Vec::new();
        src.extend_from_slice(&y);
        src.extend_from_slice(&uv);
        let layout = Yuv420Layout::semi_planar(2, 2, 2, 2);
        let mut out = Vec::new();
        to_rgba(&src, &layout, ColorSpec::default(), &mut out).unwrap();
        for px in out.chunks_exact(4) {
            assert!(px[0] >= 250, "r {}", px[0]);
            assert!(px[1] <= 5, "g {}", px[1]);
            assert!(px[2] <= 5, "b {}", px[2]);
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn full_range_grey_is_identity() {
        let y = [77u8; 4];
        let u = [128u8];
        let v = [128u8];
        let mut src = Vec::new();
        src.extend_from_slice(&y);
        src.extend_from_slice(&u);
        src.extend_from_slice(&v);
        let layout = Yuv420Layout::planar(2, 2, 2, 2);
        let spec = ColorSpec {
            matrix: Matrix::Bt601,
            range: Range::Full,
        };
        let mut out = Vec::new();
        to_rgba(&src, &layout, spec, &mut out).unwrap();
        assert_eq!(&out[0..3], &[77, 77, 77]);
    }

    #[test]
    fn odd_width_and_padding_rows_are_handled() {
        // 3×3 visible inside a 4-stride, 4-slice buffer.
        let layout = Yuv420Layout::planar(3, 3, 4, 4);
        let src = vec![128u8; layout.required_len()];
        let mut out = Vec::new();
        to_rgba(&src, &layout, ColorSpec::default(), &mut out).unwrap();
        assert_eq!(out.len(), 3 * 3 * 4);
        assert!(out
            .chunks_exact(4)
            .all(|p| p[3] == 255 && p[0] == p[1] && p[1] == p[2]));
    }

    #[test]
    fn short_buffer_is_an_error_not_a_panic() {
        let layout = Yuv420Layout::planar(16, 16, 16, 16);
        let src = vec![0u8; 100];
        let mut out = Vec::new();
        assert!(to_rgba(&src, &layout, ColorSpec::default(), &mut out).is_err());
    }
}
