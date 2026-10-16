//! The jitter buffer between the network and a watched stream's decoder.
//!
//! Video reaches a viewer on one ordered QUIC stream per group (a
//! keyframe and the deltas that depend on it), and the transport hands
//! groups over in order per sharer (ARCHITECTURE §4), so frames arrive
//! in sequence order, each at most once. What can happen is that one
//! never arrives: the relay abandoned its group, the screen thread's
//! channel was full, the sharer dropped it. Nothing will repair it, so
//! a missing frame is gone for good the moment a later one is here.
//!
//! The buffer keeps one invariant: **frames go to the decoder in
//! sequence order and never across a gap** — a delta whose reference
//! was lost decodes into smear that stays until the next keyframe.
//!
//! # Playout pacing
//!
//! Every frame carries the sender's capture timestamp. The buffer maps
//! that clock onto the local one by tracking the fastest observed path
//! (smallest `arrival − timestamp`) and releases each frame at
//! `timestamp + fastest_path + playout_delay` — the sender's cadence,
//! not the network's. Without this, a 200 KB keyframe that takes
//! ~200 ms of wire time is followed by a dozen deltas that arrive
//! bunched behind it, and the viewer sees the stream lurch slow-then-fast
//! every keyframe interval. The delay is adaptive: a decaying peak of how
//! late frames have been arriving relative to the fastest path, clamped
//! to [`PLAYOUT_DELAY_MIN`]..=[`PLAYOUT_DELAY_MAX`]. On a clean link it
//! sits at the minimum; a stream with big keyframes settles near their
//! serialization time. Frames that arrive later than their slot are
//! released at once — pacing never adds to lateness.
//!
//! # Gaps
//!
//! When the next frame is missing and a later one is here, the buffer
//! acts at once (review P1-12; it used to wait 200–350 ms for a resend
//! that the ordered streams can no longer deliver):
//!
//! - a buffered **keyframe** after the gap: everything before it is
//!   dropped and delivery goes on from it, at its own playout slot;
//! - no keyframe: the buffer **freezes** — the buffered deltas are
//!   dropped, it goes back to waiting for a keyframe like a stream's
//!   start, and asks the sender for one. For people watching, a short
//!   freeze on the last clean picture reads far better than motion made
//!   of artifacts.
//!
//! A frame numbered at or before the newest one seen is a new share
//! whose numbering started over, and the buffer starts over with it.
//!
//! # Keyframe requests
//!
//! The buffer never talks to the network; it raises a flag when
//! recovery needs the sender. The owner polls
//! [`VideoJitterBuffer::take_keyframe_request`] after draining and
//! relays it (the sender calls `VideoEncoder::force_keyframe`). The
//! flag is rate-limited here so a broken stream cannot spam requests.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// How long the buffer will wait, unprimed, watching undecodable delta
/// frames stream past before asking the sender for a keyframe. Covers
/// the "joined mid-GOP" case.
const UNPRIMED_REQUEST_AFTER: Duration = Duration::from_millis(400);

/// Minimum spacing between keyframe requests. Below the sender's
/// keyframe interval by design: a request, its IDR, and that IDR's loss
/// can all happen within one interval.
const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_millis(700);

/// Bounds for the playout delay (see the module docs). The minimum is
/// about one frame at 60 fps of slack so ordinary arrival jitter never
/// makes a frame late; the maximum caps the latency spent absorbing
/// keyframe bursts (a VBV-sized keyframe at stream bitrate is ~200 ms
/// of wire time, so this covers it at full rate — lowering the encoder's
/// `VBV_MAX_BITS` is the lever if that ever needs to be smaller).
const PLAYOUT_DELAY_MIN: Duration = Duration::from_millis(20);
const PLAYOUT_DELAY_MAX: Duration = Duration::from_millis(200);
/// Headroom added to the lateness peak.
const PLAYOUT_MARGIN_MS: f64 = 5.0;
/// Decay of the lateness peak when quiet, ms per second: a 200 ms
/// keyframe peak fades over ~20 s if keyframes stop being late, and is
/// refreshed every keyframe interval while they aren't.
const PEAK_DECAY_MS_PER_S: f64 = 10.0;
/// The fastest-path offset is the minimum over this many recent
/// buckets of [`OFFSET_BUCKET`] each (a 2 s window). A window, not an
/// all-time minimum: a hardware encoder draining its pipeline can emit
/// several frames at once, and those arrive *early* relative to their
/// capture stamps — an all-time minimum would lock onto that burst and
/// mark every normal frame late until clock drift caught up. A window
/// forgets it in 2 s and tracks drift for free.
const OFFSET_BUCKETS: usize = 4;
const OFFSET_BUCKET: Duration = Duration::from_millis(500);
/// The backlog cap, in seconds of stream at the frame rate the buffer
/// has learned from arrivals. Longer than the sender's keyframe interval
/// (2 s), so a backlog that reaches it holds a keyframe to jump to: a
/// cap fixed at 90 frames (3 s at the 30 fps the buffer is created
/// for) was 1.5 s of a 60 fps stream, and about half the overflows
/// found no keyframe and froze (2026-10-08, MOB-18).
const BACKLOG_CAP_SECS: f32 = 3.0;
/// A frame this far behind live when its turn comes: skip to the
/// newest buffered keyframe instead of decoding a backlog the viewer
/// would watch late — well before the cap, so a device that cannot keep
/// up drops video and stays near its sound.
const CATCH_UP_BEHIND: Duration = Duration::from_secs(1);
/// An implied-offset jump larger than this means the sender's clock
/// restarted (new share): re-anchor instead of holding frames for ages
/// or treating them all as hopelessly late.
const CLOCK_JUMP_MS: f64 = 5_000.0;

/// An encoded video packet waiting in the jitter buffer.
#[derive(Debug)]
pub struct BufferedPacket {
    pub seq: u32,
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub arrival_time: Instant,
    /// Sender capture time, ms on the sender's clock (wraps). Drives
    /// playout pacing.
    pub timestamp_ms: u32,
}

/// Counters for observability. All monotonic except `current_depth`.
#[derive(Debug, Default, Clone)]
pub struct JitterBufferStats {
    /// Packets pushed into the buffer.
    pub packets_received: u64,
    /// Packets passed over for a newer keyframe: the backlog hit its cap,
    /// or the next frame was too far behind live.
    pub packets_dropped_overflow: u64,
    /// Sequence gaps: a frame that never came while later ones did.
    pub gaps_detected: u64,
    /// Packets discarded because a gap broke their reference chain (the
    /// buffer froze, or jumped past them, rather than decode them into
    /// smear).
    pub packets_dropped_broken_reference: u64,
    /// Jumps forward to a buffered keyframe (after a gap, a backlog, or
    /// a frame too far behind live).
    pub keyframe_resets: u64,
    /// Keyframe requests raised toward the sender.
    pub keyframe_requests: u64,
    /// Packets currently buffered.
    pub current_depth: usize,
}

/// Sequence-ordered jitter buffer with keyframe-aware loss recovery.
///
/// See the module docs. Not thread-safe by design — owned and driven
/// entirely by the screen thread.
pub struct VideoJitterBuffer {
    /// Buffered frames, oldest first. Frames arrive in sequence order,
    /// so they are only ever appended; every one is after `next_seq`
    /// or is it.
    pending: VecDeque<BufferedPacket>,
    /// Next sequence number to release; `None` until a keyframe primes
    /// the buffer (nothing before one is decodable).
    next_seq: Option<u32>,
    /// When packets started arriving while unprimed (keyframe-wait timer).
    unprimed_since: Option<Instant>,
    /// Set when recovery needs a fresh IDR from the sender.
    want_keyframe: bool,
    /// Last time a keyframe request was handed out (rate limiting).
    last_keyframe_request: Option<Instant>,
    /// The sender's frame rate, learned from arrivals (the backlog cap),
    /// and the arrival jitter the viewer's stats line shows.
    jitter: JitterEstimate,
    /// Sender-clock → local-clock mapping and lateness tracking.
    playout: PlayoutClock,
    /// Test hook: fixed playout delay override.
    #[cfg(test)]
    playout_delay_override: Option<Duration>,
    pub stats: JitterBufferStats,
}

impl VideoJitterBuffer {
    /// `fps` seeds the frame-rate estimate that sizes the backlog cap;
    /// the buffer then learns the sender's real cadence from arrivals,
    /// so the seed only matters for the first second or so of a stream.
    #[must_use]
    pub fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        Self {
            pending: VecDeque::new(),
            next_seq: None,
            unprimed_since: None,
            want_keyframe: false,
            last_keyframe_request: None,
            jitter: JitterEstimate::new(fps),
            playout: PlayoutClock::new(),
            #[cfg(test)]
            playout_delay_override: None,
            stats: JitterBufferStats::default(),
        }
    }

    /// Insert a packet that arrived from the network.
    pub fn push(&mut self, packet: BufferedPacket) {
        self.stats.packets_received += 1;

        // Frames come in order, each once: one that is not newer than
        // the newest seen is a new share whose numbering started over.
        // Starting over is right for it; holding it back as "late"
        // froze the new share until its numbers passed the old ones.
        if let Some(next) = self.next_seq {
            let newest = self.pending.back().map_or(next.wrapping_sub(1), |p| p.seq);
            if !is_seq_after(packet.seq, newest) {
                log::debug!(
                    "video seq {} after {newest}: the sharer started over, so does the buffer",
                    packet.seq
                );
                self.reset();
            }
        }

        self.jitter.record_arrival(packet.arrival_time);
        self.playout
            .observe(packet.arrival_time, packet.timestamp_ms);

        if self.next_seq.is_none() {
            if !packet.is_keyframe {
                // Nothing before a keyframe is decodable, so nothing is
                // kept. If this goes on long enough, ask the sender for
                // an IDR instead of waiting out its keyframe interval.
                let since = *self.unprimed_since.get_or_insert(packet.arrival_time);
                if packet.arrival_time.duration_since(since) >= UNPRIMED_REQUEST_AFTER {
                    self.want_keyframe = true;
                }
                return;
            }
            // Primed: the keyframe answers any request still pending.
            self.next_seq = Some(packet.seq);
            self.unprimed_since = None;
            self.want_keyframe = false;
        }

        self.pending.push_back(packet);
        if self.pending.len() > self.max_depth() {
            self.recover_from_overflow();
        }
        self.stats.current_depth = self.pending.len();
    }

    /// Release the next packet in sequence, if the buffer's rules allow.
    ///
    /// Call repeatedly until it returns `None`. Afterwards, poll
    /// [`Self::take_keyframe_request`] and forward it to the sender.
    pub fn pull_next(&mut self) -> Option<BufferedPacket> {
        self.pull_next_at(Instant::now())
    }

    fn pull_next_at(&mut self, now: Instant) -> Option<BufferedPacket> {
        loop {
            let next = self.next_seq?;
            let front = self.pending.front()?;
            if front.seq != next {
                // `next` never came and a later frame did: it never will.
                self.recover_from_gap(next, now);
                continue;
            }
            // The next frame is here — released at its playout time. One
            // already past its slot goes at once.
            if now < self.playout_due(front) {
                return None;
            }
            let far_behind = self
                .behind_live(front.timestamp_ms, now)
                .is_some_and(|behind| behind >= CATCH_UP_BEHIND);
            if let Some(kf) = far_behind
                .then(|| self.newest_keyframe_after(next))
                .flatten()
            {
                self.catch_up_to(kf);
                continue;
            }
            let packet = self.pending.pop_front()?;
            self.next_seq = Some(next.wrapping_add(1));
            self.stats.current_depth = self.pending.len();
            return Some(packet);
        }
    }

    /// When [`Self::pull_next`] or [`Self::take_keyframe_request`] next
    /// has something to do: the next frame's playout slot, now for a gap
    /// still to resolve, the end of the request cooldown while a request
    /// waits. `None` while there is nothing to wait for — a new frame
    /// arriving is what changes that. The screen thread sleeps until
    /// the earliest of these over its streams (review P1-7).
    #[must_use]
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        let release = match (self.next_seq, self.pending.front()) {
            (Some(next), Some(front)) if front.seq == next => Some(self.playout_due(front)),
            // A gap is resolved by the next pull, whatever the time.
            (Some(_), Some(_)) => Some(now),
            _ => None,
        };
        let request = self.want_keyframe.then(|| {
            self.last_keyframe_request
                .map_or(now, |last| last + KEYFRAME_REQUEST_COOLDOWN)
        });
        match (release, request) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// True at most once per [`KEYFRAME_REQUEST_COOLDOWN`] when recovery
    /// wants a fresh IDR from the sender. The caller relays this to the
    /// sender's encoder (`force_keyframe`).
    pub fn take_keyframe_request(&mut self) -> bool {
        if !self.want_keyframe {
            return false;
        }
        let now = Instant::now();
        if let Some(last) = self.last_keyframe_request
            && now.duration_since(last) < KEYFRAME_REQUEST_COOLDOWN
        {
            return false;
        }
        self.want_keyframe = false;
        self.last_keyframe_request = Some(now);
        self.stats.keyframe_requests += 1;
        true
    }

    /// Start again from a keyframe and ask the sender for one: the
    /// frames in between were never pushed (a viewer nobody could see,
    /// review P1-7), so whatever is buffered no longer leads anywhere.
    /// The request goes out through [`Self::take_keyframe_request`],
    /// once, and a keyframe that arrives first answers it.
    pub fn wait_for_keyframe(&mut self, now: Instant) {
        let dropped = self.pending.len();
        self.pending.clear();
        self.stats.packets_dropped_broken_reference += dropped as u64;
        self.stats.current_depth = 0;
        self.next_seq = None;
        self.unprimed_since = Some(now);
        self.want_keyframe = true;
    }

    /// Let go of what is buffered for a stream nobody sees any more:
    /// nothing was lost, so none of it counts as dropped (the viewing
    /// telemetry reports drops, review 2026-10 P3-36), and no keyframe
    /// is asked for until it is seen again ([`Self::wait_for_keyframe`]).
    pub fn set_aside(&mut self) {
        self.pending.clear();
        self.stats.current_depth = 0;
        self.next_seq = None;
    }

    /// Number of packets currently buffered.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.pending.len()
    }

    /// Buffered span in milliseconds: arrival-time spread between the
    /// oldest and newest pending packets.
    #[must_use]
    pub fn depth_ms(&self) -> f32 {
        match (self.pending.front(), self.pending.back()) {
            (Some(oldest), Some(newest)) => {
                newest
                    .arrival_time
                    .saturating_duration_since(oldest.arrival_time)
                    .as_secs_f32()
                    * 1000.0
            }
            _ => 0.0,
        }
    }

    /// Current playout delay in milliseconds — the steady latency the
    /// pacer spends to release frames at the sender's cadence.
    #[must_use]
    pub fn playout_delay_ms(&self) -> f32 {
        self.playout_delay().as_secs_f32() * 1000.0
    }

    /// Smoothed arrival-jitter estimate in milliseconds.
    #[must_use]
    pub fn adaptive_jitter_ms(&self) -> f32 {
        self.jitter.jitter_ms()
    }

    /// Whether a keyframe has primed the buffer (and no gap or backlog
    /// has unprimed it since).
    #[must_use]
    pub fn is_primed(&self) -> bool {
        self.next_seq.is_some()
    }

    /// How far behind live a frame with this sender timestamp is at
    /// `at`: how much later than the fastest delivery seen it is being
    /// handled. The playout delay and network wobble are part of it
    /// (tens of ms); a decoder slower than the stream makes it grow
    /// without bound. `None` until the clock has a mapping.
    #[must_use]
    pub fn behind_live(&self, timestamp_ms: u32, at: Instant) -> Option<Duration> {
        self.playout
            .local_time_of(timestamp_ms)
            .map(|live| at.saturating_duration_since(live))
    }

    /// Clear all state (e.g. when the peer restarts their share).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.next_seq = None;
        self.unprimed_since = None;
        self.want_keyframe = false;
        self.playout = PlayoutClock::new();
        self.stats.current_depth = 0;
    }

    // ── Internals ───────────────────────────────────────────────────

    /// Current playout delay: how far behind the fastest observed path
    /// frames are scheduled (see module docs).
    fn playout_delay(&self) -> Duration {
        #[cfg(test)]
        if let Some(d) = self.playout_delay_override {
            return d;
        }
        Duration::from_secs_f64((self.playout.late_peak_ms() + PLAYOUT_MARGIN_MS) / 1000.0)
            .clamp(PLAYOUT_DELAY_MIN, PLAYOUT_DELAY_MAX)
    }

    /// Local time at which `packet` should be handed to the decoder.
    fn playout_due(&self, packet: &BufferedPacket) -> Instant {
        self.playout
            .local_time_of(packet.timestamp_ms)
            .map_or(packet.arrival_time, |t| t + self.playout_delay())
    }

    /// The backlog cap: [`BACKLOG_CAP_SECS`] at the learned frame rate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a frame count
    fn max_depth(&self) -> usize {
        let fps = 1000.0 / self.jitter.expected_interval_ms.max(1.0);
        ((fps * BACKLOG_CAP_SECS).round() as usize).max(8)
    }

    /// The newest buffered keyframe after `after` (wraparound-aware).
    fn newest_keyframe_after(&self, after: u32) -> Option<u32> {
        self.pending
            .iter()
            .rev()
            .find(|p| p.is_keyframe && is_seq_after(p.seq, after))
            .map(|p| p.seq)
    }

    /// Drop every buffered frame before `seq` (a buffered keyframe) and
    /// go on from it. Returns how many were dropped.
    fn restart_at(&mut self, seq: u32) -> usize {
        let keep_from = self
            .pending
            .iter()
            .position(|p| p.seq == seq)
            .unwrap_or(self.pending.len());
        self.pending.drain(..keep_from);
        self.next_seq = Some(seq);
        self.stats.keyframe_resets += 1;
        self.stats.current_depth = self.pending.len();
        keep_from
    }

    /// Catch up: the next frame is too far behind live, and a newer
    /// keyframe is buffered.
    fn catch_up_to(&mut self, kf: u32) {
        let dropped = self.restart_at(kf);
        self.stats.packets_dropped_overflow += dropped as u64;
        // Counted in the viewer's log (`dropped`, `jumps`).
        log::debug!("video over {CATCH_UP_BEHIND:?} behind live: jumped to keyframe seq {kf}, dropped {dropped}");
    }

    /// `next` is missing and later frames are buffered. Go on from the
    /// first buffered keyframe if there is one (at its own playout slot,
    /// the next pull's business); otherwise freeze and ask for one.
    fn recover_from_gap(&mut self, next: u32, now: Instant) {
        self.stats.gaps_detected += 1;
        if let Some(kf) = self.pending.iter().find(|p| p.is_keyframe).map(|p| p.seq) {
            let dropped = self.restart_at(kf);
            self.stats.packets_dropped_broken_reference += dropped as u64;
            // Counted in the viewer's summary line (`lost`).
            log::debug!("video seq {next} never came: going on from keyframe seq {kf}");
            return;
        }
        // Every buffered and future delta references data that is gone.
        // Decoding them would keep motion on screen at the cost of smear
        // until the next IDR — the wrong trade for people *watching*.
        // Freeze: drop back to unprimed (only a keyframe restarts
        // delivery — the same machinery as stream start) and ask for an
        // IDR. If that IDR is itself lost, the unprimed-grace logic in
        // `push` asks again, so recovery heals itself.
        let dropped = self.pending.len();
        self.pending.clear();
        self.stats.packets_dropped_broken_reference += dropped as u64;
        self.stats.current_depth = 0;
        self.next_seq = None;
        self.unprimed_since = Some(now);
        self.want_keyframe = true;
        log::debug!(
            "video seq {next} never came and no keyframe is buffered: froze, dropped {dropped}, asking for one"
        );
    }

    /// The buffer hit its size cap: the consumer stalled for seconds.
    /// Jump to the *freshest* buffered keyframe (dropping everything
    /// older — it's stale by definition), or clear entirely and request
    /// an IDR if no keyframe is buffered.
    fn recover_from_overflow(&mut self) {
        let depth = self.pending.len();
        if let Some(kf) = self
            .pending
            .iter()
            .rev()
            .find(|p| p.is_keyframe)
            .map(|p| p.seq)
        {
            let dropped = self.restart_at(kf);
            self.stats.packets_dropped_overflow += dropped as u64;
            // Counted in the viewer's log (`dropped`, `jumps`).
            log::debug!(
                "video fell {depth} frames behind (the decoder is slower than the stream): \
                 jumped to keyframe seq {kf}, dropped {dropped} frame(s)"
            );
        } else {
            self.stats.packets_dropped_overflow += depth as u64;
            self.pending.clear();
            self.stats.current_depth = 0;
            // Same freeze as a gap with no keyframe: nothing buffered is
            // decodable without one, so drop to unprimed and resume at
            // the next IDR.
            self.next_seq = None;
            self.unprimed_since = Some(Instant::now());
            self.want_keyframe = true;
            // The viewer's log names the freeze and counts the frames.
            log::debug!("video fell {depth} frames behind with no keyframe to jump to: froze, asking for one");
        }
    }

    // ── Test hooks ──────────────────────────────────────────────────

    #[cfg(test)]
    fn set_playout_delay(&mut self, d: Duration) {
        self.playout_delay_override = Some(d);
    }

    #[cfg(test)]
    fn set_unprimed_since(&mut self, t: Instant) {
        self.unprimed_since = Some(t);
    }
}

/// Wraparound-aware "is `a` after `b`" for u32 sequence numbers.
///
/// Uses the RTP convention: `a` is after `b` if the wrapped distance is in
/// the forward half of the sequence space.
fn is_seq_after(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

// ---------------------------------------------------------------------------
// Playout clock
// ---------------------------------------------------------------------------

/// Maps sender capture timestamps onto the local clock and tracks how
/// late frames arrive relative to the fastest path seen.
///
/// The mapping is `local = timestamp + offset`, where `offset` is the
/// smallest `arrival − timestamp` observed over a sliding window (the
/// fastest path), so it follows clock drift. A frame's *lateness* is how
/// much its own `arrival − timestamp` exceeds that; the decaying peak of
/// lateness sizes the playout delay. Sender timestamps are unwrapped
/// into a running `f64` so the u32 wrap is invisible here.
struct PlayoutClock {
    /// Local reference for the ms arithmetic: the first arrival seen,
    /// so no arrival can predate it.
    epoch: Option<Instant>,
    /// Last raw sender timestamp and its unwrapped value, ms.
    last_ts: Option<(u32, f64)>,
    /// Windowed minimum of `local_ms − sender_ms` (the fastest path).
    offset: MinWindow,
    /// When the peak was last decayed.
    last_observed: Option<Instant>,
    late_peak: PeakHold,
}

impl PlayoutClock {
    fn new() -> Self {
        Self {
            epoch: None,
            last_ts: None,
            offset: MinWindow::default(),
            last_observed: None,
            late_peak: PeakHold::default(),
        }
    }

    /// Milliseconds since the epoch, at microsecond resolution so a
    /// round trip through [`Self::instant_at`] reproduces the instant.
    #[allow(clippy::cast_precision_loss)] // µs since a process-local epoch: exact for centuries
    fn local_ms(epoch: Instant, at: Instant) -> f64 {
        at.saturating_duration_since(epoch).as_micros() as f64 / 1000.0
    }

    /// Inverse of [`Self::local_ms`].
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped ≥ 0, µs fits
    fn instant_at(epoch: Instant, local_ms: f64) -> Instant {
        epoch + Duration::from_micros((local_ms.max(0.0) * 1000.0).round() as u64)
    }

    /// Unwrap a sender timestamp against the last one seen.
    fn unwrap_ts(&mut self, ts: u32) -> f64 {
        let unwrapped = match self.last_ts {
            // Wrap-aware signed delta: a u32 timestamp that wrapped
            // still yields a small positive step.
            Some((last_raw, last_unwrapped)) => {
                #[allow(clippy::cast_possible_wrap)] // reinterpreting the wrapped delta as signed
                let delta = ts.wrapping_sub(last_raw) as i32;
                last_unwrapped + f64::from(delta)
            }
            None => f64::from(ts),
        };
        self.last_ts = Some((ts, unwrapped));
        unwrapped
    }

    /// Record one arrival.
    fn observe(&mut self, arrival: Instant, ts: u32) {
        let elapsed_s = self
            .last_observed
            .map_or(0.0, |t| arrival.saturating_duration_since(t).as_secs_f64());
        self.last_observed = Some(arrival);
        self.late_peak.decay(elapsed_s);

        let epoch = *self.epoch.get_or_insert(arrival);
        let sender_ms = self.unwrap_ts(ts);
        let offset = Self::local_ms(epoch, arrival) - sender_ms;
        if let Some(current) = self.offset.min()
            && (offset - current).abs() > CLOCK_JUMP_MS
        {
            // Sender clock restarted: start over rather than
            // schedule against a dead reference.
            self.offset = MinWindow::default();
            self.late_peak = PeakHold::default();
        }
        self.offset.observe(offset, arrival);
        let fastest = self.offset.min().unwrap_or(offset);
        self.late_peak.raise(offset - fastest);
    }

    /// Local instant the fastest path would have delivered a frame with
    /// this timestamp. `None` before the first observation.
    fn local_time_of(&self, ts: u32) -> Option<Instant> {
        let epoch = self.epoch?;
        let (last_raw, last_unwrapped) = self.last_ts?;
        let offset = self.offset.min()?;
        #[allow(clippy::cast_possible_wrap)] // see unwrap_ts
        let delta = ts.wrapping_sub(last_raw) as i32;
        let sender_ms = last_unwrapped + f64::from(delta);
        Some(Self::instant_at(epoch, sender_ms + offset))
    }

    fn late_peak_ms(&self) -> f64 {
        self.late_peak.value
    }
}

/// Minimum over a sliding window of [`OFFSET_BUCKETS`] × [`OFFSET_BUCKET`].
#[derive(Default)]
struct MinWindow {
    buckets: [Option<f64>; OFFSET_BUCKETS],
    /// Index of the bucket being filled and when it opened.
    current: usize,
    opened: Option<Instant>,
}

impl MinWindow {
    fn observe(&mut self, sample: f64, at: Instant) {
        match self.opened {
            None => self.opened = Some(at),
            Some(opened) => {
                // Advance one bucket per elapsed interval, clearing what
                // it overwrites; a long silence clears the whole window.
                let elapsed = at.saturating_duration_since(opened);
                let steps = (elapsed.as_secs_f64() / OFFSET_BUCKET.as_secs_f64()).floor();
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // floor ≥ 0
                let steps = (steps as usize).min(OFFSET_BUCKETS);
                for _ in 0..steps {
                    self.current = (self.current + 1) % OFFSET_BUCKETS;
                    self.buckets[self.current] = None;
                }
                if steps > 0 {
                    self.opened = Some(at);
                }
            }
        }
        let slot = &mut self.buckets[self.current];
        *slot = Some(slot.map_or(sample, |v| v.min(sample)));
    }

    fn min(&self) -> Option<f64> {
        self.buckets.iter().flatten().copied().reduce(f64::min)
    }
}

/// A peak detector that forgets slowly: holds the largest sample seen,
/// decaying linearly at [`PEAK_DECAY_MS_PER_S`] while samples stay low.
#[derive(Default)]
struct PeakHold {
    value: f64,
}

impl PeakHold {
    fn decay(&mut self, elapsed_s: f64) {
        self.value = (self.value - PEAK_DECAY_MS_PER_S * elapsed_s).max(0.0);
    }

    fn raise(&mut self, sample_ms: f64) {
        self.value = self.value.max(sample_ms);
    }
}

// ---------------------------------------------------------------------------
// Arrival jitter estimation
// ---------------------------------------------------------------------------

/// EMA of the deviation between consecutive packet arrival gaps and the
/// nominal frame interval, and the sender's frame interval learned from
/// the same gaps.
///
/// **Known bias:** keyframes are ~10× larger than delta frames, so their
/// serialization/chunking delay inflates arrival gaps once per keyframe
/// interval, and with them this estimate. It only feeds the viewer's
/// stats line; the learned interval sizes the backlog cap.
struct JitterEstimate {
    expected_interval_ms: f32,
    jitter_ms: f32,
    last_arrival: Option<Instant>,
}

impl JitterEstimate {
    fn new(fps: u32) -> Self {
        Self {
            expected_interval_ms: 1000.0 / fps.max(1) as f32,
            // Seed at half a frame interval until measurements accumulate.
            jitter_ms: 1000.0 / fps.max(1) as f32 / 2.0,
            last_arrival: None,
        }
    }

    fn record_arrival(&mut self, at: Instant) {
        const ALPHA: f32 = 1.0 / 16.0; // RFC 3550 smoothing factor
        if let Some(last) = self.last_arrival {
            let gap_ms = at.saturating_duration_since(last).as_secs_f32() * 1000.0;

            // Learn the sender's actual frame cadence instead of trusting
            // the constructor's nominal fps: sharers pick 30 or 60, and
            // sender-side fps adaptation changes the true rate mid-stream,
            // so any static number is eventually wrong. Only plausible
            // frame-cadence gaps train the estimate — chunk-completion
            // bursts (~0 ms) and keyframe-assembly pauses (150 ms+) are
            // arrival artifacts, not cadence.
            if (8.0..=120.0).contains(&gap_ms) {
                const RATE_ALPHA: f32 = 1.0 / 32.0;
                self.expected_interval_ms += RATE_ALPHA * (gap_ms - self.expected_interval_ms);
            }

            // Clamp the deviation so a stream pause (alt-tab, network
            // hiccup measured in seconds) doesn't poison the estimate.
            let deviation = (gap_ms - self.expected_interval_ms).abs().min(250.0);
            self.jitter_ms += ALPHA * (deviation - self.jitter_ms);
        }
        self.last_arrival = Some(at);
    }

    fn jitter_ms(&self) -> f32 {
        self.jitter_ms
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32, is_keyframe: bool) -> BufferedPacket {
        pkt_at(seq, is_keyframe, Instant::now(), 0)
    }

    fn pkt_at(
        seq: u32,
        is_keyframe: bool,
        arrival_time: Instant,
        timestamp_ms: u32,
    ) -> BufferedPacket {
        BufferedPacket {
            seq,
            is_keyframe,
            width: 1280,
            height: 720,
            data: vec![0u8; 8],
            arrival_time,
            timestamp_ms,
        }
    }

    /// A buffer with playout pacing disabled, for tests about ordering
    /// and loss recovery where "available == released" is the premise.
    fn unpaced(fps: u32) -> VideoJitterBuffer {
        let mut buf = VideoJitterBuffer::new(fps);
        buf.set_playout_delay(Duration::ZERO);
        buf
    }

    #[test]
    fn unprimed_until_first_keyframe() {
        let mut buf = unpaced(30);
        buf.push(pkt(5, false));
        buf.push(pkt(6, false));
        assert!(!buf.is_primed());
        // Undecodable deltas are not even kept.
        assert_eq!(buf.depth(), 0);
        assert!(buf.pull_next().is_none());

        buf.push(pkt(7, true));
        assert!(buf.is_primed());
        let p = buf.pull_next().expect("keyframe releases");
        assert_eq!(p.seq, 7);
        assert!(p.is_keyframe);
    }

    #[test]
    fn releases_immediately_and_in_order() {
        let mut buf = unpaced(30);
        buf.push(pkt(10, true));
        // No depth gating: available == released.
        assert_eq!(buf.pull_next().unwrap().seq, 10);
        assert!(buf.pull_next().is_none());

        buf.push(pkt(11, false));
        buf.push(pkt(12, false));
        assert_eq!(buf.pull_next().unwrap().seq, 11);
        assert_eq!(buf.pull_next().unwrap().seq, 12);
        assert!(buf.pull_next().is_none());
    }

    #[test]
    fn a_gap_without_a_keyframe_freezes_and_asks_at_once() {
        let mut buf = unpaced(30);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        buf.push(pkt(3, false)); // 1, 2 never come; no keyframe ahead
        buf.push(pkt(4, false));
        // No waiting: the pull that sees the gap freezes. Nothing is
        // delivered (decoding these deltas would smear), the orphans
        // are dropped, and an IDR is requested.
        assert!(buf.pull_next().is_none());
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
        assert_eq!(buf.stats.packets_dropped_broken_reference, 2);
        assert_eq!(buf.stats.gaps_detected, 1);
        assert!(buf.take_keyframe_request());
        // Rate-limited: not twice in a row.
        assert!(!buf.take_keyframe_request());

        // More deltas from the broken GOP arrive: still frozen.
        buf.push(pkt(5, false));
        assert!(buf.pull_next().is_none());

        // The requested IDR arrives: delivery resumes cleanly from it.
        buf.push(pkt(6, true));
        let p = buf.pull_next().expect("resumes at the keyframe");
        assert_eq!(p.seq, 6);
        assert!(p.is_keyframe);
        buf.push(pkt(7, false));
        assert_eq!(buf.pull_next().unwrap().seq, 7);
    }

    #[test]
    fn a_gap_jumps_to_a_buffered_keyframe_at_once() {
        let mut buf = unpaced(30);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        buf.push(pkt(4, false));
        buf.push(pkt(6, true)); // a fresh GOP starts at 6
        buf.push(pkt(7, false));

        let p = buf.pull_next().expect("jumps to keyframe");
        assert_eq!(p.seq, 6);
        assert!(p.is_keyframe);
        // The orphaned delta before the keyframe was discarded.
        assert_eq!(buf.pull_next().unwrap().seq, 7);
        // Clean keyframe restart → no request needed.
        assert!(!buf.take_keyframe_request());
        assert_eq!(buf.stats.keyframe_resets, 1);
        assert_eq!(buf.stats.gaps_detected, 1);
        assert_eq!(buf.stats.packets_dropped_broken_reference, 1);
    }

    #[test]
    fn an_older_sequence_number_is_a_new_share() {
        let mut buf = unpaced(30);
        buf.push(pkt(10, true));
        assert_eq!(buf.pull_next().unwrap().seq, 10);
        buf.push(pkt(11, false));
        // The sharer started over: its first keyframe primes again.
        buf.push(pkt(0, true));
        assert_eq!(buf.depth(), 1);
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        assert_eq!(buf.pull_next().unwrap().seq, 1);
    }

    #[test]
    fn a_repeated_sequence_number_starts_over_too() {
        let mut buf = unpaced(30);
        buf.push(pkt(10, true));
        buf.push(pkt(11, false));
        // Not newer than the newest seen: start over; a delta cannot
        // prime, so the buffer waits for a keyframe.
        buf.push(pkt(11, false));
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
    }

    #[test]
    fn overflow_jumps_to_freshest_keyframe() {
        let mut buf = unpaced(3); // max_depth = 9
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        // Stalled consumer: later packets pile up unpulled.
        for seq in [2, 3, 4, 5, 6] {
            buf.push(pkt(seq, false));
        }
        buf.push(pkt(7, true)); // a fresh GOP is in there
        for seq in [8, 9, 10, 11] {
            buf.push(pkt(seq, false)); // final push exceeds max_depth
        }

        assert_eq!(buf.stats.packets_dropped_overflow, 5);
        // Recovery landed exactly on the keyframe.
        assert_eq!(buf.pull_next().unwrap().seq, 7);
        assert_eq!(buf.pull_next().unwrap().seq, 8);
    }

    #[test]
    fn overflow_without_keyframe_freezes_and_requests() {
        let mut buf = unpaced(3); // max_depth = 9
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);

        for seq in 1..=10 {
            buf.push(pkt(seq, false)); // all deltas, overflow on the 10th
        }
        assert_eq!(buf.depth(), 0);
        assert!(!buf.is_primed());
        assert!(buf.take_keyframe_request());

        // Resumes at the next keyframe, exactly like stream start.
        buf.push(pkt(11, true));
        assert!(buf.is_primed());
        assert_eq!(buf.pull_next().unwrap().seq, 11);
    }

    #[test]
    fn unprimed_stream_requests_keyframe_after_grace() {
        let mut buf = unpaced(30);
        buf.push(pkt(100, false));
        assert!(!buf.take_keyframe_request()); // inside the grace period

        // Simulate the grace period having elapsed.
        buf.set_unprimed_since(
            Instant::now()
                .checked_sub(UNPRIMED_REQUEST_AFTER * 2)
                .unwrap(),
        );
        buf.push(pkt(101, false));
        assert!(buf.take_keyframe_request());
    }

    #[test]
    fn a_keyframe_answers_a_request_not_yet_sent() {
        let mut buf = unpaced(30);
        buf.set_unprimed_since(
            Instant::now()
                .checked_sub(UNPRIMED_REQUEST_AFTER * 2)
                .unwrap(),
        );
        buf.push(pkt(5, false));
        buf.push(pkt(6, true));
        assert!(!buf.take_keyframe_request());
    }

    #[test]
    fn waiting_for_a_keyframe_asks_once_and_resumes_on_one() {
        let mut buf = unpaced(30);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        // Back on screen after frames 2..9 were never pushed.
        buf.wait_for_keyframe(Instant::now());
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
        assert!(buf.take_keyframe_request());
        assert!(!buf.take_keyframe_request());
        buf.push(pkt(10, false));
        assert!(buf.pull_next().is_none());
        buf.push(pkt(11, true));
        assert_eq!(buf.pull_next().unwrap().seq, 11);
    }

    #[test]
    fn seq_wraparound() {
        assert!(is_seq_after(1, 0));
        assert!(is_seq_after(0, u32::MAX)); // wraps
        assert!(!is_seq_after(u32::MAX, 0));
        assert!(!is_seq_after(0, 0));

        let mut buf = unpaced(30);
        buf.push(pkt(u32::MAX, true));
        assert_eq!(buf.pull_next().unwrap().seq, u32::MAX);
        buf.push(pkt(0, false));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        assert_eq!(buf.pull_next().unwrap().seq, 1);
    }

    #[test]
    fn reset_clears_everything() {
        let mut buf = unpaced(30);
        buf.push(pkt(0, true));
        assert_eq!(buf.pull_next().unwrap().seq, 0);
        buf.push(pkt(1, false));
        buf.reset();
        assert!(!buf.is_primed());
        assert_eq!(buf.depth(), 0);
        assert!(buf.pull_next().is_none());
    }

    mod next_due {
        use super::*;

        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }

        #[test]
        fn nothing_to_wait_for_when_caught_up() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            assert_eq!(buf.next_due(t0), None);
            buf.push(pkt_at(1, true, t0, 0));
            let due = buf.next_due(t0).expect("a frame is buffered");
            assert_eq!(buf.pull_next_at(due).map(|p| p.seq), Some(1));
            assert_eq!(buf.next_due(due), None);
        }

        #[test]
        fn is_the_next_frames_playout_slot() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            let due = buf.next_due(t0).unwrap();
            assert!(due > t0 + ms(1), "held for the playout delay");
            assert!(buf.pull_next_at(t0 + ms(1)).is_none());
            assert!(buf.pull_next_at(due).is_some());
        }

        #[test]
        fn a_gap_is_due_at_once() {
            let t0 = Instant::now();
            let mut buf = unpaced(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.pull_next_at(t0);
            buf.push(pkt_at(3, false, t0, 32));
            assert_eq!(buf.next_due(t0 + ms(5)), Some(t0 + ms(5)));
        }

        #[test]
        fn a_held_back_request_is_due_when_the_cooldown_ends() {
            let mut buf = unpaced(30);
            buf.wait_for_keyframe(Instant::now());
            let now = Instant::now();
            assert_eq!(buf.next_due(now), Some(now), "the first one goes now");
            assert!(buf.take_keyframe_request());
            assert_eq!(buf.next_due(now), None);
            // Asked again inside the cooldown: due when it ends.
            buf.wait_for_keyframe(Instant::now());
            assert!(!buf.take_keyframe_request());
            let due = buf.next_due(Instant::now()).unwrap();
            assert!(due > Instant::now() + KEYFRAME_REQUEST_COOLDOWN / 2);
            assert!(due <= Instant::now() + KEYFRAME_REQUEST_COOLDOWN);
        }
    }

    mod playout_pacing {
        use super::*;

        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }

        #[test]
        fn behind_live_is_the_lateness_against_the_fastest_path() {
            // Two frames 33 ms apart that arrived on the fastest path;
            // the second handled 400 ms after it could have been.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            assert_eq!(buf.behind_live(0, t0), None);
            buf.push(pkt_at(1, true, t0, 1_000));
            buf.push(pkt_at(2, false, t0 + ms(33), 1_033));
            assert_eq!(buf.behind_live(1_033, t0 + ms(33)), Some(Duration::ZERO));
            assert_eq!(buf.behind_live(1_033, t0 + ms(433)), Some(ms(400)));
        }

        #[test]
        fn a_frame_far_behind_live_skips_to_the_newest_keyframe() {
            // A 30 fps stream that arrived on time, handled 1.5 s late:
            // the backlog holds keyframes at 10 and 40.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            for seq in 1..=50u32 {
                let ts = (seq - 1) * 33;
                let kf = seq == 1 || seq == 10 || seq == 40;
                buf.push(pkt_at(seq, kf, t0 + ms(u64::from(ts)), ts));
            }
            let at = t0 + ms(1_500);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(40));
            assert_eq!(buf.stats.keyframe_resets, 1);
            assert_eq!(buf.stats.packets_dropped_overflow, 39);
        }

        #[test]
        fn a_frame_a_little_late_is_not_skipped() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            for seq in 1..=20u32 {
                let ts = (seq - 1) * 33;
                buf.push(pkt_at(
                    seq,
                    seq == 1 || seq == 10,
                    t0 + ms(u64::from(ts)),
                    ts,
                ));
            }
            assert_eq!(buf.pull_next_at(t0 + ms(500)).map(|p| p.seq), Some(1));
            assert_eq!(buf.stats.keyframe_resets, 0);
        }

        #[test]
        fn the_backlog_cap_follows_the_learned_frame_rate() {
            // Created for 30 fps, fed 60: the cap grows to 3 s of 60.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(30);
            assert_eq!(buf.max_depth(), 90);
            for seq in 1..=300u32 {
                let ts = (seq - 1) * 1000 / 60;
                buf.push(pkt_at(seq, seq == 1, t0 + ms(u64::from(ts)), ts));
                let _ = buf.pull_next_at(t0 + ms(u64::from(ts)) + PLAYOUT_DELAY_MAX);
            }
            assert!(buf.max_depth() >= 170, "{}", buf.max_depth());
        }

        #[test]
        fn a_frame_is_held_until_its_playout_slot() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            assert!(buf.pull_next_at(t0).is_none());
        }

        #[test]
        fn a_frame_is_released_once_the_minimum_delay_has_passed() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            let at = t0 + PLAYOUT_DELAY_MIN + ms(1);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(1));
        }

        /// Steady 60 fps for a while (establishes the fastest path), then
        /// a keyframe-sized stall: frames 5..8 all land together, 150 ms
        /// late. Returns the buffer and the bunch's arrival instant.
        fn after_a_keyframe_bunch() -> (VideoJitterBuffer, Instant) {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            for i in 0..4u32 {
                buf.push(pkt_at(i + 1, i == 0, t0 + ms(16 * u64::from(i)), 16 * i));
            }
            for _ in 0..4 {
                buf.pull_next_at(t0 + ms(100));
            }
            let bunch_at = t0 + ms(64) + ms(150);
            for i in 4..8u32 {
                buf.push(pkt_at(i + 1, false, bunch_at, 16 * i));
            }
            (buf, bunch_at)
        }

        #[test]
        fn first_frame_of_a_bunch_is_released_within_the_margin() {
            let (mut buf, bunch_at) = after_a_keyframe_bunch();
            let at = bunch_at + Duration::from_secs_f64(PLAYOUT_MARGIN_MS / 1000.0) + ms(1);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(5));
        }

        #[test]
        fn rest_of_a_bunch_is_paced_at_sender_cadence() {
            let (mut buf, bunch_at) = after_a_keyframe_bunch();
            let at = bunch_at + Duration::from_secs_f64(PLAYOUT_MARGIN_MS / 1000.0) + ms(1);
            buf.pull_next_at(at);
            assert!(
                buf.pull_next_at(at).is_none(),
                "seq 6 must wait its 16 ms slot"
            );
        }

        #[test]
        fn second_frame_of_a_bunch_lands_one_interval_later() {
            let (mut buf, bunch_at) = after_a_keyframe_bunch();
            let at = bunch_at + Duration::from_secs_f64(PLAYOUT_MARGIN_MS / 1000.0) + ms(1);
            buf.pull_next_at(at);
            assert_eq!(buf.pull_next_at(at + ms(16)).map(|p| p.seq), Some(6));
        }

        #[test]
        fn an_early_burst_does_not_anchor_the_clock_for_long() {
            // Encoder pipeline drain: seq 2 arrives together with seq 1
            // despite a 16 ms newer stamp. After the window rolls, a
            // steady frame is on schedule again rather than "late".
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.push(pkt_at(2, false, t0, 16));
            let later = t0 + OFFSET_BUCKET * (OFFSET_BUCKETS as u32 + 1);
            buf.push(pkt_at(3, false, later, 3000));
            // Exactly zero by construction: nothing was late.
            #[expect(clippy::float_cmp)]
            {
                assert_eq!(buf.playout.late_peak_ms(), 0.0);
            }
        }

        #[test]
        fn lateness_grows_the_playout_delay() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.push(pkt_at(2, false, t0 + ms(16) + ms(150), 16));
            assert!(
                buf.playout_delay_ms() >= 150.0,
                "delay {}",
                buf.playout_delay_ms()
            );
        }

        #[test]
        fn playout_delay_never_exceeds_the_cap() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.push(pkt_at(2, false, t0 + ms(16) + ms(900), 16));
            assert_eq!(buf.playout_delay(), PLAYOUT_DELAY_MAX);
        }

        #[test]
        fn a_sender_clock_restart_reanchors_instead_of_waiting() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 60_000));
            buf.pull_next_at(t0 + ms(30));
            // New share: timestamps start over from 0.
            buf.push(pkt_at(2, true, t0 + ms(100), 0));
            let at = t0 + ms(100) + PLAYOUT_DELAY_MIN + ms(1);
            assert_eq!(buf.pull_next_at(at).map(|p| p.seq), Some(2));
        }

        #[test]
        fn timestamps_unwrap_across_u32_max() {
            let mut clock = PlayoutClock::new();
            let before = clock.unwrap_ts(u32::MAX - 5);
            let after = clock.unwrap_ts(10);
            // Sixteen ticks apart across the wrap: exact, no rounding.
            #[expect(clippy::float_cmp)]
            {
                assert_eq!(after - before, 16.0);
            }
        }

        #[test]
        fn a_keyframe_after_a_gap_still_waits_for_its_playout_slot() {
            // The jump happens at once; the keyframe is not shown before
            // its own slot.
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.pull_next_at(t0 + ms(30));
            buf.push(pkt_at(3, true, t0 + ms(32), 32));
            assert!(buf.pull_next_at(t0 + ms(33)).is_none());
            assert_eq!(buf.stats.gaps_detected, 1);
        }

        #[test]
        fn a_keyframe_after_a_gap_goes_once_its_slot_has_passed() {
            let t0 = Instant::now();
            let mut buf = VideoJitterBuffer::new(60);
            buf.push(pkt_at(1, true, t0, 0));
            buf.pull_next_at(t0 + ms(30));
            buf.push(pkt_at(3, true, t0 + ms(32), 32));
            let after_slot = t0 + ms(32) + PLAYOUT_DELAY_MIN + ms(1);
            assert_eq!(buf.pull_next_at(after_slot).map(|p| p.seq), Some(3));
        }
    }
}
