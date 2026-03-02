//! Lock-free voice reference ring buffer.
//!
//! Shares the voice output mix with echo cancellers across threads.
//! Single producer (output worker), multiple consumers (mic echo
//! canceller, stream echo canceller) that read delay-compensated
//! chunks via random access.
//!
//! ## Thread safety
//!
//! The buffer stores f32 samples as `AtomicU32` (bit-cast). The writer
//! advances `write_cursor` after writing all samples in a batch, giving
//! readers a consistent snapshot. Readers only read — no contention.
//!
//! This is NOT sequentially consistent — a reader may see a slightly
//! stale cursor. At 48 kHz with 10 ms writes, staleness is at most
//! one hop (480 samples / 10 ms), which is well within the delay
//! estimation tolerance.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Lock-free ring buffer for sharing voice reference audio.
///
/// Capacity should be at least 200 ms at 48 kHz (9600 samples) to
/// cover the longest expected echo delay (mic in a large room) plus
/// the FFT window size.
pub struct VoiceReferenceRing {
    /// Sample storage — f32 bits stored as AtomicU32.
    buffer: Box<[AtomicU32]>,
    /// Total ring capacity in samples.
    capacity: usize,
    /// Write position (monotonically increasing, wraps via modulo).
    /// Only the writer thread advances this.
    write_cursor: AtomicUsize,
}

impl VoiceReferenceRing {
    /// Create a new reference ring with the given capacity in samples.
    ///
    /// Recommended: `(sample_rate * 200) / 1000` for 200 ms at 48 kHz.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Ring capacity must be > 0");

        let buffer: Box<[AtomicU32]> = (0..capacity)
            .map(|_| AtomicU32::new(0.0_f32.to_bits()))
            .collect();

        Self {
            buffer,
            capacity,
            write_cursor: AtomicUsize::new(0),
        }
    }

    /// Push a slice of samples into the ring.
    ///
    /// Called by the output worker every hop (480 samples / 10 ms).
    /// Advances the write cursor after all samples are written so
    /// readers see a consistent batch.
    pub fn push(&self, samples: &[f32]) {
        let cursor = self.write_cursor.load(Ordering::Relaxed);

        for (i, &s) in samples.iter().enumerate() {
            let idx = (cursor + i) % self.capacity;
            self.buffer[idx].store(s.to_bits(), Ordering::Relaxed);
        }

        // Advance cursor AFTER writing — readers that see the new cursor
        // are guaranteed to see the new samples (on the same thread; cross-
        // thread visibility is relaxed but the 10 ms write interval gives
        // ample time for store buffers to drain).
        self.write_cursor
            .store(cursor + samples.len(), Ordering::Release);
    }

    /// Read `count` samples from `delay_samples` ago into `out`.
    ///
    /// The read position is `write_cursor - delay_samples - count`,
    /// giving the echo canceller a time-aligned reference chunk.
    ///
    /// If the requested range extends beyond available data (ring hasn't
    /// been filled yet), the missing samples are zero-filled.
    ///
    /// Returns the number of valid (non-zero-filled) samples read.
    pub fn read_delayed(&self, delay_samples: usize, out: &mut [f32]) -> usize {
        let count = out.len();
        let cursor = self.write_cursor.load(Ordering::Acquire);

        // Total samples ever written (cursor is monotonic)
        let total_written = cursor;

        // Where we want to start reading
        let read_end = cursor.saturating_sub(delay_samples);
        let read_start = read_end.saturating_sub(count);

        // How many valid samples are available?
        // If the ring hasn't filled yet, some of the requested range
        // predates any writes.
        let valid_start = if total_written >= self.capacity {
            // Ring has wrapped — oldest available is cursor - capacity
            cursor.saturating_sub(self.capacity)
        } else {
            0
        };

        let actual_start = read_start.max(valid_start);
        let valid_count = read_end.saturating_sub(actual_start).min(count);
        let zero_prefix = count - valid_count;

        // Zero-fill the prefix (data not yet available)
        out[..zero_prefix].fill(0.0);

        // Read valid samples
        for i in 0..valid_count {
            let idx = (actual_start + i) % self.capacity;
            let bits = self.buffer[idx].load(Ordering::Relaxed);
            out[zero_prefix + i] = f32::from_bits(bits);
        }

        valid_count
    }

    /// Current write cursor position (for diagnostics).
    pub fn samples_written(&self) -> usize {
        self.write_cursor.load(Ordering::Relaxed)
    }

    /// Ring capacity in samples.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

// Safety: all fields are atomic, no interior mutability beyond atomics.
unsafe impl Send for VoiceReferenceRing {}
unsafe impl Sync for VoiceReferenceRing {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_read_basic() {
        let ring = VoiceReferenceRing::new(1024);

        // Push a known pattern
        let samples: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        ring.push(&samples);

        // Read it back with zero delay
        let mut out = vec![0.0f32; 480];
        let valid = ring.read_delayed(0, &mut out);

        assert_eq!(valid, 480);
        for (i, &s) in out.iter().enumerate() {
            assert!(
                (s - samples[i]).abs() < 1e-6,
                "Sample {i}: expected {}, got {s}",
                samples[i]
            );
        }
    }

    #[test]
    fn read_with_delay() {
        let ring = VoiceReferenceRing::new(4800);

        // Push two batches
        let batch1: Vec<f32> = (0..480).map(|i| i as f32).collect();
        let batch2: Vec<f32> = (480..960).map(|i| i as f32).collect();
        ring.push(&batch1);
        ring.push(&batch2);

        // Read 480 samples from 480 samples ago → should get batch1
        let mut out = vec![0.0f32; 480];
        let valid = ring.read_delayed(480, &mut out);

        assert_eq!(valid, 480);
        for (i, &s) in out.iter().enumerate() {
            assert!(
                (s - batch1[i]).abs() < 1e-6,
                "Sample {i}: expected {}, got {s}",
                batch1[i]
            );
        }
    }

    #[test]
    fn read_before_fill_zero_pads() {
        let ring = VoiceReferenceRing::new(4800);

        // Push only 240 samples
        let samples: Vec<f32> = (0..240).map(|i| i as f32).collect();
        ring.push(&samples);

        // Try to read 480 — first 240 should be zeros
        let mut out = vec![0.0f32; 480];
        let valid = ring.read_delayed(0, &mut out);

        assert_eq!(valid, 240);
        // First 240 are zero-filled
        for &s in &out[..240] {
            assert_eq!(s, 0.0);
        }
        // Last 240 are the real samples
        for (i, &s) in out[240..].iter().enumerate() {
            assert!(
                (s - samples[i]).abs() < 1e-6,
                "Sample {i}: expected {}, got {s}",
                samples[i]
            );
        }
    }

    #[test]
    fn wraps_around_correctly() {
        let ring = VoiceReferenceRing::new(960); // small ring

        // Push 3 batches (overflows capacity)
        let batch1: Vec<f32> = vec![1.0; 480];
        let batch2: Vec<f32> = vec![2.0; 480];
        let batch3: Vec<f32> = vec![3.0; 480];
        ring.push(&batch1);
        ring.push(&batch2);
        ring.push(&batch3); // overwrites batch1

        // Read latest 480 → should get batch3
        let mut out = vec![0.0f32; 480];
        ring.read_delayed(0, &mut out);
        assert!(out.iter().all(|&s| (s - 3.0).abs() < 1e-6));

        // Read 480 samples from 480 ago → should get batch2
        ring.read_delayed(480, &mut out);
        assert!(out.iter().all(|&s| (s - 2.0).abs() < 1e-6));
    }
}
