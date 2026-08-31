//! Stereo Opus decoder for incoming screen share audio.
//!
//! Receives raw Opus-encoded stereo packets from the network, decodes
//! them, and plays them through [`StreamAudioOutput`].
//!
//! ## Architecture
//!
//! ```text
//! network → StreamAudioPacket → [decoder thread] → StreamAudioFrame → StreamAudioOutput → cpal
//! ```

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use crossbeam_channel::{bounded, Receiver, Sender};
use log::{error, info, warn};
use opus::{Channels, Decoder};

use crate::{
    stream::output::{StreamAudioFrame, StreamAudioOutput},
    OPUS_SAMPLE_RATE,
};

/// Samples per channel per frame (20 ms at 48 kHz).
const STREAM_FRAME_SIZE: usize = 960;

/// Number of channels.
const STREAM_CHANNELS: usize = 2;

/// Interleaved decode buffer size: 960 samples/ch × 2 channels.
const DECODE_BUF_SIZE: usize = STREAM_FRAME_SIZE * STREAM_CHANNELS;

// ── Incoming packet ─────────────────────────────────────────────────────

/// A raw Opus-encoded stream audio packet from the network.
#[derive(Debug, Clone)]
pub struct StreamAudioPacket {
    /// Opus-encoded stereo audio data.
    pub data: Vec<u8>,
    /// Sequence number for ordering and gap detection.
    pub seq: u32,
}

// ── Decoder worker ──────────────────────────────────────────────────────

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
        info!("Stream decoder thread started");

        while let Ok(packet) = self.rx.recv() {
            if self.should_stop.load(Ordering::Relaxed) {
                break;
            }
            self.process_packet(packet);
        }

        info!("Stream decoder thread stopped");
    }

    fn process_packet(&mut self, packet: StreamAudioPacket) {
        let curr = packet.seq;

        // ── Sequence gap detection & PLC recovery ───────────────────
        if let Some(last) = self.last_seq {
            let expect = last.wrapping_add(1);
            let ahead = curr.wrapping_sub(expect);
            let behind = expect.wrapping_sub(curr);

            if behind > 0 && behind <= 10 {
                return; // late packet
            }

            if ahead > 0 && ahead <= 10 {
                let gap = ahead as usize;
                for _ in 0..gap {
                    match self
                        .decoder
                        .decode_float(&[], &mut self.frame_buffer, false)
                    {
                        Ok(n) => {
                            let total = n * STREAM_CHANNELS;
                            let _ = self.tx.try_send(StreamAudioFrame {
                                samples: self.frame_buffer[..total].to_vec(),
                                seq: 0,
                            });
                        }
                        Err(e) => {
                            warn!("Stream PLC decode failed: {e}");
                        }
                    }
                }
            } else if ahead > 10 {
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

                let _ = self.tx.try_send(StreamAudioFrame {
                    samples: self.frame_buffer[..total].to_vec(),
                    seq: curr,
                });

                self.last_seq = Some(curr);
            }
            Err(e) => {
                error!("Stream audio decode error seq {curr}: {e}");

                if let Ok(n) = self
                    .decoder
                    .decode_float(&[], &mut self.frame_buffer, false)
                {
                    let total = n * STREAM_CHANNELS;
                    let _ = self.tx.try_send(StreamAudioFrame {
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
/// Dropping this handle stops both the decoder thread and playback.
pub struct StreamDecoder {
    packet_tx: Sender<StreamAudioPacket>,
    volume: Arc<AtomicU32>,
    is_muted: Arc<AtomicBool>,
    should_stop: Arc<AtomicBool>,
    decoder_thread: Option<std::thread::JoinHandle<()>>,
    _output: StreamAudioOutput,
}

impl StreamDecoder {
    /// Send a raw Opus-encoded packet for decoding and playback.
    pub fn send(&self, packet: StreamAudioPacket) {
        let _ = self.packet_tx.try_send(packet);
    }

    /// Set playback volume (0.0–2.0).
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Mute/unmute stream audio playback.
    pub fn set_muted(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.is_muted.load(Ordering::Relaxed)
    }
}

impl Drop for StreamDecoder {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Relaxed);
        let (dead_tx, _) = bounded::<StreamAudioPacket>(0);
        self.packet_tx = dead_tx;
        if let Some(thread) = self.decoder_thread.take() {
            let _ = thread.join();
        }
        info!("Stream decoder dropped");
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Create and start a stream audio decoder + playback pipeline.
///
/// - `device_name`: output device (or `None` for system default)
/// - `volume`: initial playback volume (0.0–2.0)
pub fn spawn(
    device_name: Option<&str>,
    volume: f32,
) -> Result<StreamDecoder, Box<dyn std::error::Error>> {
    let mut output = StreamAudioOutput::new(device_name, volume)?;

    let volume_handle = output.volume_handle();
    let muted_handle = output.muted_handle();
    let frame_tx = output.frame_sender();

    output.start()?;

    let (packet_tx, packet_rx) = bounded::<StreamAudioPacket>(16);
    let should_stop = Arc::new(AtomicBool::new(false));

    let stop = should_stop.clone();
    let decoder_thread = crate::platform::spawn_named("stream-decode", move || {
        match StreamDecoderWorker::new(packet_rx, frame_tx, stop) {
            Ok(mut worker) => worker.run(),
            Err(e) => error!("Failed to create stream Opus decoder: {e}"),
        }
    });

    Ok(StreamDecoder {
        packet_tx,
        volume: volume_handle,
        is_muted: muted_handle,
        should_stop,
        decoder_thread: Some(decoder_thread),
        _output: output,
    })
}
