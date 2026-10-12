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
mod latch;
mod loopback;
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
pub use loopback::LOOPBACK_DELAY;
pub use platform::TimerResolutionGuard;
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
#[cfg(test)]
mod fake;
#[cfg(test)]
mod supervision_tests;
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
/// get the OS echo canceller): Android; Windows per `AEC-SPEC.md` §4;
/// iOS through the voice-processing unit (`IOS-SPEC.md` §6.3); macOS
/// and Linux later (§9).
const PLATFORM_VOICE_PATH: bool = cfg!(any(target_os = "android", target_os = "ios", windows));

/// Capture and playback on the voice-call path are one device object
/// (iOS's voice-processing unit), so a side that needs reopening takes
/// the other with it.
const SHARED_VOICE_UNIT: bool = cfg!(target_os = "ios");

/// The phones keep the microphone closed whenever nothing is sent
/// (ANDROID-SPEC.md D53, IOS-SPEC.md D51): the system shows an open
/// microphone to the user, muted or not, and every open stream costs
/// battery. Out of a voice room nothing records and nothing takes the
/// call path; muted in one, the microphone is closed (Android) or muted
/// on the voice unit (iOS). The desktop keeps its streams open
/// (PRODUCTION-PLAN.md §6, item 5) and only drops the samples out of a
/// room ([`VoiceChat::set_presence`]).
pub const RELEASES_MIC: bool = cfg!(any(target_os = "android", target_os = "ios"));

/// What kind of host this is, for the device decisions below: one
/// value per platform, fixed by the build. Kept as data instead of
/// `cfg` branches so the same decisions can be run under each
/// platform's rules on any machine — the engine's tests do (`fake`),
/// and until they could, four platforms' worth of rules were checked
/// on none (review P2-55).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Traits {
    /// [`PLATFORM_VOICE_PATH`].
    pub(crate) voice_path: bool,
    /// [`RELEASES_MIC`].
    pub(crate) releases_mic: bool,
    /// [`SHARED_VOICE_UNIT`].
    pub(crate) shared_unit: bool,
    /// Android: the output selection is a route for the call, applied
    /// with communication mode — the stream itself opens on the
    /// default — so a pick re-plans, and reopens both sides.
    pub(crate) routes: bool,
    /// Windows: capture names the output as the canceller's reference,
    /// and "Default" means the communications device unless echo
    /// cancellation is Off ([`communications_default`]).
    pub(crate) reference: bool,
    /// The desktop: with no device picked, the OS default is followed.
    /// A phone's routing is the system's.
    pub(crate) follows_default: bool,
}

impl Traits {
    /// This build's.
    pub(crate) const HOST: Self = Self {
        voice_path: PLATFORM_VOICE_PATH,
        releases_mic: RELEASES_MIC,
        shared_unit: SHARED_VOICE_UNIT,
        routes: cfg!(target_os = "android"),
        reference: cfg!(windows),
        follows_default: cfg!(not(any(target_os = "android", target_os = "ios"))),
    };
}

/// What the platform says about the output right now: asked when a
/// plan is made and when one is checked for having gone stale.
pub(crate) struct Survey {
    pub(crate) output: aec::OutputKind,
    /// Android: the devices a call can use, listed once for a whole
    /// plan (review P3-25). `None` when Android will not say, and
    /// everywhere else.
    pub(crate) devices: Option<Vec<route::RouteDevice>>,
}

/// The capture side as the engine holds it: the stream — or, in this
/// crate's tests, a stand-in that only remembers how it was opened.
// In a test build the second variant is a few words beside a stream.
#[cfg_attr(test, allow(clippy::large_enum_variant))]
enum Capture {
    Real(AudioInput),
    #[cfg(test)]
    Fake(fake::Side),
}

impl Capture {
    fn opened_device_name(&self) -> Option<String> {
        match self {
            Self::Real(input) => input.opened_device_name(),
            #[cfg(test)]
            Self::Fake(side) => side.name.clone(),
        }
    }

    fn aec_verdict(&self) -> Option<Result<(), &'static str>> {
        match self {
            Self::Real(input) => input.aec_verdict(),
            #[cfg(test)]
            Self::Fake(side) => side.verdict,
        }
    }

    fn aec_reference_named(&self) -> bool {
        match self {
            Self::Real(input) => input.aec_reference_named(),
            #[cfg(test)]
            Self::Fake(side) => side.reference_named,
        }
    }
}

/// The playback side, likewise.
#[cfg_attr(test, allow(clippy::large_enum_variant))]
enum Playback {
    Real(AudioOutput),
    #[cfg(test)]
    Fake(fake::Side),
}

impl Playback {
    fn opened_device_name(&self) -> Option<String> {
        match self {
            Self::Real(output) => output.opened_device_name(),
            #[cfg(test)]
            Self::Fake(side) => side.name.clone(),
        }
    }

    fn lowers_others(&self) -> bool {
        match self {
            Self::Real(output) => output.lowers_others(),
            #[cfg(test)]
            Self::Fake(side) => side.lowers_others,
        }
    }
}

/// What the sound is coming out of, as the platform reports it
/// (`AEC-SPEC.md` §3.2, §4.2, §9), under the echo-cancellation setting
/// `_mode` (Windows: which device "Default" is, [`communications_default`]).
/// `Unknown` where nobody asks yet.
#[cfg(not(target_os = "android"))]
fn platform_output_kind(_output_device: Option<&str>, _mode: aec::AecMode) -> aec::OutputKind {
    #[cfg(windows)]
    {
        native::wasapi::output_kind(_output_device, communications_default(_mode))
    }
    // The session's route: there is no device to pick on iOS.
    #[cfg(target_os = "ios")]
    {
        native::avsession::output_kind()
    }
    #[cfg(not(any(target_os = "ios", windows)))]
    {
        aec::OutputKind::Unknown
    }
}

/// Windows: "Default" — no device picked — means the Default
/// Communications Device unless echo cancellation is Off (AEC-SPEC.md
/// D4), and it is resolved once, from the setting, for everything that
/// reads it: the form factor Automatic decides by, the devices both
/// streams open (on the call path or not) and the default the watcher
/// follows. Automatic used to read the console default and then open
/// the communications one, so a headset set for calls beside speakers
/// got the canceller and the ducking (review P1-18). False elsewhere.
#[cfg(windows)]
fn communications_default(mode: aec::AecMode) -> bool {
    mode != aec::AecMode::Off
}

/// Android: the devices a call can use, listed once for a whole plan
/// — the output's kind, then the route. Each used to list them itself,
/// so the 2 s plan check asked Android three times (review P3-25).
/// `None` when Android will not say.
#[cfg(target_os = "android")]
fn android_devices() -> Option<Vec<route::RouteDevice>> {
    match native::android::communication_devices() {
        Ok(devices) => Some(devices),
        Err(e) => {
            debug!("communication devices: {e}");
            None
        }
    }
}

/// Android: the route the call will take (the picker's choice, else a
/// headset, else the loudspeaker), not merely what is plugged in — with
/// media-only Bluetooth counted as headphones.
#[cfg(target_os = "android")]
fn android_output_kind(
    selection: Option<&str>,
    devices: Option<&[route::RouteDevice]>,
) -> aec::OutputKind {
    let types = native::android::output_device_types().unwrap_or_else(|e| {
        debug!("output devices: {e}");
        Vec::new()
    });
    match devices {
        Some(devices) => {
            route::effective_output_kind(route::explicit(selection, devices), devices, &types)
        }
        None => aec::android_output_kind(&types),
    }
}

/// Android: does this plan need communication mode, and on which route
/// (`route::label`)? The call path needs it (`wanted`); so does a
/// picked device, even with echo cancellation off — the earpiece, or
/// the loudspeaker over a connected headset, are reachable only in that
/// mode (plain streams are then routed with it as media).
fn comm_plan(
    selection: Option<&str>,
    wanted: bool,
    devices: Option<&[route::RouteDevice]>,
) -> (bool, Option<String>) {
    match devices {
        Some(devices) => {
            let need = wanted || route::explicit(selection, devices).is_some();
            let label = need
                .then(|| route::choose(selection, devices).map(|i| route::label(&devices[i])))
                .flatten();
            (need, label)
        }
        None => (wanted, None),
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
// The flags are independent facts (a capture retry pending, the plan,
// the plan gone stale, the sides on the call path), not one state.
#[allow(clippy::struct_excessive_bools)]
pub struct VoiceChat {
    input: Option<Capture>,
    output: Option<Playback>,
    /// The platform's rules ([`Traits::HOST`], but in the tests).
    traits: Traits,
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
    /// The speaking light (one for the engine's life; each input's
    /// chain drives it), and the ring its changes sound on.
    is_speaking: Arc<processor::speaking::SpeakingFlag>,
    speaking_edges: Receiver<()>,
    output_gain: Arc<AtomicU32>,
    deafened: Arc<AtomicBool>,

    // ── Device supervision (see `service`) ───────────────────────────
    /// OS default-device notifications (Windows). `None` = poll.
    #[cfg(windows)]
    device_watcher: Option<native::wasapi::DeviceWatcher>,
    /// Names of the devices actually opened, for follow-the-default.
    opened_input: Option<String>,
    opened_output: Option<String>,
    /// Capture was asked for and is not open — it never opened (no
    /// microphone; on Android the permission not granted yet) or a
    /// reopen failed: `service()` keeps trying on the restart backoff
    /// until it does, playback running.
    input_pending: bool,
    /// The same for playback: a reopen failed (the device went away
    /// with nothing to replace it yet) and `service()` keeps trying.
    /// Without it a failed restart left the room silent until the app
    /// was restarted — `service()` only restarts a stream that exists.
    output_pending: bool,
    /// Last default-device poll (non-Windows) and last restart attempt.
    last_device_poll: Instant,
    last_restart_attempt: Instant,

    // ── Echo cancellation (`aec`, AEC-SPEC.md) ───────────────────────
    /// The plan the open streams were opened with: on the platform's
    /// voice-call path or not, and the output that decided it.
    aec_wanted: bool,
    aec_output: aec::OutputKind,
    /// The sides open on the voice-call path: the plan, unless the
    /// path could not be had at plan time (iOS: the voice-processing
    /// unit would not open), in which case they take plain streams and
    /// the report says the path was refused.
    voice_streams: bool,
    /// iOS: the voice-processing unit, opened by the plan before either
    /// side starts and held while it lasts, so both sides attach to it
    /// — never one on it and the other on a plain stream outside its
    /// reference (`native::vpio`).
    #[cfg(target_os = "ios")]
    voice_unit: Option<native::vpio::Prepared>,
    /// iOS on plain streams: the session's interruptions and resets,
    /// reported as failed streams so `service()` reopens them (the
    /// voice unit has its own).
    #[cfg(target_os = "ios")]
    plain_watch: Option<native::avsession::SessionWatch>,
    /// Android: whether the plan holds communication mode, and the
    /// route (`route::label`) it points at — a change (a headset
    /// connected, another picked) re-plans even when the output kind
    /// stays the same.
    aec_comm: bool,
    aec_route: Option<String>,
    /// The plan no longer matches the output (or the setting changed):
    /// `service()` reopens both streams.
    aec_replan: bool,
    /// Android: the phone held in communication mode while the
    /// voice-call streams are open. Entered before they open, dropped
    /// after they close.
    #[cfg(target_os = "android")]
    comm_mode: Option<native::android::CommunicationMode>,
    /// Windows: the plan's [`communications_default`]: "Default" means
    /// the Communications default for the streams it opens and for the
    /// device watcher while this is set, the Console one otherwise.
    #[cfg(windows)]
    aec_communications: Arc<AtomicBool>,
    /// The same fact for the engine's own decisions, on every host
    /// (false where "Default" has one meaning): written together with
    /// the watcher's copy ([`Self::set_comms_default`]).
    comms_default: bool,

    // ── What the phones keep open (`RELEASES_MIC`) ───────────────────
    /// In a voice room (`set_presence`). Out of one — the lobby, the
    /// AFK room — Android opens nothing and iOS plays through a plain
    /// output only (an app with no audio running is suspended in the
    /// background); the desktop keeps both streams and drops what the
    /// microphone hears (`InputConfig::in_room`).
    in_room: bool,
    /// The microphone is wanted: not muted, deafened or waiting for
    /// push-to-talk. Always true on the desktop.
    mic_wanted: bool,
    /// `start()` ran: presence changes act on the streams from then on.
    started: bool,

    config: VoiceChatConfig,
    /// Holds the OS timer at 1 ms resolution for the session (Windows).
    /// Held in a voice room ([`Self::sync_timer`]).
    timer_guard: Option<platform::TimerResolutionGuard>,
}

/// What [`VoiceChat::start`] opened. Not a `Result`: with playback up
/// and no microphone the engine is worth keeping, and an `Err` for that
/// case had every caller checking `output_running()` before deciding
/// (the ones that did not, dropped the engine and silenced the room —
/// review P3-24).
#[derive(Debug)]
#[must_use = "a start that opened no playback has nothing to keep"]
pub enum StartOutcome {
    /// Everything this session wanted.
    Started,
    /// Playback runs, the microphone did not open (none there, or the
    /// permission not given yet). Keep the engine: the room is audible
    /// and [`VoiceChat::service`] retries the microphone.
    NoMicrophone(String),
    /// Playback did not open. Nothing can be heard; a caller that keeps
    /// the engine anyway gets it retried by [`VoiceChat::service`].
    NoPlayback(String),
}

impl StartOutcome {
    /// Playback is up: the engine is worth keeping.
    #[must_use]
    pub fn audible(&self) -> bool {
        !matches!(self, Self::NoPlayback(_))
    }
}

/// Why the streams are being looked at ([`VoiceChat::reconcile`]).
#[derive(Clone, Copy, Debug)]
enum Cause {
    /// A phone's room or microphone changed (`set_presence`; the
    /// desktop keeps its streams). `path_moved`: into or out of what
    /// needs the call path or playback at all.
    Presence { path_moved: bool },
    /// A microphone was picked.
    InputPicked,
    /// An output was picked; `changed` when it is not the one already
    /// configured.
    OutputPicked { changed: bool },
    /// The echo-cancellation setting changed.
    Setting,
    /// A supervision tick found something ([`VoiceChat::service`]).
    Tick(Trouble),
}

/// What a supervision tick found wrong with the streams.
#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct Trouble {
    /// An open side whose stream failed, or whose default moved.
    input_needs: bool,
    output_needs: bool,
    /// A side that should be open and is not.
    input_late: bool,
    output_late: bool,
}

impl Trouble {
    fn any(self) -> bool {
        self.input_needs || self.output_needs || self.input_late || self.output_late
    }
}

/// What is to be done to the streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Keep,
    /// Close both, make the plan again, open what is wanted. The word
    /// is for the log.
    Replan(&'static str),
    /// Each side on the plan that stands.
    Sides {
        output: Side,
        input: Side,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Keep,
    Close,
    /// Not open: open it.
    Open,
    /// Open: close it and open it again.
    Reopen,
}

/// What a [`VoiceChat::reconcile`] did: each side that was opened, and
/// how that went. `None`: left as it was (or closed).
#[derive(Default)]
struct Done {
    /// Both sides went through a new plan.
    replanned: bool,
    output: Option<Result<(), Box<dyn std::error::Error>>>,
    input: Option<Result<(), Box<dyn std::error::Error>>>,
}

impl VoiceChat {
    /// Build the engine and start its encoder thread. Opens no device
    /// ([`Self::start`] does) and cannot fail.
    #[must_use]
    pub fn new(config: VoiceChatConfig) -> Self {
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
        let (is_speaking, speaking_edges) = processor::speaking::SpeakingFlag::new();

        Self {
            input: None,
            output: None,
            #[cfg(test)]
            traits: fake::traits().unwrap_or(Traits::HOST),
            #[cfg(not(test))]
            traits: Traits::HOST,
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
            output_pending: false,
            last_device_poll: Instant::now(),
            last_restart_attempt: Instant::now().checked_sub(RESTART_BACKOFF).unwrap(),
            aec_wanted: false,
            aec_output: aec::OutputKind::Unknown,
            voice_streams: false,
            #[cfg(target_os = "ios")]
            voice_unit: None,
            #[cfg(target_os = "ios")]
            plain_watch: None,
            aec_comm: false,
            aec_route: None,
            aec_replan: false,
            #[cfg(target_os = "android")]
            comm_mode: None,
            #[cfg(windows)]
            aec_communications,
            comms_default: false,
            in_room: true,
            mic_wanted: true,
            started: false,
            stats,
            is_speaking,
            speaking_edges,
            output_gain,
            deafened,
            config,
            timer_guard: None,
        }
    }

    /// Windows: the 1 ms system timer, for as long as we are in a voice
    /// room. It was held from connecting to leaving — the lobby
    /// included, hours of it for someone who only reads the chat — and
    /// a 1 ms timer keeps the whole machine from its deeper idle states
    /// (review P2-35). The short waits that needed it out of a room are
    /// gone (P1-1, P1-4); a share or a watched stream holds its own
    /// (`client::screen`).
    fn sync_timer(&mut self) {
        if self.started && self.in_room {
            self.timer_guard
                .get_or_insert_with(platform::TimerResolutionGuard::acquire);
        } else {
            self.timer_guard = None;
        }
    }

    /// Start the audio pipeline (capture + playback) and say what
    /// opened. A side that did not is retried by [`Self::service`].
    ///
    /// The encoder thread is already running from `new()`.
    pub fn start(&mut self) -> StartOutcome {
        self.started = true;
        self.sync_timer();
        // Output first, and a missing microphone never keeps playback
        // from opening: a user with no capture device should still
        // hear the room. (Field incident: a machine with zero enabled
        // recording endpoints went completely silent because input was
        // started first and `?` bailed before output ever came up.)
        let done = self.plan_and_open(None);
        if let Some(Err(e)) = &done.output {
            log::error!("Voice output failed to start: {e}");
        }
        if let Some(Err(e)) = &done.input {
            log::error!(
                "Voice input failed to start (playback {}): {e}",
                if matches!(done.output, Some(Err(_))) {
                    "also failed"
                } else {
                    "is still running"
                }
            );
        }
        match (&done.output, &done.input) {
            (Some(Err(e)), _) => StartOutcome::NoPlayback(e.to_string()),
            (_, Some(Err(e))) => StartOutcome::NoMicrophone(e.to_string()),
            _ => StartOutcome::Started,
        }
    }

    // ── One place for what is open (review P2-55) ────────────────────────
    //
    // Which streams are open, on which device and on which path, was
    // decided in six places — `start`, the presence, the two device
    // picks, the setting and `service` — each with its own copy of the
    // opening and closing, and the copies had drifted (P1-18 and P2-20
    // were two of the drifts). Now each of them records what it was
    // told and calls [`Self::reconcile`] with the cause: `decide` is
    // every rule side by side, and below it each thing the streams can
    // be made to do exists once.

    /// Bring the streams to what the configuration, the presence and
    /// the platform call for, given what just changed.
    fn reconcile(&mut self, cause: Cause) -> Done {
        match self.decide(cause) {
            Step::Keep => Done::default(),
            Step::Replan(why) => {
                if matches!(cause, Cause::Tick(_)) {
                    // What the tick saw is dealt with by reopening
                    // everything. Cleared before, not after: a stream
                    // that fails while reopening must still be seen.
                    let stats = &self.stats;
                    stats.input_stream_failed.store(false, Ordering::Relaxed);
                    stats.output_stream_failed.store(false, Ordering::Relaxed);
                    stats.default_input_changed.store(false, Ordering::Relaxed);
                    stats.default_output_changed.store(false, Ordering::Relaxed);
                }
                self.plan_and_open(Some(why))
            }
            Step::Sides { output, input } => self.apply_sides(output, input),
        }
    }

    /// What `cause` asks of the streams. Reads, and asks the platform
    /// only where a rule needs its answer (`aec_plan_stale`, the
    /// survey: device lists on Android, the route on iOS — not free,
    /// review P3-25); changes nothing.
    fn decide(&self, cause: Cause) -> Step {
        let open = self.input.is_some() || self.output.is_some();
        match cause {
            // A phone entering or leaving a voice room re-plans
            // everything (Android enters or leaves communication mode;
            // iOS moves between the voice-processing unit and a plain
            // output): a join or a leave is the moment for it.
            Cause::Presence { path_moved: true } => Step::Replan(if self.in_room {
                "in a voice room"
            } else {
                "out of the voice rooms"
            }),
            // Muted or unmuted in one: the microphone alone follows.
            Cause::Presence { path_moved: false } => Step::Sides {
                output: Side::Keep,
                input: match (self.capture_wanted(), self.input.is_some()) {
                    (true, false) => Side::Open,
                    (false, true) => Side::Close,
                    _ => Side::Keep,
                },
            },
            // A phone with its microphone closed (out of a room,
            // muted) keeps the choice for when it opens.
            Cause::InputPicked => Step::Sides {
                output: Side::Keep,
                input: if self.capture_wanted() {
                    Side::Reopen
                } else {
                    Side::Close
                },
            },
            Cause::OutputPicked { changed } => {
                if !self.playback_wanted() {
                    // Android out of a room plays nothing; the route
                    // is applied when a room is joined.
                    Step::Keep
                } else if self.traits.routes && open {
                    // A phone's "device" is a route for the call
                    // (`route`): picking one re-plans communication
                    // mode, which reopens both streams — only when
                    // something changed (the UI's device poll repeats
                    // the current choice when the automatic route
                    // moves, which the engine's own poll has already
                    // followed).
                    if changed || self.aec_plan_stale() {
                        Step::Replan("output route changed")
                    } else {
                        Step::Keep
                    }
                } else if self.traits.reference
                    && open
                    && ((changed && self.input.is_some() && self.voice_streams)
                        || self.aec_plan_stale())
                {
                    // Windows: the output decides the voice-call plan,
                    // and on the call path capture names it as the
                    // canceller's reference (or, none picked, has
                    // Windows follow the communications default). A
                    // pick that changes either reopens both sides:
                    // reopening playback alone left capture on the old
                    // plan and reference — no canceller after
                    // headphones → speakers, ducking after speakers →
                    // headset (review P1-18).
                    Step::Replan("output device changed")
                } else {
                    Step::Sides {
                        output: Side::Reopen,
                        input: Side::Keep,
                    }
                }
            }
            Cause::Setting => {
                let mode = self.config.echo_cancellation;
                let output = self.survey().output;
                // Windows: Off and the others read "Default" as
                // different devices; a side that follows it moves.
                let default_moved = self.communications_default(mode) != self.plan_communications()
                    && (self.config.input_device.is_none() || self.config.output_device.is_none());
                if self.traits.voice_path
                    && ((self.call_path_wanted() && aec::wants_aec(mode, output))
                        != self.aec_wanted
                        || default_moved)
                {
                    Step::Replan("echo cancellation setting changed")
                } else {
                    Step::Keep
                }
            }
            Cause::Tick(seen) => {
                // A side going down usually means the output changed;
                // if the voice-call plan changes with it, both sides
                // reopen together (AEC-SPEC.md §4.5: re-evaluated on
                // every reopen) — so does an output coming back as
                // another kind of device. So do they when both are one
                // unit (iOS, `SHARED_VOICE_UNIT`): an interruption
                // stops both at once.
                let one_unit = self.traits.shared_unit && self.voice_streams;
                if self.aec_replan
                    || ((seen.input_needs || seen.output_needs || seen.output_late)
                        && (one_unit || self.aec_plan_stale()))
                {
                    Step::Replan("output changed")
                } else {
                    Step::Sides {
                        output: if seen.output_late {
                            Side::Open
                        } else if seen.output_needs {
                            Side::Reopen
                        } else {
                            Side::Keep
                        },
                        input: if seen.input_late {
                            Side::Open
                        } else if seen.input_needs {
                            Side::Reopen
                        } else {
                            Side::Keep
                        },
                    }
                }
            }
        }
    }

    /// Close both streams, make the plan again, open what is wanted:
    /// playback first, each side on its own (one that does not open is
    /// left pending for [`Self::service`]). `why` goes to the log;
    /// `None` is the first start, when nothing was open and nothing is
    /// counted as a restart.
    fn plan_and_open(&mut self, why: Option<&str>) -> Done {
        if let Some(why) = why {
            info!("Reopening voice streams ({why})");
        }
        self.close_input();
        self.output.take();
        self.input_pending = false;
        self.output_pending = false;
        self.plan_aec();
        let restart = why.is_some();
        let output = self.playback_wanted().then(|| self.open_output(restart));
        let input = self.capture_wanted().then(|| self.open_input());
        if restart {
            // `service` retries playback alone: re-planning both every
            // 2 s closed and reopened a working microphone for as long
            // as the output stayed away. A plan the output's return
            // makes stale still reopens both.
            if let Some(Err(e)) = &output {
                error!("Output reopen failed ({e}); will retry");
            }
            if let Some(Err(e)) = &input {
                error!("Input reopen failed ({e}); will retry");
            }
            self.stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
        }
        if self.input.is_none() {
            // No capture, whatever the last report said.
            self.update_aec_report();
        }
        self.apply_unit_mute();
        Done {
            replanned: true,
            output,
            input,
        }
    }

    /// On the plan that stands, do to each side what it is told. A
    /// playback that is missing opens first — the room is silent until
    /// it does — then the microphone, then a playback that only needs
    /// opening again.
    fn apply_sides(&mut self, output: Side, input: Side) -> Done {
        let mut done = Done::default();
        if output == Side::Open {
            done.output = Some(self.open_output(true));
        }
        match input {
            Side::Keep => {}
            Side::Close => {
                self.close_input();
                self.update_aec_report();
            }
            Side::Open => done.input = Some(self.open_input()),
            Side::Reopen => {
                // Closed first: if the new device fails to open, the
                // speaking flag and the meter must not keep what the
                // old one last said (review P2-20).
                self.close_input();
                done.input = Some(self.open_input());
            }
        }
        if output == Side::Reopen {
            self.output.take();
            done.output = Some(self.open_output(true));
        }
        done
    }

    /// Open playback on the configured output. One that does not open
    /// is left pending: [`Self::service`] keeps trying. Without that a
    /// failed reopen left the room silent until the app was restarted
    /// (PRODUCTION-PLAN.md CLI-9).
    fn open_output(&mut self, fresh_sounds: bool) -> Result<(), Box<dyn std::error::Error>> {
        if fresh_sounds {
            // The sounds' channel went with the output that played
            // them, and what was queued for one that never opened is
            // old news by now.
            let (sound_tx, sound_rx) = bounded::<sounds::SoundType>(4);
            self.sound_tx = sound_tx;
            self.sound_rx = Some(sound_rx);
        }
        let opened = self.start_output();
        self.output_pending = opened.is_err();
        opened
    }

    /// Open the microphone on the configured input. One that does not
    /// open is left pending (a device, or a permission, that may show
    /// up later), and the report says there is no capture — it used to
    /// keep the closed stream's words after a failed pick.
    fn open_input(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let opened = self.start_input();
        if opened.is_err() {
            self.input_pending = true;
            self.update_aec_report();
        }
        opened
    }

    fn start_input(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let input = self.open_capture()?;
        self.opened_input = input.opened_device_name();
        self.input = Some(input);
        self.input_pending = false;
        self.stats
            .default_input_changed
            .store(false, Ordering::Relaxed);
        self.update_aec_report();
        Ok(())
    }

    /// Open the microphone the configuration and the plan call for.
    fn open_capture(&mut self) -> Result<Capture, Box<dyn std::error::Error>> {
        #[cfg(test)]
        if let Some(opened) = fake::open_capture(
            self.config.input_device.as_deref(),
            self.voice_streams,
            self.comms_default,
            self.config.output_device.as_deref(),
            &self.stats,
        ) {
            return opened.map(Capture::Fake);
        }
        let mut input = AudioInput::new(
            self.config.input_device.as_deref(),
            self.audio_to_encoder_tx.clone(),
            self.loopback_tx.clone(),
            self.stats.clone(),
            self.input_config.clone(),
            self.is_speaking.clone(),
            self.voice_streams,
        )?;
        #[cfg(windows)]
        {
            input.set_aec_reference(self.config.output_device.clone());
            input.set_communications_default(self.comms_default);
        }

        input.start()?;
        Ok(Capture::Real(input))
    }

    fn start_output(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sound_rx = self.sound_rx.take().unwrap_or_else(|| {
            let (_, rx) = bounded::<sounds::SoundType>(1);
            rx
        });
        let output = self.open_playback(sound_rx)?;
        self.opened_output = output.opened_device_name();
        self.publish_voice_output();
        self.stats
            .default_output_changed
            .store(false, Ordering::Relaxed);
        self.output = Some(output);
        self.output_pending = false;
        // Whether the route held once a call stream exists (some
        // phones settle it only then).
        #[cfg(target_os = "android")]
        if self.comm_mode.is_some() {
            match native::android::current_route() {
                Ok(route) => info!("Android audio: playing to {route}"),
                Err(e) => debug!("communication device: {e}"),
            }
        }
        // The report names the output as the canceller's reference: a
        // playback reopened under a running capture (a device change,
        // a failed stream) changes it. (At start, and whenever both go
        // through a new plan, capture opens after this and reports
        // then.)
        if self.input.is_some() {
            self.update_aec_report();
        }
        Ok(())
    }

    /// Open the playback stream the configuration and the plan call
    /// for, carrying on deafened and at the volume the last one had.
    fn open_playback(
        &mut self,
        sound_rx: Receiver<sounds::SoundType>,
    ) -> Result<Playback, Box<dyn std::error::Error>> {
        // On a phone the selection is a route (`route`), applied with
        // communication mode; the stream itself opens on the default.
        let device = if self.traits.routes {
            None
        } else {
            self.config.output_device.as_deref()
        };
        #[cfg(test)]
        if let Some(opened) =
            fake::open_playback(device, self.voice_streams, self.comms_default, &self.stats)
        {
            return opened.map(Playback::Fake);
        }
        let mut output = AudioOutput::new(
            device,
            self.network_to_output_rx.clone(),
            self.loopback_rx.clone(),
            sound_rx,
            self.user_settings.clone(),
            self.removed_peers.clone(),
            self.stats.clone(),
            self.config.output_gain,
            self.voice_streams,
        )?;
        #[cfg(windows)]
        output.set_communications_default(self.comms_default);
        // A reopened output (a device change; on a phone every move
        // between the lobby and a room) carries on deafened and at the
        // volume it was — its handles start from the config otherwise.
        let gain = self.output_gain.load(Ordering::Relaxed);
        let deafened = self.deafened.load(Ordering::Relaxed);
        self.output_gain = output.output_gain_handle();
        self.deafened = output.deafened_handle();
        self.output_gain.store(gain, Ordering::Relaxed);
        self.deafened.store(deafened, Ordering::Relaxed);
        output.start()?;
        Ok(Playback::Real(output))
    }

    /// A watched stream with no output picked plays where the voice
    /// does (`stream::decoder::OutputSelection`, review P1-19).
    fn publish_voice_output(&self) {
        // The tests' machine is not this process's: its device names
        // stay out of the shared slot.
        #[cfg(test)]
        if fake::active() {
            return;
        }
        if !self.traits.releases_mic {
            stream::decoder::VOICE_OUTPUT.set(self.opened_output.as_deref());
        }
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
        if !self.reconcile(Cause::Setting).replanned {
            // Same streams, new words in the report — and on Windows the
            // new meaning of "Default", which no open stream follows now
            // (both picked) but the next to open, or the watcher, will.
            self.set_comms_default(self.communications_default(mode));
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
        let survey = self.survey();
        let output = survey.output;
        let wanted = self.traits.voice_path
            && self.call_path_wanted()
            && aec::wants_aec(self.config.echo_cancellation, output);
        self.aec_output = output;
        self.aec_wanted = wanted;
        self.voice_streams = wanted;
        self.aec_replan = false;
        self.set_comms_default(self.communications_default(self.config.echo_cancellation));
        self.engage_call_path(wanted, &survey);
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

    /// Ask the platform where the sound goes now.
    fn survey(&self) -> Survey {
        #[cfg(test)]
        if let Some(survey) = fake::survey(
            self.config.output_device.as_deref(),
            self.config.echo_cancellation,
        ) {
            return survey;
        }
        #[cfg(target_os = "android")]
        {
            let devices = android_devices();
            let output =
                android_output_kind(self.config.output_device.as_deref(), devices.as_deref());
            Survey { output, devices }
        }
        #[cfg(not(target_os = "android"))]
        {
            Survey {
                output: platform_output_kind(
                    self.config.output_device.as_deref(),
                    self.config.echo_cancellation,
                ),
                devices: None,
            }
        }
    }

    /// [`communications_default`] under this host's rules.
    fn communications_default(&self, mode: aec::AecMode) -> bool {
        self.traits.reference && mode != aec::AecMode::Off
    }

    /// What "Default" means from here on: for the streams the engine
    /// opens next and, on Windows, for the device watcher.
    fn set_comms_default(&mut self, on: bool) {
        self.comms_default = on;
        #[cfg(windows)]
        self.aec_communications.store(on, Ordering::Relaxed);
    }

    /// Put the platform in the plan's state, before either stream
    /// opens: communication mode and its route (Android), the
    /// session's mode and the voice-processing unit (iOS). A call path
    /// that cannot be had is given up here — `voice_streams` goes
    /// false and both sides take plain streams.
    // The platforms' blocks need `self` and the survey; a host with
    // neither block uses neither.
    #[cfg_attr(
        not(any(test, target_os = "android", target_os = "ios")),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    // The tests' machine answers before the platforms' blocks, of
    // which the host the tests run on has none.
    #[cfg_attr(test, allow(clippy::needless_return))]
    fn engage_call_path(&mut self, wanted: bool, survey: &Survey) {
        // Android: does this plan hold communication mode, and on which
        // route? Out of a voice room there is no call to route at all.
        let (need, label) = if self.traits.routes && self.call_path_wanted() {
            comm_plan(
                self.config.output_device.as_deref(),
                wanted,
                survey.devices.as_deref(),
            )
        } else {
            (false, None)
        };
        self.aec_comm = need;
        self.aec_route = label;
        #[cfg(test)]
        if let Some(granted) = fake::engage(
            wanted,
            need,
            self.aec_route.as_deref(),
            self.traits.releases_mic && !self.mic_wanted,
        ) {
            self.voice_streams = wanted && granted;
            return;
        }
        // The session's mode goes with the plan: `voiceChat` for the
        // voice-processing unit, the default for cpal's plain streams.
        // The unit opens here, before either side: both attach to it,
        // or — if it will not open — neither does.
        #[cfg(target_os = "ios")]
        {
            self.voice_unit = None;
            self.plain_watch = None;
            native::avsession::set_voice_mode(wanted);
            if wanted {
                // Opened already muted when the microphone is not
                // wanted, so a re-plan never lets it record (D51).
                match native::vpio::prepare(&self.stats, RELEASES_MIC && !self.mic_wanted) {
                    Ok(unit) => self.voice_unit = Some(unit),
                    Err(e) => {
                        log::warn!("Voice processing unavailable ({e}); plain streams, no echo cancellation");
                        self.voice_streams = false;
                        native::avsession::set_voice_mode(false);
                    }
                }
            }
            // Plain streams have no unit watching the session: a call or
            // a media-server reset would leave the output stopped — and
            // the lobby's output is what keeps the app alive (D51).
            if self.voice_unit.is_none() {
                self.plain_watch = Some(native::avsession::SessionWatch::start(&self.stats));
            }
        }
        #[cfg(target_os = "android")]
        {
            // Leave first: a route change re-enters with the new route.
            self.comm_mode = None;
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
    }

    /// Would a fresh plan differ from the one the streams use?
    fn aec_plan_stale(&self) -> bool {
        if !self.traits.voice_path {
            return false;
        }
        // A phone out of the voice rooms wants no plan and opened its
        // streams without one: nothing can have gone stale, and the
        // platform is not asked — three device lists on Android, the
        // route on iOS, every 2 s in the lobby (review P3-25).
        if !self.call_path_wanted() && !self.aec_wanted {
            return false;
        }
        let survey = self.survey();
        let output = survey.output;
        let wanted =
            self.call_path_wanted() && aec::wants_aec(self.config.echo_cancellation, output);
        // The output itself matters while on the call path: it decides
        // the route.
        if wanted != self.aec_wanted || (wanted && output != self.aec_output) {
            return true;
        }
        // On a phone, so does the device: wired → Bluetooth keeps the
        // kind (headphones) but needs the new route; a pick can need
        // communication mode on its own.
        if self.traits.routes && self.call_path_wanted() {
            let (need, label) = comm_plan(
                self.config.output_device.as_deref(),
                wanted,
                survey.devices.as_deref(),
            );
            if need != self.aec_comm || (need && label != self.aec_route) {
                return true;
            }
        }
        false
    }

    // ── What the phones keep open ────────────────────────────────────────

    /// Where we are and whether the microphone is wanted. On the
    /// desktop only the room counts, and the streams stay as they are:
    /// out of one, capture drops its samples like mute — nothing is
    /// denoised, encoded or sent for the relay to throw away (review
    /// P1-6); mute itself is [`Self::set_muted`]'s. On a phone
    /// ([`RELEASES_MIC`]) it is what stays open. Before `start()` it
    /// only records them; after, it opens and closes what changed:
    ///
    /// - into or out of a voice room: everything re-plans (Android
    ///   enters or leaves communication mode; iOS moves between the
    ///   voice-processing unit and a plain output) — a join or a leave
    ///   is the moment for it;
    /// - muted or unmuted in one: Android closes or reopens only the
    ///   microphone, playback and the call route untouched; on iOS's
    ///   voice unit the microphone is muted on the unit itself, which
    ///   is fast and turns the system's recording indicator off, and on
    ///   iOS's plain streams (headphones without echo cancellation) it
    ///   is closed like Android's.
    pub fn set_presence(&mut self, in_room: bool, mic_wanted: bool) {
        self.stats.in_room.store(in_room, Ordering::Relaxed);
        if !self.traits.releases_mic {
            self.in_room = in_room;
            self.sync_timer();
            self.input_config.in_room.store(in_room, Ordering::Relaxed);
            if !in_room {
                self.is_speaking.set(false);
            }
            return;
        }
        if in_room == self.in_room && mic_wanted == self.mic_wanted {
            return;
        }
        let plan_before = (self.call_path_wanted(), self.playback_wanted());
        self.in_room = in_room;
        self.mic_wanted = mic_wanted;
        if !self.started {
            return;
        }
        let path_moved = (self.call_path_wanted(), self.playback_wanted()) != plan_before;
        if !path_moved {
            self.apply_unit_mute();
            if !self.capture_wanted() {
                // Nothing to retry while it should stay closed.
                self.input_pending = false;
            }
        }
        let had_input = self.input.is_some();
        let done = self.reconcile(Cause::Presence { path_moved });
        if !done.replanned {
            match done.input {
                Some(Ok(())) => info!("Microphone open"),
                // `service()` retries, as for a microphone that never
                // opened.
                Some(Err(e)) => error!("Microphone reopen failed ({e}); will retry"),
                None if had_input && self.input.is_none() => info!("Microphone closed (muted)"),
                None => {}
            }
        }
    }

    /// Close the capture side, and with it what it was saying: the
    /// speaking flag and the meter it fed would otherwise hold their
    /// last values (a phone leaving mid-sentence stayed "speaking").
    fn close_input(&mut self) {
        self.input.take();
        self.is_speaking.set(false);
        self.stats
            .denoised_level
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
    }

    /// The streams and paths go on the call path (the platform's echo
    /// canceller, Android's communication mode): on a phone only in a
    /// voice room.
    fn call_path_wanted(&self) -> bool {
        !self.traits.releases_mic || self.in_room
    }

    /// Playback is open: out of a room Android needs none (its
    /// foreground service keeps the process); iOS keeps a plain output,
    /// the audio that lets a background app keep running.
    fn playback_wanted(&self) -> bool {
        !self.traits.releases_mic || self.in_room || self.traits.shared_unit
    }

    /// The capture side is open: on a phone only in a room, and — but
    /// on iOS's voice unit, which mutes the microphone itself and keeps
    /// the side attached for a fast unmute — only while wanted.
    fn capture_wanted(&self) -> bool {
        if !self.traits.releases_mic {
            return true;
        }
        self.in_room && (self.mic_wanted || (self.traits.shared_unit && self.voice_streams))
    }

    /// iOS: mute or unmute the microphone on the voice-processing unit
    /// to match `mic_wanted` (in a room on the unit; IOS-SPEC.md D51).
    #[cfg(target_os = "ios")]
    fn apply_unit_mute(&self) {
        if let Some(unit) = &self.voice_unit {
            unit.set_mic_muted(RELEASES_MIC && !self.mic_wanted);
        }
    }

    #[cfg(not(target_os = "ios"))]
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "the iOS twin reads the voice unit")
    )]
    fn apply_unit_mute(&self) {
        #[cfg(test)]
        if self.traits.shared_unit && self.voice_streams {
            fake::note(format!(
                "unit microphone {}",
                if self.traits.releases_mic && !self.mic_wanted {
                    "muted"
                } else {
                    "live"
                }
            ));
        }
    }

    /// Windows: what "Default" means for the open streams (see
    /// `aec_communications`). False elsewhere.
    fn plan_communications(&self) -> bool {
        self.comms_default
    }

    /// Fill `stats.aec` from the plan and what capture got (D5:
    /// verified, not assumed). Logged when it changes.
    fn update_aec_report(&self) {
        let mode = self.config.echo_cancellation;
        let output = self.aec_output;
        let report = if self.input.is_none() {
            None
        } else if !self.traits.voice_path {
            Some(aec::AecReport {
                mode,
                tier: aec::AecTier::None,
                output,
                reference: None,
                reason: aec::reasons::UNSUPPORTED,
                lowers_others: false,
            })
        } else if !self.aec_wanted {
            Some(aec::AecReport::not_wanted(mode, output))
        } else {
            let verdict = self
                .input
                .as_ref()
                .and_then(Capture::aec_verdict)
                .unwrap_or(Err(aec::reasons::PATH_REFUSED));
            // Whatever the canceller's verdict: the streams are a call
            // to Windows with or without one.
            let lowers_others = self.output.as_ref().is_some_and(Playback::lowers_others);
            Some(match verdict {
                Ok(()) => aec::AecReport {
                    mode,
                    tier: aec::AecTier::Platform,
                    output,
                    // Only a reference the platform took from us (D5):
                    // on Windows without the reference control the
                    // effect picks its own, and we do not know which.
                    reference: self
                        .input
                        .as_ref()
                        .filter(|i| i.aec_reference_named())
                        .and(self.opened_output.clone()),
                    reason: match (mode, output) {
                        (aec::AecMode::Always, aec::OutputKind::Headphones) => aec::reasons::ALWAYS,
                        (_, aec::OutputKind::Unknown) => aec::reasons::UNKNOWN_OUTPUT,
                        _ => aec::reasons::SPEAKERS,
                    },
                    lowers_others,
                },
                Err(reason) => aec::AecReport {
                    mode,
                    tier: aec::AecTier::None,
                    output,
                    reference: None,
                    reason,
                    lowers_others,
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

    // ── Public API ───────────────────────────────────────────────────────

    pub fn play_sound(&self, sound: SoundType) {
        let _ = self.sound_tx.try_send(sound);
    }

    /// The encoder's packets, for the thread that sends them: the
    /// network thread waits on this beside its socket (review P1-5).
    #[must_use]
    pub fn outgoing_receiver(&self) -> Receiver<AudioPacket> {
        self.encoder_to_network_rx.clone()
    }

    /// Handle for feeding received packets from another thread (see
    /// [`IncomingSender::send`]): the network thread's, so arrival is
    /// stamped at the socket read, which the jitter estimator measures
    /// from.
    #[must_use]
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

    pub fn set_loopback(&self, enabled: bool) {
        self.input_config
            .loopback_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.input_config.muted.store(muted, Ordering::Relaxed);
        if muted {
            self.is_speaking.set(false);
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
        if self.input_config.silenced() {
            return false;
        }
        self.is_speaking.get()
    }

    /// Rings when the speaking light may have changed ([`Self::is_speaking`]
    /// says how): the owner waits on it beside its other channels
    /// instead of polling the light (review P1-5).
    #[must_use]
    pub fn speaking_edges(&self) -> Receiver<()> {
        self.speaking_edges.clone()
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
        self.service_at(Instant::now())
    }

    /// [`Self::service`] at `now`: the backoff and the poll count from
    /// it, so a test runs minutes of supervision in no time.
    fn service_at(&mut self, now: Instant) -> bool {
        let stats = self.stats.clone();

        self.poll_devices(now);

        let seen = Trouble {
            input_needs: self.input.is_some()
                && (stats.input_stream_failed.load(Ordering::Relaxed)
                    || (self.config.input_device.is_none()
                        && stats.default_input_changed.load(Ordering::Relaxed))),
            output_needs: self.output.is_some()
                && (stats.output_stream_failed.load(Ordering::Relaxed)
                    || (self.config.output_device.is_none()
                        && stats.default_output_changed.load(Ordering::Relaxed))),
            // A side that is not open is not a failed stream (there is
            // none) — it is a device or permission that may show up
            // later: one that never opened, or whose reopen failed.
            input_late: self.input.is_none() && self.input_pending && self.capture_wanted(),
            output_late: self.output.is_none() && self.output_pending && self.playback_wanted(),
        };
        if !seen.any() && !self.aec_replan {
            return false;
        }
        if now.duration_since(self.last_restart_attempt) < RESTART_BACKOFF {
            return false;
        }
        self.last_restart_attempt = now;

        let why = |failed: &AtomicBool| {
            if failed.load(Ordering::Relaxed) {
                "stream failed"
            } else {
                "default device changed"
            }
        };
        let input_why = why(&stats.input_stream_failed);
        let output_why = why(&stats.output_stream_failed);
        let done = self.reconcile(Cause::Tick(seen));
        if done.replanned {
            return true;
        }
        let mut restarted = false;
        match (&done.output, seen.output_late) {
            (Some(Ok(())), late) => {
                stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
                let device = self.opened_output.as_deref().unwrap_or("default device");
                if late {
                    info!("Output started late ({device})");
                } else {
                    info!("Output restarted ({output_why}): {device}");
                }
                restarted = true;
            }
            // Every 2 s until it works: debug, or the log fills up.
            (Some(Err(e)), true) => debug!("output still unavailable: {e}"),
            // The old stream is gone: retried as a late side.
            (Some(Err(e)), false) => {
                error!("Output restart failed ({output_why}: {e}); will retry");
            }
            (None, _) => {}
        }
        match (&done.input, seen.input_late) {
            (Some(Ok(())), late) => {
                stats.stream_restarts.fetch_add(1, Ordering::Relaxed);
                let device = self.opened_input.as_deref().unwrap_or("default device");
                if late {
                    info!("Input started late ({device})");
                } else {
                    info!("Input restarted ({input_why}): {device}");
                }
                restarted = true;
            }
            (Some(Err(e)), true) => debug!("input still unavailable: {e}"),
            (Some(Err(e)), false) => {
                error!("Input restart failed ({input_why}: {e}); will retry");
            }
            (None, _) => {}
        }
        restarted
    }

    /// The slow poll where the OS sends no events (everything but
    /// Windows): the voice-call plan against the output, and on desktop
    /// the default devices.
    fn poll_devices(&mut self, now: Instant) {
        // Slow poll for default-device changes where there are no events.
        let watching = self.has_device_events();
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
        // Android and iOS have no default device to follow (routing is
        // the OS's), and their voice-call streams are not named like
        // cpal's default. cpal's default is the console one: while
        // "Default" means the communications device (Windows, with the
        // watcher down) the names never match, and every poll would
        // reopen the streams.
        if self.traits.follows_default && poll_due && !self.plan_communications() {
            if self.input.is_some() && self.config.input_device.is_none() {
                let current = Self::default_input_name();
                if current.is_some() && current != self.opened_input {
                    self.stats
                        .default_input_changed
                        .store(true, Ordering::Relaxed);
                }
            }
            if self.output.is_some() && self.config.output_device.is_none() {
                let current = Self::default_output_name();
                if current.is_some() && current != self.opened_output {
                    self.stats
                        .default_output_changed
                        .store(true, Ordering::Relaxed);
                }
            }
        }
    }

    /// The OS tells us when a default device changes (Windows, with its
    /// watcher up): nothing is polled then.
    #[cfg_attr(not(windows), allow(clippy::unused_self))]
    fn has_device_events(&self) -> bool {
        #[cfg(test)]
        if let Some(events) = fake::has_device_events() {
            return events;
        }
        #[cfg(windows)]
        {
            self.device_watcher.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The OS's default microphone by name: what "no device picked"
    /// follows on the desktop.
    fn default_input_name() -> Option<String> {
        #[cfg(test)]
        if let Some(name) = fake::default_input_name() {
            return name;
        }
        cpal::default_host()
            .default_input_device()
            .and_then(|d| device_name(&d))
    }

    /// The OS's default output by name.
    fn default_output_name() -> Option<String> {
        #[cfg(test)]
        if let Some(name) = fake::default_output_name() {
            return name;
        }
        cpal::default_host()
            .default_output_device()
            .and_then(|d| device_name(&d))
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
        self.config.input_device = device_name.map(String::from);
        match self.reconcile(Cause::InputPicked).input {
            Some(Ok(())) => {
                info!("Input device changed successfully");
                Ok(())
            }
            // `service()` keeps trying (a chosen device that is not
            // plugged in comes back when it is).
            Some(Err(e)) => {
                error!("start_input failed: {e}");
                Err(e)
            }
            None => Ok(()),
        }
    }

    pub fn set_output_device(
        &mut self,
        device_name: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let changed = self.config.output_device.as_deref() != device_name;
        self.config.output_device = device_name.map(String::from);
        let done = self.reconcile(Cause::OutputPicked { changed });
        if done.replanned {
            // Both sides went through a new plan, which logged why;
            // `service()` retries what did not open. A phone's route
            // is applied either way.
            return if self.output.is_some() || self.traits.routes {
                Ok(())
            } else {
                Err("the output did not open; retrying".into())
            };
        }
        match done.output {
            Some(Ok(())) => {
                info!("Output device changed");
                Ok(())
            }
            // As for the input: `service()` keeps trying.
            Some(Err(e)) => Err(e),
            None => Ok(()),
        }
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
        if let Ok(cache) = CACHE.lock()
            && let Some((at, list)) = cache.as_ref()
            && at.elapsed() < std::time::Duration::from_secs(10)
        {
            return Ok(list.clone());
        }

        let host = cpal::default_host();
        let default_name = host.default_input_device().and_then(|d| device_name(&d));
        let mut devices = Vec::new();
        for device in host.input_devices()? {
            if let Some(name) = device_name(&device) {
                let is_default = default_name.as_deref() == Some(&name);
                // As for the outputs below: no config round trip unless
                // the line is written (review P2-36).
                if log::log_enabled!(log::Level::Debug)
                    && let Ok(cfg) = device.default_input_config()
                {
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
        if let Ok(cache) = CACHE.lock()
            && let Some((at, list)) = cache.as_ref()
            && at.elapsed() < std::time::Duration::from_secs(10)
        {
            return Ok(list.clone());
        }

        let host = cpal::default_host();
        let default_name = host.default_output_device().and_then(|d| device_name(&d));
        let mut devices = Vec::new();
        for device in host.output_devices()? {
            if let Some(name) = device_name(&device) {
                let is_default = default_name.as_deref() == Some(&name);
                // Opening every device's config only to print it is a
                // round trip to each driver, on the UI thread, every
                // 10 s: only when the line will be written (review
                // P2-36).
                if log::log_enabled!(log::Level::Debug)
                    && let Ok(cfg) = device.default_output_config()
                {
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
        // And the iPhone's voice-processing unit closed, and its session
        // out of the voice-chat mode.
        #[cfg(target_os = "ios")]
        if self.voice_unit.take().is_some() {
            native::avsession::set_voice_mode(false);
        }

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
    #[test]
    fn receive_path_processes_packets_headless() {
        let vc = VoiceChat::new(VoiceChatConfig::default());
        let incoming = vc.incoming_sender();

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
            let _ = incoming.send(pkt);
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

    /// A playback that could not open is retried by `service()` — it
    /// used to be only when the failure was the first start's, never
    /// after a reopen (a device gone with nothing to replace it yet:
    /// the room stayed silent until the app restarted). Needs a machine
    /// with no output device, like CI's; elsewhere it proves nothing
    /// and says so.
    #[test]
    fn a_playback_that_failed_is_retried() {
        let mut vc = VoiceChat::new(VoiceChatConfig::default());
        if vc.start().audible() {
            eprintln!("an output opened here: nothing to retry");
            return;
        }
        assert!(vc.output_pending, "a failed start leaves playback pending");
        for _ in 0..3 {
            // The microphone (also missing here) is not what is retried.
            vc.input_pending = false;
            let due = Instant::now().checked_sub(RESTART_BACKOFF).unwrap();
            vc.last_restart_attempt = due;
            vc.service();
            assert!(
                vc.last_restart_attempt > due,
                "service() did not try the output again"
            );
            assert!(vc.output_pending, "still missing: still pending");
        }
    }

    /// Review P1-6: the desktop out of a room keeps both streams (no
    /// reopen on the way back, PRODUCTION-PLAN.md §6 item 5) and drops
    /// what capture hears; back in, it is heard again.
    #[test]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn the_desktop_out_of_a_room_drops_the_microphone_not_the_streams() {
        let mut vc = VoiceChat::new(VoiceChatConfig::default());
        vc.is_speaking.set(true);
        vc.set_presence(false, true);
        assert!(vc.input_config.silenced());
        assert!(!vc.is_speaking(), "nobody speaks from the lobby");
        assert!(
            vc.capture_wanted() && vc.playback_wanted(),
            "the streams stay"
        );
        vc.set_presence(true, true);
        assert!(!vc.input_config.silenced());
        vc.set_muted(true);
        assert!(vc.input_config.silenced(), "mute is still mute");
    }

    /// The headless path counts packets in the output worker, which only
    /// exists once `start()` succeeds — so without an output device the
    /// inbound queue must still accept packets without blocking or panicking.
    #[test]
    fn incoming_sender_never_blocks_without_output() {
        let vc = VoiceChat::new(VoiceChatConfig::default());
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
mod network_side {
    use std::time::Duration as StdDuration;

    use super::*;

    /// The engine lives on its own thread (`VoiceChat` is `!Send`, cpal
    /// streams) and the network thread feeds and drains it through the
    /// two handles alone (review P1-5): a packet in counts at once, the
    /// encoder's channel is a live receiver, and it disconnects when
    /// the engine goes — the network thread's cue to let go of it.
    #[test]
    fn the_network_thread_reaches_the_engine_through_its_handles() {
        let (handles_tx, handles_rx) = crossbeam_channel::bounded(1);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let engine = std::thread::spawn(move || {
            let vc = VoiceChat::new(VoiceChatConfig::default());
            handles_tx
                .send((
                    vc.incoming_sender(),
                    vc.outgoing_receiver(),
                    vc.stats().clone(),
                ))
                .unwrap();
            let _ = stop_rx.recv();
        });
        let (incoming, outgoing, stats) = handles_rx.recv().expect("handles");

        let mut enc = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip)
            .expect("encoder");
        let pcm: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / 48000.0).sin() * 0.4)
            .collect();
        let mut buf = vec![0u8; 512];
        for seq in 0..30u32 {
            let n = enc.encode_float(&pcm, &mut buf).expect("encode");
            let _ = incoming.send(AudioPacket::new(
                seq,
                seq * FRAME_SIZE as u32,
                7,
                buf[..n].to_vec(),
            ));
        }
        assert_eq!(
            stats
                .packets_received
                .load(std::sync::atomic::Ordering::Relaxed),
            30,
            "counted at intake"
        );
        assert!(matches!(
            outgoing.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));

        stop_tx.send(()).unwrap();
        engine.join().expect("engine thread");
        assert!(
            outgoing.recv_timeout(StdDuration::from_secs(2)).is_err(),
            "the encoder's channel closes with the engine"
        );
        assert!(!incoming.send(AudioPacket::new(0, 0, 7, vec![0xFC])));
    }
}
