//! iOS's H.264 decoder: `VideoToolbox`'s `VTDecompressionSession` as a
//! [`VideoDecoder`] — the viewer side of screen sharing on the iPhone,
//! where there is no `FFmpeg` build (IOS-SPEC.md D9, §14 D30). Android's
//! `mediacodec.rs` is the sibling; the client drives both the same way.
//!
//! Shape: the session decodes **synchronously** (no asynchronous or
//! temporal-processing flag, so the output callback runs before
//! `VTDecompressionSessionDecodeFrame` returns) into **BGRA** pixel
//! buffers — `VideoToolbox` does the YUV conversion on the GPU, honouring
//! the stream's colour signalling. The picture leaves as BGRA
//! ([`crate::PictureFormat::Bgra`]), copied once out of the pixel buffer
//! without its row padding ([`yuv::pack_rows`]); the client's
//! `FrameReady → SharedPixelBuffer` path swaps two bytes a pixel as it
//! writes the buffer it draws from. The hardware decoder does the work.
//!
//! Lifecycle in the client's terms:
//! - `new()` only builds the struct; the session is made on the first
//!   keyframe, from the SPS and PPS that ride in-band before every IDR
//!   (the senders' encoders make sure of that), via a
//!   `CMVideoFormatDescription`.
//! - Every decode re-frames the Annex-B access unit as AVCC
//!   ([`annexb::split`]), wraps it in a `CMBlockBuffer` and a
//!   `CMSampleBuffer`, and decodes it; the callback copies the picture
//!   out while the pixel buffer is locked.
//! - A keyframe whose parameter sets differ from the session's (a new
//!   size, a new encoder) rebuilds the session.
//! - `flush()` is a no-op, as on Android: the client calls it before
//!   every IDR, and an IDR already resets the reference state.
//! - Any decode failure — including `kVTInvalidSessionErr`, which iOS
//!   returns after it reclaimed the hardware decoder while the app was
//!   in the background — rebuilds the session at the next keyframe;
//!   P-frames until then are dropped (the senders send an IDR every
//!   couple of seconds, and the jitter buffer asks for one on loss).

use std::{
    ffi::c_void,
    ptr::{self, NonNull},
};

use log::{debug, info};
use objc2_core_foundation::{kCFBooleanTrue, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, CVImageBuffer,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCouldNotFindVideoDecoderErr, kVTDecompressionPropertyKey_RealTime, kVTInvalidSessionErr,
    kVTVideoDecoderBadDataErr, kVTVideoDecoderMalfunctionErr,
    kVTVideoDecoderUnsupportedDataFormatErr, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTSessionSetProperty,
};

use crate::{annexb, yuv, CodecError, DecodedFrame, EncodedFrame, VideoDecoder};

/// An `OSStatus` for logs: the number, and the name for the ones a
/// viewer meets.
fn status_text(status: i32) -> String {
    // A table, not a `match`: the framework's constants are not
    // upper-case, and the workspace denies that in patterns.
    let known = [
        (
            kVTInvalidSessionErr,
            " (session invalidated — the app was in the background)",
        ),
        (kVTVideoDecoderBadDataErr, " (bad data)"),
        (kVTVideoDecoderMalfunctionErr, " (decoder malfunction)"),
        (
            kVTVideoDecoderUnsupportedDataFormatErr,
            " (unsupported data)",
        ),
        (
            kVTCouldNotFindVideoDecoderErr,
            " (no decoder for this format)",
        ),
    ];
    let name = known
        .iter()
        .find(|(code, _)| *code == status)
        .map_or("", |(_, name)| name);
    format!("OSStatus {status}{name}")
}

/// A live session and the parameter sets it was made from.
struct Session {
    vt: CFRetained<VTDecompressionSession>,
    /// What every sample is described by.
    format: CFRetained<CMFormatDescription>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The documented orderly teardown; the release follows.
        // SAFETY: a valid session we own; no decode is running (they
        // are synchronous, on this thread).
        unsafe { self.vt.invalidate() };
    }
}

/// Where the output callback leaves the picture of the frame being
/// decoded. Lives on `decode`'s stack; its address is the frame's
/// `sourceFrameRefCon`.
struct Target {
    /// The picture as BGRA, in a buffer from the shared pool
    /// (`crate::pool`).
    pixels: Vec<u8>,
    /// The stream's announced size: the picture is cut to it (decoders
    /// may hand back padded buffers).
    bound: (u32, u32),
    /// `None`: no callback, or a dropped frame.
    result: Option<Result<(u32, u32), CodecError>>,
}

/// H.264 → RGBA through `VideoToolbox`. One per watched peer, owned and
/// driven by the client's screen thread.
pub struct VideoToolboxDecoder {
    session: Option<Session>,
    /// The AVCC bytes of the current frame (allocation reused).
    avcc: Vec<u8>,
    /// Rebuild the session at the next keyframe.
    needs_reset: bool,
    waiting_logged: bool,
    frames_in: u64,
    frames_out: u64,
    frames_dropped: u64,
}

// SAFETY: a `VTDecompressionSession` (a CF object) may be used from any
// thread, one call at a time; the `CFRetained` wrappers are `!Send` only
// because the bindings do not mark the types. The client keeps each
// decoder on its screen thread and never shares it, which is all `Send`
// promises here.
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    /// # Errors
    /// None today; the signature matches the other decoders'. Whether
    /// the device can decode is known at the first keyframe.
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            session: None,
            avcc: Vec::new(),
            needs_reset: false,
            waiting_logged: false,
            frames_in: 0,
            frames_out: 0,
            frames_dropped: 0,
        })
    }

    /// Whether `sps`/`pps` are what the current session was made from.
    fn same_parameters(&self, sps: &[&[u8]], pps: &[&[u8]]) -> bool {
        self.session.as_ref().is_some_and(|s| {
            s.sps.len() == sps.len()
                && s.pps.len() == pps.len()
                && s.sps.iter().zip(sps).all(|(a, b)| a == b)
                && s.pps.iter().zip(pps).all(|(a, b)| a == b)
        })
    }

    /// Decode one sample with the current session.
    fn decode_sample(
        &mut self,
        bound: (u32, u32),
        timestamp_ms: u64,
    ) -> Result<Option<DecodedFrame>, CodecError> {
        let session = self
            .session
            .as_ref()
            .expect("decode_sample needs a session");
        let sample = make_sample(&self.avcc, &session.format)?;
        let mut target = Target {
            pixels: crate::pool::take(bound.0 as usize * bound.1 as usize * 4),
            bound,
            result: None,
        };
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: the session and sample are valid; with neither the
        // asynchronous nor the temporal-processing flag the callback runs
        // before this returns, and the wait below covers any frame the
        // decoder still held — so `target` outlives every callback that
        // can see its address.
        let status = unsafe {
            let status = session.vt.decode_frame(
                &sample,
                VTDecodeFrameFlags::empty(),
                ptr::from_mut(&mut target).cast(),
                &raw mut info,
            );
            session.vt.wait_for_asynchronous_frames();
            status
        };
        self.frames_in += 1;
        if status != 0 {
            self.pool_return(target.pixels);
            self.needs_reset = true;
            return Err(CodecError::Decode(format!(
                "VTDecompressionSessionDecodeFrame: {}",
                status_text(status)
            )));
        }
        match target.result {
            Some(Ok((width, height))) => {
                self.frames_out += 1;
                if self.frames_out == 1 {
                    info!("VideoToolbox: first picture {width}x{height}");
                }
                Ok(Some(DecodedFrame {
                    data: target.pixels,
                    width,
                    height,
                    timestamp_ms,
                    format: crate::PictureFormat::Bgra,
                }))
            }
            Some(Err(e)) => {
                self.pool_return(target.pixels);
                self.needs_reset = true;
                Err(e)
            }
            None => {
                self.frames_dropped += 1;
                self.pool_return(target.pixels);
                Ok(None)
            }
        }
    }

    #[allow(clippy::unused_self)] // kept as a method: every exit path calls it
    fn pool_return(&mut self, buf: Vec<u8>) {
        crate::pool::recycle(buf);
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        if self.frames_in > 0 {
            info!(
                "VideoToolbox: {} frames in, {} pictures out, {} dropped by the decoder",
                self.frames_in, self.frames_out, self.frames_dropped
            );
        }
    }
}

impl VideoDecoder for VideoToolboxDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        let avcc = std::mem::take(&mut self.avcc);
        let split = annexb::split(&encoded.data, avcc);

        let have_sets = !split.sps.is_empty() && !split.pps.is_empty();
        let rebuild = self.session.is_none()
            || self.needs_reset
            || (have_sets && !self.same_parameters(&split.sps, &split.pps));
        if rebuild {
            if !(encoded.is_keyframe && have_sets) {
                if !self.waiting_logged {
                    debug!("VideoToolbox: dropping frames until a keyframe with SPS/PPS (re)builds the session");
                    self.waiting_logged = true;
                }
                self.avcc = split.avcc;
                return Ok(None);
            }
            // Release the old session before making the new one: the
            // hardware has a limited number.
            self.session = None;
            let built = make_session(&split.sps, &split.pps);
            match built {
                Ok(session) => {
                    self.session = Some(session);
                    self.needs_reset = false;
                    self.waiting_logged = false;
                }
                Err(e) => {
                    self.avcc = split.avcc;
                    self.needs_reset = true;
                    return Err(e);
                }
            }
        }
        self.avcc = split.avcc;
        if self.avcc.is_empty() {
            // Parameter sets only: nothing to show.
            return Ok(None);
        }
        let bound = (encoded.width, encoded.height);
        self.decode_sample(bound, encoded.timestamp_ms)
    }

    fn flush(&mut self) {
        // Deliberately nothing — see the module docs.
    }
}

/// A session for the stream the parameter sets describe, emitting BGRA.
fn make_session(sps: &[&[u8]], pps: &[&[u8]]) -> Result<Session, CodecError> {
    let format = make_format(sps, pps)?;

    // SAFETY: an immutable CFString constant of CoreVideo.
    let key: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
    // OSType is a FourCC; CFNumber has no unsigned 32-bit type and BGRA
    // ('BGRA' = 0x4247_5241) fits an i32.
    let bgra = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
    let attributes = CFDictionary::<CFString, CFNumber>::from_slices(&[key], &[&bgra]);
    let attributes: &CFDictionary = (*attributes).as_ref();

    let callback = VTDecompressionOutputCallbackRecord {
        decompressionOutputCallback: Some(on_decoded),
        decompressionOutputRefCon: ptr::null_mut(),
    };
    let mut raw: *mut VTDecompressionSession = ptr::null_mut();
    // SAFETY: valid format description and attribute dictionary; the
    // callback record is copied by the call; `raw` receives a +1
    // reference on success.
    let status = unsafe {
        VTDecompressionSession::create(
            None,
            &format,
            None,
            Some(attributes),
            &raw const callback,
            NonNull::from(&mut raw),
        )
    };
    let session = match NonNull::new(raw) {
        // SAFETY: a +1 reference from a successful create.
        Some(raw) if status == 0 => unsafe { CFRetained::from_raw(raw) },
        _ => {
            return Err(CodecError::DecoderInit(format!(
                "VTDecompressionSessionCreate: {}",
                status_text(status)
            )))
        }
    };

    // Live video: decode as soon as possible, not at the most efficient
    // pace. A refusal is harmless — the default works, slower to start.
    // SAFETY: a valid session, a CFString key and a CFBoolean value.
    unsafe {
        let value: Option<&CFType> = kCFBooleanTrue.map(|b| -> &CFType { b });
        let status = VTSessionSetProperty(&session, kVTDecompressionPropertyKey_RealTime, value);
        if status != 0 {
            debug!("VideoToolbox: RealTime not set: {}", status_text(status));
        }
    }

    info!(
        "VideoToolbox: decoding H.264 to BGRA ({} SPS, {} PPS)",
        sps.len(),
        pps.len()
    );
    Ok(Session {
        vt: session,
        format,
        sps: sps.iter().map(|s| s.to_vec()).collect(),
        pps: pps.iter().map(|s| s.to_vec()).collect(),
    })
}

/// A `CMVideoFormatDescription` from the in-band parameter sets.
fn make_format(
    sps: &[&[u8]],
    pps: &[&[u8]],
) -> Result<CFRetained<CMFormatDescription>, CodecError> {
    let sets: Vec<&[u8]> = sps.iter().chain(pps).copied().collect();
    // `annexb::nal_units` never yields an empty unit, so every pointer
    // is to at least one byte.
    let pointers: Vec<NonNull<u8>> = sets
        .iter()
        .map(|s| NonNull::from(*s).cast::<u8>())
        .collect();
    let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
    let (Some(first_pointer), Some(first_size)) = (pointers.first(), sizes.first()) else {
        return Err(CodecError::DecoderInit(
            "no SPS/PPS to describe the stream".into(),
        ));
    };
    let mut raw: *const CMFormatDescription = ptr::null();
    // SAFETY: `pointers` and `sizes` are parallel arrays of `sets.len()`
    // entries pointing into `sets`, which outlive the call; the
    // parameter sets are copied; `raw` receives a +1 reference.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            sets.len(),
            NonNull::from(first_pointer),
            NonNull::from(first_size),
            annexb::AVCC_LENGTH_BYTES as i32,
            NonNull::from(&mut raw),
        )
    };
    match NonNull::new(raw.cast_mut()) {
        // SAFETY: a +1 reference from a successful create.
        Some(raw) if status == 0 => Ok(unsafe { CFRetained::from_raw(raw) }),
        _ => Err(CodecError::DecoderInit(format!(
            "CMVideoFormatDescriptionCreateFromH264ParameterSets ({} SPS, {} PPS): {}",
            sps.len(),
            pps.len(),
            status_text(status)
        ))),
    }
}

/// One AVCC access unit as a ready `CMSampleBuffer` (the bytes are
/// copied into memory `CoreMedia` owns, so nothing borrows `avcc` once
/// this returns).
fn make_sample(
    avcc: &[u8],
    format: &CMFormatDescription,
) -> Result<CFRetained<CMSampleBuffer>, CodecError> {
    let len = avcc.len();
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: a NULL memory block with the default allocator asks
    // CoreMedia to allocate `len` bytes now; `block` receives +1.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            len,
            None,
            ptr::null(),
            0,
            len,
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    let block = match NonNull::new(block) {
        // SAFETY: a +1 reference from a successful create.
        Some(b) if status == 0 => unsafe { CFRetained::from_raw(b) },
        _ => {
            return Err(CodecError::Decode(format!(
                "CMBlockBufferCreateWithMemoryBlock: {}",
                status_text(status)
            )))
        }
    };
    // SAFETY: `avcc` holds `len` readable bytes; the block was made
    // with exactly `len` bytes of its own.
    let status =
        unsafe { CMBlockBuffer::replace_data_bytes(NonNull::from(avcc).cast(), &block, 0, len) };
    if status != 0 {
        return Err(CodecError::Decode(format!(
            "CMBlockBufferReplaceDataBytes: {}",
            status_text(status)
        )));
    }

    let sizes = [len];
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    // SAFETY: one sample of `len` bytes, no timing (live: decode now);
    // the arrays are copied; `sample` receives +1.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            0,
            ptr::null(),
            1,
            sizes.as_ptr(),
            NonNull::from(&mut sample),
        )
    };
    match NonNull::new(sample) {
        // SAFETY: a +1 reference from a successful create.
        Some(s) if status == 0 => Ok(unsafe { CFRetained::from_raw(s) }),
        _ => Err(CodecError::Decode(format!(
            "CMSampleBufferCreateReady: {}",
            status_text(status)
        ))),
    }
}

/// `VTDecompressionOutputCallback`: copy the picture out of the pixel
/// buffer into the [`Target`] whose address is `frame_ref`.
unsafe extern "C-unwind" fn on_decoded(
    _session_ref: *mut c_void,
    frame_ref: *mut c_void,
    status: i32,
    _info: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _presentation: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `frame_ref` is the `&mut Target` `decode_sample` passed,
    // alive and not otherwise borrowed until the decode call (and its
    // wait) return — see there.
    let Some(target) = (unsafe { frame_ref.cast::<Target>().as_mut() }) else {
        return;
    };
    target.result = if status != 0 {
        Some(Err(CodecError::Decode(format!(
            "VideoToolbox decode: {}",
            status_text(status)
        ))))
    } else {
        // SAFETY: a non-NULL image buffer is valid for the callback.
        match unsafe { image.as_ref() } {
            // Dropped by the decoder: no picture for this frame.
            None => None,
            Some(image) => Some(copy_picture(image, target.bound, &mut target.pixels)),
        }
    };
}

/// Lock the BGRA pixel buffer, copy its visible part out as it is.
fn copy_picture(
    image: &CVImageBuffer,
    bound: (u32, u32),
    out: &mut Vec<u8>,
) -> Result<(u32, u32), CodecError> {
    let format = CVPixelBufferGetPixelFormatType(image);
    if format != kCVPixelFormatType_32BGRA {
        return Err(CodecError::ColorConversion(format!(
            "VideoToolbox returned pixel format {} instead of BGRA",
            fourcc(format)
        )));
    }
    // SAFETY: a valid pixel buffer; the read-only lock is undone below
    // with the same flag.
    let locked = unsafe { CVPixelBufferLockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    if locked != 0 {
        return Err(CodecError::Decode(format!(
            "CVPixelBufferLockBaseAddress: {locked}"
        )));
    }
    let base = CVPixelBufferGetBaseAddress(image).cast::<u8>().cast_const();
    let stride = CVPixelBufferGetBytesPerRow(image);
    let full_w = CVPixelBufferGetWidth(image);
    let full_h = CVPixelBufferGetHeight(image);
    // The announced size, when the packet carried one, else the buffer's.
    let width = if bound.0 > 0 {
        (bound.0 as usize).min(full_w)
    } else {
        full_w
    };
    let height = if bound.1 > 0 {
        (bound.1 as usize).min(full_h)
    } else {
        full_h
    };
    let result = if base.is_null() {
        Err(CodecError::Decode(
            "pixel buffer has no base address".into(),
        ))
    } else {
        // SAFETY: while locked, a chunky pixel buffer's memory holds
        // `full_h` rows of `stride` bytes from `base`.
        let pixels = unsafe { std::slice::from_raw_parts(base, stride * full_h) };
        yuv::pack_rows(pixels, stride, width as u32, height as u32, out)
            .map(|()| (width as u32, height as u32))
            .map_err(CodecError::ColorConversion)
    };
    // SAFETY: undoes the lock above, same flag.
    unsafe { CVPixelBufferUnlockBaseAddress(image, CVPixelBufferLockFlags::ReadOnly) };
    result
}

/// A `FourCC` pixel format as text ('420v'), for errors.
fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("'{}'", String::from_utf8_lossy(&bytes))
    } else {
        format!("{code:#010x}")
    }
}
