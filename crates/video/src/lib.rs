//! # Architecture
//!
//! ```text
//! ┌─────────┐     ┌─────────┐     ┌──────────────┐
//! │ Capture │ ──▶ │ Encoder │ ──▶ │ encoded bytes │ ──▶ network / file
//! └─────────┘     └─────────┘     └──────────────┘
//!
//! network ──▶ ┌──────────────┐ ──▶ ┌─────────┐ ──▶ ┌────────────┐
//!             │ JitterBuffer │     │ Decoder │     │ RGBA frame  │ ──▶ display
//!             └──────────────┘     └─────────┘     └────────────┘
//! ```
//!
//! The encoder preserves the source aspect ratio: `Quality` sets an upper
//! *bound* on resolution, and the actual encoded dimensions (carried in
//! [`EncodedFrame::width`]/[`EncodedFrame::height`]) are the source scaled
//! to fit inside that bound. A 1920×1200 monitor shared at `HD30` encodes
//! as 1152×720, not a stretched 1280×720.

pub mod buffer;
pub mod capture;
#[cfg(target_os = "windows")]
pub mod dxgi;

// The real codec module is FFmpeg top to bottom, so it's swapped
// wholesale on targets without an FFmpeg build. The stub exposes the
// same types with constructors that fail cleanly — client code compiles
// unchanged and its existing error paths handle the rest.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub mod codec;
#[cfg(any(target_os = "ios", target_os = "android"))]
#[path = "codec_stub.rs"]
pub mod codec;

// Android decodes with the platform's MediaCodec instead (the stub
// above still provides the encoder, which Android does not have yet),
// iOS with VideoToolbox. `yuv` is their step to RGBA, `annexb` the
// re-framing VideoToolbox needs and `held` MediaCodec's pacing of the
// pictures it finishes, all platform-free so they are tested
// everywhere.
pub mod annexb;
pub mod held;
#[cfg(target_os = "android")]
pub mod mediacodec;
pub mod pool;
#[cfg(target_os = "ios")]
pub mod videotoolbox;
pub mod yuv;

/// Quality preset for screen sharing.
///
/// Each variant encodes a resolution *bound*, frame rate, and target
/// bitrate tuned for screen content (sharp text, flat gradients,
/// occasional motion). The encoder scales the source to fit inside
/// `width() × height()` while preserving aspect ratio, and never
/// upscales.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quality {
    /// ≤1280×720 @ 30 FPS — lowest bandwidth, good for slow connections.
    HD30,
    /// ≤1280×720 @ 60 FPS — smooth scrolling at modest bandwidth.
    HD60,
    /// ≤1920×1080 @ 30 FPS — sharp text, moderate motion.
    FHD30,
    /// ≤1920×1080 @ 60 FPS — best quality, needs hardware encoder.
    FHD60,
    /// ≤2560×1440 @ 30 FPS — near-native for 2K monitors, ~10 Mbps.
    ///
    /// Note: the transport's ~275 KB per-frame limit means keyframes get
    /// fewer bits per pixel than at 1080p, so the first frame after each
    /// IDR is slightly softer until P-frames sharpen it. Prefer FHD
    /// unless viewers genuinely need to read fine detail at 1:1.
    QHD30,
}
impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HD30 => write!(f, "720p 30fps"),
            Self::HD60 => write!(f, "720p 60fps"),
            Self::FHD30 => write!(f, "1080p 30fps"),
            Self::FHD60 => write!(f, "1080p 60fps"),
            Self::QHD30 => write!(f, "1440p 30fps"),
        }
    }
}

impl Quality {
    /// Maximum encoded width. The actual encoded width may be smaller
    /// to preserve the source aspect ratio.
    #[must_use]
    pub const fn width(self) -> u32 {
        match self {
            Self::HD30 | Self::HD60 => 1280,
            Self::FHD30 | Self::FHD60 => 1920,
            Self::QHD30 => 2560,
        }
    }

    /// Maximum encoded height. The actual encoded height may be smaller
    /// to preserve the source aspect ratio.
    #[must_use]
    pub const fn height(self) -> u32 {
        match self {
            Self::HD30 | Self::HD60 => 720,
            Self::FHD30 | Self::FHD60 => 1080,
            Self::QHD30 => 1440,
        }
    }

    #[must_use]
    pub const fn fps(self) -> u32 {
        match self {
            Self::HD30 | Self::FHD30 | Self::QHD30 => 30,
            Self::HD60 | Self::FHD60 => 60,
        }
    }

    /// Target bitrate in bits/sec at 100% scale.
    ///
    /// Tuned for **screen content** (sharp text, hard edges, flat colors).
    /// Screen content needs ~1.5–2× more bitrate than camera video at the
    /// same resolution because DCT-based codecs spend more bits preserving
    /// high-frequency detail (glyphs, 1px borders, UI elements).
    ///
    /// This is the *actual* encoder target (ABR + VBV), not a hint. The
    /// sender may run below it under uplink pressure — see
    /// [`Quality::scaled_bitrate`] and [`VideoEncoder::set_bitrate_scale_pct`].
    #[must_use]
    pub const fn bitrate(self) -> u32 {
        match self {
            Self::HD30 => 3_500_000,
            Self::HD60 => 4_500_000,
            Self::FHD30 => 6_000_000,
            Self::FHD60 => 8_000_000,
            Self::QHD30 => 10_000_000,
        }
    }

    /// [`Quality::bitrate`] scaled to `pct` percent, with `pct` clamped to
    /// [`MIN_BITRATE_SCALE_PCT`]`..=100`. Resolution and frame rate are
    /// unaffected — a scaled session encodes the same frames with fewer
    /// bits each.
    // `From` isn't const, so the widening is an `as`; the result is ≤
    // bitrate() and can't truncate.
    #[allow(clippy::cast_lossless, clippy::cast_possible_truncation)]
    #[must_use]
    pub const fn scaled_bitrate(self, pct: u32) -> u32 {
        let pct = clamp_bitrate_scale_pct(pct);
        // u64 intermediate: 10 Mbit × 100 overflows u32.
        (self.bitrate() as u64 * pct as u64 / 100) as u32
    }

    /// Duration of one frame.
    #[must_use]
    pub const fn frame_duration(self) -> std::time::Duration {
        std::time::Duration::from_nanos(1_000_000_000 / self.fps() as u64)
    }

    /// Pixel count at the maximum resolution for this preset.
    #[must_use]
    pub const fn pixel_count(self) -> usize {
        self.width() as usize * self.height() as usize
    }

    /// Byte count for one RGBA frame at the maximum resolution.
    #[must_use]
    pub const fn rgba_size(self) -> usize {
        self.pixel_count() * 4
    }

    /// Byte count for one YUV420 frame at the maximum resolution.
    #[must_use]
    pub const fn yuv420_size(self) -> usize {
        self.pixel_count() * 3 / 2
    }

    pub const ALL: &'static [Quality] = &[
        Self::HD30,
        Self::HD60,
        Self::FHD30,
        Self::FHD60,
        Self::QHD30,
    ];

    /// Stable string key for persisting a preset in settings files.
    ///
    /// Unlike an index into [`Quality::ALL`], these survive reordering
    /// or insertion of new presets in future versions.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::HD30 => "hd30",
            Self::HD60 => "hd60",
            Self::FHD30 => "fhd30",
            Self::FHD60 => "fhd60",
            Self::QHD30 => "qhd30",
        }
    }

    /// Inverse of [`Quality::key`]. Returns `None` for unknown keys
    /// (e.g. a settings file written by a newer version).
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|q| q.key() == key)
    }
}

/// Lowest bitrate scale an encoder accepts, in percent of the preset. A
/// mechanism bound, not policy: the sender's ladder decides how far down
/// it actually goes (and applies its own absolute floor); this only keeps
/// a stray value from opening a session at a bitrate rate control can't
/// work with.
pub const MIN_BITRATE_SCALE_PCT: u32 = 10;

/// Clamp a bitrate scale into the range the encoder honors.
#[must_use]
pub const fn clamp_bitrate_scale_pct(pct: u32) -> u32 {
    if pct < MIN_BITRATE_SCALE_PCT {
        MIN_BITRATE_SCALE_PCT
    } else if pct > 100 {
        100
    } else {
        pct
    }
}

// ---------------------------------------------------------------------------
// Frame types
// ---------------------------------------------------------------------------

/// Byte layout of a [`Frame`]'s pixel data (4 bytes per pixel either way).
///
/// The DXGI streaming backend delivers **BGRA** exactly as the desktop
/// stores it; converting to RGBA on the CPU would be a wasted full-frame
/// pass, since swscale accepts BGRA input directly. The screenshot
/// backend (and everything else) stays RGBA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
}

/// A raw captured frame ready for encoding (see [`PixelFormat`] for the
/// byte layout).
///
/// Not `Clone` by design — pixel buffers are large (3.7 MB at 1080p).
/// Pass by reference or transfer ownership explicitly.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Capture timestamp in milliseconds (monotonic, capturer-relative).
    /// Used as the encoder PTS, so it also drives keyframe cadence and
    /// rate control under variable capture rates.
    pub timestamp_ms: u64,
    pub format: PixelFormat,
}

/// Encoded H.264 data for one frame (one access unit).
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub is_keyframe: bool,
    /// Actual encoded width (aspect-preserving fit inside the Quality bound).
    pub width: u32,
    /// Actual encoded height (aspect-preserving fit inside the Quality bound).
    pub height: u32,
}

/// How a [`DecodedFrame`]'s bytes are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureFormat {
    /// Tightly packed RGBA, `width × height × 4` bytes.
    Rgba,
    /// The decoder's own 4:2:0 planes (Android's `MediaCodec`). Whoever
    /// shows the picture converts it ([`yuv::to_rgba_into`]) straight
    /// into the buffer it draws from, and only the pictures it will
    /// actually draw: converting every decoded frame, one core at a
    /// time, was most of a mid-range phone's time per frame (MOB-18).
    Yuv420 {
        layout: yuv::Yuv420Layout,
        color: yuv::ColorSpec,
    },
}

/// A decoded frame ready for display: RGBA, or YUV planes to convert
/// (see [`PictureFormat`]). Give `data` back to [`pool::recycle`] once
/// done with it.
///
/// Not `Clone` by design — same reasoning as [`Frame`].
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp_ms: u64,
    pub format: PictureFormat,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors from screen capture operations.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("failed to enumerate monitors: {0}")]
    MonitorList(String),
    #[error("monitor index {0} not found")]
    MonitorNotFound(usize),
    #[error("capture failed: {0}")]
    Capture(String),
}

/// Errors from encode/decode operations.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("encoder init failed: {0}")]
    EncoderInit(String),
    #[error("decoder init failed: {0}")]
    DecoderInit(String),
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("color conversion failed: {0}")]
    ColorConversion(String),
    #[error("no usable H.264 encoder (tried: {0})")]
    NoEncoder(String),
}

// ---------------------------------------------------------------------------
// Codec traits — allows swapping backends generically
// ---------------------------------------------------------------------------

/// Human-readable list of H.264 NAL unit types in an Annex-B access
/// unit, e.g. `"SPS,PPS,IDR"` — a diagnostic aid for verifying what an
/// encoder actually emitted. Hardware encoder wrappers have quirks
/// (missing in-band parameter sets, I-slices flagged as keyframes),
/// and this makes them visible from the sender's log alone.
#[must_use]
pub fn describe_annexb_nals(data: &[u8]) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i + 3 < data.len() {
        if data[i] != 0 || data[i + 1] != 0 {
            i += 1;
            continue;
        }
        let hdr = if data[i + 2] == 1 {
            i + 3
        } else if data[i + 2] == 0 && i + 4 < data.len() && data[i + 3] == 1 {
            i + 4
        } else {
            i += 1;
            continue;
        };
        if hdr >= data.len() {
            break;
        }
        let name = match data[hdr] & 0x1F {
            1 => "slice".to_string(),
            5 => "IDR".to_string(),
            6 => "SEI".to_string(),
            7 => "SPS".to_string(),
            8 => "PPS".to_string(),
            9 => "AUD".to_string(),
            t => format!("nal{t}"),
        };
        names.push(name);
        i = hdr + 1;
    }
    if names.is_empty() {
        "no start codes found".to_string()
    } else {
        names.join(",")
    }
}

/// Encoder that converts RGBA frames into an H.264 bitstream.
pub trait VideoEncoder {
    /// Encode one RGBA frame.
    ///
    /// Returns **zero or more** encoded access units:
    ///
    /// - Zero while a hardware encoder is still filling its pipeline
    ///   (the first few calls on NVENC/QSV/AMF may produce no output).
    /// - More than one when a delayed encoder flushes multiple pending
    ///   access units at once.
    ///
    /// Each returned [`EncodedFrame`] is exactly one access unit with its
    /// own timestamp and keyframe flag — send each one as its own network
    /// frame with its own sequence number. Never concatenate them.
    fn encode(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>, CodecError>;

    fn quality(&self) -> Quality;

    /// Run rate control at `pct` percent of the preset bitrate (clamped
    /// to [`MIN_BITRATE_SCALE_PCT`]`..=100`). Resolution and frame rate
    /// stay as the preset says; only bits per frame change.
    ///
    /// Takes effect on the next [`VideoEncoder::encode`] call. The
    /// `FFmpeg` backend reopens its session for this, so the first frame
    /// at the new rate is an IDR with parameter sets — a recovery point
    /// for viewers who lost packets on the way down. Frames in flight
    /// inside a hardware encoder's pipeline are discarded without
    /// consuming sequence numbers.
    ///
    /// Backends that cannot change rate treat this as a no-op and keep
    /// reporting 100 from [`VideoEncoder::bitrate_scale_pct`].
    fn set_bitrate_scale_pct(&mut self, _pct: u32) {}

    /// The scale currently requested via
    /// [`VideoEncoder::set_bitrate_scale_pct`], in percent.
    fn bitrate_scale_pct(&self) -> u32 {
        100
    }

    /// Force the next encoded frame to be a keyframe (IDR).
    ///
    /// Used when a new viewer subscribes or a viewer reports an
    /// unrecoverable gap — they need an IDR with SPS/PPS parameter sets
    /// before they can decode any P-frames.
    fn force_keyframe(&mut self);

    /// Whether a forced keyframe is queued but not yet encoded.
    ///
    /// With change-driven capture backends, no frames flow while the
    /// screen is static — the capture loop uses this to know it must
    /// re-encode the held frame *now* (a viewer is waiting on an IDR)
    /// instead of waiting for the next refresh tick.
    fn keyframe_pending(&self) -> bool {
        false
    }

    /// Whether [`VideoEncoder::force_keyframe`] can actually produce an
    /// IDR on demand.
    ///
    /// `FFmpeg`'s AMF wrapper (through 7.1) ignores frame-level keyframe
    /// requests — IDRs only appear when the encoder's own period
    /// elapses, counted in *submitted frames*. When this is `false`,
    /// the capture loop must keep frames flowing (even on a static
    /// screen) after a keyframe request so that period actually passes.
    fn supports_forced_keyframes(&self) -> bool {
        true
    }

    /// The last frame came out too large to send (over
    /// `protocol::MAX_VIDEO_FRAME_BYTES`) and was dropped: raise the
    /// minimum quantizer the next session opens with, so the keyframe
    /// that replaces it is smaller. Returns the new floor, or `None` when
    /// the backend has none or the highest is already in force. The
    /// floor relaxes on its own once frames fit again.
    fn frame_too_large(&mut self) -> Option<u32> {
        None
    }

    /// The minimum quantizer [`Self::frame_too_large`] put in force.
    fn qp_floor(&self) -> Option<u32> {
        None
    }
}

/// Where one [`VideoDecoder::decode`] call that produced a picture
/// spent its time, for the viewer's log: the codec (handing it the
/// frame and waiting for a picture — on Android and macOS/iOS the
/// hardware) against getting the picture out of it (`output` — desktop:
/// the colour conversion to RGBA; Android: copying the YUV planes out,
/// the conversion happening later and only for pictures that will be
/// shown, see [`PictureFormat::Yuv420`]). A device that cannot keep up
/// is fixed in different places depending on which one is slow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeTiming {
    pub codec: std::time::Duration,
    pub output: std::time::Duration,
}

/// Decoder that turns an H.264 bitstream back into pictures (RGBA, or
/// YUV planes to convert — see [`PictureFormat`]).
pub trait VideoDecoder {
    /// Returns `Ok(None)` when the decoder is still buffering (needs more NALs).
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError>;

    /// Return a previously-decoded buffer for reuse. Every decoder takes
    /// its buffers from [`pool`], so [`pool::recycle`] does the same from
    /// anywhere — the app's present thread gives them back that way.
    fn return_buffer(&mut self, buf: Vec<u8>) {
        pool::recycle(buf);
    }

    /// Flush decoder state, clearing internal reference frame buffers.
    ///
    /// Call before decoding a keyframe that follows a known gap so stale
    /// reference state can't leak into the new GOP. (Resolution changes
    /// are handled automatically; flushing for those is not required.)
    fn flush(&mut self) {}

    /// The split of the last `decode` call that returned a picture
    /// (see [`DecodeTiming`]); `None` from backends that do both in
    /// one step.
    fn last_timing(&self) -> Option<DecodeTiming> {
        None
    }

    /// A picture finished (or held back) since the last `decode`, handed
    /// out once frames have stopped coming for a while — the sharer's
    /// screen went still — so the last picture before a pause does not
    /// wait for the next frame. The caller asks between frames, as often
    /// as it likes; `Ok(None)` from backends whose `decode` hands out
    /// every picture itself.
    ///
    /// # Errors
    /// As [`Self::decode`].
    fn poll(&mut self) -> Result<Option<DecodedFrame>, CodecError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod scaled_bitrate {
        use super::*;

        #[test]
        fn returns_preset_bitrate_at_100() {
            assert_eq!(Quality::FHD60.scaled_bitrate(100), 8_000_000);
        }

        #[test]
        fn scales_linearly() {
            assert_eq!(Quality::FHD60.scaled_bitrate(50), 4_000_000);
        }

        #[test]
        fn rounds_down_on_inexact_percent() {
            assert_eq!(Quality::HD30.scaled_bitrate(35), 1_225_000);
        }

        #[test]
        fn clamps_below_minimum_to_minimum() {
            assert_eq!(
                Quality::QHD30.scaled_bitrate(0),
                Quality::QHD30.scaled_bitrate(MIN_BITRATE_SCALE_PCT)
            );
        }

        #[test]
        fn clamps_above_100_to_preset() {
            assert_eq!(Quality::QHD30.scaled_bitrate(250), Quality::QHD30.bitrate());
        }
    }

    mod clamp_bitrate_scale_pct {
        use super::*;

        #[test]
        fn passes_values_inside_range() {
            assert_eq!(clamp_bitrate_scale_pct(70), 70);
        }

        #[test]
        fn raises_zero_to_minimum() {
            assert_eq!(clamp_bitrate_scale_pct(0), MIN_BITRATE_SCALE_PCT);
        }

        #[test]
        fn lowers_overshoot_to_100() {
            assert_eq!(clamp_bitrate_scale_pct(u32::MAX), 100);
        }
    }
}
