//! Opus encoder thread.
//!
//! Receives raw 480-sample f32 frames from the input pipeline,
//! encodes them to Opus, and sends `AudioPacket`s toward the network.
//!
//! Owns the Opus encoder exclusively — no shared mutex needed.
//! Adaptive FEC adjusts the packet-loss hint every ~2 seconds
//! based on measured loss from `AudioStats`.

use crossbeam_channel::{Receiver, Sender};
use log::{error, info};
use opus::{Application, Bitrate, Channels, Encoder};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::{
    stats::AudioStats, transport::packet::AudioPacket, FRAME_SIZE, OPUS_BITRATE, OPUS_SAMPLE_RATE,
};

/// Encoder thread state. Created once, runs until the input channel closes.
struct EncoderWorker {
    encoder: Encoder,
    rx: Receiver<Vec<f32>>,
    tx: Sender<AudioPacket>,
    stats: Arc<AudioStats>,

    seq: u32,
    ts_48k: u32,
    scratch: Vec<u8>,

    /// Packets encoded since last FEC adaptation check.
    packets_since_fec_update: u32,
}

impl EncoderWorker {
    fn new(
        rx: Receiver<Vec<f32>>,
        tx: Sender<AudioPacket>,
        stats: Arc<AudioStats>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, Channels::Mono, Application::Voip)?;

        // ── Voice chat tuning ───────────────────────────────────────
        let _ = encoder.set_bitrate(Bitrate::Bits(OPUS_BITRATE));
        let _ = encoder.set_vbr(true);
        // Complexity 7: encoder runs on one thread (local user only),
        // so we can afford slightly more CPU for better sibilant quality.
        let _ = encoder.set_complexity(7);
        let _ = encoder.set_bandwidth(opus::Bandwidth::Superwideband);
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(5);
        let _ = encoder.set_dtx(true);

        Ok(Self {
            encoder,
            rx,
            tx,
            stats,
            seq: 0,
            ts_48k: 0,
            scratch: vec![0u8; 256],
            packets_since_fec_update: 0,
        })
    }

    fn run(&mut self) {
        info!("🎤 Encoder thread started");

        while let Ok(samples) = self.rx.recv() {
            if samples.len() != FRAME_SIZE {
                error!(
                    "Encoder: expected {FRAME_SIZE} samples, got {}",
                    samples.len()
                );
                continue;
            }

            self.encode_frame(&samples);
            self.maybe_update_fec();
        }

        info!("🎤 Encoder thread stopped");
    }
    #[expect(
        clippy::needless_return,
        reason = "explicit return exits the thread loop"
    )]
    fn encode_frame(&mut self, samples: &[f32]) {
        let t0 = Instant::now();

        match self.encoder.encode_float(samples, &mut self.scratch) {
            Ok(size) => {
                self.stats
                    .last_encode_us
                    .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);

                // DTX: Opus produces ≤2 byte packets during silence
                self.stats.dtx_active.store(size <= 2, Ordering::Relaxed);

                let packet =
                    AudioPacket::new(self.seq, self.ts_48k, 0, self.scratch[..size].to_vec());

                self.seq = self.seq.wrapping_add(1);
                self.ts_48k = self.ts_48k.wrapping_add(FRAME_SIZE as u32);
                self.stats.record_packet_sent(size);

                if self.tx.send(packet).is_err() {
                    return;
                }
            }
            Err(e) => {
                error!("Encode error: {e}");
                self.stats.encode_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Adjust Opus FEC packet-loss hint every ~200 packets (~2 seconds).
    fn maybe_update_fec(&mut self) {
        self.packets_since_fec_update += 1;
        if self.packets_since_fec_update < 200 {
            return;
        }
        self.packets_since_fec_update = 0;

        let received = self.stats.packets_received.load(Ordering::Relaxed);
        let lost = self.stats.packets_lost.load(Ordering::Relaxed);

        if received + lost > 50 {
            #[allow(clippy::cast_precision_loss)]
            let loss_pct = ((lost as f32 / (received + lost) as f32) * 100.0) as i32;
            let fec_hint = loss_pct.clamp(0, 20);
            let _ = self.encoder.set_packet_loss_perc(fec_hint);
        }
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Spawn the encoder thread. Returns the join handle.
///
/// The thread runs until the `rx` channel is closed (all senders dropped).
pub fn spawn(
    rx: Receiver<Vec<f32>>,
    tx: Sender<AudioPacket>,
    stats: Arc<AudioStats>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || match EncoderWorker::new(rx, tx, stats) {
        Ok(mut worker) => worker.run(),
        Err(e) => error!("Failed to create Opus encoder: {e}"),
    })
}
