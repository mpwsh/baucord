//! Audio input: capture → resample → denoise → gate → gain → clamp → VAD → encode.
//!
//! Device compatibility strategy:
//! - Use cpal's `default_input_config()` — accept whatever the OS/driver gives us
//! - Resample to 48 kHz internally (rubato handles any rate)
//! - Downmix to mono by averaging all channels
//! - This works with Bluetooth HFP (8/16 kHz), USB mics (44.1/48/96 kHz),
//!   audio interfaces (any rate), and everything in between.

use crate::denoise::Denoiser;
use crate::gate::NoiseGate;
use crate::jitter_buffer::DecodedFrame;
use crate::resampler::build_input_resampler;
use crate::stats::AudioStats;
use crate::vad::VADProcessor;
use crate::FRAME_SIZE;

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use crossbeam_channel::{Sender, TrySendError};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, Resampler};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Sentinel SSRC used for loopback frames so they don't collide with real peers.
pub const LOOPBACK_SSRC: u64 = u64::MAX;

pub struct AudioInput {
    device: Device,
    device_config: SupportedStreamConfig,
    stream: Option<Stream>,
    encoder_tx: Sender<Vec<f32>>,

    /// Optional sender into the output path for loopback monitoring.
    /// Points to the same channel the decoder thread feeds into.
    loopback_tx: Option<Sender<DecodedFrame>>,
    loopback_enabled: Arc<AtomicBool>,

    ring_producer: Option<Producer<f32>>,
    ring_consumer: Option<Consumer<f32>>,

    // Worker
    worker_thread: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,

    // Settings (shared with worker via atomics)
    is_muted: Arc<AtomicBool>,
    noise_suppression: Arc<AtomicBool>,
    /// Master input gain (f32 bits). Applied after gate, before encoding.
    input_gain: Arc<AtomicU32>,
    /// Noise gate threshold (f32 bits). The primary activation control.
    gate_threshold: Arc<AtomicU32>,

    // Observable state
    input_level: Arc<AtomicU32>, // f32 bits, 0.0–1.0 — raw mic capture level (no gain)
    is_speaking: Arc<AtomicBool>,

    // Stats
    stats: Arc<AudioStats>,
}

impl AudioInput {
    pub fn new(
        device_name: Option<&str>,
        encoder_tx: Sender<Vec<f32>>,
        loopback_tx: Option<Sender<DecodedFrame>>,
        stats: Arc<AudioStats>,
        noise_suppression: bool,
        input_gain: f32,
        gate_threshold: f32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        let device = if let Some(name) = device_name {
            host.input_devices()?
                .find(|d| d.name().ok().as_deref() == Some(name))
                .ok_or_else(|| format!("Input device '{}' not found", name))?
        } else {
            host.default_input_device()
                .ok_or("No default input device available")?
        };

        // Use whatever the OS says is best for this device.
        // This is the key to Bluetooth / weird device compatibility.
        let device_config = device
            .default_input_config()
            .map_err(|e| format!("Failed to get default input config: {}", e))?;

        let sample_rate = device_config.sample_rate().0;
        let channels = device_config.channels();

        info!(
            "🎤 Input device: {} @ {}Hz, {}ch, {:?}",
            device.name().unwrap_or_else(|_| "Unknown".into()),
            sample_rate,
            channels,
            device_config.sample_format(),
        );

        // Ring buffer: 200ms at device rate, mono (we downmix before pushing).
        // 200ms gives headroom for WASAPI exclusive mode and USB interfaces
        // that can deliver 20-30ms chunks, plus OS scheduling jitter.
        let ring_size = ((sample_rate as usize) * 200) / 1000;
        let (ring_producer, ring_consumer) = RingBuffer::new(ring_size.max(9600));

        // Gate threshold atomic — shared with the NoiseGate instance in the worker
        let gate_threshold_atomic =
            Arc::new(AtomicU32::new(gate_threshold.clamp(0.001, 0.5).to_bits()));

        Ok(Self {
            device,
            device_config,
            stream: None,
            encoder_tx,
            loopback_tx,
            loopback_enabled: Arc::new(AtomicBool::new(false)),
            ring_producer: Some(ring_producer),
            ring_consumer: Some(ring_consumer),
            worker_thread: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            is_muted: Arc::new(AtomicBool::new(false)),
            noise_suppression: Arc::new(AtomicBool::new(noise_suppression)),
            input_gain: Arc::new(AtomicU32::new(input_gain.to_bits())),
            gate_threshold: gate_threshold_atomic,
            input_level: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            is_speaking: Arc::new(AtomicBool::new(false)),
            stats,
        })
    }

    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate().0;
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        self.start_worker_thread(sample_rate);

        let stream = match format {
            SampleFormat::I16 => self.build_stream::<i16>(sample_rate, channels)?,
            SampleFormat::U16 => self.build_stream::<u16>(sample_rate, channels)?,
            SampleFormat::F32 => self.build_stream::<f32>(sample_rate, channels)?,
            _ => return Err(format!("Unsupported sample format: {:?}", format).into()),
        };

        stream.play()?;
        self.stream = Some(stream);
        Ok(())
    }

    fn build_stream<T>(
        &mut self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Stream, Box<dyn std::error::Error>>
    where
        T: DaspSample + FromSample<f32> + ToSample<f32> + Send + 'static + cpal::SizedSample,
    {
        let config = StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let mut ring_producer = self
            .ring_producer
            .take()
            .ok_or("Ring producer already taken")?;
        let is_muted = self.is_muted.clone();
        let stats = self.stats.clone();
        let stats_err = self.stats.clone();
        let input_level = self.input_level.clone();
        let ch = channels as usize;

        let stream = self.device.build_input_stream(
            &config,
            move |data: &[T], _: &_| {
                if is_muted.load(Ordering::Relaxed) {
                    input_level.store(0.0_f32.to_bits(), Ordering::Relaxed);
                    return;
                }

                // Downmix to mono: average all channels per frame.
                // No gain here — gain is applied in the worker after denoise/gate.
                let mut rms_acc = 0.0f32;
                let mut count = 0usize;

                for frame in data.chunks(ch) {
                    let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                    // Soft clamp: protect resampler and denoiser from digital clipping
                    let mono = (sum / ch as f32).clamp(-0.98, 0.98);

                    rms_acc += mono * mono;
                    count += 1;

                    if ring_producer.push(mono).is_err() {
                        stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }

                // RMS for VU meter
                if count > 0 {
                    let rms = (rms_acc / count as f32).sqrt();
                    let scaled = (rms * 3.0).clamp(0.0, 1.0);
                    input_level.store(scaled.to_bits(), Ordering::Relaxed);
                }
            },
            move |err| {
                error!("Input stream error: {}", err);
                stats_err.input_overruns.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )?;

        Ok(stream)
    }

    fn start_worker_thread(&mut self, device_sample_rate: u32) {
        let mut ring_consumer = self
            .ring_consumer
            .take()
            .expect("Ring consumer already taken");

        let should_stop = self.should_stop.clone();
        let stats = self.stats.clone();
        let encoder_tx = self.encoder_tx.clone();
        let is_speaking_handle = self.is_speaking.clone();
        let is_muted = self.is_muted.clone();
        let noise_suppression = self.noise_suppression.clone();
        let loopback_tx = self.loopback_tx.clone();
        let loopback_enabled = self.loopback_enabled.clone();
        let input_gain = self.input_gain.clone();
        let gate_threshold = self.gate_threshold.clone();

        self.worker_thread = Some(thread::spawn(move || {
            // ── Resampler (if device rate ≠ 48 kHz) ─────────────────────
            let needs_resample = device_sample_rate != 48000;
            let mut resampler: Option<Async<f32>> = None;
            let mut resampler_input: Vec<Vec<f32>> = Vec::new();
            let mut resampler_output: Vec<Vec<f32>> = Vec::new();
            let mut resampler_accum = Vec::with_capacity(2048);

            if needs_resample {
                let chunk = if device_sample_rate > 48000 {
                    960
                } else {
                    crate::resampler::frames_for_ms(device_sample_rate, 10)
                };
                match build_input_resampler(device_sample_rate, chunk) {
                    Ok(r) => {
                        resampler_input = vec![Vec::with_capacity(r.input_frames_next())];
                        resampler_output = vec![vec![0.0f32; r.output_frames_max()]];
                        resampler = Some(r);
                    }
                    Err(e) => {
                        error!("Failed to create input resampler: {}", e);
                        return;
                    }
                }
            }

            // ── Denoiser ────────────────────────────────────────────────
            let mut denoiser = Denoiser::new();

            // ── Noise gate (replaces AGC — users control their own gain,
            //    the gate just enforces a hard floor) ────────────────────
            // Uses the shared atomic so runtime set_gate_threshold() calls
            // from the AudioInput API propagate immediately to the gate.
            let mut gate = NoiseGate::with_threshold_handle(gate_threshold, 50);

            // ── VAD (earshot speech classifier) ─────────────────────────
            let mut vad = VADProcessor::new();

            // ── Pre-allocated conversion buffer for VAD ─────────────────
            // Avoids allocating a Vec<i16> on every VAD call.

            // ── Processing state ────────────────────────────────────────
            let mut read_buf = vec![0.0f32; 4096];
            let mut accumulator = Vec::with_capacity(FRAME_SIZE * 2);
            let mut consecutive_silence = 0usize;

            // ── VAD crossfade state ─────────────────────────────────────
            // Smooth transitions at VAD on/off edges to prevent clicks.
            // 64 samples ≈ 1.3ms at 48 kHz — inaudible but eliminates
            // the step discontinuity that causes digital cracking.
            const FADE_SAMPLES: usize = 64;
            let mut was_speech = false;
            let mut fade_in_remaining: usize = 0;
            let mut fade_out_remaining: usize = 0;

            info!(
                "🎤 Input worker started (device {}Hz, resample={})",
                device_sample_rate, needs_resample
            );

            while !should_stop.load(Ordering::Relaxed) {
                // Drain ring when muted
                if is_muted.load(Ordering::Relaxed) {
                    is_speaking_handle.store(false, Ordering::Relaxed);
                    while ring_consumer.pop().is_ok() {}
                    thread::sleep(Duration::from_millis(10));
                    denoiser.reset();
                    gate.reset();
                    continue;
                }

                // ── Read from ring ──────────────────────────────────────
                let target = if let Some(ref r) = resampler {
                    r.input_frames_next()
                } else {
                    FRAME_SIZE
                };

                let mut n_read = 0;
                for i in 0..target.min(read_buf.len()) {
                    match ring_consumer.pop() {
                        Ok(s) => {
                            read_buf[i] = s;
                            n_read += 1;
                        }
                        Err(_) => break,
                    }
                }

                if n_read == 0 {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }

                // ── Resample to 48 kHz if needed ────────────────────────
                let at_48k = if let Some(ref mut r) = resampler {
                    resampler_accum.extend_from_slice(&read_buf[..n_read]);

                    if resampler_accum.len() >= r.input_frames_next() {
                        let n = r.input_frames_next();
                        resampler_input[0].clear();
                        resampler_input[0].extend_from_slice(&resampler_accum[..n]);
                        resampler_accum.drain(..n);

                        let expected = r.output_frames_next();
                        resampler_output[0].resize(expected, 0.0);

                        let in_adapter = match SequentialSliceOfVecs::new(&resampler_input, 1, n) {
                            Ok(a) => a,
                            Err(e) => {
                                error!("Input resampler adapter error: {e}");
                                continue;
                            }
                        };
                        let mut out_adapter = match SequentialSliceOfVecs::new_mut(
                            &mut resampler_output,
                            1,
                            expected,
                        ) {
                            Ok(a) => a,
                            Err(e) => {
                                error!("Input resampler output adapter error: {e}");
                                continue;
                            }
                        };

                        match r.process_into_buffer(&in_adapter, &mut out_adapter, None) {
                            Ok((_in_len, out_len)) if out_len > 0 => {
                                resampler_output[0][..out_len].to_vec()
                            }
                            Ok(_) => Vec::new(),
                            Err(e) => {
                                error!("Resampler error: {}", e);
                                Vec::new()
                            }
                        }
                    } else {
                        continue;
                    }
                } else {
                    read_buf[..n_read].to_vec()
                };

                if at_48k.is_empty() {
                    continue;
                }

                // ── Accumulate into FRAME_SIZE (480) chunks ─────────────
                accumulator.extend_from_slice(&at_48k);

                while accumulator.len() >= FRAME_SIZE {
                    let mut frame: Vec<f32> = accumulator.drain(..FRAME_SIZE).collect();

                    // ── Denoise (runs first so gate/VAD see clean signal) ─
                    let denoise_on = noise_suppression.load(Ordering::Relaxed);
                    let rnnoise_vad_prob = if denoise_on {
                        denoiser.process_frame(&mut frame)
                    } else {
                        1.0 // assume speech if denoise is off
                    };
                    stats.denoise_active.store(denoise_on, Ordering::Relaxed);

                    // ── Post-denoise level (what the denoiser produced,
                    //    before gate/gain — use for threshold visualization)
                    {
                        let rms: f32 =
                            (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
                        let scaled = (rms * 3.0).clamp(0.0, 1.0);
                        stats
                            .denoised_level
                            .store(scaled.to_bits(), Ordering::Relaxed);
                    }

                    // ── Noise gate (hard threshold — silences everything
                    //    below the floor, passes signal untouched above it).
                    //    The gate is AUTHORITATIVE: if it's closed, we don't
                    //    send audio regardless of what VAD thinks.
                    let gate_open = gate.process_frame(&mut frame);
                    stats.gate_active.store(gate.is_open(), Ordering::Relaxed);

                    // ── Input gain (user preference, applied to gated signal)
                    let gain = f32::from_bits(input_gain.load(Ordering::Relaxed));
                    if (gain - 1.0).abs() > f32::EPSILON {
                        for s in frame.iter_mut() {
                            *s *= gain;
                        }
                    }

                    // ── Hard clamp (protect earshot VAD from overflow) ───────
                    for s in frame.iter_mut() {
                        *s = s.clamp(-0.95, 0.95);
                    }

                    // ── Pipeline output level (what VAD/encoder actually see) ─
                    {
                        let rms: f32 =
                            (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
                        let scaled = (rms * 3.0).clamp(0.0, 1.0);
                        stats
                            .pipeline_level
                            .store(scaled.to_bits(), Ordering::Relaxed);
                    }

                    // ── Loopback tap (post-pipeline: hear what others hear) ──
                    if loopback_enabled.load(Ordering::Relaxed) {
                        if let Some(ref tx) = loopback_tx {
                            let _ = tx.try_send(DecodedFrame {
                                peer_id: LOOPBACK_SSRC,
                                seq: 0,
                                arrival_time: Instant::now(),
                                samples: frame.clone(),
                                is_concealment: false,
                            });
                        }
                    }

                    // ── Speech detection ─────────────────────────────────
                    // Gate is authoritative: if the gate is closed, this frame
                    // is silence and we never send it, regardless of VAD.
                    // VAD only runs when the gate is open, to distinguish
                    // speech from non-speech noise that's above the gate
                    // threshold (e.g., a loud fan).
                    let is_speech = if !gate_open {
                        // Gate closed — definitely not speech
                        false
                    } else if rnnoise_vad_prob < 0.1 {
                        // RNNoise says definitely not speech — skip earshot
                        false
                    } else {
                        // Gate open, RNNoise ambiguous — let earshot classify
                        vad.process(&frame)
                    };

                    is_speaking_handle.store(is_speech, Ordering::Relaxed);
                    stats.vad_active.store(is_speech, Ordering::Relaxed);

                    // ── Crossfade at VAD transitions ────────────────────
                    // Detect edges
                    if is_speech && !was_speech {
                        fade_in_remaining = FADE_SAMPLES;
                    }
                    if !is_speech && was_speech {
                        fade_out_remaining = FADE_SAMPLES;
                    }
                    was_speech = is_speech;

                    // Apply fade-in: ramp onset from 0→1 over FADE_SAMPLES
                    if is_speech && fade_in_remaining > 0 {
                        for s in frame.iter_mut() {
                            if fade_in_remaining > 0 {
                                let t = 1.0 - (fade_in_remaining as f32 / FADE_SAMPLES as f32);
                                *s *= t;
                                fade_in_remaining -= 1;
                            }
                        }
                    }

                    // Send if speaking OR fading out (one extra frame ramped to zero)
                    let should_send = is_speech || fade_out_remaining > 0;

                    // Apply fade-out: ramp tail from 1→0 over FADE_SAMPLES
                    if !is_speech && fade_out_remaining > 0 {
                        for s in frame.iter_mut() {
                            if fade_out_remaining > 0 {
                                let t = fade_out_remaining as f32 / FADE_SAMPLES as f32;
                                *s *= t;
                                fade_out_remaining -= 1;
                            }
                        }
                    }

                    if should_send {
                        consecutive_silence = 0;
                        match encoder_tx.try_send(frame.clone()) {
                            Ok(_) => {}
                            Err(TrySendError::Full(_)) => {
                                // Back-pressure: yield once and retry
                                thread::yield_now();
                                let _ = encoder_tx.try_send(frame);
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    } else {
                        consecutive_silence += 1;
                        if consecutive_silence > 10 {
                            is_speaking_handle.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
        }));
    }

    // ── Public controls ──────────────────────────────────────────────────

    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
        if muted {
            self.input_level.store(0.0_f32.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn set_noise_suppression(&self, enabled: bool) {
        self.noise_suppression.store(enabled, Ordering::Relaxed);
    }

    pub fn set_loopback(&self, enabled: bool) {
        self.loopback_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn set_input_gain(&self, gain: f32) {
        let clamped = gain.clamp(0.0, 2.0);
        self.input_gain.store(clamped.to_bits(), Ordering::Relaxed);
    }

    /// Set the noise gate threshold at runtime.
    /// Values: 0.001–0.5 (RMS). See [`NoiseGate::new`] for guidance.
    pub fn set_gate_threshold(&self, threshold: f32) {
        let clamped = threshold.clamp(0.001, 0.5);
        self.gate_threshold
            .store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn input_level_handle(&self) -> Arc<AtomicU32> {
        self.input_level.clone()
    }

    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        self.is_speaking.clone()
    }

    pub fn is_muted(&self) -> bool {
        self.is_muted.load(Ordering::Relaxed)
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        // Drop the cpal stream FIRST — stops CoreAudio callbacks so they
        // can't deadlock trying to push into the ring buffer while the
        // worker thread is being joined.
        self.stream.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}
