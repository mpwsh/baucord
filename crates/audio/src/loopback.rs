//! A microphone test's playback, held back where it has to be.
//!
//! The test plays what the microphone hears. Played at once through a
//! phone's loudspeaker, that asks the echo canceller for the one thing
//! it cannot do: take the user's voice out of the microphone while the
//! user is still saying the same words into it. It cancels part of the
//! live voice with the played one, the suppressor behind it opens and
//! shuts, and the result sounds about to feed back (first phone test,
//! 2026-10-12). Held back by [`LOOPBACK_DELAY`], the words come out
//! after they were said: the canceller has a far end that is not the
//! near end, and the user hears what the others would.
//!
//! The desktop keeps the live monitor it had: its users are mostly on
//! headsets, where there is no loop to break.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::PcmFrame;

/// How long a microphone test holds the user's voice before playing it:
/// half a second on a phone, nothing on the desktop. It was a second
/// at first, which is long enough to be sure of and felt like waiting
/// (the user, 2026-10-12); half is still past the end of a short word.
/// If the hollow sound comes back on some phone, this is the number.
pub const LOOPBACK_DELAY: Duration = if crate::RELEASES_MIC {
    Duration::from_millis(500)
} else {
    Duration::ZERO
};

/// Frames a second (20 ms each).
const FRAMES_PER_SECOND: usize = 50;
/// Frames held beyond the delay's worth before the oldest is dropped: a
/// worker that stalled plays what is current when it comes back.
const SLACK_FRAMES: usize = 10;

/// The frames of a test on their way to the mix, each stamped with when
/// it was heard. Filled and emptied by the output worker, which wakes
/// with every device callback whether a frame arrived or not — so the
/// end of what was said still plays after the last frame came in.
pub struct LoopbackDelay {
    delay: Duration,
    held: VecDeque<(Instant, PcmFrame)>,
    cap: usize,
}

impl LoopbackDelay {
    /// All the room it will ever need, allocated here: `push` runs on
    /// an audio thread.
    #[must_use]
    pub fn new(delay: Duration) -> Self {
        let frames = usize::try_from(delay.as_millis()).unwrap_or(usize::MAX) / 20;
        let cap = frames
            .saturating_add(SLACK_FRAMES)
            .min(60 * FRAMES_PER_SECOND);
        Self {
            delay,
            held: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// A frame the microphone heard at `now`.
    pub fn push(&mut self, frame: &PcmFrame, now: Instant) {
        if self.held.len() == self.cap {
            self.held.pop_front();
        }
        self.held.push_back((now, *frame));
    }

    /// The next frame whose time has come at `now`, oldest first.
    pub fn pop_due(&mut self, now: Instant) -> Option<PcmFrame> {
        let (heard, _) = self.held.front()?;
        if now.saturating_duration_since(*heard) < self.delay {
            return None;
        }
        self.held.pop_front().map(|(_, frame)| frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SIZE;

    fn frame(mark: f32) -> PcmFrame {
        [mark; FRAME_SIZE]
    }

    /// What comes out at `now`, as the mark its frame was made with.
    fn due(line: &mut LoopbackDelay, now: Instant) -> Option<f32> {
        line.pop_due(now).map(|f| f[0])
    }

    #[test]
    fn no_delay_is_the_live_monitor_it_was() {
        let mut line = LoopbackDelay::new(Duration::ZERO);
        let now = Instant::now();
        line.push(&frame(1.0), now);
        line.push(&frame(2.0), now);
        assert_eq!(due(&mut line, now), Some(1.0));
        assert_eq!(due(&mut line, now), Some(2.0));
        assert_eq!(due(&mut line, now), None);
    }

    #[test]
    fn a_frame_plays_when_its_delay_has_passed_and_not_before() {
        let delay = Duration::from_secs(1);
        let mut line = LoopbackDelay::new(delay);
        let t0 = Instant::now();
        let step = Duration::from_millis(20);
        line.push(&frame(1.0), t0);
        line.push(&frame(2.0), t0 + step);
        assert_eq!(due(&mut line, t0 + Duration::from_millis(980)), None);
        assert_eq!(due(&mut line, t0 + delay), Some(1.0));
        assert_eq!(
            due(&mut line, t0 + delay),
            None,
            "the second is 20 ms younger"
        );
        assert_eq!(due(&mut line, t0 + delay + step), Some(2.0));
    }

    /// The test was stopped: nothing new arrives, and what was said in
    /// its last second still comes out, on time, with the line empty
    /// for the next test.
    #[test]
    fn the_tail_plays_out_with_no_new_frames() {
        let delay = Duration::from_millis(200);
        let mut line = LoopbackDelay::new(delay);
        let t0 = Instant::now();
        for i in 0..5u8 {
            line.push(
                &frame(f32::from(i)),
                t0 + Duration::from_millis(20) * u32::from(i),
            );
        }
        let mut out = Vec::new();
        while let Some(m) = due(&mut line, t0 + Duration::from_secs(1)) {
            out.push(m);
        }
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(due(&mut line, t0 + Duration::from_mins(1)), None);
    }

    #[test]
    fn a_stalled_worker_loses_the_oldest_not_the_newest() {
        let delay = Duration::from_millis(100);
        let mut line = LoopbackDelay::new(delay);
        let t0 = Instant::now();
        // Five frames of delay and ten of slack fit; forty were pushed.
        for i in 0..40u8 {
            line.push(&frame(f32::from(i)), t0);
        }
        assert_eq!(due(&mut line, t0 + delay), Some(25.0));
    }
}
