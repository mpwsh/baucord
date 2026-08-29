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

use crate::input_config::InputConfig;
use crate::stats::AudioStats;
use crate::OPUS_SAMPLE_RATE;

use crossbeam_channel::{bounded, Sender};
use log::{debug, error, info, warn};
use rtrb::{Consumer, Producer};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasapi::{
    AudioCaptureClient, AudioClient, AudioRenderClient, DeviceEnumerator, DeviceEventCallbacks,
    DeviceEventRegistration, Direction, Handle, Role, SampleType, StreamMode, WasapiError,
    WaveFormat,
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
    I32 { valid_bits: u16 },
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

fn find_device(
    enumerator: &DeviceEnumerator,
    direction: Direction,
    name: Option<&str>,
) -> Result<wasapi::Device, String> {
    match name {
        None => enumerator
            .get_default_device(&direction)
            .map_err(|e| format!("default {direction} device: {e}")),
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

/// Open a client for `device`, preferring 48 kHz f32 with conversion.
fn open_with_fallback(
    device: &wasapi::Device,
    direction: Direction,
    buffer_hns: i64,
    what: &str,
) -> Result<(AudioClient, Negotiated), String> {
    let mut client = device
        .get_iaudioclient()
        .map_err(|e| format!("{what} audio client: {e}"))?;
    match open_client(&mut client, direction, buffer_hns, what) {
        Ok(n) => Ok((client, n)),
        Err(first) => {
            debug!("{what}: {first}; retrying at mix format");
            let mut client = device
                .get_iaudioclient()
                .map_err(|e| format!("{what} audio client (retry): {e}"))?;
            let n = open_client_mix(&mut client, direction, buffer_hns, what)?;
            Ok((client, n))
        }
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
}

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
    pub fn start(
        device_name: Option<&str>,
        producer: Producer<f32>,
        wake_tx: Sender<()>,
        stats: Arc<AudioStats>,
        config: Arc<InputConfig>,
        input_level: Arc<AtomicU32>,
    ) -> Result<Self, CaptureStartError> {
        let should_stop = Arc::new(AtomicBool::new(false));
        let (setup_tx, setup_rx) = bounded::<SetupReport<(u32, String)>>(1);
        // Failure path must hand the ring back, so the thread only takes
        // ownership once setup has succeeded: parts travel in a channel.
        let (parts_tx, parts_rx) = bounded::<CaptureParts>(1);
        let name = device_name.map(String::from);
        let stop = should_stop.clone();

        let thread = crate::platform::spawn_named("voice-capture-wasapi", move || {
            capture_thread(name.as_deref(), &setup_tx, &parts_rx, &stop);
        });

        match setup_rx.recv() {
            Ok(Ok((sample_rate, device_name))) => {
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

fn capture_thread(
    device_name: Option<&str>,
    setup_tx: &Sender<SetupReport<(u32, String)>>,
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

    let opened = (|| -> Result<(AudioClient, AudioCaptureClient, Handle, Negotiated, String), String> {
        let enumerator = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
        let device = find_device(&enumerator, Direction::Capture, device_name)?;
        let friendly = device.get_friendlyname().unwrap_or_else(|_| "?".into());
        let (client, negotiated) =
            open_with_fallback(&device, Direction::Capture, CAPTURE_BUFFER_HNS, "capture")?;
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
        "Native WASAPI capture: {friendly} @ {} Hz, {}ch, {:?}, {} ms engine buffer",
        negotiated.sample_rate,
        negotiated.channels,
        negotiated.layout,
        CAPTURE_BUFFER_HNS / 10_000
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
                Ok(Some(0)) | Ok(None) => break,
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
            render_thread(name.as_deref(), &setup_tx, &parts_rx, &stop);
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
    setup_tx: &Sender<SetupReport<(u32, String)>>,
    parts_rx: &crossbeam_channel::Receiver<RenderParts>,
    should_stop: &AtomicBool,
) {
    if let Err(e) = wasapi::initialize_mta().ok() {
        let _ = setup_tx.send(Err(format!("COM init: {e}")));
        return;
    }
    let _rt = crate::platform::promote_current_thread(480, OPUS_SAMPLE_RATE);

    let opened = (|| -> Result<(AudioClient, AudioRenderClient, Handle, Negotiated, String), String> {
        let enumerator = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
        let device = find_device(&enumerator, Direction::Render, device_name)?;
        let friendly = device.get_friendlyname().unwrap_or_else(|_| "?".into());
        let (client, negotiated) =
            open_with_fallback(&device, Direction::Render, RENDER_BUFFER_HNS, "render")?;
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
        "Native WASAPI render: {friendly} @ {} Hz, {}ch, {:?}, {} ms engine buffer",
        negotiated.sample_rate,
        negotiated.channels,
        negotiated.layout,
        RENDER_BUFFER_HNS / 10_000
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
    pub fn start(stats: Arc<AudioStats>) -> Result<Self, String> {
        let (stop_tx, stop_rx) = bounded::<()>(1);
        let (setup_tx, setup_rx) = bounded::<Result<(), String>>(1);
        let thread = crate::platform::spawn_named("device-watch", move || {
            if let Err(e) = wasapi::initialize_mta().ok() {
                let _ = setup_tx.send(Err(format!("COM init: {e}")));
                return;
            }
            let registration = match register(&stats) {
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
fn register(stats: &Arc<AudioStats>) -> Result<(DeviceEnumerator, DeviceEventRegistration), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("enumerator: {e}"))?;
    let mut callbacks = DeviceEventCallbacks::new();
    {
        let stats = stats.clone();
        callbacks.set_default_device_callback(move |direction, role, id| {
            // Console/Multimedia are what "default device" means for apps;
            // Communications is a separate user setting.
            if !matches!(role, Role::Console | Role::Multimedia) {
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
