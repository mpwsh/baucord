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

static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for at least `len` bytes: a recycled one
/// when there is one big enough, else a new one.
#[must_use]
pub fn take(len: usize) -> Vec<u8> {
    let reused = POOL.lock().ok().and_then(|mut pool| {
        let i = pool.iter().position(|b| b.capacity() >= len)?;
        Some(pool.swap_remove(i))
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
    if let Ok(mut pool) = POOL.lock() {
        if pool.len() < KEEP {
            pool.push(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recycled_buffer_comes_back_empty_and_big_enough() {
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
}
