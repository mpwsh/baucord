//! Stereo to mono for a stream played through a phone's call output
//! (`stream::mix`), without cancelling anything.
//!
//! Adding the channels, `(L + R) / 2`, removes whatever is opposite in
//! phase between them: a game's surround or "spatial" sound folded into
//! stereo, stereo wideners, a polarity test. 2026-10-08: the guitar of
//! audiocheck.net's out-of-phase test was silent on an A07 while its
//! left/right test played both sides — a game's moving sounds came and
//! went the same way.
//!
//! Here each 21 ms window is split into frequencies (FFT, 1024 points,
//! half overlapping, √Hann windows so overlap-add rebuilds the signal
//! exactly) and every frequency is mixed on its own: the two channels'
//! strengths averaged, `(|L| + |R|) / 2`, at one phase — the sum's,
//! or the difference's where that is the stronger (the content is more
//! out of phase than in). For what is the same in both channels, and
//! for what is only in one, the result is exactly `(L + R) / 2` as
//! before; what was out of phase now plays at its level instead of
//! vanishing. One window of delay (~21 ms), a few microseconds of work
//! per window.
//!
//! Platform-free and host-tested; used on the phones only (the desktop
//! plays streams in stereo).

use std::sync::Arc;

use realfft::{num_complex::Complex, ComplexToReal, RealFftPlanner, RealToComplex};

/// FFT length: 21.3 ms at 48 kHz, 47 Hz per bin.
const N: usize = 1024;
/// Hop between windows (half overlap).
const HOP: usize = N / 2;

/// The stream's downmixer: stereo in, mono out, one per stream.
pub(crate) struct Downmix {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    /// √Hann (periodic): its square sums to one over half-overlapping
    /// windows, so analysis × synthesis rebuilds the input.
    window: Vec<f32>,
    /// The last `N` input samples per channel; new ones land in the
    /// last `HOP` slots.
    left: Vec<f32>,
    right: Vec<f32>,
    /// New samples since the last window.
    filled: usize,
    /// Overlap-add of the windows' output.
    ola: Vec<f32>,
    // Scratch, reused.
    frame_l: Vec<f32>,
    frame_r: Vec<f32>,
    spec_l: Vec<Complex<f32>>,
    spec_r: Vec<Complex<f32>>,
    scratch_fwd: Vec<Complex<f32>>,
    scratch_inv: Vec<Complex<f32>>,
}

impl Downmix {
    pub(crate) fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(N);
        let inverse = planner.plan_fft_inverse(N);
        #[allow(clippy::cast_precision_loss)] // n < 1024
        let window = (0..N)
            .map(|n| (std::f32::consts::PI * n as f32 / N as f32).sin())
            .collect();
        Self {
            window,
            left: vec![0.0; N],
            right: vec![0.0; N],
            filled: 0,
            ola: vec![0.0; N],
            frame_l: forward.make_input_vec(),
            frame_r: forward.make_input_vec(),
            spec_l: forward.make_output_vec(),
            spec_r: forward.make_output_vec(),
            scratch_fwd: forward.make_scratch_vec(),
            scratch_inv: inverse.make_scratch_vec(),
            forward,
            inverse,
        }
    }

    /// Interleaved stereo in; the mono that is ready appended to `out`
    /// (a whole number of hops: it trails the input by up to a window).
    pub(crate) fn process(&mut self, stereo: &[f32], out: &mut Vec<f32>) {
        for pair in stereo.chunks_exact(2) {
            let at = N - HOP + self.filled;
            self.left[at] = pair[0];
            self.right[at] = pair[1];
            self.filled += 1;
            if self.filled == HOP {
                self.window_out(out);
                self.left.copy_within(HOP.., 0);
                self.right.copy_within(HOP.., 0);
                self.filled = 0;
            }
        }
    }

    /// Forget everything (the stream stopped being mixed for a while):
    /// the next audio starts clean rather than overlapping stale audio.
    pub(crate) fn reset(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.ola.fill(0.0);
        self.filled = 0;
    }

    /// One window: both channels to frequencies, mixed per frequency,
    /// back to time, overlap-added; the oldest hop is done.
    fn window_out(&mut self, out: &mut Vec<f32>) {
        for (((fl, fr), (&l, &r)), &w) in self
            .frame_l
            .iter_mut()
            .zip(self.frame_r.iter_mut())
            .zip(self.left.iter().zip(&self.right))
            .zip(&self.window)
        {
            *fl = l * w;
            *fr = r * w;
        }
        // Lengths match by construction; an error here is impossible.
        let _ = self.forward.process_with_scratch(
            &mut self.frame_l,
            &mut self.spec_l,
            &mut self.scratch_fwd,
        );
        let _ = self.forward.process_with_scratch(
            &mut self.frame_r,
            &mut self.spec_r,
            &mut self.scratch_fwd,
        );
        for (m, &r) in self.spec_l.iter_mut().zip(&self.spec_r) {
            *m = mix_bin(*m, r);
        }
        // DC and Nyquist are real; keep them exactly so for the inverse.
        let last = self.spec_l.len() - 1;
        self.spec_l[0].im = 0.0;
        self.spec_l[last].im = 0.0;
        let _ = self.inverse.process_with_scratch(
            &mut self.spec_l,
            &mut self.frame_l,
            &mut self.scratch_inv,
        );
        #[allow(clippy::cast_precision_loss)]
        let scale = 1.0 / N as f32; // the inverse FFT is unnormalised
        for ((acc, &y), &w) in self.ola.iter_mut().zip(&self.frame_l).zip(&self.window) {
            *acc += y * w * scale;
        }
        out.extend_from_slice(&self.ola[..HOP]);
        self.ola.copy_within(HOP.., 0);
        self.ola[N - HOP..].fill(0.0);
    }
}

/// One frequency of each channel to one: the average strength at the
/// phase of whichever of the sum and the difference is stronger. Equal
/// channels and one-sided content come out as `(L + R) / 2`; opposite
/// channels as either one, not as nothing.
fn mix_bin(l: Complex<f32>, r: Complex<f32>) -> Complex<f32> {
    let sum = l + r;
    let diff = l - r;
    let reference = if sum.norm_sqr() >= diff.norm_sqr() {
        sum
    } else {
        diff
    };
    let len = reference.norm();
    if len <= f32::MIN_POSITIVE {
        return Complex::new(0.0, 0.0);
    }
    reference * ((l.norm() + r.norm()) * 0.5 / len)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn sine(freq: f32, len: usize) -> Vec<f32> {
        #[allow(clippy::cast_precision_loss)]
        (0..len)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / RATE).sin())
            .collect()
    }

    fn interleave(l: &[f32], r: &[f32]) -> Vec<f32> {
        l.iter().zip(r).flat_map(|(&a, &b)| [a, b]).collect()
    }

    /// Feed in decoder-sized frames (960 per channel), as the stream
    /// decoder does.
    fn run(stereo: &[f32]) -> Vec<f32> {
        let mut d = Downmix::new();
        let mut out = Vec::new();
        for frame in stereo.chunks(960 * 2) {
            d.process(frame, &mut out);
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Strength of one frequency (correlation with a sine and cosine).
    fn level_at(x: &[f32], freq: f32) -> f32 {
        let (mut s, mut c) = (0.0f32, 0.0f32);
        for (i, &v) in x.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let ph = 2.0 * std::f32::consts::PI * freq * i as f32 / RATE;
            s += v * ph.sin();
            c += v * ph.cos();
        }
        #[allow(clippy::cast_precision_loss)]
        let n = x.len() as f32;
        2.0 * (s * s + c * c).sqrt() / n
    }

    /// Past the first window, in steady state.
    fn steady(x: &[f32]) -> &[f32] {
        &x[N * 2..]
    }

    #[test]
    fn the_same_in_both_channels_comes_out_unchanged() {
        let a = sine(440.0, 48_000);
        let out = run(&interleave(&a, &a));
        // Whole hops only: the last partial window is still pending.
        assert_eq!(out.len(), 48_000 / HOP * HOP);
        // A window of delay (N − HOP), then the input itself.
        let delay = N - HOP;
        let err = steady(&out)
            .iter()
            .zip(&a[N * 2 - delay..])
            .map(|(o, i)| (o - i).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max error {err}");
    }

    #[test]
    fn opposite_channels_no_longer_cancel() {
        // audiocheck.net's out-of-phase test: R = −L.
        let a = sine(440.0, 48_000);
        let inverted: Vec<f32> = a.iter().map(|v| -v).collect();
        let stereo = interleave(&a, &inverted);
        let old: Vec<f32> = stereo
            .chunks_exact(2)
            .map(|p| (p[0] + p[1]) * 0.5)
            .collect();
        assert!(rms(&old) < 1e-6, "the old downmix cancels it");
        let out = run(&stereo);
        let level = rms(steady(&out));
        assert!(
            (level - rms(&a)).abs() < 0.01,
            "plays at its own level: {level} vs {}",
            rms(&a)
        );
    }

    #[test]
    fn one_sided_sound_is_halved_as_before() {
        let a = sine(1000.0, 48_000);
        let silent = vec![0.0; a.len()];
        let out = run(&interleave(&a, &silent));
        let level = rms(steady(&out));
        assert!((level - rms(&a) * 0.5).abs() < 0.01, "{level}");
    }

    #[test]
    fn centre_and_side_content_both_survive() {
        // A centred voice (440 Hz) and a sound spread out of phase
        // (1.5 kHz): the old downmix keeps only the first.
        let voice = sine(440.0, 48_000);
        let side = sine(1500.0, 48_000);
        let left: Vec<f32> = voice
            .iter()
            .zip(&side)
            .map(|(v, s)| 0.5 * (v + s))
            .collect();
        let right: Vec<f32> = voice
            .iter()
            .zip(&side)
            .map(|(v, s)| 0.5 * (v - s))
            .collect();
        let out = run(&interleave(&left, &right));
        let mono = steady(&out);
        let (at_voice, at_side) = (level_at(mono, 440.0), level_at(mono, 1500.0));
        assert!((at_voice - 0.5).abs() < 0.05, "voice {at_voice}");
        assert!((at_side - 0.5).abs() < 0.05, "side {at_side}");
    }

    #[test]
    fn a_reset_starts_clean() {
        let a = sine(440.0, 4_800);
        let mut d = Downmix::new();
        let mut out = Vec::new();
        d.process(&interleave(&a, &a), &mut out);
        d.reset();
        out.clear();
        let silence = vec![0.0; 2 * 4_800];
        d.process(&silence, &mut out);
        assert!(out.iter().all(|v| v.abs() < 1e-9), "nothing of before");
    }
}
