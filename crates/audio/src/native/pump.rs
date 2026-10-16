//! The real-time halves every voice backend shares: one capture
//! buffer's samples, downmixed, into the ring; one render buffer filled
//! from the ring with the voice on the front pair — the `native/mod.rs`
//! contract. The callback backends (`aaudio` on Android, `vpio` on
//! iOS), Windows' `wasapi` loops and the cpal fallbacks in `input.rs`
//! and `output.rs` all go through here; the downmix and the fan-out
//! existed three times, and had drifted (review 2026-10, P2-49). They
//! run on the platform's real-time audio thread: no locks, no
//! allocation, no logging.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use rtrb::{Consumer, Producer};

use crate::{input_config::InputConfig, latch::Ring, stats::AudioStats};

/// Largest render callback served in one piece from the scratch
/// buffer; bigger ones are served in pieces.
const RENDER_SCRATCH: usize = 8192;

/// What a capture callback needs: the ring to the capture worker and
/// its wake latch.
pub struct CapturePump {
    pub producer: Producer<f32>,
    pub wake_tx: Ring,
    pub stats: Arc<AudioStats>,
    pub config: Arc<InputConfig>,
}

impl CapturePump {
    /// `frames` mono samples from `mono` (one a frame) into the ring,
    /// then ping the worker. Nothing while silenced (muted, out of a
    /// room); what does not fit is an overrun.
    pub fn push_mono(&mut self, frames: usize, mono: impl Iterator<Item = f32>) {
        if self.config.silenced() {
            return;
        }
        let want = frames.min(self.producer.slots());
        if want < frames {
            self.stats.input_overruns.fetch_add(1, Ordering::Relaxed);
        }
        if want > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(want)
        {
            // No clamp: f32 has headroom, and the processing chain
            // (gate → AGC → compressor → clamp) owns levels.
            chunk.fill_from_iter(mono.take(want));
        }
        self.wake_tx.ring();
    }

    /// One callback's worth of interleaved `f32` (`channels` per frame)
    /// → mono into the ring, then ping the worker (the phones' backends).
    #[cfg_attr(
        not(any(target_os = "android", target_os = "ios", test)),
        allow(dead_code)
    )]
    pub fn push(&mut self, data: &[f32], channels: usize) {
        let ch = channels.max(1);
        #[allow(clippy::cast_precision_loss)]
        let mono = data
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32);
        self.push_mono(data.len() / ch, mono);
    }
}

/// What a render callback needs: the ring from the output worker, its
/// wake latch, the stats, and where to report the callback size (the
/// worker keeps one callback + margin queued, D24).
pub struct RenderPump {
    consumer: Consumer<f32>,
    wake_tx: Ring,
    stats: Arc<AudioStats>,
    /// `None` for a backend whose worker sizes its queue another way
    /// (WASAPI, whose "callback" is whatever space the engine has).
    callback_frames: Option<Arc<AtomicUsize>>,
    largest_callback: usize,
    scratch: Vec<f32>,
}

impl RenderPump {
    /// Allocates the scratch buffer: build it off the real-time thread.
    /// Starts from what `callback_frames` already says (a backend that
    /// knows its burst seeds it), so it only ever grows.
    pub fn new(
        consumer: Consumer<f32>,
        wake_tx: Ring,
        stats: Arc<AudioStats>,
        callback_frames: Option<Arc<AtomicUsize>>,
    ) -> Self {
        let largest_callback = callback_frames
            .as_ref()
            .map_or(0, |frames| frames.load(Ordering::Relaxed));
        Self {
            consumer,
            wake_tx,
            stats,
            callback_frames,
            largest_callback,
            scratch: vec![0.0; RENDER_SCRATCH],
        }
    }

    /// `frames` samples from the mono ring, each handed to `write` with
    /// its frame index — what the ring cannot supply is an underrun and
    /// plays as silence — then the output meter, and a ping for the
    /// worker to refill. The signal was limited to ±0.99 upstream: no
    /// clamp here.
    pub fn pull_mono(&mut self, frames: usize, mut write: impl FnMut(usize, f32)) {
        // Tell the worker when the device starts asking for more.
        if let Some(callback_frames) = &self.callback_frames
            && frames > self.largest_callback
        {
            self.largest_callback = frames;
            callback_frames.store(frames, Ordering::Relaxed);
        }
        let mut rms_acc = 0.0f32;
        let mut done = 0usize;
        while done < frames {
            let want = (frames - done).min(self.scratch.len());
            let mono = &mut self.scratch[..want];
            let (_, missing) = self.consumer.pop_partial_slice(mono);
            if !missing.is_empty() {
                self.stats.output_underruns.fetch_add(1, Ordering::Relaxed);
                missing.fill(0.0);
            }
            for (i, &s) in mono.iter().enumerate() {
                rms_acc += s * s;
                write(done + i, s);
            }
            done += want;
        }
        if frames > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rms = (rms_acc / frames as f32).sqrt();
            self.stats
                .output_level
                .store((rms * 3.0).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
        self.wake_tx.ring();
    }

    /// Fill one callback's interleaved `f32` buffer (`channels` per
    /// frame) from the mono ring, then ping the worker (the phones'
    /// backends).
    #[cfg_attr(
        not(any(target_os = "android", target_os = "ios", test)),
        allow(dead_code)
    )]
    pub fn pull(&mut self, out: &mut [f32], channels: usize) {
        let ch = channels.max(1);
        self.pull_mono(out.len() / ch, |i, s| {
            front_pair(&mut out[i * ch..(i + 1) * ch], s, 0.0);
        });
    }
}

/// One mono sample onto one output frame: front left and right carry
/// it, every other channel `silence`. On 5.1/7.1 endpoints (gaming
/// headsets with virtual surround report 8 channels) the others are
/// centre, LFE, rear and side, and a copy of the voice there makes the
/// driver's binaural fold-down bass-heavy and smeared. Standard channel
/// order puts FL, FR first on WASAPI, Core Audio, ALSA and the phones.
pub fn front_pair<T: Copy>(frame: &mut [T], sample: T, silence: T) {
    let (front, rest) = frame.split_at_mut(frame.len().min(2));
    front.fill(sample);
    rest.fill(silence);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pumps(capacity: usize) -> (CapturePump, RenderPump, Arc<AudioStats>) {
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        let stats = Arc::new(AudioStats::new());
        let (wake_tx, _wake_rx) = crate::latch::latch();
        let capture = CapturePump {
            producer,
            wake_tx: wake_tx.clone(),
            stats: stats.clone(),
            config: Arc::new(InputConfig::default()),
        };
        let render = RenderPump::new(consumer, wake_tx, stats.clone(), None);
        (capture, render, stats)
    }

    #[test]
    fn a_stereo_capture_is_averaged_and_played_on_the_front_pair() {
        let (mut capture, mut render, stats) = pumps(16);
        capture.push(&[0.2, 0.4, -0.6, -0.2], 2);
        // Five channels out: the voice on the first two, silence after;
        // the third frame is missing.
        let mut out = [9.0f32; 15];
        render.pull(&mut out, 5);
        let expected = [
            0.3, 0.3, 0.0, 0.0, 0.0, -0.4, -0.4, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        ];
        for (got, want) in out.iter().zip(expected) {
            assert!((got - want).abs() < 1e-6, "{out:?}");
        }
        assert_eq!(stats.output_underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_full_ring_is_an_overrun_and_silence_pushes_nothing() {
        let (mut capture, _render, stats) = pumps(2);
        capture.push(&[0.1, 0.2, 0.3], 1);
        assert_eq!(stats.input_overruns.load(Ordering::Relaxed), 1);
        assert_eq!(capture.producer.slots(), 0);
        let (mut capture, _render, _) = pumps(4);
        capture.config.muted.store(true, Ordering::Relaxed);
        capture.push(&[0.1, 0.2], 1);
        assert_eq!(capture.producer.slots(), 4);
    }

    #[test]
    fn a_mono_output_takes_the_sample() {
        let mut frame = [0i16; 1];
        front_pair(&mut frame, 7, 0);
        assert_eq!(frame, [7]);
    }
}
