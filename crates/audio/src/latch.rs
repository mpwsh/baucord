//! The wake latch between an audio callback and its worker.
//!
//! A device callback moves samples to or from a ring and then has to
//! tell its worker thread there is something to do. That used to be a
//! `crossbeam_channel::bounded(1)` and a `try_send(())` — a fine latch,
//! except that crossbeam's send, when the receiver is parked (the usual
//! state of a worker between callbacks), locks a `std::sync::Mutex` to
//! find and wake it. A real-time thread taking a lock a normal thread
//! may hold is the priority inversion the rest of the callback goes out
//! of its way to avoid (review P2-33).
//!
//! Here ringing is an atomic swap and, only when that changes the flag,
//! a [`Thread::unpark`] — a futex wake or a semaphore signal, no lock on
//! any platform we build for. Any number of [`Ring`]s (the callback,
//! and the owner's stop), one [`Wait`] on one thread.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    thread::{self, Thread},
    time::{Duration, Instant},
};

struct Shared {
    /// Rung and not yet seen by the waiter.
    rung: AtomicBool,
    /// Live [`Ring`]s: at zero nothing will ring again.
    rings: AtomicUsize,
    /// The waiting thread, set by its first wait…
    waiter: OnceLock<Thread>,
    /// …and that it is set. Its own flag, so that this store and the
    /// waiter's read of `rung` — and a ringer's store to `rung` and its
    /// read of this — are all `SeqCst`: one side always sees the other.
    registered: AtomicBool,
}

impl Shared {
    fn wake(&self) {
        if self.registered.load(Ordering::SeqCst)
            && let Some(waiter) = self.waiter.get()
        {
            waiter.unpark();
        }
    }
}

/// A new latch, not rung.
pub(crate) fn latch() -> (Ring, Wait) {
    let shared = Arc::new(Shared {
        rung: AtomicBool::new(false),
        rings: AtomicUsize::new(1),
        waiter: OnceLock::new(),
        registered: AtomicBool::new(false),
    });
    (Ring(shared.clone()), Wait(shared))
}

/// The ringing side. Clone it for everyone who may wake the worker.
pub(crate) struct Ring(Arc<Shared>);

impl Ring {
    /// Wake the waiter, now or at its next wait. Safe on a real-time
    /// thread: no lock, no allocation. Rings before the waiter looked
    /// collapse into one.
    pub(crate) fn ring(&self) {
        if !self.0.rung.swap(true, Ordering::SeqCst) {
            self.0.wake();
        }
    }
}

impl Clone for Ring {
    fn clone(&self) -> Self {
        self.0.rings.fetch_add(1, Ordering::SeqCst);
        Self(self.0.clone())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // The last one gone: the waiter learns it now, not at its
        // timeout.
        if self.0.rings.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.wake();
        }
    }
}

/// The waiting side: one worker thread.
pub(crate) struct Wait(Arc<Shared>);

impl Wait {
    /// Block until the latch is rung or `timeout` passes, whichever is
    /// first.
    ///
    /// `false` once the latch is gone: every [`Ring`] was dropped (the
    /// stream and its owner) and no ring is left unseen, so nothing
    /// will wake this again. That returns at once, so a stop never
    /// waits out the timeout; the caller leaves its loop.
    #[must_use]
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let shared = &*self.0;
        if !shared.registered.load(Ordering::Relaxed) {
            let _ = shared.waiter.set(thread::current());
            shared.registered.store(true, Ordering::SeqCst);
        }
        debug_assert_eq!(
            shared.waiter.get().map(Thread::id),
            Some(thread::current().id()),
            "a latch has one waiting thread"
        );
        let deadline = Instant::now() + timeout;
        loop {
            if shared.rung.swap(false, Ordering::SeqCst) {
                return true;
            }
            if shared.rings.load(Ordering::SeqCst) == 0 {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            // May return early for no reason, or for somebody else's
            // unpark of this thread: the loop looks again.
            thread::park_timeout(deadline - now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn a_ring_before_the_wait_is_not_lost() {
        let (ring, wait) = latch();
        ring.ring();
        ring.ring();
        let started = Instant::now();
        assert!(wait.wait(LONG));
        assert!(started.elapsed() < Duration::from_secs(5));
        // Two rings were one: the next wait runs to its timeout.
        let started = Instant::now();
        assert!(wait.wait(Duration::from_millis(30)));
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn a_ring_wakes_a_parked_waiter() {
        let (ring, wait) = latch();
        let waiter = thread::spawn(move || {
            let started = Instant::now();
            let mut woken = 0;
            while wait.wait(LONG) {
                woken += 1;
            }
            (woken, started.elapsed())
        });
        for _ in 0..3 {
            thread::sleep(Duration::from_millis(20));
            ring.ring();
        }
        thread::sleep(Duration::from_millis(20));
        drop(ring);
        let (woken, took) = waiter.join().expect("waiter");
        assert!((1..=3).contains(&woken), "woken {woken} times");
        assert!(took < Duration::from_secs(5), "never waited out a timeout");
    }

    #[test]
    fn the_last_ring_dropped_ends_the_wait_after_what_was_rung() {
        let (ring, wait) = latch();
        let stop = ring.clone();
        drop(ring);
        // One ring still lives.
        assert!(wait.wait(Duration::from_millis(5)));
        stop.ring();
        drop(stop);
        assert!(wait.wait(LONG), "the ring left behind is seen first");
        let started = Instant::now();
        assert!(!wait.wait(LONG));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn every_ring_from_a_busy_callback_is_followed_by_a_wake() {
        // A callback ringing as fast as it can against a worker that
        // parks between rings: the worker must see the last ring.
        let (ring, wait) = latch();
        let seen = Arc::new(AtomicUsize::new(0));
        let rung = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let (seen, rung) = (seen.clone(), rung.clone());
            thread::spawn(move || {
                while wait.wait(LONG) {
                    seen.store(rung.load(Ordering::SeqCst), Ordering::SeqCst);
                }
            })
        };
        for i in 1..=20_000 {
            rung.store(i, Ordering::SeqCst);
            ring.ring();
        }
        // The worker catches up with the last ring by itself.
        let started = Instant::now();
        while seen.load(Ordering::SeqCst) != 20_000 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "a ring was lost"
            );
            thread::yield_now();
        }
        drop(ring);
        waiter.join().expect("waiter");
    }
}
