//! OS scheduling niceties for the real-time audio path.
//!
//! Everything in here is best-effort and fail-safe: on error we log and
//! fall back to exactly the previous behavior, and on non-Windows
//! platforms most of it is a no-op — macOS and Linux already schedule
//! short waits and audio threads tightly enough that the workers meet
//! their deadlines without help. Windows is the platform where these
//! measurably matter:
//!
//! - `Sleep()` and timed waits are quantized to the system timer period,
//!   which defaults to ~15.6 ms — a worker "sleeping 1 ms" can nap for
//!   15. [`TimerResolutionGuard`] requests a 1 ms period for the session.
//! - MMCSS ("Pro Audio") gives the worker threads audio-class scheduling
//!   so small buffers survive load without glitching.
//!
//! No extra crates: the two Windows APIs involved (winmm, avrt) are
//! declared directly and linked from the Windows SDK.

use cpal::{BufferSize, SupportedBufferSize};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::time::Duration;

// ── High-resolution timer (process-wide) ────────────────────────────────

/// Holds the Windows multimedia timer at 1 ms resolution while alive.
///
/// `timeBeginPeriod` requests nest per process and are balanced by the
/// matching `timeEndPeriod` in `Drop`, so scoping this to the voice
/// session keeps the (small) power cost bounded to active calls.
pub struct TimerResolutionGuard {
    #[cfg_attr(not(windows), allow(dead_code))]
    active: bool,
}

#[cfg(windows)]
impl TimerResolutionGuard {
    pub fn acquire() -> Self {
        // TIMERR_NOERROR == 0
        let active = unsafe { timeBeginPeriod(1) } == 0;
        if !active {
            log::warn!("timeBeginPeriod(1) failed — timed waits may be coarse (~15 ms)");
        }
        Self { active }
    }
}

#[cfg(not(windows))]
impl TimerResolutionGuard {
    pub fn acquire() -> Self {
        Self { active: false }
    }
}

impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.active {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

// ── MMCSS thread promotion ──────────────────────────────────────────────

/// De-registers the calling thread from MMCSS when dropped.
pub struct MmcssGuard {
    #[cfg(windows)]
    handle: *mut core::ffi::c_void,
}

#[cfg(windows)]
impl Drop for MmcssGuard {
    fn drop(&mut self) {
        unsafe {
            AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}

/// Register the current thread with the Windows MMCSS "Pro Audio" task
/// class. Returns a guard to keep in scope for the thread's lifetime;
/// `None` on failure (logged, harmless) or on non-Windows platforms.
#[cfg(windows)]
pub fn promote_current_thread() -> Option<MmcssGuard> {
    // UTF-16, NUL-terminated.
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut index: u32 = 0;
    let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
    if handle.is_null() {
        log::warn!("MMCSS registration failed — audio worker runs at normal priority");
        None
    } else {
        Some(MmcssGuard { handle })
    }
}

/// No-op off Windows.
#[cfg(not(windows))]
pub fn promote_current_thread() -> Option<MmcssGuard> {
    None
}

// ── Stream buffer sizing ────────────────────────────────────────────────

/// Pick a cpal buffer size: request ~10 ms when the device advertises a
/// supported range (clamped into that range), otherwise keep the backend
/// default.
///
/// **Windows always gets `Default`.** cpal's WASAPI backend fabricates
/// `Range { 0, u32::MAX }` whenever `GetBufferSizeLimits` fails — which is
/// every normal (non-offload) audio stack — so an "advertised range" there
/// is not a promise. It then feeds the requested duration straight into
/// `IAudioClient::Initialize` with no aligned-size retry, so a rejected
/// duration doesn't degrade gracefully: it kills stream creation entirely
/// (observed in the field as total silence). `Default` already means the
/// ~10 ms shared-mode engine period on Windows; going lower is
/// IAudioClient3 territory, not this knob.
#[cfg(windows)]
pub fn request_low_latency_buffer(
    _supported: &SupportedBufferSize,
    _sample_rate: u32,
) -> BufferSize {
    BufferSize::Default
}

/// See the Windows variant above for why this is per-platform. CoreAudio
/// and ALSA report genuine ranges and honor in-range fixed sizes.
#[cfg(not(windows))]
pub fn request_low_latency_buffer(supported: &SupportedBufferSize, sample_rate: u32) -> BufferSize {
    let desired = sample_rate / 100; // 10 ms of frames
    match supported {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(desired.clamp(*min, *max)),
        SupportedBufferSize::Unknown => BufferSize::Default,
    }
}

// ── Worker wakeups ──────────────────────────────────────────────────────

/// Block until the audio callback pings the wake latch, or until
/// `timeout` elapses — whichever comes first.
///
/// If the signaling side is gone (stream torn down while the worker is
/// still running, e.g. a failed device start), fall back to a plain sleep
/// so the loop degrades to the old polling behavior instead of busy-
/// spinning on a disconnected channel.
pub fn wait_for_wake(wake_rx: &Receiver<()>, timeout: Duration) {
    match wake_rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(timeout),
    }
}

/// `thread::spawn` with a name.
///
/// Names surface in debuggers, `GetThreadDescription`-based tooling, and
/// crash dumps — anonymous threads made the 2026-08 wake-rate audit
/// nearly impossible, hence: every worker gets a name. Same
/// panic-on-spawn-failure semantics as `std::thread::spawn` (which also
/// aborts the process if the OS can't create a thread).
pub(crate) fn spawn_named<F, T>(name: &str, f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect("failed to spawn thread")
}

// ── Windows FFI (no extra crates) ───────────────────────────────────────

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(u_period: u32) -> u32;
    fn timeEndPeriod(u_period: u32) -> u32;
}

#[cfg(windows)]
#[link(name = "avrt")]
extern "system" {
    fn AvSetMmThreadCharacteristicsW(
        task_name: *const u16,
        task_index: *mut u32,
    ) -> *mut core::ffi::c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut core::ffi::c_void) -> i32;
}
