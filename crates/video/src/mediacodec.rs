//! Android's H.264 decoder through the NDK (`AMediaCodec`, libmediandk)
//! as a [`VideoDecoder`] — the viewer side of screen sharing on Android,
//! where there is no `FFmpeg` build (ANDROID-SPEC.md D19/D23).
//!
//! Shape: one `video/avc` codec in synchronous (poll) mode, output to
//! byte buffers rather than a Surface, so the frame comes back as YUV
//! planes that `yuv::to_rgba` turns into the RGBA the client expects.
//! The hardware decoder does the heavy lifting; the conversion is the
//! price of staying on the existing `FrameReady → SharedPixelBuffer →
//! Image` path instead of a native video surface.
//!
//! Lifecycle in the client's terms:
//! - `new()` only checks a decoder exists; the codec is configured on
//!   the first keyframe, whose `EncodedFrame` carries the size.
//! - Every decode queues the Annex-B access unit (SPS/PPS ride in-band
//!   before each IDR — the encoders make sure of that) and takes back
//!   whatever pictures are finished, newest wins. Hardware decoders
//!   finish a frame a few ms after it goes in, so a short wait catches
//!   most; the rest come out one call later.
//! - `flush()` is a no-op: the client calls it before every IDR as a
//!   belt-and-braces reset, and an IDR already resets the reference
//!   state; an `AMediaCodec_flush` there would discard the two or three
//!   pictures still in the decoder's pipeline every two seconds.
//! - A codec error, or a keyframe announcing a new size, rebuilds the
//!   codec at the next keyframe (P-frames until then are dropped and
//!   the jitter buffer's keyframe request does the rest).

use std::time::Duration;

use log::{debug, info, warn};
use ndk::media::{
    media_codec::{
        DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
        OutputBuffer,
    },
    media_format::MediaFormat,
};

use crate::{
    yuv::{self, ColorSpec, Matrix, Range, Yuv420Layout},
    CodecError, DecodedFrame, EncodedFrame, VideoDecoder,
};

const MIME: &str = "video/avc";
/// `MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible`: let
/// the decoder pick its native 4:2:0 layout and describe it.
const COLOR_FORMAT_YUV420_FLEXIBLE: i32 = 0x7f42_0888;
/// `COLOR_FormatYUV420Planar` (I420) / `COLOR_FormatYUV420SemiPlanar`
/// (NV12): the two fixed layouts a decoder may report instead of (or
/// alongside) an `image-data` record.
const COLOR_FORMAT_YUV420_PLANAR: i32 = 19;
const COLOR_FORMAT_YUV420_SEMI_PLANAR: i32 = 21;
/// `AMEDIACODEC_BUFFER_FLAG_KEY_FRAME` / `_CODEC_CONFIG`.
const BUFFER_FLAG_KEY_FRAME: u32 = 1;
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

/// How long to wait for a free input buffer. They free up as outputs
/// are released, which every decode call does, so waiting longer than
/// this means the codec is stuck.
const INPUT_WAIT: Duration = Duration::from_millis(10);
/// How long to wait for the picture of the frame just queued. Long
/// enough for a hardware decoder's usual turnaround at 1080p, short
/// enough that a slow one costs latency, not thread time.
const OUTPUT_WAIT: Duration = Duration::from_millis(4);
/// Synthetic presentation timestamps (the client stamps 0; decoders
/// want them increasing).
const PTS_STEP_US: u64 = 16_667;
const POOL_MAX: usize = 4;

/// A configured, started codec and the size it was configured for.
struct Running {
    codec: MediaCodec,
    width: u32,
    height: u32,
}

impl Drop for Running {
    fn drop(&mut self) {
        // `AMediaCodec_delete` releases a running codec too; stopping
        // first just makes the teardown orderly.
        let _ = self.codec.stop();
    }
}

/// H.264 → RGBA through `AMediaCodec`. One per watched peer, owned and
/// driven by the client's screen thread.
pub struct MediaCodecDecoder {
    codec: Option<Running>,
    /// Plane layout of the current output format; `None` until the
    /// first picture (or after the codec reports a format change).
    layout: Option<Yuv420Layout>,
    color: ColorSpec,
    pool: Vec<Vec<u8>>,
    frames_in: u64,
    frames_out: u64,
    frames_superseded: u64,
    /// Rebuild the codec at the next keyframe.
    needs_reset: bool,
    waiting_logged: bool,
}

// SAFETY: `AMediaCodec` has no thread affinity (any thread may call it,
// one at a time); the `MediaCodec` wrapper is `!Send` only because it
// holds a raw pointer. The client keeps each decoder on its screen
// thread and never shares it, which is all `Send` promises here.
unsafe impl Send for MediaCodecDecoder {}

impl MediaCodecDecoder {
    /// # Errors
    /// Only when the device has no `video/avc` decoder at all.
    pub fn new() -> Result<Self, CodecError> {
        // Probe, then drop: the real instance is made once the size is
        // known, and some SoCs cap the number of live hardware codecs.
        let probe = MediaCodec::from_decoder_type(MIME)
            .ok_or_else(|| CodecError::DecoderInit(format!("no {MIME} decoder on this device")))?;
        drop(probe);
        Ok(Self {
            codec: None,
            layout: None,
            color: ColorSpec::default(),
            pool: Vec::new(),
            frames_in: 0,
            frames_out: 0,
            frames_superseded: 0,
            needs_reset: false,
            waiting_logged: false,
        })
    }

    /// (Re)build the codec for a stream of `width × height`.
    fn configure(&mut self, width: u32, height: u32) -> Result<(), CodecError> {
        // Release the old instance before creating the new one.
        self.codec = None;
        self.layout = None;
        let codec = MediaCodec::from_decoder_type(MIME)
            .ok_or_else(|| CodecError::DecoderInit(format!("no {MIME} decoder on this device")))?;
        let mut format = MediaFormat::new();
        format.set_str("mime", MIME);
        format.set_i32("width", width as i32);
        format.set_i32("height", height as i32);
        format.set_i32("color-format", COLOR_FORMAT_YUV420_FLEXIBLE);
        // Android 11+ decoders that can skip their output queueing
        // delay do so with this; the others ignore the key.
        format.set_i32("low-latency", 1);
        codec
            .configure(&format, None, MediaCodecDirection::Decoder)
            .map_err(|e| CodecError::DecoderInit(format!("configure {width}x{height}: {e:?}")))?;
        codec
            .start()
            .map_err(|e| CodecError::DecoderInit(format!("start: {e:?}")))?;
        let name = codec.name().unwrap_or_else(|_| "?".into());
        info!("MediaCodec {name}: decoding {MIME} {width}x{height} to byte buffers");
        self.codec = Some(Running {
            codec,
            width,
            height,
        });
        self.needs_reset = false;
        self.waiting_logged = false;
        Ok(())
    }

    fn running(&self) -> &Running {
        self.codec.as_ref().expect("decoder configured first")
    }

    /// Queue one access unit.
    fn feed(codec: &MediaCodec, encoded: &EncodedFrame, index: u64) -> Result<(), FeedError> {
        let data = &encoded.data;
        let mut input = match codec.dequeue_input_buffer(INPUT_WAIT) {
            Ok(DequeuedInputBufferResult::Buffer(b)) => b,
            Ok(DequeuedInputBufferResult::TryAgainLater) => return Err(FeedError::Busy),
            Err(e) => return Err(FeedError::fatal(format!("dequeueInputBuffer: {e:?}"))),
        };
        let dst = input.buffer_mut();
        if dst.len() < data.len() {
            return Err(FeedError::fatal(format!(
                "{} byte frame exceeds the decoder's {} byte input buffer",
                data.len(),
                dst.len()
            )));
        }
        // SAFETY: `dst` is the codec's writable buffer of at least
        // `data.len()` bytes (checked above); copying initialised bytes
        // over `MaybeUninit<u8>` is fine, and nothing else aliases it
        // until it is queued.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst.as_mut_ptr().cast::<u8>(), data.len());
        }
        let flags = if encoded.is_keyframe {
            BUFFER_FLAG_KEY_FRAME
        } else {
            0
        };
        codec
            .queue_input_buffer(input, 0, data.len(), index * PTS_STEP_US, flags)
            .map_err(|e| FeedError::fatal(format!("queueInputBuffer: {e:?}")))
    }

    /// The size a stream is configured as (a packet without one gets a
    /// placeholder; the decoder follows the SPS anyway).
    fn configured_size(encoded: &EncodedFrame) -> (u32, u32) {
        (encoded.width.max(16), encoded.height.max(16))
    }
}

enum FeedError {
    /// Every input buffer is queued and waiting on outputs we hold.
    Busy,
    Fatal(CodecError),
}

impl FeedError {
    fn fatal(msg: String) -> Self {
        Self::Fatal(CodecError::Decode(msg))
    }
}

impl Drop for MediaCodecDecoder {
    fn drop(&mut self) {
        if self.frames_in > 0 {
            info!(
                "MediaCodec: {} frames in, {} pictures out, {} superseded before display",
                self.frames_in, self.frames_out, self.frames_superseded
            );
        }
    }
}

impl VideoDecoder for MediaCodecDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        let size = Self::configured_size(encoded);
        let size_changed = self
            .codec
            .as_ref()
            .is_none_or(|r| (r.width, r.height) != size);
        if self.needs_reset || size_changed {
            if !encoded.is_keyframe {
                if !self.waiting_logged {
                    debug!(
                        "MediaCodec: dropping P-frames until a keyframe (re)configures the decoder"
                    );
                    self.waiting_logged = true;
                }
                return Ok(None);
            }
            self.configure(size.0, size.1)?;
        }

        // Feed, then collect. A codec with no free input buffer is
        // holding finished pictures: collect those first, then retry.
        let mut early = None;
        let mut fed = Self::feed(&self.running().codec, encoded, self.frames_in);
        if matches!(fed, Err(FeedError::Busy)) {
            early = self.collect(Duration::ZERO)?;
            fed = Self::feed(&self.running().codec, encoded, self.frames_in);
        }
        match fed {
            Ok(()) => self.frames_in += 1,
            Err(FeedError::Busy) => {
                self.needs_reset = true;
                return Err(CodecError::Decode(format!(
                    "no free input buffer within {INPUT_WAIT:?} (decoder stalled)"
                )));
            }
            Err(FeedError::Fatal(e)) => {
                self.needs_reset = true;
                return Err(e);
            }
        }

        match self.collect(OUTPUT_WAIT)? {
            Some(frame) => {
                if let Some(older) = early {
                    self.frames_superseded += 1;
                    self.return_buffer(older.data);
                }
                Ok(Some(frame))
            }
            None => Ok(early),
        }
    }

    fn return_buffer(&mut self, buf: Vec<u8>) {
        if self.pool.len() < POOL_MAX {
            self.pool.push(buf);
        }
    }

    fn flush(&mut self) {
        // Deliberately nothing — see the module docs.
    }
}

impl MediaCodecDecoder {
    /// Take every finished picture out of the codec, convert the newest
    /// and release them all. `first_wait` bounds the wait for the
    /// first; later ones are only taken if already there.
    fn collect(&mut self, first_wait: Duration) -> Result<Option<DecodedFrame>, CodecError> {
        // Split borrows: the output buffers borrow the codec, while the
        // layout cache, colour spec and pool are written here.
        let Self {
            codec,
            layout,
            color,
            pool,
            frames_out,
            frames_superseded,
            needs_reset,
            ..
        } = self;
        let running = codec.as_ref().expect("collect() needs a configured codec");
        let codec = &running.codec;
        let hint = (running.width, running.height);

        let mut newest: Option<OutputBuffer<'_>> = None;
        let mut wait = first_wait;
        loop {
            match codec.dequeue_output_buffer(wait) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                    let info = buf.info();
                    let skip = info.size() <= 0 || info.flags() & BUFFER_FLAG_CODEC_CONFIG != 0;
                    if skip {
                        let _ = codec.release_output_buffer(buf, false);
                    } else if let Some(prev) = newest.replace(buf) {
                        *frames_superseded += 1;
                        let _ = codec.release_output_buffer(prev, false);
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    // Read lazily from the next buffer's own format,
                    // which carries the per-buffer `image-data`.
                    *layout = None;
                }
                // Buffer-array shuffles don't concern byte-buffer
                // clients; keep polling.
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    if let Some(buf) = newest.take() {
                        let _ = codec.release_output_buffer(buf, false);
                    }
                    *needs_reset = true;
                    return Err(CodecError::Decode(format!("dequeueOutputBuffer: {e:?}")));
                }
            }
            wait = Duration::ZERO;
        }

        let Some(buf) = newest else {
            return Ok(None);
        };
        let result = convert(&buf, layout, color, pool, hint);
        let _ = codec.release_output_buffer(buf, false);
        match result {
            Ok(frame) => {
                *frames_out += 1;
                if *frames_out == 1 {
                    info!("MediaCodec: first picture {}x{}", frame.width, frame.height);
                }
                Ok(Some(frame))
            }
            Err(e) => {
                // A layout we can't read won't fix itself; let the
                // next keyframe rebuild the codec and re-describe.
                *layout = None;
                *needs_reset = true;
                Err(e)
            }
        }
    }
}

/// One output buffer → RGBA, reading (and caching) the layout from the
/// buffer's format the first time.
fn convert(
    buf: &OutputBuffer<'_>,
    layout: &mut Option<Yuv420Layout>,
    color: &mut ColorSpec,
    pool: &mut Vec<Vec<u8>>,
    hint: (u32, u32),
) -> Result<DecodedFrame, CodecError> {
    let info = buf.info();
    let offset = usize::try_from(info.offset()).unwrap_or(0);
    let size = usize::try_from(info.size()).unwrap_or(0);
    let whole = buf.buffer();
    let data = whole.get(offset..offset + size).ok_or_else(|| {
        CodecError::Decode(format!(
            "output buffer info {offset}+{size} outside its {} bytes",
            whole.len()
        ))
    })?;

    if layout.is_none() {
        let format = buf.format();
        let (l, c) = layout_of(&format, hint)?;
        info!("MediaCodec output layout: {}", yuv::describe(&l, c));
        *layout = Some(l);
        *color = c;
    }
    let lay = layout.as_ref().expect("set above");

    let mut out = pool.pop().unwrap_or_default();
    yuv::to_rgba(data, lay, *color, &mut out).map_err(CodecError::ColorConversion)?;
    Ok(DecodedFrame {
        data: out,
        width: lay.width,
        height: lay.height,
        timestamp_ms: u64::try_from(info.presentation_time_us() / 1000).unwrap_or(0),
    })
}

/// Work out where the planes are from a `MediaCodec` output format:
/// the `image-data` record when the codec provides one (Codec2 and the
/// OMX shims always have), else the fixed I420 / NV12 layouts named by
/// `color-format` with `stride` / `slice-height`. The visible rectangle
/// is the format's `crop`, else the smaller of the format's size and
/// the stream's announced size (decoders pad up to macroblocks).
fn layout_of(
    format: &MediaFormat,
    hint: (u32, u32),
) -> Result<(Yuv420Layout, ColorSpec), CodecError> {
    let positive = |v: Option<i32>| v.filter(|&x| x > 0).map(|x| x as u32);
    let full_w = positive(format.i32("width")).unwrap_or(hint.0);
    let full_h = positive(format.i32("height")).unwrap_or(hint.1);
    let stride = positive(format.i32("stride")).unwrap_or(full_w) as usize;
    let slice_height = positive(format.i32("slice-height")).unwrap_or(full_h) as usize;
    let crop = match format.rect("crop") {
        Some((l, t, r, b)) if l >= 0 && t >= 0 && r >= l && b >= t => {
            (l as u32, t as u32, (r - l + 1) as u32, (b - t + 1) as u32)
        }
        _ => (0, 0, full_w.min(hint.0), full_h.min(hint.1)),
    };

    let layout = if let Some(image) = format.buffer("image-data") {
        Yuv420Layout::from_media_image2(image).map_err(CodecError::ColorConversion)?
    } else {
        match format.i32("color-format") {
            Some(COLOR_FORMAT_YUV420_PLANAR) => {
                Yuv420Layout::planar(full_w, full_h, stride, slice_height)
            }
            Some(COLOR_FORMAT_YUV420_SEMI_PLANAR) => {
                Yuv420Layout::semi_planar(full_w, full_h, stride, slice_height)
            }
            other => {
                warn!("MediaCodec output format without image-data: {format:?}");
                return Err(CodecError::ColorConversion(format!(
                    "unsupported decoder output colour format {other:?} (no image-data)"
                )));
            }
        }
    };
    let layout = layout.with_crop(crop.0, crop.1, crop.2, crop.3);

    // `MediaFormat.COLOR_RANGE_*` / `COLOR_STANDARD_*`. Unsignalled
    // means what the sender puts in the VUI: BT.709, limited.
    let range = match format.i32("color-range") {
        Some(1) => Range::Full,
        _ => Range::Limited,
    };
    let matrix = match format.i32("color-standard") {
        Some(2 | 4) => Matrix::Bt601,
        Some(6) => Matrix::Bt2020,
        _ => Matrix::Bt709,
    };
    Ok((layout, ColorSpec { matrix, range }))
}
