// voice.rs — Owns VoiceChat on a dedicated thread.
//
// Receives VoiceControl commands, emits VoiceEvent notifications,
// and forwards outgoing packets to the network thread.
// All audio processing is handled by the core library.
//
// This module has NO dependency on any UI toolkit.  The caller injects
// a `WakeFn` callback that is called whenever the UI event loop should
// check for new events (a no-op works for headless use).

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering, Ordering::Relaxed},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{
    AecMode, AecReport, AudioPacket, PeerAudioSettings, SoundType, VoiceChat, VoiceChatConfig,
};
use crossbeam_channel::{bounded, select, Receiver, Sender};
use log::info;

use crate::{config::Settings, network::NetworkControl, WakeFn};

// ── Channel messages ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VoiceControl {
    /// Boxed: `Settings` is by far the largest payload on this channel
    /// and every other message would pay for it otherwise.
    Start {
        settings: Box<Settings>,
        our_client_id: u64,
        /// In a voice room (or testing the microphone). A phone opens
        /// no microphone out of one (`audio::RELEASES_MIC`); the
        /// desktop ignores it.
        in_room: bool,
    },
    Stop,
    /// Into a voice room (`true`) or out to the lobby. Phones only act
    /// on it: the microphone and the call path go with the room.
    SetInRoom(bool),
    SetMuted(bool),
    /// Transmit only while the key is held (the gate still applies).
    SetPushToTalk(bool),
    /// The push-to-talk key went down / came up.
    PushToTalkHeld(bool),
    SetDeafened(bool),
    SetLoopback(bool),
    SetGateThreshold(f32),
    SetInputGain(f32),
    SetOutputGain(f32),
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),
    /// The echo-cancellation setting; reopens the streams when the
    /// voice-call decision changes.
    SetEchoCancellation(AecMode),
    AddPeer(u64),
    RemovePeer(u64),
    SetPeerAudio(u64, PeerAudioSettings),
    /// Loss on our outbound path in percent (0–50), from the relay's
    /// voice receiver report (only when the window held enough packets
    /// to mean something). Feeds the encoder's FEC hint.
    SetUplinkLoss(u32),
    /// Round-trip time in ms from the same report. Feeds the
    /// encoder's rate controller (congestion vs random loss).
    SetLinkRtt(u32),
    IncomingVoicePacket(AudioPacket),
    PlaySound(SoundType),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum VoiceEvent {
    Started,
    Error(String),
    SpeakingChanged(bool),
}

// ── Shared levels (written here, read by UI) ────────────────────────────

#[derive(Clone)]
pub struct SharedAudioState {
    pub input_level: Arc<AtomicU32>,
    pub output_level: Arc<AtomicU32>,
    pub denoised_level: Arc<AtomicU32>,
    pub speaking: Arc<AtomicBool>,
    pub jitter_buffer_ms: Arc<AtomicU32>,
    /// What echo cancellation the open streams got (`None` = no voice
    /// session); refreshed when the engine's report changes.
    pub aec: Arc<Mutex<Option<AecReport>>>,
}

impl Default for SharedAudioState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedAudioState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            input_level: Arc::new(AtomicU32::new(0)),
            output_level: Arc::new(AtomicU32::new(0)),
            denoised_level: Arc::new(AtomicU32::new(0)),
            speaking: Arc::new(AtomicBool::new(false)),
            jitter_buffer_ms: Arc::new(AtomicU32::new(0)),
            aec: Arc::new(Mutex::new(None)),
        }
    }
}

// ── Config conversion ───────────────────────────────────────────────────

fn settings_to_config(s: &Settings) -> VoiceChatConfig {
    VoiceChatConfig {
        input_device: s.audio.input.device.clone(),
        output_device: s.audio.output.device.clone(),
        gate_threshold: s.audio.gate_threshold,
        noise_suppression: true,
        input_gain: s.audio.input.gain,
        output_gain: s.audio.output.gain,
        agc_enabled: true,
        agc_target_rms: 0.10,
        compressor_enabled: true,
        echo_cancellation: s.audio.echo_cancellation,
    }
}

// ── Public API ──────────────────────────────────────────────────────────

pub struct VoiceThreadHandle {
    pub tx: Sender<VoiceControl>,
    pub event_rx: Receiver<VoiceEvent>,
    pub shared: SharedAudioState,
    /// Joined by the frontend on shutdown (with a timeout).
    pub join_handle: Option<thread::JoinHandle<()>>,
}

impl VoiceThreadHandle {
    /// Spawn the voice thread. Returns a handle for sending commands and
    /// receiving events.
    pub fn spawn(
        network_tx: Sender<NetworkControl>,
        is_testing_mic: Arc<AtomicBool>,
        wake: WakeFn,
    ) -> Self {
        let (tx, control_rx) = bounded::<VoiceControl>(64);
        let (event_tx, event_rx) = bounded::<VoiceEvent>(32);
        let shared = SharedAudioState::new();

        let shared_clone = shared.clone();
        let join_handle = thread::Builder::new()
            .name("client-voice".into())
            .spawn(move || {
                run_voice_loop(
                    control_rx,
                    event_tx,
                    network_tx,
                    shared_clone,
                    is_testing_mic,
                    wake,
                );
            })
            .expect("failed to spawn voice thread");

        Self {
            tx,
            event_rx,
            shared,
            join_handle: Some(join_handle),
        }
    }

    /// Send shutdown and wait for the thread to exit.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(VoiceControl::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for VoiceThreadHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Thread loop state ───────────────────────────────────────────────────

/// Cadence of the engine-stats line in the log. Cheap (a few atomic
/// loads) and the one place `outgoing_dropped`, PLC/FEC counts, the FEC
/// hint and jitter depth are recorded for after-the-fact diagnosis.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// The voice loop's longest sleep while the engine runs (meters,
/// speaking edges, device supervision); see the loop.
const VOICE_TICK: Duration = Duration::from_millis(20);

/// The same while no microphone is open (a phone in the lobby or
/// muted, ANDROID-SPEC.md D53): no input meter or speaking edge to
/// follow, only device supervision — four wakes a second, not fifty.
const VOICE_TICK_NO_MIC: Duration = Duration::from_millis(250);

// Mute, deafen, push-to-talk and the rest are independent switches
// the loop reads every packet; their combinations are all valid.
#[expect(clippy::struct_excessive_bools)]
struct VoiceLoopState {
    vc: Option<VoiceChat>,
    our_client_id: Option<u64>,
    was_speaking: bool,
    /// Explicit mute set by the user (independent of deafen).
    user_muted: bool,
    /// Push-to-talk mode: the mic is closed unless `ptt_held`.
    ptt_mode: bool,
    ptt_held: bool,
    /// In a voice room (`SetInRoom`); a phone keeps its microphone
    /// closed out of one.
    in_room: bool,
    running: bool,
    last_stats_log: Instant,
    /// `aec_generation` of the engine whose report `shared.aec` holds
    /// (`None` = no engine).
    aec_published: Option<u64>,
}

impl VoiceLoopState {
    fn new() -> Self {
        Self {
            vc: None,
            our_client_id: None,
            was_speaking: false,
            user_muted: false,
            ptt_mode: false,
            ptt_held: false,
            in_room: true,
            running: true,
            last_stats_log: Instant::now(),
            aec_published: None,
        }
    }

    /// Write the audio engine's counters to the log every
    /// [`STATS_LOG_INTERVAL`] while an engine is running, and hand the
    /// same numbers to the network thread for the telemetry sample
    /// (spec §9.1) — packets sent, ladder state, overruns, devices: what
    /// tells "his network" from "his machine" in the server log.
    fn maybe_log_stats(&mut self, network_tx: &Sender<NetworkControl>) {
        let Some(voice) = self.vc.as_ref() else {
            return;
        };
        if self.last_stats_log.elapsed() < STATS_LOG_INTERVAL {
            return;
        }
        self.last_stats_log = Instant::now();
        info!("audio stats: {}", voice.stats_summary());
        let s = voice.stats();
        let (input_device, output_device) = voice.opened_devices();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let report_age_ms = now_ms.saturating_sub(s.uplink_loss_updated_ms.load(Relaxed));
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let sample = protocol::AudioSample {
            bitrate_bps: s.send_bitrate_bps.load(Relaxed),
            band: format!("L{}", s.adapt_level.load(Relaxed)),
            frame_ms: s.frame_ms.load(Relaxed),
            red: s.red_active.load(Relaxed),
            fec_hint_pct: s.fec_loss_hint_pct.load(Relaxed),
            smoothed_loss_pct: s.uplink_loss_pct.load(Relaxed) as f32,
            // A report younger than three intervals means the relay is
            // still telling us about our voice.
            exercised: report_age_ms < 6_000,
            packets_sent: s.packets_sent.load(Relaxed),
            encode_us: s.last_encode_us.load(Relaxed),
            input_overruns: s.input_overruns.load(Relaxed),
            output_underruns: s.output_underruns.load(Relaxed),
            input_device: input_device.unwrap_or_default(),
            output_device: output_device.unwrap_or_default(),
            packets_received: s.packets_received.load(Relaxed),
            packets_lost: s.packets_lost.load(Relaxed),
            fec_recoveries: s.fec_recoveries.load(Relaxed),
            red_recoveries: s.red_recoveries.load(Relaxed),
            plc_frames: s.plc_frames.load(Relaxed),
            jitter_ms: s.jitter_buffer_occupancy_ms.load(Relaxed) as u32,
            jitter_target_ms: s.jitter_buffer_target_ms.load(Relaxed) as u32,
            decode_us: s.last_decode_us.load(Relaxed),
        };
        let _ = network_tx.try_send(NetworkControl::TelemetryAudio(sample));
    }

    /// Whether the mic should be muted: user-muted, deafened, or in
    /// push-to-talk mode with the key up.
    fn effective_mute(&self) -> bool {
        let ptt_closed = self.ptt_mode && !self.ptt_held;
        if let Some(v) = &self.vc {
            self.user_muted || ptt_closed || v.is_deafened()
        } else {
            self.user_muted || ptt_closed
        }
    }

    /// Hand the engine the effective mute — the samples are dropped —
    /// and, on a phone, what to keep open: out of a room or muted, the
    /// microphone closes (`VoiceChat::set_presence`).
    fn apply_mute(&mut self) {
        let mute = self.effective_mute();
        let in_room = self.in_room;
        if let Some(v) = self.vc.as_mut() {
            v.set_muted(mute);
            v.set_presence(in_room, !mute);
        }
    }

    fn handle_control(&mut self, ctrl: VoiceControl, event_tx: &Sender<VoiceEvent>, wake: &WakeFn) {
        match ctrl {
            VoiceControl::Start {
                settings,
                our_client_id: id,
                in_room,
            } => {
                let config = settings_to_config(&settings);
                // The old engine goes first: its device state (Android's
                // communication mode, iOS's voice-processing unit and
                // session mode) is process-wide, and leaving it after the
                // new one entered would undo the new one's.
                self.vc.take();
                // A fresh engine starts unmuted; push-to-talk closes it
                // until the key is held. Known before `start()`, so a
                // phone never opens a microphone it would close at once.
                self.user_muted = false;
                self.ptt_held = false;
                self.in_room = in_room;
                match VoiceChat::new(config) {
                    Ok(mut voice) => {
                        voice.set_presence(in_room, !self.effective_mute());
                        if let Err(e) = voice.start() {
                            if !voice.output_running() {
                                // Nothing came up: no engine this time.
                                event_tx
                                    .send(VoiceEvent::Error(format!("Start failed: {e}")))
                                    .ok();
                                wake();
                                return;
                            }
                            // Playback is up and only capture failed (no
                            // microphone; on Android the permission not
                            // granted yet). Keep the engine: the room stays
                            // audible, and `service()` retries the input
                            // every 2 s until it opens. Dropping it here
                            // silenced everything (first phone test,
                            // 2026-10-05).
                            event_tx
                                .send(VoiceEvent::Error(format!("Microphone unavailable: {e}")))
                                .ok();
                        }
                        self.our_client_id = Some(id);
                        self.vc = Some(voice);
                        // Generations count per engine: republish.
                        self.aec_published = None;
                        self.was_speaking = false;
                        self.apply_mute();
                        event_tx.send(VoiceEvent::Started).ok();
                        wake();
                        info!("Voice started for client {id:?}");
                    }
                    Err(e) => {
                        event_tx
                            .send(VoiceEvent::Error(format!("Init failed: {e}")))
                            .ok();
                        wake();
                    }
                }
            }
            VoiceControl::Stop => {
                self.vc = None;
                self.our_client_id = None;
                self.was_speaking = false;
                self.user_muted = false;
                info!("Voice stopped");
            }
            VoiceControl::SetInRoom(in_room) => {
                self.in_room = in_room;
                self.apply_mute();
            }
            VoiceControl::SetMuted(muted) => {
                self.user_muted = muted;
                self.apply_mute();
            }
            VoiceControl::SetPushToTalk(on) => {
                self.ptt_mode = on;
                self.ptt_held = false;
                self.apply_mute();
            }
            VoiceControl::PushToTalkHeld(held) => {
                self.ptt_held = held;
                self.apply_mute();
            }
            VoiceControl::SetDeafened(deafened) => {
                if let Some(v) = &self.vc {
                    v.set_deafened(deafened);
                }
                // Recompute effective mute — deafen implies mute, but
                // undeafen restores the user's explicit mute state.
                self.apply_mute();
            }
            VoiceControl::SetLoopback(enabled) => {
                if let Some(v) = &self.vc {
                    v.set_loopback(enabled);
                }
            }
            VoiceControl::SetGateThreshold(t) => {
                if let Some(v) = &self.vc {
                    v.set_gate_threshold(t);
                }
            }
            VoiceControl::SetInputGain(g) => {
                if let Some(v) = &self.vc {
                    v.set_input_gain(g);
                }
            }
            VoiceControl::SetOutputGain(g) => {
                if let Some(v) = &self.vc {
                    v.set_output_gain(g);
                }
            }
            VoiceControl::SetInputDevice(d) => {
                if let Some(v) = self.vc.as_mut()
                    && let Err(e) = v.set_input_device(d.as_deref())
                {
                    event_tx
                        .send(VoiceEvent::Error(format!("Input device: {e}")))
                        .ok();
                    wake();
                }
            }
            VoiceControl::SetOutputDevice(d) => {
                if let Some(v) = self.vc.as_mut()
                    && let Err(e) = v.set_output_device(d.as_deref())
                {
                    event_tx
                        .send(VoiceEvent::Error(format!("Output device: {e}")))
                        .ok();
                    wake();
                }
            }
            VoiceControl::SetEchoCancellation(mode) => {
                if let Some(v) = self.vc.as_mut() {
                    v.set_echo_cancellation(mode);
                }
            }
            VoiceControl::AddPeer(id) => {
                if let Some(v) = self.vc.as_mut() {
                    v.add_peer(id);
                }
            }
            VoiceControl::RemovePeer(id) => {
                if let Some(v) = self.vc.as_mut() {
                    v.remove_peer(id);
                }
            }
            VoiceControl::SetPeerAudio(id, settings) => {
                if let Some(v) = &self.vc {
                    v.set_peer_settings(id, settings);
                }
            }
            VoiceControl::SetUplinkLoss(pct) => {
                if let Some(v) = &self.vc {
                    v.set_uplink_loss_pct(pct);
                }
            }
            VoiceControl::SetLinkRtt(rtt_ms) => {
                if let Some(v) = &self.vc {
                    v.set_link_rtt_ms(rtt_ms);
                }
            }
            VoiceControl::IncomingVoicePacket(packet) => {
                if let Some(v) = &self.vc {
                    v.receive_packet(packet);
                }
            }
            VoiceControl::PlaySound(sound) => {
                if let Some(v) = &self.vc {
                    v.play_sound(sound);
                }
            }
            VoiceControl::Shutdown => {
                self.running = false;
            }
        }
    }
}

// ── Thread loop ─────────────────────────────────────────────────────────

/// Stamp our client id on an outgoing packet and hand it to the network
/// thread. Voice frames are droppable by design (`try_send`) — losing one
/// under pressure is better than stalling the voice loop.
fn forward_outgoing(network_tx: &Sender<NetworkControl>, our_id: u64, mut packet: AudioPacket) {
    packet.header.ssrc = our_id;
    let _ = network_tx.try_send(NetworkControl::SendVoicePacket(packet));
}

fn run_voice_loop(
    control_rx: Receiver<VoiceControl>,
    event_tx: Sender<VoiceEvent>,
    network_tx: Sender<NetworkControl>,
    shared: SharedAudioState,
    is_testing_mic: Arc<AtomicBool>,
    wake: WakeFn,
) {
    let mut state = VoiceLoopState::new();
    let (_dead_tx, dead_rx) = bounded::<audio::AudioPacket>(1);

    while state.running {
        if state.vc.is_none() {
            // Idle: no engine, so no meters to feed, no packets to
            // forward, and no speaking edges to detect — block until a
            // command arrives instead of spinning the 5 ms cadence
            // below (which cost ~200 wakeups/s from app launch until
            // the first connect).
            match control_rx.recv() {
                Ok(ctrl) => state.handle_control(ctrl, &event_tx, &wake),
                Err(_) => {
                    state.running = false;
                }
            }
        } else {
            let outgoing = state.vc.as_ref().map(audio::VoiceChat::outgoing_receiver);
            let mic_open = state.vc.as_ref().is_some_and(VoiceChat::input_running);
            let tick = if mic_open || !audio::RELEASES_MIC {
                VOICE_TICK
            } else {
                VOICE_TICK_NO_MIC
            };
            select! {
                recv(control_rx) -> msg => match msg {
                    Ok(ctrl) => state.handle_control(ctrl, &event_tx, &wake),
                    Err(_) => { state.running = false; }
                },
                recv(outgoing.as_ref().unwrap_or(&dead_rx)) -> pkt => {
                    // Low-latency path: forward the packet that woke us.
                    // Anything queued behind it is drained below.
                    if let (Ok(packet), Some(id)) = (pkt, state.our_client_id)
                        && !is_testing_mic.load(Ordering::Relaxed)
                    {
                        forward_outgoing(&network_tx, id, packet);
                    }
                },
                // Engine running: this cadence feeds the UI level
                // meters even when the gate is closed (no packets),
                // notices the gate closing (the speaking light), and
                // supervises the devices. Packets and commands wake the
                // loop at once, so nothing waits on it but those. It was
                // 5 ms — 200 wakes a second for as long as a session
                // lasted, a quiet channel included; the meters redraw
                // every 125 ms and the speaking indicator holds for
                // hundreds, so 20 ms is still far finer than anything
                // reads it (IOS-SPEC.md §14 D41).
                default(tick) => {}
            }
        }

        // Drain any queued control messages
        for _ in 0..32 {
            match control_rx.try_recv() {
                Ok(ctrl) => state.handle_control(ctrl, &event_tx, &wake),
                Err(_) => break,
            }
        }

        // Device supervision (audio 0.7): restart a side whose device
        // went away, follow the OS default when none is pinned. Cheap
        // when nothing changed; the engine logs what it did.
        if let Some(voice) = state.vc.as_mut() {
            let _ = voice.service();
        }

        // Poll outgoing packets and update shared state
        if let Some(voice) = state.vc.as_ref() {
            // Forward encoded packets to network (or drain in test mode)
            if let Some(id) = state.our_client_id {
                if is_testing_mic.load(Ordering::Relaxed) {
                    while voice.outgoing_receiver().try_recv().is_ok() {}
                } else {
                    for packet in voice.take_outgoing_packets() {
                        forward_outgoing(&network_tx, id, packet);
                    }
                }
            }

            // Update shared levels for UI meters
            shared
                .input_level
                .store(voice.input_level().to_bits(), Ordering::Relaxed);
            shared
                .output_level
                .store(voice.output_level().to_bits(), Ordering::Relaxed);
            shared
                .denoised_level
                .store(voice.denoised_level().to_bits(), Ordering::Relaxed);
            shared
                .speaking
                .store(voice.is_speaking(), Ordering::Relaxed);
            shared.jitter_buffer_ms.store(
                voice
                    .stats()
                    .jitter_buffer_occupancy_ms
                    .load(Ordering::Relaxed) as u32,
                Ordering::Relaxed,
            );

            // Edge-triggered speaking notification. Blocking sends on
            // purpose: there is no periodic refresh, so a dropped edge
            // means a speaking light stuck in the wrong state until the
            // next transition. Both channels are drained frequently.
            //
            // While mic-testing we're in pure loopback — voice packets
            // are already gated above, and effective speaking is forced
            // false here so the test never lights us up (server-side or
            // on our own sidebar). Treating it as `raw && !testing`
            // makes the edges self-correcting: starting a test
            // mid-sentence emits a clean false edge, stopping it while
            // still talking emits a clean true edge.
            let speaking = voice.is_speaking() && !is_testing_mic.load(Ordering::Relaxed);
            if speaking != state.was_speaking {
                state.was_speaking = speaking;
                let _ = network_tx.send(NetworkControl::SendSpeakingStatus(speaking));
                let _ = event_tx.send(VoiceEvent::SpeakingChanged(speaking));
                wake();
            }
        }

        // The echo-cancellation report for the settings page: it changes
        // only when streams (re)open, so compare the generation and copy
        // the report then — and clear it when the engine is gone.
        let generation = state
            .vc
            .as_ref()
            .map(|v| v.stats().aec_generation.load(Ordering::Relaxed));
        if generation != state.aec_published {
            state.aec_published = generation;
            if let Ok(mut slot) = shared.aec.lock() {
                *slot = state.vc.as_ref().and_then(VoiceChat::aec_report);
            }
        }

        state.maybe_log_stats(&network_tx);
    }
}
