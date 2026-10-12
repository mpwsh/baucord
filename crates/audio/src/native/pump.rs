//! The real-time halves the callback-driven voice backends share
//! (`aaudio` on Android, `vpio` on iOS): one capture callback's samples
//! into the ring, one render callback's buffer out of it — the
//! `native/mod.rs` contract. Both run on the platform's real-time audio
//! thread: no locks, no allocation, no logging.

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
    /// One callback's worth of interleaved `f32` (`channels` per frame)
    /// → mono into the ring, then ping the worker.
    pub fn push(&mut self, data: &[f32], channels: usize) {
        if self.config.silenced() {
            return;
        }
        let ch = channels.max(1);
        let frames = data.len() / ch;
        let want = frames.min(self.producer.slots());
        if want < frames {
            self.stats.input_overruns.fetch_add(1, Ordering::Relaxed);
        }
        if want == 0 {
            return;
        }
        if let Ok(chunk) = self.producer.write_chunk_uninit(want) {
            #[allow(clippy::cast_precision_loss)]
            let mut it = data
                .chunks_exact(ch)
                .take(want)
                .map(|frame| frame.iter().sum::<f32>() / ch as f32);
            chunk.fill_from_iter(&mut it);
        }
        self.wake_tx.ring();
    }
}

/// What a render callback needs: the ring from the output worker, its
/// wake latch, the stats, and where to report the callback size (the
/// worker keeps one callback + margin queued, D24).
pub struct RenderPump {
    consumer: Consumer<f32>,
    wake_tx: Ring,
    stats: Arc<AudioStats>,
    callback_frames: Arc<AtomicUsize>,
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
        callback_frames: Arc<AtomicUsize>,
    ) -> Self {
        let largest_callback = callback_frames.load(Ordering::Relaxed);
        Self {
            consumer,
            wake_tx,
            stats,
            callback_frames,
            largest_callback,
            scratch: vec![0.0; RENDER_SCRATCH],
        }
    }

    /// Fill one callback's interleaved buffer (`channels` per frame)
    /// from the mono ring, then ping the worker.
    pub fn pull(&mut self, out: &mut [f32], channels: usize) {
        let ch = channels.max(1);
        let frames_total = out.len() / ch;
        // Tell the worker when the device starts asking for more.
        if frames_total > self.largest_callback {
            self.largest_callback = frames_total;
            self.callback_frames.store(frames_total, Ordering::Relaxed);
        }
        let mut rms_acc = 0.0f32;
        let mut done = 0usize;
        while done < frames_total {
            let want = (frames_total - done).min(self.scratch.len());
            let mono = &mut self.scratch[..want];
            let (_, missing) = self.consumer.pop_partial_slice(mono);
            if !missing.is_empty() {
                self.stats.output_underruns.fetch_add(1, Ordering::Relaxed);
                missing.fill(0.0);
            }
            for (i, &s) in mono.iter().enumerate() {
                rms_acc += s * s;
                let base = (done + i) * ch;
                // Mono to every channel: a phone's voice path is mono
                // or a stereo pair; there is no surround to protect.
                out[base..base + ch].fill(s);
            }
            done += want;
        }
        if frames_total > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rms = (rms_acc / frames_total as f32).sqrt();
            self.stats
                .output_level
                .store((rms * 3.0).clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
        self.wake_tx.ring();
    }
}
