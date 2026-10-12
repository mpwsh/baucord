//! H.264 encode/decode via `FFmpeg` (`ffmpeg-next` crate).
//!
//! Tries hardware encoders in priority order, falling back to the next
//! candidate **when opening fails**, not just when the encoder is missing
//! from the build (an `FFmpeg` build can contain `h264_nvenc` on a machine
//! with no NVIDIA GPU — the open is the real capability test).
//!
//! # Scaling architecture
//!
//! The encoder accepts frames at **any** input resolution (typically the
//! monitor's native resolution) and scales to the output resolution during
//! the RGBA→YUV420P colorspace conversion — a single swscale pass with
//! bicubic filtering.
//!
//! The output resolution is the source scaled to **fit inside** the
//! `Quality` bound with aspect ratio preserved (never upscaled, rounded
//! to even dimensions for 4:2:0 chroma). A 1920×1200 source at `HD30`
//! encodes as 1152×720; a portrait 1080×1920 source as 404×720.
//!
//! # Colorimetry
//!
//! RGB↔YUV conversion uses BT.709 with limited-range YUV, explicitly
//! configured on swscale and signaled in the bitstream VUI. Without this,
//! swscale silently defaults to BT.601 and the stream is unsignaled —
//! fine as long as our own decoder makes the same wrong assumption, but
//! subtly wrong colors the moment any other decoder touches the stream.
//!
//! # Timestamps and keyframes
//!
//! The encoder runs a 1/1000 time base and uses the frame's real capture
//! timestamp as PTS, so rate control stays correct when the capture rate
//! wobbles (screenshot APIs can't hold an exact frame rate). Keyframe
//! cadence is time-based (one IDR every [`KEYFRAME_INTERVAL_MS`]) with a
//! codec-level GOP as a long backstop, and forced keyframes are forced as
//! **IDR** on every backend so a joining viewer always gets a clean
//! recovery point with parameter sets.
//!
//! # Bitrate scaling
//!
//! The sender can run rate control below the preset
//! ([`VideoEncoder::set_bitrate_scale_pct`]) when its uplink is losing
//! packets. A scale change is handled exactly like a source-resolution
//! change: the session is reopened with target, maxrate and VBV buffer
//! all derived from the scaled bitrate ([`RateControl`]). Reopening is
//! deliberate over a live reconfigure — it is the one path every backend
//! supports, and the fresh session's first frame is an IDR with parameter
//! sets, which is exactly the recovery point viewers need after loss.
//!
//! # System requirements
//!
//! - **Windows (static):** `vcpkg install ffmpeg[x264,gpl]:x64-windows-static-md`
//!   then set `VCPKG_ROOT`.
//! - **Linux:** `apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev`
//! - **macOS:** `brew install ffmpeg`

extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::VecDeque,
    ffi::c_int,
    ptr,
    time::{Duration, Instant},
};

use ffmpeg::{
    codec::{self, encoder, packet::Packet},
    format::Pixel,
    software::scaling::{Context as SwsContext, Flags},
    util::frame::video::Video as VideoFrame,
    Dictionary, Rational,
};

use crate::{
    CodecError, DecodedFrame, EncodedFrame, Frame, PixelFormat, Quality, VideoDecoder, VideoEncoder,
};

/// Time between IDR keyframes, in milliseconds of *capture time*.
pub const KEYFRAME_INTERVAL_MS: u64 = 2_000;

/// Minimum spacing between IDR-via-session-reopen events (see the
/// reopen logic in [`FfmpegEncoder::encode`]). Each reopen costs a
/// rate-control reset, in-flight frames, and a stall while the session
/// comes back — so the floor is the natural IDR cadence itself: a
/// viewer in recovery waits at most one [`KEYFRAME_INTERVAL_MS`] for
/// the scheduled IDR either way, and everyone else stops paying for
/// every request. The 2026-08-29 logs show why a shorter floor is
/// harmful: with 2–4 viewers each requesting on their own cooldown, a
/// 700 ms floor produced one reopen per second for minutes, and the
/// encode rate fell to 18–28 fps from the stalls alone.
const MIN_IDR_REOPEN_INTERVAL_MS: u64 = KEYFRAME_INTERVAL_MS;

/// Codec-level GOP backstop, in seconds. The primary keyframe cadence is
/// the time-based forcing above; this only guarantees an eventual refresh
/// if that logic ever fails. Kept long so the two mechanisms don't fight.
const GOP_BACKSTOP_SECS: u32 = 8;

/// Encoder names to try, in priority order: the hardware ones (each
/// exists in the `FFmpeg` build of the systems that can have it — the
/// first three on Windows and Linux, `VideoToolbox` on macOS), then
/// software. One that is absent, or will not open, is passed over.
const ENCODER_PRIORITY: &[&str] = &[
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_videotoolbox",
    "libx264",
];

/// Hard per-access-unit cap for AMF, in bits: 250 KB, under the
/// transport's 275,400 B chunking limit with room for the AUD/SPS/PPS
/// that ride along. Only AMF needs it (see the option block); x264 and
/// NVENC respect the VBV closely enough that `VBV_MAX_BITS` suffices.
const AMF_MAX_AU_BITS: u32 = 2_000_000;

/// VBV buffer ceiling in bits. Bounds the worst-case single-frame size
/// (≈ bufsize bits). NOTE: under `tune zerolatency` (no lookahead) x264
/// treats VBV as a soft bound and overshoots forced IDRs by 10–25% on
/// detailed content — so this must sit WELL below the app transport's
/// hard per-frame chunking limit (~275 KB), not just under it. 1.6 Mbit
/// ⇒ ~200 KB nominal, leaving overshoot room.
const VBV_MAX_BITS: u32 = 1_600_000;

/// The quantizer floors an encoder steps through when it makes frames
/// too large to send (`VideoEncoder::frame_too_large`), lowest first. A
/// fresh AMF session's first IDR ignores both the VBV and
/// `max_au_size`: 287–522 KB against the 275 KB limit on a detailed
/// 1080p desktop (2026-10-08), and 425 KB from a session opened at
/// 2.1 Mbit/s — so a lower bitrate does not shrink it, but a quantizer
/// floor binds every frame from the first. Each step roughly halves a
/// detailed keyframe; text stays readable at all of them.
const QP_FLOORS: [u32; 4] = [24, 28, 32, 36];
/// A floor is relaxed one step after this long without a frame too
/// large (re-applied with the next session, which a change opens).
const QP_FLOOR_RELAX_AFTER: Duration = Duration::from_secs(30);

/// The three rate-control numbers a session is opened with, all derived
/// from one target so they scale together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    /// Average bitrate the encoder aims for, bits/s.
    pub target: u32,
    /// Peak bitrate: 1.5× target gives keyframes room to breathe without
    /// allowing unconstrained spikes.
    pub maxrate: u32,
    /// VBV buffer, bits. Caps the worst-case size of a single frame (a
    /// full buffer drain), which matters twice over: it bounds keyframe
    /// bursts on the wire, and the app's video chunking hard-limits one
    /// frame to 255 × 1080 B ≈ 275 KB (`protocol::MAX_VIDEO_FRAME_BYTES`)
    /// — beyond that the frame can't be sent at all. Never above
    /// [`VBV_MAX_BITS`] (≈ 200 KB/frame), and at low targets it shrinks
    /// with them so a keyframe stays a fraction of a second of budget
    /// rather than a multi-second burst.
    pub bufsize: u32,
}

impl RateControl {
    #[must_use]
    pub const fn for_target(target: u32) -> Self {
        Self {
            target,
            maxrate: target * 3 / 2,
            bufsize: if target < VBV_MAX_BITS {
                target
            } else {
                VBV_MAX_BITS
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Hardware-accelerated (or software fallback) H.264 encoder via `FFmpeg`.
///
/// Accepts RGBA input at any resolution. The encoding session (codec
/// context + scaler + frame buffers) is created lazily on the first
/// `encode()` call, when the source resolution is known, and recreated if
/// the source resolution changes mid-stream (monitor switch, DPI change).
pub struct FfmpegEncoder {
    codec: ffmpeg::Codec,
    enc_name: &'static str,
    quality: Quality,
    /// Requested rate-control scale, percent of the preset bitrate
    /// (see [`VideoEncoder::set_bitrate_scale_pct`]). Applied by
    /// [`Self::ensure_session`] on the next encode.
    bitrate_scale_pct: u32,
    force_next_keyframe: bool,
    /// Capture timestamp of the most recent keyframe AU this encoder
    /// actually emitted — ground truth, independent of what the codec
    /// promised. Drives the reopen-for-IDR backstop in [`Self::encode`].
    last_idr_out_ms: Option<u64>,
    /// Last time [`Self::encode`] reopened the session to force an IDR
    /// (rate limiting, capture-timestamp clock).
    last_idr_reopen_ms: Option<u64>,
    /// The session shape last logged at info (source size, output
    /// size, scale): a reopen that only buys an IDR — every keyframe on
    /// AMF — logs at debug, so the log says what changed, not each
    /// keyframe (the sharer's summary line counts those).
    logged_session: Option<(u32, u32, u32, u32, u32)>,
    /// Capture timestamp of the last IDR we forced (drives cadence).
    last_keyframe_ms: Option<u64>,
    /// Capture timestamp of the last frame submitted: what a repeated
    /// frame's still time is measured from ([`Self::encode_repeat`]).
    last_frame_ms: Option<u64>,
    /// Last PTS handed to the encoder — used to keep PTS strictly increasing.
    last_pts: Option<i64>,
    /// Index into [`QP_FLOORS`] once a frame came out too large to send,
    /// and when it last moved (relaxed after [`QP_FLOOR_RELAX_AFTER`]).
    qp_floor: Option<usize>,
    qp_floor_moved: Option<Instant>,
    session: Option<EncSession>,
}

/// Everything tied to one (source resolution → output resolution,
/// bitrate scale) configuration.
struct EncSession {
    encoder: encoder::Video,
    sws: SwsContext,
    src_w: u32,
    src_h: u32,
    src_format: PixelFormat,
    out_w: u32,
    out_h: u32,
    /// Scale this session's rate control was opened with, percent.
    bitrate_scale_pct: u32,
    /// The quantizer floor it was opened with (`qmin`), if any.
    qp_floor: Option<u32>,
    /// Reusable `FFmpeg` frame for YUV420P — fed to the encoder.
    yuv_frame: VideoFrame,
    /// Reusable packet for receiving encoder output.
    pkt: Packet,
    /// Capture timestamps of frames submitted but not yet emitted.
    ///
    /// B-frames are disabled, so output order == input order and a plain
    /// FIFO maps each output packet back to its input frame's timestamp.
    /// (Matters for hardware encoders, which run a few frames of delay.)
    pending_ts: VecDeque<u64>,
}

impl FfmpegEncoder {
    /// Create an encoder. Tries hardware encoders first, falls back to
    /// libx264 — including when a hardware encoder exists in the `FFmpeg`
    /// build but fails to open (no GPU / driver missing).
    ///
    /// The real encoding session is created lazily on the first
    /// `encode()` call; `new()` opens and discards a probe session at the
    /// preset's nominal resolution so "no usable encoder" is reported
    /// here, up front, rather than mid-stream.
    pub fn new(quality: Quality) -> Result<Self, CodecError> {
        ffmpeg::init().map_err(|e| CodecError::EncoderInit(e.to_string()))?;

        let mut errors: Vec<String> = Vec::new();

        for &name in ENCODER_PRIORITY {
            let Some(codec) = encoder::find_by_name(name) else {
                errors.push(format!("{name}: not in FFmpeg build"));
                continue;
            };

            // The probe: actually open a session. `find_by_name` succeeding
            // only means the encoder was compiled in, not that the hardware
            // behind it exists. (The probe's pixel format is arbitrary —
            // this session is discarded; the real one opens lazily from
            // the first frame's actual dimensions and format.)
            let probe = open_session(
                codec,
                name,
                quality,
                100,
                None,
                quality.width(),
                quality.height(),
                PixelFormat::Rgba,
            );
            match probe {
                Ok(_) => {
                    log::info!("Using FFmpeg encoder: {name}");
                    return Ok(Self {
                        codec,
                        enc_name: name,
                        quality,
                        bitrate_scale_pct: 100,
                        force_next_keyframe: false,
                        last_idr_out_ms: None,
                        last_idr_reopen_ms: None,
                        logged_session: None,
                        last_keyframe_ms: None,
                        last_frame_ms: None,
                        last_pts: None,
                        qp_floor: None,
                        qp_floor_moved: None,
                        session: None,
                    });
                }
                Err(e) => {
                    log::warn!("Encoder {name} failed to open, trying next: {e}");
                    errors.push(format!("{name}: {e}"));
                }
            }
        }

        Err(CodecError::NoEncoder(errors.join("; ")))
    }

    /// Ensure the session matches the input frame's geometry and the
    /// requested bitrate scale, creating or recreating it as needed.
    ///
    /// When an existing session is replaced, whatever output it already
    /// has ready is salvaged into `out` (frames still inside a hardware
    /// encoder's pipeline are lost — they consumed no sequence numbers,
    /// so viewers see no loss signal).
    fn ensure_session(
        &mut self,
        frame: &Frame,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError> {
        let scale = self.bitrate_scale_pct;
        let floor = self.qp_floor.map(|i| QP_FLOORS[i]);
        if let Some(s) = self.session.as_mut() {
            let same_source =
                s.src_w == frame.width && s.src_h == frame.height && s.src_format == frame.format;
            if same_source && s.bitrate_scale_pct == scale && s.qp_floor == floor {
                return Ok(());
            }
            let reason = if same_source && s.bitrate_scale_pct == scale {
                let qp = |f: Option<u32>| f.map_or_else(|| "none".to_string(), |q| q.to_string());
                format!("quantizer floor {} → {}", qp(s.qp_floor), qp(floor))
            } else if same_source {
                format!(
                    "bitrate scale {}% → {}% ({} → {} kbit/s)",
                    s.bitrate_scale_pct,
                    scale,
                    self.quality.scaled_bitrate(s.bitrate_scale_pct) / 1000,
                    self.quality.scaled_bitrate(scale) / 1000,
                )
            } else {
                format!(
                    "source resolution {}x{} → {}x{}",
                    s.src_w, s.src_h, frame.width, frame.height
                )
            };
            // Salvage first, so the count below is what is really lost.
            let _ = drain_packets(s, out);
            log::info!(
                "encoder: {reason}, reopening session ({} in-flight frame(s) dropped)",
                s.pending_ts.len(),
            );
            // The replacement starts with an IDR. Count it now: on
            // encoders that can't force IDRs, the overdue check in
            // encode() would otherwise re-fire before the async pipeline
            // surfaces that first frame and reopen again in a loop.
            self.last_idr_out_ms = Some(frame.timestamp_ms);
        }

        let session = open_session(
            self.codec,
            self.enc_name,
            self.quality,
            scale,
            floor,
            frame.width,
            frame.height,
            frame.format,
        )?;
        let shape = (
            frame.width,
            frame.height,
            session.out_w,
            session.out_h,
            scale,
        );
        let level = if self.logged_session == Some(shape) {
            log::Level::Debug
        } else {
            self.logged_session = Some(shape);
            log::Level::Info
        };
        log::log!(
            level,
            "encoder session: {}x{} → {}x{} ({}), {} kbit/s{}",
            frame.width,
            frame.height,
            session.out_w,
            session.out_h,
            if frame.width == session.out_w && frame.height == session.out_h {
                "colorspace only"
            } else {
                "bicubic downscale + colorspace"
            },
            self.quality.scaled_bitrate(scale) / 1000,
            match (scale, floor) {
                (100, None) => String::new(),
                (100, Some(q)) => format!(" (QP ≥ {q})"),
                (_, None) => format!(" ({scale}% of preset)"),
                (_, Some(q)) => format!(" ({scale}% of preset, QP ≥ {q})"),
            },
        );

        // A fresh session starts with an IDR anyway, but keep our cadence
        // bookkeeping honest.
        self.last_keyframe_ms = None;
        self.session = Some(session);
        Ok(())
    }

    fn encode_frame(
        &mut self,
        frame: &Frame,
        still: bool,
    ) -> Result<Vec<EncodedFrame>, CodecError> {
        let mut out = Vec::new();

        if still {
            let since_last = self
                .last_frame_ms
                .map_or(0, |last| frame.timestamp_ms.saturating_sub(last));
            for at in [&mut self.last_keyframe_ms, &mut self.last_idr_out_ms]
                .into_iter()
                .flatten()
            {
                *at += since_last;
            }
        }
        self.last_frame_ms = Some(frame.timestamp_ms);

        // Reopen-for-IDR backstop. Some encoders (FFmpeg's AMF wrapper
        // <= 7.1) cannot force an IDR: frame-level pict_type is ignored
        // and the IDR-period property is assigned after component init,
        // where it can fail silently — leaving the session's first
        // frame as the only IDR it will ever emit. The one mechanism
        // that provably works everywhere is that a *fresh session
        // always starts with an IDR carrying parameter sets. So when an
        // IDR is demanded (a viewer asked, or none has been emitted for
        // 2x the cadence interval) and this encoder can't force one,
        // reopen the session: one rate-control reset and ~10-30 ms
        // (rate limited) buys a guaranteed, immediate keyframe. Frames
        // in flight inside the old session are discarded — they consume
        // no sequence numbers, so viewers see no loss signal.
        if !self.supports_forced_keyframes() && self.session.is_some() {
            let idr_overdue = match self.last_idr_out_ms {
                Some(t) => frame.timestamp_ms.saturating_sub(t) >= 2 * KEYFRAME_INTERVAL_MS,
                // The running session opened with an IDR; None just
                // means the async pipeline hasn't produced output yet.
                None => false,
            };
            let recently_reopened = match self.last_idr_reopen_ms {
                Some(t) => frame.timestamp_ms.saturating_sub(t) < MIN_IDR_REOPEN_INTERVAL_MS,
                None => false,
            };
            if (self.force_next_keyframe || idr_overdue) && !recently_reopened {
                log::debug!(
                    "encoder can't force IDRs — reopening session for one \
                     (viewer_forced={}, cadence_overdue={})",
                    self.force_next_keyframe,
                    idr_overdue
                );
                // Salvage whatever the dying session already has ready
                // — with sparse input (static-screen refresh at ~1 fps)
                // the previous submission's AU, often the IDR we
                // reopened for last time, is sitting in the encoder and
                // would otherwise be destroyed with the session.
                if let Some(s) = self.session.as_mut() {
                    let _ = drain_packets(s, &mut out);
                }
                self.last_idr_reopen_ms = Some(frame.timestamp_ms);
                // Optimistically count the reopen as an IDR emission:
                // the new session's first output *is* one, but it may
                // surface a tick or two later — without this stamp the
                // overdue check re-fires before that happens and the
                // backstop reopens in a loop, killing every session
                // before it can produce anything.
                self.last_idr_out_ms = Some(frame.timestamp_ms);
                self.session = None;
            }
        }
        self.relax_qp_floor(Instant::now());
        self.ensure_session(frame, &mut out)?;

        // Strictly increasing PTS in the 1/1000 time base. Capture
        // timestamps are ms-resolution, so a burst can repeat a value.
        let mut pts = frame.timestamp_ms as i64;
        if let Some(last) = self.last_pts
            && pts <= last
        {
            pts = last + 1;
        }
        self.last_pts = Some(pts);

        // Time-based keyframe cadence: one IDR per KEYFRAME_INTERVAL_MS of
        // capture time, plus explicit force_keyframe() requests. This is
        // the single source of truth for cadence — the codec GOP is only a
        // long backstop (see GOP_BACKSTOP_SECS).
        let keyframe_due = match self.last_keyframe_ms {
            None => true,
            Some(last) => frame.timestamp_ms.saturating_sub(last) >= KEYFRAME_INTERVAL_MS,
        };
        let make_keyframe = self.force_next_keyframe || keyframe_due;
        if make_keyframe {
            self.force_next_keyframe = false;
            self.last_keyframe_ms = Some(frame.timestamp_ms);
        }

        let session = self.session.as_mut().expect("ensure_session sets this");

        // RGBA/BGRA (src) → YUV (out) via swscale, resize and colorspace
        // in one pass, read straight from the captured frame (review
        // P1-11: it was copied into an AVFrame first, 8 MB a 1080p frame).
        scale_packed(
            &mut session.sws,
            &frame.data,
            frame.width,
            frame.height,
            &mut session.yuv_frame,
        )?;

        session.yuv_frame.set_pts(Some(pts));
        if make_keyframe {
            // With the per-backend `forced-idr` options set in
            // codec_options(), a forced I picture is encoded as a true IDR
            // (with parameter sets), not an open-GOP I frame.
            session.yuv_frame.set_kind(ffmpeg::picture::Type::I);
        } else {
            session.yuv_frame.set_kind(ffmpeg::picture::Type::None);
        }

        // Async encoders (AMF): collect finished output BEFORE this
        // submission — see drain_before_submit() for the output-spacing
        // rationale. Others: after, for minimum latency.
        let drain_first = drain_before_submit(self.enc_name);
        if drain_first {
            drain_packets(session, &mut out)?;
        }

        match session.encoder.send_frame(&session.yuv_frame) {
            Ok(()) => {
                session.pending_ts.push_back(frame.timestamp_ms);
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                // Encoder's output queue is full — drain, then retry once.
                drain_packets(session, &mut out)?;
                session
                    .encoder
                    .send_frame(&session.yuv_frame)
                    .map_err(|e| CodecError::Encode(e.to_string()))?;
                session.pending_ts.push_back(frame.timestamp_ms);
            }
            Err(e) => return Err(CodecError::Encode(e.to_string())),
        }

        if !drain_first {
            drain_packets(session, &mut out)?;
        }
        if out.iter().any(|f| f.is_keyframe) {
            // Ground truth for the reopen-for-IDR backstop: a keyframe
            // actually left this encoder (stamped with the submitting
            // frame's clock; pipeline delay of a few frames is noise at
            // the 2 s cadence scale).
            self.last_idr_out_ms = Some(frame.timestamp_ms);
        }
        Ok(out)
    }

    /// One step back down after [`QP_FLOOR_RELAX_AFTER`] without a
    /// frame too large.
    fn relax_qp_floor(&mut self, now: Instant) {
        if let (Some(i), Some(moved)) = (self.qp_floor, self.qp_floor_moved)
            && now.duration_since(moved) >= QP_FLOOR_RELAX_AFTER
        {
            self.qp_floor = i.checked_sub(1);
            self.qp_floor_moved = Some(now);
        }
    }

    /// Returns the name of the active encoder (e.g. "`h264_nvenc`", "libx264").
    #[must_use]
    pub fn encoder_name(&self) -> &'static str {
        self.enc_name
    }
}

impl VideoEncoder for FfmpegEncoder {
    /// Encode one RGBA frame. See [`VideoEncoder::encode`] for the output
    /// contract (zero or more access units, one `EncodedFrame` each).
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>, CodecError> {
        self.encode_frame(frame, false)
    }

    /// See [`VideoEncoder::encode_repeat`]: the time since the last
    /// frame was spent on a still picture and does not count towards
    /// the next keyframe. A still share used to send a full IDR every
    /// [`KEYFRAME_INTERVAL_MS`] — every second refresh — and on AMF
    /// reopened its session for one every two intervals (review P2-41).
    /// A viewer who needs a keyframe asks, and
    /// [`Self::force_keyframe`] still gets one out at once.
    fn encode_repeat(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>, CodecError> {
        self.encode_frame(frame, true)
    }

    /// See [`VideoEncoder::frames_in_flight`].
    fn frames_in_flight(&self) -> usize {
        self.session.as_ref().map_or(0, |s| s.pending_ts.len())
    }

    /// See [`VideoEncoder::drain`].
    ///
    /// # Errors
    /// When the encoder fails while handing its output out.
    fn drain(&mut self) -> Result<Vec<EncodedFrame>, CodecError> {
        let mut out = Vec::new();
        let Some(session) = self.session.as_mut() else {
            return Ok(out);
        };
        if session.pending_ts.is_empty() {
            return Ok(out);
        }
        drain_packets(session, &mut out)?;
        if let Some(keyframe) = out.iter().find(|f| f.is_keyframe) {
            // Ground truth for the reopen-for-IDR backstop, as in encode.
            self.last_idr_out_ms = Some(keyframe.timestamp_ms);
        }
        Ok(out)
    }

    /// See [`VideoEncoder::output_needs_input`]: `FFmpeg` 7.1's AMF
    /// wrapper (`ff_amf_receive_packet`) returns at once, without asking
    /// AMF for output, when no new frame is queued — so a drain hands
    /// out only what an earlier submission already collected.
    fn output_needs_input(&self) -> bool {
        self.enc_name == "h264_amf"
    }

    /// Force the next encoded frame to be an IDR keyframe.
    ///
    /// Call this when a new viewer subscribes or reports an unrecoverable
    /// gap, so they immediately get a decodable frame with SPS/PPS.
    fn force_keyframe(&mut self) {
        self.force_next_keyframe = true;
    }

    /// Whether a forced keyframe is queued but not yet encoded.
    fn keyframe_pending(&self) -> bool {
        self.force_next_keyframe
    }

    /// See [`VideoEncoder::frame_too_large`]: the next quantizer floor,
    /// applied by the session [`Self::ensure_session`] opens next.
    fn frame_too_large(&mut self) -> Option<u32> {
        let next = qp_floor_after_oversize(self.qp_floor)?;
        self.qp_floor = Some(next);
        self.qp_floor_moved = Some(Instant::now());
        Some(QP_FLOORS[next])
    }

    /// The quantizer floor in force, if any.
    fn qp_floor(&self) -> Option<u32> {
        self.qp_floor.map(|i| QP_FLOORS[i])
    }

    /// See [`VideoEncoder::supports_forced_keyframes`]: everything we
    /// use honors `pict_type=I` except `FFmpeg`'s AMF wrapper (<= 7.1).
    fn supports_forced_keyframes(&self) -> bool {
        self.enc_name != "h264_amf"
    }

    /// See [`VideoEncoder::set_bitrate_scale_pct`]. Only records the
    /// request; [`Self::ensure_session`] reopens on the next encode if
    /// the running session was opened at a different scale.
    fn set_bitrate_scale_pct(&mut self, pct: u32) {
        self.bitrate_scale_pct = crate::clamp_bitrate_scale_pct(pct);
    }

    fn bitrate_scale_pct(&self) -> u32 {
        self.bitrate_scale_pct
    }
}

/// The floor index after a frame too large: the first, the next, or
/// `None` when the highest is already in force.
fn qp_floor_after_oversize(current: Option<usize>) -> Option<usize> {
    match current {
        None => Some(0),
        Some(i) if i + 1 < QP_FLOORS.len() => Some(i + 1),
        Some(_) => None,
    }
}

// SAFETY: These types own raw FFmpeg pointers (AVCodecContext, SwsContext,
// AVFrame, AVPacket), none of which are thread-affine — FFmpeg permits
// using a context from any thread as long as calls aren't concurrent, and
// `&mut self` on every method already guarantees that. `Send` (move-only)
// is therefore sound for every current field.
//
// MAINTENANCE HAZARD: these are blanket impls. If a genuinely non-Send
// field is ever added (an `Rc`, a thread-local handle, ...), these impls
// silently become unsound. Re-audit the field list when changing either
// struct.
unsafe impl Send for FfmpegEncoder {}
unsafe impl Send for FfmpegDecoder {}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// H.264 decoder via `FFmpeg`.
///
/// The YUV→RGBA scaler is created on the first decoded frame and rebuilt
/// automatically whenever the decoded dimensions, pixel format, or
/// colorimetry change mid-stream (sender quality switch, monitor change) —
/// no `flush()` choreography required for those.
pub struct FfmpegDecoder {
    decoder: ffmpeg::decoder::Video,
    sws: Option<DecoderSws>,
    /// Reusable `FFmpeg` frame for decoded YUV output.
    decoded_frame: VideoFrame,
    /// Scratch frame that `receive_frame` writes into.
    ///
    /// `avcodec_receive_frame` always unrefs its output frame first —
    /// including on the EAGAIN call that terminates a drain loop. Receiving
    /// directly into `decoded_frame` would therefore wipe the last good
    /// frame right before we convert it. So we receive here and swap on
    /// success; the terminating EAGAIN only ever clears the scratch.
    scratch_frame: VideoFrame,
    /// RGBA conversion output for widths that cannot be converted
    /// straight into a tight buffer (see [`DIRECT_WIDTH_MULTIPLE`]);
    /// made when first needed.
    rgba_frame: Option<VideoFrame>,
    /// The split of the last call that produced a picture.
    last_timing: Option<crate::DecodeTiming>,
}

/// Decoded widths that are a multiple of this convert straight into the
/// pooled buffer: every swscale path writes such a row exactly. Its
/// SIMD writes whole blocks — 8 pixels on x86, 16 on aarch64 — and on
/// other widths runs past the row's end, or (x86's unscaled YUV → RGB,
/// when the stride leaves no room) stops short of it. Checked against
/// `FFmpeg` 7.1's `libswscale` sources.
const DIRECT_WIDTH_MULTIPLE: u32 = 16;

/// The scaler plus the source parameters it was built for.
struct DecoderSws {
    ctx: SwsContext,
    w: u32,
    h: u32,
    format: Pixel,
    color_space: ffmpeg::color::Space,
    color_range: ffmpeg::color::Range,
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
            sws: None,
            decoded_frame: VideoFrame::empty(),
            scratch_frame: VideoFrame::empty(),
            rgba_frame: None,
            last_timing: None,
        })
    }

    /// Decode one encoded access unit into RGBA.
    ///
    /// Returns `Ok(None)` when the decoder needs more data (EAGAIN).
    /// If the decoder emits several frames at once (post-recovery), all
    /// are consumed and the most recent is returned — older ones have
    /// already served their purpose as reference frames.
    pub fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        if encoded.data.is_empty() {
            return Ok(None);
        }
        let started = std::time::Instant::now();

        let mut pkt = Packet::copy(&encoded.data);
        pkt.set_pts(Some(encoded.timestamp_ms as i64));
        pkt.set_dts(Some(encoded.timestamp_ms as i64));

        self.decoder
            .send_packet(&pkt)
            .map_err(|e| CodecError::Decode(e.to_string()))?;

        // Drain everything available; keep the newest frame. See the
        // `scratch_frame` field docs for why we can't receive directly
        // into `decoded_frame` here.
        let mut got_frame = false;
        loop {
            match self.decoder.receive_frame(&mut self.scratch_frame) {
                Ok(()) => {
                    std::mem::swap(&mut self.decoded_frame, &mut self.scratch_frame);
                    got_frame = true;
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(CodecError::Decode(e.to_string())),
            }
        }
        if !got_frame {
            return Ok(None);
        }
        let decoded = started.elapsed();

        let w = self.decoded_frame.width();
        let h = self.decoded_frame.height();
        let format = self.decoded_frame.format();
        let color_space = self.decoded_frame.color_space();
        let color_range = self.decoded_frame.color_range();

        // (Re)build the scaler when the source parameters change. This is
        // what makes mid-stream resolution/quality switches safe without
        // any external flush() choreography.
        let needs_rebuild = match &self.sws {
            None => true,
            Some(s) => {
                s.w != w
                    || s.h != h
                    || s.format != format
                    || s.color_space != color_space
                    || s.color_range != color_range
            }
        };
        if needs_rebuild {
            if self.sws.is_some() {
                log::info!("decoder: stream parameters changed, rebuilding scaler ({w}x{h})");
            }
            let mut ctx = SwsContext::get(format, w, h, Pixel::RGBA, w, h, Flags::FAST_BILINEAR)
                .map_err(|e| CodecError::ColorConversion(e.to_string()))?;

            // YUV(limited, signaled colorspace) → RGBA(full). Falls back to
            // BT.709/601-by-resolution when the stream is unsignaled.
            let coeffs = sws_coefficients_for(color_space, h);
            let src_full = color_range == ffmpeg::color::Range::JPEG;
            set_sws_colorspace(&mut ctx, coeffs, i32::from(src_full), 1);

            self.sws = Some(DecoderSws {
                ctx,
                w,
                h,
                format,
                color_space,
                color_range,
            });
            self.rgba_frame = None;
        }

        let sws = self.sws.as_mut().expect("built above");
        let data = if w.is_multiple_of(DIRECT_WIDTH_MULTIPLE) {
            // Straight into the buffer that goes to the present thread
            // (review P1-11: a scratch frame and a row copy before).
            convert_into_pool(&mut sws.ctx, &self.decoded_frame, w, h)?
        } else {
            // swscale's SIMD paths write whole blocks of pixels: past the
            // end of a row (and of a tight buffer) or short of it (the
            // x86 one, when the stride does not leave room). An AVFrame's
            // rows are padded for it; a width like 1366 takes this path.
            let rgba_frame = self
                .rgba_frame
                .get_or_insert_with(|| VideoFrame::new(Pixel::RGBA, w, h));
            sws.ctx
                .run(&self.decoded_frame, rgba_frame)
                .map_err(|e| CodecError::ColorConversion(e.to_string()))?;
            // Reuses a recycled buffer's allocation (`crate::pool`); rows
            // are appended, so nothing is zero-filled first.
            let stride = rgba_frame.stride(0);
            let src = rgba_frame.data(0);
            let row_bytes = w as usize * 4;
            let mut data = crate::pool::take(row_bytes * h as usize);
            for row in 0..h as usize {
                let src_start = row * stride;
                data.extend_from_slice(&src[src_start..src_start + row_bytes]);
            }
            data
        };
        self.last_timing = Some(crate::DecodeTiming {
            codec: decoded,
            output: started.elapsed().saturating_sub(decoded),
        });

        Ok(Some(DecodedFrame {
            data,
            width: w,
            height: h,
            timestamp_ms: encoded.timestamp_ms,
            format: crate::PictureFormat::Rgba,
        }))
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        self.decode(encoded)
    }

    fn last_timing(&self) -> Option<crate::DecodeTiming> {
        self.last_timing
    }

    fn flush(&mut self) {
        // avcodec_flush_buffers — clears internal reference frames so the
        // next keyframe decodes from a clean state after a known gap.
        self.decoder.flush();
        // The scaler is parameter-checked every frame and rebuilds itself,
        // so it does not need to be dropped here.
    }
}

// ---------------------------------------------------------------------------
// Session construction
// ---------------------------------------------------------------------------

/// Compute output dimensions: fit `src` inside the `Quality` bound,
/// preserving aspect ratio, never upscaling, rounded down to even
/// dimensions (4:2:0 chroma requires them), minimum 2×2.
fn fit_dimensions(src_w: u32, src_h: u32, quality: Quality) -> (u32, u32) {
    let (max_w, max_h) = (u64::from(quality.width()), u64::from(quality.height()));
    let (sw, sh) = (u64::from(src_w.max(1)), u64::from(src_h.max(1)));

    let (out_w, out_h) = if sw <= max_w && sh <= max_h {
        (sw, sh) // never upscale
    } else if sw * max_h >= sh * max_w {
        // width-bound: source is wider (proportionally) than the target box
        (max_w, (sh * max_w) / sw)
    } else {
        // height-bound
        ((sw * max_h) / sh, max_h)
    };

    (((out_w as u32) & !1).max(2), ((out_h as u32) & !1).max(2))
}

/// Whether this encoder's output should be collected *before*
/// submitting the new frame rather than after. AMF's asynchronous
/// pipeline finishes an AU ~15-20 ms after submission — right at one
/// 60 fps tick — so a submit-then-drain loop collects AUs in an
/// alternating 0,2,0,2 pattern: sixty per second, but paired
/// back-to-back thirty times a second. Viewers decode all sixty, then
/// UI coalescing collapses each pair into one displayed frame — the
/// sharer looks like 30 fps despite every rate counter reading 60.
/// Draining at the top of the call gives every AU a full tick to
/// become ready, restoring one-AU-per-tick spacing at the cost of ≤1
/// tick (~17 ms) of extra latency. NVENC/QSV/x264 finish well inside a
/// tick and keep the lower-latency submit-then-drain path.
fn drain_before_submit(name: &str) -> bool {
    name == "h264_amf"
}

/// Input pixel format for a given encoder. Most accept planar YUV420P;
/// Intel QSV only takes the semi-planar NV12 layout (same 4:2:0 subsampling,
/// interleaved chroma). swscale converts RGBA to either directly.
fn encoder_pix_fmt(name: &str) -> Pixel {
    match name {
        // VideoToolbox takes planar frames too, and converts them; NV12
        // is what its hardware reads.
        "h264_qsv" | "h264_videotoolbox" => Pixel::NV12,
        _ => Pixel::YUV420P,
    }
}

/// Open one encoding session. `quality` sets the resolution bound and
/// frame rate; the bitrate is the preset's scaled to `bitrate_scale_pct`
/// (see [`RateControl`] for everything derived from it); `qp_floor` is
/// the minimum quantizer after frames came out too large ([`QP_FLOORS`]).
#[allow(clippy::too_many_arguments)] // one session's whole shape
fn open_session(
    codec: ffmpeg::Codec,
    name: &str,
    quality: Quality,
    bitrate_scale_pct: u32,
    qp_floor: Option<u32>,
    src_w: u32,
    src_h: u32,
    src_format: PixelFormat,
) -> Result<EncSession, CodecError> {
    let (out_w, out_h) = fit_dimensions(src_w, src_h, quality);
    let rc = RateControl::for_target(quality.scaled_bitrate(bitrate_scale_pct));
    let pix_fmt = encoder_pix_fmt(name);
    // swscale takes either 4-byte layout natively — BGRA straight from
    // DXGI costs the same as RGBA and saves the capture side a
    // full-frame swizzle pass.
    let in_pix = match src_format {
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Bgra => Pixel::BGRA,
    };

    // new_with_codec (allocation *with* the codec) is required, not
    // cosmetic: it applies the codec's own default table. Allocating a
    // bare context leaves FFmpeg's generic legacy defaults (qmin=2,
    // qmax=31, ...) in place, which libx264 detects and rejects with
    // "broken ffmpeg default settings detected".
    let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|e| CodecError::EncoderInit(e.to_string()))?;

    ctx.set_width(out_w);
    ctx.set_height(out_h);
    ctx.set_format(pix_fmt);
    // 1/1000 time base so PTS carries real capture milliseconds — keeps
    // rate control honest under variable capture rates.
    ctx.set_time_base(Rational(1, 1000));
    ctx.set_frame_rate(Some(Rational(quality.fps() as i32, 1)));
    ctx.set_bit_rate(rc.target as usize);
    ctx.set_max_bit_rate(rc.maxrate as usize);
    // Backstop GOP only — the real cadence is the time-based IDR forcing
    // in encode(). Keeping this much longer than KEYFRAME_INTERVAL_MS
    // means exactly one mechanism decides where keyframes go.
    //
    // EXCEPT h264_amf: FFmpeg's AMF wrapper (through 7.1) ignores
    // frame-level pict_type entirely — there is NO way to force an IDR
    // mid-stream. The manual cadence silently produces P-frames, so the
    // encoder-side IDR period is the only keyframe mechanism AMF has;
    // set it to the design cadence instead of the backstop. (FFmpeg 8's
    // rewritten amfenc honors forced_idr — revisit on upgrade.)
    let gop_secs = if name == "h264_amf" {
        (KEYFRAME_INTERVAL_MS / 1000) as u32
    } else {
        GOP_BACKSTOP_SECS
    };
    ctx.set_gop(quality.fps() * gop_secs);
    ctx.set_max_b_frames(0);

    // Signal BT.709 limited-range in the bitstream VUI so any decoder —
    // not just ours — converts colors correctly.
    unsafe {
        let p = ctx.as_mut_ptr();
        (*p).colorspace = ffmpeg::ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*p).color_primaries = ffmpeg::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*p).color_trc = ffmpeg::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*p).color_range = ffmpeg::ffi::AVColorRange::AVCOL_RANGE_MPEG;
    }

    let mut opts = codec_options(name, rc);
    if let Some(q) = qp_floor {
        // `qmin` is the context's own option; avcodec_open2 applies it
        // and every backend maps it to its minimum QP (AMF:
        // AMF_VIDEO_ENCODER_MIN_QP, bound from the first frame).
        opts.set("qmin", &q.to_string());
    }
    let encoder = ctx
        .open_with(opts)
        .map_err(|e| CodecError::EncoderInit(format!("{name}: {e}")))?;

    // swscale: RGBA(src, full-range sRGB) → YUV420P(out, BT.709 limited),
    // resize + colorspace conversion in one pass. BICUBIC when actually
    // resizing — much sharper than FAST_BILINEAR for text and hard edges.
    let flags = if src_w == out_w && src_h == out_h {
        Flags::FAST_BILINEAR
    } else {
        Flags::BICUBIC
    };
    let mut sws = SwsContext::get(in_pix, src_w, src_h, pix_fmt, out_w, out_h, flags)
        .map_err(|e| CodecError::ColorConversion(e.to_string()))?;
    set_sws_colorspace(
        &mut sws,
        unsafe { ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_ITU709) },
        1, // RGB input is full range
        0, // YUV output is limited range
    );

    let mut yuv_frame = VideoFrame::new(pix_fmt, out_w, out_h);
    // Tag the frames we hand to the encoder with the same colorimetry.
    unsafe {
        let f = yuv_frame.as_mut_ptr();
        (*f).colorspace = ffmpeg::ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*f).color_primaries = ffmpeg::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*f).color_trc = ffmpeg::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*f).color_range = ffmpeg::ffi::AVColorRange::AVCOL_RANGE_MPEG;
    }

    Ok(EncSession {
        encoder,
        sws,
        src_w,
        src_h,
        src_format,
        out_w,
        out_h,
        bitrate_scale_pct,
        qp_floor,
        yuv_frame,
        pkt: Packet::empty(),
        pending_ts: VecDeque::new(),
    })
}

/// Receive all currently-available packets from the encoder, emitting one
/// `EncodedFrame` per packet (one access unit each — never concatenated).
fn drain_packets(session: &mut EncSession, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
    loop {
        match session.encoder.receive_packet(&mut session.pkt) {
            Ok(()) => {
                let Some(data) = session.pkt.data() else {
                    continue;
                };
                let timestamp_ms = session
                    .pending_ts
                    .pop_front()
                    .unwrap_or_else(|| session.pkt.pts().unwrap_or(0).max(0) as u64);
                out.push(EncodedFrame {
                    data: data.to_vec(),
                    timestamp_ms,
                    is_keyframe: session.pkt.is_key(),
                    width: session.out_w,
                    height: session.out_h,
                });
            }
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(CodecError::Encode(e.to_string())),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build codec-specific option dictionaries for screen-share encoding.
///
/// Key design decisions for libx264:
///
/// - **`veryfast` preset** (not `ultrafast`): Re-enables adaptive quantization,
///   better motion estimation refinement, and trellis. This is the single biggest
///   quality lever — `ultrafast` disables almost all quality tools and produces
///   visibly worse output at the same bitrate.
///
/// - **`high` profile**: CABAC + 8×8 DCT, ~15-20% better compression vs
///   baseline. We control both endpoints, so compatibility is not a concern.
///   B-frames stay disabled via `max_b_frames(0)`.
///
/// - **`zerolatency` tune**: Disables lookahead and frame reordering.
///
/// - **ABR + VBV, no CRF**: the session's [`RateControl`] target (the
///   `Quality` preset bitrate, scaled by the sender's ladder) is the
///   actual rate-control target. (Setting `crf` alongside a bitrate makes
///   x264 ignore the bitrate entirely — capped CRF — which made the preset
///   numbers decorative. One rate-control story now.)
///
/// - **`forced-idr`** on every backend: frames we force via
///   `picture::Type::I` become true IDRs with parameter sets, not open-GOP
///   I frames. Without this, hardware encoders can emit a forced "keyframe"
///   that a newly-joined viewer cannot actually start decoding from.
fn codec_options(name: &str, rc: RateControl) -> Dictionary<'static> {
    let mut opts = Dictionary::new();

    let RateControl {
        maxrate,
        bufsize: vbv_bufsize,
        ..
    } = rc;

    match name {
        "libx264" => {
            opts.set("preset", "veryfast");
            opts.set("tune", "zerolatency");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
            opts.set("aq-mode", "3");
            opts.set("profile", "high");
            opts.set("forced-idr", "1");
        }
        "h264_nvenc" => {
            // p4 (default/"medium"), not p1: NVENC runs on a dedicated
            // ASIC block with enormous headroom at 1080p60 — p1 trades
            // visible quality for speed nobody needs, and the encode
            // block is separate from the 3D units, so a slower preset
            // does not touch game performance.
            opts.set("preset", "p4");
            opts.set("tune", "ull");
            opts.set("rc", "cbr");
            opts.set("zerolatency", "1");
            // Same job as x264's aq-mode: shift bits toward detailed
            // regions (text, UI edges) — the content screen share is
            // mostly made of.
            opts.set("spatial-aq", "1");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
            opts.set("forced-idr", "1");
            // Hand each access unit out on the call that submitted its
            // frame. The default holds output back until that many
            // frames are queued (`delay`, stored as `async_depth`: INT_MAX,
            // capped at the surface count − 1, in FFmpeg 7.1's
            // nvenc_h264.c / nvenc.c `output_ready`), so a still screen's
            // last change waited for the next refresh (review P1-13).
            opts.set("delay", "0");
        }
        "h264_qsv" => {
            opts.set("preset", "veryfast");
            opts.set("low_power", "1");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
            // A packet leaves only once `async_depth` frames are queued
            // (default 4; FFmpeg 7.1's qsvenc.h / `ff_qsv_encode`): 1 hands
            // each out on the call that submitted its frame (review P1-13).
            opts.set("async_depth", "1");
            // No-op on FFmpeg <= 7.1 (the option doesn't exist there;
            // unknown dict keys are silently ignored) but honored by
            // FFmpeg 8's rewritten amfenc — kept so an ffmpeg upgrade
            // restores on-demand IDRs without a code change.
            opts.set("forced_idr", "1");
        }
        "h264_videotoolbox" => {
            // The Mac's hardware encoder. The release build has had it
            // all along and the list never tried it, so a Mac encoded
            // its share with x264 on the CPU (review P2-42).
            //
            // Hardware or nothing: a Mac without one fails the probe
            // and gets libx264, tuned above, not VideoToolbox's own
            // software encoder.
            opts.set("allow_sw", "0");
            // No lookahead, no reordering: a frame is encoded as it
            // arrives. It finishes after the call that submitted it;
            // the next call, or the next tick's `drain`, hands it out.
            opts.set("realtime", "1");
            opts.set("profile", "high");
            // Besides the target, the one bound VideoToolbox takes: a
            // data-rate limit over a second. `bufsize` means nothing
            // to it. A frame forced to `I` is an IDR with its
            // parameter sets, as `FFmpeg` writes it out.
            opts.set("maxrate", &format!("{maxrate}"));
        }
        "h264_amf" => {
            // NO usage preset — deliberately. FFmpeg's AMF wrapper
            // (<= 7.1) assigns IDR_PERIOD *after* the AMF component is
            // initialized; under usage presets (observed with
            // ultralowlatency) that late assignment fails silently and
            // the stream never gets another IDR after frame 0. The
            // default (transcoding) usage is the well-trodden path, and
            // everything the preset tuned is set explicitly below
            // (CBR + HRD, no filler, no frame skipping, bf=0 via
            // max_b_frames).
            // "balanced", not "speed": AMD's encode block also has
            // headroom to spare at these resolutions — same trade as
            // NVENC p4 above.
            opts.set("quality", "balanced");
            // Explicit CBR + HRD: left on auto, amfenc picks
            // peak-constrained VBR (because max_bit_rate is set) with
            // HRD enforcement OFF — VBV becomes advisory and IDRs
            // overshoot the transport's per-frame cap (observed: 359 KB
            // against a 275 KB limit → dropped keyframes). enforce_hrd
            // makes bufsize binding, like x264's VBV. filler_data off:
            // CBR padding on static scenes would defeat the capture
            // loop's skip-encode entirely. frame_skipping off: keep
            // one-AU-per-frame semantics predictable for the transport.
            opts.set("rc", "cbr");
            opts.set("enforce_hrd", "1");
            opts.set("filler_data", "0");
            opts.set("frame_skipping", "0");
            opts.set("maxrate", &format!("{maxrate}"));
            opts.set("bufsize", &format!("{vbv_bufsize}"));
            // enforce_hrd is not enough on its own: the *first* IDR of a
            // fresh session comes out of cold rate control and ignores
            // the buffer (2026-08-30 log: 30 of 31 oversize drops were
            // the first frame after a reopen, 280–578 KB). Since every
            // viewer keyframe request is a reopen on AMF, that turned
            // each request into a dropped keyframe, a forced retry, and
            // a multi-second freeze for everyone. AMF's own per-AU cap
            // (`AMF_VIDEO_ENCODER_MAX_AU_SIZE`, bits) is binding from
            // frame 0; set it just under the transport limit.
            opts.set("max_au_size", &format!("{AMF_MAX_AU_BITS}"));
            // No-op on FFmpeg <= 7.1 (the option doesn't exist there;
            // unknown dict keys are silently ignored) but honored by
            // FFmpeg 8's rewritten amfenc — kept so an ffmpeg upgrade
            // restores on-demand IDRs without a code change.
            opts.set("forced_idr", "1");
            // CRITICAL for this pipeline: AMF only emits SPS/PPS at
            // stream start by default, so a viewer who joins mid-stream
            // receives IDRs they can never decode — observed as an
            // endless keyframe-request loop. x264 repeats headers
            // before every IDR by default; header_spacing=1 makes AMF
            // attach them to every AU (~50 bytes/frame, noise at our
            // bitrates), so any IDR primes a fresh decoder instantly.
            opts.set("header_spacing", "1");
        }
        _ => {}
    }
    opts
}

/// Pick the swscale coefficient table for a signaled (or unsignaled)
/// colorspace.
fn sws_coefficients_for(space: ffmpeg::color::Space, height: u32) -> *const i32 {
    use ffmpeg::color::Space;
    let cs = match space {
        Space::BT709 => ffmpeg::ffi::SWS_CS_ITU709,
        Space::BT470BG | Space::SMPTE170M | Space::SMPTE240M => ffmpeg::ffi::SWS_CS_ITU601,
        // Unsignaled: guess by resolution, the same convention players use.
        _ if height >= 720 => ffmpeg::ffi::SWS_CS_ITU709,
        _ => ffmpeg::ffi::SWS_CS_ITU601,
    };
    unsafe { ffmpeg::ffi::sws_getCoefficients(cs) }
}

/// Apply colorspace coefficients + ranges to a swscale context.
///
/// `src_range`/`dst_range`: 0 = limited (MPEG), 1 = full (JPEG/RGB).
fn set_sws_colorspace(sws: &mut SwsContext, coeffs: *const i32, src_range: i32, dst_range: i32) {
    unsafe {
        let ret = ffmpeg::ffi::sws_setColorspaceDetails(
            sws.as_mut_ptr(),
            coeffs,
            src_range,
            coeffs,
            dst_range,
            0,       // brightness
            1 << 16, // contrast (fixed-point 1.0)
            1 << 16, // saturation (fixed-point 1.0)
        );
        if ret < 0 {
            log::warn!("sws_setColorspaceDetails unsupported for this conversion");
        }
    }
}

/// Convert a decoded `w × h` picture to tightly packed RGBA straight into
/// a buffer from `crate::pool` (rows `w × 4` bytes apart). Only for
/// widths that are a multiple of [`DIRECT_WIDTH_MULTIPLE`].
fn convert_into_pool(
    sws: &mut SwsContext,
    src: &VideoFrame,
    w: u32,
    h: u32,
) -> Result<Vec<u8>, CodecError> {
    let row_bytes = w as usize * 4;
    let len = row_bytes * h as usize;
    let mut data = crate::pool::take(len);
    let planes: [*mut u8; 4] = [
        data.as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
    ];
    let strides: [c_int; 4] = [row_bytes as c_int, 0, 0, 0];
    // SAFETY: the scaler was built for `src`'s format and size with a
    // `w × h` RGBA output (`decode` rebuilds it when either changes);
    // `src` is a decoded frame whose planes FFmpeg owns, and `data` has
    // room for `h` rows of `row_bytes`, which is exactly what a width
    // that is a multiple of `DIRECT_WIDTH_MULTIPLE` makes it write.
    let rows = unsafe {
        let input = src.as_ptr();
        ffmpeg::ffi::sws_scale(
            sws.as_mut_ptr(),
            (*input).data.as_ptr().cast(),
            (*input).linesize.as_ptr(),
            0,
            h as c_int,
            planes.as_ptr(),
            strides.as_ptr(),
        )
    };
    if rows != h as c_int {
        crate::pool::recycle(data);
        return Err(CodecError::ColorConversion(format!(
            "sws_scale wrote {rows} of {h} rows"
        )));
    }
    // SAFETY: every one of the `h` rows of `row_bytes` was written
    // (checked above), so the first `len` bytes are initialised.
    unsafe { data.set_len(len) };
    Ok(data)
}

/// Run the encoder's scaler on a tightly packed 4-byte-per-pixel frame
/// (`width × height`, the size the scaler was built for) into `dst`.
fn scale_packed(
    sws: &mut SwsContext,
    src: &[u8],
    width: u32,
    height: u32,
    dst: &mut VideoFrame,
) -> Result<(), CodecError> {
    let row_bytes = width as usize * 4;
    let needed = row_bytes * height as usize;
    if src.len() < needed {
        return Err(CodecError::Encode(format!(
            "frame data too small: {} bytes for {width}x{height} (need {needed})",
            src.len()
        )));
    }
    let planes: [*const u8; 4] = [src.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
    let strides: [c_int; 4] = [row_bytes as c_int, 0, 0, 0];
    // SAFETY: the scaler was built for a `width × height` packed input
    // (`open_session`) and `src` holds that many rows of `row_bytes`
    // (checked above); `dst` is the session's own frame, allocated by
    // `VideoFrame::new` at the scaler's output size and format, so its
    // planes and line sizes are what `sws_scale` writes through.
    let rows = unsafe {
        let out = dst.as_mut_ptr();
        ffmpeg::ffi::sws_scale(
            sws.as_mut_ptr(),
            planes.as_ptr(),
            strides.as_ptr(),
            0,
            height as c_int,
            (*out).data.as_ptr(),
            (*out).linesize.as_ptr(),
        )
    };
    if rows <= 0 {
        return Err(CodecError::ColorConversion(format!(
            "sws_scale wrote no rows ({rows})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod rate_control {
        use super::*;
        use crate::MIN_BITRATE_SCALE_PCT;

        #[test]
        fn maxrate_is_one_and_a_half_times_target() {
            assert_eq!(RateControl::for_target(8_000_000).maxrate, 12_000_000);
        }

        #[test]
        fn bufsize_equals_target_below_the_ceiling() {
            assert_eq!(RateControl::for_target(1_200_000).bufsize, 1_200_000);
        }

        #[test]
        fn bufsize_is_capped_at_the_ceiling() {
            assert_eq!(RateControl::for_target(10_000_000).bufsize, VBV_MAX_BITS);
        }

        /// The per-frame transport cap (`protocol::MAX_VIDEO_FRAME_BYTES`,
        /// 275,400 B) must hold at every rung of every preset: bufsize
        /// bounds a keyframe, so it must stay under the ceiling that was
        /// sized against that cap.
        #[test]
        fn bufsize_never_exceeds_ceiling_for_any_preset_at_any_scale() {
            for &q in Quality::ALL {
                for pct in MIN_BITRATE_SCALE_PCT..=100 {
                    let rc = RateControl::for_target(q.scaled_bitrate(pct));
                    assert!(
                        rc.bufsize <= VBV_MAX_BITS,
                        "{q} at {pct}%: bufsize {} > {VBV_MAX_BITS}",
                        rc.bufsize
                    );
                }
            }
        }

        #[test]
        fn scaling_shrinks_all_three_numbers_together() {
            let full = RateControl::for_target(Quality::FHD60.scaled_bitrate(100));
            let half = RateControl::for_target(Quality::FHD60.scaled_bitrate(50));
            assert!(half.target < full.target);
            assert!(half.maxrate < full.maxrate);
            assert!(half.bufsize <= full.bufsize);
        }
    }

    #[test]
    fn fit_preserves_aspect_and_never_upscales() {
        // 16:9 native match
        assert_eq!(fit_dimensions(1920, 1080, Quality::FHD30), (1920, 1080));
        // 16:10 source at HD30 (1280x720 bound) → height-bound
        assert_eq!(fit_dimensions(1920, 1200, Quality::HD30), (1152, 720));
        // Ultrawide → width-bound
        assert_eq!(fit_dimensions(3440, 1440, Quality::HD30), (1280, 534));
        // Portrait → height-bound
        assert_eq!(fit_dimensions(1080, 1920, Quality::HD30), (404, 720));
        // QHD bound: native 2K passes through, ultrawide/4K fit inside
        assert_eq!(fit_dimensions(2560, 1440, Quality::QHD30), (2560, 1440));
        assert_eq!(fit_dimensions(3840, 2160, Quality::QHD30), (2560, 1440));
        assert_eq!(fit_dimensions(3440, 1440, Quality::QHD30), (2560, 1070));
        // Smaller than target: unchanged (no upscale)
        assert_eq!(fit_dimensions(1280, 720, Quality::FHD30), (1280, 720));
        assert_eq!(fit_dimensions(800, 600, Quality::HD30), (800, 600));
        // Already-even odd-ball resolution passes through
        assert_eq!(fit_dimensions(1366, 768, Quality::FHD30), (1366, 768));
        // Odd input snaps to even
        let (w, h) = fit_dimensions(1367, 769, Quality::FHD30);
        assert_eq!((w % 2, h % 2), (0, 0));
        // Degenerate inputs can't panic or produce 0
        let (w, h) = fit_dimensions(1, 1, Quality::HD30);
        assert!(w >= 2 && h >= 2);
        let (w, h) = fit_dimensions(10_000, 2, Quality::HD30);
        assert!(w >= 2 && h >= 2 && w <= 1280);
    }
}
