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

// The real codec module is FFmpeg top to bottom, so it's swapped
// wholesale on targets without an FFmpeg build. The stub exposes the
// same types with constructors that fail cleanly — client code compiles
// unchanged and its existing error paths handle the rest.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
pub mod codec;
#[cfg(any(target_os = "ios", target_os = "android"))]
#[path = "codec_stub.rs"]
pub mod codec;

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
    pub const fn width(self) -> u32 {
        match self {
            Self::HD30 | Self::HD60 => 1280,
            Self::FHD30 | Self::FHD60 => 1920,
            Self::QHD30 => 2560,
        }
    }

    /// Maximum encoded height. The actual encoded height may be smaller
    /// to preserve the source aspect ratio.
    pub const fn height(self) -> u32 {
        match self {
            Self::HD30 | Self::HD60 => 720,
            Self::FHD30 | Self::FHD60 => 1080,
            Self::QHD30 => 1440,
        }
    }

    pub const fn fps(self) -> u32 {
        match self {
            Self::HD30 | Self::FHD30 | Self::QHD30 => 30,
            Self::HD60 | Self::FHD60 => 60,
        }
    }

    /// Target bitrate in bits/sec.
    ///
    /// Tuned for **screen content** (sharp text, hard edges, flat colors).
    /// Screen content needs ~1.5–2× more bitrate than camera video at the
    /// same resolution because DCT-based codecs spend more bits preserving
    /// high-frequency detail (glyphs, 1px borders, UI elements).
    ///
    /// This is the *actual* encoder target (ABR + VBV), not a hint.
    pub const fn bitrate(self) -> u32 {
        match self {
            Self::HD30 => 3_500_000,
            Self::HD60 => 4_500_000,
            Self::FHD30 => 6_000_000,
            Self::FHD60 => 8_000_000,
            Self::QHD30 => 10_000_000,
        }
    }

    /// Duration of one frame.
    pub const fn frame_duration(self) -> std::time::Duration {
        std::time::Duration::from_nanos(1_000_000_000 / self.fps() as u64)
    }

    /// Pixel count at the maximum resolution for this preset.
    pub const fn pixel_count(self) -> usize {
        self.width() as usize * self.height() as usize
    }

    /// Byte count for one RGBA frame at the maximum resolution.
    pub const fn rgba_size(self) -> usize {
        self.pixel_count() * 4
    }

    /// Byte count for one YUV420 frame at the maximum resolution.
    pub const fn yuv420_size(self) -> usize {
        self.pixel_count() * 3 / 2
    }

    pub const ALL: &[Quality] = &[
        Self::HD30,
        Self::HD60,
        Self::FHD30,
        Self::FHD60,
        Self::QHD30,
    ];
}

// ---------------------------------------------------------------------------
// Frame types
// ---------------------------------------------------------------------------

/// An RGBA frame ready for encoding or display.
///
/// Not `Clone` by design — RGBA buffers are large (3.7 MB at 1080p).
/// Pass by reference or transfer ownership explicitly.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Capture timestamp in milliseconds (monotonic, capturer-relative).
    /// Used as the encoder PTS, so it also drives keyframe cadence and
    /// rate control under variable capture rates.
    pub timestamp_ms: u64,
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

/// Decoded RGBA frame ready for display.
///
/// Not `Clone` by design — same reasoning as [`Frame`].
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp_ms: u64,
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

    /// Force the next encoded frame to be a keyframe (IDR).
    ///
    /// Used when a new viewer subscribes or a viewer reports an
    /// unrecoverable gap — they need an IDR with SPS/PPS parameter sets
    /// before they can decode any P-frames.
    fn force_keyframe(&mut self);
}

/// Decoder that converts H.264 bitstream back into RGBA frames.
pub trait VideoDecoder {
    /// Returns `Ok(None)` when the decoder is still buffering (needs more NALs).
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError>;

    /// Return a previously-decoded RGBA buffer for reuse.
    ///
    /// Avoids allocating a new buffer on the next `decode()` call.
    /// Optional — callers that don't recycle buffers can ignore this.
    fn return_buffer(&mut self, _buf: Vec<u8>) {}

    /// Flush decoder state, clearing internal reference frame buffers.
    ///
    /// Call before decoding a keyframe that follows a known gap so stale
    /// reference state can't leak into the new GOP. (Resolution changes
    /// are handled automatically; flushing for those is not required.)
    fn flush(&mut self) {}
}
