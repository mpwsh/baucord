//! High-quality resampling with rubato.
//!
//! Handles device rate ↔ 48 kHz conversion for both input and output paths.
//! Users never see this — we accept whatever sample rate the device gives us.
//!
//! [`ResamplerBridge`] wraps the accumulate → adapt → process boilerplate that
//! every resampling site needs, so the four pipelines (voice in/out, stream
//! in/out) share one implementation instead of four drifting copies.
//!
//! Note on drift: the resamplers run at a fixed ratio
//! (`max_resample_ratio_relative = 1.0`). Long-term clock drift between the
//! capture/playback devices and the 48 kHz pipeline is absorbed by the ring
//! fill targets and the jitter buffer's sustained-excess drain, not by ratio
//! adjustment. If drift ever becomes audible on marathon sessions, raising
//! `max_resample_ratio_relative` and steering the ratio from ring occupancy
//! is the intended upgrade path.

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use log::{error, info};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

fn sinc_params() -> SincInterpolationParameters {
    SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
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
        "Input resampler: {}Hz → 48kHz (ratio {:.4}), chunk={}, ch={}",
        input_rate, ratio, chunk_frames, channels,
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
        "Output resampler: 48kHz → {}Hz (ratio {:.4}), chunk={}, ch={}",
        device_rate, ratio, chunk_frames, channels,
    );

    Ok(resampler)
}

/// Frame count for a given duration at a sample rate.
pub fn frames_for_ms(rate: u32, ms: u32) -> usize {
    (rate as usize * ms as usize).div_ceil(1000)
}

/// Accumulating adapter around a rubato [`Async`] resampler.
///
/// Callers feed interleaved samples in whatever sizes the device delivers;
/// the bridge buffers them, runs the resampler whenever a full chunk is
/// available, and hands planar output to a sink closure. All scratch
/// buffers are allocated once at construction.
// NOTE: not yet wired into the four resample sites (voice in/out, stream
// in/out) — they still carry their original inline loops. This type is the
// intended consolidation target; migrating the call sites is mechanical.
#[allow(dead_code)]
pub struct ResamplerBridge {
    resampler: Async<f32>,
    channels: usize,
    /// Per-channel input samples waiting for a full chunk.
    pending: Vec<Vec<f32>>,
    /// Per-channel scratch fed to rubato each process call.
    input: Vec<Vec<f32>>,
    /// Per-channel rubato output.
    output: Vec<Vec<f32>>,
}

#[allow(dead_code)]
impl ResamplerBridge {
    /// Wrap an already-built resampler.
    pub fn new(resampler: Async<f32>, channels: usize) -> Self {
        let out_max = resampler.output_frames_max();
        Self {
            channels,
            pending: vec![Vec::with_capacity(8192); channels],
            input: vec![Vec::with_capacity(resampler.input_frames_max()); channels],
            output: vec![vec![0.0f32; out_max]; channels],
            resampler,
        }
    }

    /// Device rate → 48 kHz (fixed input chunk).
    pub fn to_48k(
        device_rate: u32,
        chunk_frames: usize,
        channels: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::new(
            build_input_resampler(device_rate, chunk_frames, channels)?,
            channels,
        ))
    }

    /// 48 kHz → device rate (fixed output chunk).
    pub fn from_48k(
        device_rate: u32,
        chunk_frames: usize,
        channels: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::new(
            build_output_resampler(device_rate, chunk_frames, channels)?,
            channels,
        ))
    }

    /// Feed interleaved samples (mono: just samples; stereo: L R L R …).
    ///
    /// Trailing samples that don't form a complete frame are dropped
    /// (callers on stereo paths should only pass whole frames).
    pub fn push_interleaved(&mut self, samples: &[f32]) {
        match self.channels {
            1 => self.pending[0].extend_from_slice(samples),
            2 => {
                for pair in samples.chunks_exact(2) {
                    self.pending[0].push(pair[0]);
                    self.pending[1].push(pair[1]);
                }
            }
            n => {
                for frame in samples.chunks_exact(n) {
                    for (ch, &s) in frame.iter().enumerate() {
                        self.pending[ch].push(s);
                    }
                }
            }
        }
    }

    /// Run the resampler over all complete pending chunks.
    ///
    /// `sink` is called once per chunk with `(planar_output, frames)`;
    /// channel `c`'s samples are `planar_output[c][..frames]`.
    pub fn process(&mut self, mut sink: impl FnMut(&[Vec<f32>], usize)) {
        loop {
            let needed = self.resampler.input_frames_next();
            if self.pending.iter().any(|c| c.len() < needed) {
                return;
            }

            for ch in 0..self.channels {
                self.input[ch].clear();
                self.input[ch].extend_from_slice(&self.pending[ch][..needed]);
                self.pending[ch].drain(..needed);
            }

            let expected = self.resampler.output_frames_next();
            for ch in 0..self.channels {
                self.output[ch].resize(expected, 0.0);
            }

            let in_adapter = match SequentialSliceOfVecs::new(&self.input, self.channels, needed) {
                Ok(a) => a,
                Err(e) => {
                    error!("Resampler input adapter error: {e}");
                    return;
                }
            };
            let mut out_adapter =
                match SequentialSliceOfVecs::new_mut(&mut self.output, self.channels, expected) {
                    Ok(a) => a,
                    Err(e) => {
                        error!("Resampler output adapter error: {e}");
                        return;
                    }
                };

            match self
                .resampler
                .process_into_buffer(&in_adapter, &mut out_adapter, None)
            {
                Ok((_in_len, out_len)) if out_len > 0 => sink(&self.output, out_len),
                Ok(_) => {}
                Err(e) => {
                    error!("Resampler error: {e}");
                    return;
                }
            }
        }
    }

    /// Discard all pending input (used on mute / reset).
    pub fn clear(&mut self) {
        for c in &mut self.pending {
            c.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audioadapter_buffers::direct::SequentialSliceOfVecs;
    use rubato::Resampler;

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
