//! H.264 encode/decode via FFmpeg (`ffmpeg-next` crate).
//!
//! Tries hardware encoders in priority order, falls back to libx264.
//! Reuses FFmpeg frame objects and output buffers across calls.
//!
//! # Scaling architecture
//!
//! The encoder accepts frames at **any** input resolution (typically the
//! monitor's native resolution) and scales to the target encoding
//! resolution during the RGBA→YUV420P colorspace conversion. This is a
//! single swscale pass with bicubic filtering — no separate prescale step.
//!
//! This gives significantly sharper output for screen content vs the
//! previous nearest-neighbor prescale, especially for text at non-native
//! resolutions (e.g. 1080p monitor → 720p encode).
//!
//! # System requirements
//!
//! - **Windows (static):** `vcpkg install ffmpeg[x264,gpl]:x64-windows-static-md`
//!   then set `VCPKG_ROOT`.
//! - **Linux:** `apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev`
//! - **macOS:** `brew install ffmpeg`

extern crate ffmpeg_next as ffmpeg;

use ffmpeg::codec::{self, encoder, packet::Packet};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling::{Context as SwsContext, Flags};
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg::{Dictionary, Rational};

use crate::{CodecError, DecodedFrame, EncodedFrame, Frame, Quality, VideoDecoder, VideoEncoder};

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Hardware-accelerated (or software fallback) H.264 encoder via FFmpeg.
///
/// Accepts RGBA input at any resolution and downscales + converts to
/// YUV420P in a single swscale pass. The swscale context is created
/// lazily on the first `encode()` call (when the source resolution is
/// known) and recreated if the source resolution changes mid-stream.
pub struct FfmpegEncoder {
    encoder: ffmpeg::encoder::Video,
    quality: Quality,
    frame_count: i64,
    keyframe_interval: i64,
    force_next_keyframe: bool,

    // ── Scaling state (lazily initialized) ───────────────────────────
    /// swscale context: RGBA(src_w×src_h) → YUV420P(target_w×target_h).
    /// Created on first frame, recreated if source resolution changes.
    sws_ctx: Option<SwsContext>,
    /// Source resolution the current sws_ctx was built for.
    sws_src_w: u32,
    sws_src_h: u32,
    /// Reusable FFmpeg frame for RGBA input (source resolution).
    /// Recreated when source resolution changes.
    rgba_frame: Option<VideoFrame>,

    /// Reusable FFmpeg frame for YUV420P — fed to encoder (target resolution).
    yuv_frame: VideoFrame,
    /// Reusable packet for receiving encoder output.
    pkt: Packet,
    /// Reusable buffer for collecting encoded NAL data.
    /// Taken via `std::mem::take` each frame to avoid cloning.
    out_buf: Vec<u8>,
}

/// Encoder names to try, in priority order.
const ENCODER_PRIORITY: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "libx264"];

impl FfmpegEncoder {
    /// Create an encoder. Tries hardware encoders first, falls back to libx264.
    ///
    /// The swscale context is **not** created here — it's lazily initialized
    /// on the first `encode()` call when the source resolution is known.
    pub fn new(quality: Quality) -> Result<Self, CodecError> {
        ffmpeg::init().map_err(|e| CodecError::EncoderInit(e.to_string()))?;

        let (enc, enc_name) = find_encoder()?;

        let mut ctx = ffmpeg::codec::context::Context::new_with_codec(enc)
            .encoder()
            .video()
            .map_err(|e| CodecError::EncoderInit(e.to_string()))?;

        let target_bitrate = quality.bitrate() as usize;
        // Peak bitrate: 1.5× target gives keyframes room to breathe
        // without allowing unconstrained spikes.
        let max_bitrate = target_bitrate * 3 / 2;

        ctx.set_width(quality.width());
        ctx.set_height(quality.height());
        ctx.set_format(Pixel::YUV420P);
        ctx.set_time_base(Rational(1, quality.fps() as i32));
        ctx.set_frame_rate(Some(Rational(quality.fps() as i32, 1)));
        ctx.set_bit_rate(target_bitrate);
        ctx.set_max_bit_rate(max_bitrate);
        ctx.set_gop(quality.fps());
        ctx.set_max_b_frames(0);

        let opts = codec_options(enc_name, quality);
        let encoder = ctx
            .open_with(opts)
            .map_err(|e| CodecError::EncoderInit(format!("{enc_name}: {e}")))?;

        log::info!("Using FFmpeg encoder: {enc_name}");

        let mut yuv_frame = VideoFrame::new(Pixel::YUV420P, quality.width(), quality.height());
        yuv_frame.set_format(Pixel::YUV420P);

        // Pre-allocate output buffer for typical keyframe size
        let estimated_frame_bytes = target_bitrate / quality.fps() as usize / 8;
        let out_buf = Vec::with_capacity(estimated_frame_bytes * 2);

        Ok(Self {
            encoder,
            quality,
            frame_count: 0,
            keyframe_interval: (quality.fps() * 2) as i64,
            //keyframe_interval: quality.fps() as i64,
            force_next_keyframe: false,
            sws_ctx: None,
            sws_src_w: 0,
            sws_src_h: 0,
            rgba_frame: None,
            yuv_frame,
            pkt: Packet::empty(),
            out_buf,
        })
    }

    /// Ensure the swscale context matches the input frame dimensions.
    ///
    /// Creates the context on first call; recreates if source resolution
    /// changes (e.g. monitor switch, DPI change).
    fn ensure_sws(&mut self, src_w: u32, src_h: u32) -> Result<(), CodecError> {
        if self.sws_ctx.is_some() && self.sws_src_w == src_w && self.sws_src_h == src_h {
            return Ok(());
        }

        let dst_w = self.quality.width();
        let dst_h = self.quality.height();

        // Use BICUBIC for downscaling — much sharper than FAST_BILINEAR for
        // screen content (text, UI elements, hard edges). The CPU cost is
        // negligible since swscale is SIMD-optimized and we're doing the
        // colorspace conversion anyway.
        let flags = if src_w == dst_w && src_h == dst_h {
            // No resize needed — skip the expensive filter, just convert colorspace
            Flags::FAST_BILINEAR
        } else {
            Flags::BICUBIC
        };

        let sws = SwsContext::get(
            Pixel::RGBA,
            src_w,
            src_h,
            Pixel::YUV420P,
            dst_w,
            dst_h,
            flags,
        )
        .map_err(|e| CodecError::ColorConversion(e.to_string()))?;

        if self.sws_src_w != 0 {
            log::info!(
                "swscale: source resolution changed {}x{} → {}x{}, target {}x{}",
                self.sws_src_w,
                self.sws_src_h,
                src_w,
                src_h,
                dst_w,
                dst_h,
            );
        } else {
            log::info!(
                "swscale: {}x{} → {}x{} ({})",
                src_w,
                src_h,
                dst_w,
                dst_h,
                if src_w == dst_w && src_h == dst_h {
                    "colorspace only"
                } else {
                    "bicubic downscale + colorspace"
                },
            );
        }

        self.sws_ctx = Some(sws);
        self.sws_src_w = src_w;
        self.sws_src_h = src_h;
        self.rgba_frame = Some(VideoFrame::new(Pixel::RGBA, src_w, src_h));

        Ok(())
    }

    /// Encode one RGBA frame.
    ///
    /// The input frame can be at **any** resolution — the encoder scales
    /// to the target quality resolution during colorspace conversion.
    pub fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, CodecError> {
        // Ensure swscale matches this frame's resolution
        self.ensure_sws(frame.width, frame.height)?;

        let rgba_frame = self.rgba_frame.as_mut().expect("ensure_sws sets this");

        // Copy RGBA data into reusable FFmpeg frame (stride-aware)
        fill_rgba_frame(rgba_frame, &frame.data, frame.width, frame.height);

        // RGBA(src) → YUV420P(target) via swscale: resize + colorspace in one pass
        let sws = self.sws_ctx.as_mut().expect("ensure_sws sets this");
        sws.run(rgba_frame, &mut self.yuv_frame)
            .map_err(|e| CodecError::ColorConversion(e.to_string()))?;

        self.yuv_frame.set_pts(Some(self.frame_count));

        if self.force_next_keyframe || self.frame_count % self.keyframe_interval == 0 {
            self.yuv_frame.set_kind(ffmpeg::picture::Type::I);
            self.force_next_keyframe = false;
        } else {
            self.yuv_frame.set_kind(ffmpeg::picture::Type::None);
        }

        self.encoder
            .send_frame(&self.yuv_frame)
            .map_err(|e| CodecError::Encode(e.to_string()))?;

        // Collect output packets into reusable buffer.
        // Re-initialize capacity from previous frame's size to avoid
        // repeated small allocations after the take() below.
        if self.out_buf.capacity() == 0 {
            let estimated = self.quality.bitrate() as usize / self.quality.fps() as usize / 8;
            self.out_buf.reserve(estimated * 2);
        }
        self.out_buf.clear();
        let mut is_keyframe = false;
        while self.encoder.receive_packet(&mut self.pkt).is_ok() {
            if let Some(data) = self.pkt.data() {
                self.out_buf.extend_from_slice(data);
            }
            if self.pkt.is_key() {
                is_keyframe = true;
            }
        }

        self.frame_count += 1;

        // Transfer ownership instead of cloning — saves one allocation
        // per frame (30-60 allocs/sec at typical frame rates). The buffer
        // is re-allocated with capacity on the next encode() call above.
        Ok(EncodedFrame {
            data: std::mem::take(&mut self.out_buf),
            timestamp_ms: frame.timestamp_ms,
            is_keyframe,
        })
    }

    /// Force the next encoded frame to be a keyframe.
    ///
    /// Call this when a new viewer subscribes so they immediately get a
    /// decodable frame (with SPS/PPS parameter sets).
    pub fn force_keyframe(&mut self) {
        self.force_next_keyframe = true;
    }

    pub fn quality(&self) -> Quality {
        self.quality
    }

    /// Returns the name of the active encoder (e.g. "h264_nvenc", "libx264").
    pub fn encoder_name(&self) -> String {
        self.encoder
            .codec()
            .map(|c| c.name().to_owned())
            .unwrap_or_else(|| "unknown".into())
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<EncodedFrame, CodecError> {
        self.encode(frame)
    }

    fn quality(&self) -> Quality {
        self.quality
    }

    fn force_keyframe(&mut self) {
        self.force_keyframe();
    }
}

// SAFETY: FFmpeg contexts are single-owner. We never share them across
// threads — they move into the screen thread and stay there for the
// lifetime of the capture session.
unsafe impl Send for FfmpegEncoder {}
unsafe impl Send for FfmpegDecoder {}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// H.264 decoder via FFmpeg.
///
/// Lazily initializes the YUV→RGBA scaler on first decoded frame
/// (dimensions aren't known until then). Reuses the RGBA output frame
/// and pixel buffer across calls.
pub struct FfmpegDecoder {
    decoder: ffmpeg::decoder::Video,
    sws_ctx: Option<SwsContext>,
    /// Reusable FFmpeg frame for decoded YUV output.
    decoded_frame: VideoFrame,
    /// Reusable FFmpeg frame for RGBA conversion output.
    rgba_frame: Option<VideoFrame>,
    /// Reusable pixel buffer for the final RGBA data.
    rgba_buf: Vec<u8>,
}

impl FfmpegDecoder {
    pub fn new() -> Result<Self, CodecError> {
        ffmpeg::init().map_err(|e| CodecError::DecoderInit(e.to_string()))?;

        let dec = ffmpeg::decoder::find(codec::Id::H264).ok_or(CodecError::DecoderInit(
            "H264 decoder not found in FFmpeg".into(),
        ))?;

        let ctx = ffmpeg::codec::context::Context::new_with_codec(dec)
            .decoder()
            .video()
            .map_err(|e| CodecError::DecoderInit(e.to_string()))?;

        Ok(Self {
            decoder: ctx,
            sws_ctx: None,
            decoded_frame: VideoFrame::empty(),
            rgba_frame: None,
            rgba_buf: Vec::new(),
        })
    }

    /// Decode H.264 NAL data into RGBA.
    ///
    /// Returns `Ok(None)` when the decoder needs more data (EAGAIN).
    pub fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        if encoded.data.is_empty() {
            return Ok(None);
        }

        let mut pkt = Packet::copy(&encoded.data);
        pkt.set_pts(Some(encoded.timestamp_ms as i64));
        pkt.set_dts(Some(encoded.timestamp_ms as i64));

        self.decoder
            .send_packet(&pkt)
            .map_err(|e| CodecError::Decode(e.to_string()))?;

        match self.decoder.receive_frame(&mut self.decoded_frame) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                return Ok(None);
            }
            Err(e) => return Err(CodecError::Decode(e.to_string())),
        }

        let w = self.decoded_frame.width();
        let h = self.decoded_frame.height();

        // Lazily create SWS + RGBA frame on first decode (dimensions unknown until now)
        if self.sws_ctx.is_none() {
            self.sws_ctx = Some(
                SwsContext::get(
                    self.decoded_frame.format(),
                    w,
                    h,
                    Pixel::RGBA,
                    w,
                    h,
                    Flags::FAST_BILINEAR,
                )
                .map_err(|e| CodecError::ColorConversion(e.to_string()))?,
            );
            self.rgba_frame = Some(VideoFrame::new(Pixel::RGBA, w, h));
        }

        let rgba_frame = self.rgba_frame.as_mut().expect("initialized above");
        let sws = self.sws_ctx.as_mut().expect("initialized above");

        sws.run(&self.decoded_frame, rgba_frame)
            .map_err(|e| CodecError::ColorConversion(e.to_string()))?;

        // Extract RGBA data (stride-aware copy)
        let stride = rgba_frame.stride(0);
        let src = rgba_frame.data(0);
        let rgba_size = (w * h * 4) as usize;
        self.rgba_buf.resize(rgba_size, 0);

        let row_bytes = w as usize * 4;
        for row in 0..h as usize {
            let src_start = row * stride;
            let dst_start = row * row_bytes;
            self.rgba_buf[dst_start..dst_start + row_bytes]
                .copy_from_slice(&src[src_start..src_start + row_bytes]);
        }

        // Hand off the buffer, replace with empty
        let data = std::mem::take(&mut self.rgba_buf);

        Ok(Some(DecodedFrame {
            data,
            width: w,
            height: h,
            timestamp_ms: encoded.timestamp_ms,
        }))
    }

    /// Return a previously-decoded RGBA buffer for reuse.
    pub fn return_buffer(&mut self, mut buf: Vec<u8>) {
        buf.clear();
        self.rgba_buf = buf;
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        self.decode(encoded)
    }

    fn return_buffer(&mut self, buf: Vec<u8>) {
        self.return_buffer(buf);
    }
    fn flush(&mut self) {
        // Calls avcodec_flush_buffers — clears internal reference frames
        // so the next keyframe decodes from a clean state.
        self.decoder.flush();
        // Also drop the SWS context — the next keyframe might have
        // different dimensions after a stream reset.
        self.sws_ctx = None;
        self.rgba_frame = None;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the best available H.264 encoder.
fn find_encoder() -> Result<(ffmpeg::Codec, &'static str), CodecError> {
    for &name in ENCODER_PRIORITY {
        if let Some(enc) = encoder::find_by_name(name) {
            return Ok((enc, name));
        }
    }
    Err(CodecError::NoEncoder(ENCODER_PRIORITY.join(", ")))
}

/// Build codec-specific option dictionaries for screen-share encoding.
///
/// Key design decisions for libx264:
///
/// - **`veryfast` preset** (not `ultrafast`): Re-enables adaptive quantization,
///   better motion estimation refinement, and trellis. This is the single biggest
///   quality lever — `ultrafast` disables almost all quality tools and produces
///   visibly worse output at the same bitrate. At 720p30 the CPU cost of
///   `veryfast` is modest (~15-20% more than `ultrafast`).
///
/// - **`high` profile**: Enables CABAC entropy coding + 8×8 DCT transforms,
///   giving ~15-20% better compression vs baseline. Since we control both
///   encoder and decoder, compatibility is not a concern. B-frames are
///   disabled via `max_b_frames(0)` on the context, so High profile only
///   adds coding efficiency without latency.
///
/// - **`zerolatency` tune**: Disables lookahead and frame reordering. Adds
///   some quality cost but keeps per-frame encode latency minimal for
///   real-time screen sharing.
///
/// VBV (Video Buffering Verifier) constrains peak bitrate for real-time
/// transport. Buffer is 1 second at target bitrate — tight enough for
/// responsive rate control on scene changes, with enough room for keyframes.
fn codec_options(name: &str, quality: Quality) -> Dictionary<'static> {
    let mut opts = Dictionary::new();

    let target = quality.bitrate();
    // VBV buffer: 1 second at target bitrate.
    let vbv_bufsize = target;
    // Peak rate: 1.5× target — enough for keyframe bursts without
    // overwhelming the network or protocol buffer budget.
    let maxrate = target * 3 / 2;

    match name {
        "libx264" => {
            // veryfast: best quality/speed tradeoff for real-time screen content.
            // Enables AQ, trellis, and better ME vs ultrafast.
            opts.set("preset", "veryfast");
            opts.set("tune", "zerolatency");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
            opts.set("crf", "20");
            opts.set("aq-mode", "3");
            opts.set("profile", "high");
        }
        "h264_nvenc" => {
            opts.set("preset", "p1");
            opts.set("tune", "ull");
            opts.set("rc", "cbr");
            opts.set("zerolatency", "1");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
        }
        "h264_qsv" => {
            opts.set("preset", "veryfast");
            opts.set("low_power", "1");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
        }
        "h264_amf" => {
            opts.set("usage", "ultralowlatency");
            opts.set("quality", "speed");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
        }
        _ => {}
    }
    opts
}

/// Copy RGBA pixel data into an ffmpeg VideoFrame (stride-aware).
fn fill_rgba_frame(frame: &mut VideoFrame, rgba: &[u8], width: u32, height: u32) {
    let stride = frame.stride(0);
    let dst = frame.data_mut(0);
    let row_bytes = width as usize * 4;

    for row in 0..height as usize {
        let src_start = row * row_bytes;
        let dst_start = row * stride;
        dst[dst_start..dst_start + row_bytes]
            .copy_from_slice(&rgba[src_start..src_start + row_bytes]);
    }
}