//! Audio pipeline diagnostics.
//!
//! Lock-free atomic counters readable from any thread.
//! Tracks everything a TeamSpeak-style client needs for its debug overlay:
//! packet rates, codec stats, bandwidth, latency budgets, and buffer health.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Shared, lock-free audio statistics.
pub struct AudioStats {
    // ── Packet stats ────────────────────────────────────────────────────
    pub packets_sent: AtomicU64,
    /// Packets handed to the engine by the application (counted at intake).
    pub packets_received: AtomicU64,
    /// Playout slots that never got their packet: concealed (FEC/PLC) or
    /// skipped by a playhead jump after a long outage.
    pub packets_lost: AtomicU64,
    /// Packets that arrived after their slot was already played/concealed.
    pub packets_late: AtomicU64,
    /// Frames dropped on the way out: capture → encoder, or encoder →
    /// network, because the consumer wasn't draining.
    pub outgoing_dropped: AtomicU64,

    // ── Codec stats ─────────────────────────────────────────────────────
    pub encode_errors: AtomicU64,
    /// Kept for the debug overlay; a corrupt packet is now concealed
    /// rather than reported, so this stays at 0.
    pub decode_errors: AtomicU64,
    /// Slots recovered from in-band FEC that was actually present in the
    /// following packet. (Earlier releases counted every *attempt*.)
    pub fec_recoveries: AtomicU64,
    /// Slots filled by Opus PLC, including dry-buffer extrapolation.
    pub plc_frames: AtomicU64,

    // ── Uplink loss feedback (written by the app, read by the encoder) ──
    /// Loss on *our outbound* path as a percentage (0–100), supplied by
    /// the transport (renet's ack-derived estimate). The decoder-side
    /// counters above only see the downlink; a user whose uplink drops
    /// packets while the downlink is clean needs this to get any FEC at
    /// all. See [`crate::VoiceChat::set_uplink_loss_pct`].
    pub uplink_loss_pct: AtomicU32,
    /// The packet-loss hint most recently applied to the Opus encoder
    /// (0–100). Diagnostic: lets the overlay show whether FEC is armed.
    pub fec_loss_hint_pct: AtomicU32,

    // ── Buffer health ───────────────────────────────────────────────────
    /// Capture glitches: engine-reported discontinuities (audio the OS
    /// dropped before we saw it) plus ring overflows on our side.
    pub input_overruns: AtomicU64,
    pub output_underruns: AtomicU64,

    // ── Device health (see `VoiceChat::service`) ────────────────────────
    /// The capture stream hit an unrecoverable error (device removed,
    /// invalidated, repeated engine failures). Cleared by a restart.
    pub input_stream_failed: AtomicBool,
    /// Same for playback.
    pub output_stream_failed: AtomicBool,
    /// The OS default capture device changed since we opened ours.
    pub default_input_changed: AtomicBool,
    /// The OS default playback device changed since we opened ours.
    pub default_output_changed: AtomicBool,
    /// Automatic stream restarts performed so far (both sides).
    pub stream_restarts: AtomicU64,

    // ── Link feedback in / adaptation state out ─────────────────────
    /// Round-trip time the transport reports (0 = unknown). See
    /// [`crate::VoiceChat::set_link_rtt_ms`].
    pub link_rtt_ms: AtomicU32,
    /// Opus target bitrate currently in force (bits/s).
    pub send_bitrate_bps: AtomicU32,
    /// Opus frame length per packet currently in force (20 or 40).
    pub frame_ms: AtomicU32,
    /// Rung of the adaptation ladder (0 = best quality).
    pub adapt_level: AtomicU32,
    /// Redundant audio (RED) is being sent.
    pub red_active: AtomicBool,
    /// Packets recovered at the receiver from a RED copy in a later
    /// packet (the original never arrived or arrived late).
    pub red_recoveries: AtomicU64,

    // ── Jitter buffer ───────────────────────────────────────────────────
    pub jitter_buffer_occupancy_ms: AtomicUsize,
    /// Current adaptive target depth in milliseconds.
    pub jitter_buffer_target_ms: AtomicUsize,

    // ── Bandwidth tracking ──────────────────────────────────────────────
    /// Total bytes of Opus data sent (excluding headers).
    pub bytes_sent: AtomicU64,
    /// Total bytes of Opus data received (excluding headers).
    pub bytes_received: AtomicU64,
    /// Most recent encoded packet size in bytes.
    pub last_packet_size: AtomicUsize,
    /// Smoothed average packet size (EMA, stored as f32 bits).
    pub avg_packet_size: AtomicU32,

    // ── Codec timing (microseconds) ─────────────────────────────────────
    /// Last encode duration in microseconds.
    pub last_encode_us: AtomicU64,
    /// Wall time of the output worker's last refill pass — decoding every
    /// peer's due slot, mixing and limiting — in microseconds. This is the
    /// number to watch when many people talk at once.
    pub last_decode_us: AtomicU64,

    // ── Levels ──────────────────────────────────────────────────────────
    /// Output (playback) RMS level, stored as f32 bits (0.0–1.0).
    pub output_level: AtomicU32,
    /// Post-denoise input level, stored as f32 bits (0.0–1.0).
    /// This is the signal level after noise suppression but before the
    /// gate — use this for the gate threshold visualization, not the raw
    /// capture level.
    pub denoised_level: AtomicU32,
    /// Post-pipeline input level, stored as f32 bits (0.0–1.0).
    /// This is what the encoder actually sees — after denoise, gate, AGC,
    /// compressor, gain, and clamp.
    pub pipeline_level: AtomicU32,

    // ── Pipeline state ──────────────────────────────────────────────────
    /// Speaking indicator: RNNoise speech probability + open gate, with
    /// hold (see `processor::speaking`).
    pub vad_active: AtomicBool,
    pub denoise_active: AtomicBool,
    pub dtx_active: AtomicBool,
    /// Whether the noise gate is currently open (passing audio).
    pub gate_active: AtomicBool,

    // ── Timing ──────────────────────────────────────────────────────────
    pub start_time: Instant,
}
impl Default for AudioStats {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioStats {
    pub fn new() -> Self {
        Self {
            packets_sent: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            packets_lost: AtomicU64::new(0),
            packets_late: AtomicU64::new(0),
            outgoing_dropped: AtomicU64::new(0),
            encode_errors: AtomicU64::new(0),
            decode_errors: AtomicU64::new(0),
            fec_recoveries: AtomicU64::new(0),
            plc_frames: AtomicU64::new(0),
            uplink_loss_pct: AtomicU32::new(0),
            fec_loss_hint_pct: AtomicU32::new(0),
            input_overruns: AtomicU64::new(0),
            output_underruns: AtomicU64::new(0),
            input_stream_failed: AtomicBool::new(false),
            output_stream_failed: AtomicBool::new(false),
            default_input_changed: AtomicBool::new(false),
            default_output_changed: AtomicBool::new(false),
            stream_restarts: AtomicU64::new(0),
            link_rtt_ms: AtomicU32::new(0),
            send_bitrate_bps: AtomicU32::new(0),
            frame_ms: AtomicU32::new(20),
            adapt_level: AtomicU32::new(0),
            red_active: AtomicBool::new(false),
            red_recoveries: AtomicU64::new(0),
            jitter_buffer_occupancy_ms: AtomicUsize::new(0),
            jitter_buffer_target_ms: AtomicUsize::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            last_packet_size: AtomicUsize::new(0),
            avg_packet_size: AtomicU32::new(0.0_f32.to_bits()),
            last_encode_us: AtomicU64::new(0),
            last_decode_us: AtomicU64::new(0),
            output_level: AtomicU32::new(0.0_f32.to_bits()),
            denoised_level: AtomicU32::new(0.0_f32.to_bits()),
            pipeline_level: AtomicU32::new(0.0_f32.to_bits()),
            vad_active: AtomicBool::new(false),
            denoise_active: AtomicBool::new(true),
            dtx_active: AtomicBool::new(false),
            gate_active: AtomicBool::new(false),
            start_time: Instant::now(),
        }
    }

    /// Record an encoded packet's size and update the running average.
    pub fn record_packet_sent(&self, opus_bytes: usize) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent
            .fetch_add(opus_bytes as u64, Ordering::Relaxed);
        self.last_packet_size.store(opus_bytes, Ordering::Relaxed);

        // EMA: avg = 0.9 * avg + 0.1 * new
        let prev = f32::from_bits(self.avg_packet_size.load(Ordering::Relaxed));
        let new_avg = if prev < 0.01 {
            opus_bytes as f32
        } else {
            prev * 0.9 + opus_bytes as f32 * 0.1
        };
        self.avg_packet_size
            .store(new_avg.to_bits(), Ordering::Relaxed);
    }

    /// Record a received packet's size.
    pub fn record_packet_received(&self, opus_bytes: usize) {
        self.packets_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received
            .fetch_add(opus_bytes as u64, Ordering::Relaxed);
    }

    /// Estimated send bandwidth in kbps, from what was actually sent:
    /// Opus bytes plus per-packet overhead (16-byte `PacketHeader` +
    /// 28 bytes IPv4/UDP) over the session. DTX pauses therefore lower it,
    /// as they do on the wire — the old estimate assumed 100 packets/s of
    /// average size regardless.
    pub fn estimated_send_kbps(&self) -> f32 {
        let elapsed = self.start_time.elapsed().as_secs_f32();
        if elapsed < 0.5 {
            return 0.0;
        }
        const OVERHEAD_BYTES: f32 = 16.0 + 28.0;
        #[allow(clippy::cast_precision_loss)]
        let bytes = self.bytes_sent.load(Ordering::Relaxed) as f32
            + self.packets_sent.load(Ordering::Relaxed) as f32 * OVERHEAD_BYTES;
        bytes * 8.0 / elapsed / 1000.0
    }

    /// Estimated receive bandwidth in kbps (based on actual received data rate).
    pub fn estimated_recv_kbps(&self) -> f32 {
        let elapsed = self.start_time.elapsed().as_secs_f32();
        if elapsed < 0.5 {
            return 0.0;
        }
        let bytes = self.bytes_received.load(Ordering::Relaxed) as f32;
        bytes * 8.0 / elapsed / 1000.0
    }

    pub fn get_summary(&self) -> String {
        let elapsed = self.start_time.elapsed().as_secs();
        let sent = self.packets_sent.load(Ordering::Relaxed);
        let received = self.packets_received.load(Ordering::Relaxed);
        let lost = self.packets_lost.load(Ordering::Relaxed);

        let loss_rate = if received + lost > 0 {
            (lost as f32 / (received + lost) as f32) * 100.0
        } else {
            0.0
        };

        format!(
            "Audio ({}s) | Sent {} Recv {} Lost {} ({:.1}%) | \
             FEC {} RED {} PLC {} (hint {}%, uplink {}%) | Rate {}k/{}ms{} | Overruns {} Underruns {} Drop {} | Jitter {}ms (target {}ms) | \
             BW ↑{:.1}kbps ↓{:.1}kbps | Pkt {}B (avg {:.0}B) | \
             Enc {}µs Dec {}µs | Errors: enc {} dec {}",
            elapsed,
            sent,
            received,
            lost,
            loss_rate,
            self.fec_recoveries.load(Ordering::Relaxed),
            self.red_recoveries.load(Ordering::Relaxed),
            self.plc_frames.load(Ordering::Relaxed),
            self.fec_loss_hint_pct.load(Ordering::Relaxed),
            self.uplink_loss_pct.load(Ordering::Relaxed),
            self.send_bitrate_bps.load(Ordering::Relaxed) / 1000,
            self.frame_ms.load(Ordering::Relaxed),
            if self.red_active.load(Ordering::Relaxed) { "+RED" } else { "" },
            self.input_overruns.load(Ordering::Relaxed),
            self.output_underruns.load(Ordering::Relaxed),
            self.outgoing_dropped.load(Ordering::Relaxed),
            self.jitter_buffer_occupancy_ms.load(Ordering::Relaxed),
            self.jitter_buffer_target_ms.load(Ordering::Relaxed),
            self.estimated_send_kbps(),
            self.estimated_recv_kbps(),
            self.last_packet_size.load(Ordering::Relaxed),
            f32::from_bits(self.avg_packet_size.load(Ordering::Relaxed)),
            self.last_encode_us.load(Ordering::Relaxed),
            self.last_decode_us.load(Ordering::Relaxed),
            self.encode_errors.load(Ordering::Relaxed),
            self.decode_errors.load(Ordering::Relaxed),
        )
    }
}
