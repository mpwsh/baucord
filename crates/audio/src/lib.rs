//! # voice-chat-core
//!
//! Low-latency voice chat engine with noise suppression.
//!
//! ## Input pipeline (processor chain)
//!
//! ```text
//! capture → resample → denoise → meter → gate → AGC → compressor → gain → clamp → meter → speaking → encode
//! ```
//!
//! ## Output pipeline
//!
//! ```text
//! network → per-peer packet buffer → decode at playout (+ FEC/PLC) → mix → gain → limit → resample → playback
//! ```
//!
//! Packets are held *encoded* until their playout slot is due, so the
//! adaptive jitter target is the only late-tolerance knob and Opus in-band
//! FEC is applied exactly when a slot goes missing (see
//! [`transport::packet_buffer`]). One output worker owns every peer's
//! decoder; a 20 ms mono Opus decode costs tens of microseconds, so even 30
//! simultaneous speakers fit comfortably inside one 10 ms hop.
//!
//! ## Device backends
//!
//! On Windows the microphone and speaker are driven by threads this crate
//! owns (`native::wasapi`): registered with MMCSS "Pro Audio" before the
//! device is opened, event-driven shared mode, and the audio engine
//! converting to/from 48 kHz so no resampler runs. cpal remains the
//! fallback there and the primary backend on macOS (whose `CoreAudio`
//! callback already runs on a real-time thread) and Linux. Either way the
//! engine asks for 48 kHz first (see [`device`]) and only resamples when a
//! device cannot provide it.
//!
//! ## Loss protection and rate adaptation
//!
//! Every packet carries Opus in-band FEC for the previous frame. On top of
//! that the sender runs a small rate controller ([`adapt`]) fed by the
//! transport's uplink loss ([`VoiceChat::set_uplink_loss_pct`]) and RTT
//! ([`VoiceChat::set_link_rtt_ms`]): random loss turns on redundant audio
//! (RED, [`transport::red`] — full copies of the preceding packet) and
//! steps the codec down one rung to pay for it; congestion (loss with RTT
//! inflation) steps down without ever adding bytes; recovery is slow and
//! one rung at a time. The receiver holds playout one repair-distance
//! longer only while loss is actually being observed, so FEC and RED can
//! land before their slot plays, and drops back to the minimum latency on
//! a clean link.
//!
//! **Wire format (protocol bump):** `PacketHeader::payload_type` is one
//! byte the application must serialise and parse ([`PayloadType`]);
//! receivers construct packets with [`AudioPacket::new_typed`].
//!
//! ## Device supervision
//!
//! Call [`VoiceChat::service`] from a periodic tick. It restarts a side
//! whose device vanished and follows the OS default device when none was
//! explicitly chosen — driven by `IMMNotificationClient` on Windows and a
//! two-second poll elsewhere.

mod adapt;
pub mod aec;
mod config;
mod device;
mod encoder;
mod input;
mod input_config;
mod native;
mod output;
mod platform;
pub mod processor;
mod resampler;
pub mod route;
pub mod sounds;
pub mod stats;
pub mod stream;
pub mod transport;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

/// The 10 s device-enumeration cache: when it was taken and what it saw.
type DeviceListCache = Mutex<Option<(Instant, Vec<(String, bool)>)>>;

pub use aec::{AecMode, AecReport, AecTier, OutputKind};
pub use config::{PeerAudioSettings, VoiceChatConfig};
use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use log::{debug, error, info};
pub use platform::{promote_current_thread, RtGuard};
pub use sounds::SoundType;
pub use stats::AudioStats;
pub use transport::packet::{AudioPacket, PayloadType};

/// Human-readable device name.
///
/// cpal 0.18 replaced `Device::name()` with `description()`, which
/// returns structured metadata. Every call site in this crate only ever
/// wanted the display name, so the new shape is wrapped exactly once.
pub(crate) fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}
use input::AudioInput;
use output::AudioOutput;

use crate::input_config::InputConfig;

/// Internal pipeline sample rate (Opus native).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Samples per voice frame at 48 kHz: 20 ms.
///
/// 20 ms halves the packet rate and per-packet header overhead of the old
/// 10 ms frames, and — the reason it matters under loss — lets Opus
/// in-band FEC (which carries only the *previous* frame) cover the single
/// packet a typical 20 ms burst loss removes, instead of one of two.
/// The 10 ms of extra algorithmic delay is smaller than the reorder hold
/// this release removed from the receive path.
pub const FRAME_SIZE: usize = 960;

/// One frame of pipeline PCM. Sent by value over channels: no per-frame
/// heap allocation, no cross-thread free.
pub(crate) type PcmFrame = [f32; FRAME_SIZE];

/// Incoming packet with the instant it entered the crate — the timing
/// point the jitter estimator relies on.
type Incoming = (AudioPacket, Instant);

/// Minimum spacing between automatic stream restart attempts, so a device
/// that is genuinely gone is retried calmly rather than hammered.
const RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// How often `service` re-checks the OS default device where there are no
/// change notifications (macOS/Linux).
const DEFAULT_DEVICE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Maximum expected peers. Used for channel sizing.
const DEFAULT_MAX_PEERS: usize = 32;

/// This build can open streams on a platform voice-call path (and so
/// get the OS echo canceller): Android today; Windows per
/// `AEC-SPEC.md` §4; macOS and Linux later (§9).
const PLATFORM_VOICE_PATH: bool = cfg!(any(target_os = "android", windows));

/// What the sound is coming out of, as the platform reports it
/// (`AEC-SPEC.md` §3.2, §4.2, §9). `Unknown` where nobody asks yet.
fn platform_output_kind(_output_device: Option<&str>) -> aec::OutputKind {
    #[cfg(target_os = "android")]
    {
        // The route the call will take (the picker's choice, else a
        // headset, else the loudspeaker), not merely what is plugged in
        // — with media-only Bluetooth counted as headphones.
        let types = native::android::output_device_types().unwrap_or_else(|e| {
            debug!("output devices: {e}");
            Vec::new()
        });
        match native::android::communication_devices() {
            Ok(devices) => route::effective_output_kind(
                route::explicit(_output_device, &devices),
                &devices,
                &types,
            ),
            Err(e) => {
                debug!("communication devices: {e}");
                aec::android_output_kind(&types)
            }
        }
    }
    #[cfg(windows)]
    {
        native::wasapi::output_kind(_output_device)
    }
    #[cfg(not(any(target_os = "android", windows)))]
    {
        aec::OutputKind::Unknown
    }
}

/// Android: does this plan need communication mode, and on which route
/// (`route::label`)? The call path needs it (`wanted`); so does a
/// picked device, even with echo cancellation off — the earpiece, or
/// the loudspeaker over a connected headset, are reachable only in that
/// mode (plain streams are then routed with it as media).
#[cfg(target_os = "android")]
fn android_comm_plan(selection: Option<&str>, wanted: bool) -> (bool, Option<String>) {
    match native::android::communication_devices() {
        Ok(devices) => {
            let need = wanted || route::explicit(selection, &devices).is_some();
            let label = need
                .then(|| route::choose(selection, &devices).map(|i| route::label(&devices[i])))
                .flatten();
            (need, label)
        }
        Err(e) => {
            debug!("communication devices: {e}");
            (wanted, None)
        }
    }
}

/// Clonable, thread-safe handle for feeding received packets into the
/// engine from another thread. Stamps arrival on send.
#[derive(Clone)]
pub struct IncomingSender {
    tx: Sender<Incoming>,
    stats: Arc<AudioStats>,
}

impl IncomingSender {
    /// Non-blocking; returns `false` if the engine's inbound queue is full
    /// or the engine is gone (dropping is correct real-time behaviour).
    #[must_use]
    pub fn send(&self, packet: AudioPacket) -> bool {
        // Counted at intake: "packets received from the network", which is
        // also correct while deafened or before an output device exists.
        self.stats.record_packet_received(packet.opus_data.len());
        match self.tx.try_send((packet, Instant::now())) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }
}

/// The main voice chat engine.
///
/// Owns the full audio pipeline from capture to playback.
/// The caller is responsible for shuttling [`AudioPacket`]s to/from the network.
pub struct VoiceChat {
    input: Option<AudioInput>,
    output: Option<AudioOutput>,
    input_config: Arc<InputConfig>,
    // ── Channels (each side stored separately, owned by who uses it) ─
    audio_to_encoder_tx: Sender<PcmFrame>,
    encoder_to_network_rx: Receiver<AudioPacket>,
    network_to_output_tx: Sender<Incoming>,
    network_to_output_rx: Receiver<Incoming>,
    /// Mic self-monitor frames, input worker → output worker.
    loopback_tx: Sender<PcmFrame>,
    loopback_rx: Receiver<PcmFrame>,
    sound_tx: Sender<sounds::SoundType>,
    sound_rx: Option<Receiver<sounds::SoundType>>,

    // ── Thread handles ───────────────────────────────────────────────
    encoder_thread: Option<std::thread::JoinHandle<()>>,

    // ── Shared state ─────────────────────────────────────────────────
    user_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,
    stats: Arc<AudioStats>,
    is_speaking: Arc<AtomicBool>,
    output_gain: Arc<AtomicU32>,
    deafened: Arc<AtomicBool>,

    // ── Device supervision (see `service`) ───────────────────────────
    /// OS default-device notifications (Windows). `None` = poll.
    #[cfg(windows)]
    device_watcher: Option<native::wasapi::DeviceWatcher>,
    /// Names of the devices actually opened, for follow-the-default.
    opened_input: Option<String>,
    opened_output: Option<String>,
    /// Capture was asked for and never opened (no microphone; on
    /// Android the permission not granted yet): `service()` keeps
    /// trying on the restart backoff until it does, playback running.
    input_pending: bool,
    /// Last default-device poll (non-Windows) and last restart attempt.
    last_device_poll: Instant,
    last_restart_attempt: Instant,

    // ── Echo cancellation (`aec`, AEC-SPEC.md) ───────────────────────
    /// The plan the open streams were opened with: on the platform's
    /// voice-call path or not, and the output that decided it.
    aec_wanted: bool,
    aec_output: aec::OutputKind,
    /// Android: whether the plan holds communication mode, and the
    /// route (`route::label`) it points at — a change (a headset
    /// connected, another picked) re-plans even when the output kind
    /// stays the same.
    #[cfg(target_os = "android")]
    aec_comm: bool,
    #[cfg(target_os = "android")]
    aec_route: Option<String>,
    /// The plan no longer matches the output (or the setting changed):
    /// `service()` reopens both streams.
    aec_replan: bool,
    /// Android: the phone held in communication mode while the
    /// voice-call streams are open. Entered before they open, dropped
    /// after they close.
    #[cfg(target_os = "android")]
    comm_mode: Option<native::android::CommunicationMode>,
    /// Windows: the device watcher follows the Communications default
    /// while this is set (the voice-call path), Console otherwise.
    #[cfg(windows)]
    aec_communications: Arc<AtomicBool>,

    config: VoiceChatConfig,
    /// Holds the OS timer at 1 ms resolution for the session (Windows).
    _timer_guard: platform::TimerResolutionGuard,
}

impl VoiceChat {
    pub fn new(config: VoiceChatConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let max_peers = DEFAULT_MAX_PEERS;

        // ── Build channels ──────────────────────────────────────────
        let (audio_to_encoder_tx, audio_to_encoder_rx) = bounded::<PcmFrame>(4);
        let (encoder_to_network_tx, encoder_to_network_rx) = bounded(8);
        // Inbound packets wait here only until the output worker's next
        // hop (≤ 10 ms): 32 peers × 50 pkt/s × 10 ms ≈ 16 in flight, with
        // room for scheduler slop.
        let (network_to_output_tx, network_to_output_rx) = bounded::<Incoming>(max_peers * 8);
        let (loopback_tx, loopback_rx) = bounded::<PcmFrame>(8);
        let (sound_tx, sound_rx) = bounded::<sounds::SoundType>(4);

        // ── Shared atomics (created once, passed to children) ───────
        let output_gain = Arc::new(AtomicU32::new(config.output_gain.to_bits()));
        let deafened = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AudioStats::new());
        let removed_peers = Arc::new(Mutex::new(HashSet::new()));

        // ── Spawn encoder thread ────────────────────────────────────
        let encoder_thread =
            encoder::spawn(audio_to_encoder_rx, encoder_to_network_tx, stats.clone());

        let input_config = Arc::new(InputConfig::new(
            config.noise_suppression,
            config.input_gain,
            config.gate_threshold,
            config.compressor_enabled,
            config.agc_enabled,
            config.agc_target_rms,
        ));

        #[cfg(windows)]
        let aec_communications = Arc::new(AtomicBool::new(false));

        Ok(Self {
            input: None,
            output: None,
            audio_to_encoder_tx,
            encoder_to_network_rx,
            network_to_output_tx,
            network_to_output_rx,
            loopback_tx,
            loopback_rx,
            sound_tx,
            input_config,
            sound_rx: Some(sound_rx),
            encoder_thread: Some(encoder_thread),
            user_settings: Arc::new(Mutex::new(HashMap::new())),
            removed_peers,
            #[cfg(windows)]
            device_watcher: match native::wasapi::DeviceWatcher::start(
                stats.clone(),
                aec_communications.clone(),
            ) {
                Ok(w) => Some(w),
                Err(e) => {
                    debug!("device notifications unavailable ({e}); polling instead");
                    None
                }
            },
            opened_input: None,
            opened_output: None,
            input_pending: false,
            last_device_poll: Instant::now(),
            last_restart_attempt: Instant::now().checked_sub(RESTART_BACKOFF).unwrap(),
            aec_wanted: false,
            aec_output: aec::OutputKind::Unknown,
            #[cfg(target_os = "android")]
            aec_comm: false,
            #[cfg(target_os = "android")]
            aec_route: None,
            aec_replan: false,
            #[cfg(target_os = "android")]
            comm_mode: None,
            #[cfg(windows)]
            aec_communications,
            stats,
            is_speaking: Arc::new(AtomicBool::new(false)),
            output_gain,
            deafened,
            config,
            _timer_guard: platform::TimerResolutionGuard::acquire(),
        })
    }

    /// Start the audio pipeline (capture + playback).
    ///
    /// The encoder thread is already running from `new()`.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Decide the voice-call path before anything opens: on Android
        // the phone's mode has to be set first.
        self.plan_aec();
        // Output first, and don't let a missing microphone abort playback:
        // a user with no capture device should still hear the room. (Field
        // incident: a machine with zero enabled recording endpoints went
        // completely silent because input was started first and `?` bailed
        // before output ever came up.)
        let output_result = self.start_output();
        let input_result = self.start_input();

        if let Err(e) = &output_result {
            log::error!("Voice output failed to start: {e}");
        }
        if let Err(e) = &input_result {
            log::error!(
                "Voice input failed to start (playback {}): {e}",
                if output_result.is_ok() {
                    "is still running"
                } else {
                    "also failed"
                }
            );
            // Retried from `service()` — see `input_pending`.
            self.input_pending = true;
        }

        // Report the first failure, but note: callers that get an Err while
        // output started successfully can keep this instance alive and hear
        // peers — dropping it on input failure silences everything.
        output_result?;
        input_result?;
        Ok(())
    }

    fn start_input(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut input = AudioInput::new(
            self.config.input_device.as_deref(),
            self.audio_to_encoder_tx.clone(),
            self.loopback_tx.clone(),
            self.stats.clone(),
            self.input_config.clone(),
            self.aec_wanted,
        )?;
        #[cfg(windows)]
        input.set_aec_reference(self.config.output_device.clone());

        input.start()?;
        self.is_speaking = input.speaking_handle();
        self.opened_input = input.opened_device_name();
        self.input = Some(input);
        self.stats
            .default_input_changed
            .store(false, Ordering::Relaxed);
        self.update_aec_report();
        Ok(())
    }

    fn start_output(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sound_rx = self.sound_rx.take().unwrap_or_else(|| {
            let (_, rx) = bounded::<sounds::SoundType>(1);
            rx
        });

        // On a phone the selection is a route (`route`), applied with
        // communication mode; the stream itself opens on the default.
        #[cfg(target_os = "android")]
        let device = None;
        #[cfg(not(target_os = "android"))]
        let device = self.config.output_device.as_deref();
        let mut output = AudioOutput::new(
            device,
            self.network_to_output_rx.clone(),
            self.loopback_rx.clone(),
            sound_rx,
            self.user_settings.clone(),
            self.removed_peers.clone(),
            self.stats.clone(),
            self.config.output_gain,
            self.aec_wanted,
        )?;
        self.output_gain = output.output_gain_handle();
        self.deafened = output.deafened_handle();
        output.start()?;
        self.opened_output = output.opened_device_name();
        self.stats
            .default_output_changed
            .store(false, Ordering::Relaxed);
        self.output = Some(output);
        // Whether the route held once a call stream exists (some
        // phones settle it only then).
        #[cfg(target_os = "android")]
        if self.comm_mode.is_some() {
            match native::android::current_route() {
                Ok(route) => info!("Android audio: playing to {route}"),
                Err(e) => debug!("communication device: {e}"),
            }
        }
        Ok(())
    }

    // ── Echo cancellation ────────────────────────────────────────────────

    /// Change the echo-cancellation setting. Reopens both streams when
    /// the voice-call decision changes (a no-op otherwise).
    pub fn set_echo_cancellation(&mut self, mode: aec::AecMode) {
        if mode == self.config.echo_cancellation {
            return;
        }
        self.config.echo_cancellation = mode;
        if self.input.is_none() && self.output.is_none() {
            return;
        }
        let output = platform_output_kind(self.config.output_device.as_deref());
        if PLATFORM_VOICE_PATH && aec::wants_aec(mode, output) != self.aec_wanted {
            self.restart_all("echo cancellation setting changed");
        } else {
            // Same streams, new words in the report.
            self.update_aec_report();
        }
    }

    /// The current echo-cancellation report (`None` = no capture open).
    #[must_use]
    pub fn aec_report(&self) -> Option<aec::AecReport> {
        self.stats.aec_report()
    }

    /// Decide the voice-call path for the streams about to open, and
    /// put the platform in the matching state. Call with both streams
    /// closed.
    fn plan_aec(&mut self) {
        let output = platform_output_kind(self.config.output_device.as_deref());
        let wanted = PLATFORM_VOICE_PATH && aec::wants_aec(self.config.echo_cancellation, output);
        self.aec_output = output;
        self.aec_wanted = wanted;
        self.aec_replan = false;
        #[cfg(windows)]
        self.aec_communications.store(wanted, Ordering::Relaxed);
        #[cfg(target_os = "android")]
        {
            // Leave first: a route change re-enters with the new route.
            self.comm_mode = None;
            let (need, label) = android_comm_plan(self.config.output_device.as_deref(), wanted);
            self.aec_comm = need;
            self.aec_route = label;
            if need {
                match native::android::CommunicationMode::enter(
                    self.config.output_device.as_deref(),
                ) {
                    Ok((mode, _route)) => self.comm_mode = Some(mode),
                    // The streams still open on the voice-call path; the
                    // report says whether the canceller engaged.
                    Err(e) => log::warn!("communication mode unavailable: {e}"),
                }
            }
        }
        info!(
            "Echo cancellation: {:?} on {}: {}",
            self.config.echo_cancellation,
            output,
            if wanted {
                "voice-call path"
            } else {
                "plain streams"
            }
        );
    }

    /// Would a fresh plan differ from the one the streams use?
    fn aec_plan_stale(&self) -> bool {
        if !PLATFORM_VOICE_PATH {
            return false;
        }
        let output = platform_output_kind(self.config.output_device.as_deref());
        let wanted = aec::wants_aec(self.config.echo_cancellation, output);
        // The output itself matters while on the call path: it decides
        // the route.
        if wanted != self.aec_wanted || (wanted && output != self.aec_output) {
            return true;
        }
        // On a phone, so does the device: wired → Bluetooth keeps the
        // kind (headphones) but needs the new route; a pick can need
        // communication mode on its own.
        #[cfg(target_os = "android")]
        {
            let (need, label) = android_comm_plan(self.config.output_device.as_deref(), wanted);
            if need != self.aec_comm || (need && label != self.aec_route) {
                return true;
            }
        }
        false
    }

    /// Close both streams, re-plan, reopen both.
    fn restart_all(&mut self, why: &str) {
        info!("Reopening voice streams ({why})");
        self.input.take();
        self.output.take();
        self.plan_aec();
        if let Err(e) = self.reopen_output() {
            error!("Output reopen failed ({e}); will retry");
            // Nothing else would: `service` restarts an output that
            // exists, and this one does not.
            self.aec_replan = true;
        }
        let input_sel = self.config.input_device.clone();
        if let Err(e) = self.set_input_device(input_sel.as_deref()) {
            error!("Input reopen failed ({e}); will retry");
            self.input_pending = true;
            self.update_aec_report();
        }
        self.stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
    }

    /// Fill `stats.aec` from the plan and what capture got (D5:
    /// verified, not assumed). Logged when it changes.
    fn update_aec_report(&self) {
        let mode = self.config.echo_cancellation;
        let output = self.aec_output;
        let report = if self.input.is_none() {
            None
        } else if !PLATFORM_VOICE_PATH {
            Some(aec::AecReport {
                mode,
                tier: aec::AecTier::None,
                output,
                reference: None,
                reason: aec::reasons::UNSUPPORTED,
            })
        } else if !self.aec_wanted {
            Some(aec::AecReport::not_wanted(mode, output))
        } else {
            let verdict = self
                .input
                .as_ref()
                .and_then(AudioInput::aec_verdict)
                .unwrap_or(Err(aec::reasons::PATH_REFUSED));
            Some(match verdict {
                Ok(()) => aec::AecReport {
                    mode,
                    tier: aec::AecTier::Platform,
                    output,
                    reference: self.opened_output.clone(),
                    reason: match (mode, output) {
                        (aec::AecMode::Always, aec::OutputKind::Headphones) => aec::reasons::ALWAYS,
                        (_, aec::OutputKind::Unknown) => aec::reasons::UNKNOWN_OUTPUT,
                        _ => aec::reasons::SPEAKERS,
                    },
                },
                Err(reason) => aec::AecReport {
                    mode,
                    tier: aec::AecTier::None,
                    output,
                    reference: None,
                    reason,
                },
            })
        };
        let Ok(mut slot) = self.stats.aec.lock() else {
            return;
        };
        if *slot != report {
            if let Some(r) = &report {
                info!("aec: {}", r.summary());
            }
            *slot = report;
            self.stats.aec_generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Playback is open. A caller whose `start()` failed on the capture
    /// side only should keep this instance: the room stays audible and
    /// the microphone is retried by `service()`.
    #[must_use]
    pub fn output_running(&self) -> bool {
        self.output.is_some()
    }

    /// Capture is open.
    #[must_use]
    pub fn input_running(&self) -> bool {
        self.input.is_some()
    }

    // ── Public API ───────────────────────────────────────────────────────

    pub fn play_sound(&self, sound: SoundType) {
        let _ = self.sound_tx.try_send(sound);
    }

    /// Feed a packet received from the network. Arrival is stamped here —
    /// call it as close to the socket read as possible; the jitter
    /// estimator measures from this instant.
    pub fn receive_packet(&self, packet: AudioPacket) {
        self.stats.record_packet_received(packet.opus_data.len());
        let _ = self.network_to_output_tx.try_send((packet, Instant::now()));
    }

    pub fn take_outgoing_packets(&self) -> Vec<AudioPacket> {
        let mut packets = Vec::new();
        while let Ok(p) = self.encoder_to_network_rx.try_recv() {
            packets.push(p);
        }
        packets
    }

    pub fn outgoing_receiver(&self) -> Receiver<AudioPacket> {
        self.encoder_to_network_rx.clone()
    }

    /// Handle for feeding packets from another thread (see
    /// [`IncomingSender::send`]).
    pub fn incoming_sender(&self) -> IncomingSender {
        IncomingSender {
            tx: self.network_to_output_tx.clone(),
            stats: self.stats.clone(),
        }
    }

    pub fn set_peer_settings(&self, peer_id: u64, settings: PeerAudioSettings) {
        if let Ok(mut s) = self.user_settings.lock() {
            s.insert(peer_id, settings);
        }
    }

    pub fn add_peer(&self, peer_id: u64) {
        if let Ok(mut s) = self.user_settings.lock() {
            s.entry(peer_id).or_insert_with(PeerAudioSettings::default);
        }
        if let Ok(mut r) = self.removed_peers.lock() {
            r.remove(&peer_id);
        }
        info!("Peer {peer_id} added");
    }

    pub fn remove_peer(&self, peer_id: u64) {
        if let Ok(mut r) = self.removed_peers.lock() {
            r.insert(peer_id);
        }
        if let Ok(mut s) = self.user_settings.lock() {
            s.remove(&peer_id);
        }
        info!("Peer {peer_id} removed");
    }

    /// Report the loss on our *outbound* path, as a percentage (0–100).
    ///
    /// The encoder folds this into its Opus FEC hint (see
    /// `encoder::maybe_update_fec`). Without it the hint is derived only
    /// from loss we *observe* on received streams, which says nothing
    /// about what listeners hear when our own uplink is the lossy leg.
    /// Callers should feed the transport's ack-derived loss estimate
    /// here whenever it refreshes (every couple of seconds is plenty).
    /// Prefer *clamping* an extreme reading over withholding it: each
    /// write also refreshes a staleness timestamp, and a value nobody
    /// refreshes for ~10 s is treated as unknown by the encoder's rate
    /// controller (the estimate decays instead of latching). Withholding
    /// during a bad stretch therefore reads as "no data", not "no loss"
    /// — safe, but blind.
    /// Round-trip time to the server/peer as the transport measures it,
    /// in milliseconds. Feeds the rate controller's congestion detector
    /// (loss *with* RTT inflation = queues filling = send fewer bytes,
    /// never more redundancy). Call it whenever the transport updates its
    /// estimate; 0 means unknown.
    pub fn set_link_rtt_ms(&self, rtt_ms: u32) {
        self.stats.link_rtt_ms.store(rtt_ms, Ordering::Relaxed);
    }

    pub fn set_uplink_loss_pct(&self, loss_pct: u32) {
        self.stats
            .uplink_loss_pct
            .store(loss_pct.min(100), Ordering::Relaxed);
        // Freshness stamp for the consumer-side staleness check; `max(1)`
        // keeps 0 reserved for "never written".
        let now_ms = u64::try_from(self.stats.start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.stats
            .uplink_loss_updated_ms
            .store(now_ms.max(1), Ordering::Relaxed);
    }

    pub fn set_noise_suppression(&self, enabled: bool) {
        self.input_config
            .denoise_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn set_input_gain(&self, gain: f32) {
        self.input_config
            .input_gain
            .store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_gate_threshold(&self, threshold: f32) {
        self.input_config
            .gate
            .threshold
            .store(threshold.clamp(0.001, 0.5).to_bits(), Ordering::Relaxed);
    }

    pub fn set_compressor_enabled(&self, enabled: bool) {
        self.input_config
            .compressor_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn set_compressor_threshold(&self, threshold_db: f32) {
        self.input_config
            .compressor
            .threshold_db
            .store(threshold_db.clamp(-60.0, 0.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_compressor_ratio(&self, ratio: f32) {
        self.input_config
            .compressor
            .ratio
            .store(ratio.clamp(1.0, 20.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_agc_enabled(&self, enabled: bool) {
        self.input_config
            .agc_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn set_loopback(&self, enabled: bool) {
        self.input_config
            .loopback_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn is_loopback(&self) -> bool {
        self.input_config.loopback_enabled.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.input_config.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.is_speaking.store(false, Ordering::Relaxed);
        }
    }

    pub fn is_muted(&self) -> bool {
        self.input_config.muted.load(Ordering::Relaxed)
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
    }

    pub fn is_deafened(&self) -> bool {
        self.deafened.load(Ordering::Relaxed)
    }

    pub fn set_output_gain(&self, gain: f32) {
        let clamped = gain.clamp(0.0, 2.0);
        self.output_gain.store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn is_speaking(&self) -> bool {
        if self.is_muted() {
            return false;
        }
        self.is_speaking.load(Ordering::Relaxed)
    }

    pub fn input_level(&self) -> f32 {
        self.input.as_ref().map_or(0.0, |i| {
            f32::from_bits(i.input_level_handle().load(Ordering::Relaxed))
        })
    }

    pub fn output_level(&self) -> f32 {
        f32::from_bits(self.stats.output_level.load(Ordering::Relaxed))
    }

    pub fn denoised_level(&self) -> f32 {
        f32::from_bits(self.stats.denoised_level.load(Ordering::Relaxed))
    }

    pub fn stats_summary(&self) -> String {
        self.stats.get_summary()
    }

    /// Names of the devices currently open, `(input, output)`.
    #[must_use]
    pub fn opened_devices(&self) -> (Option<String>, Option<String>) {
        (self.opened_input.clone(), self.opened_output.clone())
    }

    /// Device supervision. Call this from any periodic tick (100 ms – 1 s
    /// is fine; it is cheap when nothing changed). It:
    ///
    /// - restarts a side whose stream failed (device unplugged, invalidated,
    ///   repeated engine errors) on the same selection, with a 2 s backoff;
    /// - follows the OS default device when no device was explicitly
    ///   chosen — via notifications on Windows, a slow poll elsewhere;
    /// - keeps retrying an explicitly chosen device that has gone away,
    ///   so it comes back when plugged in.
    ///
    /// Returns `true` if a restart was performed.
    pub fn service(&mut self) -> bool {
        let now = Instant::now();
        let stats = self.stats.clone();

        self.poll_devices(now);

        let input_needs = self.input.is_some()
            && (stats.input_stream_failed.load(Ordering::Relaxed)
                || (self.config.input_device.is_none()
                    && stats.default_input_changed.load(Ordering::Relaxed)));
        let output_needs = self.output.is_some()
            && (stats.output_stream_failed.load(Ordering::Relaxed)
                || (self.config.output_device.is_none()
                    && stats.default_output_changed.load(Ordering::Relaxed)));
        // A capture side that never opened is not a failed stream (there
        // is none) — it is a device or permission that may show up later.
        let input_late = self.input.is_none() && self.input_pending;
        if !input_needs && !output_needs && !input_late && !self.aec_replan {
            return false;
        }
        if now.duration_since(self.last_restart_attempt) < RESTART_BACKOFF {
            return false;
        }
        self.last_restart_attempt = now;

        // A side going down usually means the output changed; if the
        // voice-call plan changes with it, both sides reopen together
        // (AEC-SPEC.md §4.5: re-evaluated on every reopen).
        if self.aec_replan || ((input_needs || output_needs) && self.aec_plan_stale()) {
            stats.input_stream_failed.store(false, Ordering::Relaxed);
            stats.output_stream_failed.store(false, Ordering::Relaxed);
            stats.default_input_changed.store(false, Ordering::Relaxed);
            stats.default_output_changed.store(false, Ordering::Relaxed);
            self.restart_all("output changed");
            return true;
        }

        let mut restarted = false;
        if input_late {
            match self.start_input() {
                Ok(()) => {
                    self.input_pending = false;
                    stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
                    info!(
                        "Input started late ({})",
                        self.opened_input.as_deref().unwrap_or("default device")
                    );
                    restarted = true;
                }
                // Every 2 s until it works: debug, or the log fills up.
                Err(e) => debug!("input still unavailable: {e}"),
            }
        }
        if input_needs {
            let why = if stats.input_stream_failed.load(Ordering::Relaxed) {
                "stream failed"
            } else {
                "default device changed"
            };
            info!("Restarting input ({why})");
            let selection = self.config.input_device.clone();
            match self.set_input_device(selection.as_deref()) {
                Ok(()) => {
                    stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
                    restarted = true;
                }
                Err(e) => error!("Input restart failed ({e}); will retry"),
            }
        }
        if output_needs {
            let why = if stats.output_stream_failed.load(Ordering::Relaxed) {
                "stream failed"
            } else {
                "default device changed"
            };
            info!("Restarting output ({why})");
            match self.reopen_output() {
                Ok(()) => {
                    stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
                    restarted = true;
                }
                Err(e) => error!("Output restart failed ({e}); will retry"),
            }
        }
        restarted
    }

    /// The slow poll where the OS sends no events (everything but
    /// Windows): the voice-call plan against the output, and on desktop
    /// the default devices.
    fn poll_devices(&mut self, now: Instant) {
        // Slow poll for default-device changes where there are no events.
        let watching = {
            #[cfg(windows)]
            {
                self.device_watcher.is_some()
            }
            #[cfg(not(windows))]
            {
                false
            }
        };
        let poll_due =
            !watching && now.duration_since(self.last_device_poll) >= DEFAULT_DEVICE_POLL;
        if poll_due {
            self.last_device_poll = now;
            // The voice-call plan follows the output (headphones plugged
            // or pulled): on Android nothing else tells us, since a
            // rerouted stream need not fail.
            if (self.input.is_some() || self.output.is_some()) && self.aec_plan_stale() {
                self.aec_replan = true;
            }
        }
        // Android has no default device to follow (routing is the OS's),
        // and our voice-call streams are not named like cpal's default.
        #[cfg(not(target_os = "android"))]
        if poll_due {
            let host = cpal::default_host();
            if self.input.is_some() && self.config.input_device.is_none() {
                let current = host.default_input_device().and_then(|d| device_name(&d));
                if current.is_some() && current != self.opened_input {
                    self.stats
                        .default_input_changed
                        .store(true, Ordering::Relaxed);
                }
            }
            if self.output.is_some() && self.config.output_device.is_none() {
                let current = host.default_output_device().and_then(|d| device_name(&d));
                if current.is_some() && current != self.opened_output {
                    self.stats
                        .default_output_changed
                        .store(true, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn stats(&self) -> &Arc<AudioStats> {
        &self.stats
    }

    // ── Device hot-swap ──────────────────────────────────────────────────

    pub fn set_input_device(
        &mut self,
        device_name: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("set_input_device called with: {device_name:?}");
        self.input.take();
        self.config.input_device = device_name.map(String::from);
        match self.start_input() {
            Ok(()) => {
                info!("Input device changed successfully");
                Ok(())
            }
            Err(e) => {
                error!("start_input failed: {e}");
                Err(e)
            }
        }
    }

    pub fn set_output_device(
        &mut self,
        device_name: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(target_os = "android")]
        let changed = self.config.output_device.as_deref() != device_name;
        self.config.output_device = device_name.map(String::from);
        // A phone's "device" is a route for the call (`route`): picking
        // one re-plans communication mode, which reopens both streams —
        // only when something changed (the UI's device poll repeats the
        // current choice when the automatic route moves, which the
        // engine's own poll has already followed).
        #[cfg(target_os = "android")]
        if self.input.is_some() || self.output.is_some() {
            if changed || self.aec_plan_stale() {
                self.restart_all("output route changed");
            }
            return Ok(());
        }
        self.reopen_output()
    }

    /// Close and reopen playback on the configured output.
    fn reopen_output(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.output.take();
        let (sound_tx, sound_rx) = bounded::<sounds::SoundType>(4);
        self.sound_tx = sound_tx;
        self.sound_rx = Some(sound_rx);
        self.start_output()?;
        info!("Output device changed");
        Ok(())
    }

    pub fn list_input_devices() -> Result<Vec<(String, bool)>, Box<dyn std::error::Error>> {
        // Real enumeration is expensive (per-device property queries via
        // CoreAudio/WASAPI) and callers are known to poll this from UI
        // timers every couple of seconds — a plausible periodic staller
        // for the audio path, and definite log spam. Cache for 10 s:
        // callers can poll as fast as they like, the OS gets asked ~4×
        // less often, and a hot-plugged device shows up within 10 s.
        // (If the periodic receive-side underruns thin out after this
        // change, enumeration stalls were indeed the cause.)
        static CACHE: DeviceListCache = Mutex::new(None);
        if let Ok(cache) = CACHE.lock() {
            if let Some((at, list)) = cache.as_ref() {
                if at.elapsed() < std::time::Duration::from_secs(10) {
                    return Ok(list.clone());
                }
            }
        }

        let host = cpal::default_host();
        let default_name = host.default_input_device().and_then(|d| device_name(&d));
        let mut devices = Vec::new();
        for device in host.input_devices()? {
            if let Some(name) = device_name(&device) {
                let is_default = default_name.as_deref() == Some(&name);
                if let Ok(cfg) = device.default_input_config() {
                    debug!(
                        "  Input device: \"{name}\" {}Hz {}ch {:?}{}",
                        cfg.sample_rate(),
                        cfg.channels(),
                        cfg.sample_format(),
                        if is_default { " [DEFAULT]" } else { "" },
                    );
                }
                devices.push((name, is_default));
            }
        }
        debug!("Found {} input devices", devices.len());
        if let Ok(mut cache) = CACHE.lock() {
            *cache = Some((std::time::Instant::now(), devices.clone()));
        }
        Ok(devices)
    }

    pub fn list_output_devices() -> Result<Vec<(String, bool)>, Box<dyn std::error::Error>> {
        // A phone lists routes for the call, not sound cards (`route`):
        // cheap, and live (a headset shows up at once).
        #[cfg(target_os = "android")]
        return Ok(route::picker(&native::android::communication_devices()?));
        #[cfg(not(target_os = "android"))]
        Self::list_cpal_output_devices()
    }

    #[cfg(not(target_os = "android"))]
    fn list_cpal_output_devices() -> Result<Vec<(String, bool)>, Box<dyn std::error::Error>> {
        // Same 10 s cache as list_input_devices — see the note there.
        static CACHE: DeviceListCache = Mutex::new(None);
        if let Ok(cache) = CACHE.lock() {
            if let Some((at, list)) = cache.as_ref() {
                if at.elapsed() < std::time::Duration::from_secs(10) {
                    return Ok(list.clone());
                }
            }
        }

        let host = cpal::default_host();
        let default_name = host.default_output_device().and_then(|d| device_name(&d));
        let mut devices = Vec::new();
        for device in host.output_devices()? {
            if let Some(name) = device_name(&device) {
                let is_default = default_name.as_deref() == Some(&name);
                if let Ok(cfg) = device.default_output_config() {
                    debug!(
                        "  Output device: \"{name}\" {}Hz {}ch {:?}{}",
                        cfg.sample_rate(),
                        cfg.channels(),
                        cfg.sample_format(),
                        if is_default { " [DEFAULT]" } else { "" },
                    );
                }
                devices.push((name, is_default));
            }
        }
        debug!("Found {} output devices", devices.len());
        if let Ok(mut cache) = CACHE.lock() {
            *cache = Some((std::time::Instant::now(), devices.clone()));
        }
        Ok(devices)
    }
}

impl Drop for VoiceChat {
    fn drop(&mut self) {
        // Drop audio devices first — stops platform callbacks and joins the
        // input/output workers (the output worker owns every decoder).
        self.input.take();
        self.output.take();
        // Only now let the phone out of communication mode.
        #[cfg(target_os = "android")]
        self.comm_mode.take();

        // Drop the encoder's input sender to unblock its recv(); the input
        // worker's clone is already gone with `input`.
        let (dead_tx, _) = crossbeam_channel::bounded::<PcmFrame>(0);
        self.audio_to_encoder_tx = dead_tx;

        if let Some(thread) = self.encoder_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod headless_tests {
    use std::time::Duration as StdDuration;

    use super::*;

    /// Prove the intake path works with no audio devices: packets fed via
    /// the public API must register in stats without blocking.
    /// Mirrors exactly what client/voice.rs does with incoming packets.
    #[test]
    fn receive_path_processes_packets_headless() {
        let vc = VoiceChat::new(VoiceChatConfig::default()).expect("new");

        // Real Opus payloads (the decoder rejects garbage)
        let mut enc = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)
            .expect("encoder");
        let pcm: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.5)
            .collect();
        let mut buf = vec![0u8; 512];

        for seq in 0..20u32 {
            let n = enc.encode_float(&pcm, &mut buf).expect("encode");
            let pkt = AudioPacket::new(seq, seq * FRAME_SIZE as u32, 42, buf[..n].to_vec());
            vc.receive_packet(pkt);
            std::thread::sleep(StdDuration::from_millis(2));
        }
        std::thread::sleep(StdDuration::from_millis(100));

        let received = vc
            .stats()
            .packets_received
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(received >= 15, "intake only counted {received}/20 packets");

        // outgoing_receiver must be a live owned clone usable in select!
        let rx = vc.outgoing_receiver();
        assert!(rx.try_recv().is_err()); // empty, but connected
    }

    /// The headless path counts packets in the output worker, which only
    /// exists once `start()` succeeds — so without an output device the
    /// inbound queue must still accept packets without blocking or panicking.
    #[test]
    fn incoming_sender_never_blocks_without_output() {
        let vc = VoiceChat::new(VoiceChatConfig::default()).expect("new");
        let tx = vc.incoming_sender();
        for seq in 0..1000u32 {
            let _ = tx.send(AudioPacket::new(
                seq,
                seq * FRAME_SIZE as u32,
                1,
                vec![0xFC],
            ));
        }
    }
}

#[cfg(test)]
mod voice_loop_repro {
    use std::time::Duration as StdDuration;

    use crossbeam_channel::{bounded as cb_bounded, select};

    use super::*;

    enum Ctrl {
        Incoming(AudioPacket),
        Shutdown,
    }

    /// Replicates client/voice.rs's select loop shape (post-fix) against a
    /// real `VoiceChat`: per-iteration outgoing clone, three-arm select with
    /// dead-channel fallback, control drain, `take_outgoing_packets` poll.
    #[test]
    fn voice_loop_select_shape_processes_incoming() {
        let (ctrl_tx, ctrl_rx) = cb_bounded::<Ctrl>(64);
        let (stats_tx, stats_rx) = cb_bounded(1);
        let loop_thread = std::thread::spawn(move || {
            // VoiceChat is !Send (cpal streams) — created on the loop
            // thread, exactly like client/voice.rs does.
            let vc = VoiceChat::new(VoiceChatConfig::default()).expect("new");
            stats_tx.send(vc.stats.clone()).unwrap();
            let (_dead_tx, dead_rx) = cb_bounded::<AudioPacket>(1);
            let mut forwarded = 0u32;
            let mut running = true;
            while running {
                let outgoing = Some(vc.outgoing_receiver());
                select! {
                    recv(ctrl_rx) -> msg => match msg {
                        Ok(Ctrl::Incoming(p)) => vc.receive_packet(p),
                        Ok(Ctrl::Shutdown) | Err(_) => running = false,
                    },
                    recv(outgoing.as_ref().unwrap_or(&dead_rx)) -> pkt => {
                        if pkt.is_ok() { forwarded += 1; }
                    },
                    default(StdDuration::from_millis(5)) => {}
                }
                for _ in 0..32 {
                    match ctrl_rx.try_recv() {
                        Ok(Ctrl::Incoming(p)) => vc.receive_packet(p),
                        Ok(Ctrl::Shutdown) => running = false,
                        Err(_) => break,
                    }
                }
                let _ = vc.take_outgoing_packets();
                let _ = vc.input_level();
                let _ = vc.is_speaking();
            }
            forwarded
        });

        let mut enc = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)
            .expect("encoder");
        let pcm: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / 48000.0).sin() * 0.4)
            .collect();
        let mut buf = vec![0u8; 512];
        for seq in 0..30u32 {
            let n = enc.encode_float(&pcm, &mut buf).expect("encode");
            let pkt = AudioPacket::new(seq, seq * FRAME_SIZE as u32, 7, buf[..n].to_vec());
            ctrl_tx.send(Ctrl::Incoming(pkt)).unwrap();
            std::thread::sleep(StdDuration::from_millis(2));
        }
        std::thread::sleep(StdDuration::from_millis(100));
        ctrl_tx.send(Ctrl::Shutdown).unwrap();
        let _forwarded = loop_thread
            .join()
            .expect("loop thread must not panic or hang");

        let stats = stats_rx.recv().expect("stats from loop thread");
        let received = stats
            .packets_received
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            received >= 25,
            "loop only processed {received}/30 incoming packets"
        );
    }
}
