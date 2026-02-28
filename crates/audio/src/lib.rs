//! # voice-chat-core
//!
//! Low-latency voice chat engine with noise suppression.
//!
//! ## Input pipeline (processor chain)
//!
//! ```text
//! capture → resample → denoise → meter → gate → compressor → AGC → gain → clamp → meter → VAD → encode
//! ```
//!
//! ## Output pipeline
//!
//! ```text
//! network → decode (+ FEC/PLC) → jitter buffer → gain → resample → playback
//! ```
//!
//! ## Device compatibility
//!
//! The engine accepts whatever sample rate and channel count the OS gives us
//! via `default_input_config()` / `default_output_config()`. Bluetooth headphones,
//! USB mics, audio interfaces — they all just work. We resample to 48 kHz internally.
//!
//! ## Scaling
//!
//! Decoding is parallelised across a worker pool (dispatched by peer SSRC),
//! allowing 30+ simultaneous speakers without garbling or starvation.

mod config;
mod decoder;
mod encoder;
mod input;
mod input_config;
mod output;
pub mod processor;
mod resampler;
pub mod sounds;
pub mod stats;
pub mod stream;
pub mod transport;

pub use config::{PeerAudioSettings, VoiceChatConfig};
pub use sounds::SoundType;
pub use stats::AudioStats;
pub use transport::{jitter_buffer::DecodedFrame, packet::AudioPacket};

use crossbeam_channel::{bounded, Receiver, Sender};
use log::{error, info};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait};
use input::AudioInput;
use output::AudioOutput;

use crate::input_config::InputConfig;

/// Internal pipeline sample rate (Opus native).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;

/// Samples per 10 ms frame at 48 kHz.
pub const FRAME_SIZE: usize = 480;

/// Default Opus bitrate (bits/s). 24 kbps for 10 ms VoIP frames.
const OPUS_BITRATE: i32 = 24_000;

/// Maximum expected peers. Used for channel sizing.
const DEFAULT_MAX_PEERS: usize = 32;

/// The main voice chat engine.
///
/// Owns the full audio pipeline from capture to playback.
/// The caller is responsible for shuttling [`AudioPacket`]s to/from the network.
pub struct VoiceChat {
    input: Option<AudioInput>,
    output: Option<AudioOutput>,
    input_config: Arc<InputConfig>,
    // ── Channels (each side stored separately, owned by who uses it) ─
    audio_to_encoder_tx: Sender<Vec<f32>>,
    encoder_to_network_rx: Receiver<AudioPacket>,
    network_to_decoder_tx: Sender<AudioPacket>,
    decoder_to_audio_tx: Sender<DecodedFrame>,
    decoder_to_audio_rx: Receiver<DecodedFrame>,
    sound_tx: Sender<sounds::SoundType>,
    sound_rx: Option<Receiver<sounds::SoundType>>,

    // ── Thread handles ───────────────────────────────────────────────
    encoder_thread: Option<std::thread::JoinHandle<()>>,
    decoder_pool: Option<decoder::DecoderPool>,
    should_stop: Arc<AtomicBool>,

    // ── Shared state ─────────────────────────────────────────────────
    user_settings: Arc<Mutex<HashMap<u64, PeerAudioSettings>>>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,
    stats: Arc<AudioStats>,
    is_speaking: Arc<AtomicBool>,
    output_gain: Arc<AtomicU32>,
    deafened: Arc<AtomicBool>,

    config: VoiceChatConfig,
}

impl VoiceChat {
    pub fn new(config: VoiceChatConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let max_peers = DEFAULT_MAX_PEERS;

        // ── Build channels ──────────────────────────────────────────
        let (audio_to_encoder_tx, audio_to_encoder_rx) = bounded(4);
        let (encoder_to_network_tx, encoder_to_network_rx) = bounded(8);
        let (network_to_decoder_tx, network_to_decoder_rx) = bounded(max_peers * 4);
        let (decoder_to_audio_tx, decoder_to_audio_rx) = bounded(max_peers * 2);
        let (sound_tx, sound_rx) = bounded::<sounds::SoundType>(4);

        // ── Shared atomics (created once, passed to children) ───────
        let output_gain = Arc::new(AtomicU32::new(config.output_gain.to_bits()));
        let deafened = Arc::new(AtomicBool::new(false));
        let should_stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(AudioStats::new());
        let removed_peers = Arc::new(Mutex::new(HashSet::new()));

        // ── Spawn encoder thread ────────────────────────────────────
        let encoder_thread =
            encoder::spawn(audio_to_encoder_rx, encoder_to_network_tx, stats.clone());

        // ── Spawn decoder pool ──────────────────────────────────────
        let decoder_pool = decoder::spawn_pool(
            network_to_decoder_rx,
            decoder_to_audio_tx.clone(),
            stats.clone(),
            should_stop.clone(),
            removed_peers.clone(),
        );
        let input_config = Arc::new(InputConfig::new(
            config.noise_suppression,
            config.input_gain,
            config.gate_threshold,
            true, // compressor_enabled
            true, // agc_enabled
            0.10, // agc_target_rms
        ));

        Ok(Self {
            input: None,
            output: None,
            audio_to_encoder_tx,
            encoder_to_network_rx,
            network_to_decoder_tx,
            decoder_to_audio_tx,
            decoder_to_audio_rx,
            sound_tx,
            input_config,
            sound_rx: Some(sound_rx),
            encoder_thread: Some(encoder_thread),
            decoder_pool: Some(decoder_pool),
            should_stop,
            user_settings: Arc::new(Mutex::new(HashMap::new())),
            removed_peers,
            stats,
            is_speaking: Arc::new(AtomicBool::new(false)),
            output_gain,
            deafened,
            config,
        })
    }

    /// Start the audio pipeline (capture + playback).
    ///
    /// Encoder and decoder threads are already running from `new()`.
    pub fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.start_input()?;
        self.start_output()?;
        Ok(())
    }

    fn start_input(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let loopback_tx = self.decoder_to_audio_tx.clone();
        let mut input = AudioInput::new(
            self.config.input_device.as_deref(),
            self.audio_to_encoder_tx.clone(),
            Some(loopback_tx),
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
            self.decoder_to_audio_rx.clone(),
            sound_rx,
            self.user_settings.clone(),
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

    pub fn receive_packet(&self, packet: AudioPacket) {
        let _ = self.network_to_decoder_tx.try_send(packet);
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

    pub fn incoming_sender(&self) -> Sender<AudioPacket> {
        self.network_to_decoder_tx.clone()
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
        let host = cpal::default_host();
        let default_name = host.default_input_device().and_then(|d| d.name().ok());
        let mut devices = Vec::new();
        for device in host.input_devices()? {
            if let Ok(name) = device.name() {
                let is_default = default_name.as_deref() == Some(&name);
                if let Ok(cfg) = device.default_input_config() {
                    info!(
                        "  Input device: \"{name}\" {}Hz {}ch {:?}{}",
                        cfg.sample_rate().0,
                        cfg.channels(),
                        cfg.sample_format(),
                        if is_default { " [DEFAULT]" } else { "" },
                    );
                }
                devices.push((name, is_default));
            }
        }
        info!("Found {} input devices", devices.len());
        Ok(devices)
    }

    pub fn list_output_devices() -> Result<Vec<(String, bool)>, Box<dyn std::error::Error>> {
        let host = cpal::default_host();
        let default_name = host.default_output_device().and_then(|d| d.name().ok());
        let mut devices = Vec::new();
        for device in host.output_devices()? {
            if let Ok(name) = device.name() {
                let is_default = default_name.as_deref() == Some(&name);
                if let Ok(cfg) = device.default_output_config() {
                    info!(
                        "  Output device: \"{name}\" {}Hz {}ch {:?}{}",
                        cfg.sample_rate().0,
                        cfg.channels(),
                        cfg.sample_format(),
                        if is_default { " [DEFAULT]" } else { "" },
                    );
                }
                devices.push((name, is_default));
            }
        }
        info!("Found {} output devices", devices.len());
        Ok(devices)
    }
}

impl Drop for VoiceChat {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Release);

        // Drop audio devices first — stops platform callbacks.
        self.input.take();
        self.output.take();

        // Drop channel senders to unblock threads blocked on recv().
        let (dead_tx, _) = crossbeam_channel::bounded::<Vec<f32>>(0);
        self.audio_to_encoder_tx = dead_tx;

        let (dead_tx, _) = crossbeam_channel::bounded::<AudioPacket>(0);
        self.network_to_decoder_tx = dead_tx;

        if let Some(thread) = self.encoder_thread.take() {
            let _ = thread.join();
        }

        if let Some(pool) = self.decoder_pool.take() {
            let _ = pool.dispatcher.join();
            for thread in pool.workers {
                let _ = thread.join();
            }
        }
    }
}
