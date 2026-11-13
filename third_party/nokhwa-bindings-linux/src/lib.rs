/*
 * Copyright 2022 l1npengtul <l1npengtul@protonmail.com> / The Nokhwa Contributors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#[cfg(target_os = "linux")]
mod internal {
    use nokhwa_core::{
        buffer::Buffer,
        error::NokhwaError,
        traits::CaptureBackendTrait,
        types::{
            ApiBackend, CameraControl, CameraFormat, CameraIndex, CameraInfo,
            ControlValueDescription, ControlValueSetter, FrameFormat, KnownCameraControl,
            KnownCameraControlFlag, RequestedFormat, RequestedFormatType, Resolution,
        },
    };
    use std::{
        borrow::Cow,
        collections::HashMap,
        io::{self, ErrorKind},
    };
    use v4l::v4l_sys::{
        V4L2_CID_BACKLIGHT_COMPENSATION, V4L2_CID_BRIGHTNESS, V4L2_CID_CONTRAST, V4L2_CID_EXPOSURE,
        V4L2_CID_FOCUS_RELATIVE, V4L2_CID_GAIN, V4L2_CID_GAMMA, V4L2_CID_HUE,
        V4L2_CID_IRIS_RELATIVE, V4L2_CID_PAN_RELATIVE, V4L2_CID_SATURATION, V4L2_CID_SHARPNESS,
        V4L2_CID_TILT_RELATIVE, V4L2_CID_WHITE_BALANCE_TEMPERATURE, V4L2_CID_ZOOM_RELATIVE,
    };
    use v4l::{
        control::{Control, Flags, Type, Value},
        fraction::Fraction,
        frameinterval::FrameIntervalEnum,
        framesize::FrameSizeEnum,
        io::traits::{CaptureStream, Stream},
        prelude::MmapStream,
        video::{capture::Parameters, Capture},
        Device, Format, FourCC,
    };

    /// Attempts to convert a [`KnownCameraControl`] into a V4L2 Control ID.
    /// If the associated control is not found, this will return `None` (`ColorEnable`, `Roll`)
    #[allow(clippy::cast_possible_truncation)]
    pub fn known_camera_control_to_id(ctrl: KnownCameraControl) -> u32 {
        match ctrl {
            KnownCameraControl::Brightness => V4L2_CID_BRIGHTNESS,
            KnownCameraControl::Contrast => V4L2_CID_CONTRAST,
            KnownCameraControl::Hue => V4L2_CID_HUE,
            KnownCameraControl::Saturation => V4L2_CID_SATURATION,
            KnownCameraControl::Sharpness => V4L2_CID_SHARPNESS,
            KnownCameraControl::Gamma => V4L2_CID_GAMMA,
            KnownCameraControl::WhiteBalance => V4L2_CID_WHITE_BALANCE_TEMPERATURE,
            KnownCameraControl::BacklightComp => V4L2_CID_BACKLIGHT_COMPENSATION,
            KnownCameraControl::Gain => V4L2_CID_GAIN,
            KnownCameraControl::Pan => V4L2_CID_PAN_RELATIVE,
            KnownCameraControl::Tilt => V4L2_CID_TILT_RELATIVE,
            KnownCameraControl::Zoom => V4L2_CID_ZOOM_RELATIVE,
            KnownCameraControl::Exposure => V4L2_CID_EXPOSURE,
            KnownCameraControl::Iris => V4L2_CID_IRIS_RELATIVE,
            KnownCameraControl::Focus => V4L2_CID_FOCUS_RELATIVE,
            KnownCameraControl::Other(id) => id as u32,
        }
    }

    /// Attempts to convert a [`u32`] V4L2 Control ID into a [`KnownCameraControl`]
    /// If the associated control is not found, this will return `None` (`ColorEnable`, `Roll`)
    #[allow(clippy::cast_lossless)]
    pub fn id_to_known_camera_control(id: u32) -> KnownCameraControl {
        match id {
            V4L2_CID_BRIGHTNESS => KnownCameraControl::Brightness,
            V4L2_CID_CONTRAST => KnownCameraControl::Contrast,
            V4L2_CID_HUE => KnownCameraControl::Hue,
            V4L2_CID_SATURATION => KnownCameraControl::Saturation,
            V4L2_CID_SHARPNESS => KnownCameraControl::Sharpness,
            V4L2_CID_GAMMA => KnownCameraControl::Gamma,
            V4L2_CID_WHITE_BALANCE_TEMPERATURE => KnownCameraControl::WhiteBalance,
            V4L2_CID_BACKLIGHT_COMPENSATION => KnownCameraControl::BacklightComp,
            V4L2_CID_GAIN => KnownCameraControl::Gain,
            V4L2_CID_PAN_RELATIVE => KnownCameraControl::Pan,
            V4L2_CID_TILT_RELATIVE => KnownCameraControl::Tilt,
            V4L2_CID_ZOOM_RELATIVE => KnownCameraControl::Zoom,
            V4L2_CID_EXPOSURE => KnownCameraControl::Exposure,
            V4L2_CID_IRIS_RELATIVE => KnownCameraControl::Iris,
            V4L2_CID_FOCUS_RELATIVE => KnownCameraControl::Focus,
            id => KnownCameraControl::Other(id as u128),
        }
    }

    /// query v4l2 cameras
    #[allow(clippy::unnecessary_wraps)]
    #[allow(clippy::cast_possible_truncation)]
    pub fn query() -> Result<Vec<CameraInfo>, NokhwaError> {
        Ok({
            // BAUCORD-PATCH: capture nodes only, in /dev/videoN order.
            // A USB camera has a second node for metadata, by the same
            // name and with no picture formats; `/dev` lists the nodes
            // in no set order, so a pick by name could land on it and
            // every open failed with "Failed to Fufill".
            let mut nodes: Vec<_> = v4l::context::enum_devices()
                .into_iter()
                .filter(|node| captures(node.path()))
                .collect();
            nodes.sort_by_key(v4l::context::Node::index);
            let camera_info: Vec<CameraInfo> = nodes
                .iter()
                .map(|node| {
                    CameraInfo::new(
                        &node
                            .name()
                            .unwrap_or(format!("{}", node.path().to_string_lossy())),
                        &format!("Video4Linux Device @ {}", node.path().to_string_lossy()),
                        "",
                        CameraIndex::Index(node.index() as u32),
                    )
                })
                .collect();
            camera_info
        })
    }

    /// BAUCORD-PATCH: whether a `/dev/video*` node captures pictures
    /// (`V4L2_CAP_VIDEO_CAPTURE` among its own capabilities). One that
    /// cannot be opened to ask is kept: opening it is what will say why.
    fn captures(path: &std::path::Path) -> bool {
        match Device::with_path(path).and_then(|d| d.query_caps()) {
            Ok(caps) => {
                let yes = caps
                    .capabilities
                    .contains(v4l::capability::Flags::VIDEO_CAPTURE);
                if !yes {
                    log::debug!(
                        "camera: {} ({}) is not a capture node, not listed",
                        path.display(),
                        caps.card
                    );
                }
                yes
            }
            Err(_) => true,
        }
    }

    type SharedDevice = std::sync::Arc<std::sync::Mutex<Device>>;
    type WeakSharedDevice = std::sync::Weak<std::sync::Mutex<Device>>;

    struct WeakSharedDeviceEntry {
        device: WeakSharedDevice,
        index: usize,
    }

    type SharedDeviceList = std::sync::OnceLock<std::sync::Mutex<Vec<WeakSharedDeviceEntry>>>;

    /// Global array that keep track of every Device that are currently open.
    /// This is used to open multiple handle to the same device.
    /// This is a workaround for the fact that the v4l2 backend does not support multiple handles to the same device.
    /// This replicate behavior of MF backend.
    /// The reference is a reference of Weak<Mutex<Device>>, so that the Device can be dropped when the last handle is closed.
    /// This list might need some cleanup because it will accumulate every device that is opened, this should not be a problem because the list should not grow too much.
    /// The list is also protected by a mutex, so it should be thread safe.
    static DEVICES: SharedDeviceList = std::sync::OnceLock::new();

    fn cleanup_dropped_devices(devices: &mut Vec<WeakSharedDeviceEntry>) {
        devices.retain(|entry| entry.device.strong_count() > 0);
    }

    fn new_shared_device(index: usize) -> Result<SharedDevice, NokhwaError> {
        let mut devices = DEVICES
            .get_or_init(|| std::sync::Mutex::new(Vec::new()))
            .lock()
            .map_err(|e| NokhwaError::InitializeError {
                backend: ApiBackend::Video4Linux,
                error: format!("Fail to lock global device list mutex: {}", e),
            })?;

        // do some cleanup, this will avoid here memory to grow forever
        // if for some reason someone has tons of camera plugged in
        cleanup_dropped_devices(&mut devices);

        if let Some(entry) = devices.iter().find(|entry| entry.index == index) {
            if let Some(device) = entry.device.upgrade() {
                return Ok(device);
            }
        }

        // Cleanup a second, the device we are interested might have been dropped during before upgrade call
        // For this point on we are assured that the device is not in the list
        cleanup_dropped_devices(&mut devices);

        // Let's be extra sure, this code should never panic, but maybe will help catch some race condition
        assert!(
            devices.iter().find(|entry| entry.index == index).is_none(),
            "Device {index} should not be in the list"
        );

        // Now we can open the device, and never run into a busy io error,
        // as long as the device isn't opened by other programs.
        let device = match Device::new(index) {
            Ok(dev) => dev,
            Err(why) => {
                return Err(NokhwaError::OpenDeviceError(
                    index.to_string(),
                    format!("V4L2 Error: {}", why),
                ))
            }
        };

        let device = std::sync::Arc::new(std::sync::Mutex::new(device));
        devices.push(WeakSharedDeviceEntry {
            device: std::sync::Arc::downgrade(&device),
            index,
        });

        // Last check to be sure that every devices have a unique index
        // and that the data isn't corrupted
        if devices.len() > 1 {
            let indices: std::collections::HashSet<_> = devices.iter().map(|d| d.index).collect();
            assert_eq!(
                indices.len(),
                devices.len(),
                "Device list should not contain duplicate indexes"
            );
        }

        Ok(device)
    }

    // ── BAUCORD-PATCH helpers (BAUCORD-PATCH.md) ─────────────────────────

    /// BAUCORD-PATCH: frames a second for a V4L2 frame interval (seconds
    /// a frame), rounded. Upstream kept only intervals over 1 (`1/30`):
    /// 1001/30000 (29.97 fps) was dropped, and a camera that states
    /// every rate that way listed nothing ("Failed to Fufill").
    fn fps_of(interval: Fraction) -> u32 {
        if interval.numerator == 0 {
            return 0;
        }
        let (n, d) = (
            u64::from(interval.numerator),
            u64::from(interval.denominator),
        );
        u32::try_from((d + n / 2) / n).unwrap_or(u32::MAX)
    }

    /// The rates offered from within a stepwise (continuous) range.
    const COMMON_RATES: [u32; 9] = [60, 50, 30, 25, 24, 20, 15, 10, 5];

    /// BAUCORD-PATCH: the rates one frame-interval entry offers. A
    /// stepwise range (virtual cameras, v4l2loopback) gives its ends and
    /// the common rates between them; upstream walked its numerators and
    /// listed rates such as 1 fps, or nothing.
    fn rates_of(interval: &FrameIntervalEnum) -> Vec<u32> {
        let mut out = match interval {
            FrameIntervalEnum::Discrete(d) => vec![fps_of(*d)],
            FrameIntervalEnum::Stepwise(s) => {
                let fastest = fps_of(s.min);
                let slowest = fps_of(s.max);
                let mut out: Vec<u32> = COMMON_RATES
                    .iter()
                    .copied()
                    .filter(|r| *r <= fastest && *r >= slowest)
                    .collect();
                out.push(fastest);
                out
            }
        };
        out.retain(|r| *r > 0);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The sizes offered from within a stepwise (continuous) range.
    const COMMON_SIZES: [(u32, u32); 9] = [
        (3840, 2160),
        (2560, 1440),
        (1920, 1080),
        (1280, 720),
        (960, 540),
        (854, 480),
        (640, 480),
        (640, 360),
        (320, 240),
    ];

    /// BAUCORD-PATCH: the sizes one frame-size entry offers. A stepwise
    /// range gives the common sizes on its grid (its largest too when
    /// that is no bigger than 4K); upstream paired widths and heights
    /// along the diagonal, never reaching the largest.
    fn sizes_of(size: &FrameSizeEnum) -> Vec<Resolution> {
        match size {
            FrameSizeEnum::Discrete(d) => vec![Resolution::new(d.width, d.height)],
            FrameSizeEnum::Stepwise(s) => {
                let on_grid = |w: u32, h: u32| {
                    (s.min_width..=s.max_width).contains(&w)
                        && (s.min_height..=s.max_height).contains(&h)
                        && (s.step_width <= 1 || (w - s.min_width) % s.step_width == 0)
                        && (s.step_height <= 1 || (h - s.min_height) % s.step_height == 0)
                };
                let mut out: Vec<Resolution> = COMMON_SIZES
                    .iter()
                    .filter(|(w, h)| on_grid(*w, *h))
                    .map(|(w, h)| Resolution::new(*w, *h))
                    .collect();
                if out.is_empty() || (s.max_width <= 3840 && s.max_height <= 2160) {
                    out.push(Resolution::new(s.max_width, s.max_height));
                }
                out.sort();
                out.dedup();
                out
            }
        }
    }

    /// BAUCORD-PATCH: the planar 4:2:0 layouts read as NV12 (their
    /// chroma planes interleaved on the way out, `planar_to_nv12`): what
    /// virtual cameras and v4l2loopback commonly offer, and nothing
    /// upstream could read. The flag: V's plane comes before U's.
    fn planar_420(fourcc: FourCC) -> Option<bool> {
        match fourcc.str().ok()? {
            "YU12" => Some(false),
            "YV12" => Some(true),
            _ => None,
        }
    }

    /// The device's pixel formats.
    fn offered_fourccs(device: &Device) -> Vec<FourCC> {
        device
            .enum_formats()
            .map(|all| all.into_iter().map(|f| f.fourcc).collect())
            .unwrap_or_default()
    }

    /// BAUCORD-PATCH: the FourCC to ask the device for `format`, and
    /// whether frames then need `planar_to_nv12`: NV12 on a device that
    /// has none but has YU12 or YV12 is read from those.
    fn device_fourcc(offered: &[FourCC], format: FrameFormat) -> (FourCC, Option<bool>) {
        if format == FrameFormat::NV12 && !offered.contains(&FourCC::new(b"NV12")) {
            if let Some((fcc, v_first)) = offered
                .iter()
                .find_map(|f| planar_420(*f).map(|v| (*f, v)))
            {
                return (fcc, Some(v_first));
            }
        }
        (frameformat_to_fourcc(format), None)
    }

    /// BAUCORD-PATCH: what a device FourCC is listed as: upstream's six,
    /// and the planar 4:2:0 layout `device_fourcc` would pick, as NV12.
    fn listed_format(offered: &[FourCC], fourcc: FourCC) -> Option<FrameFormat> {
        fourcc_to_frameformat(fourcc).or_else(|| {
            (planar_420(fourcc).is_some()
                && device_fourcc(offered, FrameFormat::NV12).0 == fourcc)
                .then_some(FrameFormat::NV12)
        })
    }

    /// BAUCORD-PATCH: a planar 4:2:0 picture (Y, then the two chroma
    /// planes) as NV12 (Y, then U and V interleaved). `None` when the
    /// buffer is shorter than the size says.
    fn planar_to_nv12(data: &[u8], width: u32, height: u32, v_first: bool) -> Option<Vec<u8>> {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (luma, chroma) = (w * h, cw * ch);
        if data.len() < luma + 2 * chroma {
            return None;
        }
        let first = &data[luma..luma + chroma];
        let second = &data[luma + chroma..luma + 2 * chroma];
        let (u, v) = if v_first { (second, first) } else { (first, second) };
        let mut out = Vec::with_capacity(luma + 2 * chroma);
        out.extend_from_slice(&data[..luma]);
        for (a, b) in u.iter().zip(v) {
            out.push(*a);
            out.push(*b);
        }
        Some(out)
    }

    /// BAUCORD-PATCH: the helpers above.
    #[cfg(test)]
    mod baucord_tests {
        use super::*;
        use v4l::{frameinterval, framesize};

        fn frac(numerator: u32, denominator: u32) -> Fraction {
            Fraction {
                numerator,
                denominator,
            }
        }

        #[test]
        fn fractional_rates_are_rounded_not_dropped() {
            assert_eq!(fps_of(frac(1, 30)), 30);
            assert_eq!(fps_of(frac(1001, 30000)), 30);
            assert_eq!(fps_of(frac(1001, 60000)), 60);
            assert_eq!(fps_of(frac(333_333, 10_000_000)), 30);
            assert_eq!(fps_of(frac(0, 30)), 0);
        }

        #[test]
        fn a_stepwise_rate_range_offers_its_common_rates() {
            let range = FrameIntervalEnum::Stepwise(frameinterval::Stepwise {
                min: frac(1, 60),
                max: frac(1, 1),
                step: frac(1, 1),
            });
            assert_eq!(rates_of(&range), vec![5, 10, 15, 20, 24, 25, 30, 50, 60]);
            let one = FrameIntervalEnum::Discrete(frac(1001, 30000));
            assert_eq!(rates_of(&one), vec![30]);
        }

        #[test]
        fn a_stepwise_size_range_offers_the_common_sizes_on_its_grid() {
            let range = FrameSizeEnum::Stepwise(framesize::Stepwise {
                min_width: 2,
                max_width: 8192,
                step_width: 2,
                min_height: 2,
                max_height: 8192,
                step_height: 2,
            });
            let sizes = sizes_of(&range);
            assert!(sizes.contains(&Resolution::new(1280, 720)));
            assert!(sizes.contains(&Resolution::new(1920, 1080)));
            // 854 is on a step-2 grid from 2; the 8K corner is left out.
            assert!(sizes.contains(&Resolution::new(854, 480)));
            assert!(!sizes.contains(&Resolution::new(8192, 8192)));
            let odd = FrameSizeEnum::Stepwise(framesize::Stepwise {
                min_width: 100,
                max_width: 700,
                step_width: 3,
                min_height: 100,
                max_height: 500,
                step_height: 3,
            });
            assert_eq!(sizes_of(&odd), vec![Resolution::new(700, 500)]);
        }

        #[test]
        fn yu12_is_read_as_nv12_only_when_the_device_has_no_nv12() {
            let yu12 = FourCC::new(b"YU12");
            let nv12 = FourCC::new(b"NV12");
            let mjpg = FourCC::new(b"MJPG");
            assert_eq!(listed_format(&[mjpg, yu12], yu12), Some(FrameFormat::NV12));
            assert_eq!(device_fourcc(&[mjpg, yu12], FrameFormat::NV12), (yu12, Some(false)));
            assert_eq!(listed_format(&[nv12, yu12], yu12), None);
            assert_eq!(device_fourcc(&[nv12, yu12], FrameFormat::NV12), (nv12, None));
            assert_eq!(listed_format(&[FourCC::new(b"RGB4")], FourCC::new(b"RGB4")), None);
        }

        #[test]
        fn planar_chroma_is_interleaved() {
            // 2x2: four Y, one U, one V.
            let yu12 = [1, 2, 3, 4, 10, 20];
            assert_eq!(planar_to_nv12(&yu12, 2, 2, false), Some(vec![1, 2, 3, 4, 10, 20]));
            let yv12 = [1, 2, 3, 4, 20, 10];
            assert_eq!(planar_to_nv12(&yv12, 2, 2, true), Some(vec![1, 2, 3, 4, 10, 20]));
            // 4x2: U planes [a b], V [c d] → a c b d.
            let wide = [0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4];
            assert_eq!(
                planar_to_nv12(&wide, 4, 2, false).map(|v| v[8..].to_vec()),
                Some(vec![1, 3, 2, 4])
            );
            assert_eq!(planar_to_nv12(&yu12[..5], 2, 2, false), None);
        }
    }

    fn get_device_format(device: &Device) -> Result<CameraFormat, NokhwaError> {
        match device.format() {
            Ok(format) => {
                // BAUCORD-PATCH: YU12/YV12 read as NV12 (`planar_420`).
                let frame_format = fourcc_to_frameformat(format.fourcc)
                    .or_else(|| planar_420(format.fourcc).map(|_| FrameFormat::NV12))
                    .ok_or(NokhwaError::GetPropertyError {
                        property: "FrameFormat".to_string(),
                        error: format!("unsupported ({})", format.fourcc),
                    })?;

                // BAUCORD-PATCH: rounded (`fps_of`); a rate such as
                // 1001/30000 was an error here, after the format was set.
                let fps = match device.params() {
                    Ok(params) => fps_of(params.interval),
                    Err(why) => {
                        return Err(NokhwaError::GetPropertyError {
                            property: "V4L2 FrameRate".to_string(),
                            error: why.to_string(),
                        })
                    }
                };

                Ok(CameraFormat::new(
                    Resolution::new(format.width, format.height),
                    frame_format,
                    fps,
                ))
            }
            Err(why) => Err(NokhwaError::GetPropertyError {
                property: "parameters".to_string(),
                error: why.to_string(),
            }),
        }
    }

    /// The backend struct that interfaces with V4L2.
    /// To see what this does, please see [`CaptureBackendTrait`].
    /// # Quirks
    /// - Calling [`set_resolution()`](CaptureBackendTrait::set_resolution), [`set_frame_rate()`](CaptureBackendTrait::set_frame_rate), or [`set_frame_format()`](CaptureBackendTrait::set_frame_format) each internally calls [`set_camera_format()`](CaptureBackendTrait::set_camera_format).
    pub struct V4LCaptureDevice<'a> {
        camera_format: CameraFormat,
        camera_info: CameraInfo,
        device: SharedDevice,
        stream_handle: Option<MmapStream<'a>>,
        /// BAUCORD-PATCH: the device sends planar 4:2:0 (YU12, or YV12
        /// when `Some(true)`), shown to nokhwa as NV12 (`planar_420`).
        planar: Option<bool>,
    }

    /// BAUCORD-PATCH: same size and pixel format, whatever the rate.
    fn same_picture(a: CameraFormat, b: CameraFormat) -> bool {
        a.resolution() == b.resolution() && a.format() == b.format()
    }

    impl<'a> V4LCaptureDevice<'a> {
        /// Creates a new capture device using the `V4L2` backend. Indexes are gives to devices by the OS, and usually numbered by order of discovery.
        /// # Errors
        /// This function will error if the camera is currently busy or if `V4L2` can't read device information.
        #[allow(clippy::too_many_lines)]
        pub fn new(index: &CameraIndex, cam_fmt: RequestedFormat) -> Result<Self, NokhwaError> {
            let index = index.clone();

            let shared_device = new_shared_device(index.as_index()? as usize)?;
            let device = shared_device
                .lock()
                .map_err(|e| NokhwaError::InitializeError {
                    backend: ApiBackend::Video4Linux,
                    error: format!("Fail to lock device mutex: {}", e),
                })?;

            // get all formats
            // get all fcc
            let mut camera_formats = vec![];

            let frame_formats = match device.enum_formats() {
                Ok(formats) => {
                    let mut frame_format_vec = vec![];
                    formats
                        .iter()
                        .for_each(|fmt| frame_format_vec.push(fmt.fourcc));
                    frame_format_vec.dedup();
                    Ok(frame_format_vec)
                }
                Err(why) => Err(NokhwaError::GetPropertyError {
                    property: "FrameFormat".to_string(),
                    error: why.to_string(),
                }),
            }?;

            for &ff in &frame_formats {
                // BAUCORD-PATCH: YU12/YV12 as NV12, and what is left out
                // is said.
                let framefmt = match listed_format(&frame_formats, ff) {
                    Some(s) => s,
                    None => {
                        log::info!("camera: {ff} is not a format nokhwa reads, skipped");
                        continue;
                    }
                };
                // i write unmaintainable blobs of code because i am so cute uwu~~
                let mut formats = device
                    .enum_framesizes(ff)
                    .map_err(|why| NokhwaError::GetPropertyError {
                        property: "ResolutionList".to_string(),
                        error: why.to_string(),
                    })?
                    .into_iter()
                    // BAUCORD-PATCH: `sizes_of`.
                    .flat_map(|x| sizes_of(&x.size))
                    .flat_map(|res| {
                        device
                            .enum_frameintervals(ff, res.x(), res.y())
                            .unwrap_or_default()
                            .into_iter()
                            // BAUCORD-PATCH: `rates_of`.
                            .flat_map(|x| rates_of(&x.interval))
                            .map(move |fps| CameraFormat::new(res, framefmt, fps))
                    })
                    .collect::<Vec<CameraFormat>>();
                camera_formats.append(&mut formats);
            }

            let format = cam_fmt
                .fulfill(&camera_formats)
                // BAUCORD-PATCH: how many were listed, for the log.
                .ok_or_else(|| NokhwaError::GetPropertyError {
                    property: "CameraFormat".to_string(),
                    error: format!(
                        "Failed to Fufill ({} formats listed)",
                        camera_formats.len()
                    ),
                })?;

            // BAUCORD-PATCH: a current format this crate cannot read
            // (YU12 on a virtual camera, a fractional rate) is replaced,
            // not an error; YU12/YV12 are asked for when NV12 is.
            let current_format = get_device_format(&device).ok();
            let (fourcc, planar) = device_fourcc(&frame_formats, format.format());
            let current_fourcc = device.format().ok().map(|f| f.fourcc);

            if current_format.map_or(true, |c| {
                c.width() != format.width() || c.height() != format.height()
            }) || current_fourcc != Some(fourcc)
            {
                if let Err(why) = device.set_format(&Format::new(
                    format.width(),
                    format.height(),
                    fourcc,
                )) {
                    return Err(NokhwaError::SetPropertyError {
                        property: "Resolution, FrameFormat".to_string(),
                        value: format.to_string(),
                        error: why.to_string(),
                    });
                }
            }

            if current_format.map_or(true, |c| c.frame_rate() != format.frame_rate()) {
                if let Err(why) = device.set_params(&Parameters::with_fps(format.frame_rate())) {
                    return Err(NokhwaError::SetPropertyError {
                        property: "Frame rate".to_string(),
                        value: format.frame_rate().to_string(),
                        error: why.to_string(),
                    });
                }
            }

            let device_caps = device
                .query_caps()
                .map_err(|why| NokhwaError::GetPropertyError {
                    property: "Device Capabilities".to_string(),
                    error: why.to_string(),
                })?;

            drop(device);

            let mut v4l2 = V4LCaptureDevice {
                camera_format: format,
                camera_info: CameraInfo::new(
                    &device_caps.card,
                    &device_caps.driver,
                    &format!("{} {:?}", device_caps.bus, device_caps.version),
                    index,
                ),
                device: shared_device,
                stream_handle: None,
                planar,
            };

            v4l2.force_refresh_camera_format()?;
            // BAUCORD-PATCH: the size and pixel format must be the ones
            // asked for; the rate is what the driver settled on (a camera
            // asked for 30 may run at 29.97, or at its nearest).
            if !same_picture(v4l2.camera_format(), format) {
                return Err(NokhwaError::SetPropertyError {
                    property: "CameraFormat".to_string(),
                    value: String::new(),
                    error: "Not same/Rejected".to_string(),
                });
            }

            Ok(v4l2)
        }

        /// Create a new `V4L2` Camera with desired settings. This may or may not work.
        /// # Errors
        /// This function will error if the camera is currently busy or if `V4L2` can't read device information.
        #[deprecated(since = "0.10.0", note = "please use `new` instead.")]
        #[allow(clippy::needless_pass_by_value)]
        pub fn new_with(
            index: CameraIndex,
            width: u32,
            height: u32,
            fps: u32,
            fourcc: FrameFormat,
        ) -> Result<Self, NokhwaError> {
            let camera_format = CameraFormat::new_from(width, height, fourcc, fps);
            V4LCaptureDevice::new(
                &index,
                RequestedFormat::with_formats(
                    RequestedFormatType::Exact(camera_format),
                    vec![camera_format.format()].as_slice(),
                ),
            )
        }

        fn lock_device(&self) -> Result<std::sync::MutexGuard<'_, Device>, NokhwaError> {
            self.device
                .lock()
                .map_err(|e| NokhwaError::GeneralError(format!("Failed to lock device: {}", e)))
        }

        fn get_resolution_list(&self, fourcc: FrameFormat) -> Result<Vec<Resolution>, NokhwaError> {
            let device = self.lock_device()?;
            // BAUCORD-PATCH: the FourCC the device really has
            // (`device_fourcc`), and `sizes_of`.
            let format = device_fourcc(&offered_fourccs(&device), fourcc).0;

            match device.enum_framesizes(format) {
                Ok(frame_sizes) => {
                    let mut resolutions = vec![];
                    for frame_size in frame_sizes {
                        resolutions.extend(sizes_of(&frame_size.size));
                    }
                    Ok(resolutions)
                }
                Err(why) => Err(NokhwaError::GetPropertyError {
                    property: "Resolutions".to_string(),
                    error: why.to_string(),
                }),
            }
        }

        /// Force refreshes the inner [`CameraFormat`] state.
        /// # Errors
        /// If the internal representation in the driver is invalid, this will error.
        pub fn force_refresh_camera_format(&mut self) -> Result<(), NokhwaError> {
            let camera_format = get_device_format(&*self.lock_device()?)?;
            self.camera_format = camera_format;
            Ok(())
        }
    }

    impl<'a> CaptureBackendTrait for V4LCaptureDevice<'a> {
        fn backend(&self) -> ApiBackend {
            ApiBackend::Video4Linux
        }

        fn camera_info(&self) -> &CameraInfo {
            &self.camera_info
        }

        fn refresh_camera_format(&mut self) -> Result<(), NokhwaError> {
            self.force_refresh_camera_format()
        }

        fn camera_format(&self) -> CameraFormat {
            self.camera_format
        }

        fn set_camera_format(&mut self, new_fmt: CameraFormat) -> Result<(), NokhwaError> {
            let device = self.lock_device()?;
            let prev_format = match Capture::format(&*device) {
                Ok(fmt) => fmt,
                Err(why) => {
                    return Err(NokhwaError::GetPropertyError {
                        property: "Resolution, FrameFormat".to_string(),
                        error: why.to_string(),
                    })
                }
            };
            let prev_fps = match Capture::params(&*device) {
                Ok(fps) => fps,
                Err(why) => {
                    return Err(NokhwaError::GetPropertyError {
                        property: "Frame rate".to_string(),
                        error: why.to_string(),
                    })
                }
            };

            // BAUCORD-PATCH: `device_fourcc`.
            let (v4l_fcc, planar) = device_fourcc(&offered_fourccs(&device), new_fmt.format());

            let format = Format::new(new_fmt.width(), new_fmt.height(), v4l_fcc);
            let frame_rate = Parameters::with_fps(new_fmt.frame_rate());

            if let Err(why) = Capture::set_format(&*device, &format) {
                return Err(NokhwaError::SetPropertyError {
                    property: "Resolution, FrameFormat".to_string(),
                    value: format.to_string(),
                    error: why.to_string(),
                });
            }
            if let Err(why) = Capture::set_params(&*device, &frame_rate) {
                return Err(NokhwaError::SetPropertyError {
                    property: "Frame rate".to_string(),
                    value: frame_rate.to_string(),
                    error: why.to_string(),
                });
            }

            drop(device);
            self.planar = planar;

            if self.stream_handle.is_some() {
                return match self.open_stream() {
                    Ok(_) => Ok(()),
                    Err(why) => {
                        // undo
                        let device = self.lock_device()?;
                        if let Err(why) = Capture::set_format(&*device, &prev_format) {
                            return Err(NokhwaError::SetPropertyError {
                                property: format!("Attempt undo due to stream acquisition failure with error {}. Resolution, FrameFormat", why),
                                value: prev_format.to_string(),
                                error: why.to_string(),
                            });
                        }
                        if let Err(why) = Capture::set_params(&*device, &prev_fps) {
                            return Err(NokhwaError::SetPropertyError {
                                property:
                                format!("Attempt undo due to stream acquisition failure with error {}. Frame rate", why),
                                value: prev_fps.to_string(),
                                error: why.to_string(),
                            });
                        }
                        Err(why)
                    }
                };
            }
            self.camera_format = new_fmt;

            self.force_refresh_camera_format()?;
            // BAUCORD-PATCH: as in `new`, the rate is the driver's.
            if !same_picture(self.camera_format, new_fmt) {
                return Err(NokhwaError::SetPropertyError {
                    property: "CameraFormat".to_string(),
                    value: new_fmt.to_string(),
                    error: "Rejected".to_string(),
                });
            }

            Ok(())
        }

        fn compatible_list_by_resolution(
            &mut self,
            fourcc: FrameFormat,
        ) -> Result<HashMap<Resolution, Vec<u32>>, NokhwaError> {
            let resolutions = self.get_resolution_list(fourcc)?;
            // BAUCORD-PATCH: `device_fourcc`.
            let format = device_fourcc(&offered_fourccs(&*self.lock_device()?), fourcc).0;
            let mut res_map = HashMap::new();
            for res in resolutions {
                let mut compatible_fps = vec![];
                match self
                    .lock_device()?
                    .enum_frameintervals(format, res.width(), res.height())
                {
                    Ok(intervals) => {
                        // BAUCORD-PATCH: `rates_of`.
                        for interval in intervals {
                            compatible_fps.extend(rates_of(&interval.interval));
                        }
                    }
                    Err(why) => {
                        return Err(NokhwaError::GetPropertyError {
                            property: "Frame rate".to_string(),
                            error: why.to_string(),
                        })
                    }
                }
                res_map.insert(res, compatible_fps);
            }
            Ok(res_map)
        }

        fn compatible_fourcc(&mut self) -> Result<Vec<FrameFormat>, NokhwaError> {
            match self.lock_device()?.enum_formats() {
                Ok(formats) => {
                    let mut frame_format_vec = vec![];
                    // BAUCORD-PATCH: `listed_format`.
                    let offered: Vec<FourCC> = formats.iter().map(|f| f.fourcc).collect();
                    for format in formats {
                        match listed_format(&offered, format.fourcc) {
                            Some(ff) => frame_format_vec.push(ff),
                            None => continue,
                        }
                    }
                    frame_format_vec.sort();
                    frame_format_vec.dedup();
                    Ok(frame_format_vec)
                }
                Err(why) => Err(NokhwaError::GetPropertyError {
                    property: "FrameFormat".to_string(),
                    error: why.to_string(),
                }),
            }
        }

        fn resolution(&self) -> Resolution {
            self.camera_format.resolution()
        }

        fn set_resolution(&mut self, new_res: Resolution) -> Result<(), NokhwaError> {
            let mut new_fmt = self.camera_format;
            new_fmt.set_resolution(new_res);
            self.set_camera_format(new_fmt)
        }

        fn frame_rate(&self) -> u32 {
            self.camera_format.frame_rate()
        }

        fn set_frame_rate(&mut self, new_fps: u32) -> Result<(), NokhwaError> {
            let mut new_fmt = self.camera_format;
            new_fmt.set_frame_rate(new_fps);
            self.set_camera_format(new_fmt)
        }

        fn frame_format(&self) -> FrameFormat {
            self.camera_format.format()
        }

        fn set_frame_format(&mut self, fourcc: FrameFormat) -> Result<(), NokhwaError> {
            let mut new_fmt = self.camera_format;
            new_fmt.set_format(fourcc);
            self.set_camera_format(new_fmt)
        }

        fn camera_control(
            &self,
            control: KnownCameraControl,
        ) -> Result<CameraControl, NokhwaError> {
            let controls = self.camera_controls()?;
            for supported_control in controls {
                if supported_control.control() == control {
                    return Ok(supported_control);
                }
            }
            Err(NokhwaError::GetPropertyError {
                property: control.to_string(),
                error: "not found/not supported".to_string(),
            })
        }

        #[allow(clippy::cast_possible_wrap)]
        fn camera_controls(&self) -> Result<Vec<CameraControl>, NokhwaError> {
            let device = self.lock_device()?;
            device
                .query_controls()
                .map_err(|why| NokhwaError::GetPropertyError {
                    property: "V4L2 Controls".to_string(),
                    error: why.to_string(),
                })?
                .into_iter()
                .map(|desc| {
                    let id_as_kcc = id_to_known_camera_control(desc.id);
                    let ctrl_current = device.control(desc.id)?.value;

                    let ctrl_value_desc = match (desc.typ, ctrl_current) {
                        (
                            Type::Integer
                            | Type::Integer64
                            | Type::Menu
                            | Type::U8
                            | Type::U16
                            | Type::U32
                            | Type::IntegerMenu,
                            Value::Integer(current),
                        ) => ControlValueDescription::IntegerRange {
                            min: desc.minimum as i64,
                            max: desc.maximum,
                            value: current,
                            step: desc.step as i64,
                            default: desc.default,
                        },
                        (Type::Boolean, Value::Boolean(current)) => {
                            ControlValueDescription::Boolean {
                                value: current,
                                default: desc.default != 0,
                            }
                        }

                        (Type::String, Value::String(current)) => ControlValueDescription::String {
                            value: current,
                            default: None,
                        },
                        _ => {
                            return Err(io::Error::new(
                                ErrorKind::Unsupported,
                                "what is this?????? todo: support ig",
                            ))
                        }
                    };

                    let is_readonly = desc
                        .flags
                        .intersects(Flags::READ_ONLY)
                        .then_some(KnownCameraControlFlag::ReadOnly);
                    let is_writeonly = desc
                        .flags
                        .intersects(Flags::WRITE_ONLY)
                        .then_some(KnownCameraControlFlag::WriteOnly);
                    let is_disabled = desc
                        .flags
                        .intersects(Flags::DISABLED)
                        .then_some(KnownCameraControlFlag::Disabled);
                    let is_volatile = desc
                        .flags
                        .intersects(Flags::VOLATILE)
                        .then_some(KnownCameraControlFlag::Volatile);
                    let is_inactive = desc
                        .flags
                        .intersects(Flags::INACTIVE)
                        .then_some(KnownCameraControlFlag::Disabled);
                    let flags_vec = vec![
                        is_inactive,
                        is_readonly,
                        is_volatile,
                        is_disabled,
                        is_writeonly,
                    ]
                    .into_iter()
                    .filter(Option::is_some)
                    .collect::<Option<Vec<KnownCameraControlFlag>>>()
                    .unwrap_or_default();

                    Ok(CameraControl::new(
                        id_as_kcc,
                        desc.name,
                        ctrl_value_desc,
                        flags_vec,
                        !desc.flags.intersects(Flags::INACTIVE),
                    ))
                })
                .filter(Result::is_ok)
                .collect::<Result<Vec<CameraControl>, io::Error>>()
                .map_err(|x| NokhwaError::GetPropertyError {
                    property: "www".to_string(),
                    error: x.to_string(),
                })
        }

        fn set_camera_control(
            &mut self,
            id: KnownCameraControl,
            value: ControlValueSetter,
        ) -> Result<(), NokhwaError> {
            let conv_value = match value.clone() {
                ControlValueSetter::None => Value::None,
                ControlValueSetter::Integer(i) => Value::Integer(i),
                ControlValueSetter::Boolean(b) => Value::Boolean(b),
                ControlValueSetter::String(s) => Value::String(s),
                ControlValueSetter::Bytes(b) => Value::CompoundU8(b),
                v => {
                    return Err(NokhwaError::SetPropertyError {
                        property: id.to_string(),
                        value: v.to_string(),
                        error: "not supported".to_string(),
                    })
                }
            };
            self.lock_device()?
                .set_control(Control {
                    id: known_camera_control_to_id(id),
                    value: conv_value,
                })
                .map_err(|why| NokhwaError::SetPropertyError {
                    property: id.to_string(),
                    value: format!("{:?}", value),
                    error: why.to_string(),
                })?;
            // verify

            let control = self.camera_control(id)?;
            if control.value() != value {
                return Err(NokhwaError::SetPropertyError {
                    property: id.to_string(),
                    value: format!("{:?}", value),
                    error: "Rejected".to_string(),
                });
            }
            Ok(())
        }

        fn open_stream(&mut self) -> Result<(), NokhwaError> {
            // Disable mut warning, since mut is only required when not using arena buffers
            #[allow(unused_mut)]
            let mut stream =
                match MmapStream::new(&*self.lock_device()?, v4l::buffer::Type::VideoCapture) {
                    Ok(s) => s,
                    Err(why) => return Err(NokhwaError::OpenStreamError(why.to_string())),
                };

            // Explicitly start now, or won't work with the RPi. As a consequence, buffers will only be used as required.
            // WARNING: This will cause drop of half of the frames
            #[cfg(feature = "no-arena-buffer")]
            match stream.start() {
                Ok(s) => s,
                Err(why) => return Err(NokhwaError::OpenStreamError(why.to_string())),
            }
            self.stream_handle = Some(stream);
            Ok(())
        }

        fn is_stream_open(&self) -> bool {
            self.stream_handle.is_some()
        }

        fn frame(&mut self) -> Result<Buffer, NokhwaError> {
            let cam_fmt = self.camera_format;
            match &mut self.stream_handle {
                Some(sh) => match sh.next() {
                    Ok((data, meta)) => {
                        let wall_ts = monotonic_to_wallclock(meta.timestamp);
                        // BAUCORD-PATCH: YU12/YV12 handed on as NV12.
                        if let Some(v_first) = self.planar {
                            let nv12 =
                                planar_to_nv12(data, cam_fmt.width(), cam_fmt.height(), v_first)
                                    .ok_or_else(|| {
                                        NokhwaError::ReadFrameError(format!(
                                            "short 4:2:0 frame: {} bytes for {}",
                                            data.len(),
                                            cam_fmt.resolution()
                                        ))
                                    })?;
                            return Ok(Buffer::with_timestamp(
                                cam_fmt.resolution(),
                                &nv12,
                                cam_fmt.format(),
                                wall_ts,
                            ));
                        }
                        Ok(Buffer::with_timestamp(
                            cam_fmt.resolution(),
                            data,
                            cam_fmt.format(),
                            wall_ts,
                        ))
                    }
                    Err(why) => Err(NokhwaError::ReadFrameError(why.to_string())),
                },
                None => Err(NokhwaError::ReadFrameError(
                    "Stream Not Started".to_string(),
                )),
            }
        }

        fn frame_raw(&mut self) -> Result<Cow<'_, [u8]>, NokhwaError> {
            match &mut self.stream_handle {
                Some(sh) => match sh.next() {
                    Ok((data, _)) => Ok(Cow::Borrowed(data)),
                    Err(why) => Err(NokhwaError::ReadFrameError(why.to_string())),
                },
                None => Err(NokhwaError::ReadFrameError(
                    "Stream Not Started".to_string(),
                )),
            }
        }

        fn stop_stream(&mut self) -> Result<(), NokhwaError> {
            if self.stream_handle.is_some() {
                self.stream_handle = None;
            }
            Ok(())
        }
    }

    fn fourcc_to_frameformat(fourcc: FourCC) -> Option<FrameFormat> {
        match fourcc.str().ok()? {
            "YUYV" => Some(FrameFormat::YUYV),
            "MJPG" => Some(FrameFormat::MJPEG),
            "GRAY" => Some(FrameFormat::GRAY),
            "RGB3" => Some(FrameFormat::RAWRGB),
            "BGR3" => Some(FrameFormat::RAWBGR),
            "NV12" => Some(FrameFormat::NV12),
            _ => None,
        }
    }

    fn frameformat_to_fourcc(fourcc: FrameFormat) -> FourCC {
        match fourcc {
            FrameFormat::MJPEG => FourCC::new(b"MJPG"),
            FrameFormat::YUYV => FourCC::new(b"YUYV"),
            FrameFormat::GRAY => FourCC::new(b"GRAY"),
            FrameFormat::RAWRGB => FourCC::new(b"RGB3"),
            FrameFormat::RAWBGR => FourCC::new(b"BGR3"),
            FrameFormat::NV12 => FourCC::new(b"NV12"),
        }
    }

    /// Convert a V4L2 CLOCK_MONOTONIC timestamp to a wallclock Duration since UNIX_EPOCH.
    fn monotonic_to_wallclock(ts: v4l::Timestamp) -> Option<std::time::Duration> {
        let frame_mono = std::time::Duration::from(ts);
        if frame_mono.is_zero() {
            return None;
        }

        let mut mono_now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut wall_now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: passing valid pointers to kernel clock_gettime
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut mono_now);
            libc::clock_gettime(libc::CLOCK_REALTIME, &mut wall_now);
        }
        let mono_now =
            std::time::Duration::new(mono_now.tv_sec as u64, mono_now.tv_nsec as u32);
        let wall_now =
            std::time::Duration::new(wall_now.tv_sec as u64, wall_now.tv_nsec as u32);

        // frame_age = how long ago the frame was captured (monotonic delta)
        let frame_age = mono_now.checked_sub(frame_mono)?;
        wall_now.checked_sub(frame_age)
    }
}

#[cfg(not(target_os = "linux"))]
mod internal {
    use nokhwa_core::buffer::Buffer;
    use nokhwa_core::error::NokhwaError;
    use nokhwa_core::traits::CaptureBackendTrait;
    use nokhwa_core::types::{
        ApiBackend, CameraControl, CameraFormat, CameraIndex, CameraInfo, ControlValueSetter,
        FrameFormat, KnownCameraControl, RequestedFormat, Resolution,
    };
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::marker::PhantomData;

    /// Attempts to convert a [`KnownCameraControl`] into a V4L2 Control ID.
    /// If the associated control is not found, this will return `None` (`ColorEnable`, `Roll`)
    #[allow(clippy::cast_possible_truncation)]
    pub fn known_camera_control_to_id(_ctrl: KnownCameraControl) -> u32 {
        0
    }

    /// Attempts to convert a [`u32`] V4L2 Control ID into a [`KnownCameraControl`]
    /// If the associated control is not found, this will return `None` (`ColorEnable`, `Roll`)
    #[allow(clippy::cast_lossless)]
    pub fn id_to_known_camera_control(id: u32) -> KnownCameraControl {
        KnownCameraControl::Other(id as u128)
    }

    /// The backend struct that interfaces with V4L2.
    /// To see what this does, please see [`CaptureBackendTrait`].
    /// # Quirks
    /// - Calling [`set_resolution()`](CaptureBackendTrait::set_resolution), [`set_frame_rate()`](CaptureBackendTrait::set_frame_rate), or [`set_frame_format()`](CaptureBackendTrait::set_frame_format) each internally calls [`set_camera_format()`](CaptureBackendTrait::set_camera_format).
    pub struct V4LCaptureDevice<'a> {
        __holder: PhantomData<&'a str>,
    }

    #[allow(unused_variables)]
    impl<'a> V4LCaptureDevice<'a> {
        /// Creates a new capture device using the `V4L2` backend. Indexes are gives to devices by the OS, and usually numbered by order of discovery.
        /// # Errors
        /// This function will error if the camera is currently busy or if `V4L2` can't read device information.
        #[allow(clippy::too_many_lines)]
        pub fn new(index: &CameraIndex, cam_fmt: RequestedFormat) -> Result<Self, NokhwaError> {
            Err(NokhwaError::NotImplementedError(
                "V4L2 only on Linux".to_string(),
            ))
        }

        /// Create a new `V4L2` Camera with desired settings. This may or may not work.
        /// # Errors
        /// This function will error if the camera is currently busy or if `V4L2` can't read device information.
        #[deprecated(since = "0.10.0", note = "please use `new` instead.")]
        pub fn new_with(
            index: CameraIndex,
            width: u32,
            height: u32,
            fps: u32,
            fourcc: FrameFormat,
        ) -> Result<Self, NokhwaError> {
            Err(NokhwaError::NotImplementedError(
                "V4L2 only on Linux".to_string(),
            ))
        }

        /// Force refreshes the inner [`CameraFormat`] state.
        /// # Errors
        /// If the internal representation in the driver is invalid, this will error.
        pub fn force_refresh_camera_format(&mut self) -> Result<(), NokhwaError> {
            Err(NokhwaError::NotImplementedError(
                "V4L2 only on Linux".to_string(),
            ))
        }
    }

    #[allow(unused_variables)]
    impl<'a> CaptureBackendTrait for V4LCaptureDevice<'a> {
        fn backend(&self) -> ApiBackend {
            ApiBackend::Video4Linux
        }

        fn camera_info(&self) -> &CameraInfo {
            todo!()
        }

        fn refresh_camera_format(&mut self) -> Result<(), NokhwaError> {
            todo!()
        }

        fn camera_format(&self) -> CameraFormat {
            todo!()
        }

        fn set_camera_format(&mut self, new_fmt: CameraFormat) -> Result<(), NokhwaError> {
            todo!()
        }

        fn compatible_list_by_resolution(
            &mut self,
            fourcc: FrameFormat,
        ) -> Result<HashMap<Resolution, Vec<u32>>, NokhwaError> {
            todo!()
        }

        fn compatible_fourcc(&mut self) -> Result<Vec<FrameFormat>, NokhwaError> {
            todo!()
        }

        fn resolution(&self) -> Resolution {
            todo!()
        }

        fn set_resolution(&mut self, new_res: Resolution) -> Result<(), NokhwaError> {
            todo!()
        }

        fn frame_rate(&self) -> u32 {
            todo!()
        }

        fn set_frame_rate(&mut self, new_fps: u32) -> Result<(), NokhwaError> {
            todo!()
        }

        fn frame_format(&self) -> FrameFormat {
            todo!()
        }

        fn set_frame_format(&mut self, fourcc: FrameFormat) -> Result<(), NokhwaError> {
            todo!()
        }

        fn camera_control(
            &self,
            control: KnownCameraControl,
        ) -> Result<CameraControl, NokhwaError> {
            todo!()
        }

        fn camera_controls(&self) -> Result<Vec<CameraControl>, NokhwaError> {
            todo!()
        }

        fn set_camera_control(
            &mut self,
            id: KnownCameraControl,
            value: ControlValueSetter,
        ) -> Result<(), NokhwaError> {
            todo!()
        }

        fn open_stream(&mut self) -> Result<(), NokhwaError> {
            todo!()
        }

        fn is_stream_open(&self) -> bool {
            todo!()
        }

        fn frame(&mut self) -> Result<Buffer, NokhwaError> {
            todo!()
        }

        fn frame_raw(&mut self) -> Result<Cow<'_, [u8]>, NokhwaError> {
            todo!()
        }

        fn stop_stream(&mut self) -> Result<(), NokhwaError> {
            todo!()
        }
    }
}

pub use internal::*;
