//! Stereo audio playback for screen share system audio.
//!
//! Runs alongside the mono voice chat `AudioOutput` as a completely
//! independent playback pipeline. Receives decoded stereo frames,
//! resamples to the output device rate, and plays back through the
//! output device.
//!
//! ## Why separate from `AudioOutput`?
//!
//! - Voice chat is mono; screen share audio is stereo (game panning, music)
//! - Independent volume controls (voice vs stream audio)
//! - Deafening voice shouldn't kill screen share audio
//! - No jitter buffer complexity needed — system audio is continuous,
//!   not bursty like multi-peer voice chat
//!
//! The exception is a phone on the voice-call path: there a second,
//! media output beside the call has the wrong volume, the wrong route and
//! no echo cancellation, so the stream goes into the voice output's mix
//! instead (`stream::mix`; the decoder chooses, and keeps volume, mute
//! and "deafen leaves it alone" the same either way).
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
//! decoder thread:  Opus decode (stereo) → resample to the device rate → ring
//! device callback: ring → paced reading (`stream::pace`) → volume → device
//! ```
//!
//! No thread of its own: the decoder pushes each frame into the ring as
//! it decodes it, and the device's callback reads the ring the way the
//! phones' mix does — a prebuffer (60 ms on the desktop, the phones'
//! 100 ms there: `Pacing::own_output`), a backlog drained by playing
//! slightly faster, a trim past 400 ms. A worker used to sit between the two and
//! wake every 5 ms for as long as a stream was watched, silence included
//! (review P1-4), and on a device not at 48 kHz it held a backlog for
//! good (PRODUCTION-PLAN.md CLI-14).

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, Stream, SupportedStreamConfig,
};
use dasp_sample::{FromSample, Sample as DaspSample, ToSample};
use log::{error, info};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, FixedAsync, Resampler};

use crate::{
    stream::pace::{PacedReader, Pacing},
    OPUS_SAMPLE_RATE,
};

/// Stereo.
const CHANNELS: usize = 2;

/// What the ring may hold: 500 ms, like a source on the phones' mix
/// (the reader trims past 400 ms).
const RING_MS: usize = 500;

/// Frames one callback is served in at a time from the scratch buffer;
/// a bigger callback is served in pieces.
const CALLBACK_SCRATCH: usize = 4096;

/// The resampler's input chunk: one decoded frame (20 ms at 48 kHz).
const RESAMPLE_CHUNK: usize = 960;

/// Stereo audio output for screen share streams.
///
/// The decoder thread owns it: [`Self::push`] queues decoded stereo at
/// 48 kHz, the device callback plays it.
pub struct StreamAudioOutput {
    device: Device,
    device_config: SupportedStreamConfig,
    stream: Option<Stream>,

    /// The decoder's side of the ring.
    writer: Writer,
    /// The callback's side, until `start` hands it over.
    consumer: Option<Consumer<f32>>,

    // Controls
    /// Stream audio volume (f32 bits, 0.0–2.0), applied in the callback.
    volume: Arc<AtomicU32>,
    /// Mute stream audio without stopping the pipeline.
    is_muted: Arc<AtomicBool>,
    /// The device went away or the stream must be rebuilt
    /// (`DeviceNotAvailable`, `StreamInvalidated`: a reroute, an unplugged
    /// headset): the stream is dead and the owner reopens it.
    failed: Arc<AtomicBool>,
}

impl StreamAudioOutput {
    /// Open the stream's output on `device_name` (`None` = the
    /// default), with volume (f32 bits, 0.0–2.0) and mute owned by the
    /// caller: the stream decoder keeps them across the outputs it opens
    /// and reopens.
    ///
    /// # Errors
    /// The device is not there, or will not say how it plays.
    pub fn with_controls(
        device_name: Option<&str>,
        volume: Arc<AtomicU32>,
        is_muted: Arc<AtomicBool>,
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

        let (writer, consumer) = Writer::new(sample_rate)?;
        Ok(Self {
            device,
            device_config,
            stream: None,
            writer,
            consumer: Some(consumer),
            volume,
            is_muted,
            failed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Audio waiting to be played, ms: the ring (not the device's own
    /// buffer). For the viewer's log — the audio half of A/V sync.
    #[must_use]
    pub fn queued_ms(&self) -> u32 {
        self.writer.queued_ms()
    }

    /// Whether the device went away under the stream (see `failed`).
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Open the device stream; it plays what [`Self::push`] queues.
    ///
    /// # Errors
    /// The device refused the stream, or plays a format we do not.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sample_rate = self.device_config.sample_rate();
        let channels = self.device_config.channels();
        let format = self.device_config.sample_format();

        let stream = match format {
            SampleFormat::I16 => self.build_stream::<i16>(sample_rate, channels)?,
            SampleFormat::U16 => self.build_stream::<u16>(sample_rate, channels)?,
            SampleFormat::F32 => self.build_stream::<f32>(sample_rate, channels)?,
            _ => return Err(format!("Unsupported format: {format:?}").into()),
        };

        stream.play()?;
        self.stream = Some(stream);
        // Windows lowers what is not part of a call while one runs, and
        // this output is not: ask it to leave ours alone (AEC-SPEC.md
        // E14).
        #[cfg(windows)]
        crate::native::wasapi::keep_own_volume_on(crate::device_name(&self.device).as_deref());
        info!("Stream audio output started");
        Ok(())
    }

    /// Queue one decoded frame (interleaved stereo, 48 kHz). What does
    /// not fit — a device that stopped reading for half a second — is
    /// dropped.
    pub fn push(&mut self, stereo: &[f32]) {
        self.writer.push(stereo);
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

        let consumer = self.consumer.take().ok_or("Ring consumer already taken")?;
        let mut reader = PacedReader::<CHANNELS>::new(consumer, Pacing::own_output(sample_rate));
        // Allocated here, never in the callback.
        let mut scratch = vec![0.0f32; CALLBACK_SCRATCH * CHANNELS];
        let ch = usize::from(channels).max(1);
        let volume = self.volume.clone();
        let is_muted = self.is_muted.clone();
        let failed = self.failed.clone();
        // cpal's render thread runs this callback at its own priority,
        // below a game's audio: promoted on first entry like the voice
        // output's (review P3-26; `promote_callback_thread`).
        let mut promoted = false;

        let stream = self.device.build_output_stream(
            config,
            move |data: &mut [T], _: &_| {
                if !promoted {
                    promoted = true;
                    crate::platform::promote_callback_thread(480, sample_rate);
                }
                let gain = if is_muted.load(Ordering::Relaxed) {
                    // Muted still reads: the queue must not grow.
                    0.0
                } else {
                    f32::from_bits(volume.load(Ordering::Relaxed))
                };
                let frames_total = data.len() / ch;
                let mut done = 0;
                while done < frames_total {
                    let n = (frames_total - done).min(CALLBACK_SCRATCH);
                    let stereo = &mut scratch[..n * CHANNELS];
                    stereo.fill(0.0);
                    reader.add_to(stereo);
                    let out = &mut data[done * ch..(done + n) * ch];
                    write_frames(stereo, gain, out, ch);
                    done += n;
                }
            },
            move |err| {
                // AAudio's "Disconnected" after a reroute, an unplugged
                // device, a stream the backend says must be rebuilt: this
                // one will not play again; the decoder opens a new one
                // (`stream::decoder`).
                if matches!(
                    err.kind(),
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
                ) {
                    failed.store(true, Ordering::Relaxed);
                }
                error!("Stream audio output error: {err}");
            },
            None,
        )?;

        Ok(stream)
    }
}

impl Drop for StreamAudioOutput {
    fn drop(&mut self) {
        // Stops the device's callbacks, and with them the reading.
        self.stream.take();
        info!("Stream audio output dropped");
    }
}

/// Stereo frames, volume applied, into the device's `ch` channels: one
/// channel gets the average, more get left and right and silence after.
fn write_frames<T>(stereo: &[f32], gain: f32, out: &mut [T], ch: usize)
where
    T: DaspSample + FromSample<f32>,
{
    for (pair, frame) in stereo.chunks_exact(CHANNELS).zip(out.chunks_exact_mut(ch)) {
        let left = (pair[0] * gain).clamp(-0.99, 0.99);
        let right = (pair[1] * gain).clamp(-0.99, 0.99);
        match frame {
            [mono] => *mono = T::from_sample((left + right) * 0.5),
            [l, r, rest @ ..] => {
                *l = T::from_sample(left);
                *r = T::from_sample(right);
                for slot in rest {
                    *slot = T::EQUILIBRIUM;
                }
            }
            [] => {}
        }
    }
}

// ── The decoder's side ──────────────────────────────────────────────────

/// Pushes decoded stereo into the ring at the device's rate, through a
/// resampler when the device is not at 48 kHz. Whole frames only: a
/// split pair would swap left and right for good.
struct Writer {
    producer: Producer<f32>,
    /// Ring size, in samples.
    capacity: usize,
    device_rate: u32,
    resample: Option<Resample>,
}

/// 48 kHz → the device's rate, fed one decoded frame at a time.
struct Resample {
    resampler: Async<f32>,
    /// Left and right waiting for a whole chunk (less than one).
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    /// The output interleaved again, for the ring.
    interleaved: Vec<f32>,
}

impl Writer {
    fn new(device_rate: u32) -> Result<(Self, Consumer<f32>), Box<dyn std::error::Error>> {
        let capacity = (device_rate as usize * RING_MS / 1000).max(1) * CHANNELS;
        let (producer, consumer) = RingBuffer::new(capacity);
        let resample = if device_rate == OPUS_SAMPLE_RATE {
            None
        } else {
            let resampler = Async::<f32>::new_sinc(
                f64::from(device_rate) / f64::from(OPUS_SAMPLE_RATE),
                1.0,
                &crate::resampler::sinc_params(),
                RESAMPLE_CHUNK,
                CHANNELS,
                FixedAsync::Input,
            )?;
            let out_max = resampler.output_frames_max();
            info!("Stream audio resampler: 48kHz → {device_rate}Hz");
            Some(Resample {
                resampler,
                input: (0..CHANNELS)
                    .map(|_| Vec::with_capacity(2 * RESAMPLE_CHUNK))
                    .collect(),
                output: vec![vec![0.0; out_max]; CHANNELS],
                interleaved: Vec::with_capacity(out_max * CHANNELS),
            })
        };
        Ok((
            Self {
                producer,
                capacity,
                device_rate,
                resample,
            },
            consumer,
        ))
    }

    fn push(&mut self, stereo: &[f32]) {
        let Some(rs) = self.resample.as_mut() else {
            push_frames(&mut self.producer, stereo);
            return;
        };
        for pair in stereo.chunks_exact(CHANNELS) {
            rs.input[0].push(pair[0]);
            rs.input[1].push(pair[1]);
        }
        while rs.input[0].len() >= rs.resampler.input_frames_next() {
            match rs.chunk() {
                Ok(()) => push_frames(&mut self.producer, &rs.interleaved),
                Err(e) => {
                    error!("Stream audio resampler: {e}");
                    for side in &mut rs.input {
                        side.clear();
                    }
                    break;
                }
            }
        }
    }

    /// The ring's fill, plus what waits for the resampler, in ms.
    fn queued_ms(&self) -> u32 {
        let ring = (self.capacity - self.producer.slots()) / CHANNELS;
        let pending = self.resample.as_ref().map_or(0, |rs| rs.input[0].len());
        let ms = ring as u64 * 1000 / u64::from(self.device_rate.max(1))
            + pending as u64 * 1000 / u64::from(OPUS_SAMPLE_RATE);
        u32::try_from(ms).unwrap_or(u32::MAX)
    }
}

impl Resample {
    /// Resample the next input chunk into `interleaved`.
    fn chunk(&mut self) -> Result<(), String> {
        let needed = self.resampler.input_frames_next();
        let expected = self.resampler.output_frames_next();
        let input = SequentialSliceOfVecs::new(&self.input, CHANNELS, needed)
            .map_err(|e| format!("input: {e}"))?;
        let mut output = SequentialSliceOfVecs::new_mut(&mut self.output, CHANNELS, expected)
            .map_err(|e| format!("output: {e}"))?;
        let (used, produced) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| e.to_string())?;
        for side in &mut self.input {
            side.drain(..used.min(side.len()));
        }
        self.interleaved.clear();
        for (l, r) in self.output[0][..produced]
            .iter()
            .zip(&self.output[1][..produced])
        {
            self.interleaved.push(*l);
            self.interleaved.push(*r);
        }
        Ok(())
    }
}

/// Push whole interleaved frames, as many as fit.
fn push_frames(producer: &mut Producer<f32>, interleaved: &[f32]) {
    let fits = interleaved.len().min(producer.slots()) / CHANNELS * CHANNELS;
    let _ = producer.push_partial_slice(&interleaved[..fits]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 ms of a 440 Hz tone, interleaved stereo at 48 kHz, left and
    /// right in opposite phase.
    fn frame() -> Vec<f32> {
        (0..RESAMPLE_CHUNK)
            .flat_map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                [s, -s]
            })
            .collect()
    }

    #[test]
    fn at_48k_frames_go_straight_into_the_ring() {
        let (mut writer, consumer) = Writer::new(48_000).unwrap();
        writer.push(&frame());
        assert_eq!(consumer.slots(), RESAMPLE_CHUNK * CHANNELS);
        assert_eq!(writer.queued_ms(), 20);
    }

    /// CLI-14's device: what is decoded is in the ring at the device's
    /// rate right away, where the callback's reader can shed a backlog —
    /// nothing piles up in front of it.
    #[test]
    fn at_44k1_frames_are_resampled_into_the_ring() {
        let (mut writer, consumer) = Writer::new(44_100).unwrap();
        for _ in 0..20 {
            writer.push(&frame());
        }
        // 400 ms at 44.1 kHz.
        let frames = consumer.slots() / CHANNELS;
        assert!((17_500..=17_700).contains(&frames), "{frames} frames");
        assert_eq!(consumer.slots() % CHANNELS, 0, "whole frames only");
        let pending = writer.resample.as_ref().unwrap().input[0].len();
        assert!(pending < RESAMPLE_CHUNK, "{pending} frames held back");
        assert!((390..=410).contains(&writer.queued_ms()));
    }

    #[test]
    fn a_full_ring_takes_whole_frames_only() {
        let (mut producer, consumer) = RingBuffer::<f32>::new(5);
        push_frames(&mut producer, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        assert_eq!(consumer.slots(), 4);
    }

    #[test]
    fn devices_get_the_stereo_they_can_take() {
        let stereo = [0.5, -0.5, 0.25, 0.75];
        let same = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        let mut mono = [0.0f32; 2];
        write_frames(&stereo, 1.0, &mut mono, 1);
        assert!(same(&mono, &[0.0, 0.5]), "{mono:?}");
        let mut surround = [1.0f32; 8];
        write_frames(&stereo, 2.0, &mut surround, 4);
        assert!(
            same(&surround, &[0.99, -0.99, 0.0, 0.0, 0.5, 0.99, 0.0, 0.0]),
            "{surround:?}"
        );
    }
}
