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
//! ## No receiver-side voice ducking
//!
//! Voice-chat echo in the stream is handled at the *source*: the
//! sharer's capture excludes the app's own playback (process loopback
//! on Windows — see `stream::process_loopback`). Playback here applies
//! only the user's volume; it never rides the stream level up and down
//! with speech.
//!
//! ## Pipeline
//!
//! ```text
//! Network → Opus decode (stereo) → ring buffer → resample → cpal playback
//! ```

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, Stream, SupportedStreamConfig,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::OPUS_SAMPLE_RATE;

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
    wake_tx: Option<Sender<()>>,
    wake_rx: Option<Receiver<()>>,

    // Worker
    worker_thread: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,

    // Controls
    /// Stream audio volume (f32 bits, 0.0–2.0). Applied before playback.
    volume: Arc<AtomicU32>,
    /// Mute stream audio without stopping the pipeline.
    is_muted: Arc<AtomicBool>,
}

impl StreamAudioOutput {
    /// Create a new stream audio output.
    ///
    /// - `device_name`: output device (or `None` for default)
    /// - `volume`: initial playback volume (0.0–2.0)
    pub fn new(device_name: Option<&str>, volume: f32) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        let device = if let Some(name) = device_name {
            host.output_devices()?
                .find(|d| crate::device_name(d).as_deref() == Some(name))
                .ok_or_else(|| format!("Output device '{}' not found", name))?
        } else {
            host.default_output_device()
                .ok_or("No default output device available")?
        };

        // 48 kHz when the device can do it, so the stereo sinc resampler
        // below is the exception path (see crate::device).
        let device_config = crate::device::preferred_output_config(&device)
            .map_err(|e| format!("Failed to get output config: {e}"))?;

        let sample_rate = device_config.sample_rate();
        let channels = device_config.channels();

        info!(
            "Stream audio output: {} @ {}Hz, {}ch, {:?}",
            crate::device_name(&device).unwrap_or_else(|| "Unknown".into()),
            sample_rate,
            channels,
            device_config.sample_format(),
        );

        // Ring buffer: 100ms at device rate, stereo interleaved.
        let ring_size = ((sample_rate as usize) * 2 * 100) / 1000;
        let ring_size = ring_size.max(9600);
        let (ring_producer, ring_consumer) = RingBuffer::new(ring_size);
        // Wake latch: playback callback → worker ("ring space freed").
        let (wake_tx, wake_rx) = bounded::<()>(1);

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
            wake_tx: Some(wake_tx),
            wake_rx: Some(wake_rx),
            worker_thread: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(volume.clamp(0.0, 2.0).to_bits())),
            is_muted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Start the playback pipeline.
    ///
    /// Spawns the worker thread and opens the cpal output stream.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate();
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        // Start worker thread BEFORE the cpal stream so it's ready to fill the ring
        if let (Some(ring_producer), Some(frame_rx), Some(wake_rx)) = (
            self.ring_producer.take(),
            self.frame_rx.take(),
            self.wake_rx.take(),
        ) {
            let handle = Self::spawn_worker(
                ring_producer,
                frame_rx,
                wake_rx,
                self.should_stop.clone(),
                self.ring_capacity,
                sample_rate,
                self.volume.clone(),
                self.is_muted.clone(),
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
        info!("Stream audio output started");
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
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let mut ring_consumer = self
            .ring_consumer
            .take()
            .ok_or("Ring consumer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("Wake sender already taken")?;
        let ch = channels as usize;

        let stream = self.device.build_output_stream(
            config,
            move |data: &mut [T], _: &_| {
                let stereo_frames_needed = data.len() / ch;

                for frame_idx in 0..stereo_frames_needed {
                    // Pop whole pairs only: draining the ring mid-pair
                    // (producer between its L and R push) would orphan
                    // the R sample and permanently swap channels for the
                    // rest of the stream. A lone half-pair stays in the
                    // ring; this frame renders silence instead.
                    let (left, right) = if ring_consumer.slots() >= 2 {
                        (
                            ring_consumer.pop().unwrap_or(0.0).clamp(-0.99, 0.99),
                            ring_consumer.pop().unwrap_or(0.0).clamp(-0.99, 0.99),
                        )
                    } else {
                        (0.0, 0.0) // underrun — render silence
                    };

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

                // Ring space just freed — wake the worker to refill.
                let _ = wake_tx.try_send(());
            },
            move |err| {
                error!("Stream audio output error: {err}");
            },
            None,
        )?;

        Ok(stream)
    }

    // ── Worker thread ───────────────────────────────────────────────────
    #[allow(clippy::too_many_arguments)]
    fn spawn_worker(
        mut ring_producer: Producer<f32>,
        frame_rx: Receiver<StreamAudioFrame>,
        wake_rx: Receiver<()>,
        should_stop: Arc<AtomicBool>,
        ring_capacity: usize,
        device_sample_rate: u32,
        volume: Arc<AtomicU32>,
        is_muted: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        crate::platform::spawn_named("stream-audio-out", move || {
            // Audio-class scheduling (MMCSS "Pro Audio" on Windows).
            let _rt = crate::platform::promote_current_thread(960, crate::OPUS_SAMPLE_RATE);

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
                    f_cutoff: Some(0.95),
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
                "Stream audio worker started (device {}Hz, resample={})",
                device_sample_rate, needs_resample,
            );

            while !should_stop.load(Ordering::Relaxed) {
                // ── Drain decoded frames into accumulator ────────────────
                while let Ok(frame) = frame_rx.try_recv() {
                    if is_muted.load(Ordering::Relaxed) {
                        continue; // drain but discard
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
                                // Whole stereo pairs only — a split pair
                                // on a full ring permanently swaps L/R
                                // downstream. Budget free slots up front.
                                let mut budget = ring_producer.slots() & !1_usize;
                                for i in 0..out_len {
                                    if budget < 2 {
                                        break; // ring full — drop remainder
                                    }
                                    let _ = ring_producer.push(resampler_output[0][i]);
                                    let _ = ring_producer.push(resampler_output[1][i]);
                                    budget -= 2;
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
                        // Whole stereo pairs only (see resample path).
                        // Unpushed remainder is still drained/dropped —
                        // same policy as before, now without pair splits.
                        let mut budget = ring_producer.slots() & !1_usize;
                        for i in 0..frames {
                            if budget < 2 {
                                break; // ring full — drop remainder
                            }
                            let _ = ring_producer.push(accum_l[i]);
                            let _ = ring_producer.push(accum_r[i]);
                            budget -= 2;
                        }
                        accum_l.drain(..frames);
                        accum_r.drain(..frames);
                    }
                }

                // ── Pace at 5 ms, but wake early when the device drains
                // the ring (production above is gated on ring fill, so
                // extra wakes are harmless). Replaces a bare sleep that
                // Windows could stretch to 15+ ms.
                let now = Instant::now();
                if now >= next_tick {
                    next_tick += Duration::from_millis(5);
                    if next_tick <= now {
                        next_tick = now + Duration::from_millis(5);
                    }
                }
                let wait = next_tick.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    crate::platform::wait_for_wake(&wake_rx, wait);
                }
            }

            info!("Stream audio worker stopped");
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
        info!("Stream audio output dropped");
    }
}
