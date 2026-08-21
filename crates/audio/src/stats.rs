//! Audio pipeline diagnostics.
//!
//! Lock-free atomic counters readable from any thread.
//! Tracks everything a TeamSpeak-style client needs for its debug overlay:
//! packet rates, codec stats, bandwidth, latency budgets, and buffer health.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Shared, lock-free audio statistics.
pub struct AudioStats {
    // ── Packet stats ────────────────────────────────────────────────────
    pub packets_sent: AtomicU64,
    pub packets_received: AtomicU64,
    pub packets_lost: AtomicU64,
    /// Packets that arrived after their slot was already played/concealed.
    pub packets_late: AtomicU64,
    /// Outgoing packets dropped because the app wasn't draining them.
    pub outgoing_dropped: AtomicU64,

    // ── Codec stats ─────────────────────────────────────────────────────
    pub encode_errors: AtomicU64,
    pub decode_errors: AtomicU64,
    pub fec_recoveries: AtomicU64,
    pub plc_frames: AtomicU64,

    // ── Buffer health ───────────────────────────────────────────────────
    pub input_overruns: AtomicU64,
    pub output_underruns: AtomicU64,
    /// Measured capture-device clock skew vs nominal, signed ppm.
    /// 0 until the estimator's warmup (~30 s of capture) completes.
    /// A sustained large value here is the "why does everyone hear me
    /// breaking up" diagnostic — see [`crate::clock`].
    pub capture_clock_skew_ppm: AtomicI64,

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
    /// Last decode duration in microseconds.
    pub last_decode_us: AtomicU64,

    // ── Levels ──────────────────────────────────────────────────────────
    /// Output (playback) RMS level, stored as f32 bits (0.0–1.0).
    pub output_level: AtomicU32,
    /// Post-denoise input level, stored as f32 bits (0.0–1.0).
    /// This is the signal level after noise suppression but before AGC/gain —
    /// use this for the VAD threshold visualization, not the raw capture level.
    pub denoised_level: AtomicU32,
    /// Post-pipeline input level, stored as f32 bits (0.0–1.0).
    /// This is what the VAD and encoder actually see — after denoise, AGC,
    /// gain, and clamp. Useful for debugging "why isn't VAD triggering" issues.
    pub pipeline_level: AtomicU32,

    // ── Pipeline state ──────────────────────────────────────────────────
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
            input_overruns: AtomicU64::new(0),
            output_underruns: AtomicU64::new(0),
            capture_clock_skew_ppm: AtomicI64::new(0),
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

    /// Estimated send bandwidth in kbps.
    ///
    /// Based on average packet size + 12 bytes UDP/RTP-like header overhead,
    /// at 100 packets/sec (10ms frames).
    pub fn estimated_send_kbps(&self) -> f32 {
        let avg = f32::from_bits(self.avg_packet_size.load(Ordering::Relaxed));
        if avg < 0.01 {
            return 0.0;
        }
        // (payload + header_overhead) * packets_per_sec * 8 bits / 1000
        let packets_per_sec = 100.0; // 10ms frames
        let header_overhead = 12.0; // minimal RTP-like header
        (avg + header_overhead) * packets_per_sec * 8.0 / 1000.0
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
             FEC {} PLC {} | Overruns {} Underruns {} | Jitter {}ms (target {}ms) | \
             BW ↑{:.1}kbps ↓{:.1}kbps | Pkt {}B (avg {:.0}B) | \
             Enc {}µs Dec {}µs | Errors: enc {} dec {}",
            elapsed,
            sent,
            received,
            lost,
            loss_rate,
            self.fec_recoveries.load(Ordering::Relaxed),
            self.plc_frames.load(Ordering::Relaxed),
            self.input_overruns.load(Ordering::Relaxed),
            self.output_underruns.load(Ordering::Relaxed),
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
