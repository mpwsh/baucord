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
//! ## Device compatibility
//!
//! The engine asks each device for 48 kHz first (Opus' native rate) and
//! only falls back to the OS default rate — with a resampler — when the
//! device cannot do 48 kHz (see [`device`]). Bluetooth HFP mics at 8/16 kHz,
//! 44.1 kHz interfaces and anything else still just work.

mod config;
mod device;
mod encoder;
mod input;
mod input_config;
mod output;
mod platform;
pub mod processor;
mod resampler;
pub mod sounds;
pub mod stats;
pub mod stream;
pub mod transport;

pub use config::{PeerAudioSettings, VoiceChatConfig};
pub use sounds::SoundType;
pub use stats::AudioStats;
pub use transport::packet::AudioPacket;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use log::{debug, error, info};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait};

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

/// Default Opus bitrate (bits/s)
const OPUS_BITRATE: i32 = 48_000;

/// Maximum expected peers. Used for channel sizing.
const DEFAULT_MAX_PEERS: usize = 32;

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
        )?;

        input.start()?;
        self.is_speaking = input.speaking_handle();
        self.input = Some(input);
        Ok(())
    }

    fn start_output(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let sound_rx = self.sound_rx.take().unwrap_or_else(|| {
            let (_, rx) = bounded::<sounds::SoundType>(1);
            rx
        });

        let mut output = AudioOutput::new(
            self.config.output_device.as_deref(),
            self.network_to_output_rx.clone(),
            self.loopback_rx.clone(),
            sound_rx,
            self.user_settings.clone(),
            self.removed_peers.clone(),
            self.stats.clone(),
            self.config.output_gain,
        )?;
        self.output_gain = output.output_gain_handle();
        self.deafened = output.deafened_handle();
        output.start()?;
        self.output = Some(output);
        Ok(())
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
        let _ = self
            .network_to_output_tx
            .try_send((packet, Instant::now()));
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
    /// here whenever it refreshes (every couple of seconds is plenty);
    /// values the transport can't measure reliably (idle link, huge
    /// implausible readings) should be withheld rather than passed.
    pub fn set_uplink_loss_pct(&self, loss_pct: u32) {
        self.stats
            .uplink_loss_pct
            .store(loss_pct.min(100), Ordering::Relaxed);
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
        self.output.take();
        self.config.output_device = device_name.map(String::from);
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
        static CACHE: Mutex<Option<(std::time::Instant, Vec<(String, bool)>)>> = Mutex::new(None);
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
        // Same 10 s cache as list_input_devices — see the note there.
        static CACHE: Mutex<Option<(std::time::Instant, Vec<(String, bool)>)>> = Mutex::new(None);
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
    use super::*;
    use std::time::Duration as StdDuration;

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
        assert!(
            received >= 15,
            "intake only counted {received}/20 packets"
        );

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
            let _ = tx.send(AudioPacket::new(seq, seq * FRAME_SIZE as u32, 1, vec![0xFC]));
        }
    }
}

#[cfg(test)]
mod voice_loop_repro {
    use super::*;
    use crossbeam_channel::{bounded as cb_bounded, select};
    use std::time::Duration as StdDuration;

    enum Ctrl {
        Incoming(AudioPacket),
        Shutdown,
    }

    /// Replicates client/voice.rs's select loop shape (post-fix) against a
    /// real VoiceChat: per-iteration outgoing clone, three-arm select with
    /// dead-channel fallback, control drain, take_outgoing_packets poll.
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
