//! Audio input: capture → resample → processor chain → encode.
//!
//! The processor chain handles all signal processing in a declarative,
//! DAW-style pipeline:
//!
//! ```text
//! denoise → meter(denoised) → gate → compressor → AGC → gain → clamp → meter(pipeline) → VAD
//! ```
//!
//! ## Device compatibility
//!
//! Uses cpal's `default_input_config()` — accepts whatever the OS gives us.
//! Resamples to 48 kHz internally, downmixes to mono. Works with Bluetooth
//! HFP (8/16 kHz), USB mics (44.1/48/96 kHz), audio interfaces, etc.

use crate::input_config::InputConfig;
use crate::processor::{
    agc::AutoGainControl,
    clamp::ClampStage,
    compressor::Compressor,
    denoise::Denoiser,
    gain::GainStage,
    gate::NoiseGate,
    level_meter::{LevelMeter, MeterTarget},
    vad::VadStage,
    FrameContext, ProcessorChain,
};
use crate::resampler::build_input_resampler;
use crate::stats::AudioStats;
use crate::transport::jitter_buffer::DecodedFrame;
use crate::FRAME_SIZE;

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
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
    loopback_tx: Option<Sender<DecodedFrame>>,

    ring_producer: Option<Producer<f32>>,
    ring_consumer: Option<Consumer<f32>>,

    /// Wake latch: capture callback → worker ("data available").
    wake_tx: Option<Sender<()>>,
    wake_rx: Option<Receiver<()>>,

    worker_thread: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,

    /// The macro rack — one Arc, all knobs.
    config: Arc<InputConfig>,

    // ── Observable state (read-only from outside) ────────────────────
    input_level: Arc<AtomicU32>,
    is_speaking: Arc<AtomicBool>,

    stats: Arc<AudioStats>,
}

impl AudioInput {
    pub fn new(
        device_name: Option<&str>,
        encoder_tx: Sender<Vec<f32>>,
        loopback_tx: Option<Sender<DecodedFrame>>,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        let device = if let Some(name) = device_name {
            host.input_devices()?
                .find(|d| crate::device_name(d).as_deref() == Some(name))
                .ok_or_else(|| format!("Input device '{name}' not found"))?
        } else {
            host.default_input_device()
                .ok_or("No default input device available")?
        };

        let device_config = device
            .default_input_config()
            .map_err(|e| format!("Failed to get default input config: {e}"))?;

        let sample_rate = device_config.sample_rate();
        let channels = device_config.channels();

        info!(
            "🎤 Input device: {} @ {}Hz, {}ch, {:?}",
            crate::device_name(&device).unwrap_or_else(|| "Unknown".into()),
            sample_rate,
            channels,
            device_config.sample_format(),
        );

        let ring_size = ((sample_rate as usize) * 200) / 1000;
        let (ring_producer, ring_consumer) = RingBuffer::new(ring_size.max(9600));
        // Wake latch: the capture callback pings this after pushing samples
        // so the worker wakes on data instead of sleep-polling. (Windows
        // quantizes short sleeps to the system timer — historically up to
        // ~15.6 ms — so polling with sleep(1ms) was a hidden latency tax.)
        let (wake_tx, wake_rx) = bounded::<()>(1);

        Ok(Self {
            device,
            device_config,
            stream: None,
            encoder_tx,
            loopback_tx,
            ring_producer: Some(ring_producer),
            ring_consumer: Some(ring_consumer),
            wake_tx: Some(wake_tx),
            wake_rx: Some(wake_rx),
            worker_thread: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            config,
            input_level: Arc::new(AtomicU32::new(0.0_f32.to_bits())),
            is_speaking: Arc::new(AtomicBool::new(false)),
            stats,
        })
    }

    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate();
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        self.start_worker_thread(sample_rate);

        let stream = match format {
            SampleFormat::I16 => self.build_stream::<i16>(sample_rate, channels)?,
            SampleFormat::U16 => self.build_stream::<u16>(sample_rate, channels)?,
            SampleFormat::F32 => self.build_stream::<f32>(sample_rate, channels)?,
            _ => return Err(format!("Unsupported sample format: {format:?}").into()),
        };

        stream.play()?;
        self.stream = Some(stream);
        Ok(())
    }

    // ── cpal stream callback ─────────────────────────────────────────────

    fn build_stream<T>(
        &mut self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Stream, Box<dyn std::error::Error>>
    where
        T: DaspSample + FromSample<f32> + ToSample<f32> + Send + 'static + cpal::SizedSample,
    {
        let stream_config = StreamConfig {
            channels,
            sample_rate,
            // ~10 ms when the device advertises a supported range;
            // untouched (Default) when it doesn't. See platform.rs.
            buffer_size: crate::platform::request_low_latency_buffer(
                self.device_config.buffer_size(),
                sample_rate,
            ),
        };

        let mut ring_producer = self
            .ring_producer
            .take()
            .ok_or("Ring producer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("Wake sender already taken")?;
        let config = self.config.clone();
        let stats = self.stats.clone();
        let stats_err = self.stats.clone();
        let input_level = self.input_level.clone();
        let ch = channels as usize;

        let stream = self.device.build_input_stream(
            stream_config,
            move |data: &[T], _: &_| {
                if config.muted.load(Ordering::Relaxed) {
                    input_level.store(0.0_f32.to_bits(), Ordering::Relaxed);
                    return;
                }

                let mut rms_acc = 0.0f32;
                let mut count = 0usize;

                for frame in data.chunks(ch) {
                    let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                    // No clamp here: f32 has headroom and the processing
                    // chain (gate → comp → AGC → clamp stage) owns levels.
                    // Clipping at capture destroyed the very peaks the
                    // compressor exists to handle gracefully.
                    let mono = sum / ch as f32;

                    rms_acc += mono * mono;
                    count += 1;

                    if ring_producer.push(mono).is_err() {
                        stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }

                if count > 0 {
                    let rms = (rms_acc / count as f32).sqrt();
                    let scaled = (rms * 3.0).clamp(0.0, 1.0);
                    input_level.store(scaled.to_bits(), Ordering::Relaxed);
                }

                // Data available — wake the worker (lock-free latch).
                let _ = wake_tx.try_send(());
            },
            move |err| {
                error!("Input stream error: {err}");
                stats_err.input_overruns.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )?;

        Ok(stream)
    }

    // ── Worker thread ────────────────────────────────────────────────────

    fn start_worker_thread(&mut self, device_sample_rate: u32) {
        let mut ring_consumer = self
            .ring_consumer
            .take()
            .expect("Ring consumer already taken");
        let wake_rx = self.wake_rx.take().expect("Wake receiver already taken");

        let should_stop = self.should_stop.clone();
        let stats = self.stats.clone();
        let encoder_tx = self.encoder_tx.clone();
        let loopback_tx = self.loopback_tx.clone();
        let config = self.config.clone();

        // ── Build the processor chain (the DAW view) ─────────────────
        let gate = NoiseGate::new(config.gate.clone(), 200);
        let compressor = Compressor::with_defaults(config.compressor.clone());
        let agc = AutoGainControl::new(config.agc.clone(), 30.0);
        let vad = VadStage::new();
        let is_speaking_handle = vad.speaking_handle();
        self.is_speaking = is_speaking_handle.clone();

        let mut chain = ProcessorChain::new("voice_input");
        chain.add(Box::new(Denoiser::new()));
        chain.add(Box::new(LevelMeter::new(MeterTarget::Denoised)));
        chain.add(Box::new(gate));
        chain.add(Box::new(compressor));
        chain.add(Box::new(agc));
        chain.add(Box::new(GainStage::new()));
        chain.add(Box::new(ClampStage::new()));
        chain.add(Box::new(LevelMeter::new(MeterTarget::Pipeline)));
        chain.add(Box::new(vad));

        self.worker_thread = Some(crate::platform::spawn_named("voice-capture", move || {
            // ── Resampler setup ─────────────────────────────────────
            let needs_resample = device_sample_rate != 48_000;
            let mut resampler: Option<Async<f32>> = None;
            let mut resampler_input: Vec<Vec<f32>> = Vec::new();
            let mut resampler_output: Vec<Vec<f32>> = Vec::new();
            let mut resampler_accum = Vec::with_capacity(2048);

            if needs_resample {
                let chunk = if device_sample_rate > 48_000 {
                    960
                } else {
                    crate::resampler::frames_for_ms(device_sample_rate, 10)
                };
                match build_input_resampler(device_sample_rate, chunk, 1) {
                    Ok(r) => {
                        resampler_input = vec![Vec::with_capacity(r.input_frames_next())];
                        resampler_output = vec![vec![0.0f32; r.output_frames_max()]];
                        resampler = Some(r);
                    }
                    Err(e) => {
                        error!("Failed to create input resampler: {e}");
                        return;
                    }
                }
            }

            let mut read_buf = vec![0.0f32; 4096];
            let mut accumulator = Vec::with_capacity(FRAME_SIZE * 2);
            // Monotonic sequence number for loopback frames. Without this the
            // jitter buffer sees every loopback frame as `seq: 0`, decides
            // each one is a late duplicate, and pumps up its adaptive target
            // over time — adding 100+ ms of latency to what should be the
            // fastest path in the pipeline.
            let mut loopback_seq: u32 = 0;
            // 48 kHz media clock for loopback frames (mirrors the encoder's
            // timestamp so the jitter buffer's estimator sees real deltas).
            let mut loopback_ts: u32 = 0;

            // Ask the OS for audio-class scheduling (MMCSS "Pro Audio"
            // on Windows; no-op elsewhere). Keeps small buffers glitch-
            // free under load.
            let _mmcss = crate::platform::promote_current_thread();

            info!(
                "🎤 Input worker started (device {}Hz, resample={})",
                device_sample_rate, needs_resample
            );

            while !should_stop.load(Ordering::Relaxed) {
                if config.muted.load(Ordering::Relaxed) {
                    is_speaking_handle.store(false, Ordering::Relaxed);
                    while ring_consumer.pop().is_ok() {}
                    crate::platform::wait_for_wake(&wake_rx, Duration::from_millis(10));
                    chain.reset();
                    continue;
                }

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
                    // Block until the capture callback delivers more audio
                    // (or a short timeout as a safety net) instead of
                    // sleep-polling.
                    crate::platform::wait_for_wake(&wake_rx, Duration::from_millis(5));
                    continue;
                }

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
                                error!("Resampler error: {e}");
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

                accumulator.extend_from_slice(&at_48k);

                while accumulator.len() >= FRAME_SIZE {
                    let mut frame: Vec<f32> = accumulator.drain(..FRAME_SIZE).collect();

                    let mut ctx = FrameContext {
                        denoise_enabled: config.denoise_enabled.load(Ordering::Relaxed),
                        compressor_enabled: config.compressor_enabled.load(Ordering::Relaxed),
                        agc_enabled: config.agc_enabled.load(Ordering::Relaxed),
                        input_gain: f32::from_bits(config.input_gain.load(Ordering::Relaxed)),
                        ..FrameContext::default()
                    };

                    chain.process(&mut frame, &mut ctx);

                    stats
                        .denoise_active
                        .store(ctx.denoise_enabled, Ordering::Relaxed);
                    stats
                        .denoised_level
                        .store(ctx.denoised_level.to_bits(), Ordering::Relaxed);
                    stats
                        .pipeline_level
                        .store(ctx.pipeline_level.to_bits(), Ordering::Relaxed);
                    stats.gate_active.store(ctx.gate_open, Ordering::Relaxed);
                    stats.vad_active.store(ctx.is_speech, Ordering::Relaxed);

                    if config.loopback_enabled.load(Ordering::Relaxed) {
                        if let Some(ref tx) = loopback_tx {
                            let _ = tx.try_send(DecodedFrame {
                                peer_id: LOOPBACK_SSRC,
                                seq: loopback_seq,
                                timestamp: loopback_ts,
                                arrival_time: Instant::now(),
                                samples: frame.clone(),
                                is_concealment: false,
                            });
                            loopback_seq = loopback_seq.wrapping_add(1);
                        }
                    }

                    // Every frame goes to the encoder: the gate has already
                    // zeroed non-speech, and Opus DTX (in the encoder thread)
                    // decides what actually hits the wire. Gating transmission
                    // here used to clip word onsets (earshot needs 16 ms of
                    // audio before its first speech decision) and starve DTX.
                    loopback_ts = loopback_ts.wrapping_add(FRAME_SIZE as u32);
                    match encoder_tx.try_send(frame) {
                        Ok(()) => {}
                        Err(TrySendError::Full(f)) => {
                            thread::yield_now();
                            let _ = encoder_tx.try_send(f);
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
        }));
    }

    pub fn input_level_handle(&self) -> Arc<AtomicU32> {
        self.input_level.clone()
    }

    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        self.is_speaking.clone()
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        self.stream.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}
