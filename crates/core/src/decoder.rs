//! Parallel Opus decoder pool.
//!
//! Architecture:
//! - One **dispatcher** thread reads incoming packets and routes them
//!   to workers by `ssrc % DECODER_WORKERS`.
//! - Each **worker** thread owns its own `HashMap<u64, Decoder>` — no
//!   shared mutex during decode operations.
//! - All workers send decoded audio to a shared output channel for the
//!   jitter buffer / mixer.
//!
//! This eliminates the serialized single-decoder bottleneck that caused
//! garbled audio at 15+ simultaneous peers.

use crossbeam_channel::{bounded, Receiver, Sender};
use log::{error, info, warn};
use opus::{Channels, Decoder};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::jitter_buffer::DecodedFrame;
use crate::packet::AudioPacket;
use crate::stats::AudioStats;
use crate::{FRAME_SIZE, OPUS_SAMPLE_RATE};

/// Number of decoder worker threads.
pub const DECODER_WORKERS: usize = 4;

// ── Decoder worker ──────────────────────────────────────────────────────

/// Per-worker state. Owns decoders for the peers routed to this worker.
struct DecoderWorker {
    id: usize,
    rx: Receiver<AudioPacket>,
    tx: Sender<DecodedFrame>,
    stats: Arc<AudioStats>,
    should_stop: Arc<AtomicBool>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,

    decoders: HashMap<u64, Decoder>,
    last_seq: HashMap<u64, u32>,
    frame_buffer: Vec<f32>,
}

impl DecoderWorker {
    fn new(
        id: usize,
        rx: Receiver<AudioPacket>,
        tx: Sender<DecodedFrame>,
        stats: Arc<AudioStats>,
        should_stop: Arc<AtomicBool>,
        removed_peers: Arc<Mutex<HashSet<u64>>>,
    ) -> Self {
        Self {
            id,
            rx,
            tx,
            stats,
            should_stop,
            removed_peers,
            decoders: HashMap::new(),
            last_seq: HashMap::new(),
            frame_buffer: vec![0.0f32; FRAME_SIZE * 2],
        }
    }

    fn run(&mut self) {
        info!("🔊 Decoder worker {} started", self.id);

        while let Ok(packet) = self.rx.recv() {
            if self.should_stop.load(Ordering::Relaxed) {
                break;
            }
            self.process_packet(packet);
        }

        info!("🔊 Decoder worker {} stopped", self.id);
    }

    /// Process one incoming packet: gap recovery + decode.
    ///
    /// This is intentionally one method (not split into sub-methods) because
    /// the Opus decoder is stateful — PLC, FEC, and normal decode must happen
    /// in exact sequence on the same `&mut Decoder`. Splitting into methods
    /// that each do `self.decoders.get_mut()` would fight the borrow checker
    /// and risk reordering decoder state mutations.
    fn process_packet(&mut self, packet: AudioPacket) {
        // Stamp arrival time NOW — before any decode work.
        // This is the critical timing point for jitter estimation.
        let arrival_time = Instant::now();
        let ssrc = packet.header.ssrc;

        // ── Check if peer was removed ───────────────────────────────
        {
            let Ok(mut removed_set) = self.removed_peers.lock() else {
                return;
            };
            if removed_set.contains(&ssrc) {
                self.decoders.remove(&ssrc);
                self.last_seq.remove(&ssrc);
                removed_set.remove(&ssrc);
                return;
            }
        }

        let curr = packet.header.seq;
        let worker_id = self.id;

        let decoder = self.decoders.entry(ssrc).or_insert_with(|| {
            info!("Worker {worker_id}: adding decoder for peer {ssrc}");
            match Decoder::new(OPUS_SAMPLE_RATE, Channels::Mono) {
                Ok(d) => d,
                Err(e) => {
                    // This is extremely unlikely (Opus init failure) but we must
                    // not panic on an audio thread. Create a decoder that will
                    // produce silence via PLC on every call.
                    error!("Failed to create decoder for peer {ssrc}: {e}");
                    // Last resort — try once more. If this also fails, the
                    // application has deeper problems.
                    Decoder::new(OPUS_SAMPLE_RATE, Channels::Mono)
                        .expect("Opus decoder init failed twice — cannot recover")
                }
            }
        });

        // ── Sequence gap detection & recovery ───────────────────────
        let last = *self.last_seq.get(&ssrc).unwrap_or(&curr.wrapping_sub(1));
        let expect = last.wrapping_add(1);
        let ahead = curr.wrapping_sub(expect);
        let behind = expect.wrapping_sub(curr);

        // Late packet — already played past this, drop it
        if behind > 0 && behind <= 10 {
            return;
        }

        // Gap detected — recover missing frames
        if ahead > 0 && ahead <= 20 {
            let gap = ahead.min(10) as usize;
            self.stats
                .packets_lost
                .fetch_add(ahead as u64, Ordering::Relaxed);

            // PLC for frames 1..N-1
            if gap > 1 {
                for j in 0..(gap - 1) {
                    match decoder.decode_float(&[], &mut self.frame_buffer, false) {
                        Ok(n) => {
                            let _ = self.tx.send(DecodedFrame {
                                peer_id: ssrc,
                                seq: expect.wrapping_add(j as u32),
                                arrival_time,
                                samples: self.frame_buffer[..n].to_vec(),
                                is_concealment: true,
                            });
                            self.stats.plc_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            warn!("PLC decode failed for peer {ssrc} gap frame {j}: {e}");
                        }
                    }
                }
            }

            // FEC for the last missing frame
            match decoder.decode_float(&packet.opus_data, &mut self.frame_buffer, true) {
                Ok(n) => {
                    let _ = self.tx.send(DecodedFrame {
                        peer_id: ssrc,
                        seq: curr.wrapping_sub(1),
                        arrival_time,
                        samples: self.frame_buffer[..n].to_vec(),
                        is_concealment: true,
                    });
                    self.stats.fec_recoveries.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    if let Ok(n) = decoder.decode_float(&[], &mut self.frame_buffer, false) {
                        let _ = self.tx.send(DecodedFrame {
                            peer_id: ssrc,
                            seq: curr.wrapping_sub(1),
                            arrival_time,
                            samples: self.frame_buffer[..n].to_vec(),
                            is_concealment: true,
                        });
                        self.stats.plc_frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        } else if ahead > 20 {
            warn!("Peer {ssrc} seq jumped by {ahead} — resetting decoder");
            if let Ok(new_dec) = Decoder::new(OPUS_SAMPLE_RATE, Channels::Mono) {
                *decoder = new_dec;
            } else {
                error!("Failed to reset decoder for peer {ssrc}");
            }
        }

        // ── Decode current packet normally ──────────────────────────
        let t0 = Instant::now();
        match decoder.decode_float(&packet.opus_data, &mut self.frame_buffer, false) {
            Ok(n) => {
                self.stats
                    .last_decode_us
                    .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
                self.stats.record_packet_received(packet.opus_data.len());

                if self
                    .tx
                    .send(DecodedFrame {
                        peer_id: ssrc,
                        seq: curr,
                        arrival_time,
                        samples: self.frame_buffer[..n].to_vec(),
                        is_concealment: false,
                    })
                    .is_err()
                {
                    return;
                }
                self.last_seq.insert(ssrc, curr);
            }
            Err(e) => {
                error!("Decode error for peer {ssrc}: {e}");
                self.stats.decode_errors.fetch_add(1, Ordering::Relaxed);

                if let Ok(n) = decoder.decode_float(&[], &mut self.frame_buffer, false) {
                    let _ = self.tx.send(DecodedFrame {
                        peer_id: ssrc,
                        seq: curr,
                        arrival_time,
                        samples: self.frame_buffer[..n].to_vec(),
                        is_concealment: true,
                    });
                    self.stats.plc_frames.fetch_add(1, Ordering::Relaxed);
                }
                self.last_seq.insert(ssrc, curr);
            }
        }
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Thread handles returned by [`spawn_pool`].
pub struct DecoderPool {
    pub dispatcher: std::thread::JoinHandle<()>,
    pub workers: Vec<std::thread::JoinHandle<()>>,
}

/// Spawn the decoder dispatcher + worker pool.
///
/// - `network_rx`: incoming encoded packets from the network
/// - `decoded_tx`: decoded frames sent to the output / jitter buffer
/// - `stats`: shared audio statistics
/// - `should_stop`: signal to shut down all threads
/// - `removed_peers`: peers marked for decoder cleanup
pub fn spawn_pool(
    network_rx: Receiver<AudioPacket>,
    decoded_tx: Sender<DecodedFrame>,
    stats: Arc<AudioStats>,
    should_stop: Arc<AtomicBool>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,
) -> DecoderPool {
    // Per-worker channels
    let mut worker_txs: Vec<Sender<AudioPacket>> = Vec::with_capacity(DECODER_WORKERS);
    let mut worker_rxs: Vec<Option<Receiver<AudioPacket>>> = Vec::with_capacity(DECODER_WORKERS);

    for _ in 0..DECODER_WORKERS {
        let (tx, rx) = bounded::<AudioPacket>(32);
        worker_txs.push(tx);
        worker_rxs.push(Some(rx));
    }

    // Dispatcher thread (lightweight: just routes by ssrc)
    let dispatcher_txs = worker_txs;
    let dispatcher_stop = should_stop.clone();
    let dispatcher = std::thread::spawn(move || {
        info!("🔊 Decoder dispatcher started ({DECODER_WORKERS} workers)");

        while let Ok(packet) = network_rx.recv() {
            if dispatcher_stop.load(Ordering::Relaxed) {
                break;
            }
            let worker_idx = (packet.header.ssrc as usize) % DECODER_WORKERS;
            // try_send: if a worker is backed up, drop the packet rather
            // than blocking the dispatcher (which would stall ALL peers).
            let _ = dispatcher_txs[worker_idx].try_send(packet);
        }

        info!("🔊 Decoder dispatcher stopped");
    });

    // Worker threads
    let mut workers = Vec::with_capacity(DECODER_WORKERS);
    for (i, slot) in worker_rxs.iter_mut().enumerate() {
        let rx = slot.take().unwrap();
        let tx = decoded_tx.clone();
        let stats = stats.clone();
        let stop = should_stop.clone();
        let removed = removed_peers.clone();
        let handle = std::thread::spawn(move || {
            let mut worker = DecoderWorker::new(i, rx, tx, stats, stop, removed);
            worker.run();
        });
        workers.push(handle);
    }

    DecoderPool {
        dispatcher,
        workers,
    }
}
