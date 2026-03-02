//! Echo cancellation module.
//!
//! Provides spectral subtraction-based echo cancellation using a
//! known reference signal. Two primary use cases:
//!
//! - **Stream voice removal**: subtract voice output from system
//!   loopback audio so screen share viewers don't hear voices twice.
//! - **Mic echo cancellation**: subtract speaker output from mic
//!   capture to eliminate acoustic feedback in speakerphone mode.
//!
//! ## Components
//!
//! - [`EchoCanceller`]: the core `AudioProcessor` that performs
//!   spectral subtraction with overlap-add.
//! - [`EchoCancelConfig`]: per-use-case tuning (delay range,
//!   aggressiveness, smoothing).
//! - [`VoiceReferenceRing`]: lock-free ring buffer for sharing the
//!   voice output mix between threads.
//! - [`DelayEstimator`]: cross-correlation based delay finder.
//!
//! ## Architecture
//!
//! ```text
//! Voice output worker ──push()──→ VoiceReferenceRing ←──read_delayed()──┐
//!                                                                        │
//!                                            ┌───────────────────────────┤
//!                                            │                           │
//!                                   ┌────────┴────────┐        ┌────────┴────────┐
//!                                   │  EchoCanceller   │        │  EchoCanceller   │
//!                                   │  (mic echo)      │        │  (stream)        │
//!                                   │  in input chain   │        │  in capture      │
//!                                   └──────────────────┘        └──────────────────┘
//! ```
//!
//! ## Dependencies
//!
//! - `rustfft` for FFT/IFFT operations

pub mod canceller;
pub mod delay;
pub mod reference_ring;
pub mod spectral;

pub use canceller::{EchoCancelConfig, EchoCanceller};
pub use reference_ring::VoiceReferenceRing;
