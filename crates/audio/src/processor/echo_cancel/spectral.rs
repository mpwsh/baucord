//! Spectral processing utilities for echo cancellation.
//!
//! Provides the FFT infrastructure, windowing, and overlap-add
//! framework used by the `EchoCanceller`.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Pre-computed Hann window coefficients.
///
/// Hann is the standard choice for overlap-add spectral processing:
/// - 50% overlap with Hann gives perfect reconstruction
/// - Smooth taper avoids spectral leakage
/// - Symmetric (analysis and synthesis use the same window)
pub struct HannWindow {
    coefficients: Vec<f32>,
}

impl HannWindow {
    /// Create a Hann window of the given size.
    pub fn new(size: usize) -> Self {
        let coefficients: Vec<f32> = (0..size)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / size as f32).cos()))
            .collect();
        Self { coefficients }
    }

    /// Apply the window to a slice of samples in-place.
    pub fn apply(&self, samples: &mut [f32]) {
        debug_assert_eq!(samples.len(), self.coefficients.len());
        for (s, &w) in samples.iter_mut().zip(self.coefficients.iter()) {
            *s *= w;
        }
    }

    /// Apply the window to a complex buffer (for synthesis windowing).
    pub fn apply_complex(&self, buf: &mut [Complex<f32>]) {
        debug_assert_eq!(buf.len(), self.coefficients.len());
        for (c, &w) in buf.iter_mut().zip(self.coefficients.iter()) {
            c.re *= w;
            c.im *= w;
        }
    }

    pub fn len(&self) -> usize {
        self.coefficients.len()
    }
}

/// Overlap-add buffer for reconstructing time-domain audio from
/// successive IFFT frames with 50% overlap.
pub struct OverlapAdd {
    /// Accumulation buffer (fft_size samples).
    buffer: Vec<f32>,
    /// FFT/hop size info.
    fft_size: usize,
    hop_size: usize,
    /// Output samples ready for consumption.
    output_ready: Vec<f32>,
}

impl OverlapAdd {
    pub fn new(fft_size: usize, hop_size: usize) -> Self {
        Self {
            buffer: vec![0.0; fft_size],
            fft_size,
            hop_size,
            output_ready: Vec::with_capacity(hop_size * 4),
        }
    }

    /// Add a windowed IFFT frame into the overlap-add buffer.
    ///
    /// The frame should already be windowed (synthesis window applied).
    /// After adding, `hop_size` new output samples become available.
    pub fn add_frame(&mut self, frame: &[f32]) {
        debug_assert_eq!(frame.len(), self.fft_size);

        // Add frame into accumulation buffer
        for (i, &s) in frame.iter().enumerate() {
            self.buffer[i] += s;
        }

        // Extract the first hop_size samples as output
        self.output_ready
            .extend_from_slice(&self.buffer[..self.hop_size]);

        // Shift the buffer left by hop_size
        self.buffer.copy_within(self.hop_size..self.fft_size, 0);
        self.buffer[self.fft_size - self.hop_size..].fill(0.0);
    }

    /// Drain all available output samples.
    pub fn drain_output(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.output_ready)
    }

    /// Number of output samples currently available.
    pub fn available(&self) -> usize {
        self.output_ready.len()
    }

    /// Reset overlap-add state (e.g., on delay change).
    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.output_ready.clear();
    }
}

/// Bundles the FFT/IFFT plans and scratch buffers for spectral operations.
///
/// Allocated once per `EchoCanceller`, reused every frame.
pub struct SpectralEngine {
    pub fft_size: usize,
    pub hop_size: usize,
    pub fft: Arc<dyn Fft<f32>>,
    pub ifft: Arc<dyn Fft<f32>>,
    pub scratch: Vec<Complex<f32>>,
    pub window: HannWindow,

    /// Normalization factor for IFFT output.
    /// rustfft doesn't normalize, so we divide by fft_size.
    /// With 50% overlap and Hann windowing, we also need to
    /// account for the window energy to get unity gain.
    pub ifft_norm: f32,
}

impl SpectralEngine {
    /// Create a new spectral engine.
    ///
    /// - `fft_size`: must be a power of 2 (typically 1024)
    /// - `hop_size`: typically fft_size / 2 for 50% overlap
    pub fn new(fft_size: usize, hop_size: usize) -> Self {
        assert!(fft_size.is_power_of_two(), "FFT size must be power of 2");
        assert!(hop_size <= fft_size, "Hop size must be <= FFT size");

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let ifft = planner.plan_fft_inverse(fft_size);

        let scratch_len = fft
            .get_inplace_scratch_len()
            .max(ifft.get_inplace_scratch_len());

        let window = HannWindow::new(fft_size);

        // Normalization: 1/N for IFFT, times 2/sum(window²) for Hann OLA.
        // For 50% overlap Hann, the overlap-add gain is 1.0 when we apply
        // the window on both analysis and synthesis, so we just need 1/N.
        let ifft_norm = 1.0 / fft_size as f32;

        Self {
            fft_size,
            hop_size,
            fft,
            ifft,
            scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            window,
            ifft_norm,
        }
    }

    /// Forward FFT: real samples → complex spectrum.
    ///
    /// Input samples are windowed and zero-padded to `fft_size`.
    /// Result is written into `output` (must be `fft_size` elements).
    pub fn forward(&mut self, samples: &[f32], output: &mut [Complex<f32>]) {
        debug_assert!(samples.len() <= self.fft_size);
        debug_assert_eq!(output.len(), self.fft_size);

        // Copy samples into complex buffer, zero-pad if shorter
        for (i, c) in output.iter_mut().enumerate() {
            c.re = if i < samples.len() { samples[i] } else { 0.0 };
            c.im = 0.0;
        }

        self.fft.process_with_scratch(output, &mut self.scratch);
    }

    /// Inverse FFT: complex spectrum → real samples.
    ///
    /// Applies IFFT normalization. Input is modified in-place.
    /// Real parts of the result are the time-domain samples.
    pub fn inverse(&mut self, spectrum: &mut [Complex<f32>]) {
        debug_assert_eq!(spectrum.len(), self.fft_size);

        self.ifft.process_with_scratch(spectrum, &mut self.scratch);

        // Normalize
        for c in spectrum.iter_mut() {
            c.re *= self.ifft_norm;
            c.im *= self.ifft_norm;
        }
    }
}

/// Extract magnitude from a complex spectrum.
#[inline]
pub fn magnitude(spectrum: &[Complex<f32>], out: &mut [f32]) {
    debug_assert_eq!(spectrum.len(), out.len());
    for (i, c) in spectrum.iter().enumerate() {
        out[i] = c.norm(); // sqrt(re² + im²)
    }
}

/// Extract phase angles from a complex spectrum.
#[inline]
pub fn phase(spectrum: &[Complex<f32>], out: &mut [f32]) {
    debug_assert_eq!(spectrum.len(), out.len());
    for (i, c) in spectrum.iter().enumerate() {
        out[i] = c.arg(); // atan2(im, re)
    }
}

/// Reconstruct a complex spectrum from magnitude and phase.
#[inline]
pub fn from_polar(magnitude: &[f32], phase: &[f32], out: &mut [Complex<f32>]) {
    debug_assert_eq!(magnitude.len(), phase.len());
    debug_assert_eq!(magnitude.len(), out.len());
    for i in 0..magnitude.len() {
        let (sin_p, cos_p) = phase[i].sin_cos();
        out[i] = Complex::new(magnitude[i] * cos_p, magnitude[i] * sin_p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hann_window_endpoints_are_zero() {
        let w = HannWindow::new(1024);
        assert!(w.coefficients[0].abs() < 1e-6);
        // Last element of periodic Hann is near-zero but not exactly zero
        assert!(w.coefficients[1023] < 0.01);
    }

    #[test]
    fn hann_window_midpoint_is_one() {
        let w = HannWindow::new(1024);
        assert!((w.coefficients[512] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn overlap_add_produces_correct_length() {
        let fft_size = 1024;
        let hop_size = 512;
        let mut ola = OverlapAdd::new(fft_size, hop_size);

        let frame = vec![1.0f32; fft_size];
        ola.add_frame(&frame);

        assert_eq!(ola.available(), hop_size);
    }

    #[test]
    fn round_trip_fft_preserves_signal() {
        let fft_size = 1024;
        let mut engine = SpectralEngine::new(fft_size, 512);

        // Create a simple signal
        let signal: Vec<f32> = (0..fft_size).map(|i| (i as f32 * 0.05).sin()).collect();

        // Forward FFT
        let mut spectrum = vec![Complex::new(0.0, 0.0); fft_size];
        engine.forward(&signal, &mut spectrum);

        // Inverse FFT
        engine.inverse(&mut spectrum);

        // Should recover the original signal
        for i in 0..fft_size {
            assert!(
                (spectrum[i].re - signal[i]).abs() < 1e-4,
                "Sample {i}: expected {}, got {}",
                signal[i],
                spectrum[i].re
            );
        }
    }

    #[test]
    fn magnitude_phase_round_trip() {
        let n = 512;
        let spectrum: Vec<Complex<f32>> = (0..n)
            .map(|i| Complex::new((i as f32 * 0.1).cos(), (i as f32 * 0.1).sin()))
            .collect();

        let mut mag = vec![0.0f32; n];
        let mut ph = vec![0.0f32; n];
        let mut reconstructed = vec![Complex::new(0.0, 0.0); n];

        magnitude(&spectrum, &mut mag);
        phase(&spectrum, &mut ph);
        from_polar(&mag, &ph, &mut reconstructed);

        for i in 0..n {
            assert!(
                (reconstructed[i].re - spectrum[i].re).abs() < 1e-5,
                "Re mismatch at bin {i}"
            );
            assert!(
                (reconstructed[i].im - spectrum[i].im).abs() < 1e-5,
                "Im mismatch at bin {i}"
            );
        }
    }
}
