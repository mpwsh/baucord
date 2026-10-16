//! The encoder's stand-in for platforms without an FFmpeg build (iOS /
//! Android).
//!
//! Same type name and constructor signature as the real [`codec`
//! module](crate::codec)'s; construction always fails with a clear
//! [`CodecError`], which the client's `create_encoder` factory already
//! converts into a UI-visible error. There is no decoder here: Android
//! decodes with `mediacodec.rs`, iOS with `videotoolbox.rs` (the
//! client's `create_decoder` picks them).

use crate::{CodecError, EncodedFrame, Frame, Quality, VideoEncoder};

const UNSUPPORTED: &str = "no video codec backend on this platform";

/// Stub encoder — construction always fails.
pub struct FfmpegEncoder {
    _private: (),
}

impl FfmpegEncoder {
    pub fn new(_quality: Quality) -> Result<Self, CodecError> {
        Err(CodecError::EncoderInit(UNSUPPORTED.into()))
    }

    pub fn new_without(_quality: Quality, _skip: &[&str]) -> Result<Self, CodecError> {
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

    fn force_keyframe(&mut self) {
        unreachable!("stub FfmpegEncoder cannot be constructed")
    }
}
