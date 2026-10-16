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
        atomic::{AtomicBool, Ordering},
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
use crossbeam_channel::{Sender, TrySendError};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, Resampler};

use crate::{
    input_config::InputConfig,
    latch::{Ring, Wait},
    processor::{
        agc::AutoGainControl,
        compressor::Compressor,
        gate::NoiseGate,
        speaking::{SpeakingFlag, SpeakingIndicator},
        FrameContext, InputChain,
    },
    resampler::build_input_resampler,
    stats::AudioStats,
    PcmFrame, FRAME_SIZE, OPUS_SAMPLE_RATE,
};

pub struct AudioInput {
    /// cpal's device and the config it opens at: asked in `start_cpal`,
    /// only when the cpal path runs — a native backend finds its device
    /// by itself (see the same field in `output.rs`; PRODUCTION-PLAN.md
    /// CLI-12; review 2026-10 P2-49).
    cpal: Option<(Device, SupportedStreamConfig)>,
    /// The user's device selection (`None` = follow the OS default), for
    /// the native backend's own device lookup (and cpal's, late).
    device_name: Option<String>,
    /// cpal stream, when the cpal backend is in use.
    stream: Option<Stream>,
    /// Native backend stream, when it opened successfully (Windows).
    #[cfg(windows)]
    native: Option<crate::native::wasapi::NativeCapture>,
    /// The voice-call stream, when the session wants the platform's echo
    /// canceller and it opened (Android `native::aaudio`, iOS
    /// `native::vpio`).
    #[cfg(any(target_os = "android", target_os = "ios"))]
    native_voice: Option<crate::native::VoiceCapture>,
    /// Open on the platform's voice-call path (`aec::wants_aec` said so).
    voice: bool,
    /// The output device the canceller should treat as its reference
    /// (`None` = the default communications device, followed by
    /// Windows itself). Windows only: Android routes and references by
    /// itself.
    #[cfg(windows)]
    aec_reference: Option<String>,
    /// Windows: no device picked means the Default Communications
    /// Device (`communications_default` in lib.rs), else the console
    /// default.
    #[cfg(windows)]
    communications_default: bool,
    /// Windows: the canceller took the reference from us (see
    /// [`Self::aec_reference_named`]).
    #[cfg(windows)]
    aec_reference_named: bool,
    /// The backend's verdict once capture opened on the voice-call path:
    /// `Ok` = the platform canceller is on the microphone, `Err(why)` =
    /// it is not. `None` = not on the voice-call path.
    aec_verdict: Option<Result<(), &'static str>>,
    encoder_tx: Sender<PcmFrame>,
    loopback_tx: Sender<PcmFrame>,

    ring_producer: Option<Producer<f32>>,
    ring_consumer: Option<Consumer<f32>>,

    /// Wake latch: capture callback → worker ("data available").
    wake_tx: Option<Ring>,
    wake_rx: Option<Wait>,
    /// Our own ring of the latch: a stop wakes the worker at once
    /// instead of after its safety wait. Held for the input's life, it
    /// also keeps the latch from going dead under a running worker (a
    /// stream that stopped by itself), which would have it spin.
    stop_ring: Option<Ring>,

    worker_thread: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,

    /// The macro rack — one Arc, all knobs.
    config: Arc<InputConfig>,

    // ── Observable state (read-only from outside) ────────────────────
    /// The engine's speaking light, which this input's chain drives.
    is_speaking: Arc<SpeakingFlag>,

    stats: Arc<AudioStats>,
}

impl AudioInput {
    // Opens nothing (see the `cpal` field): the `Result` is the callers'.
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(
        device_name: Option<&str>,
        encoder_tx: Sender<PcmFrame>,
        loopback_tx: Sender<PcmFrame>,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
        is_speaking: Arc<SpeakingFlag>,
        voice: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // cpal is asked only by the cpal path (`start_cpal`): a phone on
        // the call path, or Windows' native capture, never needs it, and
        // asking was a device enumeration per open (review 2026-10,
        // P2-49). The ring is made for 48 kHz, the native backends'
        // rate; `start_cpal` grows it if cpal's rate needs more.
        let cpal: Option<(Device, SupportedStreamConfig)> = None;
        let (ring_producer, ring_consumer) = RingBuffer::new(ring_size_for(
            cpal.as_ref()
                .map_or(OPUS_SAMPLE_RATE, |(_, config)| config.sample_rate()),
        ));
        // Wake latch: the capture callback pings this after pushing samples
        // so the worker wakes on data instead of sleep-polling. (Windows
        // quantizes short sleeps to the system timer — historically up to
        // ~15.6 ms — so polling with sleep(1ms) was a hidden latency tax.)
        let (wake_tx, wake_rx) = crate::latch::latch();

        Ok(Self {
            cpal,
            device_name: device_name.map(String::from),
            stream: None,
            #[cfg(windows)]
            native: None,
            #[cfg(any(target_os = "android", target_os = "ios"))]
            native_voice: None,
            voice,
            #[cfg(windows)]
            aec_reference: None,
            #[cfg(windows)]
            communications_default: false,
            #[cfg(windows)]
            aec_reference_named: false,
            aec_verdict: None,
            encoder_tx,
            loopback_tx,
            ring_producer: Some(ring_producer),
            ring_consumer: Some(ring_consumer),
            stop_ring: Some(wake_tx.clone()),
            wake_tx: Some(wake_tx),
            wake_rx: Some(wake_rx),
            worker_thread: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            config,
            is_speaking,
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
                Ok(()) => {
                    CAPTURE_FALLBACK_SAID.store(false, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
                // Once a streak: a device that is not there is tried
                // again and again (review 2026-10, P2-38).
                Err(e)
                    if CAPTURE_FALLBACK_SAID.swap(true, std::sync::atomic::Ordering::Relaxed) =>
                {
                    log::debug!("Native WASAPI capture unavailable ({e}); using cpal");
                }
                Err(e) => log::warn!("Native WASAPI capture unavailable ({e}); using cpal"),
            }
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        if self.voice {
            match self.start_native_voice() {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!(
                        "Voice-call capture unavailable ({e}); using cpal, no echo cancellation"
                    );
                    self.aec_verdict = Some(Err(crate::aec::reasons::PATH_REFUSED));
                }
            }
        }
        // cpal opens plain streams: no platform canceller on this path.
        if self.voice && self.aec_verdict.is_none() {
            self.aec_verdict = Some(Err(crate::aec::reasons::PATH_REFUSED));
        }
        self.start_cpal()
    }

    /// The backend's echo-cancellation verdict (see the field).
    #[must_use]
    pub fn aec_verdict(&self) -> Option<Result<(), &'static str>> {
        self.aec_verdict
    }

    /// Windows: name the output device the canceller should use as its
    /// reference (call before `start`).
    #[cfg(windows)]
    pub fn set_aec_reference(&mut self, output_device: Option<String>) {
        self.aec_reference = output_device;
    }

    /// Windows: what "Default" means for this stream (call before
    /// `start`).
    #[cfg(windows)]
    pub fn set_communications_default(&mut self, on: bool) {
        self.communications_default = on;
    }

    /// Whether the platform took our output as the canceller's
    /// reference: on Windows, the reference endpoint was set — to the
    /// picked output, or to follow the communications default that
    /// playback opened; without the control (Windows 10, early 11) the
    /// effect picks its own. Elsewhere the call path's own playback is
    /// the reference.
    #[must_use]
    #[cfg_attr(not(windows), allow(clippy::unused_self))]
    pub fn aec_reference_named(&self) -> bool {
        #[cfg(windows)]
        {
            self.aec_reference_named
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// Android and iOS: the microphone on the voice-call path
    /// (`native::aaudio`, `native::vpio`), which is what lets the
    /// platform run its echo canceller on it. On failure the ring and
    /// latch go back for the cpal path.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    fn start_native_voice(&mut self) -> Result<(), String> {
        let producer = self
            .ring_producer
            .take()
            .ok_or("ring producer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("wake sender already taken")?;
        match crate::native::VoiceCapture::start(
            producer,
            wake_tx,
            self.stats.clone(),
            self.config.clone(),
        ) {
            Ok(native) => {
                let rate = native.sample_rate;
                // Verified, not assumed (AEC-SPEC.md D5): on Android the
                // preset has to have stuck and the canceller has to be on
                // the session (aaudio::echo_canceller_on); on iOS the
                // unit's processing has to read back as on.
                self.aec_verdict = Some(native.aec_verdict);
                self.native_voice = Some(native);
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
        // On the voice-call path: the Communications category, with the
        // canceller's reference set to our output (AEC-SPEC.md §4.1).
        let voice = self.voice.then(|| self.aec_reference.clone());
        match crate::native::wasapi::NativeCapture::start(
            self.device_name.as_deref(),
            voice,
            self.communications_default,
            producer,
            wake_tx,
            self.stats.clone(),
            self.config.clone(),
        ) {
            Ok(native) => {
                let rate = native.sample_rate;
                self.aec_verdict = native.aec_verdict;
                self.aec_reference_named = native.aec_reference_named;
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
        // cpal is asked only now (see `new`) — and its rate can want a
        // bigger ring than the 48 kHz one `new()` made. Nothing holds the
        // ring yet.
        if self.cpal.is_none() {
            // cpal's default is the console one; with "Default" meaning
            // the communications device (`communications_default`) this
            // fallback opened another device than voice follows, and the
            // echo report described streams that were not there (review
            // 2026-10, P3-31). That device, by name — cpal brings COM up
            // on this thread before it is asked.
            #[cfg(windows)]
            let name = self.device_name.clone().or_else(|| {
                self.communications_default
                    .then(|| {
                        let _ = cpal::default_host().default_input_device();
                        crate::native::wasapi::communications_default_name(true)
                    })
                    .flatten()
            });
            #[cfg(not(windows))]
            let name = self.device_name.clone();
            let (device, config) = open_cpal(name.as_deref())?;
            let size = ring_size_for(config.sample_rate());
            let held = self.ring_producer.as_ref().map(|p| p.buffer().capacity());
            if held.is_some_and(|held| size > held) && self.ring_consumer.is_some() {
                let (producer, consumer) = RingBuffer::new(size);
                self.ring_producer = Some(producer);
                self.ring_consumer = Some(consumer);
            }
            self.cpal = Some((device, config));
        }
        let config = &self.cpal.as_ref().ok_or("no cpal device")?.1;
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        let format = config.sample_format();

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
        let (device, device_config) = self.cpal.as_ref().ok_or("no cpal device")?;
        let stream_config = StreamConfig {
            channels,
            sample_rate,
            // Capture wants *tolerance*, not a small buffer: see
            // device::capture_buffer_size.
            buffer_size: crate::device::capture_buffer_size(device, device_config),
        };

        let mut pump = crate::native::pump::CapturePump {
            producer: self
                .ring_producer
                .take()
                .ok_or("Ring producer already taken")?,
            wake_tx: self.wake_tx.take().ok_or("Wake sender already taken")?,
            stats: self.stats.clone(),
            config: self.config.clone(),
        };
        let stats_err = self.stats.clone();
        let ch = channels as usize;
        // cpal's capture thread runs our callback; promote it on first
        // entry (see platform::promote_callback_thread).
        let mut promoted = false;

        let stream = device.build_input_stream(
            stream_config,
            move |data: &[T], _: &_| {
                if !promoted {
                    promoted = true;
                    crate::platform::promote_callback_thread(FRAME_SIZE as u32, sample_rate);
                }
                // Downmixed into the ring in one chunked write, and the
                // worker woken (`pump`).
                #[allow(clippy::cast_precision_loss)]
                pump.push_mono(
                    data.len() / ch,
                    data.chunks_exact(ch).map(|frame| {
                        frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32
                    }),
                );
            },
            // Counted, not logged: this runs on the stream's own thread,
            // real-time on some hosts, where a log line is a file write;
            // the stats line has the count, the engine's reopen line the
            // loss. Nor counted as overruns, which the data path counts
            // itself — AAudio's xruns were counted twice (review 2026-10,
            // P3-33).
            move |err| {
                if matches!(err.kind(), cpal::ErrorKind::DeviceNotAvailable) {
                    stats_err.flag(crate::stats::Health::InputFailed);
                } else {
                    stats_err.input_errors.fetch_add(1, Ordering::Relaxed);
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

        let chain = build_chain(&config, self.is_speaking.clone());
        let is_speaking = self.is_speaking.clone();

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
                is_speaking,
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
        #[cfg(any(target_os = "android", target_os = "ios"))]
        if let Some(n) = self.native_voice.as_ref() {
            return Some(n.device_name.clone());
        }
        crate::device_name(&self.cpal.as_ref()?.0)
    }
}

/// The microphone's chain with this input's settings, driving
/// `is_speaking`.
fn build_chain(config: &InputConfig, is_speaking: Arc<SpeakingFlag>) -> InputChain {
    InputChain::new(
        NoiseGate::new(config.gate.clone(), 200),
        AutoGainControl::new(30.0),
        Compressor::with_defaults(),
        SpeakingIndicator::with_flag(is_speaking),
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // one straight-line real-time loop
fn worker_loop(
    mut ring_consumer: Consumer<f32>,
    wake_rx: Wait,
    should_stop: Arc<AtomicBool>,
    stats: Arc<AudioStats>,
    encoder_tx: Sender<PcmFrame>,
    loopback_tx: Sender<PcmFrame>,
    config: Arc<InputConfig>,
    mut chain: InputChain,
    is_speaking: Arc<SpeakingFlag>,
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

    let mut muted = false;
    while !should_stop.load(Ordering::Relaxed) {
        // Muted, or (the desktop) out of a voice room, where the relay
        // would drop it anyway: nothing is denoised, encoded or sent
        // (review P1-6).
        if config.silenced() {
            if !muted {
                // What it was saying goes with it: the meters would
                // hold their last values.
                stats
                    .denoised_level
                    .store(0.0_f32.to_bits(), Ordering::Relaxed);
                stats
                    .pipeline_level
                    .store(0.0_f32.to_bits(), Ordering::Relaxed);
            }
            muted = true;
            is_speaking.set(false);
            // Discard whatever landed while muted.
            let stale = ring_consumer.slots();
            if let Ok(chunk) = ring_consumer.read_chunk(stale) {
                chunk.commit_all();
            }
            // The capture callback stops waking us while muted, so this
            // waits out the timeout, nothing processed. Unmuting costs
            // nothing — the callback's next (unmuted) buffer rings at
            // once — and a stop rings too, so it is long: five wakes a
            // second for as long as one stayed muted or in the lobby
            // was the 200 ms safety wait (review 2026-10, P3-24).
            if !wake_rx.wait(SILENCED_WAIT) {
                break;
            }
            continue;
        }
        if muted {
            // The unmute edge: a clean chain, as if the microphone had
            // just opened. Once here, not on every pass while muted —
            // a reset rebuilds RNNoise's whole state (review P1-4).
            muted = false;
            chain.reset();
            accumulator.clear();
            resampler_accum.clear();
        }

        #[allow(clippy::redundant_closure_for_method_calls)] // keeps Self/T inference explicit
        let target = resampler
            .as_ref()
            .map_or(FRAME_SIZE, |r| r.input_frames_next());

        let want = target.min(read_buf.len());
        let (popped, _) = ring_consumer.pop_partial_slice(&mut read_buf[..want]);
        let n_read = popped.len();

        if n_read == 0 {
            // Block until the capture callback delivers more audio. The
            // callback rings on every buffer, so the timeout is only a
            // safety net, not a tick (review P1-4).
            if !wake_rx.wait(crate::platform::LATCH_SAFETY_WAIT) {
                break;
            }
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
                input_gain: f32::from_bits(config.input_gain.load(Ordering::Relaxed)),
                ..FrameContext::default()
            };

            chain.process(&mut frame, &mut ctx);

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
            if !config.transmit.load(Ordering::Relaxed) {
                continue;
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

/// The capture worker's wait while silenced: only a backstop, since
/// unmuting and stopping both ring it.
const SILENCED_WAIT: Duration = Duration::from_secs(5);

/// The ring between the device and the worker: 200 ms at the device
/// rate, never under 9600 samples.
fn ring_size_for(sample_rate: u32) -> usize {
    (sample_rate as usize * 200 / 1000).max(9600)
}

/// cpal's device for `device_name` (`None` = the default) and the
/// config it opens at (48 kHz when it can, [`crate::device`]).
fn open_cpal(
    device_name: Option<&str>,
) -> Result<(Device, SupportedStreamConfig), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = if let Some(name) = device_name {
        host.input_devices()?
            .find(|d| crate::device_name(d).as_deref() == Some(name))
            .ok_or_else(|| format!("Input device '{name}' not found"))?
    } else {
        host.default_input_device()
            .ok_or("No default input device available")?
    };
    let config = crate::device::preferred_input_config(&device)
        .map_err(|e| format!("Failed to get input config: {e}"))?;
    info!(
        "Input device: {} @ {}Hz, {}ch, {:?}",
        crate::device_name(&device).unwrap_or_else(|| "Unknown".into()),
        config.sample_rate(),
        config.channels(),
        config.sample_format(),
    );
    Ok((device, config))
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        if let Some(ring) = &self.stop_ring {
            ring.ring();
        }
        self.stream.take();
        #[cfg(windows)]
        self.native.take();
        #[cfg(any(target_os = "android", target_os = "ios"))]
        self.native_voice.take();
        // With the stream's sender gone too the latch is dead, and the
        // worker's wait returns at once (but on iOS, where a detached
        // half lingers in the voice unit until swept: the ring above).
        self.stop_ring.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}

/// The native capture stream would not open and the warning was given;
/// it is said again once one has opened.
#[cfg(windows)]
static CAPTURE_FALLBACK_SAID: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossbeam_channel::{bounded, Receiver};

    use super::*;

    /// The capture worker on a ring and latch of our own, fed the way a
    /// backend feeds it.
    struct Rig {
        producer: Producer<f32>,
        wake: Option<Ring>,
        frames: Receiver<PcmFrame>,
        config: Arc<InputConfig>,
        stop: Arc<AtomicBool>,
        worker: thread::JoinHandle<()>,
    }

    fn rig() -> Rig {
        let (producer, consumer) = RingBuffer::new(ring_size_for(OPUS_SAMPLE_RATE));
        let (wake, wake_rx) = crate::latch::latch();
        let (encoder_tx, frames) = bounded(64);
        let (loopback_tx, _) = bounded(8);
        let config = Arc::new(InputConfig::default());
        let speaking = SpeakingFlag::new().0;
        let chain = build_chain(&config, speaking.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (config, stop) = (config.clone(), stop.clone());
            thread::spawn(move || {
                worker_loop(
                    consumer,
                    wake_rx,
                    stop,
                    Arc::new(AudioStats::new()),
                    encoder_tx,
                    loopback_tx,
                    config,
                    chain,
                    speaking,
                    OPUS_SAMPLE_RATE,
                );
            })
        };
        Rig {
            producer,
            wake: Some(wake),
            frames,
            config,
            stop,
            worker,
        }
    }

    impl Rig {
        /// `ms` of a tone into the ring, then the callback's ring.
        fn speak(&mut self, ms: usize) {
            for i in 0..OPUS_SAMPLE_RATE as usize * ms / 1000 {
                #[allow(clippy::cast_precision_loss)]
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3;
                let _ = self.producer.push(s);
            }
            if let Some(wake) = &self.wake {
                wake.ring();
            }
        }

        /// Stop the way `AudioInput`'s drop does; how long the worker
        /// took to leave.
        fn stop(mut self, ring: bool) -> Duration {
            self.stop.store(true, Ordering::Relaxed);
            let started = Instant::now();
            if ring {
                if let Some(wake) = &self.wake {
                    wake.ring();
                }
            } else {
                self.wake.take();
            }
            self.worker.join().unwrap();
            started.elapsed()
        }
    }

    /// Review P1-6: out of a room nothing reaches the encoder; back in,
    /// it flows again at once.
    #[test]
    fn out_of_a_room_nothing_is_encoded() {
        let mut rig = rig();
        rig.config.in_room.store(false, Ordering::Relaxed);
        rig.speak(100);
        thread::sleep(Duration::from_millis(100));
        assert!(rig.frames.try_recv().is_err(), "encoded out of a room");

        rig.config.in_room.store(true, Ordering::Relaxed);
        rig.speak(100);
        let frame = rig.frames.recv_timeout(Duration::from_secs(2));
        assert!(frame.is_ok(), "nothing encoded back in the room");
        rig.stop(true);
    }

    /// Review P1-4: the worker waits on its latch with a long safety
    /// timeout, so a stop must not wait it out — rung, or with the latch
    /// gone.
    #[test]
    fn a_stop_does_not_wait_out_the_safety_timeout() {
        for ring in [true, false] {
            let mut rig = rig();
            rig.config.muted.store(true, Ordering::Relaxed);
            rig.speak(20);
            thread::sleep(Duration::from_millis(50));
            let took = rig.stop(ring);
            assert!(
                took < Duration::from_millis(100),
                "{took:?} to stop (ring: {ring})"
            );
        }
    }
}
