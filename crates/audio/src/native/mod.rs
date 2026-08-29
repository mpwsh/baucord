//! Native per-platform device backends.
//!
//! cpal stays the universal fallback and the enumeration API. The hot
//! paths — the thread that reads the microphone and the thread that feeds
//! the speaker — are taken over per platform where cpal's abstraction
//! cannot express what a voice engine needs: who runs the callback thread
//! and at what priority, what a buffer size means, which channel is which,
//! and whether the device just went away.
//!
//! Every backend honours one contract, so the workers in `input.rs` and
//! `output.rs` are backend-agnostic:
//!
//! - **Capture** pushes device-rate mono `f32` into an `rtrb` ring and
//!   pings a wake latch after every packet. It reports the sample rate it
//!   actually delivers so the worker can decide whether to resample.
//! - **Render** pops device-rate mono `f32` from an `rtrb` ring, fans it
//!   out to the front pair (silence elsewhere), and pings the latch after
//!   every period so the worker refills promptly.
//! - Both count glitches into `AudioStats`, set the `*_stream_failed` flag
//!   when the device goes away, and stop within ~100 ms of a stop request.
//!
//! Windows: [`wasapi`] — event-driven shared mode on our own MMCSS thread.
//! macOS: cpal's CoreAudio path runs the callback on the HAL's real-time
//! thread already; nothing to take over yet. Linux: cpal/ALSA until there
//! is a PipeWire backend.

#[cfg(windows)]
pub mod wasapi;
