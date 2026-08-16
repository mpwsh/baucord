//! Windows process-exclusive system audio capture.
//!
//! Captures the system mix through WASAPI *process loopback*
//! (`ActivateAudioInterfaceAsync` on the virtual process-loopback device)
//! in `EXCLUDE_TARGET_PROCESS_TREE` mode targeting our own process: the
//! audio engine hands us everything the system renders *minus what this
//! process tree plays*. Decoded peer voices, notification sounds and
//! anything else Baucord itself outputs never enter the stream — which is
//! what keeps viewers from hearing their own voices echoed back while
//! watching a share.
//!
//! Availability: the API shipped with Windows 10 2004 (build 19041);
//! Microsoft's ApplicationLoopback sample README claims build 20348+.
//! We don't version-sniff — activation failure *is* the signal, and
//! `input::SystemAudioCapture` falls back to the cpal endpoint-loopback
//! backend when `start()` reports one.
//!
//! ## Differences from endpoint loopback worth remembering
//!
//! - **No endpoint**: the client is activated on a virtual device, not on
//!   the default output, so capture is expected to keep working across
//!   default-device changes. `ScreenControl::RestartSystemAudio` stays
//!   wired up as belt and braces, and the endpoint fallback still needs it.
//! - **We choose the format**: `GetMixFormat` is unimplemented for process
//!   loopback; the engine converts to whatever we request. We request
//!   48 kHz stereo f32 so the encode worker's resampler is bypassed.
//! - **Silence is empty, not zeros**: the capture event only fires while
//!   non-excluded sessions render. When the system is quiet no packets
//!   arrive, the worker has nothing to encode, and no network packets are
//!   sent (seq doesn't advance, so the receiver sees no loss).
//!
//! ## Threading
//!
//! The `IAudioClient` from this activation is `!Send`, so the entire
//! client lifecycle (COM init → activate → initialize → pump → teardown)
//! lives on one dedicated thread. `start()` performs a one-shot channel
//! handshake with it so activation failures surface synchronously to the
//! caller — that is what makes the runtime fallback possible.

use crate::stats::AudioStats;

use crossbeam_channel::{bounded, Sender};
use log::{debug, error, info};
use rtrb::Producer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use wasapi::{
    AudioCaptureClient, AudioClient, Direction, Handle, SampleType, StreamMode, WasapiError,
    WaveFormat,
};

use super::input::SystemAudioBackend;

/// Capture format: 48 kHz stereo f32 — exactly what the Opus encode
/// worker wants, so `needs_resample` is false and rubato never runs.
const CAPTURE_SAMPLE_RATE: u32 = crate::OPUS_SAMPLE_RATE;

/// Stereo — the ring/worker contract is interleaved stereo f32.
const CAPTURE_CHANNELS: usize = 2;

/// The engine delivers f32 samples in the format we requested.
const BYTES_PER_SAMPLE: usize = std::mem::size_of::<f32>();

/// One interleaved stereo frame.
const BYTES_PER_FRAME: usize = CAPTURE_CHANNELS * BYTES_PER_SAMPLE;

/// Engine capture buffer: 20 ms in 100-ns units — the value used by both
/// Microsoft's ApplicationLoopback sample and wasapi-rs's own example.
/// Latency is governed by how fast we drain (event-driven), not by this.
const BUFFER_DURATION_HNS: i64 = 200_000;

/// Capture-event wait timeout. WASAPI signals the event only while
/// non-excluded sessions render, so during system silence no events
/// arrive at all; this timeout exists solely to bound `stop()` latency
/// (at most one wait) by re-checking `should_stop`. It is an OS event
/// wait — zero CPU while blocked — not a polling sleep.
const EVENT_WAIT_MS: u32 = 100;

/// Consecutive capture errors before the pump gives up, so a dead audio
/// engine degrades to a silent stream instead of a hot error loop.
const MAX_CONSECUTIVE_ERRORS: u32 = 50;

/// One-shot setup report from the pump thread back to `start()`.
type SetupReport = Result<(), String>;

/// System audio backend that excludes our own process tree at capture
/// time. Preferred over [`super::input`]'s endpoint loopback whenever the
/// OS supports it.
pub(crate) struct ProcessLoopback {
    /// Pump thread — owns COM and every WASAPI object for its lifetime.
    pump: Option<thread::JoinHandle<()>>,
    /// Signal to stop the pump thread.
    should_stop: Arc<AtomicBool>,
    /// Ring producer, held until `start()` hands it to the pump thread.
    ring_producer: Option<Producer<f32>>,
    /// Stats (shared with voice chat engine for unified reporting).
    stats: Arc<AudioStats>,
}

impl ProcessLoopback {
    /// Create the backend. Infallible — all fallible work (COM, WASAPI
    /// activation) happens in [`SystemAudioBackend::start`] so the caller
    /// can fall back to another backend when this build of Windows lacks
    /// process loopback.
    pub(crate) fn new(ring_producer: Producer<f32>, stats: Arc<AudioStats>) -> Self {
        Self {
            pump: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            ring_producer: Some(ring_producer),
            stats,
        }
    }
}

impl SystemAudioBackend for ProcessLoopback {
    fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.pump.is_some() {
            return Ok(());
        }
        let ring_producer = self
            .ring_producer
            .take()
            .ok_or("Ring producer already taken")?;
        self.should_stop.store(false, Ordering::Relaxed);

        let should_stop = self.should_stop.clone();
        let stats = self.stats.clone();

        // One-shot handshake: the pump reports setup success or failure
        // exactly once, so activation errors surface synchronously here.
        // That is what lets input.rs fall back to endpoint loopback.
        let (setup_tx, setup_rx) = bounded::<SetupReport>(1);

        let pump = thread::Builder::new()
            .name("sysaudio-procloop".into())
            .spawn(move || pump_thread(ring_producer, &stats, &should_stop, &setup_tx))?;

        // Blocking recv with no timeout needed: the pump always reports
        // (a panic disconnects the channel, surfacing as RecvError).
        match setup_rx.recv() {
            Ok(Ok(())) => {
                self.pump = Some(pump);
                Ok(())
            }
            Ok(Err(msg)) => {
                let _ = pump.join();
                Err(msg.into())
            }
            Err(_) => {
                let _ = pump.join();
                Err("System audio pump thread died during setup".into())
            }
        }
    }

    fn stop(&mut self) {
        self.should_stop.store(true, Ordering::Release);
        if let Some(pump) = self.pump.take() {
            // The pump notices the flag within one event wait (bounded
            // by EVENT_WAIT_MS), so this join is short.
            let _ = pump.join();
        }
    }

    fn sample_rate(&self) -> u32 {
        CAPTURE_SAMPLE_RATE
    }

    fn device_name(&self) -> String {
        "System mix (own process excluded)".into()
    }
}

impl Drop for ProcessLoopback {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Pump thread ──────────────────────────────────────────────────────────

/// Entry point of the dedicated capture thread. Owns COM and every WASAPI
/// object from activation to teardown.
fn pump_thread(
    ring_producer: Producer<f32>,
    stats: &AudioStats,
    should_stop: &AtomicBool,
    setup_tx: &Sender<SetupReport>,
) {
    // Per-thread COM (MTA), balanced by `deinitialize` on every exit path.
    if let Err(e) = wasapi::initialize_mta().ok() {
        let _ = setup_tx.send(Err(format!("COM init failed: {e}")));
        return;
    }

    match setup_capture() {
        Ok((client, capture, event)) => {
            let _ = setup_tx.send(Ok(()));
            // Audio-class scheduling (MMCSS "Pro Audio"), same as the
            // encode worker.
            let _mmcss = crate::platform::promote_current_thread();
            run_pump(&capture, &event, ring_producer, stats, should_stop);
            if let Err(e) = client.stop_stream() {
                debug!("System audio process loopback: stop_stream failed: {e}");
            }
        }
        Err(msg) => {
            let _ = setup_tx.send(Err(msg));
        }
    }

    wasapi::deinitialize();
}

/// Activate, initialize and start the process-loopback capture client.
///
/// Errors are strings because they cross the setup handshake channel;
/// every step names itself so the fallback log stays diagnosable.
fn setup_capture() -> Result<(AudioClient, AudioCaptureClient, Handle), String> {
    // `include_tree: false` reads like "target process only" in the
    // wasapi crate docs, but the implementation maps it to
    // PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE — capture
    // everything EXCEPT our process tree, which is exactly what we want.
    // Verified against wasapi 0.24 source (api.rs, new_application_
    // loopback_client); do not "fix" this to `true`.
    let mut client = AudioClient::new_application_loopback_client(std::process::id(), false)
        .map_err(|e| format!("process loopback activation: {e}"))?;

    // The engine converts to the format we request (`GetMixFormat` is
    // unimplemented for process loopback). 48 kHz stereo f32 matches the
    // encode worker, bypassing the resampler entirely.
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        CAPTURE_SAMPLE_RATE as usize,
        CAPTURE_CHANNELS,
        None,
    );
    client
        .initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: BUFFER_DURATION_HNS,
            },
        )
        .map_err(|e| format!("process loopback init: {e}"))?;

    let event = client
        .set_get_eventhandle()
        .map_err(|e| format!("process loopback event handle: {e}"))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|e| format!("process loopback capture client: {e}"))?;
    client
        .start_stream()
        .map_err(|e| format!("process loopback start: {e}"))?;

    Ok((client, capture, event))
}

/// The capture loop: block on the engine's event, drain every pending
/// packet into the ring, repeat until stopped.
///
/// The wait is an OS event wait (zero CPU while blocked). During system
/// silence WASAPI raises no events, so the only wakeups are the
/// `EVENT_WAIT_MS` shutdown polls — ~10/s of a no-op branch.
fn run_pump(
    capture: &AudioCaptureClient,
    event: &Handle,
    mut ring_producer: Producer<f32>,
    stats: &AudioStats,
    should_stop: &AtomicBool,
) {
    // Scratch for one engine packet; grows to the largest packet seen
    // (~10 ms ≈ 3.9 KiB at 48 kHz stereo f32) and is reused thereafter.
    let mut packet_buf: Vec<u8> = Vec::new();
    let mut consecutive_errors: u32 = 0;

    info!("🔊🎤 System audio process-loopback pump started (48 kHz stereo f32)");

    while !should_stop.load(Ordering::Relaxed) {
        match event.wait_for_event(EVENT_WAIT_MS) {
            Ok(()) => {}
            // Timeout: system silence (no sessions rendering) or simply
            // the periodic chance to observe `should_stop`. Not an error.
            Err(WasapiError::EventTimeout) => continue,
            Err(e) => {
                error!("System audio capture event wait failed: {e}");
                break;
            }
        }

        // Drain every packet queued behind this event before waiting again.
        loop {
            let frames = match capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(frames)) => frames as usize,
                Err(e) => {
                    consecutive_errors += 1;
                    error!("System audio capture packet size failed: {e}");
                    break;
                }
            };

            let bytes = frames * BYTES_PER_FRAME;
            if packet_buf.len() < bytes {
                packet_buf.resize(bytes, 0);
            }

            match capture.read_from_device(&mut packet_buf[..bytes]) {
                // Zero frames with a non-zero announced packet: bail out
                // to the event wait rather than risk a hot re-query loop.
                Ok((0, _)) => break,
                Ok((got, buffer_info)) => {
                    consecutive_errors = 0;
                    if buffer_info.flags.data_discontinuity {
                        debug!("System audio capture discontinuity");
                    }
                    let got_bytes = got as usize * BYTES_PER_FRAME;
                    push_interleaved(
                        &mut ring_producer,
                        &packet_buf[..got_bytes],
                        buffer_info.flags.silent,
                        stats,
                    );
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!("System audio capture read failed: {e}");
                    break;
                }
            }
        }

        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            error!(
                "System audio capture failing persistently — stopping pump \
                 (screen share continues without system audio)"
            );
            break;
        }
    }

    info!("🔊🎤 System audio process-loopback pump stopped");
}

/// Push one packet of interleaved stereo f32 into the ring.
///
/// The engine already delivers the exact layout the ring contract wants
/// (L R L R … f32 at 48 kHz), so this is a straight byte→f32 copy. A
/// SILENT-flagged buffer may hold garbage per the WASAPI docs, so it
/// becomes zeros — keeping the timeline continuous instead of injecting
/// noise.
fn push_interleaved(
    ring_producer: &mut Producer<f32>,
    bytes: &[u8],
    silent: bool,
    stats: &AudioStats,
) {
    let total = bytes.len() / BYTES_PER_SAMPLE;
    // Never split a stereo pair across a full-ring drop: an odd push
    // count would permanently swap L/R for everything that follows.
    let available = ring_producer.slots() & !1_usize;
    let n = total.min(available);
    if n < total {
        stats.input_overruns.fetch_add(1, Ordering::Relaxed);
    }
    for sample in bytes.chunks_exact(BYTES_PER_SAMPLE).take(n) {
        let value = if silent {
            0.0
        } else {
            f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]])
        };
        // Cannot fail: n ≤ free slots and this thread is the sole producer.
        let _ = ring_producer.push(value);
    }
}
