//! High-quality resampling with rubato.
//!
//! Handles device rate ↔ 48 kHz conversion for both input and output paths.
//! Users never see this — we accept whatever sample rate the device gives us.

use log::info;
use rubato::{
    Async, FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Build an async resampler with fixed input size for input (device rate → 48 kHz).
pub fn build_input_resampler(
    input_rate: u32,
    chunk_frames: usize,
) -> Result<Async<f32>, Box<dyn std::error::Error>> {
    let ratio = 48000.0 / input_rate as f64;

    let params = SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };

    let resampler = Async::<f32>::new_sinc(
        ratio,
        1.0, // max_resample_ratio_relative — no drift compensation needed
        &params,
        chunk_frames,
        1, // mono
        FixedAsync::Input,
    )?;

    info!(
        "Input resampler: {}Hz → 48kHz (ratio {:.4}), chunk={}",
        input_rate, ratio, chunk_frames,
    );

    Ok(resampler)
}

/// Build an async resampler with fixed output size for output (48 kHz → device rate).
pub fn build_output_resampler(
    device_rate: u32,
    chunk_frames: usize,
) -> Result<Async<f32>, Box<dyn std::error::Error>> {
    let ratio = device_rate as f64 / 48000.0;

    let params = SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };

    let resampler = Async::<f32>::new_sinc(
        ratio,
        1.0, // max_resample_ratio_relative
        &params,
        chunk_frames,
        1, // mono
        FixedAsync::Output,
    )?;

    info!(
        "Output resampler: 48kHz → {}Hz (ratio {:.4}), chunk={}",
        device_rate, ratio, chunk_frames,
    );

    Ok(resampler)
}

/// Frame count for a given duration at a sample rate.
pub fn frames_for_ms(rate: u32, ms: u32) -> usize {
    (rate as usize * ms as usize).div_ceil(1000)
}
