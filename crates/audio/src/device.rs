//! Stream-config selection: ask for 48 kHz before settling for the default,
//! and prove the choice opens before committing to it.
//!
//! The pipeline runs at Opus' native 48 kHz. Opening the device at any
//! other rate costs a sinc resampler on that path — ~64 MACs per sample
//! per channel, ~0.7 ms of group delay, and a second buffer stage. Most
//! devices *can* run at 48 kHz; they just are not always set to it. So:
//!
//! 1. If the OS default config is already 48 kHz, use it (no rate switch).
//! 2. Otherwise look for a supported range that contains 48 kHz, preferring
//!    the default's channel count and sample format so nothing else about
//!    the stream changes — **and open a throwaway stream with it**. If the
//!    open fails, keep the default and let the caller resample.
//!
//! The probe exists because "advertised" and "openable" are not the same
//! thing: cpal 0.18.0/0.18.1 on WASAPI list every common output rate but
//! refuse to open anything except the mixer's own rate ("Stream
//! configuration is not supported in shared mode"). 0.18.2 fixed that,
//! and `Cargo.toml` requires it, but the probe is cheap insurance against
//! the next backend or driver that lies. A probe stream is never played;
//! it costs one extra device open at startup.
//!
//! Platform notes: CoreAudio honours a 48 kHz request by switching the
//! device's nominal rate — the same thing every other VoIP client does.
//! WASAPI shared mode (cpal ≥ 0.18.2) converts inside the audio engine
//! instead, so on a 44.1 kHz Windows mixer the OS does the resampling and
//! this crate's resampler stays idle either way.

use cpal::traits::DeviceTrait;
use cpal::{Device, SampleFormat, SupportedStreamConfig, SupportedStreamConfigRange};
use log::{debug, info};

use crate::OPUS_SAMPLE_RATE;

/// Input config for `device`, at 48 kHz when the device supports it.
pub fn preferred_input_config(device: &Device) -> Result<SupportedStreamConfig, cpal::Error> {
    let default = device.default_input_config()?;
    if default.sample_rate() == OPUS_SAMPLE_RATE {
        return Ok(default);
    }
    let Ok(ranges) = device.supported_input_configs() else {
        return Ok(default);
    };
    let Some(candidate) = pick_48k(ranges, &default) else {
        return Ok(default);
    };
    Ok(if probe_input(device, &candidate) {
        info!(
            "Input device opened at 48 kHz (OS default was {} Hz)",
            default.sample_rate()
        );
        candidate
    } else {
        default
    })
}

/// Output config for `device`, at 48 kHz when the device supports it.
pub fn preferred_output_config(device: &Device) -> Result<SupportedStreamConfig, cpal::Error> {
    let default = device.default_output_config()?;
    if default.sample_rate() == OPUS_SAMPLE_RATE {
        return Ok(default);
    }
    let Ok(ranges) = device.supported_output_configs() else {
        return Ok(default);
    };
    let Some(candidate) = pick_48k(ranges, &default) else {
        return Ok(default);
    };
    Ok(if probe_output(device, &candidate) {
        info!(
            "Output device opened at 48 kHz (OS default was {} Hz)",
            default.sample_rate()
        );
        candidate
    } else {
        default
    })
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

// ── Probes: open, never play, drop ──────────────────────────────────────

fn probe_output(device: &Device, cfg: &SupportedStreamConfig) -> bool {
    let result = match cfg.sample_format() {
        SampleFormat::F32 => probe_output_as::<f32>(device, cfg),
        SampleFormat::I16 => probe_output_as::<i16>(device, cfg),
        SampleFormat::U16 => probe_output_as::<u16>(device, cfg),
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("probe: unsupported sample format {other:?}"),
        )),
    };
    match result {
        Ok(()) => true,
        Err(e) => {
            debug!("48 kHz output config advertised but refused ({e}); keeping the OS default");
            false
        }
    }
}

fn probe_input(device: &Device, cfg: &SupportedStreamConfig) -> bool {
    let result = match cfg.sample_format() {
        SampleFormat::F32 => probe_input_as::<f32>(device, cfg),
        SampleFormat::I16 => probe_input_as::<i16>(device, cfg),
        SampleFormat::U16 => probe_input_as::<u16>(device, cfg),
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("probe: unsupported sample format {other:?}"),
        )),
    };
    match result {
        Ok(()) => true,
        Err(e) => {
            debug!("48 kHz input config advertised but refused ({e}); keeping the OS default");
            false
        }
    }
}

fn probe_output_as<T: cpal::SizedSample>(
    device: &Device,
    cfg: &SupportedStreamConfig,
) -> Result<(), cpal::Error> {
    let stream = device.build_output_stream(cfg.config(), |_: &mut [T], _| {}, |_| {}, None)?;
    drop(stream);
    Ok(())
}

fn probe_input_as<T: cpal::SizedSample>(
    device: &Device,
    cfg: &SupportedStreamConfig,
) -> Result<(), cpal::Error> {
    let stream = device.build_input_stream(cfg.config(), |_: &[T], _| {}, |_| {}, None)?;
    drop(stream);
    Ok(())
}
