//! Codec stubs for platforms without an FFmpeg build (iOS / Android).
//!
//! Same type names and constructor signatures as the real [`codec`
//! module](crate::codec); construction always fails with a clear
//! [`CodecError`], which the client's `create_encoder` /
//! `create_decoder` factories already convert into UI-visible errors
//! (and, for incoming streams, a one-time warning + dropped frames).
//!
//! A future mobile *viewer* replaces `FfmpegDecoder` here with a
//! VideoToolbox / MediaCodec implementation of [`VideoDecoder`] — the
//! trait, `Quality`, and the jitter buffer already compile on mobile.

use crate::{
    CodecError, DecodedFrame, EncodedFrame, Frame, Quality, VideoDecoder, VideoEncoder,
};

const UNSUPPORTED: &str = "no video codec backend on this platform";

/// Stub encoder — construction always fails.
pub struct FfmpegEncoder {
    _private: (),
}

impl FfmpegEncoder {
    pub fn new(_quality: Quality) -> Result<Self, CodecError> {
        Err(CodecError::EncoderInit(UNSUPPORTED.into()))
    }

    pub fn encoder_name(&self) -> &'static str {
        unreachable!("stub FfmpegEncoder cannot be constructed")
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn encode(&mut self, _frame: &Frame) -> Result<Vec<EncodedFrame>, CodecError> {
        unreachable!("stub FfmpegEncoder cannot be constructed")
    }

    fn quality(&self) -> Quality {
        unreachable!("stub FfmpegEncoder cannot be constructed")
    }

    fn force_keyframe(&mut self) {
        unreachable!("stub FfmpegEncoder cannot be constructed")
    }
}

/// Stub decoder — construction always fails.
pub struct FfmpegDecoder {
    _private: (),
}

impl FfmpegDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Err(CodecError::DecoderInit(UNSUPPORTED.into()))
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, _encoded: &EncodedFrame) -> Result<Option<DecodedFrame>, CodecError> {
        unreachable!("stub FfmpegDecoder cannot be constructed")
    }
}
