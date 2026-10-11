//! High-quality resampling with rubato.
//!
//! Handles device rate ↔ 48 kHz conversion for both input and output paths.
//! Since [`crate::device`] asks every device for 48 kHz first, this is the
//! exception path (Bluetooth HFP, fixed-rate interfaces, a 44.1 kHz
//! Windows mixer), not the default one.
//!
//! ## Why sinc, not FFT
//!
//! rubato's `Fft` resamplers are cheaper per sample at a fixed ratio, but
//! they buffer a whole chunk and add roughly that much group delay — 10 ms
//! or more on each side of a call. The sinc `Async` resampler's delay is
//! `sinc_len / 2` samples (0.7 ms). For a latency-first voice path the
//! extra MACs are the right trade; the resampler is rare here anyway.
//!
//! ## Drift
//!
//! The resamplers run at a fixed ratio (`max_resample_ratio_relative =
//! 1.0`) — deliberately. Sender/receiver clock skew is corrected at the
//! receiver by the jitter buffer's per-peer rate compensation, which
//! sees the *combined* skew of both ends; steering these ratios would
//! add sinc group delay for no benefit.

use log::info;
use rubato::{
    Async, FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

pub(crate) fn sinc_params() -> SincInterpolationParameters {
    SincInterpolationParameters {
        sinc_len: 64,
        // rubato 4: Option — None would auto-derive the highest
        // anti-aliasing-safe cutoff from sinc_len + window. Kept at the
        // hand-picked value to preserve the existing frequency response.
        f_cutoff: Some(0.95),
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    }
}

/// Build an async resampler with fixed input size for input (device rate → 48 kHz).
pub fn build_input_resampler(
    input_rate: u32,
    chunk_frames: usize,
    channels: usize,
) -> Result<Async<f32>, Box<dyn std::error::Error>> {
    let ratio = 48000.0 / f64::from(input_rate);

    let resampler = Async::<f32>::new_sinc(
        ratio,
        1.0, // max_resample_ratio_relative — fixed ratio, see module docs
        &sinc_params(),
        chunk_frames,
        channels,
        FixedAsync::Input,
    )?;

    info!(
        "Input resampler: {input_rate}Hz → 48kHz (ratio {ratio:.4}), chunk={chunk_frames}, ch={channels}",
    );

    Ok(resampler)
}

/// Build an async resampler with fixed output size for output (48 kHz → device rate).
pub fn build_output_resampler(
    device_rate: u32,
    chunk_frames: usize,
    channels: usize,
) -> Result<Async<f32>, Box<dyn std::error::Error>> {
    let ratio = f64::from(device_rate) / 48000.0;

    let resampler = Async::<f32>::new_sinc(
        ratio,
        1.0, // max_resample_ratio_relative — fixed ratio, see module docs
        &sinc_params(),
        chunk_frames,
        channels,
        FixedAsync::Output,
    )?;

    info!(
        "Output resampler: 48kHz → {device_rate}Hz (ratio {ratio:.4}), chunk={chunk_frames}, ch={channels}",
    );

    Ok(resampler)
}

/// Frame count for a given duration at a sample rate.
pub fn frames_for_ms(rate: u32, ms: u32) -> usize {
    (rate as usize * ms as usize).div_ceil(1000)
}

#[cfg(test)]
mod tests {
    use audioadapter_buffers::direct::SequentialSliceOfVecs;
    use rubato::Resampler;

    use super::*;

    /// Replicates the input.rs inline loop at a 44.1 kHz device rate —
    /// the path a typical Windows shared-mode device exercises and a
    /// 48 kHz Mac never does.
    #[test]
    fn input_path_44100_produces_48k_audio() {
        let chunk = frames_for_ms(44100, 10); // 441
        let mut r = build_input_resampler(44100, chunk, 1).expect("build");
        let mut accum: Vec<f32> = Vec::new();
        let mut input = vec![Vec::with_capacity(r.input_frames_next())];
        let mut output = vec![vec![0.0f32; r.output_frames_max()]];
        let mut produced = 0usize;
        let mut energy = 0.0f32;

        // 1 second of 440 Hz sine at 44.1k, delivered in 512-sample callbacks
        let src: Vec<f32> = (0..44100)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 44100.0).sin() * 0.5)
            .collect();
        for cb in src.chunks(512) {
            accum.extend_from_slice(cb);
            while accum.len() >= r.input_frames_next() {
                let n = r.input_frames_next();
                input[0].clear();
                input[0].extend_from_slice(&accum[..n]);
                accum.drain(..n);
                let expected = r.output_frames_next();
                output[0].resize(expected, 0.0);
                let ia = SequentialSliceOfVecs::new(&input, 1, n).unwrap();
                let mut oa = SequentialSliceOfVecs::new_mut(&mut output, 1, expected).unwrap();
                let (_, out_len) = r.process_into_buffer(&ia, &mut oa, None).expect("process");
                produced += out_len;
                energy += output[0][..out_len].iter().map(|s| s * s).sum::<f32>();
            }
        }
        // ~48000 samples out (minus filter startup), with real signal energy
        assert!(produced > 45000, "only {produced} samples at 48k");
        assert!(energy / produced as f32 > 0.01, "output is near-silent");
    }

    /// Replicates the output.rs inline loop: 48 kHz hops → 44.1 kHz device.
    #[test]
    fn output_path_48k_to_44100_produces_audio() {
        let chunk = frames_for_ms(44100, 10); // 441 — device-rate chunk
        let mut r = build_output_resampler(44100, chunk, 1).expect("build");
        let mut accum: Vec<f32> = Vec::new();
        let mut input = vec![Vec::with_capacity(r.input_frames_next() + 64)];
        let mut output = vec![vec![0.0f32; chunk * 2]];
        let mut produced = 0usize;
        let mut energy = 0.0f32;

        let hop: Vec<f32> = (0..480)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.5)
            .collect();
        for _ in 0..100 {
            // 1 second of 10 ms hops
            accum.extend_from_slice(&hop);
            while accum.len() >= r.input_frames_next() {
                let n = r.input_frames_next();
                input[0].clear();
                input[0].extend_from_slice(&accum[..n]);
                accum.drain(..n);
                let expected = r.output_frames_next();
                output[0].resize(expected, 0.0);
                let ia = SequentialSliceOfVecs::new(&input, 1, n).unwrap();
                let mut oa = SequentialSliceOfVecs::new_mut(&mut output, 1, expected).unwrap();
                let (_, out_len) = r.process_into_buffer(&ia, &mut oa, None).expect("process");
                produced += out_len;
                energy += output[0][..out_len].iter().map(|s| s * s).sum::<f32>();
            }
        }
        assert!(produced > 41000, "only {produced} samples at 44.1k");
        assert!(energy / produced as f32 > 0.01, "output is near-silent");
    }
}
