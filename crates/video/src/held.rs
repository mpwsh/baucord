//! Pictures a decoder has finished but not handed out yet, and when to
//! hand them out.
//!
//! A hardware decoder in poll mode does not finish one picture per frame
//! fed: on a 2026-10-08 Galaxy A07 (`c2.mtk.avc.decoder`) a call often
//! found none and the next one found two. Keeping only the newest threw
//! away 15 % of a 60 fps stream (3,799 of 25,539 pictures) on a phone
//! with half its decode time to spare, and the screen showed ~51 fps.
//!
//! Here every finished picture waits in a short queue and each decode
//! call hands out the oldest one, so pictures leave at the cadence the
//! frames come in — the jitter buffer releases those at the sharer's
//! capture pace — rather than in pairs a screen would show as one. A
//! burst costs at most one frame of latency ([`KEEP`]); a frame that
//! starts falling further behind is passed over, oldest first.
//!
//! When frames stop coming (the sharer's screen went still: capture is
//! change-driven) there is no next call to hand out the last picture,
//! so the decoder's poll does, once the gap is clearly longer than the
//! stream's usual spacing ([`HeldPictures::idle`]).
//!
//! Platform-free so it is tested on the host; `mediacodec` uses it.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{pool, DecodedFrame};

/// Pictures that may wait at once. Two: the one the next call hands
/// out and one more from a burst. A third means the stream is falling
/// behind, and the oldest is passed over.
pub const KEEP: usize = 2;

/// A poll hands out what is waiting once no frame has come for this
/// many times the usual spacing between frames…
const IDLE_SPACINGS: f32 = 1.5;
/// …within these bounds (a 60 fps stream: 25 ms; 30 fps: 50 ms).
const IDLE_MIN: Duration = Duration::from_millis(10);
const IDLE_MAX: Duration = Duration::from_millis(100);
/// Spacing assumed before two frames have been seen (60 fps).
const FIRST_SPACING: Duration = Duration::from_micros(16_667);
/// Gaps longer than this are a pause, not the stream's cadence, and do
/// not count towards its spacing.
const SPACING_CAP: Duration = Duration::from_millis(200);

/// The queue and the stream's cadence. One per decoder.
#[derive(Default)]
pub struct HeldPictures {
    queue: VecDeque<DecodedFrame>,
    /// When the last frame was fed.
    last_call: Option<Instant>,
    /// Smoothed spacing between frames fed; `None` before the second.
    spacing: Option<Duration>,
    /// Pictures passed over for newer ones (the decoder's summary).
    passed_over: u64,
}

impl HeldPictures {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A frame is being fed: note the stream's cadence.
    pub fn on_frame(&mut self, now: Instant) {
        if let Some(last) = self.last_call {
            let gap = now.saturating_duration_since(last);
            if gap <= SPACING_CAP {
                self.spacing = Some(match self.spacing {
                    // An eighth of each new gap: steady through a burst,
                    // following a frame-rate change within a second.
                    Some(s) => (s * 7 + gap) / 8,
                    None => gap,
                });
            }
        }
        self.last_call = Some(now);
    }

    /// A finished picture, in decode order. Past [`KEEP`] the oldest is
    /// passed over (its buffer goes back to the pool).
    pub fn push(&mut self, frame: DecodedFrame) {
        self.queue.push_back(frame);
        while self.queue.len() > KEEP {
            if let Some(old) = self.queue.pop_front() {
                pool::recycle(old.data);
                self.passed_over += 1;
            }
        }
    }

    /// The picture to hand out now: the oldest waiting.
    pub fn hand_out(&mut self) -> Option<DecodedFrame> {
        self.queue.pop_front()
    }

    /// Whether a poll at `now` should hand out what is waiting: no frame
    /// for clearly longer than the stream's usual spacing.
    #[must_use]
    pub fn idle(&self, now: Instant) -> bool {
        self.idle_at().is_some_and(|at| now >= at)
    }

    /// When the stream counts as paused ([`Self::idle`]) if no frame
    /// comes before; `None` before the first frame.
    #[must_use]
    pub fn idle_at(&self) -> Option<Instant> {
        let last = self.last_call?;
        let spacing = self.spacing.unwrap_or(FIRST_SPACING);
        Some(last + spacing.mul_f32(IDLE_SPACINGS).clamp(IDLE_MIN, IDLE_MAX))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    #[must_use]
    pub fn passed_over(&self) -> u64 {
        self.passed_over
    }

    /// Drop everything waiting (the codec is being rebuilt).
    pub fn clear(&mut self) {
        for frame in self.queue.drain(..) {
            pool::recycle(frame.data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PictureFormat;

    /// Picture `n`: the number is its pixels.
    fn picture(n: u64) -> DecodedFrame {
        DecodedFrame {
            data: n.to_le_bytes().to_vec(),
            width: 1,
            height: 2,
            format: PictureFormat::Rgba,
        }
    }

    fn number(p: &DecodedFrame) -> u64 {
        u64::from_le_bytes(p.data[..8].try_into().unwrap())
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The A07's pattern: a call finds nothing, the next finds two.
    /// Every picture still comes out, one per call, in order.
    #[test]
    fn pictures_that_come_out_together_leave_one_per_frame() {
        let mut held = HeldPictures::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let mut finished = 0;
        for call in 0..60u64 {
            held.on_frame(t0 + ms(call * 16));
            // Odd calls find this frame's and the previous one's picture.
            if call % 2 == 1 {
                held.push(picture(finished));
                held.push(picture(finished + 1));
                finished += 2;
            }
            if let Some(p) = held.hand_out() {
                out.push(number(&p));
            }
        }
        assert_eq!(held.passed_over(), 0);
        // All but the one still waiting, in order.
        assert_eq!(out, (0..59).collect::<Vec<_>>());
        assert!(!held.is_empty());
    }

    #[test]
    fn a_stream_falling_behind_passes_over_the_oldest() {
        let mut held = HeldPictures::new();
        for n in 0..4 {
            held.push(picture(n));
        }
        assert_eq!(held.passed_over(), 2);
        assert_eq!(held.hand_out().as_ref().map(number), Some(2));
        assert_eq!(held.hand_out().as_ref().map(number), Some(3));
        assert!(held.hand_out().is_none());
    }

    #[test]
    fn a_pause_is_told_from_the_streams_cadence() {
        let mut held = HeldPictures::new();
        let t0 = Instant::now();
        assert!(!held.idle(t0), "nothing fed yet");
        for i in 0..20 {
            held.on_frame(t0 + ms(i * 33)); // 30 fps
        }
        let last = t0 + ms(19 * 33);
        assert!(!held.idle(last + ms(33)), "one frame late is the cadence");
        assert!(held.idle(last + ms(50)), "1.5 frames late is a pause");
        // A long pause does not stretch the spacing it is judged by.
        held.on_frame(last + ms(5_000));
        assert!(held.idle(last + ms(5_000) + ms(50)));
    }

    #[test]
    fn idle_at_is_when_a_pause_begins() {
        let mut held = HeldPictures::new();
        let t0 = Instant::now();
        assert_eq!(held.idle_at(), None);
        for i in 0..20 {
            held.on_frame(t0 + ms(i * 33));
        }
        let at = held.idle_at().expect("frames were fed");
        assert!(!held.idle(at.checked_sub(Duration::from_micros(1)).unwrap()));
        assert!(held.idle(at));
    }

    #[test]
    fn the_wait_is_bounded_whatever_the_cadence() {
        let mut fast = HeldPictures::new();
        let t0 = Instant::now();
        for i in 0..10 {
            fast.on_frame(t0 + Duration::from_micros(i * 4_000));
        }
        let last = t0 + Duration::from_micros(36_000);
        assert!(!fast.idle(last + ms(9)));
        assert!(fast.idle(last + IDLE_MIN));

        let mut slow = HeldPictures::new();
        for i in 0..10 {
            slow.on_frame(t0 + ms(i * 150));
        }
        assert!(slow.idle(t0 + ms(9 * 150) + IDLE_MAX));
    }
}
