//! Audio input: capture → resample → processor chain → encode.
//!
//! The processor chain handles all signal processing in a declarative,
//! DAW-style pipeline:
//!
//! ```text
//! denoise → meter(denoised) → gate → AGC → compressor → gain → clamp → meter(pipeline) → speaking
//! ```
//!
//! AGC runs before the compressor: the slow level normaliser must see the
//! raw dynamics, otherwise it is chasing a signal the fast stage already
//! flattened and its "target RMS" stops meaning anything.
//!
//! ## Device compatibility
//!
//! Asks the device for 48 kHz first (see [`crate::device`]) and only
//! resamples when it cannot do that — Bluetooth HFP at 8/16 kHz, fixed
//! 44.1 kHz interfaces. Downmixes to mono.
//!
//! ## Footprint
//!
//! One frame is a [`PcmFrame`] (`[f32; FRAME_SIZE]`) passed by value: the
//! capture → encoder and capture → self-monitor hand-offs allocate nothing
//! per frame, and the ring buffer is read in chunks, not sample by sample.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, Stream, StreamConfig, SupportedStreamConfig,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, Resampler};

use crate::{
    input_config::InputConfig,
    processor::{
        agc::AutoGainControl,
        clamp::ClampStage,
        compressor::Compressor,
        denoise::Denoiser,
        gain::GainStage,
        gate::NoiseGate,
        level_meter::{LevelMeter, MeterTarget},
        speaking::SpeakingIndicator,
        FrameContext, ProcessorChain,
    },
    resampler::build_input_resampler,
    stats::AudioStats,
    PcmFrame, FRAME_SIZE, OPUS_SAMPLE_RATE,
};

pub struct AudioInput {
    device: Device,
    device_config: SupportedStreamConfig,
    /// The user's device selection (`None` = follow the OS default), for
    /// the native backend's own device lookup.
    #[cfg(windows)]
    device_name: Option<String>,
    /// cpal stream, when the cpal backend is in use.
    stream: Option<Stream>,
    /// Native backend stream, when it opened successfully (Windows).
    #[cfg(windows)]
    native: Option<crate::native::wasapi::NativeCapture>,
    encoder_tx: Sender<PcmFrame>,
    loopback_tx: Sender<PcmFrame>,

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
        encoder_tx: Sender<PcmFrame>,
        loopback_tx: Sender<PcmFrame>,
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

        let device_config = crate::device::preferred_input_config(&device)
            .map_err(|e| format!("Failed to get input config: {e}"))?;

        let sample_rate = device_config.sample_rate();
        let channels = device_config.channels();

        info!(
            "Input device: {} @ {}Hz, {}ch, {:?}",
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
            #[cfg(windows)]
            device_name: device_name.map(String::from),
            stream: None,
            #[cfg(windows)]
            native: None,
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
        self.stats
            .input_stream_failed
            .store(false, Ordering::Relaxed);

        #[cfg(windows)]
        {
            match self.start_native() {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!("Native WASAPI capture unavailable ({e}); using cpal"),
            }
        }
        self.start_cpal()
    }

    /// Windows: open the device on our own MMCSS thread (see
    /// `native::wasapi`). On failure the ring and latch are handed back
    /// untouched so the cpal path can use them.
    #[cfg(windows)]
    fn start_native(&mut self) -> Result<(), String> {
        let producer = self
            .ring_producer
            .take()
            .ok_or("ring producer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("wake sender already taken")?;
        match crate::native::wasapi::NativeCapture::start(
            self.device_name.as_deref(),
            producer,
            wake_tx,
            self.stats.clone(),
            self.config.clone(),
            self.input_level.clone(),
        ) {
            Ok(native) => {
                let rate = native.sample_rate;
                self.native = Some(native);
                self.start_worker_thread(rate)
                    .map_err(|e| format!("worker thread: {e}"))
            }
            Err(err) => {
                self.ring_producer = Some(err.producer);
                self.wake_tx = Some(err.wake_tx);
                Err(err.reason)
            }
        }
    }

    fn start_cpal(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate();
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        self.start_worker_thread(sample_rate)?;

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
            // Capture wants *tolerance*, not a small buffer: see
            // device::capture_buffer_size.
            buffer_size: crate::device::capture_buffer_size(&self.device, &self.device_config),
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
        // cpal's capture thread runs our callback; promote it on first
        // entry (see platform::promote_callback_thread).
        let mut promoted = false;
        // The engine reports every capture gap through the error callback;
        // under a starving game that is tens per second. Count all of them
        // (stats), log at most one per second.
        let mut last_err_log: Option<std::time::Instant> = None;

        let stream = self.device.build_input_stream(
            stream_config,
            move |data: &[T], _: &_| {
                if !promoted {
                    promoted = true;
                    crate::platform::promote_callback_thread(FRAME_SIZE as u32, sample_rate);
                }
                if config.muted.load(Ordering::Relaxed) {
                    input_level.store(0.0_f32.to_bits(), Ordering::Relaxed);
                    return;
                }

                let mut rms_acc = 0.0f32;
                let mut count = 0usize;
                let mut overrun = false;

                // Downmix into the ring in one chunked write per callback.
                // `write_chunk_uninit` hands back exactly the free slots
                // (capped at what we need); anything beyond is an overrun.
                let frames = data.len() / ch;
                let want = frames.min(ring_producer.slots());
                if want < frames {
                    overrun = true;
                }
                if want > 0 {
                    if let Ok(chunk) = ring_producer.write_chunk_uninit(want) {
                        let mut it = data.chunks(ch).take(want).map(|frame| {
                            let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                            // No clamp here: f32 has headroom and the
                            // processing chain (gate → AGC → comp → clamp
                            // stage) owns levels. Clipping at capture
                            // destroyed the very peaks the compressor
                            // exists to handle gracefully.
                            #[allow(clippy::cast_precision_loss)]
                            let mono = sum / ch as f32;
                            rms_acc += mono * mono;
                            count += 1;
                            mono
                        });
                        chunk.fill_from_iter(&mut it);
                    } else {
                        overrun = true;
                    }
                }
                if overrun {
                    stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                }

                if count > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let rms = (rms_acc / count as f32).sqrt();
                    let scaled = (rms * 3.0).clamp(0.0, 1.0);
                    input_level.store(scaled.to_bits(), Ordering::Relaxed);
                }

                // Data available — wake the worker (lock-free latch).
                let _ = wake_tx.try_send(());
            },
            move |err| {
                if matches!(err.kind(), cpal::ErrorKind::DeviceNotAvailable) {
                    stats_err.input_stream_failed.store(true, Ordering::Relaxed);
                }
                stats_err.input_overruns.fetch_add(1, Ordering::Relaxed);
                let now = std::time::Instant::now();
                if last_err_log.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
                    last_err_log = Some(now);
                    error!(
                        "Input stream error: {err} (total {})",
                        stats_err.input_overruns.load(Ordering::Relaxed)
                    );
                }
            },
            None,
        )?;

        Ok(stream)
    }

    // ── Worker thread ────────────────────────────────────────────────────

    fn start_worker_thread(
        &mut self,
        device_sample_rate: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ring_consumer = self
            .ring_consumer
            .take()
            .ok_or("Ring consumer already taken")?;
        let wake_rx = self.wake_rx.take().ok_or("Wake receiver already taken")?;

        let should_stop = self.should_stop.clone();
        let stats = self.stats.clone();
        let encoder_tx = self.encoder_tx.clone();
        let loopback_tx = self.loopback_tx.clone();
        let config = self.config.clone();

        // ── Build the processor chain (the DAW view) ─────────────────
        let gate = NoiseGate::new(config.gate.clone(), 200);
        let agc = AutoGainControl::new(config.agc.clone(), 30.0);
        let compressor = Compressor::with_defaults(config.compressor.clone());
        let speaking = SpeakingIndicator::new();
        let is_speaking_handle = speaking.speaking_handle();
        self.is_speaking = is_speaking_handle.clone();

        let mut chain = ProcessorChain::new("voice_input");
        chain.add(Box::new(Denoiser::new()));
        chain.add(Box::new(LevelMeter::new(MeterTarget::Denoised)));
        chain.add(Box::new(gate));
        chain.add(Box::new(agc));
        chain.add(Box::new(compressor));
        chain.add(Box::new(GainStage::new()));
        chain.add(Box::new(ClampStage::new()));
        chain.add(Box::new(LevelMeter::new(MeterTarget::Pipeline)));
        chain.add(Box::new(speaking));

        self.worker_thread = Some(crate::platform::spawn_named("voice-capture", move || {
            worker_loop(
                ring_consumer,
                wake_rx,
                should_stop,
                stats,
                encoder_tx,
                loopback_tx,
                config,
                chain,
                is_speaking_handle,
                device_sample_rate,
            );
        }));
        Ok(())
    }

    /// Friendly name of the device actually opened (native backend's
    /// report when in use, otherwise cpal's), for follow-the-default.
    pub fn opened_device_name(&self) -> Option<String> {
        #[cfg(windows)]
        if let Some(n) = self.native.as_ref() {
            return Some(n.device_name.clone());
        }
        crate::device_name(&self.device)
    }

    pub fn input_level_handle(&self) -> Arc<AtomicU32> {
        self.input_level.clone()
    }

    pub fn speaking_handle(&self) -> Arc<AtomicBool> {
        self.is_speaking.clone()
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one straight-line real-time loop
fn worker_loop(
    mut ring_consumer: Consumer<f32>,
    wake_rx: Receiver<()>,
    should_stop: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
    encoder_tx: Sender<PcmFrame>,
    loopback_tx: Sender<PcmFrame>,
    config: Arc<InputConfig>,
    mut chain: ProcessorChain,
    is_speaking: Arc<AtomicBool>,
    device_sample_rate: u32,
) {
    // ── Resampler setup ─────────────────────────────────────
    let needs_resample = device_sample_rate != OPUS_SAMPLE_RATE;
    let mut resampler: Option<Async<f32>> = None;
    let mut resampler_input: Vec<Vec<f32>> = Vec::new();
    let mut resampler_output: Vec<Vec<f32>> = Vec::new();
    let mut resampler_accum = Vec::with_capacity(4096);

    if needs_resample {
        let chunk = if device_sample_rate > OPUS_SAMPLE_RATE {
            FRAME_SIZE
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
    let mut accumulator: Vec<f32> = Vec::with_capacity(FRAME_SIZE * 4);
    let mut frame: PcmFrame = [0.0; FRAME_SIZE];

    // Ask the OS for audio-class scheduling. Keeps small buffers glitch-
    // free under load.
    #[allow(clippy::cast_possible_truncation)]
    let _rt = crate::platform::promote_current_thread(FRAME_SIZE as u32, OPUS_SAMPLE_RATE);

    info!("Input worker started (device {device_sample_rate}Hz, resample={needs_resample})");

    while !should_stop.load(Ordering::Relaxed) {
        if config.muted.load(Ordering::Relaxed) {
            is_speaking.store(false, Ordering::Relaxed);
            // Discard whatever landed while muted.
            let stale = ring_consumer.slots();
            if let Ok(chunk) = ring_consumer.read_chunk(stale) {
                chunk.commit_all();
            }
            crate::platform::wait_for_wake(&wake_rx, Duration::from_millis(10));
            chain.reset();
            accumulator.clear();
            resampler_accum.clear();
            continue;
        }

        #[allow(clippy::redundant_closure_for_method_calls)] // keeps Self/T inference explicit
        let target = resampler
            .as_ref()
            .map_or(FRAME_SIZE, |r| r.input_frames_next());

        let want = target.min(read_buf.len());
        let (popped, _) = ring_consumer.pop_partial_slice(&mut read_buf[..want]);
        let n_read = popped.len();

        if n_read == 0 {
            // Block until the capture callback delivers more audio
            // (or a short timeout as a safety net) instead of
            // sleep-polling.
            crate::platform::wait_for_wake(&wake_rx, Duration::from_millis(5));
            continue;
        }

        if let Some(ref mut r) = resampler {
            resampler_accum.extend_from_slice(&read_buf[..n_read]);

            let needed = r.input_frames_next();
            if resampler_accum.len() < needed {
                continue;
            }
            resampler_input[0].clear();
            resampler_input[0].extend_from_slice(&resampler_accum[..needed]);
            resampler_accum.drain(..needed);

            let expected = r.output_frames_next();
            resampler_output[0].resize(expected, 0.0);

            let in_adapter = match SequentialSliceOfVecs::new(&resampler_input, 1, needed) {
                Ok(a) => a,
                Err(e) => {
                    error!("Input resampler adapter error: {e}");
                    continue;
                }
            };
            let mut out_adapter =
                match SequentialSliceOfVecs::new_mut(&mut resampler_output, 1, expected) {
                    Ok(a) => a,
                    Err(e) => {
                        error!("Input resampler output adapter error: {e}");
                        continue;
                    }
                };

            match r.process_into_buffer(&in_adapter, &mut out_adapter, None) {
                Ok((_in_len, out_len)) => {
                    accumulator.extend_from_slice(&resampler_output[0][..out_len]);
                }
                Err(e) => error!("Resampler error: {e}"),
            }
        } else {
            accumulator.extend_from_slice(&read_buf[..n_read]);
        }

        while accumulator.len() >= FRAME_SIZE {
            frame.copy_from_slice(&accumulator[..FRAME_SIZE]);
            accumulator.drain(..FRAME_SIZE);

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
                // Frames are `Copy`: no clone, no allocation.
                let _ = loopback_tx.try_send(frame);
            }

            // Every frame goes to the encoder: the gate has already
            // zeroed non-speech, and Opus DTX (in the encoder thread)
            // decides what actually hits the wire. Gating transmission
            // here used to clip word onsets and starve DTX.
            match encoder_tx.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(f)) => {
                    thread::yield_now();
                    if encoder_tx.try_send(f).is_err() {
                        stats.outgoing_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        self.stream.take();
        #[cfg(windows)]
        self.native.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}
