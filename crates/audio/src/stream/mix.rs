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
//! only reader. Each source is read at a steady pace (`stream::pace`):
//! it primes to 100 ms before it plays (and again after running dry), a
//! backlog drains by playing 1–4 % faster instead of the 150 ms jump the
//! old trim made, heard as a skip, and only one past 400 ms is still
//! dropped at once. A source whose decoder let go is dropped at once: a
//! stream that stopped stops.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex, PoisonError, TryLockError,
};

use rtrb::{Producer, RingBuffer};

use crate::stream::pace::{PacedReader, Pacing};

/// What one source may hold: 500 ms at 48 kHz mono.
const CAPACITY: usize = 24_000;

/// The process's bus: stream decoders write, the phone's voice output
/// reads.
pub(crate) static BUS: MixBus = MixBus::new();

/// A stream's ring, read at a steady pace (`stream::pace`).
type Source = PacedReader<1>;

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

    /// A new source; it plays once its first cushion is queued and
    /// leaves the bus when dropped.
    pub(crate) fn source(&self) -> MixSource {
        let (producer, consumer) = RingBuffer::new(CAPACITY);
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Source::new(
                consumer,
                Pacing::at_rate(crate::OPUS_SAMPLE_RATE),
            ));
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
        sources.retain(|s| !s.is_abandoned());
        for source in sources.iter_mut() {
            source.add_to(out);
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

    // The levels at 48 kHz (`pace`'s tests pin them).
    const PREBUFFER: usize = 4_800;
    const CATCH_UP_STOP: usize = 5_760;
    const MAX_FILL: usize = 19_200;
    const TRIM_TO: usize = 9_600;

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
        let left = bus.sources.lock().unwrap()[0].queued();
        // Trimmed, then one hop read a little faster than it plays.
        assert!((TRIM_TO - 500..TRIM_TO - 480).contains(&left), "{left}");
    }

    fn queued(bus: &MixBus) -> usize {
        bus.sources.lock().unwrap()[0].queued()
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
        let catching_up = |bus: &MixBus| bus.sources.lock().unwrap()[0].catching_up();
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
