//! Notification sounds embedded in the binary.
//!
//! Sounds are decoded from WAV at startup into `Vec<f32>` at 48 kHz mono,
//! ready to be mixed directly into the output hop buffer. Adding a new
//! sound is three steps:
//!
//! 1. Add the WAV file to `assets/sounds/`
//! 2. Add a variant to [`SoundType`]
//! 3. Add a line in [`SoundBank::new`]

use std::io::Cursor;

use log::{debug, warn};

use crate::OPUS_SAMPLE_RATE;

// ── Embedded WAV data ───────────────────────────────────────────────────

const JOIN_WAV: &[u8] = include_bytes!("../../../assets/sounds/join.wav");
const LEAVE_WAV: &[u8] = include_bytes!("../../../assets/sounds/leave.wav");

// ── Sound types ─────────────────────────────────────────────────────────

/// Identifies a notification sound to play.
///
/// Send this to [`VoiceChat::play_sound`](crate::VoiceChat::play_sound).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundType {
    UserJoined,
    UserLeft,
    /// A message named us.
    Mention,
    /// A direct message arrived.
    DirectMessage,
    /// Someone in our channel started sharing their screen.
    ShareStarted,
    /// Someone started watching the stream we share (softer: it is
    /// for the sharer alone, and viewers come and go).
    ViewerJoined,
}

// ── Sound bank ──────────────────────────────────────────────────────────

/// Pre-decoded notification sounds, all normalized to 48 kHz mono f32.
pub struct SoundBank {
    join: Vec<f32>,
    leave: Vec<f32>,
    mention: Vec<f32>,
    direct: Vec<f32>,
    share_started: Vec<f32>,
    viewer_joined: Vec<f32>,
}
impl Default for SoundBank {
    fn default() -> Self {
        Self::new()
    }
}
impl SoundBank {
    /// Decode all embedded sounds. Call once at startup.
    pub fn new() -> Self {
        Self {
            join: decode_wav("join", JOIN_WAV),
            leave: decode_wav("leave", LEAVE_WAV),
            // Synthesized rather than shipped: two soft sines with a
            // short envelope, quieter than the join/leave chimes. Built
            // once here, next to the WAV decodes — ~10k samples, 40 KB.
            mention: synth_notes(&[(660.0, 0.09), (880.0, 0.12)], 0.30),
            direct: synth_notes(&[(523.0, 0.08), (659.0, 0.08), (784.0, 0.12)], 0.28),
            // A stream going up in the room: two low notes a fifth
            // apart, held — nothing like the DM chime's quick high
            // arpeggio. A short, quieter pair for a viewer arriving.
            share_started: synth_notes(&[(392.0, 0.12), (587.0, 0.22)], 0.30),
            viewer_joined: synth_notes(&[(784.0, 0.06), (988.0, 0.10)], 0.16),
        }
    }

    /// Get the PCM samples for a sound type.
    pub fn get(&self, sound: SoundType) -> &[f32] {
        match sound {
            SoundType::UserJoined => &self.join,
            SoundType::UserLeft => &self.leave,
            SoundType::Mention => &self.mention,
            SoundType::DirectMessage => &self.direct,
            SoundType::ShareStarted => &self.share_started,
            SoundType::ViewerJoined => &self.viewer_joined,
        }
    }
}

// ── Synthesis ───────────────────────────────────────────────────────────

/// A sequence of sine notes `(hz, seconds)` at 48 kHz mono, each with
/// a 5 ms attack and a decay over its whole length so they never click.
fn synth_notes(notes: &[(f32, f32)], gain: f32) -> Vec<f32> {
    const RATE: f32 = 48_000.0;
    let mut out = Vec::new();
    for &(hz, secs) in notes {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let len = (secs * RATE) as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let attack = (RATE * 0.005) as usize;
        for i in 0..len {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / RATE;
            #[allow(clippy::cast_precision_loss)]
            let env = if i < attack {
                i as f32 / attack as f32
            } else {
                1.0 - (i - attack) as f32 / (len - attack).max(1) as f32
            };
            out.push((t * hz * std::f32::consts::TAU).sin() * env * gain);
        }
    }
    out
}

// ── WAV decoding ────────────────────────────────────────────────────────

/// Decode a WAV file to 48 kHz mono f32 samples.
///
/// Handles:
/// - Any bit depth hound supports (8, 16, 24, 32 int, 32 float)
/// - Stereo → mono downmix
/// - Sample rate conversion to 48 kHz (linear interpolation)
fn decode_wav(name: &str, data: &[u8]) -> Vec<f32> {
    let reader = match hound::WavReader::new(Cursor::new(data)) {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to decode {name}.wav: {e}");
            return Vec::new();
        }
    };

    let spec = reader.spec();
    let channels = spec.channels as usize;
    let source_rate = spec.sample_rate;

    debug!(
        "Loading {name}.wav: {}Hz {}ch {}bit {:?}",
        source_rate, channels, spec.bits_per_sample, spec.sample_format,
    );

    // Read all samples as f32
    let raw_samples = read_samples_f32(reader);

    // Stereo → mono downmix
    let mono = if channels > 1 {
        downmix_to_mono(&raw_samples, channels)
    } else {
        raw_samples
    };

    // Resample to 48 kHz if needed
    let resampled = if source_rate != OPUS_SAMPLE_RATE {
        resample(&mono, source_rate, OPUS_SAMPLE_RATE)
    } else {
        mono
    };

    debug!(
        "{name}.wav: {} samples ({:.0}ms)",
        resampled.len(),
        resampled.len() as f32 / OPUS_SAMPLE_RATE as f32 * 1000.0,
    );

    resampled
}

/// Read all samples from a hound reader as normalized f32 (-1.0..1.0).
fn read_samples_f32(mut reader: hound::WavReader<Cursor<&[u8]>>) -> Vec<f32> {
    let spec = reader.spec();

    match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = (1u32 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / max_val)
                .collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
    }
}

/// Downmix interleaved multi-channel audio to mono by averaging channels.
fn downmix_to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Resample using linear interpolation.
///
/// Good enough for short notification sounds. Not intended for
/// real-time voice — use rubato for that (see `resampler.rs`).
fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if samples.is_empty() || from_rate == to_rate {
        return samples.to_vec();
    }

    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = (samples.len() as f64 * ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;

        let sample = if idx + 1 < samples.len() {
            samples[idx] * (1.0 - frac) + samples[idx + 1] * frac
        } else if idx < samples.len() {
            samples[idx]
        } else {
            0.0
        };

        output.push(sample);
    }

    debug!(
        "Resampled: {from_rate}Hz → {to_rate}Hz ({} → {} samples)",
        samples.len(),
        output.len(),
    );

    output
}
