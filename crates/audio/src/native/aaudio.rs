//! Android's voice-call streams on `AAudio` directly (`ndk::audio`), the
//! one thing cpal's `AAudio` host cannot open: it has no way to say
//! *usage* or *input preset*, and without `VoiceCommunication` on both
//! Android never runs its echo canceller on our microphone
//! (`AEC-SPEC.md` §9, `ANDROID-SPEC.md` §6.1–6.2).
//!
//! Used only when the session wants the voice-call path
//! (`aec::wants_aec`); otherwise — headphones, or the setting Off — the
//! cpal path runs exactly as before. Both sides honour the
//! `native/mod.rs` contract: capture pushes device-rate mono `f32` into
//! the ring and pings the wake latch; render pops mono, fans it out,
//! reports its callback size for the playout cushion (D24) and pings
//! the latch; both raise `*_stream_failed` on a disconnect (a headset
//! plugged in, a phone call taking the device) so `VoiceChat::service`
//! reopens them.
//!
//! The data callbacks run on `AAudio`'s real-time thread: no locks, no
//! allocation, no logging there. Their state reaches them through a
//! one-slot channel only once the stream has opened *and* started (the
//! first callbacks before that play or drop silence), so a refusal at
//! either step hands the ring back untouched for the cpal fallback.

use std::{
    slice,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
};

use crossbeam_channel::{bounded, Receiver, Sender};
use log::{info, warn};
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioError, AudioFormat,
    AudioInputPreset, AudioPerformanceMode, AudioSharingMode, AudioStream, AudioStreamBuilder,
    AudioStreamDataCallback, AudioUsage,
};
use rtrb::{Consumer, Producer};

use crate::{input_config::InputConfig, stats::AudioStats, OPUS_SAMPLE_RATE};

/// Largest render callback served in one piece from the scratch
/// buffer; bigger ones are served in pieces.
const RENDER_SCRATCH: usize = 8192;

/// An open, started `AAudio` stream; stops and closes on drop.
struct Running(AudioStream);

// SAFETY: `AudioStream` is `!Send` only because it holds a raw pointer.
// AAudio's control calls may be made from any thread (one at a time —
// this wrapper is owned, never shared), and the callbacks it owns are
// `Send`. The same reasoning as cpal's AAudio `Stream`.
unsafe impl Send for Running {}

impl Drop for Running {
    fn drop(&mut self) {
        // Stop first so the close that follows (AudioStream's own drop)
        // never races a running callback.
        let _ = self.0.request_stop();
    }
}

/// A builder with everything the voice-call path needs.
fn voice_builder(direction: AudioDirection) -> Result<AudioStreamBuilder, AudioError> {
    let builder = AudioStreamBuilder::new()?
        .direction(direction)
        .format(AudioFormat::PCM_Float)
        .channel_count(1)
        .sample_rate(OPUS_SAMPLE_RATE as i32)
        .sharing_mode(AudioSharingMode::Shared)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .usage(AudioUsage::VoiceCommunication);
    Ok(match direction {
        AudioDirection::Input => builder.input_preset(AudioInputPreset::VoiceCommunication),
        _ => builder.content_type(AudioContentType::Speech),
    })
}

/// What the stream actually got, for the log line.
fn describe(stream: &AudioStream) -> String {
    format!(
        "{} Hz, {}ch, {:?}, {:?}, burst {} frames, device {}",
        stream.sample_rate(),
        stream.channel_count(),
        stream.performance_mode(),
        stream.sharing_mode(),
        stream.frames_per_burst(),
        stream.device_id(),
    )
}

fn positive_u32(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

// ── Capture ─────────────────────────────────────────────────────────────

/// A running voice-call capture stream. Dropping it stops it.
pub struct NativeCapture {
    _stream: Running,
    /// Rate of the samples arriving in the ring.
    pub sample_rate: u32,
    /// What the log calls it.
    pub device_name: String,
    /// The stream really carries the `VoiceCommunication` preset (`AAudio`
    /// may substitute on an odd device).
    pub voice_preset: bool,
}

/// Handed back when the stream could not be opened, so the cpal path
/// can use the ring and the latch instead.
pub struct CaptureStartError {
    pub reason: String,
    pub producer: Producer<f32>,
    pub wake_tx: Sender<()>,
}

struct CaptureParts {
    producer: Producer<f32>,
    wake_tx: Sender<()>,
    stats: Arc<AudioStats>,
    config: Arc<InputConfig>,
    input_level: Arc<AtomicU32>,
    channels: usize,
}

impl CaptureParts {
    /// One callback's worth of interleaved `f32` → mono into the ring.
    fn push(&mut self, data: &[f32]) {
        if self.config.muted.load(Ordering::Relaxed) {
            self.input_level.store(0.0_f32.to_bits(), Ordering::Relaxed);
            return;
        }
        let ch = self.channels.max(1);
        let frames = data.len() / ch;
        let want = frames.min(self.producer.slots());
        if want < frames {
            self.stats.input_overruns.fetch_add(1, Ordering::Relaxed);
        }
        if want == 0 {
            return;
        }
        let mut rms_acc = 0.0f32;
        if let Ok(chunk) = self.producer.write_chunk_uninit(want) {
            let mut it = data.chunks_exact(ch).take(want).map(|frame| {
                #[allow(clippy::cast_precision_loss)]
                let mono = frame.iter().sum::<f32>() / ch as f32;
                rms_acc += mono * mono;
                mono
            });
            chunk.fill_from_iter(&mut it);
        }
        #[allow(clippy::cast_precision_loss)]
        let rms = (rms_acc / want as f32).sqrt();
        self.input_level
            .store((rms * 3.0).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        let _ = self.wake_tx.try_send(());
    }
}

impl NativeCapture {
    /// Open the microphone on the voice-call path and start pumping into
    /// `producer`. The phone must already be in communication mode
    /// (`android::CommunicationMode`) for the platform chain to engage.
    ///
    /// # Errors
    /// The open was refused; the ring and latch come back in the error.
    pub fn start(
        producer: Producer<f32>,
        wake_tx: Sender<()>,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
        input_level: Arc<AtomicU32>,
    ) -> Result<Self, CaptureStartError> {
        let (parts_tx, parts_rx) = bounded::<CaptureParts>(1);
        let failed = stats.clone();
        let opened = voice_builder(AudioDirection::Input).and_then(|b| {
            b.data_callback(capture_callback(parts_rx))
                .error_callback(Box::new(move |_stream, error| {
                    // Not the real-time thread: AAudio calls this from
                    // its own, and only for disconnects and the like.
                    failed.input_stream_failed.store(true, Ordering::Relaxed);
                    warn!("AAudio voice capture error: {error:?}");
                }))
                .open_stream()
        });
        let stream = match opened {
            Ok(s) => s,
            Err(e) => {
                return Err(CaptureStartError {
                    reason: format!("AAudio voice capture refused: {e:?}"),
                    producer,
                    wake_tx,
                })
            }
        };

        if let Err(e) = stream.request_start() {
            return Err(CaptureStartError {
                reason: format!("AAudio voice capture start: {e:?}"),
                producer,
                wake_tx,
            });
        }
        let channels = usize::try_from(stream.channel_count()).unwrap_or(1).max(1);
        let sample_rate = positive_u32(stream.sample_rate());
        let voice_preset = stream.input_preset() == AudioInputPreset::VoiceCommunication;
        info!(
            "AAudio voice capture: {}, preset {:?}",
            describe(&stream),
            stream.input_preset()
        );
        let device_name = format!("voice microphone (device {})", stream.device_id());
        // The channel is empty and ours: this cannot fail.
        let _ = parts_tx.send(CaptureParts {
            producer,
            wake_tx,
            stats,
            config,
            input_level,
            channels,
        });
        Ok(Self {
            _stream: Running(stream),
            sample_rate,
            device_name,
            voice_preset,
        })
    }
}

fn capture_callback(parts_rx: Receiver<CaptureParts>) -> AudioStreamDataCallback {
    let mut parts: Option<CaptureParts> = None;
    Box::new(move |_stream, data, num_frames| {
        if parts.is_none() {
            parts = parts_rx.try_recv().ok();
        }
        if let Some(p) = parts.as_mut() {
            let n = usize::try_from(num_frames).unwrap_or(0) * p.channels;
            if n > 0 && !data.is_null() {
                // SAFETY: AAudio hands `num_frames × channel_count`
                // samples in the format the stream was opened with
                // (PCM_Float), valid for the duration of the callback.
                let samples = unsafe { slice::from_raw_parts(data.cast::<f32>(), n) };
                p.push(samples);
            }
        }
        AudioCallbackResult::Continue
    })
}

// ── Render ──────────────────────────────────────────────────────────────

/// A running voice-call render stream. Dropping it stops it.
pub struct NativeRender {
    _stream: Running,
    /// Rate the ring must be filled at.
    pub sample_rate: u32,
    pub device_name: String,
}

/// Handed back when the stream could not be opened.
pub struct RenderStartError {
    pub reason: String,
    pub consumer: Consumer<f32>,
    pub wake_tx: Sender<()>,
}

struct RenderParts {
    consumer: Consumer<f32>,
    wake_tx: Sender<()>,
    stats: Arc<AudioStats>,
    callback_frames: Arc<AtomicUsize>,
    largest_callback: usize,
    channels: usize,
    scratch: Vec<f32>,
}

impl RenderParts {
    /// Fill one callback's interleaved buffer from the mono ring.
    fn pull(&mut self, out: &mut [f32]) {
        let ch = self.channels.max(1);
        let frames_total = out.len() / ch;
        // The worker keeps one callback + margin queued (D24); tell it
        // when the device starts asking for more.
        if frames_total > self.largest_callback {
            self.largest_callback = frames_total;
            self.callback_frames.store(frames_total, Ordering::Relaxed);
        }
        let mut rms_acc = 0.0f32;
        let mut done = 0usize;
        while done < frames_total {
            let want = (frames_total - done).min(self.scratch.len());
            let mono = &mut self.scratch[..want];
            let (_, missing) = self.consumer.pop_partial_slice(mono);
            if !missing.is_empty() {
                self.stats.output_underruns.fetch_add(1, Ordering::Relaxed);
                missing.fill(0.0);
            }
            for (i, &s) in mono.iter().enumerate() {
                rms_acc += s * s;
                let base = (done + i) * ch;
                // Mono to every channel: a phone's voice path is mono
                // or a stereo pair; there is no surround to protect.
                out[base..base + ch].fill(s);
            }
            done += want;
        }
        if frames_total > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rms = (rms_acc / frames_total as f32).sqrt();
            self.stats
                .output_level
                .store((rms * 3.0).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
        let _ = self.wake_tx.try_send(());
    }
}

impl NativeRender {
    /// Open the speaker on the voice-call path and start pulling from
    /// `consumer`.
    ///
    /// # Errors
    /// The open was refused; the ring and latch come back in the error.
    pub fn start(
        consumer: Consumer<f32>,
        wake_tx: Sender<()>,
        stats: Arc<AudioStats>,
        callback_frames: Arc<AtomicUsize>,
    ) -> Result<Self, RenderStartError> {
        let (parts_tx, parts_rx) = bounded::<RenderParts>(1);
        let failed = stats.clone();
        let opened = voice_builder(AudioDirection::Output).and_then(|b| {
            b.data_callback(render_callback(parts_rx))
                .error_callback(Box::new(move |_stream, error| {
                    failed.output_stream_failed.store(true, Ordering::Relaxed);
                    warn!("AAudio voice render error: {error:?}");
                }))
                .open_stream()
        });
        let stream = match opened {
            Ok(s) => s,
            Err(e) => {
                return Err(RenderStartError {
                    reason: format!("AAudio voice render refused: {e:?}"),
                    consumer,
                    wake_tx,
                })
            }
        };

        if let Err(e) = stream.request_start() {
            return Err(RenderStartError {
                reason: format!("AAudio voice render start: {e:?}"),
                consumer,
                wake_tx,
            });
        }
        let channels = usize::try_from(stream.channel_count()).unwrap_or(1).max(1);
        let sample_rate = positive_u32(stream.sample_rate());
        info!(
            "AAudio voice render: {}, usage {:?}",
            describe(&stream),
            stream.usage()
        );
        let device_name = format!("voice speaker (device {})", stream.device_id());
        let _ = parts_tx.send(RenderParts {
            consumer,
            wake_tx,
            stats,
            callback_frames,
            largest_callback: 0,
            channels,
            scratch: vec![0.0; RENDER_SCRATCH],
        });
        Ok(Self {
            _stream: Running(stream),
            sample_rate,
            device_name,
        })
    }
}

fn render_callback(parts_rx: Receiver<RenderParts>) -> AudioStreamDataCallback {
    let mut parts: Option<RenderParts> = None;
    Box::new(move |_stream, data, num_frames| {
        if parts.is_none() {
            parts = parts_rx.try_recv().ok();
        }
        let channels = parts.as_ref().map_or(1, |p| p.channels);
        let n = usize::try_from(num_frames).unwrap_or(0) * channels;
        if n == 0 || data.is_null() {
            return AudioCallbackResult::Continue;
        }
        // SAFETY: AAudio hands a writable buffer of `num_frames ×
        // channel_count` samples in the opened format (PCM_Float),
        // valid for the duration of the callback.
        let out = unsafe { slice::from_raw_parts_mut(data.cast::<f32>(), n) };
        match parts.as_mut() {
            Some(p) => p.pull(out),
            None => out.fill(0.0),
        }
        AudioCallbackResult::Continue
    })
}
