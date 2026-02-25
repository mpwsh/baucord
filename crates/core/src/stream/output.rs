//! Stereo audio playback for screen share system audio.
//!
//! Runs alongside the mono voice chat `AudioOutput` as a completely
//! independent playback pipeline. Receives decoded stereo frames,
//! resamples to the output device rate, and plays back through the
//! default output device.
//!
//! ## Why separate from AudioOutput?
//!
//! - Voice chat is mono; screen share audio is stereo (game panning, music)
//! - Independent volume controls (voice vs stream audio)
//! - Deafening voice shouldn't kill screen share audio
//! - No jitter buffer complexity needed — system audio is continuous,
//!   not bursty like multi-peer voice chat
//!
//! ## Voice cancellation
//!
//! The stream audio typically contains a delayed copy of voice chat
//! (captured by the streamer's loopback). The [`VoiceCanceller`] uses
//! the viewer's own voice output energy to duck the stream during
//! active speech, suppressing the echo while preserving game/music audio.
//!
//! ## Pipeline
//!
//! ```text
//! Network → Opus decode (stereo) → voice cancel → ring buffer → resample → cpal playback
//! ```

use crate::stream::cancel::VoiceCanceller;
use crate::OPUS_SAMPLE_RATE;

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, SupportedStreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Decoded stereo audio frame from the network.
///
/// This is what the network layer produces after receiving and
/// Opus-decoding a `SystemAudioPacket`. The samples are interleaved
/// stereo f32 at 48 kHz: [L0, R0, L1, R1, ...].
#[derive(Debug, Clone)]
pub struct StreamAudioFrame {
    /// Interleaved stereo samples at 48 kHz.
    pub samples: Vec<f32>,
    /// Sequence number (for gap detection / stats).
    pub seq: u32,
}

/// Stereo audio output for screen share streams.
///
/// Receives decoded stereo frames, buffers them, resamples to the
/// output device rate, and plays through the default output device.
pub struct StreamAudioOutput {
    device: Device,
    device_config: SupportedStreamConfig,
    stream: Option<Stream>,

    /// Channel for receiving decoded stereo frames from the decoder thread.
    frame_tx: Sender<StreamAudioFrame>,
    frame_rx: Option<Receiver<StreamAudioFrame>>,

    ring_producer: Option<Producer<f32>>,
    ring_consumer: Option<Consumer<f32>>,
    ring_capacity: usize,

    // Worker
    worker_thread: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,

    // Controls
    /// Stream audio volume (f32 bits, 0.0–2.0). Applied before playback.
    volume: Arc<AtomicU32>,
    /// Mute stream audio without stopping the pipeline.
    is_muted: Arc<AtomicBool>,

    /// Voice output energy for voice cancellation (f32 bits, 0.0–1.0).
    /// Read from the voice chat's `AudioStats::output_level`.
    /// `None` if no voice reference is available (canceller disabled).
    voice_energy: Option<Arc<AtomicU32>>,
}

impl StreamAudioOutput {
    /// Create a new stream audio output.
    ///
    /// - `device_name`: output device (or `None` for default)
    /// - `volume`: initial playback volume (0.0–2.0)
    /// - `voice_energy`: shared atomic from voice output for cancellation
    ///    (`None` to disable voice cancellation)
    pub fn new(
        device_name: Option<&str>,
        volume: f32,
        voice_energy: Option<Arc<AtomicU32>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        let device = if let Some(name) = device_name {
            host.output_devices()?
                .find(|d| d.name().ok().as_deref() == Some(name))
                .ok_or_else(|| format!("Output device '{}' not found", name))?
        } else {
            host.default_output_device()
                .ok_or("No default output device available")?
        };

        let device_config = device
            .default_output_config()
            .map_err(|e| format!("Failed to get default output config: {e}"))?;

        let sample_rate = device_config.sample_rate().0;
        let channels = device_config.channels();

        info!(
            "🔊🎬 Stream audio output: {} @ {}Hz, {}ch, {:?}",
            device.name().unwrap_or_else(|_| "Unknown".into()),
            sample_rate,
            channels,
            device_config.sample_format(),
        );

        // Ring buffer: 100ms at device rate, stereo interleaved.
        let ring_size = ((sample_rate as usize) * 2 * 100) / 1000;
        let ring_size = ring_size.max(9600);
        let (ring_producer, ring_consumer) = RingBuffer::new(ring_size);

        // Frame channel: decoder thread pushes decoded frames here
        let (frame_tx, frame_rx) = bounded::<StreamAudioFrame>(16);

        Ok(Self {
            device,
            device_config,
            stream: None,
            frame_tx,
            frame_rx: Some(frame_rx),
            ring_producer: Some(ring_producer),
            ring_consumer: Some(ring_consumer),
            ring_capacity: ring_size,
            worker_thread: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(volume.clamp(0.0, 2.0).to_bits())),
            is_muted: Arc::new(AtomicBool::new(false)),
            voice_energy,
        })
    }

    /// Start the playback pipeline.
    ///
    /// Spawns the worker thread and opens the cpal output stream.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate().0;
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        // Start worker thread BEFORE the cpal stream so it's ready to fill the ring
        if let (Some(ring_producer), Some(frame_rx)) =
            (self.ring_producer.take(), self.frame_rx.take())
        {
            let handle = Self::spawn_worker(
                ring_producer,
                frame_rx,
                self.should_stop.clone(),
                self.ring_capacity,
                sample_rate,
                self.volume.clone(),
                self.is_muted.clone(),
                self.voice_energy.clone(),
            );
            self.worker_thread = Some(handle);
        }

        let stream = match format {
            SampleFormat::I16 => self.build_stream::<i16>(sample_rate, channels)?,
            SampleFormat::U16 => self.build_stream::<u16>(sample_rate, channels)?,
            SampleFormat::F32 => self.build_stream::<f32>(sample_rate, channels)?,
            _ => return Err(format!("Unsupported format: {:?}", format).into()),
        };

        stream.play()?;
        self.stream = Some(stream);
        info!("🔊🎬 Stream audio output started");
        Ok(())
    }

    /// Push a decoded stereo frame for playback.
    pub fn push_frame(&self, frame: StreamAudioFrame) {
        let _ = self.frame_tx.try_send(frame);
    }

    /// Set stream audio volume (0.0–2.0). 1.0 is passthrough.
    pub fn set_volume(&self, vol: f32) {
        let clamped = vol.clamp(0.0, 2.0);
        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Get a handle to the volume atomic for external control.
    pub fn volume_handle(&self) -> Arc<AtomicU32> {
        self.volume.clone()
    }

    /// Mute/unmute stream audio.
    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.is_muted.load(Ordering::Relaxed)
    }

    /// Get a handle to the muted atomic for external control.
    pub fn muted_handle(&self) -> Arc<AtomicBool> {
        self.is_muted.clone()
    }

    /// Get a clone of the internal frame sender.
    ///
    /// Used by the stream decoder to push decoded frames directly.
    pub fn frame_sender(&self) -> Sender<StreamAudioFrame> {
        self.frame_tx.clone()
    }

    // ── cpal stream ─────────────────────────────────────────────────────

    fn build_stream<T>(
        &mut self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Stream, Box<dyn std::error::Error>>
    where
        T: DaspSample + FromSample<f32> + ToSample<f32> + Send + 'static + cpal::SizedSample,
    {
        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let mut ring_consumer = self
            .ring_consumer
            .take()
            .ok_or("Ring consumer already taken")?;
        let ch = channels as usize;

        let stream = self.device.build_output_stream(
            &config,
            move |data: &mut [T], _: &_| {
                let stereo_frames_needed = data.len() / ch;

                for frame_idx in 0..stereo_frames_needed {
                    let left = ring_consumer.pop().unwrap_or(0.0).clamp(-0.99, 0.99);
                    let right = ring_consumer.pop().unwrap_or(0.0).clamp(-0.99, 0.99);

                    let base = frame_idx * ch;
                    match ch {
                        1 => {
                            data[base] = T::from_sample((left + right) * 0.5);
                        }
                        2 => {
                            data[base] = T::from_sample(left);
                            data[base + 1] = T::from_sample(right);
                        }
                        _ => {
                            data[base] = T::from_sample(left);
                            if base + 1 < data.len() {
                                data[base + 1] = T::from_sample(right);
                            }
                            for c in 2..ch {
                                if base + c < data.len() {
                                    data[base + c] = T::from_sample(0.0f32);
                                }
                            }
                        }
                    }
                }
            },
            move |err| {
                error!("Stream audio output error: {err}");
            },
            None,
        )?;

        Ok(stream)
    }

    // ── Worker thread ───────────────────────────────────────────────────

    fn spawn_worker(
        mut ring_producer: Producer<f32>,
        frame_rx: Receiver<StreamAudioFrame>,
        should_stop: Arc<AtomicBool>,
        ring_capacity: usize,
        device_sample_rate: u32,
        volume: Arc<AtomicU32>,
        is_muted: Arc<AtomicBool>,
        voice_energy: Option<Arc<AtomicU32>>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            // ── Voice canceller ─────────────────────────────────────────
            let mut canceller = voice_energy.as_ref().map(|_| VoiceCanceller::new());

            // ── Resampler (stereo, 48kHz → device rate) ─────────────────
            let needs_resample = device_sample_rate != OPUS_SAMPLE_RATE;
            let mut resampler: Option<Async<f32>> = None;
            let mut resampler_input: Vec<Vec<f32>> = Vec::new();
            let mut resampler_output: Vec<Vec<f32>> = Vec::new();

            if needs_resample {
                let chunk = crate::resampler::frames_for_ms(OPUS_SAMPLE_RATE, 20);
                let ratio = device_sample_rate as f64 / OPUS_SAMPLE_RATE as f64;
                let params = SincInterpolationParameters {
                    sinc_len: 64,
                    f_cutoff: 0.95,
                    interpolation: SincInterpolationType::Linear,
                    oversampling_factor: 128,
                    window: WindowFunction::BlackmanHarris2,
                };
                match Async::<f32>::new_sinc(
                    ratio,
                    1.0,
                    &params,
                    chunk,
                    2, // stereo
                    FixedAsync::Output,
                ) {
                    Ok(r) => {
                        let out_max = r.output_frames_max();
                        resampler_input = vec![
                            Vec::with_capacity(chunk + 64),
                            Vec::with_capacity(chunk + 64),
                        ];
                        resampler_output = vec![vec![0.0f32; out_max], vec![0.0f32; out_max]];
                        resampler = Some(r);
                    }
                    Err(e) => {
                        error!("Failed to create stream audio resampler: {e}");
                    }
                }
            }

            // ── Accumulator for interleaved stereo at 48kHz ─────────────
            let mut accum_l: Vec<f32> = Vec::with_capacity(4096);
            let mut accum_r: Vec<f32> = Vec::with_capacity(4096);

            let high_wm = ring_capacity * 85 / 100;
            let mut next_tick = Instant::now();

            info!(
                "🔊🎬 Stream audio worker started (device {}Hz, resample={}, voice_cancel={})",
                device_sample_rate,
                needs_resample,
                voice_energy.is_some(),
            );

            while !should_stop.load(Ordering::Relaxed) {
                // ── Update voice canceller ───────────────────────────────
                if let (Some(ref energy), Some(ref mut cancel)) = (&voice_energy, &mut canceller) {
                    let rms = f32::from_bits(energy.load(Ordering::Relaxed));
                    cancel.update_voice_energy(rms);
                }

                // ── Drain decoded frames into accumulator ────────────────
                while let Ok(mut frame) = frame_rx.try_recv() {
                    if is_muted.load(Ordering::Relaxed) {
                        continue; // drain but discard
                    }

                    // Apply voice cancellation BEFORE volume (so the
                    // canceller sees the raw signal, not user-scaled).
                    if let Some(ref mut cancel) = canceller {
                        cancel.process(&mut frame.samples);
                    }

                    // Apply volume
                    let vol = f32::from_bits(volume.load(Ordering::Relaxed));

                    // Deinterleave into L/R accumulators
                    for pair in frame.samples.chunks_exact(2) {
                        let l = if (vol - 1.0).abs() > f32::EPSILON {
                            (pair[0] * vol).clamp(-0.99, 0.99)
                        } else {
                            pair[0]
                        };
                        let r = if (vol - 1.0).abs() > f32::EPSILON {
                            (pair[1] * vol).clamp(-0.99, 0.99)
                        } else {
                            pair[1]
                        };
                        accum_l.push(l);
                        accum_r.push(r);
                    }
                }

                // ── Feed resampler and push to ring ─────────────────────
                if let Some(ref mut r) = resampler {
                    loop {
                        let needed = r.input_frames_next();
                        if accum_l.len() < needed || accum_r.len() < needed {
                            break;
                        }
                        // Check ring capacity before processing
                        let current_fill = ring_capacity - ring_producer.slots();
                        if current_fill >= high_wm {
                            break; // ring is full enough
                        }

                        resampler_input[0].clear();
                        resampler_input[0].extend_from_slice(&accum_l[..needed]);
                        resampler_input[1].clear();
                        resampler_input[1].extend_from_slice(&accum_r[..needed]);
                        accum_l.drain(..needed);
                        accum_r.drain(..needed);

                        let expected = r.output_frames_next();
                        resampler_output[0].resize(expected, 0.0);
                        resampler_output[1].resize(expected, 0.0);

                        let in_adapter =
                            match SequentialSliceOfVecs::new(&resampler_input, 2, needed) {
                                Ok(a) => a,
                                Err(e) => {
                                    error!("Stream audio resampler input adapter error: {e}");
                                    continue;
                                }
                            };
                        let mut out_adapter = match SequentialSliceOfVecs::new_mut(
                            &mut resampler_output,
                            2,
                            expected,
                        ) {
                            Ok(a) => a,
                            Err(e) => {
                                error!("Stream audio resampler output adapter error: {e}");
                                continue;
                            }
                        };

                        match r.process_into_buffer(&in_adapter, &mut out_adapter, None) {
                            Ok((_in_len, out_len)) => {
                                for i in 0..out_len {
                                    if ring_producer.push(resampler_output[0][i]).is_err() {
                                        break;
                                    }
                                    if ring_producer.push(resampler_output[1][i]).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Stream audio resampler error: {e}");
                            }
                        }
                    }
                } else {
                    // No resampling needed — push interleaved directly
                    let current_fill = ring_capacity - ring_producer.slots();
                    if current_fill < high_wm {
                        let frames = accum_l.len().min(accum_r.len());
                        for i in 0..frames {
                            if ring_producer.push(accum_l[i]).is_err() {
                                break;
                            }
                            if ring_producer.push(accum_r[i]).is_err() {
                                break;
                            }
                        }
                        accum_l.drain(..frames);
                        accum_r.drain(..frames);
                    }
                }

                // ── Pace at 5ms intervals ───────────────────────────────
                next_tick += Duration::from_millis(5);
                let now = Instant::now();
                if next_tick > now {
                    thread::sleep(next_tick - now);
                } else {
                    next_tick = now;
                }
            }

            info!("🔊🎬 Stream audio worker stopped");
        })
    }
}

impl Drop for StreamAudioOutput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        // Drop the cpal stream FIRST — stops platform audio callbacks so
        // they can't deadlock pulling from the ring buffer while the
        // worker thread is being joined.
        self.stream.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
        info!("🔊🎬 Stream audio output dropped");
    }
}
