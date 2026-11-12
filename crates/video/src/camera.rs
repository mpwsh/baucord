//! Cameras (1.5): a webcam's pictures as RGBA [`Frame`]s for the
//! encoder.
//!
//! One trait, [`Camera`], over the platform's own capture API. On the
//! desktops it is [nokhwa]'s native backends — Media Foundation on
//! Windows, `AVFoundation` on macOS, V4L2 on Linux — opened at the size
//! closest to the size asked, and this module turns whatever the device
//! hands out (YUYV, NV12, MJPEG, RGB, BGR, grey) into RGBA. The phones
//! capture in their hosts and get a backend of their own here later;
//! until then [`open`] refuses there.
//!
//! [`Camera::frame`] blocks until the device's next picture: the
//! caller's thread is paced by the camera, never by a timer.
//!
//! [nokhwa]: https://crates.io/crates/nokhwa

use crate::{Frame, PixelFormat};

/// The frame rate asked of a camera; one without it gives its closest.
pub const WANTED_FPS: u32 = 30;

/// A camera the system knows of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// What [`open`] takes.
    pub index: u32,
    /// The name the system gives it.
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("no camera found")]
    NoCamera,
    #[error("camera access was not allowed")]
    NotAllowed,
    #[error("cameras are not supported on this platform yet")]
    Unsupported,
    #[error("camera: {0}")]
    Backend(String),
    #[error("camera frame: {0}")]
    Decode(String),
}

/// An open camera. Not `Send`: some backends (`AVFoundation`, Media
/// Foundation) belong to the thread that opened them, so the thread
/// that reads a camera opens it.
pub trait Camera {
    /// The next picture, waiting for it. Timestamps are milliseconds
    /// since the camera opened.
    ///
    /// # Errors
    /// The device went away or sent something undecodable.
    fn frame(&mut self) -> Result<Frame, CameraError>;
    /// The size the device delivers.
    fn size(&self) -> (u32, u32);
    /// The device's name, for the log.
    fn name(&self) -> &str;
    /// The heights the device can deliver, tallest first (empty when it
    /// will not say).
    fn heights(&mut self) -> Vec<u32>;
    /// How it was opened, for the log and telemetry ("1280x720 30 fps
    /// Nv12").
    fn mode(&self) -> String;
}

// ── Pixel formats ───────────────────────────────────────────────────────

/// What a camera's buffer holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFormat {
    /// Packed 4:2:2, `Y0 U Y1 V`.
    Yuyv,
    /// 4:2:0, a Y plane then interleaved `U V`.
    Nv12,
    /// One JPEG a frame.
    Mjpeg,
    Rgb,
    Bgr,
    Gray,
}

/// BT.601 limited range, as webcams send it, in fixed point.
#[inline]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]
fn yuv_to_rgba(y: u8, u: u8, v: u8) -> [u8; 4] {
    let c = (i32::from(y) - 16) * 298;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let r = (c + 409 * e + 128) >> 8;
    let g = (c - 100 * d - 208 * e + 128) >> 8;
    let b = (c + 516 * d + 128) >> 8;
    [
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
        255,
    ]
}

/// One camera buffer as tightly packed RGBA, `width × height × 4`.
/// An MJPEG frame carries its own size, which wins over the one given.
///
/// # Errors
/// The buffer is shorter than its format and size need, or its JPEG
/// does not decode.
pub fn to_rgba(
    format: RawFormat,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<(Vec<u8>, u32, u32), CameraError> {
    let (w, h) = (width as usize, height as usize);
    let short = |need: usize| {
        CameraError::Decode(format!(
            "{format:?} {width}x{height}: {} bytes, {need} needed",
            data.len()
        ))
    };
    let mut out = Vec::with_capacity(w * h * 4);
    match format {
        RawFormat::Yuyv => {
            let need = w * h * 2;
            if data.len() < need || w % 2 != 0 {
                return Err(short(need));
            }
            for px in data[..need].chunks_exact(4) {
                out.extend_from_slice(&yuv_to_rgba(px[0], px[1], px[3]));
                out.extend_from_slice(&yuv_to_rgba(px[2], px[1], px[3]));
            }
        }
        RawFormat::Nv12 => {
            let need = w * h + w.div_ceil(2) * 2 * h.div_ceil(2);
            if data.len() < need {
                return Err(short(need));
            }
            let (luma, chroma) = data.split_at(w * h);
            let stride = w.div_ceil(2) * 2;
            for row in 0..h {
                let uv = &chroma[(row / 2) * stride..];
                for col in 0..w {
                    let i = (col / 2) * 2;
                    out.extend_from_slice(&yuv_to_rgba(luma[row * w + col], uv[i], uv[i + 1]));
                }
            }
        }
        RawFormat::Rgb | RawFormat::Bgr => {
            let need = w * h * 3;
            if data.len() < need {
                return Err(short(need));
            }
            for px in data[..need].chunks_exact(3) {
                if format == RawFormat::Rgb {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                } else {
                    out.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
            }
        }
        RawFormat::Gray => {
            let need = w * h;
            if data.len() < need {
                return Err(short(need));
            }
            for &g in &data[..need] {
                out.extend_from_slice(&[g, g, g, 255]);
            }
        }
        RawFormat::Mjpeg => return decode_mjpeg(data),
    }
    Ok((out, width, height))
}

fn decode_mjpeg(data: &[u8]) -> Result<(Vec<u8>, u32, u32), CameraError> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
    let pixels = decoder
        .decode()
        .map_err(|e| CameraError::Decode(format!("MJPEG: {e:?}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| CameraError::Decode("MJPEG: no header".into()))?;
    Ok((pixels, u32::from(info.width), u32::from(info.height)))
}

// ── Picking a format ────────────────────────────────────────────────────

/// One size, rate and pixel format a device lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: RawFormat,
}

/// The offer to open for `height` at `fps`: the first of
/// [`rank_formats`]. `None` when nothing is listed.
#[must_use]
pub fn choose_format(offers: &[Offer], height: u32, fps: u32) -> Option<usize> {
    rank_formats(offers, height, fps).first().copied()
}

/// Every offer, best first, for `height` at `fps`: a smooth rate first
/// (a USB 2 camera gives 720p uncompressed at ~10 fps, MJPEG at 30),
/// then the smallest size at least as tall as asked (else the tallest
/// there is), then 16:9, then the rate nearest `fps`, then the
/// cheapest pixels to turn into RGBA (NV12 and YUYV before MJPEG). The
/// next one is the one to try when a device refuses the first.
#[must_use]
pub fn rank_formats(offers: &[Offer], height: u32, fps: u32) -> Vec<usize> {
    let smooth = |o: &Offer| o.fps >= 24;
    let cost = |f: RawFormat| match f {
        RawFormat::Nv12 => 0,
        RawFormat::Yuyv => 1,
        RawFormat::Mjpeg => 2,
        RawFormat::Rgb | RawFormat::Bgr => 3,
        RawFormat::Gray => 4,
    };
    let mut order: Vec<usize> = (0..offers.len()).collect();
    order.sort_by_key(|&i| {
        let o = &offers[i];
        let size = if o.height >= height {
            (0, o.height - height)
        } else {
            (1, height - o.height)
        };
        // How far from 16:9, in thousandths.
        let aspect = (i64::from(o.width) * 9_000 / i64::from(o.height.max(1)) - 16_000).abs();
        (
            !smooth(o),
            size,
            aspect,
            o.fps.abs_diff(fps),
            cost(o.format),
        )
    });
    order
}

// ── The desktops: nokhwa ────────────────────────────────────────────────

#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod desktop {
    use std::time::Instant;

    use nokhwa::pixel_format::RgbAFormat;
    use nokhwa::utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    };

    use super::{to_rgba, CameraError, CameraInfo, RawFormat, WANTED_FPS};
    use crate::{Frame, PixelFormat};

    fn backend_error(e: &nokhwa::NokhwaError) -> CameraError {
        let text = e.to_string();
        let lower = text.to_lowercase();
        if lower.contains("permission") || lower.contains("denied") || lower.contains("authoriz") {
            CameraError::NotAllowed
        } else {
            CameraError::Backend(text)
        }
    }

    pub fn list() -> Result<Vec<CameraInfo>, CameraError> {
        let found = nokhwa::query(ApiBackend::Auto).map_err(|e| backend_error(&e))?;
        Ok(found
            .into_iter()
            .filter_map(|c| match c.index() {
                CameraIndex::Index(i) => Some(CameraInfo {
                    index: *i,
                    name: c.human_name(),
                }),
                CameraIndex::String(_) => None,
            })
            .collect())
    }

    /// macOS asks the user once, the first time any camera is opened;
    /// the answer comes on a callback. Elsewhere there is nothing to ask.
    #[cfg(target_os = "macos")]
    fn ask_access() -> Result<(), CameraError> {
        if nokhwa::nokhwa_check() {
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        nokhwa::nokhwa_initialize(move |granted| {
            let _ = tx.send(granted);
        });
        // The prompt waits on the person: a minute is plenty.
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(true) => Ok(()),
            _ => Err(CameraError::NotAllowed),
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::unnecessary_wraps)]
    fn ask_access() -> Result<(), CameraError> {
        Ok(())
    }

    pub struct NokhwaCamera {
        camera: nokhwa::Camera,
        name: String,
        mode: String,
        size: (u32, u32),
        format: RawFormat,
        opened: Instant,
    }

    /// Every size the device lists, tallest first, without opening a
    /// stream (Settings → Camera's sizes).
    pub fn heights_of(index: u32) -> Result<Vec<u32>, CameraError> {
        ask_access()?;
        let request = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::None);
        let mut camera = nokhwa::Camera::new(CameraIndex::Index(index), request)
            .map_err(|e| backend_error(&e))?;
        Ok(heights(&mut camera))
    }

    fn heights(camera: &mut nokhwa::Camera) -> Vec<u32> {
        let mut out: Vec<u32> = listed(camera).iter().map(CameraFormat::height).collect();
        out.sort_unstable_by(|a, b| b.cmp(a));
        out.dedup();
        out
    }

    /// What the device lists, every size and rate.
    fn listed(camera: &mut nokhwa::Camera) -> Vec<CameraFormat> {
        let formats = camera.compatible_camera_formats().unwrap_or_default();
        #[cfg(target_os = "macos")]
        let formats = mac_formats(camera, formats);
        formats
    }

    /// nokhwa 0.10's `AVFoundation` backend lists, under each pixel
    /// format, the sizes of the *other* formats (its
    /// `compatible_list_by_resolution` filters with `!=`): a camera with
    /// one format — a C920 on a Mac — lists nothing, and the camera
    /// opened at whatever came first (160×90). Asked for a format that
    /// no camera there has (BGR), the same filter hands back every size
    /// and rate. `AVFoundation` converts frames to the pixel format the
    /// output asks, so each is offered as YUYV, which is what the
    /// output is then set to; a fixed nokhwa lists them properly and
    /// these only repeat them.
    #[cfg(target_os = "macos")]
    fn mac_formats(camera: &mut nokhwa::Camera, listed: Vec<CameraFormat>) -> Vec<CameraFormat> {
        let mut all: Vec<CameraFormat> = listed
            .into_iter()
            .map(|f| CameraFormat::new(f.resolution(), FrameFormat::YUYV, f.frame_rate()))
            .collect();
        if let Ok(map) = camera.compatible_list_by_resolution(FrameFormat::RAWBGR) {
            for (res, rates) in map {
                all.extend(
                    rates
                        .into_iter()
                        .map(|fps| CameraFormat::new(res, FrameFormat::YUYV, fps)),
                );
            }
        }
        all.sort_by_key(|f| (f.width(), f.height(), f.frame_rate()));
        all.dedup_by_key(|f| (f.width(), f.height(), f.frame_rate()));
        all
    }

    fn raw_of(format: FrameFormat) -> RawFormat {
        match format {
            FrameFormat::MJPEG => RawFormat::Mjpeg,
            FrameFormat::YUYV => RawFormat::Yuyv,
            FrameFormat::NV12 => RawFormat::Nv12,
            FrameFormat::GRAY => RawFormat::Gray,
            FrameFormat::RAWRGB => RawFormat::Rgb,
            FrameFormat::RAWBGR => RawFormat::Bgr,
        }
    }

    pub fn open(index: u32, height: u32) -> Result<NokhwaCamera, CameraError> {
        ask_access()?;
        // The device's own list, and our pick from it (`choose_format`).
        // nokhwa's `Closest` wants the pixel format and the exact size
        // it was given, which a Mac camera's list (NV12/YUYV, no MJPEG)
        // never has: 1.5's first 720p build could not open one.
        let listed = {
            let probe = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::None);
            let mut camera = nokhwa::Camera::new(CameraIndex::Index(index), probe)
                .map_err(|e| backend_error(&e))?;
            listed(&mut camera)
        };
        let offers: Vec<super::Offer> = listed
            .iter()
            .map(|f| super::Offer {
                width: f.width(),
                height: f.height(),
                fps: f.frame_rate(),
                format: raw_of(f.format()),
            })
            .collect();
        // Best first; a device may refuse one it listed (a rate it
        // rounds), so the next is tried, a few at most. A device that
        // lists nothing opens at whatever it starts with.
        let mut camera = None;
        let mut last_error = None;
        for i in super::rank_formats(&offers, height, WANTED_FPS)
            .into_iter()
            .take(6)
        {
            let request = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Exact(listed[i]));
            match nokhwa::Camera::new(CameraIndex::Index(index), request) {
                Ok(c) => {
                    camera = Some(c);
                    break;
                }
                Err(e) => {
                    log::info!("camera: {} refused: {e}", listed[i]);
                    last_error = Some(e);
                }
            }
        }
        let mut camera = match (camera, last_error) {
            (Some(c), _) => c,
            (None, Some(e)) => return Err(backend_error(&e)),
            (None, None) => {
                let request = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::None);
                nokhwa::Camera::new(CameraIndex::Index(index), request)
                    .map_err(|e| backend_error(&e))?
            }
        };
        camera.open_stream().map_err(|e| backend_error(&e))?;
        let format = camera.camera_format();
        let raw = raw_of(format.format());
        let name = camera.info().human_name();
        let mode = format!(
            "{}x{} {} fps {:?}",
            format.width(),
            format.height(),
            format.frame_rate(),
            raw
        );
        log::info!("camera: {name} opened at {mode}");
        Ok(NokhwaCamera {
            mode,
            size: (format.width(), format.height()),
            camera,
            name,
            format: raw,
            opened: Instant::now(),
        })
    }

    impl super::Camera for NokhwaCamera {
        fn frame(&mut self) -> Result<Frame, CameraError> {
            let buffer = self.camera.frame().map_err(|e| backend_error(&e))?;
            let res = buffer.resolution();
            let (data, width, height) =
                to_rgba(self.format, res.width(), res.height(), buffer.buffer())?;
            self.size = (width, height);
            Ok(Frame {
                data,
                width,
                height,
                timestamp_ms: u64::try_from(self.opened.elapsed().as_millis()).unwrap_or(u64::MAX),
                format: PixelFormat::Rgba,
            })
        }

        fn size(&self) -> (u32, u32) {
            self.size
        }

        fn name(&self) -> &str {
            &self.name
        }

        fn heights(&mut self) -> Vec<u32> {
            heights(&mut self.camera)
        }

        fn mode(&self) -> String {
            self.mode.clone()
        }
    }

    impl Drop for NokhwaCamera {
        fn drop(&mut self) {
            let _ = self.camera.stop_stream();
        }
    }
}

/// The cameras the system knows of, in its order.
///
/// # Errors
/// The platform's camera API failed, or there is none here yet.
pub fn list_cameras() -> Result<Vec<CameraInfo>, CameraError> {
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        desktop::list()
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        Err(CameraError::Unsupported)
    }
}

/// Open camera `index` (a [`CameraInfo::index`]) at the size closest
/// to `height` (16:9, 30 fps); on macOS this may show the system's
/// permission prompt and wait for the answer.
///
/// # Errors
/// No such camera, access refused, or the device would not start.
pub fn open(index: u32, height: u32) -> Result<Box<dyn Camera>, CameraError> {
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        Ok(Box::new(desktop::open(index, height)?))
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        let _ = (index, height);
        Err(CameraError::Unsupported)
    }
}

/// The heights camera `index` can deliver, tallest first.
///
/// # Errors
/// No such camera, or access refused.
pub fn heights(index: u32) -> Result<Vec<u32>, CameraError> {
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        desktop::heights_of(index)
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        let _ = index;
        Err(CameraError::Unsupported)
    }
}

/// Shrink an RGBA frame by whole steps until it fits `max_w × max_h`,
/// for the self-view: a 640×360 picture costs the UI a quarter of the
/// copy at 320×180. Nearest-neighbour; the tile is small.
#[must_use]
pub fn downscale_rgba(frame: &Frame, max_w: u32, max_h: u32) -> (Vec<u8>, u32, u32) {
    let step = (frame.width.div_ceil(max_w.max(1)))
        .max(frame.height.div_ceil(max_h.max(1)))
        .max(1);
    let (w, h) = (frame.width / step, frame.height / step);
    let src_w = frame.width as usize;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = y * step as usize * src_w;
        for x in 0..w as usize {
            let i = (row + x * step as usize) * 4;
            match frame.data.get(i..i + 4) {
                Some(px) if frame.format == PixelFormat::Rgba => out.extend_from_slice(px),
                Some(px) => out.extend_from_slice(&[px[2], px[1], px[0], px[3]]),
                None => out.extend_from_slice(&[0, 0, 0, 255]),
            }
        }
    }
    (out, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuyv_grey_and_colours_land_where_bt601_puts_them() {
        // Two pixels of mid grey (Y 126, no colour), as a camera sends
        // a blank wall.
        let (rgba, w, h) = to_rgba(RawFormat::Yuyv, 2, 1, &[126, 128, 126, 128]).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(&rgba[..4], &[128, 128, 128, 255]);
        assert_eq!(&rgba[4..], &[128, 128, 128, 255]);
        // Studio white and black.
        let (white, ..) = to_rgba(RawFormat::Yuyv, 2, 1, &[235, 128, 16, 128]).unwrap();
        assert_eq!(&white[..4], &[255, 255, 255, 255]);
        assert_eq!(&white[4..], &[0, 0, 0, 255]);
        // Strong V: red.
        let (red, ..) = to_rgba(RawFormat::Yuyv, 2, 1, &[81, 90, 81, 240]).unwrap();
        assert!(red[0] > 240 && red[1] < 10 && red[2] < 10, "{red:?}");
    }

    #[test]
    fn nv12_shares_one_chroma_pair_per_two_by_two() {
        // 2x2: four luma, one U/V pair.
        let data = [235, 235, 16, 16, 128, 128];
        let (rgba, ..) = to_rgba(RawFormat::Nv12, 2, 2, &data).unwrap();
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&rgba[8..12], &[0, 0, 0, 255]);
    }

    #[test]
    fn rgb_bgr_and_grey_expand_to_rgba() {
        let (rgb, ..) = to_rgba(RawFormat::Rgb, 1, 1, &[1, 2, 3]).unwrap();
        assert_eq!(rgb, [1, 2, 3, 255]);
        let (bgr, ..) = to_rgba(RawFormat::Bgr, 1, 1, &[1, 2, 3]).unwrap();
        assert_eq!(bgr, [3, 2, 1, 255]);
        let (g, ..) = to_rgba(RawFormat::Gray, 1, 1, &[9]).unwrap();
        assert_eq!(g, [9, 9, 9, 255]);
    }

    #[test]
    fn a_short_buffer_is_an_error_not_a_panic() {
        assert!(to_rgba(RawFormat::Yuyv, 4, 4, &[0; 10]).is_err());
        assert!(to_rgba(RawFormat::Nv12, 4, 4, &[0; 10]).is_err());
        assert!(to_rgba(RawFormat::Mjpeg, 4, 4, &[0; 10]).is_err());
    }

    fn offer(width: u32, height: u32, fps: u32, format: RawFormat) -> Offer {
        Offer {
            width,
            height,
            fps,
            format,
        }
    }

    #[test]
    fn a_mac_camera_without_mjpeg_opens_at_the_size_asked() {
        // What a C920 lists on macOS: no MJPEG, 16:9 and 4:3 sizes.
        let offers = [
            offer(640, 480, 30, RawFormat::Yuyv),
            offer(640, 360, 30, RawFormat::Yuyv),
            offer(1280, 720, 30, RawFormat::Nv12),
            offer(1280, 720, 30, RawFormat::Yuyv),
            offer(1920, 1080, 30, RawFormat::Nv12),
        ];
        assert_eq!(choose_format(&offers, 720, 30), Some(2));
        assert_eq!(choose_format(&offers, 1080, 30), Some(4));
        // 480p: 640x480 is exactly as tall; the shape only breaks ties.
        assert_eq!(choose_format(&offers, 480, 30), Some(0));
    }

    #[test]
    fn usb_2_takes_mjpeg_for_a_smooth_720p_and_a_small_camera_its_tallest() {
        let offers = [
            offer(1280, 720, 10, RawFormat::Yuyv),
            offer(1280, 720, 30, RawFormat::Mjpeg),
            offer(640, 360, 30, RawFormat::Yuyv),
        ];
        assert_eq!(choose_format(&offers, 720, 30), Some(1));
        let small = [
            offer(320, 240, 30, RawFormat::Yuyv),
            offer(160, 120, 30, RawFormat::Yuyv),
        ];
        assert_eq!(choose_format(&small, 720, 30), Some(0));
        // Only slow offers: still one.
        let slow = [offer(1280, 720, 10, RawFormat::Yuyv)];
        assert_eq!(choose_format(&slow, 720, 30), Some(0));
        assert_eq!(choose_format(&[], 720, 30), None);
    }

    #[test]
    fn the_ranking_puts_smooth_rates_first_and_keeps_every_offer() {
        let offers = [
            offer(160, 90, 30, RawFormat::Yuyv),
            offer(1280, 720, 30, RawFormat::Yuyv),
            offer(1280, 720, 5, RawFormat::Yuyv),
            offer(1920, 1080, 30, RawFormat::Yuyv),
        ];
        let order = rank_formats(&offers, 720, 30);
        assert_eq!(order, vec![1, 3, 0, 2]);
    }

    #[test]
    fn the_self_view_is_shrunk_by_whole_steps() {
        let frame = Frame {
            data: vec![7; 640 * 360 * 4],
            width: 640,
            height: 360,
            timestamp_ms: 0,
            format: PixelFormat::Rgba,
        };
        let (px, w, h) = downscale_rgba(&frame, 320, 180);
        assert_eq!((w, h), (320, 180));
        assert_eq!(px.len(), 320 * 180 * 4);
    }
}
