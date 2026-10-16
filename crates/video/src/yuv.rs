//! YUV 4:2:0 → RGBA for decoders that hand back raw planes.
//!
//! `FFmpeg` converts through swscale; Android's `MediaCodec` (see
//! `mediacodec.rs`) returns the picture as it sits in the codec's output
//! buffer — three planes at vendor-chosen offsets and strides, described
//! either by a `MediaImage2` record (`image-data`) or by the classic
//! `color-format` + `stride` + `slice-height` trio. [`Yuv420Layout`]
//! names the common shape of both and [`to_rgba_into`] walks it.
//!
//! `VideoToolbox` (`videotoolbox.rs`, iOS) is asked for BGRA instead —
//! it converts on the GPU with the stream's own colour signalling. The
//! decoder drops the row padding ([`pack_rows`]) and whoever shows the
//! picture swaps two bytes per pixel as it writes its own buffer
//! ([`bgra_to_rgba_into`]).
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
    /// Visible size — the [`to_rgba_into`] output's dimensions.
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

/// [`to_rgba_into`] on one thread, into an `out` it sizes: what the
/// tests convert with.
#[cfg(test)]
fn to_rgba(
    src: &[u8],
    layout: &Yuv420Layout,
    spec: ColorSpec,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let len = layout.width as usize * layout.height as usize * 4;
    if out.len() != len {
        out.clear();
        out.resize(len, 0);
    }
    to_rgba_into(src, layout, spec, out, None)
}

/// Rows per band handed to a thread by [`to_rgba_into`]: small enough
/// that a slow core (a phone's little cluster) takes few of them, large
/// enough that handing them out costs nothing.
const BAND_ROWS: usize = 32;

/// Threads parked between pictures, each taking bands of rows of the
/// next one as [`to_rgba_into`] hands them out. Started once: a scope
/// per picture started (and joined) up to three threads per frame, 180
/// a second at 60 fps (review 2026-10, P2-35). Parked on a channel,
/// they cost nothing between pictures; dropping the pool ends them.
pub struct BandPool {
    jobs: Vec<std::sync::mpsc::Sender<Job>>,
    done: std::sync::mpsc::Receiver<()>,
}

/// A picture's band loop, borrowed for the length of one
/// [`BandPool::run`], which does not return before every worker has
/// said it is finished with it.
struct Job(*const (dyn Fn() + Sync));

// SAFETY: the closure is `Sync`, and `BandPool::run` keeps it alive
// until every worker it was sent to is done with it.
unsafe impl Send for Job {}

impl BandPool {
    /// A pool converting on `threads` threads, the caller's included:
    /// `threads - 1` are started here. `None` when one thread is all
    /// there is.
    #[must_use]
    pub fn new(threads: usize) -> Option<Self> {
        if threads < 2 {
            return None;
        }
        let (done_tx, done) = std::sync::mpsc::channel();
        let mut jobs = Vec::with_capacity(threads - 1);
        for i in 1..threads {
            let (tx, rx) = std::sync::mpsc::channel::<Job>();
            let done_tx = done_tx.clone();
            let started = std::thread::Builder::new()
                .name(format!("yuv-band-{i}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        // SAFETY: see `Job`: `run` is still waiting.
                        let work = unsafe { &*job.0 };
                        // A panic leaves its band unconverted, and the
                        // thread parked for the next picture.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                        if done_tx.send(()).is_err() {
                            break;
                        }
                    }
                });
            if started.is_ok() {
                jobs.push(tx);
            }
        }
        (!jobs.is_empty()).then_some(Self { jobs, done })
    }

    /// Threads a picture is spread over, the caller's included.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.jobs.len() + 1
    }

    /// Run `work` on every pooled thread and on this one, returning once
    /// all of them have finished — also when this thread's run panics.
    fn run(&self, work: &(dyn Fn() + Sync)) {
        struct WaitForAll<'a> {
            done: &'a std::sync::mpsc::Receiver<()>,
            sent: usize,
        }
        impl Drop for WaitForAll<'_> {
            fn drop(&mut self) {
                for _ in 0..self.sent {
                    // A worker gone is a worker no longer using `work`.
                    if self.done.recv().is_err() {
                        break;
                    }
                }
            }
        }
        let work_ptr: *const (dyn Fn() + Sync + '_) = work;
        // SAFETY: only the lifetime is erased; `WaitForAll` keeps `work`
        // borrowed until every worker sent it has reported back.
        // A cast cannot lengthen a trait object's lifetime; a transmute
        // between the two pointer types is the way.
        #[allow(clippy::transmute_ptr_to_ptr)]
        let work_ptr = unsafe {
            std::mem::transmute::<*const (dyn Fn() + Sync + '_), *const (dyn Fn() + Sync + 'static)>(
                work_ptr,
            )
        };
        let mut wait = WaitForAll {
            done: &self.done,
            sent: 0,
        };
        for jobs in &self.jobs {
            if jobs.send(Job(work_ptr)).is_ok() {
                wait.sent += 1;
            }
        }
        work();
    }
}

/// Convert the visible rectangle of `src` (laid out as `layout` says)
/// into tightly packed RGBA in `out`, of exactly `width × height × 4`
/// bytes — the buffer the picture will be drawn from — on this thread
/// and those of `pool`, which take bands of rows as they come free. A
/// 1080p picture is ~2 M pixels: on a mid-range phone one core takes
/// ~25 ms over it (2026-10-08, MOB-18), more than a 60 fps frame has.
///
/// # Errors
/// The source is too small for the layout (or a stride is zero), or
/// `out` is not the visible size — errors, not panics, since the layout
/// came from a vendor codec.
pub fn to_rgba_into(
    src: &[u8],
    layout: &Yuv420Layout,
    spec: ColorSpec,
    out: &mut [u8],
    pool: Option<&BandPool>,
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
    if out.len() != width * height * 4 {
        return Err(format!(
            "{width}x{height} RGBA needs {} bytes, the target has {}",
            width * height * 4,
            out.len()
        ));
    }
    if width == 0 || height == 0 {
        return Ok(());
    }
    let co = Coeffs::new(spec);
    let row_bytes = width * 4;
    let Some(pool) = pool.filter(|_| height > BAND_ROWS) else {
        convert_rows(src, layout, &co, 0, out);
        return Ok(());
    };
    let bands = out
        .chunks_mut(BAND_ROWS * row_bytes)
        .enumerate()
        .map(|(i, band)| (i * BAND_ROWS, band));
    let queue = std::sync::Mutex::new(bands);
    pool.run(&|| loop {
        let next = match queue.lock() {
            Ok(mut q) => q.next(),
            Err(poisoned) => poisoned.into_inner().next(),
        };
        let Some((first_row, band)) = next else {
            break;
        };
        convert_rows(src, layout, &co, first_row, band);
    });
    Ok(())
}

/// Rows `first_row..` of the visible rectangle into `dst` (whole rows).
/// The caller has checked `required_len`, so every slice below exists.
fn convert_rows(src: &[u8], layout: &Yuv420Layout, co: &Coeffs, first_row: usize, dst: &mut [u8]) {
    let width = layout.width as usize;
    let (x0, y0) = (layout.crop_x as usize, layout.crop_y as usize);
    let chroma_cols = width.div_ceil(2);
    let (ys, us, vs) = (
        layout.y.pixel_stride,
        layout.u.pixel_stride,
        layout.v.pixel_stride,
    );
    for (i, out_row) in dst.chunks_exact_mut(width * 4).enumerate() {
        let py = y0 + first_row + i;
        let pc = py / 2;
        let y_start = layout.y.offset + py * layout.y.row_stride + x0 * ys;
        let u_start = layout.u.offset + pc * layout.u.row_stride + (x0 / 2) * us;
        let v_start = layout.v.offset + pc * layout.v.row_stride + (x0 / 2) * vs;
        if ys == 1 && us == 1 && vs == 1 {
            // I420 / YV12: three plain planes — the common MediaCodec
            // output (the A07's c2.mtk.avc.decoder).
            convert_row_planar(
                co,
                &src[y_start..y_start + width],
                &src[u_start..u_start + chroma_cols],
                &src[v_start..v_start + chroma_cols],
                out_row,
            );
        } else if ys == 1 && us == 2 && vs == 2 && u_start.abs_diff(v_start) == 1 {
            // NV12 / NV21: chroma pairs interleaved in one plane.
            let base = u_start.min(v_start);
            convert_row_interleaved(
                co,
                &src[y_start..y_start + width],
                &src[base..base + chroma_cols * 2],
                u_start < v_start,
                out_row,
            );
        } else {
            convert_row(
                co,
                &src[y_start..=y_start + (width - 1) * ys],
                ys,
                &src[u_start..=u_start + (chroma_cols - 1) * us],
                us,
                &src[v_start..=v_start + (chroma_cols - 1) * vs],
                vs,
                out_row,
            );
        }
    }
}

/// One row from plain planes: no strides to step over, so the loop is
/// over plain slices the compiler vectorises (≈3× the general walker on
/// x86; benchmarked against hand-hoisted variants, which it did not
/// vectorise and ran 5–8× slower).
#[inline]
fn convert_row_planar(co: &Coeffs, y: &[u8], u: &[u8], v: &[u8], dst: &mut [u8]) {
    let mut pairs = dst.chunks_exact_mut(8);
    for (((px, luma), &cb), &cr) in pairs.by_ref().zip(y.chunks_exact(2)).zip(u).zip(v) {
        px[..4].copy_from_slice(&co.px(luma[0], cb, cr));
        px[4..].copy_from_slice(&co.px(luma[1], cb, cr));
    }
    let tail = pairs.into_remainder();
    if tail.len() == 4 {
        let (Some(&luma), Some(&cb), Some(&cr)) = (y.last(), u.last(), v.last()) else {
            return;
        };
        tail.copy_from_slice(&co.px(luma, cb, cr));
    }
}

/// One row with interleaved chroma: `uv` holds the pairs, U first when
/// `u_first` (NV12), else V first (NV21). The pairs are split into two
/// short rows first (a row of chroma is under 1 KB), so the conversion
/// itself is the planar loop.
#[inline]
fn convert_row_interleaved(co: &Coeffs, y: &[u8], uv: &[u8], u_first: bool, dst: &mut [u8]) {
    const MAX_COLS: usize = 2048;
    let cols = uv.len() / 2;
    let mut u_buf = [0u8; MAX_COLS];
    let mut v_buf = [0u8; MAX_COLS];
    let (u, v): (Vec<u8>, Vec<u8>);
    let (u, v) = if cols <= MAX_COLS {
        for (i, c) in uv.chunks_exact(2).enumerate() {
            let (cb, cr) = if u_first { (c[0], c[1]) } else { (c[1], c[0]) };
            u_buf[i] = cb;
            v_buf[i] = cr;
        }
        (&u_buf[..cols], &v_buf[..cols])
    } else {
        (u, v) = uv
            .chunks_exact(2)
            .map(|c| if u_first { (c[0], c[1]) } else { (c[1], c[0]) })
            .unzip();
        (&u[..], &v[..])
    };
    convert_row_planar(co, y, u, v, dst);
}

/// One output row: two luma samples share each chroma pair. The general
/// case, any sample strides.
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

/// Copy the top-left `width × height` pixels of a four-byte-a-pixel
/// image whose rows start `stride` bytes apart into `out`, tightly
/// packed (`out` is emptied first and grown to fit). The bytes are not
/// touched and nothing is zero-filled first: a recycled buffer costs
/// one copy.
///
/// # Errors
/// The buffer is too small for the rectangle, or `stride` is shorter
/// than a row — an error, not a panic, since both come from the
/// decoder.
pub fn pack_rows(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    let row = w * 4;
    out.clear();
    if w == 0 || h == 0 {
        return Ok(());
    }
    if stride < row {
        return Err(format!(
            "{stride} byte rows cannot hold {width} four-byte pixels"
        ));
    }
    let need = stride * (h - 1) + row;
    if src.len() < need {
        return Err(format!(
            "{width}x{height} pixels at stride {stride} need {need} bytes, decoder gave {}",
            src.len()
        ));
    }
    out.reserve(row * h);
    for src_row in src.chunks(stride).take(h) {
        out.extend_from_slice(&src_row[..row]);
    }
    Ok(())
}

/// Write tightly packed BGRA as RGBA into `out`, a buffer of the same
/// size. Alpha is set opaque: a decoded video frame has none, and some
/// decoders leave the byte undefined.
///
/// # Errors
/// The two are not the same size, or not whole pixels.
pub fn bgra_to_rgba_into(src: &[u8], out: &mut [u8]) -> Result<(), String> {
    if src.len() != out.len() || !src.len().is_multiple_of(4) {
        return Err(format!(
            "{} bytes of BGRA for a {}-byte picture",
            src.len(),
            out.len()
        ));
    }
    for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 255;
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
    fn packing_drops_row_padding_and_keeps_the_bytes() {
        // 2×2 at a 12-byte stride (4 bytes of padding per row).
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, 0xEE, 0xEE, 0xEE, 0xEE, //
            7, 8, 9, 0, 10, 11, 12, 0,
        ];
        // A recycled buffer: longer than needed, and not zeroed.
        let mut out = vec![0xAB; 64];
        pack_rows(&src, 12, 2, 2, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0]);
        // Cut to the announced size: one pixel of the first row.
        pack_rows(&src, 12, 1, 1, &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 0]);
    }

    #[test]
    fn packing_refuses_short_buffers_and_rows() {
        let mut out = vec![1, 2, 3];
        assert!(
            pack_rows(&[0; 8], 4, 2, 1, &mut out).is_err(),
            "stride < row"
        );
        assert!(out.is_empty(), "nothing left of the last picture");
        assert!(
            pack_rows(&[0; 15], 8, 2, 2, &mut out).is_err(),
            "one byte short"
        );
        assert!(pack_rows(&[0; 16], 8, 2, 2, &mut out).is_ok());
        assert!(pack_rows(&[], 0, 0, 0, &mut out).is_ok());
        assert!(out.is_empty());
    }

    #[test]
    fn bgra_is_written_as_rgba_with_opaque_alpha() {
        let src = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        let mut out = [0xAB; 16];
        bgra_to_rgba_into(&src, &mut out).unwrap();
        assert_eq!(
            out,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
        assert!(
            bgra_to_rgba_into(&src, &mut [0; 12]).is_err(),
            "sizes differ"
        );
        assert!(
            bgra_to_rgba_into(&src[..6], &mut [0; 6]).is_err(),
            "half a pixel"
        );
        assert!(bgra_to_rgba_into(&[], &mut []).is_ok());
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

    /// A pseudo-random picture in `layout`, so every pixel differs.
    fn noise(layout: &Yuv420Layout) -> Vec<u8> {
        let mut x = 0x1234_5678_u32;
        (0..layout.required_len())
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    /// The general row walker, the reference the fast paths must match.
    fn reference(src: &[u8], layout: &Yuv420Layout, spec: ColorSpec) -> Vec<u8> {
        let co = Coeffs::new(spec);
        let width = layout.width as usize;
        let mut out = vec![0u8; width * layout.height as usize * 4];
        let (x0, y0) = (layout.crop_x as usize, layout.crop_y as usize);
        let cols = width.div_ceil(2);
        for (row, dst) in out.chunks_exact_mut(width * 4).enumerate() {
            let (py, pc) = (y0 + row, (y0 + row) / 2);
            let (y, u, v) = (layout.y, layout.u, layout.v);
            let ys = y.offset + py * y.row_stride + x0 * y.pixel_stride;
            let us = u.offset + pc * u.row_stride + (x0 / 2) * u.pixel_stride;
            let vs = v.offset + pc * v.row_stride + (x0 / 2) * v.pixel_stride;
            convert_row(
                &co,
                &src[ys..=ys + (width - 1) * y.pixel_stride],
                y.pixel_stride,
                &src[us..=us + (cols - 1) * u.pixel_stride],
                u.pixel_stride,
                &src[vs..=vs + (cols - 1) * v.pixel_stride],
                v.pixel_stride,
                dst,
            );
        }
        out
    }

    #[test]
    fn planar_fast_path_matches_the_general_one() {
        // Odd width and a crop, inside padded rows.
        let layout = Yuv420Layout::planar(1920, 1088, 1920, 1088).with_crop(2, 4, 1917, 1080);
        let src = noise(&layout);
        let mut out = Vec::new();
        to_rgba(&src, &layout, ColorSpec::default(), &mut out).unwrap();
        assert!(out == reference(&src, &layout, ColorSpec::default()));
    }

    #[test]
    fn nv12_and_nv21_fast_paths_match_the_general_one() {
        let nv12 = Yuv420Layout::semi_planar(641, 361, 704, 368);
        let mut nv21 = nv12;
        std::mem::swap(&mut nv21.u, &mut nv21.v);
        for layout in [nv12, nv21] {
            let src = noise(&layout);
            let mut out = Vec::new();
            to_rgba(&src, &layout, ColorSpec::default(), &mut out).unwrap();
            assert!(out == reference(&src, &layout, ColorSpec::default()));
        }
    }

    #[test]
    fn threads_change_nothing_but_the_time() {
        let layout = Yuv420Layout::planar(1280, 722, 1280, 736);
        let src = noise(&layout);
        let mut one = vec![0u8; 1280 * 722 * 4];
        let mut four = vec![0u8; 1280 * 722 * 4];
        let pool = BandPool::new(4).unwrap();
        assert_eq!(pool.threads(), 4);
        to_rgba_into(&src, &layout, ColorSpec::default(), &mut one, None).unwrap();
        to_rgba_into(&src, &layout, ColorSpec::default(), &mut four, Some(&pool)).unwrap();
        assert!(one == four);
        // The parked threads take the next picture too.
        four.fill(0);
        to_rgba_into(&src, &layout, ColorSpec::default(), &mut four, Some(&pool)).unwrap();
        assert!(one == four);
        assert!(BandPool::new(1).is_none());
    }

    #[test]
    fn a_target_of_the_wrong_size_is_an_error() {
        let layout = Yuv420Layout::planar(16, 16, 16, 16);
        let src = vec![0u8; layout.required_len()];
        let mut out = vec![0u8; 16 * 15 * 4];
        assert!(to_rgba_into(&src, &layout, ColorSpec::default(), &mut out, None).is_err());
    }
}
