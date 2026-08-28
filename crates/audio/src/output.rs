//! Audio output: packet buffers → decode at pull → mix → gain → limit → resample → playback.
//!
//! Asks the device for 48 kHz first (see [`crate::device`]); only devices
//! that cannot do 48 kHz get a resampler on this path.

use crate::config::PeerAudioSettings;
use crate::processor::limiter::Limiter;
use crate::resampler::build_output_resampler;
use crate::sounds::{SoundBank, SoundType};
use crate::stats::AudioStats;
use crate::transport::jitter_buffer::AdaptiveJitterBuffer;
use crate::transport::packet::AudioPacket;
use crate::{PcmFrame, OPUS_SAMPLE_RATE};

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, SupportedStreamConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{debug, error};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, Resampler};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Samples pulled from the jitter buffer per hop (10 ms at 48 kHz). The
/// codec frame is 20 ms; the packet buffers stage decoded PCM so the hop
/// and the frame stay decoupled.
const HOP: usize = 480;

/// Largest single playback callback we serve from the scratch buffer.
/// Anything bigger is served in pieces (never happens with a 10–20 ms
/// device period, but cpal does not promise one).
const CALLBACK_SCRATCH: usize = 8192;

pub struct AudioOutput {
    device: Device,
    device_config: SupportedStreamConfig,
    stream: Option<Stream>,
    packet_rx: Option<Receiver<(AudioPacket, Instant)>>,
    loopback_rx: Option<Receiver<PcmFrame>>,
    sound_rx: Option<Receiver<SoundType>>,

    ring_producer: Option<Producer<f32>>,
    ring_consumer: Option<Consumer<f32>>,
    ring_capacity: usize,

    /// Wake latch: playback callback → worker ("ring space freed").
    wake_tx: Option<Sender<()>>,
    wake_rx: Option<Receiver<()>>,

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

        debug!(
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
            stream: None,
            ring_producer: Some(ring_producer),
            ring_consumer: Some(ring_consumer),
            ring_capacity: ring_size,
            wake_tx: Some(wake_tx),
            wake_rx: Some(wake_rx),
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

    pub fn output_gain_handle(&self) -> Arc<AtomicU32> {
        self.output_gain.clone()
    }

    pub fn deafened_handle(&self) -> Arc<AtomicBool> {
        self.deafened.clone()
    }

    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate();
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        // Start producer worker BEFORE the stream so it's ready when callbacks arrive
        if let (Some(ring_producer), Some(packet_rx), Some(loopback_rx), Some(sound_rx), Some(wake_rx)) = (
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
                peer_settings: self.peer_settings.clone(),
                removed_peers: self.removed_peers.clone(),
                stats: self.stats.clone(),
                should_stop: self.should_stop.clone(),
                ring_capacity: self.ring_capacity,
                device_sample_rate: sample_rate,
                output_gain: self.output_gain.clone(),
                deafened: self.deafened.clone(),
            });
            self.worker_thread = Some(handle);
        }

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
        // Allocated once, outside the callback.
        let mut mono = vec![0.0f32; CALLBACK_SCRATCH];

        let stream = self.device.build_output_stream(
            config,
            move |data: &mut [T], _: &_| {
                let mut rms_acc = 0.0f32;
                let mut frames_done = 0usize;
                let frames_total = data.len() / ch;

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
                        // Duplicate mono to all output channels
                        let base = (frames_done + frame_idx) * ch;
                        for c in 0..ch {
                            data[base + c] = T::from_sample(sample);
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
                error!("Output stream error: {err}");
                stats_err.output_underruns.fetch_add(1, Ordering::Relaxed);
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
    let mut hop_buf = vec![0.0f32; HOP];
    let mut resample_accum: Vec<f32> = Vec::with_capacity(2048);
    let mut output_limiter = Limiter::new();
    let mut next_tick = Instant::now();

    // Audio-class scheduling: MMCSS "Pro Audio" on Windows, real-time
    // policy on macOS/Linux. This thread now decodes every peer, so it
    // needs the priority more than it used to.
    #[allow(clippy::cast_possible_truncation)]
    let _rt = crate::platform::promote_current_thread(HOP as u32, OPUS_SAMPLE_RATE);

    debug!(
        "Output worker started (device {}Hz, resample={})",
        device_sample_rate, needs_resample
    );
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
        // Target: 20ms of audio in the ring at device sample rate — two
        // device periods of cushion with event-driven wakeups. Shrink
        // further only after output_underruns stays flat in the field.
        let target_fill = (device_sample_rate as usize * 20) / 1000;
        let max_hops_per_tick = 6; // cap to stay responsive

        let t_hop = Instant::now();
        for _ in 0..max_hops_per_tick {
            // Stop if ring is sufficiently full
            let current_fill = ring_capacity - ring_producer.slots();
            if current_fill >= target_fill || current_fill >= high_wm {
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
                for s in hop_buf.iter_mut() {
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

                    let in_adapter = match SequentialSliceOfVecs::new(&resampler_input, 1, needed)
                    {
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
                            let _ = ring_producer.push_partial_slice(&resampler_output[0][..out_len]);
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

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        // Drop the cpal stream FIRST — stops platform audio callbacks so
        // they can't deadlock pulling from the ring buffer while the
        // worker thread is being joined.
        self.stream.take();
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}
