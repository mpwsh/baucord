//! A watched stream's audio played through the voice output (phones,
//! ANDROID-SPEC.md D56, IOS-SPEC.md D52).
//!
//! On a phone's voice-call path — Android's communication mode, iOS's
//! voice-processing unit — a stream played on an output of its own is a
//! *media* stream beside a call: Android gives it the media volume while
//! the volume buttons move the call volume (and some phones silence media
//! during a call altogether), it does not follow the call's route, and on
//! iOS it is not part of what the echo canceller removes from the
//! microphone. So while a voice output on the call path runs, each
//! stream decoder downmixes to mono (`stream::downmix`, which keeps
//! out-of-phase sound a plain sum would cancel) and hands its samples here
//! ([`MixSource`]), and that output's worker adds them to every 10 ms hop
//! ([`MixBus::mix_into`]) — after deafen and the voice volume, which a
//! stream ignores as before, and before the limiter. Everywhere else (the
//! desktop; a phone on headphones or with echo cancellation off) the
//! stream keeps its own stereo output (`stream::output`).
//!
//! One SPSC ring per source, all held by [`BUS`]; the voice worker is the
//! only reader. A source primes to [`PREBUFFER`] before it plays (and
//! again after running dry). Above [`CATCH_UP_START`] queued — a burst
//! from the network, the sender's clock running ahead of the phone's, a
//! voice output that stopped reading for a moment — it plays a little
//! faster (1–4 %, [`MAX_SPEEDUP`]) until it is back to
//! [`CATCH_UP_STOP`]: some seconds slightly quick instead of the
//! 150 ms jump the old trim made, heard as a skip (2026-10-08, the
//! A07 sat at 250–290 ms queued for a minute after one burst). Only a
//! backlog past [`MAX_FILL`] is still dropped at once. A source whose
//! decoder let go is dropped at once: a stream that stopped stops.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex, PoisonError, TryLockError,
};

use rtrb::{Consumer, Producer, RingBuffer};

/// What one source may hold: 500 ms at 48 kHz mono.
const CAPACITY: usize = 24_000;
/// Fill a source waits for before it plays, and after running dry:
/// 100 ms — network jitter plus one Android playback callback, which the
/// voice worker refills in one pass (up to 40 ms, ANDROID-SPEC.md D24).
pub(crate) const PREBUFFER: usize = 4_800;
/// Past this much queued (180 ms) a source plays faster…
pub(crate) const CATCH_UP_START: usize = 8_640;
/// …until it is back down to this (120 ms: the cushion and a burst).
pub(crate) const CATCH_UP_STOP: usize = 5_760;
/// How much faster: in proportion to the excess over the stop level
/// (1 % per 40 ms), between these — 4 % is about two-thirds of a
/// semitone for a second or two, 1 % a sixth of one — so it eases in
/// and out. 160 ms over takes ~10 s to drain.
const SPEEDUP_PER_SECOND: f32 = 0.25;
const MIN_SPEEDUP: f32 = 0.01;
pub(crate) const MAX_SPEEDUP: f32 = 0.04;
/// Past this much queued (400 ms) the oldest audio is dropped at once…
pub(crate) const MAX_FILL: usize = 19_200;
/// …down to this (200 ms), and the catch-up does the rest.
pub(crate) const TRIM_TO: usize = 9_600;

/// The process's bus: stream decoders write, the phone's voice output
/// reads.
pub(crate) static BUS: MixBus = MixBus::new();

struct Source {
    consumer: Consumer<f32>,
    /// Playing (prebuffered); false until [`PREBUFFER`] is queued.
    primed: bool,
    /// Playing faster to bring the queue back down (see the module docs).
    catching_up: bool,
    /// The last sample consumed, and where past it the next one played
    /// sits (0 < `phase` ≤ 1 of a sample; 1 is the first one queued, as
    /// in plain reading): the faster reading interpolates between
    /// samples.
    prev: f32,
    phase: f32,
}

/// Stream sources and the voice outputs mixing them.
pub(crate) struct MixBus {
    sources: Mutex<Vec<Source>>,
    /// Voice outputs on the call path whose worker is running (a count,
    /// so the next output starting before the last one stops is no gap).
    mixers: AtomicUsize,
}

impl MixBus {
    pub(crate) const fn new() -> Self {
        Self {
            sources: Mutex::new(Vec::new()),
            mixers: AtomicUsize::new(0),
        }
    }

    /// Whether a voice output is mixing streams now.
    pub(crate) fn is_mixing(&self) -> bool {
        self.mixers.load(Ordering::Acquire) > 0
    }

    /// Held by a voice output's worker for as long as it mixes.
    pub(crate) fn mixer(&self) -> Mixer<'_> {
        self.mixers.fetch_add(1, Ordering::AcqRel);
        Mixer(self)
    }

    /// A new source; it plays once its first [`PREBUFFER`] is queued and
    /// leaves the bus when dropped.
    pub(crate) fn source(&self) -> MixSource {
        let (producer, consumer) = RingBuffer::new(CAPACITY);
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Source {
                consumer,
                primed: false,
                catching_up: false,
                prev: 0.0,
                phase: 1.0,
            });
        MixSource { producer }
    }

    /// Add every playing source to `out` (48 kHz mono). Never waits: if a
    /// decoder is registering a source this very moment, this hop goes
    /// without stream audio.
    pub(crate) fn mix_into(&self, out: &mut [f32]) {
        let mut sources = match self.sources.try_lock() {
            Ok(sources) => sources,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        // A decoder that let go (the viewer closed, the stream moved to an
        // output of its own) is done: its tail would only play late — a
        // source left unprimed would never even drain.
        sources.retain(|s| !s.consumer.is_abandoned());
        for source in sources.iter_mut() {
            source.mix_into(out);
        }
    }

    #[cfg(test)]
    fn source_count(&self) -> usize {
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl Source {
    /// This source's next hop, added to `out`.
    fn mix_into(&mut self, out: &mut [f32]) {
        let queued = self.consumer.slots();
        if queued > MAX_FILL {
            if let Ok(chunk) = self.consumer.read_chunk(queued - TRIM_TO) {
                chunk.commit_all();
            }
            self.phase = 1.0;
        }
        let queued = self.consumer.slots();
        if !self.primed {
            if queued < PREBUFFER {
                return;
            }
            self.primed = true;
        }
        if queued > CATCH_UP_START {
            self.catching_up = true;
        }
        #[allow(clippy::cast_precision_loss)] // a hop
        let len = out.len() as f32;
        if self.catching_up {
            if queued > CATCH_UP_STOP {
                #[allow(clippy::cast_precision_loss)] // samples, < 2^24
                let excess = (queued - CATCH_UP_STOP) as f32 / crate::OPUS_SAMPLE_RATE as f32;
                let speedup = (excess * SPEEDUP_PER_SECOND).clamp(MIN_SPEEDUP, MAX_SPEEDUP);
                self.read_faster(out, self.phase + len * (1.0 + speedup), queued);
                return;
            }
            // Back down. This last hop ends on a whole sample (it is at
            // most 1/480 faster), so the plain reading below carries on
            // from exactly there.
            self.catching_up = false;
            if self.phase < 1.0 && queued > out.len() + 1 {
                self.read_faster(out, (self.phase + len).ceil(), queued);
                return;
            }
        }
        self.phase = 1.0;
        let n = queued.min(out.len());
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            let (first, second) = chunk.as_slices();
            for (slot, &sample) in out.iter_mut().zip(first.iter().chain(second)) {
                *slot += sample;
            }
            if let Some(&last) = second.last().or(first.last()) {
                self.prev = last;
            }
            chunk.commit_all();
        }
        if n < out.len() {
            // Ran dry: wait for a cushion again rather than play
            // every arriving packet as a click.
            self.primed = false;
        }
    }

    /// One hop played from positions `phase` up to (not including)
    /// `end`, evenly spaced, by linear interpolation between the
    /// samples: `prev` is position 0, the queue's samples 1, 2, ….
    /// Afterwards the sample just before `end` is the new `prev`. Only
    /// while far more is queued than a hop needs (above
    /// [`CATCH_UP_STOP`]), so it does not run dry here.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // positions within one hop
    fn read_faster(&mut self, out: &mut [f32], end: f32, queued: usize) {
        let step = (end - self.phase) / out.len() as f32;
        // Consumed: every sample before `end`.
        let used = (end.ceil() as usize).saturating_sub(1).min(queued);
        // Interpolating up to `end` reads one sample further.
        let peek = (used + 1).min(queued);
        let Ok(chunk) = self.consumer.read_chunk(peek) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        let sample = |i: usize| -> f32 {
            match i {
                0 => self.prev,
                i if i - 1 < first.len() => first[i - 1],
                i => second.get(i - 1 - first.len()).copied().unwrap_or(0.0),
            }
        };
        for (i, slot) in out.iter_mut().enumerate() {
            let pos = self.phase + i as f32 * step;
            let k = pos.floor();
            let frac = pos - k;
            let k = k as usize;
            *slot += sample(k) * (1.0 - frac) + sample((k + 1).min(peek)) * frac;
        }
        let prev = sample(used);
        chunk.commit(used);
        self.prev = prev;
        self.phase = end - used as f32;
    }
}

/// A voice output mixing streams; [`MixBus::is_mixing`] while alive.
pub(crate) struct Mixer<'a>(&'a MixBus);

impl Drop for Mixer<'_> {
    fn drop(&mut self) {
        self.0.mixers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One stream decoder's way onto the bus.
pub(crate) struct MixSource {
    producer: Producer<f32>,
}

impl MixSource {
    /// Queue 48 kHz mono samples; what does not fit (nobody reading) is
    /// dropped.
    pub(crate) fn push(&mut self, samples: &[f32]) {
        let _ = self.producer.push_partial_slice(samples);
    }

    /// Samples queued, not yet mixed.
    pub(crate) fn queued(&self) -> usize {
        CAPACITY - self.producer.slots()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop() -> Vec<f32> {
        vec![0.0; 480]
    }

    #[test]
    fn a_source_waits_for_its_cushion_then_adds_to_the_mix() {
        let bus = MixBus::new();
        let mut source = bus.source();
        source.push(&vec![0.25; PREBUFFER - 1]);
        let mut out = vec![0.5; 480];
        bus.mix_into(&mut out);
        assert!(
            out.iter().all(|&s| (s - 0.5).abs() < 1e-6),
            "not primed yet"
        );

        source.push(&[0.25]);
        bus.mix_into(&mut out);
        assert!(out.iter().all(|&s| (s - 0.75).abs() < 1e-6), "added on top");
    }

    #[test]
    fn running_dry_rebuffers_instead_of_clicking() {
        let bus = MixBus::new();
        let mut source = bus.source();
        source.push(&vec![0.1; PREBUFFER]);
        let mut out = hop();
        // The cushion, then one hop that finds nothing.
        for _ in 0..=PREBUFFER / 480 {
            out.fill(0.0);
            bus.mix_into(&mut out);
        }
        // Dry now; one packet's worth is not enough to restart.
        source.push(&vec![0.1; 960]);
        out.fill(0.0);
        bus.mix_into(&mut out);
        assert!(out.iter().all(|&s| s.abs() < 1e-9));
    }

    #[test]
    fn a_huge_backlog_is_still_trimmed_at_once() {
        let bus = MixBus::new();
        let mut source = bus.source();
        source.push(&vec![0.1; MAX_FILL + 2_000]);
        let mut out = hop();
        bus.mix_into(&mut out);
        let left = bus.sources.lock().unwrap()[0].consumer.slots();
        // Trimmed, then one hop read a little faster than it plays.
        assert!((TRIM_TO - 500..TRIM_TO - 480).contains(&left), "{left}");
    }

    fn queued(bus: &MixBus) -> usize {
        bus.sources.lock().unwrap()[0].consumer.slots()
    }

    /// The A07's case: one burst left 280 ms queued. It comes back down
    /// to the stop level by playing faster — no jump — while new audio
    /// keeps arriving at the normal pace, and without skipping or
    /// repeating anything, through the change of pace at either end.
    #[test]
    fn a_backlog_drains_by_playing_faster_not_by_a_jump() {
        const STEP: f32 = 1e-5;
        let bus = MixBus::new();
        let mut source = bus.source();
        // A ramp, so a dropped stretch would show as a jump in the
        // output and a repeated sample as a flat step. (Counted, not
        // summed, so f32 rounding stays well under a step.)
        let mut fed = 0u32;
        let mut feed = |source: &mut MixSource, n: usize| {
            let chunk: Vec<f32> = (0..n)
                .map(|_| {
                    fed += 1;
                    #[allow(clippy::cast_precision_loss)] // < 2^24
                    let v = fed as f32 * STEP;
                    v
                })
                .collect();
            source.push(&chunk);
        };
        let mut out = hop();
        let mut last: Option<f32> = None;
        let mut check = |out: &[f32], hops: usize| {
            for &s in out {
                if let Some(prev) = last {
                    let step = (s - prev) / STEP;
                    // A skipped sample is 2 steps, a repeated one 0; the
                    // catch-up is at most 1.04. The slack is rounding.
                    assert!(
                        (0.8..1.25).contains(&step),
                        "a step of {step} after {hops} hops"
                    );
                }
                last = Some(s);
            }
        };
        // Steady first, at the normal level.
        feed(&mut source, PREBUFFER);
        for hops in 0..20 {
            feed(&mut source, 480);
            out.fill(0.0);
            bus.mix_into(&mut out);
            check(&out, hops);
        }
        feed(&mut source, 13_440 - PREBUFFER); // the burst: 280 ms now
        let catching_up = |bus: &MixBus| bus.sources.lock().unwrap()[0].catching_up;
        let mut hops = 0;
        loop {
            feed(&mut source, 480); // the stream keeps coming, 10 ms a hop
            out.fill(0.0);
            bus.mix_into(&mut out);
            check(&out, hops);
            hops += 1;
            if hops == 1 {
                assert!(catching_up(&bus), "280 ms is a backlog");
            } else if !catching_up(&bus) {
                break;
            }
            assert!(hops < 2_000, "never caught up");
        }
        assert!(queued(&bus) <= CATCH_UP_STOP);
        // 160 ms gone in ~10 s, not in one go: 4 % at most is 400 hops.
        assert!(hops < 1_200, "took {hops} hops");
        assert!(hops > 400, "too abrupt: {hops} hops");
        // Back at the normal pace: a hop in, a hop out, still seamless.
        let level = queued(&bus);
        for hops in 0..100 {
            feed(&mut source, 480);
            out.fill(0.0);
            bus.mix_into(&mut out);
            check(&out, hops);
        }
        assert_eq!(queued(&bus), level);
    }

    #[test]
    fn two_sources_sum() {
        let bus = MixBus::new();
        let mut a = bus.source();
        let mut b = bus.source();
        a.push(&vec![0.1; PREBUFFER]);
        b.push(&vec![0.2; PREBUFFER]);
        let mut out = hop();
        bus.mix_into(&mut out);
        assert!(out.iter().all(|&s| (s - 0.3).abs() < 1e-6));
    }

    #[test]
    fn a_dropped_source_leaves_at_once() {
        let bus = MixBus::new();
        // Playing, and waiting for its cushion: both go with the decoder.
        let mut playing = bus.source();
        playing.push(&vec![0.1; PREBUFFER]);
        let mut waiting = bus.source();
        waiting.push(&vec![0.1; 960]);
        let mut out = hop();
        bus.mix_into(&mut out);
        assert_eq!(bus.source_count(), 2);
        drop(playing);
        drop(waiting);
        out.fill(0.0);
        bus.mix_into(&mut out);
        assert_eq!(bus.source_count(), 0);
        assert!(out.iter().all(|&s| s.abs() < 1e-9), "nothing late");
    }

    #[test]
    fn mixing_lasts_while_a_mixer_is_held() {
        let bus = MixBus::new();
        assert!(!bus.is_mixing());
        let first = bus.mixer();
        let second = bus.mixer();
        drop(first);
        assert!(bus.is_mixing(), "the next output overlapping the last");
        drop(second);
        assert!(!bus.is_mixing());
    }
}
