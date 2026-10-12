//! Large frame buffers, recycled between the decoders and whoever shows
//! the picture.
//!
//! A 1080p picture is 3–4 MB of YUV or 8 MB of RGBA. Allocated fresh for
//! every frame — as it was: decoders keep a pool, but the app never gave
//! a buffer back — each one is a new mapping the first write faults in
//! page by page, sixty times a second. Decoders take from here
//! ([`take`]), the consumer gives back when it is done ([`recycle`]),
//! and the same few buffers go round.

use std::sync::Mutex;

/// Buffers kept for reuse. A watched stream has a few in flight
/// (decoded, queued, being converted); more than this is a burst whose
/// extra buffers are better freed.
const KEEP: usize = 6;

struct Pool {
    buffers: Vec<Vec<u8>>,
    /// Who is making or showing pictures now ([`retain`]).
    users: usize,
    /// Whether [`recycle`] keeps what it is given: until the first
    /// [`retain`] (tests, tools), and from then on while anyone uses it.
    keeping: bool,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    buffers: Vec::new(),
    users: 0,
    keeping: true,
});

/// An empty buffer with room for at least `len` bytes: the smallest
/// recycled one that is big enough, else a new one. (It was the first
/// that fit: a 3 MB YUV picture took the 8 MB RGBA buffer, and the next
/// RGBA picture found none and allocated — review P2-43.)
#[must_use]
pub fn take(len: usize) -> Vec<u8> {
    let reused = POOL.lock().ok().and_then(|mut pool| {
        let (i, _) = pool
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.capacity() >= len)
            .min_by_key(|(_, b)| b.capacity())?;
        Some(pool.buffers.swap_remove(i))
    });
    match reused {
        Some(mut buf) => {
            buf.clear();
            buf
        }
        None => Vec::with_capacity(len),
    }
}

/// Give a buffer back for a later [`take`].
pub fn recycle(buf: Vec<u8>) {
    if buf.capacity() == 0 {
        return;
    }
    if let Ok(mut pool) = POOL.lock()
        && pool.keeping
        && pool.buffers.len() < KEEP
    {
        pool.buffers.push(buf);
    }
}

/// Pictures are being made or shown — a share, a watched stream: the
/// pool keeps buffers until the matching [`release`].
pub fn retain() {
    if let Ok(mut pool) = POOL.lock() {
        pool.users += 1;
        pool.keeping = true;
    }
}

/// The other half of [`retain`]. With the last user gone the pool frees
/// what it holds and keeps nothing more — a picture still on its way to
/// the screen is freed when it comes back. The pool used to hold its
/// six largest buffers for the rest of the run: 50 MB after one 1080p
/// stream, 88 MB after a 1440p one (review P2-43).
pub fn release() {
    if let Ok(mut pool) = POOL.lock() {
        pool.users = pool.users.saturating_sub(1);
        if pool.users == 0 {
            pool.keeping = false;
            pool.buffers = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool is the process's: these tests take turns.
    static TURN: Mutex<()> = Mutex::new(());

    #[test]
    fn a_recycled_buffer_comes_back_empty_and_big_enough() {
        let _turn = TURN.lock();
        // Unusual sizes: other tests share the process-wide pool.
        let mut buf = Vec::with_capacity(123_457);
        buf.extend_from_slice(&[7; 1000]);
        let ptr = buf.as_ptr();
        recycle(buf);
        let again = take(123_400);
        assert!(again.is_empty());
        assert!(again.capacity() >= 123_400);
        assert_eq!(again.as_ptr(), ptr, "the same allocation");
    }

    #[test]
    fn the_smallest_buffer_that_fits_is_taken() {
        let _turn = TURN.lock();
        // Smaller than any picture another test recycles; the large one
        // first, where first-fit would have found it.
        let (large, small) = (Vec::with_capacity(5_003), Vec::with_capacity(3_001));
        let (large_ptr, small_ptr): (*const u8, *const u8) = (large.as_ptr(), small.as_ptr());
        recycle(large);
        recycle(small);
        let (first, second) = (take(2_999), take(2_999));
        assert_eq!(first.as_ptr(), small_ptr);
        assert_eq!(second.as_ptr(), large_ptr, "then the next larger");
    }

    #[test]
    fn nothing_is_kept_once_the_last_user_is_gone() {
        let _turn = TURN.lock();
        let held = |cap: usize| {
            let pool = POOL.lock().unwrap();
            pool.buffers.iter().any(|b| b.capacity() == cap)
        };
        // A share and a watched stream.
        retain();
        retain();
        recycle(Vec::with_capacity(7_001));
        release();
        assert!(held(7_001), "one of them is still going");
        release();
        assert!(POOL.lock().unwrap().buffers.is_empty(), "freed at once");
        recycle(Vec::with_capacity(7_001));
        assert!(!held(7_001), "and a late one is not kept");
        retain();
        let buf = Vec::with_capacity(7_001);
        let ptr: *const u8 = buf.as_ptr();
        recycle(buf);
        let again = take(7_000);
        assert_eq!(again.as_ptr(), ptr);
        // As the other tests found it: keeping, nobody counted.
        release();
        POOL.lock().unwrap().keeping = true;
    }
}
