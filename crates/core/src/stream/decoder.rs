//! Stereo Opus decoder for incoming screen share audio.
//!
//! Receives raw Opus-encoded stereo packets from the network, decodes
//! them, and plays them through [`StreamAudioOutput`](crate::stream::output::StreamAudioOutput).
//!
//! Mirrors [`crate::decoder`] but simplified for stream audio:
//! - **Single thread** (vs worker pool) — only one stream per peer
//! - **Stereo** (vs mono for voice)
//! - **20 ms frames** (960 samples/ch at 48 kHz)
//! - **PLC for gap recovery** — same strategy as the voice decoder
//! - **No dispatcher** — packets arrive pre-routed per peer
//!
//! ## Architecture
//!
//! ```text
//! network → StreamAudioPacket → [decoder thread] → StreamAudioFrame → StreamAudioOutput → cpal
//! ```
//!
//! The public [`spawn`] function wires the full pipeline: it creates the
//! decoder thread, the decoded-frame channel, and the playback output,
//! returning a [`StreamDecoder`] handle that accepts raw Opus packets
//! and exposes volume/mute controls.

use crossbeam_channel::{bounded, Receiver, Sender};
use log::{error, info, warn};
use opus::{Channels, Decoder};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use crate::stream::output::{StreamAudioFrame, StreamAudioOutput};
use crate::OPUS_SAMPLE_RATE;

/// Samples per channel per frame (20 ms at 48 kHz).
const STREAM_FRAME_SIZE: usize = 960;

/// Number of channels.
const STREAM_CHANNELS: usize = 2;

/// Interleaved decode buffer size: 960 samples/ch × 2 channels.
const DECODE_BUF_SIZE: usize = STREAM_FRAME_SIZE * STREAM_CHANNELS;

// ── Incoming packet ─────────────────────────────────────────────────────

/// A raw Opus-encoded stream audio packet from the network.
///
/// The network layer produces these from the wire format; the decoder
/// consumes them. Kept intentionally simple — just bytes and a sequence
/// number for gap detection.
#[derive(Debug, Clone)]
pub struct StreamAudioPacket {
    /// Opus-encoded stereo audio data.
    pub data: Vec<u8>,
    /// Sequence number for ordering and gap detection.
    pub seq: u32,
}

// ── Decoder worker ──────────────────────────────────────────────────────

/// Decoder thread state. One per peer stream.
struct StreamDecoderWorker {
    decoder: Decoder,
    rx: Receiver<StreamAudioPacket>,
    tx: Sender<StreamAudioFrame>,
    should_stop: Arc<AtomicBool>,

    last_seq: Option<u32>,
    frame_buffer: Vec<f32>,
}

impl StreamDecoderWorker {
    fn new(
        rx: Receiver<StreamAudioPacket>,
        tx: Sender<StreamAudioFrame>,
        should_stop: Arc<AtomicBool>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let decoder = Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo)?;

        Ok(Self {
            decoder,
            rx,
            tx,
            should_stop,
            last_seq: None,
            frame_buffer: vec![0.0f32; DECODE_BUF_SIZE],
        })
    }

    fn run(&mut self) {
        info!("🔊🎬 Stream decoder thread started");

        while let Ok(packet) = self.rx.recv() {
            if self.should_stop.load(Ordering::Relaxed) {
                break;
            }
            self.process_packet(packet);
        }

        info!("🔊🎬 Stream decoder thread stopped");
    }

    /// Process one incoming packet: gap recovery via PLC + decode.
    ///
    /// Intentionally one method — the Opus decoder is stateful and PLC
    /// must happen in exact sequence before the current packet's decode.
    /// See [`crate::decoder::DecoderWorker::process_packet`] for the
    /// detailed rationale on why this isn't split into sub-methods.
    fn process_packet(&mut self, packet: StreamAudioPacket) {
        let curr = packet.seq;

        // ── Sequence gap detection & PLC recovery ───────────────────
        if let Some(last) = self.last_seq {
            let expect = last.wrapping_add(1);
            let ahead = curr.wrapping_sub(expect);
            let behind = expect.wrapping_sub(curr);

            // Late packet — already played past this sequence, drop it.
            if behind > 0 && behind <= 10 {
                return;
            }

            // Gap detected — fill with PLC frames so playback stays
            // continuous (avoids clicks/pops in music/game audio).
            if ahead > 0 && ahead <= 10 {
                let gap = ahead as usize;
                for _ in 0..gap {
                    match self
                        .decoder
                        .decode_float(&[], &mut self.frame_buffer, false)
                    {
                        Ok(n) => {
                            let total = n * STREAM_CHANNELS;
                            let _ = self.tx.send(StreamAudioFrame {
                                samples: self.frame_buffer[..total].to_vec(),
                                seq: 0, // concealment — seq not meaningful
                            });
                        }
                        Err(e) => {
                            warn!("Stream PLC decode failed: {e}");
                        }
                    }
                }
            } else if ahead > 10 {
                // Large gap — decoder state is too stale, reset.
                warn!("Stream audio seq jumped by {ahead} — resetting decoder");
                match Decoder::new(OPUS_SAMPLE_RATE, Channels::Stereo) {
                    Ok(new_dec) => self.decoder = new_dec,
                    Err(e) => error!("Failed to reset stream decoder: {e}"),
                }
            }
        }

        // ── Decode current packet ───────────────────────────────────
        match self
            .decoder
            .decode_float(&packet.data, &mut self.frame_buffer, false)
        {
            Ok(samples_per_channel) => {
                let total = samples_per_channel * STREAM_CHANNELS;

                let _ = self.tx.send(StreamAudioFrame {
                    samples: self.frame_buffer[..total].to_vec(),
                    seq: curr,
                });

                self.last_seq = Some(curr);
            }
            Err(e) => {
                error!("Stream audio decode error seq {curr}: {e}");

                // Decode failed — PLC so playback doesn't gap.
                if let Ok(n) = self
                    .decoder
                    .decode_float(&[], &mut self.frame_buffer, false)
                {
                    let total = n * STREAM_CHANNELS;
                    let _ = self.tx.send(StreamAudioFrame {
                        samples: self.frame_buffer[..total].to_vec(),
                        seq: curr,
                    });
                }

                self.last_seq = Some(curr);
            }
        }
    }
}

// ── Public handle ───────────────────────────────────────────────────────

/// Handle to a running stream audio decoder + playback pipeline.
///
/// Owns the decoder thread and the [`StreamAudioOutput`] for one peer's
/// screen share audio. Dropping this handle stops both the decoder
/// thread and the playback pipeline.
pub struct StreamDecoder {
    /// Send raw Opus packets here for decoding.
    packet_tx: Sender<StreamAudioPacket>,
    /// Volume control (f32 bits, 0.0–2.0). Writes to the same atomic
    /// that StreamAudioOutput reads from.
    volume: Arc<AtomicU32>,
    /// Mute control. Writes to the same atomic that StreamAudioOutput reads from.
    is_muted: Arc<AtomicBool>,
    /// Signal to stop the decoder thread.
    should_stop: Arc<AtomicBool>,
    /// Decoder thread join handle.
    decoder_thread: Option<std::thread::JoinHandle<()>>,
    /// Playback pipeline (ring buffer → resample → cpal).
    /// Dropped after the decoder thread to avoid deadlock.
    _output: StreamAudioOutput,
}

impl StreamDecoder {
    /// Send a raw Opus-encoded packet for decoding and playback.
    ///
    /// Non-blocking. Drops the packet if the decoder is backed up.
    pub fn send(&self, packet: StreamAudioPacket) {
        let _ = self.packet_tx.try_send(packet);
    }

    /// Set playback volume (0.0–2.0).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
    }

    /// Get current volume.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Mute/unmute stream audio playback.
    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    /// Whether playback is currently muted.
    pub fn is_muted(&self) -> bool {
        self.is_muted.load(Ordering::Relaxed)
    }
}

impl Drop for StreamDecoder {
    fn drop(&mut self) {
        // Signal the decoder thread to stop, then drop the packet sender
        // to unblock its recv() call.
        self.should_stop.store(true, Ordering::Relaxed);
        let (dead_tx, _) = bounded::<StreamAudioPacket>(0);
        self.packet_tx = dead_tx;
        if let Some(thread) = self.decoder_thread.take() {
            let _ = thread.join();
        }
        // _output is dropped here automatically, stopping playback.
        info!("🔊🎬 Stream decoder dropped");
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Create and start a stream audio decoder + playback pipeline.
///
/// Wires the full chain: decoder thread → frame channel → [`StreamAudioOutput`].
/// Returns a [`StreamDecoder`] handle for feeding packets and controlling
/// volume/mute.
///
/// - `device_name`: output device name (or `None` for system default)
/// - `volume`: initial playback volume (0.0–2.0)
pub fn spawn(
    device_name: Option<&str>,
    volume: f32,
) -> Result<StreamDecoder, Box<dyn std::error::Error>> {
    // Create the playback pipeline.
    let mut output = StreamAudioOutput::new(device_name, volume)?;

    // Grab the atomic handles BEFORE starting — these are the same
    // Arc<Atomic*> that the output's worker thread reads from, so
    // writes from the StreamDecoder handle are visible immediately.
    let volume_handle = output.volume_handle();
    let muted_handle = output.muted_handle();

    // Get the output's internal frame sender so the decoder thread
    // can push decoded frames directly without an extra channel hop.
    let frame_tx = output.frame_sender();

    output.start()?;

    // Packet channel: screen.rs → decoder thread.
    // 16 slots ≈ 320 ms at 20 ms/frame — generous for bursty delivery.
    let (packet_tx, packet_rx) = bounded::<StreamAudioPacket>(16);
    let should_stop = Arc::new(AtomicBool::new(false));

    let stop = should_stop.clone();
    let decoder_thread =
        std::thread::spawn(
            move || match StreamDecoderWorker::new(packet_rx, frame_tx, stop) {
                Ok(mut worker) => worker.run(),
                Err(e) => error!("Failed to create stream Opus decoder: {e}"),
            },
        );

    Ok(StreamDecoder {
        packet_tx,
        volume: volume_handle,
        is_muted: muted_handle,
        should_stop,
        decoder_thread: Some(decoder_thread),
        _output: output,
    })
}
