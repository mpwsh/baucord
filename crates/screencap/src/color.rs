//! Fixed-point BT.601 color space conversion.
//!
//! All math uses i16/i32 with bit-shifts instead of f32 multiplies.
//! At 1080p this processes ~2M pixels/frame — integer math is 2-4× faster
//! than the equivalent float path.

/// Convert RGBA to YUV420 planar into a caller-provided buffer.
///
/// Output layout: `[Y plane: w*h][U plane: w*h/4][V plane: w*h/4]`.
/// Buffer must be at least `w * h * 3 / 2` bytes.
///
/// Uses **full-range** BT.601 in fixed-point (shift-by-8, +128 for rounding):
/// - Y  =  77*R + 150*G +  29*B + 128 >> 8
/// - Cb = -43*R -  85*G + 128*B + 128 >> 8 + 128
/// - Cr = 128*R - 107*G -  21*B + 128 >> 8 + 128
///
/// Full-range maps RGB 0–255 → Y 0–255 (not studio 16–235), which is correct
/// for screen capture where pixel values are computer-generated.
pub fn rgba_to_yuv420(rgba: &[u8], w: usize, h: usize, yuv: &mut [u8]) {
    debug_assert!(rgba.len() >= w * h * 4, "RGBA buffer too small");
    debug_assert!(yuv.len() >= w * h * 3 / 2, "YUV buffer too small");

    let y_plane = w * h;
    let uv_w = w / 2;
    let u_offset = y_plane;
    let v_offset = u_offset + uv_w * (h / 2);

    // Y plane — full resolution
    for row in 0..h {
        let rgba_row = row * w * 4;
        let y_row = row * w;
        for col in 0..w {
            let i = rgba_row + col * 4;
            let r = rgba[i] as i32;
            let g = rgba[i + 1] as i32;
            let b = rgba[i + 2] as i32;
            yuv[y_row + col] = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(0, 255) as u8;
        }
    }

    // U and V planes — half resolution, sample top-left pixel of each 2×2 block
    for row in (0..h).step_by(2) {
        let rgba_row = row * w * 4;
        let uv_row = (row / 2) * uv_w;
        for col in (0..w).step_by(2) {
            let i = rgba_row + col * 4;
            let r = rgba[i] as i32;
            let g = rgba[i + 1] as i32;
            let b = rgba[i + 2] as i32;

            let uv_idx = uv_row + col / 2;
            yuv[u_offset + uv_idx] =
                (((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            yuv[v_offset + uv_idx] =
                (((128 * r - 107 * g - 21 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

/// Convert YUV420 planar (with per-plane strides) to RGBA into a caller-provided buffer.
///
/// `rgba` must be at least `w * h * 4` bytes.
///
/// Full-range BT.601 inverse in fixed-point (shift-by-8, +128 for rounding):
/// - R = Y + 359*(V-128) + 128 >> 8
/// - G = Y -  88*(U-128) - 183*(V-128) + 128 >> 8
/// - B = Y + 454*(U-128) + 128 >> 8
#[allow(clippy::too_many_arguments)]
pub fn yuv420_to_rgba(
    y_data: &[u8],
    u_data: &[u8],
    v_data: &[u8],
    y_stride: usize,
    u_stride: usize,
    v_stride: usize,
    w: usize,
    h: usize,
    rgba: &mut [u8],
) {
    debug_assert!(rgba.len() >= w * h * 4, "RGBA buffer too small");

    for row in 0..h {
        let y_row_off = row * y_stride;
        let uv_row = row / 2;
        let u_row_off = uv_row * u_stride;
        let v_row_off = uv_row * v_stride;
        let out_row = row * w * 4;

        for col in 0..w {
            let y = y_data[y_row_off + col] as i32;
            let u = u_data[u_row_off + col / 2] as i32 - 128;
            let v = v_data[v_row_off + col / 2] as i32 - 128;

            let out = out_row + col * 4;
            rgba[out] = ((y * 256 + 359 * v + 128) >> 8).clamp(0, 255) as u8;
            rgba[out + 1] = ((y * 256 - 88 * u - 183 * v + 128) >> 8).clamp(0, 255) as u8;
            rgba[out + 2] = ((y * 256 + 454 * u + 128) >> 8).clamp(0, 255) as u8;
            rgba[out + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a solid red pixel through YUV and back.
    /// Allows ±2 tolerance for fixed-point rounding.
    #[test]
    fn roundtrip_solid_red_preserves_color() {
        let w = 2;
        let h = 2;
        // 2×2 solid red (R=255, G=0, B=0, A=255)
        let rgba_in: Vec<u8> = [255, 0, 0, 255].repeat(4);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        rgba_to_yuv420(&rgba_in, w, h, &mut yuv);

        let mut rgba_out = vec![0u8; w * h * 4];
        yuv420_to_rgba(
            &yuv[..w * h],
            &yuv[w * h..w * h + 1],
            &yuv[w * h + 1..],
            w,
            1,
            1,
            w,
            h,
            &mut rgba_out,
        );

        // Check first pixel within tolerance
        assert!(
            (rgba_out[0] as i32 - 255).unsigned_abs() <= 2,
            "R={}",
            rgba_out[0]
        );
        assert!(
            (rgba_out[1] as i32).unsigned_abs() <= 2,
            "G={}",
            rgba_out[1]
        );
        assert!(
            (rgba_out[2] as i32).unsigned_abs() <= 2,
            "B={}",
            rgba_out[2]
        );
        assert_eq!(rgba_out[3], 255);
    }

    #[test]
    fn roundtrip_solid_white_preserves_color() {
        let w = 2;
        let h = 2;
        let rgba_in: Vec<u8> = [255, 255, 255, 255].repeat(4);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        rgba_to_yuv420(&rgba_in, w, h, &mut yuv);

        let mut rgba_out = vec![0u8; w * h * 4];
        yuv420_to_rgba(
            &yuv[..w * h],
            &yuv[w * h..w * h + 1],
            &yuv[w * h + 1..],
            w,
            1,
            1,
            w,
            h,
            &mut rgba_out,
        );

        for (ch, &val) in rgba_out.iter().enumerate().take(3) {
            assert!(
                (val as i32 - 255).unsigned_abs() <= 2,
                "channel {ch} = {val}",
            );
        }
    }
}
