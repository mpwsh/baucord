//! Playing a watched stream's ring at a steady pace, from the device's
//! own callback.
//!
//! A stream's audio arrives in bursts (the sharer sends it on its video
//! tick, and the network adds its jitter) and plays at the device's
//! clock. A [`PacedReader`] sits between the two on the reading side of
//! an SPSC ring:
//!
//! - it primes to [`Pacing::prebuffer`] before it plays, and again after
//!   running dry, rather than play every arriving packet as a click;
//! - above [`Pacing::catch_up_start`] queued — a burst, the sender's
//!   clock running ahead of ours, a device that stopped reading for a
//!   moment — it plays a little faster (1–4 %, [`MAX_SPEEDUP`]) until it
//!   is back to [`Pacing::catch_up_stop`]: some seconds slightly quick
//!   instead of a jump heard as a skip (2026-10-08, the A07 sat at
//!   250–290 ms queued for a minute after one burst);
//! - only a backlog past [`Pacing::max_fill`] is dropped at once, down
//!   to [`Pacing::trim_to`].
//!
//! The phones' mix (`stream::mix`, ANDROID-SPEC.md D61) was the first
//! reader. Since review P1-4 the stream's own output reads the same way
//! from its device callback (`stream::output`), which replaced a worker
//! thread that woke every 5 ms and, when it resampled, never shed a
//! backlog (PRODUCTION-PLAN.md CLI-14).
//!
//! Real-time safe: no locks, no allocation, no logging.

use rtrb::Consumer;

/// A reader's levels in ms of audio, in [`Pacing`]'s order: prebuffer,
/// catch-up start and stop, max fill, trim-to.
type LevelsMs = [usize; 5];

/// The phones' (the mix, and a stream's own output there): they cover
/// one Android playback callback, which the voice worker refills in one
/// pass (up to 40 ms, ANDROID-SPEC.md D24), and a mobile radio.
const PHONE_MS: LevelsMs = [100, 180, 120, 400, 200];

/// A desktop's own stream output, read from a ~10 ms device callback
/// (review P1-4): the cushion is two of the sharer's 20 ms packets and
/// some network jitter. Sharers send each packet as it is encoded since
/// 1.2.1 (PRODUCTION-PLAN.md LAT-2); a 1.2.0 sharer's bursts on its
/// video tick (33 ms at 30 fps) still fit. The phones' 100 ms here would
/// have played a desktop stream's sound ~60 ms behind its picture.
const DESKTOP_MS: LevelsMs = [60, 120, 80, 400, 160];

/// How much faster: in proportion to the excess over the stop level
/// (1 % per 40 ms), between these — 4 % is about two-thirds of a
/// semitone for a second or two, 1 % a sixth of one — so it eases in
/// and out. 160 ms over takes ~10 s to drain.
const SPEEDUP_PER_SECOND: f32 = 0.25;
const MIN_SPEEDUP: f32 = 0.01;
pub(crate) const MAX_SPEEDUP: f32 = 0.04;

/// A reader's levels, in frames at its ring's rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pacing {
    /// Fill a reader waits for before it plays, and after running dry
    /// (the phones' 100 ms: network jitter, a burst, a playback callback).
    pub(crate) prebuffer: usize,
    /// Past this much queued (180 ms) a reader plays faster…
    pub(crate) catch_up_start: usize,
    /// …until it is back down to this (120 ms: the cushion and a burst).
    pub(crate) catch_up_stop: usize,
    /// Past this much queued (400 ms) the oldest audio is dropped at once…
    pub(crate) max_fill: usize,
    /// …down to this (200 ms), and the catch-up does the rest.
    pub(crate) trim_to: usize,
    /// Frames per second, for the speed-up's arithmetic.
    rate: f32,
}

impl Pacing {
    /// The phones' levels for a ring of `rate` frames per second.
    pub(crate) fn at_rate(rate: u32) -> Self {
        Self::from_ms(rate, PHONE_MS)
    }

    /// The levels for a stream's own output: the desktop's lower ones,
    /// the phones' there.
    pub(crate) fn own_output(rate: u32) -> Self {
        if cfg!(any(target_os = "android", target_os = "ios")) {
            Self::at_rate(rate)
        } else {
            Self::from_ms(rate, DESKTOP_MS)
        }
    }

    fn from_ms(rate: u32, [prebuffer, start, stop, max_fill, trim_to]: LevelsMs) -> Self {
        let frames = |ms: usize| rate as usize * ms / 1000;
        Self {
            prebuffer: frames(prebuffer),
            catch_up_start: frames(start),
            catch_up_stop: frames(stop),
            max_fill: frames(max_fill),
            trim_to: frames(trim_to),
            #[allow(clippy::cast_precision_loss)] // a sample rate
            rate: rate as f32,
        }
    }
}

/// The reading side of a ring of interleaved `C`-channel frames. The
/// writer must push whole frames.
pub(crate) struct PacedReader<const C: usize> {
    consumer: Consumer<f32>,
    pacing: Pacing,
    /// Playing (prebuffered); false until the prebuffer is queued.
    primed: bool,
    /// Playing faster to bring the queue back down (see the module docs).
    catching_up: bool,
    /// The last frame consumed, and where past it the next one played
    /// sits (0 < `phase` ≤ 1 of a frame; 1 is the first one queued, as
    /// in plain reading): the faster reading interpolates between
    /// frames.
    prev: [f32; C],
    phase: f32,
}

impl<const C: usize> PacedReader<C> {
    pub(crate) fn new(consumer: Consumer<f32>, pacing: Pacing) -> Self {
        Self {
            consumer,
            pacing,
            primed: false,
            catching_up: false,
            prev: [0.0; C],
            phase: 1.0,
        }
    }

    /// The writer let go of the ring.
    pub(crate) fn is_abandoned(&self) -> bool {
        self.consumer.is_abandoned()
    }

    /// Frames queued.
    pub(crate) fn queued(&self) -> usize {
        self.consumer.slots() / C
    }

    #[cfg(test)]
    pub(crate) fn catching_up(&self) -> bool {
        self.catching_up
    }

    /// Add the next `out.len() / C` frames (interleaved) to `out`:
    /// nothing while priming, faster while catching up.
    pub(crate) fn add_to(&mut self, out: &mut [f32]) {
        let frames = out.len() / C;
        let queued = self.queued();
        if queued > self.pacing.max_fill {
            if let Ok(chunk) = self.consumer.read_chunk((queued - self.pacing.trim_to) * C) {
                chunk.commit_all();
            }
            self.phase = 1.0;
        }
        let queued = self.queued();
        if !self.primed {
            if queued < self.pacing.prebuffer {
                return;
            }
            self.primed = true;
        }
        if queued > self.pacing.catch_up_start {
            self.catching_up = true;
        }
        #[allow(clippy::cast_precision_loss)] // one callback's frames
        let len = frames as f32;
        if self.catching_up {
            if queued > self.pacing.catch_up_stop {
                #[allow(clippy::cast_precision_loss)] // frames, < 2^24
                let excess = (queued - self.pacing.catch_up_stop) as f32 / self.pacing.rate;
                let speedup = (excess * SPEEDUP_PER_SECOND).clamp(MIN_SPEEDUP, MAX_SPEEDUP);
                self.read_faster(out, self.phase + len * (1.0 + speedup), queued);
                return;
            }
            // Back down. This last read ends on a whole frame (it is at
            // most one frame faster), so the plain reading below carries
            // on from exactly there.
            self.catching_up = false;
            if self.phase < 1.0 && queued > frames + 1 {
                self.read_faster(out, (self.phase + len).ceil(), queued);
                return;
            }
        }
        self.phase = 1.0;
        let n = queued.min(frames);
        if let Ok(chunk) = self.consumer.read_chunk(n * C) {
            let (first, second) = chunk.as_slices();
            for (slot, &sample) in out.iter_mut().zip(first.iter().chain(second)) {
                *slot += sample;
            }
            if n > 0 {
                let at = |s: usize| {
                    first
                        .get(s)
                        .or_else(|| second.get(s - first.len()))
                        .copied()
                        .unwrap_or(0.0)
                };
                for (c, prev) in self.prev.iter_mut().enumerate() {
                    *prev = at((n - 1) * C + c);
                }
            }
            chunk.commit_all();
        }
        if n < frames {
            // Ran dry: wait for a cushion again rather than play every
            // arriving packet as a click.
            self.primed = false;
        }
    }

    /// One callback's frames played from positions `phase` up to (not
    /// including) `end`, evenly spaced, by linear interpolation between
    /// frames: `prev` is position 0, the queue's frames 1, 2, ….
    /// Afterwards the frame just before `end` is the new `prev`. Only
    /// while far more is queued than a callback needs (above the stop
    /// level), so it does not run dry here.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // positions within one callback
    fn read_faster(&mut self, out: &mut [f32], end: f32, queued: usize) {
        let frames = out.len() / C;
        if frames == 0 {
            return;
        }
        let step = (end - self.phase) / frames as f32;
        // Consumed: every frame before `end`.
        let used = (end.ceil() as usize).saturating_sub(1).min(queued);
        // Interpolating up to `end` reads one frame further.
        let peek = (used + 1).min(queued);
        let Ok(chunk) = self.consumer.read_chunk(peek * C) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        let prev = self.prev;
        let sample = |frame: usize, c: usize| -> f32 {
            if frame == 0 {
                return prev[c];
            }
            let s = (frame - 1) * C + c;
            first
                .get(s)
                .or_else(|| second.get(s - first.len()))
                .copied()
                .unwrap_or(0.0)
        };
        for (i, slots) in out.chunks_exact_mut(C).enumerate() {
            let pos = self.phase + i as f32 * step;
            let k = pos.floor();
            let frac = pos - k;
            let k = k as usize;
            let next = (k + 1).min(peek);
            for (c, slot) in slots.iter_mut().enumerate() {
                *slot += sample(k, c) * (1.0 - frac) + sample(next, c) * frac;
            }
        }
        let mut last = [0.0; C];
        for (c, value) in last.iter_mut().enumerate() {
            *value = sample(used, c);
        }
        chunk.commit(used * C);
        self.prev = last;
        self.phase = end - used as f32;
    }
}

#[cfg(test)]
mod tests {
    use rtrb::{Producer, RingBuffer};

    use super::*;

    /// A stereo reader at 44.1 kHz and its writer: the stream's own
    /// output on a device that is not at 48 kHz (CLI-14's case).
    fn stereo() -> (Producer<f32>, PacedReader<2>) {
        let (producer, consumer) = RingBuffer::new(44_100 * 2);
        (
            producer,
            PacedReader::new(consumer, Pacing::at_rate(44_100)),
        )
    }

    /// `n` frames of a ramp per channel: left counts up, right counts
    /// down, so a swapped or skipped frame shows.
    fn feed(producer: &mut Producer<f32>, fed: &mut u32, n: usize) {
        for _ in 0..n {
            *fed += 1;
            #[allow(clippy::cast_precision_loss)] // < 2^24
            let v = *fed as f32 * 1e-5;
            producer.push(v).unwrap();
            producer.push(-v).unwrap();
        }
    }

    #[test]
    fn levels_follow_the_rate() {
        let p = Pacing::at_rate(48_000);
        assert_eq!(
            (
                p.prebuffer,
                p.catch_up_start,
                p.catch_up_stop,
                p.max_fill,
                p.trim_to
            ),
            (4_800, 8_640, 5_760, 19_200, 9_600),
            "the phones' mix keeps its levels"
        );
        let p = Pacing::at_rate(44_100);
        assert_eq!((p.prebuffer, p.trim_to), (4_410, 8_820));
    }

    /// Each set primes below where it stops catching up, stops below
    /// where it starts, and trims to above that, below the most it holds.
    #[test]
    fn the_levels_are_in_order() {
        for levels in [PHONE_MS, DESKTOP_MS] {
            let p = Pacing::from_ms(48_000, levels);
            assert!(
                p.prebuffer < p.catch_up_stop
                    && p.catch_up_stop < p.catch_up_start
                    && p.catch_up_start < p.trim_to
                    && p.trim_to < p.max_fill,
                "{levels:?}"
            );
        }
    }

    #[test]
    fn stereo_waits_for_its_cushion_and_keeps_the_channels() {
        let (mut producer, mut reader) = stereo();
        let mut fed = 0;
        feed(&mut producer, &mut fed, 4_409);
        let mut out = vec![0.0; 441 * 2];
        reader.add_to(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "not primed yet");
        feed(&mut producer, &mut fed, 1);
        reader.add_to(&mut out);
        for (i, pair) in out.chunks_exact(2).enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let want = (i + 1) as f32 * 1e-5;
            assert!((pair[0] - want).abs() < 1e-9, "left at {i}");
            assert!((pair[1] + want).abs() < 1e-9, "right at {i}");
        }
    }

    /// CLI-14: a backlog on a 44.1 kHz device used to wait in the
    /// worker's accumulator for good. Read from the callback, it comes
    /// back down by playing faster — seamlessly, both channels in step
    /// — while the stream keeps arriving at its pace.
    #[test]
    fn a_stereo_backlog_drains_seamlessly() {
        let (mut producer, mut reader) = stereo();
        let mut fed = 0;
        let mut out = vec![0.0; 441 * 2];
        let mut last: Option<(f32, f32)> = None;
        let mut check = |out: &[f32], hop: usize| {
            for pair in out.chunks_exact(2) {
                assert!((pair[0] + pair[1]).abs() < 1e-6, "channels apart at {hop}");
                if let Some((l, _)) = last {
                    let step = (pair[0] - l) / 1e-5;
                    assert!((0.8..1.25).contains(&step), "a step of {step} at {hop}");
                }
                last = Some((pair[0], pair[1]));
            }
        };
        feed(&mut producer, &mut fed, 4_410);
        for hop in 0..20 {
            feed(&mut producer, &mut fed, 441);
            out.fill(0.0);
            reader.add_to(&mut out);
            check(&out, hop);
        }
        // A burst: 280 ms queued now.
        feed(&mut producer, &mut fed, 12_348 - 4_410);
        let mut hops = 0;
        loop {
            feed(&mut producer, &mut fed, 441);
            out.fill(0.0);
            reader.add_to(&mut out);
            check(&out, hops);
            hops += 1;
            if hops == 1 {
                assert!(reader.catching_up(), "a backlog");
            } else if !reader.catching_up() {
                break;
            }
            assert!(hops < 3_000, "never caught up");
        }
        assert!(reader.queued() <= Pacing::at_rate(44_100).catch_up_stop);
        let level = reader.queued();
        for hop in 0..100 {
            feed(&mut producer, &mut fed, 441);
            out.fill(0.0);
            reader.add_to(&mut out);
            check(&out, hop);
        }
        assert_eq!(reader.queued(), level, "back at the normal pace");
    }

    #[test]
    fn a_huge_stereo_backlog_is_trimmed_at_once() {
        let (mut producer, mut reader) = stereo();
        let mut fed = 0;
        let pacing = Pacing::at_rate(44_100);
        feed(&mut producer, &mut fed, pacing.max_fill + 1_000);
        let mut out = vec![0.0; 441 * 2];
        reader.add_to(&mut out);
        let left = reader.queued();
        // Trimmed, then one callback read a little faster than it plays.
        assert!(
            (pacing.trim_to - 460..pacing.trim_to - 441).contains(&left),
            "{left}"
        );
        // Whole frames: the right channel still mirrors the left.
        assert!((out[0] + out[1]).abs() < 1e-6);
    }
}
