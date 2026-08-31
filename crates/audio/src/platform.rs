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
//! macOS and Linux get the equivalent through `audio_thread_priority`
//! (Mach time-constraint policy / SCHED_FIFO): the output worker now
//! decodes every peer, and a normal-priority thread loses to a compiler
//! or a browser tab exactly when the buffers are smallest. Failure is
//! logged and ignored — on Linux without an rtprio limit the worker just
//! runs at normal priority, as before.
//!
//! The two Windows APIs involved (winmm, avrt) are declared directly and
//! linked from the Windows SDK.

use std::time::Duration;

use cpal::{BufferSize, SupportedBufferSize};
use crossbeam_channel::{Receiver, RecvTimeoutError};

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

// ── Real-time thread promotion ──────────────────────────────────────────

/// Reverts the calling thread's scheduling class when dropped.
pub struct RtGuard {
    #[cfg(windows)]
    handle: *mut core::ffi::c_void,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    handle: Option<audio_thread_priority::RtPriorityHandle>,
}

#[cfg(windows)]
impl Drop for RtGuard {
    fn drop(&mut self) {
        unsafe {
            AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for RtGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Err(e) = audio_thread_priority::demote_current_thread_from_real_time(handle) {
                log::debug!("real-time demotion failed: {e}");
            }
        }
    }
}

/// Register the current thread with the Windows MMCSS "Pro Audio" task
/// class. Returns a guard to keep in scope for the thread's lifetime;
/// `None` on failure (logged, harmless).
///
/// Re-exported from the crate root so the application can promote the
/// threads that sit between this crate and the socket — the one draining
/// [`crate::VoiceChat::outgoing_receiver`] above all. A foreground game
/// plus Windows Game Mode throttles normal-priority threads of background
/// processes; everything inside this crate is already promoted, so a
/// normal-priority send loop becomes the one place the voice still stalls.
#[cfg(windows)]
pub fn promote_current_thread(_buffer_frames: u32, _sample_rate: u32) -> Option<RtGuard> {
    // UTF-16, NUL-terminated.
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut index: u32 = 0;
    let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
    if handle.is_null() {
        log::warn!("MMCSS registration failed — audio worker runs at normal priority");
        None
    } else {
        Some(RtGuard { handle })
    }
}

/// Ask for real-time scheduling sized to `buffer_frames` at `sample_rate`
/// (the period the thread must meet). `None` on failure (logged once at
/// debug level — on Linux this simply means no rtprio limit is set).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn promote_current_thread(buffer_frames: u32, sample_rate: u32) -> Option<RtGuard> {
    match audio_thread_priority::promote_current_thread_to_real_time(buffer_frames, sample_rate) {
        Ok(handle) => Some(RtGuard {
            handle: Some(handle),
        }),
        Err(e) => {
            log::debug!("real-time promotion unavailable ({e}); worker runs at normal priority");
            None
        }
    }
}

/// No-op on platforms without a real-time path (iOS/Android workers rely
/// on the platform audio session).
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn promote_current_thread(_buffer_frames: u32, _sample_rate: u32) -> Option<RtGuard> {
    None
}

/// Promote the **cpal callback thread** — the thread cpal owns and runs our
/// data callback on — and never revert.
///
/// Field log, Windows + Counter-Strike 2: ~1,000 capture discontinuities
/// per minute (`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`), zero with no
/// game. cpal's WASAPI capture thread is `THREAD_PRIORITY_TIME_CRITICAL`
/// inside a normal-priority-class process — priority 15 — while the game
/// runs 13–15 on every core with its own audio on MMCSS above that. Our
/// worker threads were already promoted; the thread *reading the device*
/// was not, and it was the one being starved. `AvSetMmThreadCharacteristics`
/// applies to the calling thread, so the first callback invocation is the
/// one place we can reach it.
///
/// Call once per stream from inside the callback (guard with a `bool`).
/// The handle is intentionally leaked: the thread lives exactly as long as
/// the stream, reverting from any other thread is undefined, and the OS
/// releases it at thread exit. Skipped on macOS, where the callback runs
/// on CoreAudio's HAL thread, which is already time-constraint scheduled.
pub(crate) fn promote_callback_thread(buffer_frames: u32, sample_rate: u32) {
    #[cfg(target_os = "macos")]
    {
        let _ = (buffer_frames, sample_rate);
    }
    #[cfg(not(target_os = "macos"))]
    {
        match promote_current_thread(buffer_frames, sample_rate) {
            Some(guard) => {
                log::debug!("audio callback thread promoted to real-time priority");
                std::mem::forget(guard);
            }
            None => log::debug!("audio callback thread stays at cpal's default priority"),
        }
    }
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
