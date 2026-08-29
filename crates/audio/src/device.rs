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
use cpal::{
    BufferSize, Device, SampleFormat, StreamConfig, SupportedStreamConfig,
    SupportedStreamConfigRange,
};
use log::{debug, info};

use crate::OPUS_SAMPLE_RATE;

/// Capture buffer we ask the OS for, in milliseconds.
///
/// Capture buffer *capacity* is not latency: the engine's packets are read
/// the moment they land regardless of how much room is behind them. It is
/// tolerance — how late the reader may be before the engine overwrites
/// unread audio and flags a discontinuity (a gap in the sender's voice).
/// WASAPI's shared-mode default is roughly two engine periods (~20 ms),
/// which a foreground game blows through routinely. 50 ms covers the
/// scheduling stalls seen in the field with no cost in the good case.
const CAPTURE_BUFFER_MS: u32 = 50;

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

/// Buffer size to request for a capture stream on `device` with `cfg`.
///
/// Windows: `CAPTURE_BUFFER_MS`, but only if a throwaway stream opens
/// with it — cpal feeds the duration straight into `Initialize` with no
/// retry, and a refused duration used to mean total silence in the field.
/// Elsewhere: the existing ~10 ms low-latency request (CoreAudio and ALSA
/// report honest ranges and their capture paths are not the ones starving).
pub fn capture_buffer_size(device: &Device, cfg: &SupportedStreamConfig) -> BufferSize {
    if cfg!(windows) {
        let frames = cfg.sample_rate() * CAPTURE_BUFFER_MS / 1000;
        let wanted = BufferSize::Fixed(frames);
        if probe_input_with(device, cfg, wanted) {
            info!("Capture buffer: {CAPTURE_BUFFER_MS} ms requested and accepted");
            return wanted;
        }
        debug!("Capture buffer: {CAPTURE_BUFFER_MS} ms refused; using the backend default");
        return BufferSize::Default;
    }
    crate::platform::request_low_latency_buffer(cfg.buffer_size(), cfg.sample_rate())
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
    probe_input_with(device, cfg, BufferSize::Default)
}

fn probe_input_with(device: &Device, cfg: &SupportedStreamConfig, buffer: BufferSize) -> bool {
    let mut stream_cfg = cfg.config();
    stream_cfg.buffer_size = buffer;
    let result = match cfg.sample_format() {
        SampleFormat::F32 => probe_input_as::<f32>(device, &stream_cfg),
        SampleFormat::I16 => probe_input_as::<i16>(device, &stream_cfg),
        SampleFormat::U16 => probe_input_as::<u16>(device, &stream_cfg),
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("probe: unsupported sample format {other:?}"),
        )),
    };
    match result {
        Ok(()) => true,
        Err(e) => {
            debug!("input config probe refused ({e})");
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
    cfg: &StreamConfig,
) -> Result<(), cpal::Error> {
    let stream = device.build_input_stream(cfg.clone(), |_: &[T], _| {}, |_| {}, None)?;
    drop(stream);
    Ok(())
}
