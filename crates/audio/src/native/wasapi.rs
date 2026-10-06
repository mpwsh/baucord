//! WASAPI capture and render on threads we own.
//!
//! Why not cpal here: cpal's WASAPI backend runs its callback on a thread
//! it creates at `THREAD_PRIORITY_TIME_CRITICAL` — priority 15, normal
//! band — and never registers it with MMCSS. A foreground game runs 13–15
//! on every core with its own audio in the real-time band above that.
//! Field log: ~1,000 capture discontinuities per minute with Counter-Strike
//! 2 focused, zero without. The thread that reads the device was the one
//! being starved, and cpal owns it. Here we own it.
//!
//! What this backend does that the cpal path cannot:
//!
//! - The pump thread registers itself with MMCSS "Pro Audio" *before* it
//!   opens the device, and holds that registration for its whole life.
//! - Shared mode with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`: the engine
//!   delivers 48 kHz mono `f32` to capture and accepts 48 kHz `f32` for
//!   render whatever the endpoint's own format is, so the rubato resampler
//!   is idle on Windows. If the engine refuses (older builds, odd drivers),
//!   we fall back to the mix format and the worker resamples as before.
//! - Capture asks for a 50 ms engine buffer: tolerance for scheduling
//!   stalls at zero latency cost (packets are still read as they land).
//! - Render asks for a 20 ms engine buffer — two periods; that *is*
//!   latency on the render side, so it stays small.
//! - Every packet's `DATA_DISCONTINUITY` flag is counted into
//!   `AudioStats` (rate-limited log), `SILENT` packets become zeros so the
//!   timeline stays continuous, and `AUDCLNT_E_DEVICE_INVALIDATED` sets
//!   the stream-failed flag for the device supervisor instead of dying
//!   silently.
//! - [`DeviceWatcher`] registers `IMMNotificationClient` and raises the
//!   default-device-changed flags — the event that replaces polling.
//!
//! Built against the `wasapi` crate 0.24 API, the same one
//! `stream::process_loopback` already compiles with.
//!
//! **Echo cancellation** (`AEC-SPEC.md` §4): when the session wants the
//! voice-call path, both streams open in the `Communications` stream
//! category — the one Windows runs the endpoint's voice chain (the OEM
//! or inbox AEC effect) on — and "default" means the Default
//! Communications Device. Capture then points the canceller at our
//! render endpoint (Windows 11 22H2+) and reads the effect list back to
//! report whether an AEC effect is actually on (D5). The output's form
//! factor ([`output_kind`]) is what Auto decides by.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{bounded, Sender};
use log::{debug, error, info, warn};
use rtrb::{Consumer, Producer};
use wasapi::{
    AudioCaptureClient, AudioClient, AudioClientProperties, AudioRenderClient, DeviceEnumerator,
    DeviceEventCallbacks, DeviceEventRegistration, Direction, Handle, Role, SampleType,
    StreamCategory, StreamMode, WasapiError, WaveFormat,
};
use windows::Win32::Media::{
    Audio::AUDIO_EFFECT_STATE_ON, KernelStreaming::AUDIO_EFFECT_TYPE_ACOUSTIC_ECHO_CANCELLATION,
};

use crate::{
    aec::{self, OutputKind},
    input_config::InputConfig,
    stats::AudioStats,
    OPUS_SAMPLE_RATE,
};

/// Capture engine buffer: tolerance, not latency (see module docs).
const CAPTURE_BUFFER_HNS: i64 = 50 * 10_000;
/// Render engine buffer: two 10 ms engine periods. This one *is* latency.
const RENDER_BUFFER_HNS: i64 = 20 * 10_000;
/// Event wait timeout — also the granularity at which `should_stop` is
/// observed and at which a silent capture device is noticed.
const EVENT_WAIT_MS: u32 = 100;
/// Consecutive engine errors before a pump gives up and flags the stream.
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

// ── Errors ──────────────────────────────────────────────────────────────

/// Native capture could not start. Carries the ring and latch back so the
/// caller can hand them to the cpal fallback untouched.
pub struct CaptureStartError {
    pub reason: String,
    pub producer: Producer<f32>,
    pub wake_tx: Sender<()>,
}

/// Native render could not start; same idea as [`CaptureStartError`].
pub struct RenderStartError {
    pub reason: String,
    pub consumer: Consumer<f32>,
    pub wake_tx: Sender<()>,
}

// ── Opened stream description ───────────────────────────────────────────

/// What the engine agreed to deliver / accept.
#[derive(Debug, Clone, Copy)]
struct Negotiated {
    sample_rate: u32,
    channels: usize,
    layout: SampleLayout,
    bytes_per_frame: usize,
}

/// On-the-wire sample encoding of the opened stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleLayout {
    F32,
    I16,
    /// 24 or 32 valid bits stored in 32-bit containers, left-justified.
    I32 {
        valid_bits: u16,
    },
}

impl SampleLayout {
    fn from_format(fmt: &WaveFormat) -> Result<Self, String> {
        let bits = fmt.get_bitspersample();
        match (fmt.get_subformat(), bits) {
            (Ok(SampleType::Float), 32) => Ok(Self::F32),
            (Ok(SampleType::Int), 16) => Ok(Self::I16),
            (Ok(SampleType::Int), 32) => Ok(Self::I32 {
                valid_bits: fmt.get_validbitspersample(),
            }),
            (Ok(t), b) => Err(format!("unsupported sample layout {t}/{b}-bit")),
            (Err(e), _) => Err(format!("unknown subformat: {e}")),
        }
    }

    /// Decode one interleaved frame's worth of bytes to a mono sample
    /// (channel average).
    #[inline]
    fn frame_to_mono(self, frame: &[u8], channels: usize) -> f32 {
        let mut sum = 0.0f32;
        match self {
            Self::F32 => {
                for c in frame.chunks_exact(4) {
                    sum += f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                }
            }
            Self::I16 => {
                for c in frame.chunks_exact(2) {
                    sum += f32::from(i16::from_le_bytes([c[0], c[1]])) / 32_768.0;
                }
            }
            Self::I32 { .. } => {
                // Left-justified: the full-scale value is 2^31 regardless of
                // how many of the low bits are valid.
                for c in frame.chunks_exact(4) {
                    #[allow(clippy::cast_precision_loss)]
                    let v = i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0;
                    sum += v;
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        {
            sum / channels as f32
        }
    }

    /// Encode one sample into `dst` (exactly one channel's bytes).
    #[inline]
    fn write_sample(self, sample: f32, dst: &mut [u8]) {
        match self {
            Self::F32 => dst.copy_from_slice(&sample.to_le_bytes()),
            Self::I16 => {
                #[allow(clippy::cast_possible_truncation)]
                let v = (sample * 32_767.0).round() as i16;
                dst.copy_from_slice(&v.to_le_bytes());
            }
            Self::I32 { .. } => {
                #[allow(clippy::cast_possible_truncation)]
                let v = (f64::from(sample) * 2_147_483_647.0).round() as i32;
                dst.copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::F32 | Self::I32 { .. } => 4,
        }
    }
}

// ── Device lookup ───────────────────────────────────────────────────────

/// The device for `name`, or the default for `role` (Console as ever;
/// Communications on the voice-call path, AEC-SPEC.md D4).
fn find_device(
    enumerator: &DeviceEnumerator,
    direction: Direction,
    name: Option<&str>,
    role: Role,
) -> Result<wasapi::Device, String> {
    match name {
        None => enumerator
            .get_default_device_for_role(&direction, &role)
            .map_err(|e| format!("default {direction} device ({role}): {e}")),
        Some(name) => enumerator
            .get_device_collection(&direction)
            .map_err(|e| format!("{direction} device collection: {e}"))?
            .get_device_with_name(name)
            .map_err(|e| format!("{direction} device '{name}': {e}")),
    }
}

/// Try to open `client` at 48 kHz `f32` with engine-side conversion; if
/// the engine refuses, open it at its own mix format. Returns what was
/// actually negotiated.
fn open_client(
    client: &mut AudioClient,
    direction: Direction,
    buffer_hns: i64,
    what: &str,
) -> Result<Negotiated, String> {
    let mix = client
        .get_mixformat()
        .map_err(|e| format!("{what} mix format: {e}"))?;
    let mix_channels = usize::from(mix.get_nchannels());

    // Capture: mono — the engine's channel matrixer folds a stereo mic
    // down for us. Render: keep the endpoint's channel count so a 7.1
    // headset endpoint gets FL/FR + silence, not the matrixer's guess.
    // Channel masks matter to the matrixer: a mono capture stream is
    // KSAUDIO_SPEAKER_MONO (front centre, 0x4), and a render stream must
    // reuse the endpoint's own mask — a 7.1 endpoint is 0x63F (with side
    // channels), not the 0xFF the crate would synthesise for 8 channels.
    let (want_channels, mask) = match direction {
        Direction::Capture => (1, 0x4),
        Direction::Render => (mix_channels.max(1), mix.get_dwchannelmask()),
    };
    let wanted = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        OPUS_SAMPLE_RATE as usize,
        want_channels,
        Some(mask),
    );
    let mode = |autoconvert| StreamMode::EventsShared {
        autoconvert,
        buffer_duration_hns: buffer_hns,
    };

    match client.initialize_client(&wanted, &direction, &mode(true)) {
        Ok(()) => Ok(Negotiated {
            sample_rate: OPUS_SAMPLE_RATE,
            channels: want_channels,
            layout: SampleLayout::F32,
            bytes_per_frame: wanted.get_blockalign() as usize,
        }),
        // The client is single-use after a failed Initialize; the caller
        // re-creates it and retries at the mix format.
        Err(e) => Err(format!(
            "48 kHz f32 refused ({e}); mix format is {} Hz {mix_channels}ch",
            mix.get_samplespersec()
        )),
    }
}

/// Second attempt: the endpoint's own mix format, no conversion.
fn open_client_mix(
    client: &mut AudioClient,
    direction: Direction,
    buffer_hns: i64,
    what: &str,
) -> Result<Negotiated, String> {
    let mix = client
        .get_mixformat()
        .map_err(|e| format!("{what} mix format: {e}"))?;
    let layout = SampleLayout::from_format(&mix)?;
    client
        .initialize_client(
            &mix,
            &direction,
            &StreamMode::EventsShared {
                autoconvert: false,
                buffer_duration_hns: buffer_hns,
            },
        )
        .map_err(|e| format!("{what} init at mix format: {e}"))?;
    Ok(Negotiated {
        sample_rate: mix.get_samplespersec(),
        channels: usize::from(mix.get_nchannels()).max(1),
        layout,
        bytes_per_frame: mix.get_blockalign() as usize,
    })
}

/// A fresh client for `device`, in the `Communications` category when
/// `voice` (AEC-SPEC.md D3). The category must be set before
/// `Initialize`, so every attempt below gets its own.
fn new_client(device: &wasapi::Device, voice: bool, what: &str) -> Result<AudioClient, String> {
    let client = device
        .get_iaudioclient()
        .map_err(|e| format!("{what} audio client: {e}"))?;
    if voice {
        let props = AudioClientProperties::new().set_category(StreamCategory::Communications);
        if let Err(e) = client.set_properties(props) {
            // IAudioClient2 is Windows 8+; nothing older runs us. Log
            // and carry on as a plain stream; the report will say so.
            warn!("{what}: Communications category refused: {e}");
        }
    }
    Ok(client)
}

/// Open a client for `device`, preferring 48 kHz f32 with conversion.
fn open_with_fallback(
    device: &wasapi::Device,
    direction: Direction,
    buffer_hns: i64,
    what: &str,
    voice: bool,
) -> Result<(AudioClient, Negotiated), String> {
    let mut client = new_client(device, voice, what)?;
    match open_client(&mut client, direction, buffer_hns, what) {
        Ok(n) => Ok((client, n)),
        Err(first) => {
            debug!("{what}: {first}; retrying at mix format");
            let mut client = new_client(device, voice, what)?;
            let n = open_client_mix(&mut client, direction, buffer_hns, what)?;
            Ok((client, n))
        }
    }
}

/// The role "default" means for this session.
fn role_for(voice: bool) -> Role {
    if voice {
        Role::Communications
    } else {
        Role::Console
    }
}

/// After capture opened in the `Communications` category: point the
/// canceller at our render endpoint and say whether one is running
/// (AEC-SPEC.md §4.1 step 5).
fn engage_platform_aec(
    client: &AudioClient,
    enumerator: &DeviceEnumerator,
    reference: Option<&str>,
) -> Result<(), &'static str> {
    // Windows 11 22H2+: the reference is ours to name. Elsewhere (and
    // when this fails) the effect picks its own, usually the default
    // communications render device — which is what we opened.
    match client.get_aec_control() {
        Ok(control) => {
            let id = find_device(
                enumerator,
                Direction::Render,
                reference,
                Role::Communications,
            )
            .and_then(|d| d.get_id().map_err(|e| e.to_string()))
            .ok();
            match control.set_echo_cancellation_render_endpoint(id.clone()) {
                Ok(()) => info!(
                    "AEC reference endpoint set ({})",
                    id.as_deref().unwrap_or("auto")
                ),
                Err(e) => debug!("AEC reference endpoint not set: {e}"),
            }
        }
        Err(e) => debug!("AEC control unavailable ({e}); the effect picks its reference"),
    }
    // Verified, not assumed (D5). No effects manager (Windows 10): the
    // category is set and the OEM chain may still cancel — say on.
    match client
        .get_audio_effects_manager()
        .and_then(|m| m.get_audio_effects())
    {
        Ok(Some(effects)) => {
            let on = effects.iter().any(|e| {
                e.id == AUDIO_EFFECT_TYPE_ACOUSTIC_ECHO_CANCELLATION
                    && e.state == AUDIO_EFFECT_STATE_ON
            });
            if on {
                Ok(())
            } else {
                Err(aec::reasons::NO_CANCELLER)
            }
        }
        Ok(None) => Err(aec::reasons::NO_CANCELLER),
        Err(e) => {
            debug!("audio effects manager unavailable ({e}); assuming the endpoint's chain");
            Ok(())
        }
    }
}

/// The render endpoint's form factor as an [`OutputKind`]
/// (AEC-SPEC.md §4.2): `name`, or the console default. Runs on a
/// short-lived thread of its own so COM is initialised wherever the
/// caller is. `Unknown` when anything fails.
#[must_use]
pub fn output_kind(name: Option<&str>) -> OutputKind {
    let name = name.map(String::from);
    let worker = crate::platform::spawn_named("output-kind", move || {
        if wasapi::initialize_mta().ok().is_err() {
            return OutputKind::Unknown;
        }
        let kind = (|| -> Result<u32, String> {
            let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
            let device = find_device(
                &enumerator,
                Direction::Render,
                name.as_deref(),
                Role::Console,
            )?;
            let id = device.get_id().map_err(|e| e.to_string())?;
            form_factor(&id).map_err(|e| e.to_string())
        })();
        wasapi::deinitialize();
        match kind {
            Ok(ff) => aec::windows_output_kind(ff),
            Err(e) => {
                debug!("output form factor: {e}");
                OutputKind::Unknown
            }
        }
    });
    worker.join().unwrap_or(OutputKind::Unknown)
}

/// `PKEY_AudioEndpoint_FormFactor` of the endpoint `id`, through the
/// `windows` crate directly: `wasapi` reads properties but keeps the
/// accessor private (AEC-SPEC.md §4.2). COM must be initialised.
fn form_factor(id: &str) -> windows::core::Result<u32> {
    use windows::{
        core::HSTRING,
        Win32::{
            Media::Audio::{
                IMMDeviceEnumerator, MMDeviceEnumerator, PKEY_AudioEndpoint_FormFactor,
            },
            System::Com::{CoCreateInstance, CLSCTX_ALL, STGM_READ},
        },
    };
    // SAFETY: plain COM calls on interfaces this function owns; the
    // HSTRING outlives the call that borrows it.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let w_id = HSTRING::from(id);
        let device = enumerator.GetDevice(&w_id)?;
        let store = device.OpenPropertyStore(STGM_READ)?;
        let value = store.GetValue(&PKEY_AudioEndpoint_FormFactor)?;
        u32::try_from(&value)
    }
}

fn is_device_gone(e: &WasapiError) -> bool {
    // AUDCLNT_E_DEVICE_INVALIDATED = 0x88890004; the crate wraps HRESULTs
    // in its Windows error variant, whose Display includes the code.
    let text = e.to_string();
    text.contains("0x88890004") || text.to_lowercase().contains("invalidated")
}

// ── Capture ─────────────────────────────────────────────────────────────

/// A running native capture stream. Dropping it stops the thread.
pub struct NativeCapture {
    should_stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Rate of the samples arriving in the ring (48 kHz unless the engine
    /// refused conversion).
    pub sample_rate: u32,
    /// Friendly name of the endpoint that was opened.
    pub device_name: String,
    /// On the voice-call path: whether an AEC effect is running on the
    /// endpoint (`Err` = why not). `None` = a plain stream.
    pub aec_verdict: Option<Result<(), &'static str>>,
}

/// What the capture thread reports once the device is open: rate,
/// endpoint name, and the AEC verdict (voice-call path only).
type CaptureSetup = (u32, String, Option<Result<(), &'static str>>);

struct CaptureParts {
    producer: Producer<f32>,
    wake_tx: Sender<()>,
    stats: Arc<AudioStats>,
    config: Arc<InputConfig>,
    input_level: Arc<AtomicU32>,
}

type SetupReport<T> = Result<T, String>;

impl NativeCapture {
    /// Open `device_name` (or the default capture endpoint) on a new
    /// MMCSS thread and start pumping into `producer`.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        device_name: Option<&str>,
        voice: Option<Option<String>>,
        producer: Producer<f32>,
        wake_tx: Sender<()>,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
        input_level: Arc<AtomicU32>,
    ) -> Result<Self, CaptureStartError> {
        let should_stop = Arc::new(AtomicBool::new(false));
        let (setup_tx, setup_rx) = bounded::<SetupReport<CaptureSetup>>(1);
        // Failure path must hand the ring back, so the thread only takes
        // ownership once setup has succeeded: parts travel in a channel.
        let (parts_tx, parts_rx) = bounded::<CaptureParts>(1);
        let name = device_name.map(String::from);
        let stop = should_stop.clone();

        let thread = crate::platform::spawn_named("voice-capture-wasapi", move || {
            capture_thread(name.as_deref(), voice.as_ref(), &setup_tx, &parts_rx, &stop);
        });

        match setup_rx.recv() {
            Ok(Ok((sample_rate, device_name, aec_verdict))) => {
                let parts = CaptureParts {
                    producer,
                    wake_tx,
                    stats,
                    config,
                    input_level,
                };
                if let Err(returned) = parts_tx.send(parts) {
                    // Thread died between setup and hand-off; the parts
                    // come back in the error, so the fallback can use them.
                    should_stop.store(true, Ordering::Relaxed);
                    let _ = thread.join();
                    let parts = returned.0;
                    return Err(CaptureStartError {
                        reason: "capture thread exited before hand-off".into(),
                        producer: parts.producer,
                        wake_tx: parts.wake_tx,
                    });
                }
                Ok(Self {
                    should_stop,
                    thread: Some(thread),
                    sample_rate,
                    device_name,
                    aec_verdict,
                })
            }
            other => {
                let reason = match other {
                    Ok(Err(r)) => r,
                    _ => "capture thread exited during setup".to_string(),
                };
                let _ = thread.join();
                Err(CaptureStartError {
                    reason,
                    producer,
                    wake_tx,
                })
            }
        }
    }
}

impl Drop for NativeCapture {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `voice`: `Some(reference)` = open on the voice-call path with the
/// AEC reference set to the render endpoint `reference` (`None` inside
/// = the default communications device); `None` = a plain stream.
fn capture_thread(
    device_name: Option<&str>,
    voice: Option<&Option<String>>,
    setup_tx: &Sender<SetupReport<CaptureSetup>>,
    parts_rx: &crossbeam_channel::Receiver<CaptureParts>,
    should_stop: &AtomicBool,
) {
    if let Err(e) = wasapi::initialize_mta().ok() {
        let _ = setup_tx.send(Err(format!("COM init: {e}")));
        return;
    }
    // Real-time class before anything else: this thread is the reason
    // the backend exists.
    let _rt = crate::platform::promote_current_thread(480, OPUS_SAMPLE_RATE);

    let mut aec_verdict = None;
    let opened =
        (|| -> Result<(AudioClient, AudioCaptureClient, Handle, Negotiated, String), String> {
            let enumerator = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
            let device = find_device(
                &enumerator,
                Direction::Capture,
                device_name,
                role_for(voice.is_some()),
            )?;
            let friendly = device.get_friendlyname().unwrap_or_else(|_| "?".into());
            let (client, negotiated) = open_with_fallback(
                &device,
                Direction::Capture,
                CAPTURE_BUFFER_HNS,
                "capture",
                voice.is_some(),
            )?;
            if let Some(reference) = voice {
                aec_verdict = Some(engage_platform_aec(
                    &client,
                    &enumerator,
                    reference.as_deref(),
                ));
            }
            let event = client
                .set_get_eventhandle()
                .map_err(|e| format!("capture event handle: {e}"))?;
            let capture = client
                .get_audiocaptureclient()
                .map_err(|e| format!("capture client: {e}"))?;
            client
                .start_stream()
                .map_err(|e| format!("capture start: {e}"))?;
            Ok((client, capture, event, negotiated, friendly))
        })();

    let (client, capture, event, negotiated, friendly) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = setup_tx.send(Err(e));
            wasapi::deinitialize();
            return;
        }
    };
    if setup_tx
        .send(Ok((negotiated.sample_rate, friendly.clone(), aec_verdict)))
        .is_err()
    {
        wasapi::deinitialize();
        return;
    }
    let Ok(parts) = parts_rx.recv() else {
        wasapi::deinitialize();
        return;
    };

    info!(
        "Native WASAPI capture: {friendly} @ {} Hz, {}ch, {:?}, {} ms engine buffer, category={}",
        negotiated.sample_rate,
        negotiated.channels,
        negotiated.layout,
        CAPTURE_BUFFER_HNS / 10_000,
        if voice.is_some() {
            "Communications"
        } else {
            "Other"
        }
    );
    run_capture(&client, &capture, &event, negotiated, parts, should_stop);
    if let Err(e) = client.stop_stream() {
        debug!("capture stop_stream: {e}");
    }
    wasapi::deinitialize();
    debug!("Native WASAPI capture thread stopped");
}

fn run_capture(
    _client: &AudioClient,
    capture: &AudioCaptureClient,
    event: &Handle,
    negotiated: Negotiated,
    parts: CaptureParts,
    should_stop: &AtomicBool,
) {
    let CaptureParts {
        mut producer,
        wake_tx,
        stats,
        config,
        input_level,
    } = parts;
    let mut packet_buf: Vec<u8> = Vec::new();
    let mut consecutive_errors = 0u32;
    let mut last_glitch_log: Option<Instant> = None;
    let ch = negotiated.channels;
    let bpf = negotiated.bytes_per_frame;

    while !should_stop.load(Ordering::Relaxed) {
        match event.wait_for_event(EVENT_WAIT_MS) {
            Ok(()) => {}
            Err(WasapiError::EventTimeout) => continue,
            Err(e) => {
                error!("Native capture event wait failed: {e}");
                stats.input_stream_failed.store(true, Ordering::Relaxed);
                break;
            }
        }

        // Drain every packet queued behind this event.
        loop {
            let frames = match capture.get_next_packet_size() {
                Ok(Some(0) | None) => break,
                Ok(Some(frames)) => frames as usize,
                Err(e) => {
                    consecutive_errors += 1;
                    if is_device_gone(&e) || consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        error!("Native capture lost the device: {e}");
                        stats.input_stream_failed.store(true, Ordering::Relaxed);
                        return;
                    }
                    break;
                }
            };
            let bytes = frames * bpf;
            if packet_buf.len() < bytes {
                packet_buf.resize(bytes, 0);
            }
            let (got, info) = match capture.read_from_device(&mut packet_buf[..bytes]) {
                Ok(v) => v,
                Err(e) => {
                    consecutive_errors += 1;
                    if is_device_gone(&e) || consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        error!("Native capture read failed: {e}");
                        stats.input_stream_failed.store(true, Ordering::Relaxed);
                        return;
                    }
                    break;
                }
            };
            if got == 0 {
                break;
            }
            consecutive_errors = 0;
            let got = got as usize;

            if info.flags.data_discontinuity {
                stats.input_overruns.fetch_add(1, Ordering::Relaxed);
                let now = Instant::now();
                if last_glitch_log.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
                    last_glitch_log = Some(now);
                    warn!(
                        "Native capture discontinuity (total {})",
                        stats.input_overruns.load(Ordering::Relaxed)
                    );
                }
            }

            if config.muted.load(Ordering::Relaxed) {
                input_level.store(0.0_f32.to_bits(), Ordering::Relaxed);
                continue;
            }

            // Downmix into the ring in one chunked write. A SILENT packet
            // may hold garbage per the WASAPI docs: push zeros instead so
            // the sample count — and therefore the encoder's timeline —
            // stays continuous.
            let want = got.min(producer.slots());
            if want < got {
                stats.input_overruns.fetch_add(1, Ordering::Relaxed);
            }
            if want == 0 {
                continue;
            }
            let mut rms_acc = 0.0f32;
            let Ok(chunk) = producer.write_chunk_uninit(want) else {
                continue;
            };
            let silent = info.flags.silent;
            let layout = negotiated.layout;
            {
                let mut it = packet_buf[..got * bpf]
                    .chunks_exact(bpf)
                    .take(want)
                    .map(|frame| {
                        let mono = if silent {
                            0.0
                        } else {
                            layout.frame_to_mono(frame, ch)
                        };
                        rms_acc += mono * mono;
                        mono
                    });
                chunk.fill_from_iter(&mut it);
            }

            #[allow(clippy::cast_precision_loss)]
            let rms = (rms_acc / want as f32).sqrt();
            input_level.store((rms * 3.0).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            let _ = wake_tx.try_send(());
        }
    }
}

// ── Render ──────────────────────────────────────────────────────────────

/// A running native render stream. Dropping it stops the thread.
pub struct NativeRender {
    should_stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Rate the ring must be filled at (48 kHz unless the engine refused
    /// conversion).
    pub sample_rate: u32,
    pub device_name: String,
}

struct RenderParts {
    consumer: Consumer<f32>,
    wake_tx: Sender<()>,
    stats: Arc<AudioStats>,
}

impl NativeRender {
    pub fn start(
        device_name: Option<&str>,
        voice: bool,
        consumer: Consumer<f32>,
        wake_tx: Sender<()>,
        stats: Arc<AudioStats>,
    ) -> Result<Self, RenderStartError> {
        let should_stop = Arc::new(AtomicBool::new(false));
        let (setup_tx, setup_rx) = bounded::<SetupReport<(u32, String)>>(1);
        let (parts_tx, parts_rx) = bounded::<RenderParts>(1);
        let name = device_name.map(String::from);
        let stop = should_stop.clone();

        let thread = crate::platform::spawn_named("voice-render-wasapi", move || {
            render_thread(name.as_deref(), voice, &setup_tx, &parts_rx, &stop);
        });

        match setup_rx.recv() {
            Ok(Ok((sample_rate, device_name))) => {
                let parts = RenderParts {
                    consumer,
                    wake_tx,
                    stats,
                };
                if let Err(returned) = parts_tx.send(parts) {
                    should_stop.store(true, Ordering::Relaxed);
                    let _ = thread.join();
                    let parts = returned.0;
                    return Err(RenderStartError {
                        reason: "render thread exited before hand-off".into(),
                        consumer: parts.consumer,
                        wake_tx: parts.wake_tx,
                    });
                }
                Ok(Self {
                    should_stop,
                    thread: Some(thread),
                    sample_rate,
                    device_name,
                })
            }
            other => {
                let reason = match other {
                    Ok(Err(r)) => r,
                    _ => "render thread exited during setup".to_string(),
                };
                let _ = thread.join();
                Err(RenderStartError {
                    reason,
                    consumer,
                    wake_tx,
                })
            }
        }
    }
}

impl Drop for NativeRender {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn render_thread(
    device_name: Option<&str>,
    voice: bool,
    setup_tx: &Sender<SetupReport<(u32, String)>>,
    parts_rx: &crossbeam_channel::Receiver<RenderParts>,
    should_stop: &AtomicBool,
) {
    if let Err(e) = wasapi::initialize_mta().ok() {
        let _ = setup_tx.send(Err(format!("COM init: {e}")));
        return;
    }
    let _rt = crate::platform::promote_current_thread(480, OPUS_SAMPLE_RATE);

    let opened =
        (|| -> Result<(AudioClient, AudioRenderClient, Handle, Negotiated, String), String> {
            let enumerator = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
            let device = find_device(&enumerator, Direction::Render, device_name, role_for(voice))?;
            let friendly = device.get_friendlyname().unwrap_or_else(|_| "?".into());
            let (client, negotiated) = open_with_fallback(
                &device,
                Direction::Render,
                RENDER_BUFFER_HNS,
                "render",
                voice,
            )?;
            let event = client
                .set_get_eventhandle()
                .map_err(|e| format!("render event handle: {e}"))?;
            let render = client
                .get_audiorenderclient()
                .map_err(|e| format!("render client: {e}"))?;
            // Pre-fill the engine buffer with silence so the first period
            // isn't a glitch, then start.
            if let Ok(space) = client.get_available_space_in_frames() {
                let zeros = vec![0u8; space as usize * negotiated.bytes_per_frame];
                let _ = render.write_to_device(space as usize, &zeros, None);
            }
            client
                .start_stream()
                .map_err(|e| format!("render start: {e}"))?;
            Ok((client, render, event, negotiated, friendly))
        })();

    let (client, render, event, negotiated, friendly) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = setup_tx.send(Err(e));
            wasapi::deinitialize();
            return;
        }
    };
    if setup_tx
        .send(Ok((negotiated.sample_rate, friendly.clone())))
        .is_err()
    {
        wasapi::deinitialize();
        return;
    }
    let Ok(parts) = parts_rx.recv() else {
        wasapi::deinitialize();
        return;
    };

    info!(
        "Native WASAPI render: {friendly} @ {} Hz, {}ch, {:?}, {} ms engine buffer, category={}",
        negotiated.sample_rate,
        negotiated.channels,
        negotiated.layout,
        RENDER_BUFFER_HNS / 10_000,
        if voice { "Communications" } else { "Other" }
    );
    run_render(&client, &render, &event, negotiated, parts, should_stop);
    if let Err(e) = client.stop_stream() {
        debug!("render stop_stream: {e}");
    }
    wasapi::deinitialize();
    debug!("Native WASAPI render thread stopped");
}

fn run_render(
    client: &AudioClient,
    render: &AudioRenderClient,
    event: &Handle,
    negotiated: Negotiated,
    parts: RenderParts,
    should_stop: &AtomicBool,
) {
    let RenderParts {
        mut consumer,
        wake_tx,
        stats,
    } = parts;
    let ch = negotiated.channels;
    let bpf = negotiated.bytes_per_frame;
    let bps = negotiated.layout.bytes_per_sample();
    let layout = negotiated.layout;
    let mut mono: Vec<f32> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    let mut consecutive_errors = 0u32;
    let mut last_err_log: Option<Instant> = None;

    while !should_stop.load(Ordering::Relaxed) {
        match event.wait_for_event(EVENT_WAIT_MS) {
            Ok(()) => {}
            Err(WasapiError::EventTimeout) => continue,
            Err(e) => {
                error!("Native render event wait failed: {e}");
                stats.output_stream_failed.store(true, Ordering::Relaxed);
                break;
            }
        }

        let space = match client.get_available_space_in_frames() {
            Ok(s) => s as usize,
            Err(e) => {
                consecutive_errors += 1;
                if is_device_gone(&e) || consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    error!("Native render lost the device: {e}");
                    stats.output_stream_failed.store(true, Ordering::Relaxed);
                    return;
                }
                continue;
            }
        };
        if space == 0 {
            continue;
        }

        // Pull what the engine can take; whatever the ring can't supply is
        // an underrun and plays as silence.
        mono.resize(space, 0.0);
        let (_popped, missing) = consumer.pop_partial_slice(&mut mono[..]);
        if !missing.is_empty() {
            stats.output_underruns.fetch_add(1, Ordering::Relaxed);
            missing.fill(0.0);
        }

        // Fan out: FL and FR carry the voice, every other channel silence.
        out.clear();
        out.resize(space * bpf, 0);
        let mut rms_acc = 0.0f32;
        for (i, &s) in mono[..space].iter().enumerate() {
            rms_acc += s * s;
            let base = i * bpf;
            layout.write_sample(s, &mut out[base..base + bps]);
            if ch > 1 {
                layout.write_sample(s, &mut out[base + bps..base + 2 * bps]);
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let rms = (rms_acc / space as f32).sqrt();
        stats
            .output_level
            .store((rms * 3.0).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);

        match render.write_to_device(space, &out, None) {
            Ok(()) => consecutive_errors = 0,
            Err(e) => {
                consecutive_errors += 1;
                let now = Instant::now();
                if last_err_log.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
                    last_err_log = Some(now);
                    error!("Native render write failed: {e}");
                }
                if is_device_gone(&e) || consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    stats.output_stream_failed.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }

        // Ring space freed — refill promptly rather than on the tick.
        let _ = wake_tx.try_send(());
    }
}

// ── Device notifications ────────────────────────────────────────────────

/// Keeps an `IMMNotificationClient` registered on a thread of its own and
/// raises the shared default-device-changed flags in `AudioStats`, so the
/// supervisor can react without polling. Drop to unregister.
///
/// The COM objects never leave the watcher thread, so this handle is
/// plain `Send` regardless of how the interface wrappers are marked.
pub struct DeviceWatcher {
    stop_tx: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceWatcher {
    /// `communications`: the session is on the voice-call path, so the
    /// default that matters is the Default Communications Device
    /// (AEC-SPEC.md §4.5); otherwise Console/Multimedia, as ever.
    pub fn start(stats: Arc<AudioStats>, communications: Arc<AtomicBool>) -> Result<Self, String> {
        let (stop_tx, stop_rx) = bounded::<()>(1);
        let (setup_tx, setup_rx) = bounded::<Result<(), String>>(1);
        let thread = crate::platform::spawn_named("device-watch", move || {
            if let Err(e) = wasapi::initialize_mta().ok() {
                let _ = setup_tx.send(Err(format!("COM init: {e}")));
                return;
            }
            let registration = match register(&stats, &communications) {
                Ok(r) => r,
                Err(e) => {
                    let _ = setup_tx.send(Err(e));
                    wasapi::deinitialize();
                    return;
                }
            };
            let _ = setup_tx.send(Ok(()));
            // Park until told to stop; callbacks arrive on COM's thread.
            let _ = stop_rx.recv();
            drop(registration);
            wasapi::deinitialize();
        });
        match setup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_tx,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("device watcher thread exited during setup".into())
            }
        }
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Register the notification client; the registration must outlive the
/// enumerator it came from, so both are returned together.
fn register(
    stats: &Arc<AudioStats>,
    communications: &Arc<AtomicBool>,
) -> Result<(DeviceEnumerator, DeviceEventRegistration), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
    let mut callbacks = DeviceEventCallbacks::new();
    {
        let stats = stats.clone();
        let communications = communications.clone();
        callbacks.set_default_device_callback(move |direction, role, id| {
            // Console/Multimedia are what "default device" means for apps;
            // Communications is a separate user setting — the one that
            // applies while we are on the voice-call path.
            let relevant = if communications.load(Ordering::Relaxed) {
                matches!(role, Role::Communications)
            } else {
                matches!(role, Role::Console | Role::Multimedia)
            };
            if !relevant {
                return;
            }
            debug!("default {direction} device changed ({role}): {id:?}");
            match direction {
                Direction::Capture => stats.default_input_changed.store(true, Ordering::Relaxed),
                Direction::Render => stats.default_output_changed.store(true, Ordering::Relaxed),
            }
        });
    }
    {
        let stats = stats.clone();
        callbacks.set_device_removed_callback(move |id| {
            debug!("audio device removed: {id}");
            // Whichever side was using it fails on its next engine call
            // and raises its own flag; this just makes the supervisor
            // look sooner.
            stats.default_input_changed.store(true, Ordering::Relaxed);
            stats.default_output_changed.store(true, Ordering::Relaxed);
        });
    }
    let registration = enumerator
        .register_notification_callback(callbacks)
        .map_err(|e| format!("register notifications: {e}"))?;
    Ok((enumerator, registration))
}
