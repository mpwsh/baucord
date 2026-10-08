//! The stream log: what a viewer writes about a screen share it
//! watches, and a sharer about its own. One summary line per stream
//! every [`SUMMARY_EVERY`] — at warn when something is wrong, info
//! otherwise — and, for a viewer, a line when the picture starts
//! falling behind live, catches up again, or freezes waiting for a
//! keyframe.
//!
//! It replaces once-a-second "Slow decode" / "Slow frames" warnings and
//! per-event lines (a keyframe request every 0.7 s, an audio "seq
//! jumped" per gap, a "Video frame dropped" per abandoned group, an
//! encoder reopen per AMF keyframe). A 2026-10-08 phone log was over
//! 100 lines a minute repeating one fact, and none of them said where a
//! frame's time went, how far behind live the picture was, or how much
//! stream audio was queued: the three things that decide what to fix
//! when a device cannot keep up.
//!
//! Bookkeeping and formatting only, no I/O: `client::screen` feeds it
//! and logs what it returns.

use std::{
    fmt::Write,
    time::{Duration, Instant},
};

use audio::stream::decoder::StreamAudioStats;
use log::Level;
use video::{
    buffer::{JitterBufferStats, VideoJitterBuffer},
    DecodeTiming,
};

/// How often each watched stream gets its summary line.
pub(crate) const SUMMARY_EVERY: Duration = Duration::from_secs(5);
/// A picture this far behind live starts a "falling behind" episode.
/// Normal is the playout delay plus network wobble: tens of ms, at most
/// ~250.
const BEHIND_WARN: Duration = Duration::from_millis(600);
/// An episode ends after this long without a picture that late.
const CAUGHT_UP_AFTER: Duration = Duration::from_secs(10);
/// Frozen (no keyframe to restart from) this long: say so.
const FROZEN_WARN: Duration = Duration::from_secs(3);
/// The decode thread busy this share of the time: it is the limit.
const BUSY_WARN_PCT: u32 = 90;

/// What the UI did with one stream's frames since the last summary:
/// filled by the frontend (its present-prep thread and the UI thread),
/// taken by the summary. Pictures the decoder made but the UI never
/// showed are the difference between `decoded` and `shown`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresentTally {
    /// Frames put on screen.
    pub shown: u32,
    /// Frames passed over for a newer one of the same stream: by the
    /// present thread before it paid to prepare them, or in the UI's
    /// slot before the UI took them.
    pub replaced: u32,
    /// Frames the present thread prepared (converted or copied into
    /// the UI's pixel buffer), and the time that took, summed.
    pub prepared: u32,
    pub prepare_us: u64,
    /// Decoded → on screen, summed and worst.
    pub to_screen_us: u64,
    pub to_screen_max_us: u64,
}

/// Durations over a window: mean and 95th percentile, in ms.
#[derive(Debug, Default)]
struct Samples(Vec<f32>);

impl Samples {
    fn push(&mut self, d: Duration) {
        self.0.push(d.as_secs_f32() * 1000.0);
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[allow(clippy::cast_precision_loss)] // a few hundred samples
    fn mean(&self) -> f32 {
        if self.0.is_empty() {
            return 0.0;
        }
        self.0.iter().sum::<f32>() / self.0.len() as f32
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // an index into a few hundred samples
    fn p95(&mut self) -> f32 {
        if self.0.is_empty() {
            return 0.0;
        }
        self.0.sort_by(f32::total_cmp);
        let i = ((self.0.len() - 1) as f32 * 0.95).round() as usize;
        self.0[i]
    }

    fn max(&self) -> f32 {
        self.0.iter().copied().fold(0.0, f32::max)
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// One watched stream's log state.
pub(crate) struct Viewing {
    peer: u64,
    size: (u32, u32),
    window_start: Instant,
    buffer_base: JitterBufferStats,
    audio_base: Option<StreamAudioStats>,
    /// Time spent in `decode` calls, with or without a picture out.
    busy: Duration,
    pictures: u32,
    codec: Samples,
    output: Samples,
    /// Pictures from backends that do not split their time.
    whole: Samples,
    behind: Samples,
    /// The current falling-behind episode: when it began and the last
    /// picture that was that late.
    falling: Option<(Instant, Instant)>,
    /// Waiting for a keyframe since, and whether that was said.
    frozen: Option<(Instant, bool)>,
}

impl Viewing {
    pub(crate) fn new(peer: u64, now: Instant) -> Self {
        Self {
            peer,
            size: (0, 0),
            window_start: now,
            buffer_base: JitterBufferStats::default(),
            audio_base: None,
            busy: Duration::ZERO,
            pictures: 0,
            codec: Samples::default(),
            output: Samples::default(),
            whole: Samples::default(),
            behind: Samples::default(),
            falling: None,
            frozen: None,
        }
    }

    /// Every `decode` call, picture or not.
    pub(crate) fn on_decode_call(&mut self, took: Duration) {
        self.busy += took;
    }

    /// A call that produced a picture: its split (when the backend
    /// gives one), the whole call, and how far behind live the frame
    /// was once decoded. Returns a line to log when this picture starts
    /// a falling-behind episode.
    pub(crate) fn on_picture(
        &mut self,
        now: Instant,
        timing: Option<DecodeTiming>,
        took: Duration,
        behind_live: Option<Duration>,
        size: (u32, u32),
    ) -> Option<(Level, String)> {
        match timing {
            Some(t) => {
                self.codec.push(t.codec);
                self.output.push(t.output);
            }
            None => self.whole.push(took),
        }
        self.on_held_picture(now, behind_live, size)
    }

    /// A picture the decoder handed out between frames (`poll`: the
    /// stream paused with one still waiting). Counted, with how late it
    /// was; no decode call to time.
    pub(crate) fn on_held_picture(
        &mut self,
        now: Instant,
        behind_live: Option<Duration>,
        size: (u32, u32),
    ) -> Option<(Level, String)> {
        self.pictures += 1;
        self.size = size;
        let behind = behind_live?;
        self.behind.push(behind);
        if behind < BEHIND_WARN {
            return None;
        }
        if let Some((_, last_late)) = self.falling.as_mut() {
            *last_late = now;
            return None;
        }
        self.falling = Some((now, now));
        let busy = self.busy_pct(now);
        Some((
            Level::Warn,
            format!(
                "video from {} falling behind live: {} ms late, decoding busy {busy}% of the time",
                self.peer,
                behind.as_millis()
            ),
        ))
    }

    #[allow(clippy::cast_possible_truncation)] // a percentage
    fn busy_pct(&self, now: Instant) -> u32 {
        let window = now.duration_since(self.window_start).as_secs_f64();
        if window <= 0.0 {
            return 0;
        }
        (self.busy.as_secs_f64() / window * 100.0)
            .round()
            .min(999.0) as u32
    }

    /// Once per drain pass: the episode and freeze lines, and the
    /// summary when one is due. `present` is asked only then.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        buffer: &VideoJitterBuffer,
        audio: Option<StreamAudioStats>,
        present: impl FnOnce() -> Option<PresentTally>,
    ) -> Vec<(Level, String)> {
        let mut lines = Vec::new();
        if let Some((began, last_late)) = self.falling
            && now.duration_since(last_late) >= CAUGHT_UP_AFTER
        {
            self.falling = None;
            lines.push((
                Level::Info,
                format!(
                    "video from {} keeping up with live again (the lag lasted {:.0} s)",
                    self.peer,
                    last_late.duration_since(began).as_secs_f32()
                ),
            ));
        }
        self.track_freeze(now, buffer, &mut lines);
        if now.duration_since(self.window_start) >= SUMMARY_EVERY {
            lines.push(self.summary(now, &buffer.stats, audio, present()));
        }
        lines
    }

    fn track_freeze(
        &mut self,
        now: Instant,
        buffer: &VideoJitterBuffer,
        lines: &mut Vec<(Level, String)>,
    ) {
        let waiting = !buffer.is_primed() && buffer.stats.packets_received > 0;
        match (&mut self.frozen, waiting) {
            (None, true) => self.frozen = Some((now, false)),
            (Some((since, said)), true) => {
                if !*said && now.duration_since(*since) >= FROZEN_WARN {
                    *said = true;
                    lines.push((
                        Level::Warn,
                        format!(
                            "video from {} frozen {:.0} s: no keyframe to restart from \
                             (asked for one {} times so far)",
                            self.peer,
                            now.duration_since(*since).as_secs_f32(),
                            buffer.stats.keyframe_requests
                        ),
                    ));
                }
            }
            (Some((since, said)), false) => {
                if *said {
                    lines.push((
                        Level::Info,
                        format!(
                            "video from {} resumed: a keyframe arrived after {:.1} s",
                            self.peer,
                            now.duration_since(*since).as_secs_f32()
                        ),
                    ));
                }
                self.frozen = None;
            }
            (None, false) => {}
        }
    }

    /// The summary line, and a fresh window.
    fn summary(
        &mut self,
        now: Instant,
        stats: &JitterBufferStats,
        audio: Option<StreamAudioStats>,
        present: Option<PresentTally>,
    ) -> (Level, String) {
        let secs = now
            .duration_since(self.window_start)
            .as_secs_f32()
            .max(0.001);
        let mut line = format!("viewing {}", self.peer);
        if self.size != (0, 0) {
            let _ = write!(line, " {}x{}", self.size.0, self.size.1);
        }
        let trouble = self.write_frames(&mut line, secs, stats, present);
        let busy = self.busy_pct(now);
        self.write_timing(&mut line, busy, present);
        self.write_audio(&mut line, audio);

        let level = if trouble
            || busy >= BUSY_WARN_PCT
            || self.behind.max() >= BEHIND_WARN.as_secs_f32() * 1000.0
        {
            Level::Warn
        } else {
            Level::Info
        };

        self.window_start = now;
        self.buffer_base = stats.clone();
        self.audio_base = audio.or(self.audio_base);
        self.busy = Duration::ZERO;
        self.pictures = 0;
        self.codec.clear();
        self.output.clear();
        self.whole.clear();
        self.behind.clear();
        (level, line)
    }

    /// Frames in, decoded, shown, and what went missing. True when
    /// any did, or the stream is frozen.
    #[allow(clippy::cast_precision_loss)] // counts per second
    fn write_frames(
        &self,
        line: &mut String,
        secs: f32,
        stats: &JitterBufferStats,
        present: Option<PresentTally>,
    ) -> bool {
        let base = &self.buffer_base;
        let arrived = stats.packets_received - base.packets_received;
        let dropped = (stats.packets_dropped_overflow - base.packets_dropped_overflow)
            + (stats.packets_dropped_late - base.packets_dropped_late)
            + (stats.packets_dropped_broken_reference - base.packets_dropped_broken_reference);
        let jumps = stats.keyframe_resets - base.keyframe_resets;
        let lost = stats.gaps_detected - base.gaps_detected;
        let asks = stats.keyframe_requests - base.keyframe_requests;
        let frozen = self.frozen.is_some();

        let _ = write!(
            line,
            ": in {:.1}/s, decoded {:.1}/s",
            arrived as f32 / secs,
            self.pictures as f32 / secs,
        );
        if let Some(p) = present {
            let _ = write!(line, ", shown {:.1}/s", p.shown as f32 / secs);
            if p.replaced > 0 {
                let _ = write!(line, " ({} skipped for a newer one)", p.replaced);
            }
        }
        if dropped > 0 {
            let _ = write!(line, ", dropped {dropped}");
            if jumps > 0 {
                let _ = write!(line, " ({jumps} jumps to a keyframe)");
            }
        }
        if lost > 0 {
            let _ = write!(line, ", lost {lost}");
        }
        if asks > 0 {
            let _ = write!(line, ", asked for {asks} keyframes");
        }
        if frozen {
            line.push_str(", frozen waiting for a keyframe");
        }
        frozen || dropped > 0 || lost > 0
    }

    /// Where a frame's time went: decoding, how late it was by then,
    /// and the UI's part.
    #[allow(clippy::cast_precision_loss)] // ms averages
    fn write_timing(&mut self, line: &mut String, busy: u32, present: Option<PresentTally>) {
        let _ = write!(line, " | decoding busy {busy}%");
        if !self.codec.is_empty() {
            let _ = write!(
                line,
                ": codec {:.1} + output {:.1} ms (p95 {:.1} + {:.1})",
                self.codec.mean(),
                self.output.mean(),
                self.codec.p95(),
                self.output.p95()
            );
        }
        if !self.whole.is_empty() {
            let _ = write!(
                line,
                ": decode {:.1} ms (p95 {:.1})",
                self.whole.mean(),
                self.whole.p95()
            );
        }
        if !self.behind.is_empty() {
            let _ = write!(
                line,
                " | behind live {:.0} ms (max {:.0})",
                self.behind.mean(),
                self.behind.max()
            );
        }
        if let Some(p) = present
            && p.shown > 0
        {
            let _ = write!(
                line,
                ", then {:.1} ms to screen (max {:.1}; preparing {:.1} ms each)",
                p.to_screen_us as f32 / p.shown as f32 / 1000.0,
                p.to_screen_max_us as f32 / 1000.0,
                p.prepare_us as f32 / p.prepared.max(1) as f32 / 1000.0,
            );
        }
    }

    /// Stream audio: queued, and what the decoder had to make up.
    fn write_audio(&self, line: &mut String, audio: Option<StreamAudioStats>) {
        let Some(a) = audio else {
            return;
        };
        let _ = write!(line, " | audio {} ms queued", a.queued_ms);
        let b = self.audio_base.unwrap_or_default();
        let concealed = a.concealed.saturating_sub(b.concealed) * 20;
        let skipped = a.skipped.saturating_sub(b.skipped) * 20;
        if concealed > 0 {
            let _ = write!(line, ", {concealed} ms concealed");
        }
        if skipped > 0 {
            let _ = write!(line, ", {skipped} ms skipped");
        }
    }
}

/// Our own share's log state (the sender's side).
pub(crate) struct Sharing {
    window_start: Instant,
    fresh: u32,
    unchanged: u32,
    encoded: u32,
    keyframes: u32,
    bytes: u64,
    /// Frames dropped as too large to send, and the largest.
    too_large: u32,
    largest: usize,
    encode: Samples,
    groups_started: u32,
    groups_reset: u32,
    loss_max: f32,
    rtt_ms: f32,
    sent_kbps: Vec<f32>,
}

/// What the summary needs from the capture state at the time.
pub(crate) struct ShareState<'a> {
    pub preset: &'a str,
    pub bitrate_pct: u32,
    pub fps_pct: u32,
    pub viewers: u32,
    /// The encoder's quantizer floor after frames too large, if any.
    pub qp_floor: Option<u32>,
}

impl Sharing {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            fresh: 0,
            unchanged: 0,
            encoded: 0,
            keyframes: 0,
            bytes: 0,
            too_large: 0,
            largest: 0,
            encode: Samples::default(),
            groups_started: 0,
            groups_reset: 0,
            loss_max: 0.0,
            rtt_ms: 0.0,
            sent_kbps: Vec::new(),
        }
    }

    /// A capture tick: a new picture, or the screen unchanged.
    pub(crate) fn on_capture(&mut self, fresh: bool) {
        if fresh {
            self.fresh += 1;
        } else {
            self.unchanged += 1;
        }
    }

    pub(crate) fn on_encode(&mut self, took: Duration) {
        self.encode.push(took);
    }

    /// An access unit dropped: too large to send.
    pub(crate) fn on_too_large(&mut self, bytes: usize) {
        self.too_large += 1;
        self.largest = self.largest.max(bytes);
    }

    /// An access unit handed to the network.
    pub(crate) fn on_sent(&mut self, bytes: usize, keyframe: bool) {
        self.encoded += 1;
        self.keyframes += u32::from(keyframe);
        self.bytes += bytes as u64;
    }

    /// The network thread's ~2 s uplink sample.
    pub(crate) fn on_uplink(
        &mut self,
        started: u32,
        reset: u32,
        path_loss_pct: f32,
        rtt_ms: f32,
        kbps_up: f32,
    ) {
        self.groups_started += started;
        self.groups_reset += reset;
        self.loss_max = self.loss_max.max(path_loss_pct);
        self.rtt_ms = rtt_ms;
        self.sent_kbps.push(kbps_up);
    }

    /// The summary line when one is due.
    #[allow(clippy::cast_precision_loss)] // rates
    pub(crate) fn poll(&mut self, now: Instant, state: &ShareState<'_>) -> Option<(Level, String)> {
        let secs = now.duration_since(self.window_start).as_secs_f32();
        if secs < SUMMARY_EVERY.as_secs_f32() {
            return None;
        }
        let mut line = format!(
            "sharing {}: captured {:.1}/s (+{:.1}/s unchanged), encoded {:.1}/s, {} keyframes",
            state.preset,
            self.fresh as f32 / secs,
            self.unchanged as f32 / secs,
            self.encoded as f32 / secs,
            self.keyframes,
        );
        if self.too_large > 0 {
            let _ = write!(
                line,
                ", {} too large to send (up to {} KB)",
                self.too_large,
                self.largest / 1000
            );
        }
        if !self.encode.is_empty() {
            let _ = write!(
                line,
                ", encode {:.1} ms (p95 {:.1})",
                self.encode.mean(),
                self.encode.p95()
            );
        }
        let _ = write!(
            line,
            " | offered {:.1} Mbit/s",
            self.bytes as f32 * 8.0 / 1e6 / secs
        );
        if !self.sent_kbps.is_empty() {
            let sent = self.sent_kbps.iter().sum::<f32>() / self.sent_kbps.len() as f32;
            let _ = write!(
                line,
                ", sent {:.1} Mbit/s, abandoned {}/{} groups, path loss up to {:.0}%, rtt {:.0} ms",
                sent / 1000.0,
                self.groups_reset,
                self.groups_started,
                self.loss_max,
                self.rtt_ms
            );
        }
        let _ = write!(line, " | bitrate {}%", state.bitrate_pct);
        if let Some(qp) = state.qp_floor {
            let _ = write!(line, " (QP ≥ {qp})");
        }
        let _ = write!(
            line,
            ", fps {}%, {} viewer{}",
            state.fps_pct,
            state.viewers,
            if state.viewers == 1 { "" } else { "s" }
        );
        let level = if self.groups_reset > 0 || self.loss_max >= 10.0 || self.too_large > 0 {
            Level::Warn
        } else {
            Level::Info
        };
        *self = Self::new(now);
        Some((level, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn split(codec: u64, output: u64) -> Option<DecodeTiming> {
        Some(DecodeTiming {
            codec: ms(codec),
            output: ms(output),
        })
    }

    /// A 30 fps window where each picture costs 6 + 26 ms and lands
    /// 80 ms behind live.
    fn a_slow_phone_window(v: &mut Viewing, t0: Instant) {
        for i in 0..150 {
            let at = t0 + ms(i * 33);
            v.on_decode_call(ms(32));
            v.on_picture(at, split(6, 26), ms(32), Some(ms(80)), (1920, 1080));
        }
    }

    #[test]
    fn the_summary_says_where_the_time_went() {
        let t0 = Instant::now();
        let mut v = Viewing::new(7, t0);
        a_slow_phone_window(&mut v, t0);
        let buffer = VideoJitterBuffer::new(30);
        let lines = v.poll(t0 + SUMMARY_EVERY, &buffer, None, || None);
        let (level, line) = &lines[0];
        assert!(line.starts_with("viewing 7 1920x1080:"), "{line}");
        assert!(line.contains("decoded 30.0/s"), "{line}");
        assert!(line.contains("decoding busy 96%"), "{line}");
        assert!(line.contains("codec 6.0 + output 26.0 ms"), "{line}");
        assert!(line.contains("behind live 80 ms"), "{line}");
        // Busy over 90 %: the decode thread is the limit.
        assert_eq!(*level, Level::Warn);
    }

    #[test]
    fn a_healthy_stream_is_an_info_line() {
        let t0 = Instant::now();
        let mut v = Viewing::new(7, t0);
        for i in 0..300 {
            v.on_decode_call(ms(3));
            v.on_picture(
                t0 + ms(i * 16),
                split(2, 1),
                ms(3),
                Some(ms(40)),
                (1920, 1080),
            );
        }
        let buffer = VideoJitterBuffer::new(60);
        let present = PresentTally {
            shown: 290,
            replaced: 10,
            prepared: 300,
            prepare_us: 300_000,
            to_screen_us: 290_000,
            to_screen_max_us: 4_000,
        };
        let lines = v.poll(t0 + SUMMARY_EVERY, &buffer, None, || Some(present));
        let (level, line) = &lines[0];
        assert_eq!(*level, Level::Info, "{line}");
        assert!(
            line.contains("shown 58.0/s (10 skipped for a newer one)"),
            "{line}"
        );
        assert!(
            line.contains("then 1.0 ms to screen (max 4.0; preparing 1.0 ms each)"),
            "{line}"
        );
    }

    #[test]
    fn no_summary_before_the_window_closes() {
        let t0 = Instant::now();
        let mut v = Viewing::new(7, t0);
        let buffer = VideoJitterBuffer::new(30);
        assert!(v.poll(t0 + ms(4_900), &buffer, None, || None).is_empty());
    }

    #[test]
    fn falling_behind_is_said_once_per_episode() {
        let t0 = Instant::now();
        let mut v = Viewing::new(7, t0);
        let late = |v: &mut Viewing, at: Instant, behind: u64| {
            v.on_picture(at, split(6, 26), ms(32), Some(ms(behind)), (1920, 1080))
        };
        assert!(late(&mut v, t0, 100).is_none());
        let first = late(&mut v, t0 + ms(1_000), 700).expect("episode starts");
        assert_eq!(first.0, Level::Warn);
        assert!(first.1.contains("700 ms late"), "{}", first.1);
        // Still late, and back to normal briefly: no more lines.
        assert!(late(&mut v, t0 + ms(2_000), 1_200).is_none());
        assert!(late(&mut v, t0 + ms(3_000), 100).is_none());
        let buffer = VideoJitterBuffer::new(30);
        assert!(v.poll(t0 + ms(4_000), &buffer, None, || None).is_empty());
        // Ten seconds since the last late picture: caught up.
        let lines = v.poll(t0 + ms(12_000), &buffer, None, || None);
        assert!(
            lines
                .iter()
                .any(|(_, l)| l.contains("keeping up with live again")),
            "{lines:?}"
        );
    }

    #[test]
    fn audio_gaps_are_reported_in_ms() {
        let t0 = Instant::now();
        let mut v = Viewing::new(7, t0);
        let buffer = VideoJitterBuffer::new(30);
        let a0 = StreamAudioStats {
            received: 100,
            concealed: 0,
            skipped: 0,
            queued_ms: 110,
        };
        v.poll(t0 + SUMMARY_EVERY, &buffer, Some(a0), || None);
        let a1 = StreamAudioStats {
            received: 340,
            concealed: 3,
            skipped: 12,
            queued_ms: 130,
        };
        let lines = v.poll(t0 + SUMMARY_EVERY * 2, &buffer, Some(a1), || None);
        let line = &lines[0].1;
        assert!(
            line.contains("audio 130 ms queued, 60 ms concealed, 240 ms skipped"),
            "{line}"
        );
    }

    #[test]
    fn the_sharer_line_counts_what_the_uplink_dropped() {
        let t0 = Instant::now();
        let mut sh = Sharing::new(t0);
        for i in 0..300 {
            sh.on_capture(true);
            sh.on_encode(ms(4));
            sh.on_sent(14_000, i % 120 == 0);
        }
        sh.on_uplink(1, 1, 80.0, 45.0, 209.0);
        sh.on_uplink(1, 0, 30.0, 47.0, 604.0);
        let state = ShareState {
            preset: "1080p 60fps",
            bitrate_pct: 100,
            fps_pct: 100,
            viewers: 1,
            qp_floor: None,
        };
        assert!(sh.poll(t0 + ms(4_000), &state).is_none());
        let (level, line) = sh.poll(t0 + SUMMARY_EVERY, &state).expect("due");
        assert_eq!(level, Level::Warn);
        assert!(
            line.starts_with("sharing 1080p 60fps: captured 60.0/s"),
            "{line}"
        );
        assert!(line.contains("3 keyframes, encode 4.0 ms"), "{line}");
        assert!(
            line.contains("offered 6.7 Mbit/s, sent 0.4 Mbit/s"),
            "{line}"
        );
        assert!(
            line.contains("abandoned 1/2 groups, path loss up to 80%"),
            "{line}"
        );
        assert!(line.contains("1 viewer"), "{line}");
    }

    #[test]
    fn frames_too_large_make_the_sharer_line_a_warning() {
        let t0 = Instant::now();
        let mut sh = Sharing::new(t0);
        for _ in 0..300 {
            sh.on_capture(true);
            sh.on_sent(14_000, false);
        }
        sh.on_too_large(425_871);
        sh.on_too_large(287_331);
        let state = ShareState {
            preset: "1080p 60fps",
            bitrate_pct: 100,
            fps_pct: 100,
            viewers: 2,
            qp_floor: Some(28),
        };
        let (level, line) = sh.poll(t0 + SUMMARY_EVERY, &state).expect("due");
        assert_eq!(level, Level::Warn);
        assert!(
            line.contains("0 keyframes, 2 too large to send (up to 425 KB)"),
            "{line}"
        );
        assert!(
            line.contains("bitrate 100% (QP ≥ 28), fps 100%, 2 viewers"),
            "{line}"
        );
        // The next window starts clean.
        let (level, _) = sh
            .poll(
                t0 + SUMMARY_EVERY * 2,
                &ShareState {
                    qp_floor: None,
                    ..state
                },
            )
            .expect("due");
        assert_eq!(level, Level::Info);
    }
}
