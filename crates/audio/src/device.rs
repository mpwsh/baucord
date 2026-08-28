//! Stream-config selection: ask for 48 kHz before settling for the default.
//!
//! The pipeline runs at Opus' native 48 kHz. Opening the device at any
//! other rate costs a sinc resampler on that path — ~64 MACs per sample
//! per channel, ~0.7 ms of group delay, and a second buffer stage. Most
//! devices *can* run at 48 kHz; they just are not always set to it. So:
//!
//! 1. If the OS default config is already 48 kHz, use it (no rate switch).
//! 2. Otherwise look for a supported range that contains 48 kHz, preferring
//!    the default's channel count and sample format so nothing else about
//!    the stream changes.
//! 3. Otherwise keep the default and let the caller resample.
//!
//! Platform notes: WASAPI shared mode only advertises the mixer's own
//! format, so a 44.1 kHz Windows mixer still resamples (changing that is a
//! user setting, not ours). CoreAudio honours the request by switching the
//! device's nominal rate — the same thing every other VoIP client does.

use cpal::traits::DeviceTrait;
use cpal::{Device, SupportedStreamConfig, SupportedStreamConfigRange};

use crate::OPUS_SAMPLE_RATE;

/// Input config for `device`, at 48 kHz when the device supports it.
pub fn preferred_input_config(
    device: &Device,
) -> Result<SupportedStreamConfig, cpal::Error> {
    let default = device.default_input_config()?;
    if default.sample_rate() == OPUS_SAMPLE_RATE {
        return Ok(default);
    }
    let Ok(ranges) = device.supported_input_configs() else {
        return Ok(default);
    };
    Ok(pick_48k(ranges, &default).unwrap_or(default))
}

/// Output config for `device`, at 48 kHz when the device supports it.
pub fn preferred_output_config(
    device: &Device,
) -> Result<SupportedStreamConfig, cpal::Error> {
    let default = device.default_output_config()?;
    if default.sample_rate() == OPUS_SAMPLE_RATE {
        return Ok(default);
    }
    let Ok(ranges) = device.supported_output_configs() else {
        return Ok(default);
    };
    Ok(pick_48k(ranges, &default).unwrap_or(default))
}

/// Best 48 kHz-capable range: same channels + format as the default wins,
/// then same channels, then anything that can do 48 kHz.
fn pick_48k(
    ranges: impl Iterator<Item = SupportedStreamConfigRange>,
    default: &SupportedStreamConfig,
) -> Option<SupportedStreamConfig> {
    let mut best: Option<(u8, SupportedStreamConfigRange)> = None;
    for range in ranges {
        if !range.contains_rate(OPUS_SAMPLE_RATE) {
            continue;
        }
        let score = u8::from(range.channels() == default.channels())
            + u8::from(range.sample_format() == default.sample_format());
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, range));
        }
    }
    best.and_then(|(_, range)| range.try_with_sample_rate(OPUS_SAMPLE_RATE))
}
