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
//! stream decoder downmixes to mono and hands its samples here
//! ([`MixSource`]), and that output's worker adds them to every 10 ms hop
//! ([`MixBus::mix_into`]) — after deafen and the voice volume, which a
//! stream ignores as before, and before the limiter. Everywhere else (the
//! desktop; a phone on headphones or with echo cancellation off) the
//! stream keeps its own stereo output (`stream::output`).
//!
//! One SPSC ring per source, all held by [`BUS`]; the voice worker is the
//! only reader. A source primes to [`PREBUFFER`] before it plays (and
//! again after running dry), and never holds more than [`MAX_FILL`]: the
//! sender's clock and the phone's drift apart, and a voice output that
//! reopens stops reading for a moment. A source whose decoder let go is
//! dropped at once: a stream that stopped stops.

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
/// Past this much queued (300 ms) the oldest audio is dropped…
pub(crate) const MAX_FILL: usize = 14_400;
/// …down to this (150 ms).
pub(crate) const TRIM_TO: usize = 7_200;

/// The process's bus: stream decoders write, the phone's voice output
/// reads.
pub(crate) static BUS: MixBus = MixBus::new();

struct Source {
    consumer: Consumer<f32>,
    /// Playing (prebuffered); false until [`PREBUFFER`] is queued.
    primed: bool,
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
            let queued = source.consumer.slots();
            if queued > MAX_FILL {
                if let Ok(chunk) = source.consumer.read_chunk(queued - TRIM_TO) {
                    chunk.commit_all();
                }
            }
            let queued = source.consumer.slots();
            if !source.primed {
                if queued < PREBUFFER {
                    continue;
                }
                source.primed = true;
            }
            let n = queued.min(out.len());
            if let Ok(chunk) = source.consumer.read_chunk(n) {
                let (first, second) = chunk.as_slices();
                for (slot, &sample) in out.iter_mut().zip(first.iter().chain(second)) {
                    *slot += sample;
                }
                chunk.commit_all();
            }
            if n < out.len() {
                // Ran dry: wait for a cushion again rather than play
                // every arriving packet as a click.
                source.primed = false;
            }
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
    fn a_backlog_is_trimmed_to_bounded_latency() {
        let bus = MixBus::new();
        let mut source = bus.source();
        source.push(&vec![0.1; MAX_FILL + 2_000]);
        let mut out = hop();
        bus.mix_into(&mut out);
        let left = bus.sources.lock().unwrap()[0].consumer.slots();
        assert_eq!(left, TRIM_TO - 480);
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
