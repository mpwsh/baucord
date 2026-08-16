//! Opus decoder thread with per-peer packet reordering.
//!
//! A single worker thread owns one stateful Opus decoder per peer.
//! Decoding a 10 ms mono Opus frame costs tens of microseconds, so one
//! thread comfortably handles dozens of simultaneous streams; the earlier
//! garbling under load came from decoder state being shared across peers,
//! not from decode throughput, and per-peer decoders fix it without a
//! dispatcher and worker pool.
//!
//! ## Reordering
//!
//! UDP reorders packets routinely — especially at 100 packets/sec — and a
//! swap must not be treated as loss. When packet N+2 arrives before N+1,
//! it is *held* (not concealed) for a short window:
//!
//! - If N+1 arrives next (the common case), both are decoded in order and
//!   nothing was concealed. Zero added latency on in-order streams.
//! - If more packets pile up behind the gap, or the held packet goes
//!   stale (~30 ms), the missing frame is declared lost and recovered
//!   with PLC/FEC, exactly as before.
//!
//! Concealment frames are emitted with interpolated timestamps so the
//! jitter buffer's estimator stays clean.

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use log::{error, info, warn};
use opus::{Channels, Decoder};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    stats::AudioStats,
    transport::{jitter_buffer::DecodedFrame, packet::AudioPacket},
    FRAME_SIZE, OPUS_SAMPLE_RATE,
};

/// A packet this far behind the playhead is a true late arrival
/// (its slot has already been emitted or concealed) and is dropped.
const LATE_WINDOW: u32 = 10;

/// Gaps wider than this reset the peer's decoder instead of concealing.
const MAX_GAP: u32 = 20;

/// Maximum out-of-order packets held per peer while waiting for a gap
/// to fill. Once exceeded, the earliest gap is concealed. At 10 ms
/// frames this bounds reorder-induced delay to ~30 ms during loss and
/// adds zero delay on in-order streams.
const MAX_REORDER_HOLD: usize = 3;

/// A held packet older than this is flushed via concealment even if no
/// newer packets arrive (covers a reorder right before a DTX pause).
const REORDER_STALE: Duration = Duration::from_millis(30);

// ── Per-peer state ──────────────────────────────────────────────────────

struct PeerState {
    decoder: Decoder,
    /// Sequence number of the last emitted frame.
    last_seq: u32,
    started: bool,
    /// Out-of-order packets waiting for the gap before them to fill.
    /// Tiny (≤ MAX_REORDER_HOLD entries), so a Vec beats wrap-hostile
    /// ordered maps.
    pending: Vec<(AudioPacket, Instant)>,
}

impl PeerState {
    fn new() -> Option<Self> {
        match Decoder::new(OPUS_SAMPLE_RATE, Channels::Mono) {
            Ok(d) => Some(Self {
                decoder: d,
                last_seq: 0,
                started: false,
                pending: Vec::new(),
            }),
            Err(e) => {
                error!("Failed to create Opus decoder: {e}");
                None
            }
        }
    }
}

// ── Decoder thread ──────────────────────────────────────────────────────

struct DecoderThread {
    rx: Receiver<AudioPacket>,
    tx: Sender<DecodedFrame>,
    stats: Arc<AudioStats>,
    should_stop: Arc<AtomicBool>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,

    peers: HashMap<u64, PeerState>,
    frame_buffer: Vec<f32>,
}

impl DecoderThread {
    fn run(&mut self) {
        info!("Decoder thread started");

        while !self.should_stop.load(Ordering::Relaxed) {
            match self.rx.recv_timeout(Duration::from_millis(10)) {
                Ok(packet) => {
                    // Stamp arrival NOW — before any decode work. This is
                    // the timing point the jitter estimator relies on.
                    let arrival = Instant::now();
                    self.handle_packet(packet, arrival);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            self.flush_stale();
            self.sweep_removed();
        }

        info!("Decoder thread stopped");
    }

    fn handle_packet(&mut self, packet: AudioPacket, arrival: Instant) {
        let ssrc = packet.header.ssrc;

        // ── Removed peer? Drop state and the packet. ────────────────
        if let Ok(mut removed) = self.removed_peers.lock() {
            if removed.remove(&ssrc) {
                self.peers.remove(&ssrc);
                return;
            }
        }

        if !self.peers.contains_key(&ssrc) {
            match PeerState::new() {
                Some(state) => {
                    info!("Adding decoder for peer {ssrc}");
                    self.peers.insert(ssrc, state);
                }
                // Opus init failure is near-impossible; skip this packet
                // and retry on the next one rather than panicking on an
                // audio-adjacent thread.
                None => return,
            }
        }
        let peer = self.peers.get_mut(&ssrc).expect("peer just inserted");

        let seq = packet.header.seq;

        if !peer.started {
            peer.started = true;
            peer.last_seq = seq;
            decode_and_emit(
                peer,
                &packet,
                false,
                false,
                arrival,
                &mut self.frame_buffer,
                &self.tx,
                &self.stats,
            );
            return;
        }

        let expected = peer.last_seq.wrapping_add(1);
        let behind = expected.wrapping_sub(seq);
        let ahead = seq.wrapping_sub(expected);

        // Late/duplicate: this slot has already been emitted or concealed.
        if behind >= 1 && behind <= LATE_WINDOW + 1 {
            self.stats.packets_late.fetch_add(1, Ordering::Relaxed);
            return;
        }

        if ahead == 0 {
            // In order — the common case.
            peer.last_seq = seq;
            decode_and_emit(
                peer,
                &packet,
                false,
                false,
                arrival,
                &mut self.frame_buffer,
                &self.tx,
                &self.stats,
            );
            flush_consecutive(peer, &mut self.frame_buffer, &self.tx, &self.stats);
        } else if ahead <= MAX_GAP {
            // Ahead of the playhead: hold it and wait briefly for the
            // missing packet(s) — this is reordering until proven loss.
            if !peer.pending.iter().any(|(p, _)| p.header.seq == seq) {
                peer.pending.push((packet, arrival));
            }
            while peer.pending.len() > MAX_REORDER_HOLD {
                conceal_to_earliest_pending(peer, &mut self.frame_buffer, &self.tx, &self.stats);
                flush_consecutive(peer, &mut self.frame_buffer, &self.tx, &self.stats);
            }
        } else {
            // Huge jump — stream restarted or long outage. Reset.
            warn!("Peer {ssrc} seq jumped by {ahead} — resetting decoder");
            match Decoder::new(OPUS_SAMPLE_RATE, Channels::Mono) {
                Ok(d) => peer.decoder = d,
                Err(e) => error!("Failed to reset decoder for peer {ssrc}: {e}"),
            }
            peer.pending.clear();
            peer.last_seq = seq;
            decode_and_emit(
                peer,
                &packet,
                false,
                false,
                arrival,
                &mut self.frame_buffer,
                &self.tx,
                &self.stats,
            );
        }
    }

    /// Flush held packets whose gap will clearly never fill (e.g. a
    /// reorder right before the sender went silent via DTX).
    fn flush_stale(&mut self) {
        let now = Instant::now();
        for peer in self.peers.values_mut() {
            while peer
                .pending
                .iter()
                .any(|(_, held)| now.duration_since(*held) > REORDER_STALE)
            {
                conceal_to_earliest_pending(peer, &mut self.frame_buffer, &self.tx, &self.stats);
                flush_consecutive(peer, &mut self.frame_buffer, &self.tx, &self.stats);
            }
        }
    }

    /// Drop decoder state for peers removed via the API, even if no
    /// further packet from them ever arrives.
    fn sweep_removed(&mut self) {
        let Ok(mut removed) = self.removed_peers.lock() else {
            return;
        };
        if removed.is_empty() {
            return;
        }
        removed.retain(|ssrc| self.peers.remove(ssrc).is_none());
    }
}

// ── Decode helpers (free functions to keep peer/self borrows disjoint) ──

/// Decode `packet` and emit the frame. `fec` requests in-band FEC decode
/// (recovers the *previous* frame from this packet); `concealment` marks
/// the emitted frame as synthetic for the jitter buffer.
#[allow(clippy::too_many_arguments)]
fn decode_and_emit(
    peer: &mut PeerState,
    packet: &AudioPacket,
    fec: bool,
    concealment: bool,
    arrival: Instant,
    buf: &mut Vec<f32>,
    tx: &Sender<DecodedFrame>,
    stats: &AudioStats,
) {
    let t0 = Instant::now();
    match peer.decoder.decode_float(&packet.opus_data, buf, fec) {
        Ok(n) => {
            if !concealment {
                stats
                    .last_decode_us
                    .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
                stats.record_packet_received(packet.opus_data.len());
            }
            emit(
                packet.header.ssrc,
                packet.header.seq,
                packet.header.timestamp,
                arrival,
                &buf[..n],
                concealment,
                tx,
            );
        }
        Err(e) => {
            error!("Decode error for peer {}: {e}", packet.header.ssrc);
            stats.decode_errors.fetch_add(1, Ordering::Relaxed);

            // Corrupt payload — substitute PLC so the timeline stays intact.
            if let Ok(n) = peer.decoder.decode_float(&[], buf, false) {
                stats.plc_frames.fetch_add(1, Ordering::Relaxed);
                emit(
                    packet.header.ssrc,
                    packet.header.seq,
                    packet.header.timestamp,
                    arrival,
                    &buf[..n],
                    true,
                    tx,
                );
            }
        }
    }
}

fn emit(
    peer_id: u64,
    seq: u32,
    timestamp: u32,
    arrival_time: Instant,
    samples: &[f32],
    is_concealment: bool,
    tx: &Sender<DecodedFrame>,
) {
    // try_send: if the output side is backed up, dropping a frame is the
    // right real-time behavior — and it means shutdown can never block
    // this thread inside a send.
    let _ = tx.try_send(DecodedFrame {
        peer_id,
        seq,
        timestamp,
        arrival_time,
        samples: samples.to_vec(),
        is_concealment,
    });
}

/// Give up on the gap in front of the earliest held packet: conceal the
/// missing frames (PLC for all but the last, FEC from the held packet for
/// the last), then decode the held packet itself and advance the playhead.
fn conceal_to_earliest_pending(
    peer: &mut PeerState,
    buf: &mut Vec<f32>,
    tx: &Sender<DecodedFrame>,
    stats: &Arc<AudioStats>,
) {
    let expected = peer.last_seq.wrapping_add(1);

    // Earliest pending = smallest wrapping distance from `expected`.
    let Some(idx) = peer
        .pending
        .iter()
        .enumerate()
        .min_by_key(|(_, (p, _))| p.header.seq.wrapping_sub(expected))
        .map(|(i, _)| i)
    else {
        return;
    };
    let (packet, arrival) = peer.pending.swap_remove(idx);

    let gap = packet.header.seq.wrapping_sub(expected); // ≥ 1 missing frames
    stats
        .packets_lost
        .fetch_add(u64::from(gap), Ordering::Relaxed);

    // PLC for all missing frames except the one right before the held
    // packet — that one can often be recovered exactly from in-band FEC.
    for j in 0..gap.saturating_sub(1) {
        match peer.decoder.decode_float(&[], buf, false) {
            Ok(n) => {
                stats.plc_frames.fetch_add(1, Ordering::Relaxed);
                let seq = expected.wrapping_add(j);
                let ts = packet
                    .header
                    .timestamp
                    .wrapping_sub((gap - j) * FRAME_SIZE as u32);
                emit(packet.header.ssrc, seq, ts, arrival, &buf[..n], true, tx);
            }
            Err(e) => warn!("PLC decode failed for peer {}: {e}", packet.header.ssrc),
        }
    }

    // FEC recovery of the frame immediately before the held packet.
    let fec_seq = packet.header.seq.wrapping_sub(1);
    let fec_ts = packet.header.timestamp.wrapping_sub(FRAME_SIZE as u32);
    match peer.decoder.decode_float(&packet.opus_data, buf, true) {
        Ok(n) => {
            stats.fec_recoveries.fetch_add(1, Ordering::Relaxed);
            emit(
                packet.header.ssrc,
                fec_seq,
                fec_ts,
                arrival,
                &buf[..n],
                true,
                tx,
            );
        }
        Err(_) => {
            if let Ok(n) = peer.decoder.decode_float(&[], buf, false) {
                stats.plc_frames.fetch_add(1, Ordering::Relaxed);
                emit(
                    packet.header.ssrc,
                    fec_seq,
                    fec_ts,
                    arrival,
                    &buf[..n],
                    true,
                    tx,
                );
            }
        }
    }

    // Finally the held packet itself, in order.
    peer.last_seq = packet.header.seq;
    decode_and_emit(peer, &packet, false, false, arrival, buf, tx, stats);
}

/// Decode any held packets that are now consecutive with the playhead.
fn flush_consecutive(
    peer: &mut PeerState,
    buf: &mut Vec<f32>,
    tx: &Sender<DecodedFrame>,
    stats: &Arc<AudioStats>,
) {
    loop {
        let next = peer.last_seq.wrapping_add(1);
        let Some(idx) = peer.pending.iter().position(|(p, _)| p.header.seq == next) else {
            return;
        };
        let (packet, arrival) = peer.pending.swap_remove(idx);
        peer.last_seq = packet.header.seq;
        decode_and_emit(peer, &packet, false, false, arrival, buf, tx, stats);
    }
}

// ── Public spawn function ───────────────────────────────────────────────

/// Spawn the decoder thread. Returns the join handle.
///
/// - `network_rx`: incoming encoded packets from the network
/// - `decoded_tx`: decoded frames sent to the output / jitter buffer
/// - `stats`: shared audio statistics
/// - `should_stop`: signal to shut down (checked every ≤10 ms, so shutdown
///   cannot hang even while the application still holds sender clones)
/// - `removed_peers`: peers marked for decoder cleanup
pub fn spawn(
    network_rx: Receiver<AudioPacket>,
    decoded_tx: Sender<DecodedFrame>,
    stats: Arc<AudioStats>,
    should_stop: Arc<AtomicBool>,
    removed_peers: Arc<Mutex<HashSet<u64>>>,
) -> std::thread::JoinHandle<()> {
    crate::platform::spawn_named("voice-decode", move || {
        let mut thread = DecoderThread {
            rx: network_rx,
            tx: decoded_tx,
            stats,
            should_stop,
            removed_peers,
            peers: HashMap::new(),
            frame_buffer: vec![0.0f32; FRAME_SIZE * 2],
        };
        thread.run();
    })
}
