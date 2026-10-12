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
//! Both streams ask for performance mode *None*, and capture for an
//! audio session of its own. A low-latency stream with no session is
//! what `AAudio` puts on its MMAP path, which bypasses the platform's
//! effects altogether — the echo canceller included — on the phones
//! that have MMAP (most current ones). The legacy path costs a period
//! or two of latency; it is the path calls take. With the session, the
//! canceller is also created on it explicitly and read back
//! ([`super::android::attach_echo_canceller`]), so the verdict says what
//! the phone did (`AEC-SPEC.md` §9.3, `ANDROID-SPEC.md` D46).
//!
//! The data callbacks run on `AAudio`'s real-time thread: no locks, no
//! allocation, no logging there. Their state reaches them through a
//! one-slot channel only once the stream has opened *and* started (the
//! first callbacks before that play or drop silence), so a refusal at
//! either step hands the ring back untouched for the cpal fallback.

use std::{
    slice,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use crossbeam_channel::{bounded, Receiver};
use log::{info, warn};
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioError, AudioFormat,
    AudioInputPreset, AudioPerformanceMode, AudioSharingMode, AudioStream, AudioStreamBuilder,
    AudioStreamDataCallback, AudioUsage, SessionId,
};
use rtrb::{Consumer, Producer};

use super::{
    android::{self, EchoCanceller, EchoEffect},
    pump::{CapturePump, RenderPump},
};
use crate::{
    aec::reasons, input_config::InputConfig, latch::Ring, stats::AudioStats, OPUS_SAMPLE_RATE,
};

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

/// A builder with everything the voice-call path needs: the call's
/// usage and preset, performance mode *None* (never MMAP, never a FAST
/// track — the paths that skip the effects, see the top of the file),
/// and for capture a freshly allocated session the canceller is then
/// attached to.
fn voice_builder(direction: AudioDirection) -> Result<AudioStreamBuilder, AudioError> {
    let builder = AudioStreamBuilder::new()?
        .direction(direction)
        .format(AudioFormat::PCM_Float)
        .channel_count(1)
        .sample_rate(OPUS_SAMPLE_RATE as i32)
        .sharing_mode(AudioSharingMode::Shared)
        .performance_mode(AudioPerformanceMode::None)
        .usage(AudioUsage::VoiceCommunication);
    Ok(match direction {
        AudioDirection::Input => builder
            .input_preset(AudioInputPreset::VoiceCommunication)
            // `None` = allocate one when the stream opens.
            .session_id(None),
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

/// A running voice-call capture stream. Dropping it stops the stream,
/// then releases the echo canceller (field order).
pub struct NativeCapture {
    _stream: Running,
    _echo: Option<EchoCanceller>,
    /// Rate of the samples arriving in the ring.
    pub sample_rate: u32,
    /// What the log calls it.
    pub device_name: String,
    /// Whether the platform canceller is on this microphone, and if not
    /// why (`aec::reasons`): the preset has to have stuck and the
    /// canceller has to be on the session.
    pub aec_verdict: Result<(), &'static str>,
}

/// Handed back when the stream could not be opened, so the cpal path
/// can use the ring and the latch instead.
pub struct CaptureStartError {
    pub reason: String,
    pub producer: Producer<f32>,
    pub wake_tx: Ring,
}

struct CaptureParts {
    pump: CapturePump,
    channels: usize,
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
        wake_tx: Ring,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
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
        let session = match stream.session_id() {
            SessionId::Allocated(id) => Some(id.get()),
            SessionId::None => None,
        };
        info!(
            "AAudio voice capture: {}, preset {:?}, session {}",
            describe(&stream),
            stream.input_preset(),
            session.map_or_else(|| "none".to_string(), |id| id.to_string()),
        );
        let device_name = format!("voice microphone (device {})", stream.device_id());
        // The channel is empty and ours: this cannot fail. Sent before
        // the canceller's JNI round trips, so capture is not held up by
        // them.
        let _ = parts_tx.send(CaptureParts {
            pump: CapturePump {
                producer,
                wake_tx,
                stats,
                config,
            },
            channels,
        });
        let (echo, aec_verdict) = if voice_preset {
            echo_canceller_on(session)
        } else {
            warn!("AAudio voice capture: the voice preset did not stick; no echo cancellation");
            (None, Err(reasons::PATH_REFUSED))
        };
        Ok(Self {
            _stream: Running(stream),
            _echo: echo,
            sample_rate,
            device_name,
            aec_verdict,
        })
    }
}

/// Put the canceller on the capture session and say whether it runs
/// (one log line either way). Without a session, or with no effect to
/// create, the answer is `AcousticEchoCanceler.isAvailable()`: a phone
/// with a canceller is taken to run it on the voice path (the HAL often
/// does, without exposing an effect); one without has none.
fn echo_canceller_on(session: Option<i32>) -> (Option<EchoCanceller>, Result<(), &'static str>) {
    let fallback = |why: &str| {
        let verdict = match android::echo_canceller_available() {
            Ok(true) => Ok(()),
            Ok(false) => Err(reasons::NO_CANCELLER),
            Err(e) => {
                log::debug!("AcousticEchoCanceler.isAvailable: {e}");
                Ok(())
            }
        };
        info!(
            "AAudio voice capture: echo canceller {} ({why})",
            if verdict.is_ok() {
                "assumed on the voice path"
            } else {
                "not on this phone"
            }
        );
        verdict
    };
    let Some(id) = session else {
        return (None, fallback("no audio session to put it on"));
    };
    match android::attach_echo_canceller(id) {
        Ok(EchoEffect::On(effect)) => {
            info!("AAudio voice capture: echo canceller on (AcousticEchoCanceler, session {id})");
            (Some(effect), Ok(()))
        }
        Ok(EchoEffect::Off(effect, status)) => {
            warn!(
                "AAudio voice capture: echo canceller would not switch on \
                 (AcousticEchoCanceler.setEnabled = {status}, session {id})"
            );
            (Some(effect), Err(reasons::CANCELLER_OFF))
        }
        Ok(EchoEffect::Missing) => (None, fallback("no AcousticEchoCanceler for the session")),
        Err(e) => (None, fallback(&format!("AcousticEchoCanceler.create: {e}"))),
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
                p.pump.push(samples, p.channels);
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
    pub wake_tx: Ring,
}

struct RenderParts {
    pump: RenderPump,
    channels: usize,
}

impl NativeRender {
    /// Open the speaker on the voice-call path and start pulling from
    /// `consumer`.
    ///
    /// # Errors
    /// The open was refused; the ring and latch come back in the error.
    pub fn start(
        consumer: Consumer<f32>,
        wake_tx: Ring,
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
        // The device's burst is the callback it will ask for: size the
        // playout cushion (D24) for it now, rather than learn it from
        // the first callbacks' underruns (4–6 at every join on a Galaxy
        // S24 FE, whose speaker asks for 1280 frames).
        let burst = usize::try_from(stream.frames_per_burst()).unwrap_or(0);
        callback_frames.fetch_max(burst, Ordering::Relaxed);
        let device_name = format!("voice speaker (device {})", stream.device_id());
        let _ = parts_tx.send(RenderParts {
            pump: RenderPump::new(consumer, wake_tx, stats, callback_frames),
            channels,
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
            Some(p) => p.pump.pull(out, p.channels),
            None => out.fill(0.0),
        }
        AudioCallbackResult::Continue
    })
}
