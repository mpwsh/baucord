//! Spectral echo canceller.
//!
//! Removes a known reference signal from a captured signal using
//! frequency-domain spectral subtraction with overlap-add.
//!
//! ## Use cases
//!
//! - **Stream voice removal**: subtract voice output from system loopback
//!   so viewers don't hear voices twice.
//! - **Mic echo cancellation**: subtract speaker output from mic capture
//!   to eliminate acoustic echo (speakerphone mode).
//!
//! Both use cases share the same algorithm — only the configuration
//! differs (delay range, aggressiveness, smoothing).
//!
//! ## Pipeline position
//!
//! - Mic echo: first processor in the input chain (before denoise/gate)
//! - Stream: in the system capture worker (before encoding)
//!
//! ## Algorithm
//!
//! Per hop (512 samples, 10.7 ms at 48 kHz):
//! 1. Window capture and delay-aligned reference with Hann
//! 2. FFT both to complex spectra
//! 3. Subtract reference magnitude from capture magnitude
//! 4. Floor negative bins to spectral minimum (prevents musical noise)
//! 5. Reconstruct complex spectrum using clean magnitude + capture phase
//! 6. IFFT and overlap-add to produce clean output

use super::delay::DelayEstimator;
use super::reference_ring::VoiceReferenceRing;
use super::spectral::{self, OverlapAdd, SpectralEngine};

use crate::processor::{AudioProcessor, FrameContext};
use rustfft::num_complex::Complex;
use std::sync::Arc;

// ── Configuration ────────────────────────────────────────────────────────

/// Echo canceller configuration.
///
/// Different use cases need different tuning. Use the preset
/// constructors for sensible defaults.
#[derive(Debug, Clone)]
pub struct EchoCancelConfig {
    /// FFT size. 1024 = 21 ms at 48 kHz.
    /// Larger = better frequency resolution, more latency.
    pub fft_size: usize,
    /// Hop size. fft_size / 2 for 50% overlap.
    pub hop_size: usize,
    /// Oversubtraction factor (1.0–3.0).
    /// Higher removes more echo but risks eating non-echo audio.
    pub oversubtraction: f32,
    /// Spectral floor in dB. Bins below this after subtraction are
    /// clamped here instead of going to zero. Prevents musical noise.
    pub spectral_floor_db: f32,
    /// EMA coefficient for smoothing the reference spectrum.
    /// 0.3 = ~3-frame average. Higher = smoother, slower to adapt.
    pub spectrum_smooth_alpha: f32,
    /// Initial delay guess in samples (mic: 2400/50ms, stream: 960/20ms).
    pub initial_delay: usize,
}

impl EchoCancelConfig {
    /// Tuned for stream voice removal.
    ///
    /// Short, stable delay (digital loopback path). Moderate
    /// subtraction — we know the reference is accurate.
    pub fn for_stream() -> Self {
        Self {
            fft_size: 1024,
            hop_size: 512,
            oversubtraction: 1.5,
            spectral_floor_db: -40.0,
            spectrum_smooth_alpha: 0.3,
            initial_delay: 960, // 20 ms
        }
    }

    /// Tuned for mic echo cancellation.
    ///
    /// Longer delay (acoustic path through room). More aggressive
    /// subtraction because the acoustic path adds reverb that
    /// smears the echo across time/frequency.
    pub fn for_mic_echo() -> Self {
        Self {
            fft_size: 1024,
            hop_size: 512,
            oversubtraction: 2.0,
            spectral_floor_db: -35.0,
            spectrum_smooth_alpha: 0.4,
            initial_delay: 2400, // 50 ms
        }
    }
}

// ── Echo Canceller ──────────────────────────────────────────────────────

pub struct EchoCanceller {
    config: EchoCancelConfig,

    /// Shared reference ring (voice output samples).
    reference: Arc<VoiceReferenceRing>,

    /// Delay estimator (cross-correlation based).
    delay_estimator: DelayEstimator,

    /// FFT engine (plans, window, scratch).
    engine: SpectralEngine,

    /// Overlap-add reconstructor.
    overlap_add: OverlapAdd,

    // ── Per-frame working buffers (allocated once) ──────────────
    /// Input accumulator — collects samples until we have fft_size.
    input_accum: Vec<f32>,
    /// Output accumulator — buffered clean samples waiting to be
    /// written back into the process buffer.
    output_accum: Vec<f32>,

    /// Windowed capture frame.
    capture_windowed: Vec<f32>,
    /// Windowed reference frame (delay-compensated).
    reference_windowed: Vec<f32>,

    /// FFT buffers.
    capture_spectrum: Vec<Complex<f32>>,
    reference_spectrum: Vec<Complex<f32>>,
    clean_spectrum: Vec<Complex<f32>>,

    /// Magnitude / phase scratch.
    capture_mag: Vec<f32>,
    reference_mag: Vec<f32>,
    capture_phase: Vec<f32>,
    clean_mag: Vec<f32>,

    /// Smoothed reference magnitude spectrum (EMA across frames).
    reference_mag_smooth: Vec<f32>,

    /// Linear spectral floor (from config.spectral_floor_db).
    spectral_floor: f32,

    /// Previous input frame for overlap (last fft_size - hop_size samples).
    prev_input: Vec<f32>,
}

impl EchoCanceller {
    /// Create a new echo canceller.
    ///
    /// - `reference`: shared ring buffer of voice output audio
    /// - `config`: tuning parameters for the use case
    /// - `sample_rate`: audio sample rate (48000)
    pub fn new(
        reference: Arc<VoiceReferenceRing>,
        config: EchoCancelConfig,
        sample_rate: usize,
    ) -> Self {
        let fft_size = config.fft_size;
        let hop_size = config.hop_size;

        let engine = SpectralEngine::new(fft_size, hop_size);
        let overlap_add = OverlapAdd::new(fft_size, hop_size);
        let delay_estimator = DelayEstimator::new(config.initial_delay, sample_rate);

        let spectral_floor = 10.0_f32.powf(config.spectral_floor_db / 20.0);

        Self {
            config,
            reference,
            delay_estimator,
            engine,
            overlap_add,

            input_accum: Vec::with_capacity(fft_size),
            output_accum: Vec::with_capacity(fft_size * 2),

            capture_windowed: vec![0.0; fft_size],
            reference_windowed: vec![0.0; fft_size],

            capture_spectrum: vec![Complex::new(0.0, 0.0); fft_size],
            reference_spectrum: vec![Complex::new(0.0, 0.0); fft_size],
            clean_spectrum: vec![Complex::new(0.0, 0.0); fft_size],

            capture_mag: vec![0.0; fft_size],
            reference_mag: vec![0.0; fft_size],
            capture_phase: vec![0.0; fft_size],
            clean_mag: vec![0.0; fft_size],

            reference_mag_smooth: vec![0.0; fft_size],

            spectral_floor,

            prev_input: vec![0.0; fft_size - hop_size],
        }
    }

    /// Process one FFT hop: spectral subtraction.
    fn process_hop(&mut self) {
        let fft_size = self.config.fft_size;
        let hop_size = self.config.hop_size;
        let overlap = fft_size - hop_size;

        // ── Assemble capture frame: previous overlap + new hop ───
        self.capture_windowed[..overlap].copy_from_slice(&self.prev_input);
        self.capture_windowed[overlap..].copy_from_slice(&self.input_accum[..hop_size]);

        // Save current hop as next frame's overlap
        self.prev_input
            .copy_from_slice(&self.input_accum[..hop_size]);

        // ── Read delay-compensated reference ────────────────────
        let delay = self.delay_estimator.delay_samples();
        self.reference
            .read_delayed(delay, &mut self.reference_windowed);

        // ── Window both signals ─────────────────────────────────
        self.engine.window.apply(&mut self.capture_windowed);
        self.engine.window.apply(&mut self.reference_windowed);

        // ── Forward FFT ─────────────────────────────────────────
        self.engine
            .forward(&self.capture_windowed, &mut self.capture_spectrum);
        self.engine
            .forward(&self.reference_windowed, &mut self.reference_spectrum);

        // ── Extract magnitude and phase ─────────────────────────
        spectral::magnitude(&self.capture_spectrum, &mut self.capture_mag);
        spectral::phase(&self.capture_spectrum, &mut self.capture_phase);
        spectral::magnitude(&self.reference_spectrum, &mut self.reference_mag);

        // ── Smooth reference magnitude (EMA across frames) ──────
        let alpha = self.config.spectrum_smooth_alpha;
        for i in 0..fft_size {
            self.reference_mag_smooth[i] =
                self.reference_mag_smooth[i] * (1.0 - alpha) + self.reference_mag[i] * alpha;
        }

        // ── Spectral subtraction ────────────────────────────────
        let oversub = self.config.oversubtraction;
        let floor = self.spectral_floor;

        for i in 0..fft_size {
            let subtracted = self.capture_mag[i] - oversub * self.reference_mag_smooth[i];
            self.clean_mag[i] = subtracted.max(floor);
        }

        // ── Reconstruct complex spectrum ────────────────────────
        spectral::from_polar(
            &self.clean_mag,
            &self.capture_phase,
            &mut self.clean_spectrum,
        );

        // ── Inverse FFT ─────────────────────────────────────────
        self.engine.inverse(&mut self.clean_spectrum);

        // ── Synthesis window (for proper OLA reconstruction) ────
        // Extract real parts and apply synthesis window
        let mut frame: Vec<f32> = self.clean_spectrum.iter().map(|c| c.re).collect();
        self.engine.window.apply(&mut frame);

        // ── Overlap-add ─────────────────────────────────────────
        self.overlap_add.add_frame(&frame);

        // Drain available output into our accumulator
        let output = self.overlap_add.drain_output();
        self.output_accum.extend_from_slice(&output);
    }

    /// Feed reference samples to the delay estimator.
    ///
    /// Called internally from `process()` — reads from the reference
    /// ring and feeds both the capture and reference to the estimator.
    fn update_delay_estimate(&mut self, capture_samples: &[f32]) {
        // Feed capture to the estimator
        self.delay_estimator.feed_capture(capture_samples);

        // Read current reference window for delay estimation
        let delay = self.delay_estimator.delay_samples();
        let mut ref_buf = vec![0.0f32; capture_samples.len()];
        self.reference.read_delayed(delay, &mut ref_buf);
        self.delay_estimator.feed_reference(&ref_buf);

        // Try to compute a new estimate
        self.delay_estimator.try_estimate();
    }
}

impl AudioProcessor for EchoCanceller {
    fn name(&self) -> &str {
        "echo_cancel"
    }

    fn process(&mut self, samples: &mut [f32], _ctx: &mut FrameContext) {
        let hop_size = self.config.hop_size;

        // Update delay estimate (runs periodically, not every frame)
        self.update_delay_estimate(samples);

        // If the delay estimator isn't confident, pass through unchanged.
        // This avoids damaging audio when we don't have a reliable
        // reference alignment (nobody talking, or device just started).
        if !self.delay_estimator.is_confident() {
            return;
        }

        // Accumulate input samples
        self.input_accum.extend_from_slice(samples);

        // Process complete hops
        while self.input_accum.len() >= hop_size {
            self.process_hop();
            self.input_accum.drain(..hop_size);
        }

        // Write processed output back into the sample buffer.
        // The OLA produces hop_size samples per hop. We write as many
        // as we can into the output buffer, matching the input length.
        let available = self.output_accum.len().min(samples.len());
        samples[..available].copy_from_slice(&self.output_accum[..available]);
        self.output_accum.drain(..available);

        // If we don't have enough output yet (startup latency from
        // needing fft_size samples before the first output), zero-pad.
        // This only happens for the first ~21 ms after confidence is
        // established.
        if available < samples.len() {
            samples[available..].fill(0.0);
        }
    }

    fn reset(&mut self) {
        self.input_accum.clear();
        self.output_accum.clear();
        self.overlap_add.reset();
        self.prev_input.fill(0.0);
        self.reference_mag_smooth.fill(0.0);
    }
}
