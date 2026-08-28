//! Opus encoder thread.
//!
//! Receives 20 ms (960-sample) f32 frames from the input pipeline,
//! encodes them to Opus, and sends `AudioPacket`s toward the network.
//!
//! ## Frame size
//!
//! 20 ms, not 10: half the packet rate and header overhead, and — the
//! part that matters under loss — the LBRR copy Opus puts in packet N+1
//! then covers *all* of packet N, so a typical single-packet burst loss
//! is fully recoverable by in-band FEC. At 10 ms it covered half.
//!
//! ## Silence handling (DTX)
//!
//! The encoder sees *every* frame the pipeline produces — the noise gate
//! zeroes non-speech, and Opus DTX decides what actually goes on the
//! wire. During silence libopus emits ≤2-byte "don't transmit" frames:
//! those are withheld (RTP-style), the sequence number does not advance,
//! but the 48 kHz timestamp keeps counting. The receiver therefore sees
//! contiguous sequence numbers across talk pauses (no spurious PLC) while
//! the timestamps let the jitter buffer tell a pause from network jitter.
//!
//! Owns the Opus encoder exclusively — no shared mutex needed.
//! Adaptive FEC adjusts the packet-loss hint every ~2 seconds from the
//! worse of (a) downlink loss measured over that window and (b) uplink
//! loss the transport reports via `AudioStats::uplink_loss_pct`.

use crossbeam_channel::{Receiver, Sender, TrySendError};
use log::{error, info, warn};
use opus::{Application, Bitrate, Channels, Encoder};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::{
    stats::AudioStats, transport::packet::AudioPacket, PcmFrame, FRAME_SIZE, OPUS_BITRATE,
    OPUS_SAMPLE_RATE,
};

/// Encoder thread state. Created once, runs until the input channel closes.
struct EncoderWorker {
    encoder: Encoder,
    rx: Receiver<PcmFrame>,
    tx: Sender<AudioPacket>,
    stats: Arc<AudioStats>,

    seq: u32,
    ts_48k: u32,
    scratch: Vec<u8>,

    /// Packets encoded since last FEC adaptation check.
    packets_since_fec_update: u32,
    /// Loss-counter snapshots from the previous FEC update, so the hint
    /// tracks the *recent* loss rate instead of the lifetime average.
    prev_received: u64,
    prev_lost: u64,
    /// Hint currently applied to the encoder; avoids a libopus ctl call
    /// every window when nothing changed.
    current_fec_hint: i32,
}

impl EncoderWorker {
    fn new(
        rx: Receiver<PcmFrame>,
        tx: Sender<AudioPacket>,
        stats: Arc<AudioStats>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, Channels::Mono, Application::Voip)?;

        // ── Voice chat tuning ───────────────────────────────────────
        // Every ctl is checked: a silently ignored setting (FEC off,
        // wrong bandwidth) is exactly the kind of thing that costs a
        // week in the field.
        let ctl = |name: &str, r: opus::Result<()>| {
            if let Err(e) = r {
                warn!("Opus encoder: {name} failed: {e}");
            }
        };
        ctl(
            "set_bitrate",
            encoder.set_bitrate(Bitrate::Bits(OPUS_BITRATE)),
        );
        ctl("set_vbr", encoder.set_vbr(true));
        // Complexity 6: for speech at 48 kbps the levels above this mostly
        // buy CELT/music analysis, for roughly a third of the encode CPU —
        // which matters on the low-end hardware this targets.
        // `last_encode_us` is the A/B knob if anyone wants to argue.
        ctl("set_complexity", encoder.set_complexity(7));
        ctl(
            "set_bandwidth",
            encoder.set_bandwidth(opus::Bandwidth::Superwideband),
        );
        ctl("set_inband_fec", encoder.set_inband_fec(true));
        ctl(
            "set_packet_loss_perc",
            encoder.set_packet_loss_perc(FEC_LOSS_FLOOR_PCT),
        );
        ctl("set_dtx", encoder.set_dtx(true));
        stats
            .fec_loss_hint_pct
            .store(FEC_LOSS_FLOOR_PCT.unsigned_abs(), Ordering::Relaxed);

        Ok(Self {
            encoder,
            rx,
            tx,
            stats,
            seq: 0,
            ts_48k: 0,
            // 48 kbps × 20 ms ≈ 120 bytes; LBRR + VBR peaks stay well
            // under 512.
            scratch: vec![0u8; 512],
            packets_since_fec_update: 0,
            prev_received: 0,
            prev_lost: 0,
            current_fec_hint: FEC_LOSS_FLOOR_PCT,
        })
    }

    fn run(&mut self) {
        info!("Encoder thread started");

        while let Ok(samples) = self.rx.recv() {
            self.encode_frame(&samples);
            self.maybe_update_fec();
        }

        info!("Encoder thread stopped");
    }

    fn encode_frame(&mut self, samples: &[f32]) {
        let t0 = Instant::now();

        match self.encoder.encode_float(samples, &mut self.scratch) {
            Ok(size) => {
                self.stats
                    .last_encode_us
                    .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);

                // DTX: Opus produces ≤2-byte "do not transmit" packets
                // during silence. libopus still encodes a full comfort-
                // noise refresh frame every 400 ms (MAX_CONSECUTIVE_DTX),
                // which is > 2 bytes and goes out normally.
                let dtx = size <= 2;
                self.stats.dtx_active.store(dtx, Ordering::Relaxed);

                if dtx {
                    // Withhold the packet. The media clock still advances;
                    // the sequence number only counts transmitted packets,
                    // so the receiver sees no gap when speech resumes.
                    self.ts_48k = self.ts_48k.wrapping_add(FRAME_SIZE as u32);
                    return;
                }

                let packet =
                    AudioPacket::new(self.seq, self.ts_48k, 0, self.scratch[..size].to_vec());

                self.seq = self.seq.wrapping_add(1);
                self.ts_48k = self.ts_48k.wrapping_add(FRAME_SIZE as u32);
                self.stats.record_packet_sent(size);

                // try_send: if the application isn't draining outgoing
                // packets, dropping is correct for real-time audio and
                // guarantees this thread can never wedge in a blocking
                // send (which used to make shutdown hang).
                match self.tx.try_send(packet) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        self.stats.outgoing_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            }
            Err(e) => {
                error!("Encode error: {e}");
                self.stats.encode_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Adjust the Opus FEC packet-loss hint every ~100 frames (~2 seconds).
    ///
    /// Two signals feed it, and the *larger* wins:
    ///
    /// - **Downlink** loss measured by our decoder over the last window.
    ///   Only a proxy for our own stream (it assumes a symmetric path).
    /// - **Uplink** loss reported by the transport via
    ///   [`AudioStats::uplink_loss_pct`] — the loss our listeners actually
    ///   suffer. Field case: a user whose uplink dropped >9% while his
    ///   downlink was clean got a hint of 0 from the proxy alone, which
    ///   makes libopus stop coding LBRR entirely, so every lost frame was
    ///   pure PLC and he "broke up" for everyone else.
    ///
    /// The result never goes below [`FEC_LOSS_FLOOR_PCT`]: at 0 libopus
    /// disables in-band FEC outright, and the first seconds of every loss
    /// episode would then go unprotected until the next window closes.
    fn maybe_update_fec(&mut self) {
        self.packets_since_fec_update += 1;
        if self.packets_since_fec_update < FEC_UPDATE_INTERVAL_FRAMES {
            return;
        }
        self.packets_since_fec_update = 0;

        let downlink = self.downlink_loss_pct_since_last_update();
        // Stored value is already clamped to 0..=100 by the setter;
        // try_from keeps the cast lint-free without an unwrap.
        let uplink = i32::try_from(self.stats.uplink_loss_pct.load(Ordering::Relaxed))
            .unwrap_or(FEC_LOSS_CEIL_PCT);

        let fec_hint = downlink
            .unwrap_or(0)
            .max(uplink)
            .clamp(FEC_LOSS_FLOOR_PCT, FEC_LOSS_CEIL_PCT);

        if fec_hint != self.current_fec_hint {
            match self.encoder.set_packet_loss_perc(fec_hint) {
                Ok(()) => {
                    info!(
                        "Opus FEC hint {}% → {}% (downlink {:?}%, uplink {uplink}%)",
                        self.current_fec_hint, fec_hint, downlink
                    );
                    self.current_fec_hint = fec_hint;
                    self.stats
                        .fec_loss_hint_pct
                        .store(fec_hint.unsigned_abs(), Ordering::Relaxed);
                }
                Err(e) => error!("set_packet_loss_perc({fec_hint}) failed: {e}"),
            }
        }
    }

    /// Downlink loss over the frames since the previous call, as a whole
    /// percentage. `None` when the window held too few packets to mean
    /// anything (nobody else was talking).
    fn downlink_loss_pct_since_last_update(&mut self) -> Option<i32> {
        let received = self.stats.packets_received.load(Ordering::Relaxed);
        let lost = self.stats.packets_lost.load(Ordering::Relaxed);
        let d_received = received.saturating_sub(self.prev_received);
        let d_lost = lost.saturating_sub(self.prev_lost);
        self.prev_received = received;
        self.prev_lost = lost;

        let total = d_received + d_lost;
        if total <= FEC_MIN_WINDOW_PACKETS {
            return None;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        Some(((d_lost as f32 / total as f32) * 100.0).round() as i32)
    }
}

/// Frames between FEC hint re-evaluations (~2 s at 20 ms frames; DTX
/// frames count too, so this is wall-clock, not speech time).
const FEC_UPDATE_INTERVAL_FRAMES: u32 = 100;

/// Below this many packets in a window the downlink loss ratio is noise.
const FEC_MIN_WINDOW_PACKETS: u64 = 50;

/// Lowest hint ever applied. libopus codes no LBRR at all when the hint
/// is 0 (`decide_fec` returns early), so a small floor keeps a redundant
/// copy of every frame on the wire at the cost of a few kbit/s — cheap
/// insurance that covers the onset of a loss episode before the
/// estimators have caught up.
const FEC_LOSS_FLOOR_PCT: i32 = 5;

/// Highest hint applied. Above ~25% libopus stops increasing redundancy
/// anyway; 20 keeps the bitrate trade-off sane on a merely bad link.
const FEC_LOSS_CEIL_PCT: i32 = 20;

// ── Public spawn function ───────────────────────────────────────────────

/// Spawn the encoder thread. Returns the join handle.
///
/// The thread runs until the `rx` channel is closed (all senders dropped).
pub fn spawn(
    rx: Receiver<PcmFrame>,
    tx: Sender<AudioPacket>,
    stats: Arc<AudioStats>,
) -> std::thread::JoinHandle<()> {
    crate::platform::spawn_named("voice-encode", move || {
        match EncoderWorker::new(rx, tx, stats) {
            Ok(mut worker) => worker.run(),
            Err(e) => error!("Failed to create Opus encoder: {e}"),
        }
    })
}
