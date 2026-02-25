//! Stereo Opus encoder thread for system audio (screen share).
//!
//! Receives interleaved stereo f32 frames from the loopback capture
//! pipeline, encodes them to Opus, and sends [`SystemAudioPacket`]s
//! toward the network.
//!
//! Mirrors [`crate::encoder`] but tuned for music/game audio:
//! - **Stereo** (vs mono for voice)
//! - **128 kbps** (vs 24 kbps for voice)
//! - **`Application::Audio`** (vs `Application::Voip`)
//! - **20 ms frames** (vs 10 ms for voice — lower latency isn't needed)
//! - **No DTX** — system audio transmits continuously while sharing
//!
//! Owns the Opus encoder exclusively — no shared mutex needed.

use crossbeam_channel::{Receiver, Sender};
use log::{error, info, warn};
use opus::{Application, Bitrate, Channels, Encoder};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use crate::stream::input::SystemAudioPacket;
use crate::OPUS_SAMPLE_RATE;

/// Opus bitrate for system audio (bits/s).
/// 128 kbps stereo gives excellent quality for music/game audio.
const STREAM_BITRATE: i32 = 128_000;

/// Samples per channel per frame (20 ms at 48 kHz).
const STREAM_FRAME_SIZE: usize = 960;

/// Number of channels.
const STREAM_CHANNELS: usize = 2;

/// Expected interleaved frame length: 960 samples/ch × 2 channels.
const INTERLEAVED_FRAME_SIZE: usize = STREAM_FRAME_SIZE * STREAM_CHANNELS;

// ── Encoder worker ──────────────────────────────────────────────────────

/// Encoder thread state. Created once, runs until the input channel closes.
struct StreamEncoderWorker {
    encoder: Encoder,
    rx: Receiver<Vec<f32>>,
    tx: Sender<SystemAudioPacket>,

    /// Volume applied before encoding (f32 bits, 0.0–2.0).
    volume: Arc<AtomicU32>,
    /// Mute: drain frames without encoding.
    is_muted: Arc<AtomicBool>,

    seq: u32,
    scratch: Vec<u8>,
}

impl StreamEncoderWorker {
    fn new(
        rx: Receiver<Vec<f32>>,
        tx: Sender<SystemAudioPacket>,
        volume: Arc<AtomicU32>,
        is_muted: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, Channels::Stereo, Application::Audio)?;

        // ── Music/game audio tuning ─────────────────────────────────
        let _ = encoder.set_bitrate(Bitrate::Bits(STREAM_BITRATE));
        let _ = encoder.set_vbr(true);
        // Complexity 10: system audio is one stream per machine, we can
        // afford max quality. The encoder thread is not latency-critical.
        let _ = encoder.set_complexity(10);
        // No DTX — continuous transmission while screen sharing.
        let _ = encoder.set_dtx(false);
        // FEC helps recover music/game audio quality on lossy links.
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(5);

        Ok(Self {
            encoder,
            rx,
            tx,
            volume,
            is_muted,
            seq: 0,
            // 4096 bytes is generous for 128 kbps stereo 20 ms frames
            // (~320 bytes typical, ~640 bytes worst case with FEC).
            scratch: vec![0u8; 4096],
        })
    }

    fn run(&mut self) {
        info!("🔊🎤 Stream encoder thread started");

        while let Ok(mut frame) = self.rx.recv() {
            // Muted: drain but don't encode.
            if self.is_muted.load(Ordering::Relaxed) {
                continue;
            }

            if frame.len() != INTERLEAVED_FRAME_SIZE {
                error!(
                    "Stream encoder: expected {INTERLEAVED_FRAME_SIZE} interleaved samples, got {}",
                    frame.len()
                );
                continue;
            }

            self.apply_volume(&mut frame);
            self.encode_frame(&frame);
        }

        info!("🔊🎤 Stream encoder thread stopped");
    }

    /// Apply volume scaling in-place before encoding.
    fn apply_volume(&self, frame: &mut [f32]) {
        let vol = f32::from_bits(self.volume.load(Ordering::Relaxed));
        if (vol - 1.0).abs() > f32::EPSILON {
            for s in frame.iter_mut() {
                *s = (*s * vol).clamp(-0.99, 0.99);
            }
        }
    }

    fn encode_frame(&mut self, frame: &[f32]) {
        // Opus encode_float for stereo expects interleaved samples with
        // frame_size = samples per channel (960 for 20 ms at 48 kHz).
        match self.encoder.encode_float(frame, &mut self.scratch) {
            Ok(size) => {
                let packet = SystemAudioPacket {
                    data: self.scratch[..size].to_vec(),
                    seq: self.seq,
                    duration_ms: 20,
                    channels: STREAM_CHANNELS as u8,
                };

                self.seq = self.seq.wrapping_add(1);

                if self.tx.send(packet).is_err() {
                    // Consumer disconnected — stop encoding.
                    return;
                }
            }
            Err(e) => {
                error!("Stream audio encode error: {e}");
            }
        }
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Spawn the stream audio encoder thread. Returns the join handle.
///
/// The thread runs until the `rx` channel is closed (all senders dropped).
///
/// - `rx`: interleaved stereo f32 frames (1920 samples = 960/ch × 2ch)
/// - `tx`: encoded [`SystemAudioPacket`]s sent toward the network
/// - `volume`: shared atomic for runtime volume adjustment (f32 bits)
/// - `is_muted`: shared atomic to mute without stopping the thread
pub fn spawn(
    rx: Receiver<Vec<f32>>,
    tx: Sender<SystemAudioPacket>,
    volume: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(
        move || match StreamEncoderWorker::new(rx, tx, volume, is_muted) {
            Ok(mut worker) => worker.run(),
            Err(e) => error!("Failed to create stream Opus encoder: {e}"),
        },
    )
}
