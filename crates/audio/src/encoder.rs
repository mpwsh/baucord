//! Opus encoder thread.
//!
//! Receives 20 ms (960-sample) f32 frames from the input pipeline,
//! encodes them to Opus, and sends `AudioPacket`s toward the network.
//!
//! ## Frame size
//!
//! 20 ms by default: half the packet rate and header overhead of 10 ms,
//! and the LBRR copy Opus puts in packet N+1 then covers *all* of packet
//! N. Under adaptation the encoder can move to 40 ms frames (two pipeline
//! frames per packet), halving overhead again exactly when the codec rate
//! is low enough for 44 bytes of header per packet to matter.
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
//! ## Loss protection
//!
//! Two mechanisms, both driven by the same 2-second link window:
//!
//! - **In-band FEC (LBRR)**: always on; the loss hint is the worse of
//!   measured downlink loss and transport-reported uplink loss, floored
//!   so libopus never stops coding LBRR entirely.
//! - **RED** (`transport::red`): under random loss the
//!   [`crate::adapt::RateController`] turns on full copies of the
//!   preceding packet(s) in every packet — exact repair of single losses
//!   and, at distance 2, of pairs. The controller pays for it by stepping
//!   the codec down a rung, and never enables it when the loss looks like
//!   congestion.
//!
//! Owns the Opus encoder exclusively — no shared mutex needed.

use std::{
    collections::VecDeque,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use log::{error, info, warn};
use opus::{Application, Bitrate, Channels, Encoder};

use crate::{
    adapt::{Band, Decision, LinkSample, Profile, RateController, LADDER},
    stats::AudioStats,
    transport::{
        packet::{AudioPacket, PayloadType},
        red,
    },
    PcmFrame, FRAME_SIZE, OPUS_SAMPLE_RATE,
};

/// Largest packet the encoder will ever produce: 40 ms at the top rate,
/// with LBRR, plus two RED copies of the same. Generous.
const SCRATCH_BYTES: usize = 1500;

/// Redundant copies per packet when RED is on, decided by the ladder
/// (`Decision::red_blocks`): 1 repairs any single loss; 2 also repairs
/// back-to-back pairs, which a link dropping a third of everything
/// produces constantly (2026-09-03: a client at 40 % upstream loss for
/// 35 minutes). Capped by the RED wire format.
const RED_DISTANCE_MAX: usize = red::MAX_BLOCKS;

/// Encoder thread state. Created once, runs until the input channel closes.
struct EncoderWorker {
    encoder: Encoder,
    rx: Receiver<PcmFrame>,
    tx: Sender<AudioPacket>,
    stats: Arc<AudioStats>,

    seq: u32,
    ts_48k: u32,
    scratch: Vec<u8>,
    /// RED bundle assembly buffer.
    bundle: Vec<u8>,

    // ── Frame accumulation (20 ms in, 20/40 ms out) ─────────────────
    profile: Profile,
    /// Applied at the next packet boundary.
    pending: Option<Decision>,
    accum: Vec<f32>,
    /// Timestamp of the first pipeline frame in `accum`.
    accum_ts: u32,

    // ── RED ─────────────────────────────────────────────────────────
    red: bool,
    /// Last transmitted packets, oldest first: (timestamp, bytes).
    history: VecDeque<(u32, Vec<u8>)>,
    /// Copies to carry per packet while RED is on (1 or 2).
    red_distance: usize,

    // ── Adaptation ──────────────────────────────────────────────────
    controller: RateController,
    /// Wall-clock time of the last adaptation evaluation. Wall clock and
    /// not frame count on purpose: mute stops the frame flow entirely
    /// (input.rs discards while muted), and a frame-counted tick froze
    /// the controller — and everything downstream of it — at whatever
    /// state the last frame left it in. Field case, 2026-08-30 00:16:
    /// twelve minutes muted through a 60 % loss storm at 48 kbps, then
    /// three ticks after unmuting, 12 kbps mid-sentence.
    last_adapt: Instant,
    /// Loss-counter snapshots from the previous update, so the hint
    /// tracks the *recent* loss rate instead of the lifetime average.
    prev_received: u64,
    prev_lost: u64,
    /// Hint currently applied to the encoder; avoids a libopus ctl call
    /// every window when nothing changed.
    current_fec_hint: i32,
}

fn opus_band(b: Band) -> opus::Bandwidth {
    match b {
        Band::Narrow => opus::Bandwidth::Narrowband,
        Band::Wide => opus::Bandwidth::Wideband,
        Band::SuperWide => opus::Bandwidth::Superwideband,
    }
}

fn ctl(name: &str, r: opus::Result<()>) {
    if let Err(e) = r {
        warn!("Opus encoder: {name} failed: {e}");
    }
}

impl EncoderWorker {
    fn new(
        rx: Receiver<PcmFrame>,
        tx: Sender<AudioPacket>,
        stats: Arc<AudioStats>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut encoder = Encoder::new(OPUS_SAMPLE_RATE, Channels::Mono, Application::Voip)?;
        let profile = LADDER[0];

        // ── Voice chat tuning ───────────────────────────────────────
        // Every ctl is checked: a silently ignored setting (FEC off,
        // wrong bandwidth) is exactly the kind of thing that costs a
        // week in the field.
        ctl(
            "set_bitrate",
            encoder.set_bitrate(Bitrate::Bits(profile.bitrate)),
        );
        ctl("set_vbr", encoder.set_vbr(true));
        // Complexity 7 — and not lower. libopus only runs its tonality /
        // activity analysis at complexity >= 7 (opus_encoder.c, float
        // build); below that the DTX decision falls back to SILK's own VAD,
        // which flags a low-level mic floor as "active". Measured 2026-08-28
        // on a quiet, unmuted mic: complexity 6 sent ~35 pkt/s of 30-byte
        // near-silence frames, complexity 7 sent the 2.5/s DTX refreshes
        // and nothing else. 10 buys nothing audible over 7 for speech at
        // this bitrate; `last_encode_us` is the knob if that ever changes.
        ctl("set_complexity", encoder.set_complexity(7));
        ctl(
            "set_bandwidth",
            encoder.set_bandwidth(opus_band(profile.band)),
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
        Self::publish_profile(&stats, profile, 0, false);

        Ok(Self {
            encoder,
            rx,
            tx,
            stats,
            seq: 0,
            ts_48k: 0,
            scratch: vec![0u8; SCRATCH_BYTES],
            bundle: Vec::with_capacity(SCRATCH_BYTES),
            profile,
            pending: None,
            accum: Vec::with_capacity(FRAME_SIZE * 2),
            accum_ts: 0,
            red: false,
            history: VecDeque::with_capacity(red::MAX_BLOCKS + 1),
            red_distance: 1,
            controller: RateController::new(),
            last_adapt: Instant::now(),
            prev_received: 0,
            prev_lost: 0,
            current_fec_hint: FEC_LOSS_FLOOR_PCT,
        })
    }

    fn publish_profile(stats: &AudioStats, profile: Profile, level: usize, red: bool) {
        #[allow(clippy::cast_sign_loss)] // bitrates are positive
        stats
            .send_bitrate_bps
            .store(profile.bitrate as u32, Ordering::Relaxed);
        stats.frame_ms.store(profile.frame_ms, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)] // ladder has 5 rungs
        stats.adapt_level.store(level as u32, Ordering::Relaxed);
        stats.red_active.store(red, Ordering::Relaxed);
    }

    fn run(&mut self) {
        // Audio-class scheduling, same as the capture/playback workers. A
        // CPU-heavy game can otherwise starve this thread; the 4-frame
        // channel feeding it fills within 80 ms and the sender's own voice
        // breaks up for everyone else (counted in `outgoing_dropped`).
        #[allow(clippy::cast_possible_truncation)]
        let _rt = crate::platform::promote_current_thread(FRAME_SIZE as u32, OPUS_SAMPLE_RATE);
        info!("Encoder thread started");

        loop {
            // Bounded wait instead of a blocking recv: while muted the
            // input pipeline sends nothing, and the adaptation tick must
            // keep running so the rate controller can keep evaluating —
            // and recovering — with no frames flowing. Out of every
            // voice room there is no link to evaluate: the wait is long
            // (a frame still wakes it at once), where it was four
            // wakeups a second in a phone's lobby for as long as the
            // app ran (review P3-23).
            let tick = if self.stats.in_room.load(Ordering::Relaxed) {
                RECV_TICK
            } else {
                LOBBY_TICK
            };
            match self.rx.recv_timeout(tick) {
                Ok(samples) => self.push_frame(&samples),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.maybe_adapt();
        }

        info!("Encoder thread stopped");
    }

    /// Samples per packet at the current profile.
    fn packet_samples(&self) -> usize {
        FRAME_SIZE * (self.profile.frame_ms / 20) as usize
    }

    fn push_frame(&mut self, samples: &[f32]) {
        if self.accum.is_empty() {
            // Packet boundary: the only place a frame-length change is
            // safe to apply.
            if let Some(d) = self.pending.take() {
                self.apply(d);
            }
            self.accum_ts = self.ts_48k;
        }
        self.accum.extend_from_slice(samples);
        // The media clock counts pipeline frames regardless of packet size
        // or DTX, so timestamps stay continuous across both.
        self.ts_48k = self.ts_48k.wrapping_add(FRAME_SIZE as u32);

        if self.accum.len() >= self.packet_samples() {
            self.encode_packet();
            self.accum.clear();
        }
    }

    fn encode_packet(&mut self) {
        let t0 = Instant::now();
        let packet_ts = self.accum_ts;

        let size = match self.encoder.encode_float(&self.accum, &mut self.scratch) {
            Ok(size) => size,
            Err(e) => {
                error!("Encode error: {e}");
                self.stats.encode_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        self.stats
            .last_encode_us
            .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);

        // DTX: Opus produces ≤2-byte "do not transmit" packets during
        // silence. libopus still encodes a full comfort-noise refresh frame
        // every 400 ms (MAX_CONSECUTIVE_DTX), which is > 2 bytes and goes
        // out normally.
        let dtx = size <= 2;
        if dtx {
            // Withhold the packet. The sequence number only counts
            // transmitted packets, so the receiver sees no gap when speech
            // resumes. A pause also ends any RED run: the next packet has
            // nothing contiguous to carry a copy of (Perkins ch. 9 on the
            // first packet of a talk spurt).
            self.history.clear();
            return;
        }

        let primary = &self.scratch[..size];
        let (payload_type, wire_bytes): (PayloadType, usize) =
            if self.red && !self.history.is_empty() {
                // Copies of the preceding transmitted packets, oldest first,
                // with their media offset from this packet. Contiguity is
                // guaranteed: `history` is cleared on every DTX gap.
                let mut blocks: [(u32, &[u8]); red::MAX_BLOCKS] = [(0, &[]); red::MAX_BLOCKS];
                let mut count = 0;
                for ((ts, data), block) in self.history.iter().zip(&mut blocks) {
                    *block = (packet_ts.wrapping_sub(*ts), data.as_slice());
                    count += 1;
                }
                red::encode(primary, &blocks[..count], &mut self.bundle);
                (PayloadType::Red, self.bundle.len())
            } else {
                (PayloadType::Opus, size)
            };

        let payload = match payload_type {
            PayloadType::Red => self.bundle.clone(),
            PayloadType::Opus => primary.to_vec(),
        };
        let packet = AudioPacket::new_typed(self.seq, packet_ts, 0, payload_type, payload);

        self.seq = self.seq.wrapping_add(1);
        self.stats.record_packet_sent(wire_bytes);

        if self.red {
            // The copy that falls out of the history gives its buffer
            // to the one that enters: no allocation once it is full.
            let keep = self.red_distance.min(RED_DISTANCE_MAX);
            let mut copy = Vec::new();
            while self.history.len() >= keep.max(1) {
                if let Some((_, old)) = self.history.pop_front() {
                    copy = old;
                }
            }
            if keep > 0 {
                copy.clear();
                copy.extend_from_slice(primary);
                self.history.push_back((packet_ts, copy));
            }
        }
        // try_send: if the application isn't draining outgoing packets,
        // dropping is correct for real-time audio and guarantees this
        // thread can never wedge in a blocking send (which used to make
        // shutdown hang).
        match self.tx.try_send(packet) {
            Err(TrySendError::Full(_)) => {
                self.stats.outgoing_dropped.fetch_add(1, Ordering::Relaxed);
            }
            // Disconnected: the receiver is gone (shutdown); nothing to count.
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Apply a controller decision at a packet boundary.
    fn apply(&mut self, d: Decision) {
        if d.profile != self.profile {
            ctl(
                "set_bitrate",
                self.encoder.set_bitrate(Bitrate::Bits(d.profile.bitrate)),
            );
            ctl(
                "set_bandwidth",
                self.encoder.set_bandwidth(opus_band(d.profile.band)),
            );
            self.profile = d.profile;
        }
        if d.red != self.red {
            self.red = d.red;
            if !self.red {
                self.history.clear();
            }
        }
        if d.red_blocks != self.red_distance {
            info!(
                "Rate control: RED depth {} → {} copies per packet",
                self.red_distance, d.red_blocks
            );
            self.red_distance = d.red_blocks.clamp(1, RED_DISTANCE_MAX);
        }
        Self::publish_profile(&self.stats, self.profile, d.level, self.red);
    }

    /// Every ~2 s: refresh the FEC hint and run the rate controller.
    ///
    /// Two signals feed the hint, and the *larger* wins:
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
    fn maybe_adapt(&mut self) {
        if self.last_adapt.elapsed() < ADAPT_INTERVAL {
            return;
        }
        self.last_adapt = Instant::now();

        let downlink = self.downlink_loss_pct_since_last_update();
        // The transport-fed uplink loss carries a freshness timestamp
        // (see `AudioStats::uplink_loss_updated_ms`): a value nobody has
        // refreshed within UPLINK_STALE_MS is *unknown*, not still true.
        // Field case, 2026-08-30 00:17: a 38 % reading latched verbatim
        // for two minutes after the transport stopped reporting.
        let uplink: Option<i32> = {
            let age_ms = self.stats.uplink_report_age_ms();
            // Stored value is already clamped to 0..=100 by the setter;
            // try_from keeps the cast lint-free without an unwrap.
            let last = i32::try_from(self.stats.uplink_loss_pct.load(Ordering::Relaxed))
                .unwrap_or(FEC_LOSS_CEIL_PCT);
            let fresh = age_ms.is_some_and(|age| age <= UPLINK_STALE_MS);
            // The relay reports every 2 s unconditionally, so a gap after a
            // *bad* reading is the loss delaying the reports, not the link
            // healing: hold the hint a while longer instead of switching
            // FEC off in the middle of the trouble (2026-09-03: 68 flaps
            // 20 % → 5 % → 20 % in 40 minutes at 40 % upstream loss).
            let held =
                last >= UPLINK_HOLD_MIN_PCT && age_ms.is_some_and(|age| age <= UPLINK_HOLD_MS);
            (fresh || held).then_some(last)
        };

        let fec_hint = downlink
            .unwrap_or(0)
            .max(uplink.unwrap_or(0))
            .clamp(FEC_LOSS_FLOOR_PCT, FEC_LOSS_CEIL_PCT);

        if fec_hint != self.current_fec_hint {
            match self.encoder.set_packet_loss_perc(fec_hint) {
                Ok(()) => {
                    info!(
                        "Opus FEC hint {}% → {}% (downlink {:?}%, uplink {})",
                        self.current_fec_hint,
                        fec_hint,
                        downlink,
                        fmt_uplink(uplink),
                    );
                    self.current_fec_hint = fec_hint;
                    self.stats
                        .fec_loss_hint_pct
                        .store(fec_hint.unsigned_abs(), Ordering::Relaxed);
                }
                Err(e) => error!("set_packet_loss_perc({fec_hint}) failed: {e}"),
            }
        }

        // ── Rate control ────────────────────────────────────────────
        let rtt = self.stats.link_rtt_ms.load(Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)] // small percentages / ms
        let sample = LinkSample {
            uplink_loss_pct: uplink.map(|u| u.max(0) as f32),
            rtt_ms: (rtt > 0).then_some(rtt as f32),
        };
        let decision = self.controller.evaluate(sample);
        if decision.changed {
            info!(
                "Rate control: {} kbps {:?} {} ms frames, RED {} (uplink {}, smoothed {:.0}%, rtt {rtt} ms)",
                decision.profile.bitrate / 1000,
                decision.profile.band,
                decision.profile.frame_ms,
                if decision.red { "on" } else { "off" },
                fmt_uplink(uplink),
                self.controller.smoothed_loss_pct(),
            );
            // Frame-length changes wait for a packet boundary; everything
            // else could apply now but is simpler to apply together.
            self.pending = Some(decision);
        }
        // A packet boundary is also "no packet in progress": while muted
        // or silent the accumulator is empty, and applying here keeps the
        // encoder ctls and published stats truthful during the gap
        // instead of deferring to the first frame after unmute.
        if self.accum.is_empty()
            && let Some(d) = self.pending.take()
        {
            self.apply(d);
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

/// Wall-clock interval between adaptation evaluations. The rate
/// controller's dwell and climb holds (adapt.rs) are counted in
/// evaluations and sized against this cadence and the relay's 2 s
/// receiver-report window — change one, revisit the others.
const ADAPT_INTERVAL: Duration = Duration::from_secs(2);

/// Bounded wait on the frame channel, so the adaptation tick runs even
/// when the input pipeline is silent (muted). Frames normally arrive
/// every 20 ms; this only bounds the idle case.
const RECV_TICK: Duration = Duration::from_millis(250);

/// The same wait out of every voice room: only how soon the thread
/// notices it is in one again without a frame to tell it (joined
/// muted).
const LOBBY_TICK: Duration = Duration::from_secs(5);

/// An uplink-loss reading older than this is treated as unknown rather
/// than current (`LinkSample.uplink_loss_pct = None`): the smoothed
/// estimate then decays instead of acting on a latched value. Sized to
/// a few transport sampling periods (~2 s each).
const UPLINK_STALE_MS: u64 = 10_000;

/// A stale reading at or above this loss keeps steering the FEC hint
/// for up to [`UPLINK_HOLD_MS`] — silence from the relay under heavy
/// loss is the loss, not recovery.
const UPLINK_HOLD_MIN_PCT: i32 = 10;
const UPLINK_HOLD_MS: u64 = 45_000;

/// Display helper: the uplink reading, or "stale" when there is no
/// fresh one, so incident logs distinguish "0 % loss" from "no data".
fn fmt_uplink(uplink: Option<i32>) -> String {
    match uplink {
        Some(v) => format!("{v}%"),
        None => "stale".to_owned(),
    }
}

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
