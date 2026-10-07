//! Audio output: packet buffers → decode at pull → mix → gain → limit → resample → playback.
//!
//! Asks the device for 48 kHz first (see [`crate::device`]); only devices
//! that cannot do 48 kHz get a resampler on this path.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex,
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
use log::{debug, error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, Resampler};

use crate::{
    config::PeerAudioSettings,
    processor::limiter::Limiter,
    resampler::build_output_resampler,
    sounds::{SoundBank, SoundType},
    stats::AudioStats,
    transport::{jitter_buffer::AdaptiveJitterBuffer, packet::AudioPacket},
    PcmFrame, OPUS_SAMPLE_RATE,
};

/// Samples pulled from the jitter buffer per hop (10 ms at 48 kHz). The
/// codec frame is 20 ms; the packet buffers stage decoded PCM so the hop
/// and the frame stay decoupled.
const HOP: usize = 480;

/// Ring fill the worker keeps ahead of the device when callbacks are
/// small (≤ 10 ms): two desktop periods of cushion.
const BASE_FILL_MS: usize = 20;
/// What the worker keeps beyond one callback when callbacks are large:
/// the time it has to wake up and refill after each one.
const REFILL_MARGIN_MS: usize = 10;

/// Largest single playback callback we serve from the scratch buffer.
/// Anything bigger is served in pieces (never happens with a 10–20 ms
/// device period, but cpal does not promise one).
const CALLBACK_SCRATCH: usize = 8192;

pub struct AudioOutput {
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
    native: Option<crate::native::wasapi::NativeRender>,
    /// The voice-call stream (Android `native::aaudio`, iOS
    /// `native::vpio`), when the session wants the platform's echo
    /// canceller and it opened.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    native_voice: Option<crate::native::VoiceRender>,
    /// Open on the platform's voice-call path (`aec::wants_aec`).
    #[cfg_attr(
        not(any(windows, target_os = "android", target_os = "ios")),
        allow(dead_code)
    )]
    voice: bool,
    packet_rx: Option<Receiver<(AudioPacket, Instant)>>,
    loopback_rx: Option<Receiver<PcmFrame>>,
    sound_rx: Option<Receiver<SoundType>>,

    ring_producer: Option<Producer<f32>>,
    ring_consumer: Option<Consumer<f32>>,
    ring_capacity: usize,

    /// Wake latch: playback callback → worker ("ring space freed").
    wake_tx: Option<Sender<()>>,
    wake_rx: Option<Receiver<()>>,
    /// Largest playback callback seen, in device frames (0 = none yet,
    /// or a backend that does not report — the native WASAPI render).
    /// The worker sizes its ring fill and the jitter floor from it.
    callback_frames: Arc<AtomicUsize>,

    // Worker
    worker_thread: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,

    // Settings
    peer_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,
    /// Master output gain (f32 bits). Applied after mixing.
    output_gain: Arc<AtomicU32>,
    /// When true, the worker pushes silence instead of decoding/mixing.
    deafened: Arc<AtomicBool>,

    // Stats
    stats: Arc<AudioStats>,
}

impl AudioOutput {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_name: Option<&str>,
        packet_rx: Receiver<(AudioPacket, Instant)>,
        loopback_rx: Receiver<PcmFrame>,
        sound_rx: Receiver<SoundType>,
        peer_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
        removed_peers: Arc<Mutex<HashSet<u64>>>,
        stats: Arc<AudioStats>,
        output_gain: f32,
        voice: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();

        let device = if let Some(name) = device_name {
            host.output_devices()?
                .find(|d| crate::device_name(d).as_deref() == Some(name))
                .ok_or_else(|| format!("Output device '{name}' not found"))?
        } else {
            host.default_output_device()
                .ok_or("No default output device available")?
        };

        let device_config = crate::device::preferred_output_config(&device)
            .map_err(|e| format!("Failed to get output config: {e}"))?;

        let sample_rate = device_config.sample_rate();

        info!(
            "Output device: {} @ {}Hz, {}ch, {:?}",
            crate::device_name(&device).unwrap_or_else(|| "Unknown".into()),
            sample_rate,
            device_config.channels(),
            device_config.sample_format(),
        );

        // Ring buffer: 200ms at device rate, mono
        let ring_size = ((sample_rate as usize) * 200) / 1000;

        let ring_size = ring_size.max(9600);
        let (ring_producer, ring_consumer) = RingBuffer::new(ring_size);
        // Wake latch: playback callback pings this after draining the ring
        // so the worker refills promptly instead of waiting out its tick.
        let (wake_tx, wake_rx) = bounded::<()>(1);

        Ok(Self {
            device,
            device_config,
            #[cfg(windows)]
            device_name: device_name.map(String::from),
            stream: None,
            #[cfg(windows)]
            native: None,
            #[cfg(any(target_os = "android", target_os = "ios"))]
            native_voice: None,
            voice,
            ring_producer: Some(ring_producer),
            ring_consumer: Some(ring_consumer),
            ring_capacity: ring_size,
            wake_tx: Some(wake_tx),
            wake_rx: Some(wake_rx),
            callback_frames: Arc::new(AtomicUsize::new(0)),
            worker_thread: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            peer_settings,
            removed_peers,
            output_gain: Arc::new(AtomicU32::new(output_gain.to_bits())),
            deafened: Arc::new(AtomicBool::new(false)),
            stats,
            packet_rx: Some(packet_rx),
            loopback_rx: Some(loopback_rx),
            sound_rx: Some(sound_rx),
        })
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
        crate::device_name(&self.device)
    }

    pub fn output_gain_handle(&self) -> Arc<AtomicU32> {
        self.output_gain.clone()
    }

    pub fn deafened_handle(&self) -> Arc<AtomicBool> {
        self.deafened.clone()
    }

    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.stats
            .output_stream_failed
            .store(false, Ordering::Relaxed);

        #[cfg(windows)]
        {
            match self.start_native() {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!("Native WASAPI render unavailable ({e}); using cpal"),
            }
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        if self.voice {
            match self.start_native_voice() {
                Ok(()) => return Ok(()),
                Err(e) => log::warn!("Voice-call playback unavailable ({e}); using cpal"),
            }
        }
        self.start_cpal()
    }

    /// Android and iOS: the speaker on the voice-call path
    /// (`native::aaudio`, `native::vpio`), the render half — the
    /// reference — of what the platform's echo canceller needs. On
    /// failure the ring and latch go back for the cpal path.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    fn start_native_voice(&mut self) -> Result<(), String> {
        let consumer = self
            .ring_consumer
            .take()
            .ok_or("ring consumer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("wake sender already taken")?;
        // Prime before the stream starts pulling (the rate is the one we
        // ask for; AAudio and VPIO convert if the device runs at another).
        self.prime_ring(crate::OPUS_SAMPLE_RATE);
        match crate::native::VoiceRender::start(
            consumer,
            wake_tx,
            self.stats.clone(),
            self.callback_frames.clone(),
        ) {
            Ok(native) => {
                let rate = native.sample_rate;
                self.native_voice = Some(native);
                self.spawn_worker_for(rate);
                Ok(())
            }
            Err(err) => {
                self.ring_consumer = Some(err.consumer);
                self.wake_tx = Some(err.wake_tx);
                Err(err.reason)
            }
        }
    }

    /// Windows: play through our own MMCSS thread (see `native::wasapi`).
    /// The engine converts from 48 kHz for us, so the worker normally runs
    /// without a resampler. On failure the ring and latch are handed back
    /// untouched for the cpal path.
    #[cfg(windows)]
    fn start_native(&mut self) -> Result<(), String> {
        let consumer = self
            .ring_consumer
            .take()
            .ok_or("ring consumer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("wake sender already taken")?;
        match crate::native::wasapi::NativeRender::start(
            self.device_name.as_deref(),
            self.voice,
            consumer,
            wake_tx,
            self.stats.clone(),
        ) {
            Ok(native) => {
                let rate = native.sample_rate;
                self.native = Some(native);
                self.prime_ring(rate);
                self.spawn_worker_for(rate);
                Ok(())
            }
            Err(err) => {
                self.ring_consumer = Some(err.consumer);
                self.wake_tx = Some(err.wake_tx);
                Err(err.reason)
            }
        }
    }

    fn start_cpal(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate();
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        self.prime_ring(sample_rate);
        // Start producer worker BEFORE the stream so it's ready when callbacks arrive
        self.spawn_worker_for(sample_rate);

        let stream = match format {
            SampleFormat::I16 => self.build_stream::<i16>(sample_rate, channels)?,
            SampleFormat::U16 => self.build_stream::<u16>(sample_rate, channels)?,
            SampleFormat::F32 => self.build_stream::<f32>(sample_rate, channels)?,
            _ => return Err(format!("Unsupported format: {format:?}").into()),
        };

        stream.play()?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Prime the ring with one target fill of silence so the first
    /// device period isn't a guaranteed underrun ("Underruns 1" forever).
    fn prime_ring(&mut self, sample_rate: u32) {
        if let Some(producer) = self.ring_producer.as_mut() {
            let silence = vec![0.0f32; (sample_rate as usize * 20) / 1000];
            let _ = producer.push_partial_slice(&silence);
        }
    }

    /// Spawn the mix/decode worker filling the ring at `device_sample_rate`.
    fn spawn_worker_for(&mut self, device_sample_rate: u32) {
        if let (
            Some(ring_producer),
            Some(packet_rx),
            Some(loopback_rx),
            Some(sound_rx),
            Some(wake_rx),
        ) = (
            self.ring_producer.take(),
            self.packet_rx.take(),
            self.loopback_rx.take(),
            self.sound_rx.take(),
            self.wake_rx.take(),
        ) {
            let handle = Self::spawn_worker(WorkerParts {
                ring_producer,
                packet_rx,
                loopback_rx,
                sound_rx,
                wake_rx,
                callback_frames: self.callback_frames.clone(),
                peer_settings: self.peer_settings.clone(),
                removed_peers: self.removed_peers.clone(),
                stats: self.stats.clone(),
                should_stop: self.should_stop.clone(),
                ring_capacity: self.ring_capacity,
                device_sample_rate,
                output_gain: self.output_gain.clone(),
                deafened: self.deafened.clone(),
            });
            self.worker_thread = Some(handle);
        }
    }

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
            // ~10 ms when the device advertises a supported range;
            // untouched (Default) when it doesn't. See platform.rs.
            buffer_size: crate::platform::request_low_latency_buffer(
                self.device_config.buffer_size(),
                sample_rate,
            ),
        };

        let mut ring_consumer = self
            .ring_consumer
            .take()
            .ok_or("Ring consumer already taken")?;
        let wake_tx = self.wake_tx.take().ok_or("Wake sender already taken")?;
        let stats = self.stats.clone();
        let stats_err = self.stats.clone();
        let ch = channels as usize;
        let callback_frames = self.callback_frames.clone();
        let mut largest_callback = 0usize;
        // Allocated once, outside the callback.
        let mut mono = vec![0.0f32; CALLBACK_SCRATCH];
        // cpal's render thread runs our callback; promote it on first entry.
        let mut promoted = false;
        let mut last_err_log: Option<Instant> = None;

        let stream = self.device.build_output_stream(
            config,
            move |data: &mut [T], _: &_| {
                if !promoted {
                    promoted = true;
                    #[allow(clippy::cast_possible_truncation)]
                    crate::platform::promote_callback_thread(HOP as u32, sample_rate);
                }
                let mut rms_acc = 0.0f32;
                let mut frames_done = 0usize;
                let frames_total = data.len() / ch;
                // The worker must keep at least this much queued; tell
                // it when the device starts asking for more (AAudio
                // picks its own callback size, and may vary it).
                if frames_total > largest_callback {
                    largest_callback = frames_total;
                    callback_frames.store(frames_total, Ordering::Relaxed);
                }

                while frames_done < frames_total {
                    let want = (frames_total - frames_done).min(mono.len());
                    // One chunked read instead of a pop per sample. Whatever
                    // the ring cannot supply is an underrun and plays as
                    // silence. The signal was limited to ±0.99 upstream, so
                    // no per-sample clamp here.
                    let (_popped, missing) = ring_consumer.pop_partial_slice(&mut mono[..want]);
                    if !missing.is_empty() {
                        stats.output_underruns.fetch_add(1, Ordering::Relaxed);
                        missing.fill(0.0);
                    }
                    for (frame_idx, &sample) in mono[..want].iter().enumerate() {
                        rms_acc += sample * sample;
                        let base = (frames_done + frame_idx) * ch;
                        let s = T::from_sample(sample);
                        // Mono → the front pair only. On 5.1/7.1 endpoints
                        // (gaming headsets with virtual surround report 8
                        // channels) the remaining channels are centre, LFE,
                        // rear and side; feeding them a copy of the voice
                        // makes the driver's binaural fold-down bass-heavy
                        // and smeared. Standard channel order puts FL, FR
                        // first on WASAPI, CoreAudio and ALSA; cpal doesn't
                        // expose the mask, so that order is assumed.
                        data[base] = s;
                        if ch > 1 {
                            data[base + 1] = s;
                        }
                        for slot in &mut data[base + 2.min(ch)..base + ch] {
                            *slot = T::EQUILIBRIUM;
                        }
                    }
                    frames_done += want;
                }

                // Output level for VU meter
                if frames_total > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let rms = (rms_acc / frames_total as f32).sqrt();
                    let scaled = (rms * 3.0).clamp(0.0, 1.0);
                    stats
                        .output_level
                        .store(scaled.to_bits(), Ordering::Relaxed);
                }

                // Ring space just freed — wake the worker to refill.
                let _ = wake_tx.try_send(());
            },
            move |err| {
                if matches!(err.kind(), cpal::ErrorKind::DeviceNotAvailable) {
                    stats_err
                        .output_stream_failed
                        .store(true, Ordering::Relaxed);
                }
                stats_err.output_underruns.fetch_add(1, Ordering::Relaxed);
                let now = Instant::now();
                if last_err_log.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
                    last_err_log = Some(now);
                    error!(
                        "Output stream error: {err} (total {})",
                        stats_err.output_underruns.load(Ordering::Relaxed)
                    );
                }
            },
            None,
        )?;

        Ok(stream)
    }

    fn spawn_worker(parts: WorkerParts) -> thread::JoinHandle<()> {
        crate::platform::spawn_named("voice-output", move || worker_loop(parts))
    }
}

/// Everything the output worker owns, bundled so `spawn_worker` stays
/// under the argument-count lint without losing the names.
struct WorkerParts {
    ring_producer: Producer<f32>,
    packet_rx: Receiver<(AudioPacket, Instant)>,
    loopback_rx: Receiver<PcmFrame>,
    sound_rx: Receiver<SoundType>,
    wake_rx: Receiver<()>,
    callback_frames: Arc<AtomicUsize>,
    peer_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,
    stats: Arc<AudioStats>,
    should_stop: Arc<AtomicBool>,
    ring_capacity: usize,
    device_sample_rate: u32,
    output_gain: Arc<AtomicU32>,
    deafened: Arc<AtomicBool>,
}

#[allow(clippy::too_many_lines)] // one straight-line real-time loop
fn worker_loop(parts: WorkerParts) {
    let WorkerParts {
        mut ring_producer,
        packet_rx,
        loopback_rx,
        sound_rx,
        wake_rx,
        callback_frames,
        peer_settings,
        removed_peers,
        stats,
        should_stop,
        ring_capacity,
        device_sample_rate,
        output_gain,
        deafened,
    } = parts;

    let mut jitter_buffer =
        AdaptiveJitterBuffer::new(OPUS_SAMPLE_RATE as usize, 10, peer_settings, removed_peers);

    // Decode all notification sounds once at startup
    let sound_bank = SoundBank::new();

    // Currently playing notification: (which, cursor into the bank's PCM)
    let mut playing: Option<(SoundType, usize)> = None;

    // Build output resampler EAGERLY (not in the audio callback)
    let needs_resample = device_sample_rate != OPUS_SAMPLE_RATE;
    let mut resampler: Option<Async<f32>> = None;
    let mut resampler_input: Vec<Vec<f32>> = Vec::new();
    let mut resampler_output: Vec<Vec<f32>> = Vec::new();

    if needs_resample {
        let chunk = crate::resampler::frames_for_ms(device_sample_rate, 10);
        match build_output_resampler(device_sample_rate, chunk, 1) {
            Ok(r) => {
                resampler_input = vec![Vec::with_capacity(r.input_frames_next() + 64)];
                resampler_output = vec![vec![0.0f32; chunk * 2]];
                resampler = Some(r);
            }
            Err(e) => error!("Failed to create output resampler: {e}"),
        }
    }

    let high_wm = ring_capacity * 85 / 100;
    // Ring fill to keep ahead of the device, and how many hops one pass
    // may produce; both follow the callback size (see `Cushion`).
    let mut cushion = Cushion::for_callback(0, device_sample_rate, high_wm);
    let mut known_callback = 0usize;
    let mut hop_buf = vec![0.0f32; HOP];
    let mut resample_accum: Vec<f32> = Vec::with_capacity(2048);
    let mut output_limiter = Limiter::new();
    let mut next_tick = Instant::now();

    // Audio-class scheduling: MMCSS "Pro Audio" on Windows, real-time
    // policy on macOS/Linux, nice −16 on Android. This thread now
    // decodes every peer, so it needs the priority more than it used to.
    #[allow(clippy::cast_possible_truncation)]
    let _rt = crate::platform::promote_current_thread(HOP as u32, OPUS_SAMPLE_RATE);

    debug!("Output worker started (device {device_sample_rate}Hz, resample={needs_resample})");
    while !should_stop.load(Ordering::Relaxed) {
        // Drain ALL inbound packets into the jitter buffer. Arrival was
        // stamped at intake; the packet buffers sort by sequence.
        while let Ok((packet, arrival)) = packet_rx.try_recv() {
            jitter_buffer.push_packet(packet, arrival);
        }
        while let Ok(frame) = loopback_rx.try_recv() {
            jitter_buffer.push_loopback(&frame);
        }

        // Check for new notification sounds (latest wins)
        while let Ok(sound) = sound_rx.try_recv() {
            if !sound_bank.get(sound).is_empty() {
                playing = Some((sound, 0));
                log::debug!("Playing notification: {sound:?}");
            }
        }

        // Fill the ring up to a target level, not just one hop.
        // The cpal callback drains the ring at the device's hardware
        // clock rate, which may be slightly faster than our 10ms tick.
        // By filling multiple hops per tick when the ring is low, we
        // absorb OS scheduler jitter and clock drift without underruns.
        //
        // The target follows the device's callback size: 20 ms (two
        // desktop periods) while callbacks are ≤ 10 ms, one callback +
        // 10 ms beyond that. A fixed 20 ms against Android's 40 ms
        // callbacks left every callback half silence (2026-10-05).
        let seen = callback_frames.load(Ordering::Relaxed);
        if seen != known_callback {
            known_callback = seen;
            cushion = Cushion::for_callback(seen, device_sample_rate, high_wm);
            jitter_buffer.set_pull_burst(cushion.burst_48k, HOP);
            let ms = |frames: usize| frames as f32 * 1000.0 / device_sample_rate as f32;
            info!(
                "Output callbacks of {seen} frames ({:.1} ms): keeping {:.1} ms queued, jitter floor {:.1} ms",
                ms(seen),
                ms(cushion.target_fill),
                jitter_buffer.min_target_ms(),
            );
        }

        let t_hop = Instant::now();
        for _ in 0..cushion.max_hops {
            // Stop if ring is sufficiently full
            let current_fill = ring_capacity - ring_producer.slots();
            if current_fill >= cushion.target_fill || current_fill >= high_wm {
                break;
            }

            // Pull 10 ms from the jitter buffer (decoding as it goes).
            // Deafened still pulls: the buffers must keep draining and
            // the decoders must keep state; the audio is just discarded.
            jitter_buffer.pull_mixed(HOP, &mut hop_buf);
            if deafened.load(Ordering::Relaxed) {
                hop_buf.fill(0.0);
            }

            // Mix notification sound into hop buffer (at 48 kHz,
            // before gain so it respects the master volume).
            // Notifications play even when deafened — you should
            // still hear join/leave events.
            let mut sound_finished = false;
            if let Some((sound, cursor)) = playing.as_mut() {
                let samples = sound_bank.get(*sound);
                let remaining = samples.len().saturating_sub(*cursor);
                let to_mix = remaining.min(HOP);
                for (slot, &s) in hop_buf[..to_mix].iter_mut().zip(&samples[*cursor..]) {
                    // No clamp here: the limiter downstream is the
                    // one place that controls the summed level.
                    *slot += s;
                }
                *cursor += to_mix;
                sound_finished = *cursor >= samples.len();
            }
            if sound_finished {
                playing = None;
            }

            // Apply master output gain (always — notifications
            // need gain even when deafened).
            // Gain BEFORE the limiter: the limiter must see the
            // level the listener will hear, otherwise a gain > 1
            // re-amplifies the limited signal straight into a hard
            // clip and the "limiter" audibly does nothing.
            let gain = f32::from_bits(output_gain.load(Ordering::Relaxed));
            if (gain - 1.0).abs() > f32::EPSILON {
                for s in &mut hop_buf {
                    *s *= gain;
                }
            }
            output_limiter.process_samples(&mut hop_buf);

            if let Some(ref mut r) = resampler {
                resample_accum.extend_from_slice(&hop_buf);

                while resample_accum.len() >= r.input_frames_next() {
                    let needed = r.input_frames_next();
                    resampler_input[0].clear();
                    resampler_input[0].extend_from_slice(&resample_accum[..needed]);
                    resample_accum.drain(..needed);

                    let expected = r.output_frames_next();
                    resampler_output[0].resize(expected, 0.0);

                    let in_adapter = match SequentialSliceOfVecs::new(&resampler_input, 1, needed) {
                        Ok(a) => a,
                        Err(e) => {
                            error!("Output resampler adapter error: {e}");
                            break;
                        }
                    };
                    let mut out_adapter =
                        match SequentialSliceOfVecs::new_mut(&mut resampler_output, 1, expected) {
                            Ok(a) => a,
                            Err(e) => {
                                error!("Output resampler output adapter error: {e}");
                                break;
                            }
                        };

                    match r.process_into_buffer(&in_adapter, &mut out_adapter, None) {
                        Ok((_in_len, out_len)) => {
                            // Chunked push; the ring-full remainder is dropped
                            // (the fill gate above makes that rare).
                            let _ =
                                ring_producer.push_partial_slice(&resampler_output[0][..out_len]);
                        }
                        Err(e) => {
                            error!("Output resampler error: {e}");
                            break;
                        }
                    }
                }
            } else {
                let _ = ring_producer.push_partial_slice(&hop_buf);
            }
        }
        // Wall time of one refill pass (decode + mix + limit). Replaces
        // the old per-packet decode timing; the same `last_decode_us`
        // slot is reused so the debug overlay keeps working.
        #[allow(clippy::cast_possible_truncation)]
        stats
            .last_decode_us
            .store(t_hop.elapsed().as_micros() as u64, Ordering::Relaxed);

        // ── Tick-gated bookkeeping (strict 10 ms cadence) ───
        // The worker may wake *more* often than every 10 ms (the
        // playback callback pings the latch as it drains the ring —
        // extra wakes are harmless because production is gated on ring
        // fill above). adapt() is tuned for a 10 ms cadence, so it stays
        // on the tick, not on wakeups.
        let now = Instant::now();
        if now >= next_tick {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                stats
                    .jitter_buffer_occupancy_ms
                    .store(jitter_buffer.occupancy_ms() as usize, Ordering::Relaxed);
                stats
                    .jitter_buffer_target_ms
                    .store(jitter_buffer.target_ms() as usize, Ordering::Relaxed);
            }
            let js = jitter_buffer.drain_stats();
            if js.fec > 0 {
                stats.fec_recoveries.fetch_add(js.fec, Ordering::Relaxed);
            }
            if js.plc > 0 {
                stats.plc_frames.fetch_add(js.plc, Ordering::Relaxed);
            }
            if js.lost > 0 {
                stats.packets_lost.fetch_add(js.lost, Ordering::Relaxed);
            }
            if js.late > 0 {
                stats.packets_late.fetch_add(js.late, Ordering::Relaxed);
            }
            if js.red > 0 {
                stats.red_recoveries.fetch_add(js.red, Ordering::Relaxed);
            }
            jitter_buffer.adapt();

            next_tick += Duration::from_millis(10);
            if next_tick <= now {
                // Fell behind (scheduler stall) — resynchronize.
                next_tick = now + Duration::from_millis(10);
            }
        }

        // Wait for the device to consume audio, or for the tick —
        // whichever comes first. This replaces a bare sleep that
        // Windows could stretch to 15+ ms.
        let wait = next_tick.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            crate::platform::wait_for_wake(&wake_rx, wait);
        }
    }

    debug!("Output worker stopped");
}

/// How far ahead of the device the worker keeps the ring, for a given
/// playback callback size. Pure arithmetic, tested below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cushion {
    /// Ring fill target, device frames.
    target_fill: usize,
    /// Hops one refill pass may produce (enough to refill a whole
    /// callback in one wake, plus one).
    max_hops: usize,
    /// One callback in 48 kHz samples — the jitter buffer's pull burst.
    burst_48k: usize,
}

impl Cushion {
    /// `callback` in device frames (0 = unknown: the desktop default).
    fn for_callback(callback: usize, device_rate: u32, high_wm: usize) -> Self {
        let rate = device_rate as usize;
        let base = rate * BASE_FILL_MS / 1000;
        let with_margin = callback + rate * REFILL_MARGIN_MS / 1000;
        let target_fill = base.max(with_margin).min(high_wm);
        let target_48k = target_fill * OPUS_SAMPLE_RATE as usize / rate.max(1);
        Self {
            target_fill,
            max_hops: (target_48k.div_ceil(HOP) + 1).max(6),
            burst_48k: callback * OPUS_SAMPLE_RATE as usize / rate.max(1),
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        // Drop the cpal stream FIRST — stops platform audio callbacks so
        // they can't deadlock pulling from the ring buffer while the
        // worker thread is being joined.
        self.stream.take();
        #[cfg(windows)]
        self.native.take();
        #[cfg(any(target_os = "android", target_os = "ios"))]
        self.native_voice.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIGH_WM: usize = 9600 * 85 / 100;

    #[test]
    fn desktop_periods_keep_the_old_cushion() {
        // Unknown, 10 ms and a hair over: 20 ms, six hops, as before.
        for cb in [0, 480] {
            let c = Cushion::for_callback(cb, 48_000, HIGH_WM);
            assert_eq!(c.target_fill, 960, "callback {cb}");
            assert_eq!(c.max_hops, 6);
        }
        assert_eq!(Cushion::for_callback(512, 48_000, HIGH_WM).target_fill, 992);
    }

    #[test]
    fn large_callbacks_get_one_callback_plus_margin() {
        // Android AAudio, normal mode: 40 ms callbacks.
        let c = Cushion::for_callback(1920, 48_000, HIGH_WM);
        assert_eq!(c.target_fill, 1920 + 480);
        assert!(
            c.max_hops * HOP >= 1920,
            "one pass refills a whole callback"
        );
        assert_eq!(c.burst_48k, 1920);
        // 44.1 kHz device: burst converted to the jitter buffer's rate.
        let c = Cushion::for_callback(1764, 44_100, HIGH_WM);
        assert_eq!(c.burst_48k, 1920);
    }

    #[test]
    fn never_past_the_high_watermark() {
        let c = Cushion::for_callback(48_000, 48_000, HIGH_WM);
        assert_eq!(c.target_fill, HIGH_WM);
    }
}
