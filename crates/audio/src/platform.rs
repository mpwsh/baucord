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
//! (Mach time-constraint policy / `SCHED_FIFO)`: the output worker now
//! decodes every peer, and a normal-priority thread loses to a compiler
//! or a browser tab exactly when the buffers are smallest. Failure is
//! logged and ignored — on Linux without an rtprio limit the worker just
//! runs at normal priority, as before.
//!
//! Android gets Android's own audio class: nice −16
//! (`THREAD_PRIORITY_AUDIO`, what the framework gives `AudioTrack`'s
//! threads). Without it every thread of ours sits at nice −10: Android
//! boosts the foreground app's main thread to −10
//! (`THREAD_PRIORITY_TOP_APP_BOOST`) and `android_main`'s thread, created
//! from it, passes that on to everything it spawns (observed
//! 2026-10-05: `nice -10 -> -16`). So the audio workers tied with the
//! video decode, colour conversion and network threads; −16 puts them
//! first. Apps may lower their own threads' nice (init sets
//! `RLIMIT_NICE` for every process); a refusal is logged and ignored.
//!
//! The two Windows APIs involved (winmm, avrt) are declared directly and
//! linked from the Windows SDK.

use std::time::Duration;

use cpal::{BufferSize, SupportedBufferSize};

// ── High-resolution timer (process-wide) ────────────────────────────────

/// Holds the Windows multimedia timer at 1 ms resolution while alive
/// (nothing elsewhere).
///
/// `timeBeginPeriod` requests nest per process and are balanced by the
/// matching `timeEndPeriod` in `Drop`, so each holder scopes its own:
/// the voice engine in a room, the screen threads while a stream is
/// shared or watched — whoever paces something by short timed waits.
/// Nobody holds it while the app only sits connected (review P2-35).
pub struct TimerResolutionGuard {
    #[cfg_attr(not(windows), allow(dead_code))]
    active: bool,
}

#[cfg(windows)]
impl TimerResolutionGuard {
    #[must_use]
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
    #[must_use]
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
    /// The thread and the nice value to put back.
    #[cfg(target_os = "android")]
    tid: libc::pid_t,
    #[cfg(target_os = "android")]
    previous_nice: libc::c_int,
}

#[cfg(target_os = "android")]
impl Drop for RtGuard {
    fn drop(&mut self) {
        // SAFETY: plain syscall on our own thread id; no memory involved.
        unsafe {
            libc::setpriority(
                libc::PRIO_PROCESS,
                self.tid as libc::id_t,
                self.previous_nice,
            );
        }
    }
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
        if let Some(handle) = self.handle.take()
            && let Err(e) = audio_thread_priority::demote_current_thread_from_real_time(handle)
        {
            log::debug!("real-time demotion failed: {e}");
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
#[must_use]
pub fn promote_current_thread(_buffer_frames: u32, _sample_rate: u32) -> Option<RtGuard> {
    // UTF-16, NUL-terminated.
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut index: u32 = 0;
    let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &raw mut index) };
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

/// Android: `THREAD_PRIORITY_AUDIO` (nice −16) for the calling thread
/// (see the module docs). `None` if the kernel refuses (logged once per
/// thread at info level — it is the first thing to check when Android
/// voice breaks up).
#[cfg(target_os = "android")]
pub fn promote_current_thread(_buffer_frames: u32, _sample_rate: u32) -> Option<RtGuard> {
    /// `android.os.Process.THREAD_PRIORITY_AUDIO`.
    const THREAD_PRIORITY_AUDIO: libc::c_int = -16;
    let name = std::thread::current().name().unwrap_or("?").to_owned();
    // SAFETY: plain syscalls on the calling thread's own id.
    unsafe {
        let tid = libc::gettid();
        // bionic returns the nice value itself (−20..19); -1 is a valid
        // answer, so a failed read just restores to -1 later — harmless.
        let previous_nice = libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t);
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, THREAD_PRIORITY_AUDIO) == 0 {
            log::info!("{name}: audio priority (nice {previous_nice} -> {THREAD_PRIORITY_AUDIO})");
            Some(RtGuard { tid, previous_nice })
        } else {
            log::info!(
                "{name}: audio priority refused ({}); stays at nice {previous_nice}",
                std::io::Error::last_os_error()
            );
            None
        }
    }
}

/// No-op on platforms without a real-time path (iOS workers rely on the
/// platform audio session).
#[cfg(not(any(
    windows,
    target_os = "macos",
    target_os = "linux",
    target_os = "android"
)))]
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
/// releases it at thread exit. Skipped on macOS and iOS, where the
/// callback runs on `CoreAudio`'s I/O thread, which is already
/// time-constraint scheduled, and on Android, where `AAudio`'s callback
/// thread already runs at the framework's audio priority (`SCHED_FIFO`
/// in low-latency mode) — our nice −16 could only lower it.
pub(crate) fn promote_callback_thread(buffer_frames: u32, sample_rate: u32) {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    {
        let _ = (buffer_frames, sample_rate);
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
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
/// `IAudioClient3` territory, not this knob.
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

/// A buffer for an output that only keeps an app alive and plays a
/// chime (the iPhone's lobby, D51): 4096 frames, about 85 ms, inside
/// what the device allows — not 10 ms, about 100 callbacks a second for
/// hours in the background (review 2026-10, P1-7).
pub fn request_quiet_buffer(supported: &SupportedBufferSize) -> BufferSize {
    match supported {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(4096u32.clamp(*min, *max)),
        SupportedBufferSize::Unknown => BufferSize::Fixed(4096),
    }
}

// ── Worker wakeups ──────────────────────────────────────────────────────

/// How long a worker blocked on its device latch waits before looking
/// again on its own. Only a safety net: every backend rings the latch
/// on each callback, and a stop rings it too, so this is not a tick
/// (review P1-4: the 5 and 10 ms timers beside the latch woke the
/// voice workers 100–200 times a second for no audio).
pub(crate) const LATCH_SAFETY_WAIT: Duration = Duration::from_millis(200);

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
unsafe extern "system" {
    fn timeBeginPeriod(u_period: u32) -> u32;
    fn timeEndPeriod(u_period: u32) -> u32;
}

#[cfg(windows)]
#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(
        task_name: *const u16,
        task_index: *mut u32,
    ) -> *mut core::ffi::c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut core::ffi::c_void) -> i32;
}
