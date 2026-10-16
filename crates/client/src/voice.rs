// voice.rs — Owns VoiceChat on a dedicated thread.
//
// Receives VoiceControl commands and emits VoiceEvent notifications.
// Packets do not pass through here: at each start the engine's two
// packet handles go to the network thread, which feeds and drains the
// engine itself (review P1-5). All audio processing is handled by the
// core library.
//
// This module has NO dependency on any UI toolkit.  The caller injects
// a `WakeFn` callback that is called whenever the UI event loop should
// check for new events (a no-op works for headless use).

use std::{
    sync::{
        atomic::{AtomicBool, Ordering, Ordering::Relaxed},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use audio::{
    AecMode, AecReport, AudioStats, PeerAudioSettings, SoundType, VoiceChat, VoiceChatConfig,
};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use log::info;

use crate::{
    config::Settings,
    network::{NetworkControl, VoiceLink},
    WakeFn,
};

// ── Channel messages ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VoiceControl {
    /// The engine's configuration only ([`engine_config`]): the whole
    /// `Settings` was cloned for it, servers and keybinds and all
    /// (review 2026-10, P3-41). Boxed: the largest payload here.
    Start {
        config: Box<VoiceChatConfig>,
        /// The session's client id. `None` is an engine with no session
        /// behind it — Settings' meter or a microphone test while not
        /// connected: nothing it hears can leave the device for as long
        /// as it lives (`VoiceLink::local_only`), so mute and
        /// push-to-talk have nothing to keep in and do not close its
        /// microphone. A push-to-talk user's meter would stay dead
        /// otherwise, with no session to hold the key in.
        our_client_id: Option<u64>,
        /// In a voice room (or testing the microphone). Out of one a
        /// phone opens no microphone (`audio::RELEASES_MIC`), and the
        /// desktop drops what its microphone hears (review P1-6).
        in_room: bool,
    },
    Stop,
    /// Into a voice room (`true`) or out of one (the lobby, the AFK
    /// room). On a phone the microphone and the call path go with the
    /// room; on the desktop the streams stay open and capture drops its
    /// samples (`VoiceChat::set_presence`).
    SetInRoom(bool),
    /// Whether anyone can hear what is sent: off for a microphone test,
    /// the Settings meter and a room with nobody else in it, when the
    /// encoder gets nothing (`VoiceChat::set_transmit`, review 2026-10
    /// P2-37).
    SetTransmit(bool),
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
    PlaySound(SoundType),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum VoiceEvent {
    Started,
    Error(String),
    SpeakingChanged(bool),
}

// ── Shared state (published here, read by the UI) ───────────────────────

#[derive(Clone)]
pub struct SharedAudioState {
    /// The running engine's counters (`None` = no engine). The UI reads
    /// its meters and the jitter depth straight from them, so nothing
    /// copies them on a timer (review P1-5).
    engine: Arc<Mutex<Option<Arc<AudioStats>>>>,
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
            engine: Arc::new(Mutex::new(None)),
            aec: Arc::new(Mutex::new(None)),
        }
    }

    fn engine_stats(&self) -> Option<Arc<AudioStats>> {
        self.engine.lock().ok()?.clone()
    }

    fn publish(&self, stats: Option<Arc<AudioStats>>) {
        if let Ok(mut slot) = self.engine.lock() {
            *slot = stats;
        }
    }

    /// The meters: the microphone after denoising and the playback.
    /// `None` without an engine, which is not the same as silence to
    /// whoever is asking why a bar does not move.
    #[must_use]
    pub fn meters(&self) -> Option<(f32, f32)> {
        self.engine_stats().map(|s| {
            (
                f32::from_bits(s.denoised_level.load(Relaxed)),
                f32::from_bits(s.output_level.load(Relaxed)),
            )
        })
    }

    /// How much voice the jitter buffer holds, in ms.
    #[must_use]
    pub fn jitter_buffer_ms(&self) -> u32 {
        self.engine_stats().map_or(0, |s| {
            u32::try_from(s.jitter_buffer_occupancy_ms.load(Relaxed)).unwrap_or(u32::MAX)
        })
    }
}

// ── Config conversion ───────────────────────────────────────────────────

/// The voice engine's configuration from the user's settings: what
/// `VoiceControl::Start` carries.
#[must_use]
pub fn engine_config(s: &Settings) -> VoiceChatConfig {
    VoiceChatConfig {
        input_device: s.audio.input.device.clone(),
        output_device: s.audio.output.device.clone(),
        gate_threshold: s.audio.gate_threshold,
        input_gain: s.audio.input.gain,
        output_gain: s.audio.output.gain,
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
        // Unbounded both ways (ARCHITECTURE.md §6's rule for channels
        // between two threads that each send to the other): joining a
        // server with 31+ people online queued more AddPeer and
        // SetPeerAudio commands than the 64 slots while the engine was
        // opening its devices, and the UI blocked on the send until they
        // opened (review 2026-10, P2-15). Both are a few small commands
        // a second at most.
        let (tx, control_rx) = unbounded::<VoiceControl>();
        let (event_tx, event_rx) = unbounded::<VoiceEvent>();
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

/// The voice loop's sleep when nothing is due: commands, the speaking
/// light's edges and the engine's trouble (a failed stream, a moved
/// default) wake it, and the engine says when its supervision is next
/// due (`VoiceChat::next_service`); packets never pass through it
/// (review P1-5). It ticked every second whenever an engine existed —
/// a phone's lobby with nothing open included — and before that every
/// 20 ms (review 2026-10, P2-41).
const IDLE_WAIT: Duration = Duration::from_hours(1);

// Mute, deafen, push-to-talk and the rest are independent switches
// the loop reads every packet; their combinations are all valid.
#[expect(clippy::struct_excessive_bools)]
struct VoiceLoopState {
    vc: Option<VoiceChat>,
    was_speaking: bool,
    /// Explicit mute set by the user (independent of deafen).
    user_muted: bool,
    /// Push-to-talk mode: the mic is closed unless `ptt_held`.
    ptt_mode: bool,
    ptt_held: bool,
    /// In a voice room (`SetInRoom`); out of one a phone keeps its
    /// microphone closed and the desktop drops the samples.
    in_room: bool,
    /// `SetTransmit`, kept for the next engine.
    transmit: bool,
    /// The engine has no session behind it (`Start` without a client
    /// id): what it hears stays here, and mute and push-to-talk wait.
    sessionless: bool,
    running: bool,
    /// The engine was started, replaced or stopped: the network thread
    /// and the UI are told after this pass's commands.
    relink: bool,
    last_stats_log: Instant,
    /// Packets sent and received and restarts at the last stats line:
    /// an engine that moved none is not logged again.
    stats_logged: (u64, u64, u64),
    /// `aec_generation` of the engine whose report `shared.aec` holds
    /// (`None` = no engine).
    aec_published: Option<u64>,
}

impl VoiceLoopState {
    fn new() -> Self {
        Self {
            vc: None,
            was_speaking: false,
            user_muted: false,
            ptt_mode: false,
            ptt_held: false,
            in_room: true,
            transmit: true,
            sessionless: false,
            running: true,
            relink: false,
            last_stats_log: Instant::now(),
            stats_logged: (0, 0, 0),
            aec_published: None,
        }
    }

    /// When the loop must wake without being rung: the engine's next
    /// supervision, and in a voice room the next stats line.
    fn next_wake(&self, voice: &VoiceChat) -> Option<Instant> {
        let stats = self
            .in_room
            .then(|| self.last_stats_log + STATS_LOG_INTERVAL);
        match (voice.next_service(), stats) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Write the audio engine's counters to the log every
    /// [`STATS_LOG_INTERVAL`] in a voice room, and hand the same numbers
    /// to the network thread for the telemetry sample (spec §9.1) —
    /// packets sent, ladder state, overruns, devices: what tells "his
    /// network" from "his machine" in the server log. Only when a packet
    /// moved since the last: the same line every 10 s for an idle
    /// engine said nothing (review 2026-10, P2-41).
    fn maybe_log_stats(&mut self, network_tx: &Sender<NetworkControl>) {
        let Some(voice) = self.vc.as_ref() else {
            return;
        };
        if self.last_stats_log.elapsed() < STATS_LOG_INTERVAL {
            return;
        }
        self.last_stats_log = Instant::now();
        let s = voice.stats();
        let moved = (
            s.packets_sent.load(Relaxed),
            s.packets_received.load(Relaxed),
            s.stream_restarts.load(Relaxed),
        );
        if moved == self.stats_logged {
            return;
        }
        self.stats_logged = moved;
        info!("audio stats: {}", voice.stats_summary());
        let (input_device, output_device) = voice.opened_devices();
        let report_age_ms = s.uplink_report_age_ms();
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
            exercised: report_age_ms.is_some_and(|age| age < 6_000),
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
    /// push-to-talk mode with the key up — and never without a
    /// session, where nothing leaves the device.
    fn effective_mute(&self) -> bool {
        if self.sessionless {
            return false;
        }
        let ptt_closed = self.ptt_mode && !self.ptt_held;
        if let Some(v) = &self.vc {
            self.user_muted || ptt_closed || v.is_deafened()
        } else {
            self.user_muted || ptt_closed
        }
    }

    /// Hand the engine the effective mute — the samples are dropped —
    /// and where we are: out of a room the desktop drops them too, and
    /// on a phone, out of a room or muted, the microphone closes
    /// (`VoiceChat::set_presence`).
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
                config,
                our_client_id: id,
                in_room,
            } => {
                let config = *config;
                // The old engine goes first: its device state (Android's
                // communication mode, iOS's voice-processing unit and
                // session mode) is process-wide, and leaving it after the
                // new one entered would undo the new one's.
                self.vc.take();
                self.relink = true;
                // A fresh engine starts unmuted; push-to-talk closes it
                // until the key is held. Known before `start()`, so a
                // phone never opens a microphone it would close at once.
                self.user_muted = false;
                self.ptt_held = false;
                self.in_room = in_room;
                self.sessionless = id.is_none();
                let mut voice = VoiceChat::new(config);
                voice.set_transmit(self.transmit);
                voice.set_presence(in_room, !self.effective_mute());
                match voice.start() {
                    audio::StartOutcome::Started => {}
                    // Nothing can be heard: no engine this time.
                    audio::StartOutcome::NoPlayback(e) => {
                        event_tx
                            .send(VoiceEvent::Error(format!("Start failed: {e}")))
                            .ok();
                        wake();
                        return;
                    }
                    // Playback is up and only capture failed (no
                    // microphone; on Android the permission not granted
                    // yet). Keep the engine: the room stays audible, and
                    // `service()` retries the input every 2 s until it
                    // opens. Dropping it here silenced everything
                    // (first phone test, 2026-10-05).
                    audio::StartOutcome::NoMicrophone(e) => {
                        event_tx
                            .send(VoiceEvent::Error(format!("Microphone unavailable: {e}")))
                            .ok();
                    }
                }
                self.vc = Some(voice);
                // Generations count per engine: republish.
                self.aec_published = None;
                self.was_speaking = false;
                self.apply_mute();
                event_tx.send(VoiceEvent::Started).ok();
                wake();
                info!("Voice started for client {id:?}");
            }
            VoiceControl::Stop => {
                self.vc = None;
                self.relink = true;
                self.was_speaking = false;
                self.user_muted = false;
                info!("Voice stopped");
            }
            VoiceControl::SetInRoom(in_room) => {
                self.in_room = in_room;
                self.apply_mute();
            }
            VoiceControl::SetTransmit(on) => {
                self.transmit = on;
                if let Some(v) = &self.vc {
                    v.set_transmit(on);
                }
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

fn run_voice_loop(
    control_rx: Receiver<VoiceControl>,
    event_tx: Sender<VoiceEvent>,
    network_tx: Sender<NetworkControl>,
    shared: SharedAudioState,
    is_testing_mic: Arc<AtomicBool>,
    wake: WakeFn,
) {
    let mut state = VoiceLoopState::new();

    while state.running {
        let engine = state
            .vc
            .as_ref()
            .map(|v| (v.speaking_edges(), v.trouble(), state.next_wake(v)));
        match engine {
            // Idle: no engine, nothing to supervise — block until a
            // command arrives.
            None => match control_rx.recv() {
                Ok(ctrl) => state.handle_control(ctrl, &event_tx, &wake),
                Err(_) => state.running = false,
            },
            Some((edges, trouble, due)) => {
                let wait = due.map_or(IDLE_WAIT, |at| at.saturating_duration_since(Instant::now()));
                select! {
                    recv(control_rx) -> msg => match msg {
                        Ok(ctrl) => state.handle_control(ctrl, &event_tx, &wake),
                        Err(_) => state.running = false,
                    },
                    // The speaking light changed: compared below.
                    recv(edges) -> _ => {},
                    // A stream failed or a default moved: serviced below.
                    recv(trouble) -> _ => {},
                    default(wait) => {},
                }
            }
        }

        // Drain any queued control messages
        for _ in 0..32 {
            match control_rx.try_recv() {
                Ok(ctrl) => state.handle_control(ctrl, &event_tx, &wake),
                Err(_) => break,
            }
        }

        // A new engine (or none): the network thread feeds and drains
        // it directly, and the UI reads its meters (review P1-5). A
        // blocking send: the network thread must not miss an engine.
        if std::mem::take(&mut state.relink) {
            let link = state.vc.as_ref().map(|v| VoiceLink {
                incoming: v.incoming_sender(),
                outgoing: v.outgoing_receiver(),
                testing: is_testing_mic.clone(),
                local_only: state.sessionless,
            });
            let _ = network_tx.send(NetworkControl::VoiceLink(link));
            shared.publish(state.vc.as_ref().map(|v| v.stats().clone()));
        }

        // Device supervision (audio 0.7): restart a side whose device
        // went away, follow the OS default when none is pinned. Cheap
        // when nothing changed; the engine logs what it did.
        if let Some(voice) = state.vc.as_mut() {
            let _ = voice.service();
        }

        if let Some(voice) = state.vc.as_ref() {
            // Edge-triggered speaking notification. Blocking sends on
            // purpose: there is no periodic refresh, so a dropped edge
            // means a speaking light stuck in the wrong state until the
            // next transition. Both channels are drained frequently.
            //
            // While mic-testing we're in pure loopback — the network
            // thread drops our packets — and effective speaking is
            // forced false here so the test never lights us up
            // (server-side or on our own sidebar). Treating it as `raw
            // && !testing` makes the edges self-correcting: starting a
            // test mid-sentence emits a clean false edge, stopping it
            // while still talking emits a clean true edge. A test starts
            // and stops with a command, which wakes this loop.
            let speaking = voice.is_speaking()
                && !state.sessionless
                && !is_testing_mic.load(Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tell(state: &mut VoiceLoopState, ctrl: VoiceControl) {
        let (event_tx, _event_rx) = crossbeam_channel::bounded::<VoiceEvent>(4);
        let wake: WakeFn = Arc::new(|| {});
        state.handle_control(ctrl, &event_tx, &wake);
    }

    /// The meter on Settings' Audio tab, not connected: a push-to-talk
    /// user has no session to hold the key in, and nothing the engine
    /// hears can leave. With a session both rule again.
    #[test]
    fn mute_and_push_to_talk_wait_only_while_there_is_no_session() {
        let mut state = VoiceLoopState::new();
        tell(&mut state, VoiceControl::SetPushToTalk(true));
        assert!(state.effective_mute(), "key up");

        // What `Start` records, without the devices it would open.
        state.sessionless = true;
        assert!(!state.effective_mute());
        tell(&mut state, VoiceControl::SetMuted(true));
        assert!(!state.effective_mute());

        state.sessionless = false;
        assert!(state.effective_mute());
        tell(&mut state, VoiceControl::SetPushToTalk(false));
        assert!(state.effective_mute(), "still muted by hand");
        tell(&mut state, VoiceControl::SetMuted(false));
        assert!(!state.effective_mute());
    }
}
