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
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{AudioPacket, PeerAudioSettings, SoundType, VoiceChat, VoiceChatConfig};
use crossbeam_channel::{bounded, select, Receiver, Sender};
use log::info;

use crate::{config::Settings, network::NetworkControl, WakeFn};

// ── Channel messages ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VoiceControl {
    Start {
        settings: Settings,
        our_client_id: u64,
    },
    Stop,
    SetMuted(bool),
    SetDeafened(bool),
    SetLoopback(bool),
    SetGateThreshold(f32),
    SetInputGain(f32),
    SetOutputGain(f32),
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),
    AddPeer(u64),
    RemovePeer(u64),
    SetPeerAudio(u64, PeerAudioSettings),
    /// Loss on our outbound path in percent (0–49), from the network
    /// thread's periodic renet sample. Feeds the encoder's FEC hint.
    SetUplinkLoss(u32),
    /// Round-trip time in ms from the same renet sample. Feeds the
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
}

impl SharedAudioState {
    pub fn new() -> Self {
        Self {
            input_level: Arc::new(AtomicU32::new(0)),
            output_level: Arc::new(AtomicU32::new(0)),
            denoised_level: Arc::new(AtomicU32::new(0)),
            speaking: Arc::new(AtomicBool::new(false)),
            jitter_buffer_ms: Arc::new(AtomicU32::new(0)),
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

struct VoiceLoopState {
    vc: Option<VoiceChat>,
    our_client_id: Option<u64>,
    was_speaking: bool,
    /// Explicit mute set by the user (independent of deafen).
    user_muted: bool,
    running: bool,
    last_stats_log: Instant,
}

impl VoiceLoopState {
    fn new() -> Self {
        Self {
            vc: None,
            our_client_id: None,
            was_speaking: false,
            user_muted: false,
            running: true,
            last_stats_log: Instant::now(),
        }
    }

    /// Write the audio engine's counters to the log every
    /// [`STATS_LOG_INTERVAL`] while an engine is running.
    fn maybe_log_stats(&mut self) {
        let Some(voice) = self.vc.as_ref() else {
            return;
        };
        if self.last_stats_log.elapsed() < STATS_LOG_INTERVAL {
            return;
        }
        self.last_stats_log = Instant::now();
        info!("audio stats: {}", voice.stats_summary());
    }

    /// Whether the mic should be muted — true if either user-muted or deafened.
    fn effective_mute(&self) -> bool {
        if let Some(v) = &self.vc {
            self.user_muted || v.is_deafened()
        } else {
            self.user_muted
        }
    }

    fn handle_control(&mut self, ctrl: VoiceControl, event_tx: &Sender<VoiceEvent>, wake: &WakeFn) {
        match ctrl {
            VoiceControl::Start {
                settings,
                our_client_id: id,
            } => {
                let config = settings_to_config(&settings);
                match VoiceChat::new(config) {
                    Ok(mut voice) => {
                        if let Err(e) = voice.start() {
                            event_tx
                                .send(VoiceEvent::Error(format!("Start failed: {e}")))
                                .ok();
                            wake();
                            return;
                        }
                        self.our_client_id = Some(id);
                        self.vc = Some(voice);
                        self.was_speaking = false;
                        self.user_muted = false;
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
            VoiceControl::SetMuted(muted) => {
                self.user_muted = muted;
                if let Some(v) = &self.vc {
                    v.set_muted(self.effective_mute());
                }
            }
            VoiceControl::SetDeafened(deafened) => {
                if let Some(v) = &self.vc {
                    v.set_deafened(deafened);
                    // Recompute effective mute — deafen implies mute,
                    // but undeafen restores the user's explicit mute state.
                    v.set_muted(self.effective_mute());
                }
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
            let outgoing = state.vc.as_ref().map(|v| v.outgoing_receiver());
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
                // Engine running: the 5 ms cadence feeds the UI level
                // meters even when the gate is closed (no packets).
                default(Duration::from_millis(5)) => {}
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
                if !is_testing_mic.load(Ordering::Relaxed) {
                    for packet in voice.take_outgoing_packets() {
                        forward_outgoing(&network_tx, id, packet);
                    }
                } else {
                    while voice.outgoing_receiver().try_recv().is_ok() {}
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

        state.maybe_log_stats();
    }
}
