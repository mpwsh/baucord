// resources.rs — own-process memory the way the platform's monitor
// reports it.
//
// sysinfo's `memory()` returns RSS (total resident pages, including
// clean shared framework/dylib mappings). That is NOT the number users
// compare against:
//
//   - macOS Activity Monitor "Memory"  = phys_footprint
//     (private dirty + compressed — the RAM the app truly "owns")
//   - Windows Task Manager "Memory"    = private working set
//   - Linux top/htop "RES"             = RSS (sysinfo already matches)
//
// RSS routinely reads 1.5–2× higher than footprint for framework-heavy
// GUI apps, which reads like a leak to users. This module answers with
// the native figure where RSS diverges from it, `None` where RSS is
// already the local convention.
//
// The syscalls are declared by hand instead of pulling mach2/windows
// crates: one call each, layouts verified against the platform headers
// (XNU osfmk/mach/task_info.h, in `app_core::usage`; psapi.h), zero
// new dependencies.
//
// Also here: the [`Sampler`] behind the resource line and `/mem`. The
// desktop's is `sysinfo`; a phone's is `app_core::usage` — two system
// calls about itself, for which it carried that library (review
// P3-50).

// The workspace otherwise avoids `unsafe`; these two single-syscall
// wrappers are the deliberate exception (see module docs).
#![allow(unsafe_code)]

/// Bytes of memory as the platform's own task monitor would show for
/// this process, or `None` when sysinfo's RSS already matches the
/// local convention (Linux) or the syscall fails.
#[cfg(target_os = "macos")]
#[must_use]
pub fn native_monitor_memory() -> Option<u64> {
    // The physical footprint, from the one `task_info` the iOS build
    // also makes.
    app_core::usage::memory()
}

/// Bytes of memory as the platform's own task monitor would show for
/// this process, or `None` when the syscall fails (the caller then
/// falls back to sysinfo's figure).
#[cfg(target_os = "windows")]
#[must_use]
pub fn native_monitor_memory() -> Option<u64> {
    // PROCESS_MEMORY_COUNTERS_EX2 (psapi.h). The EX2 tail
    // (PrivateWorkingSetSize) is what Task Manager's "Memory" column
    // shows, supported on Windows 10 21H2+ / Windows 11. On older
    // builds the EX2-sized call fails and we retry with the EX size,
    // taking PrivateUsage (private commit) — still far closer to Task
    // Manager than the shared-inclusive working set.
    #[repr(C)]
    struct ProcessMemoryCountersEx2 {
        cb: u32,
        _page_fault_count: u32,
        _peak_working_set_size: usize,
        _working_set_size: usize,
        _quota_peak_paged_pool_usage: usize,
        _quota_paged_pool_usage: usize,
        _quota_peak_non_paged_pool_usage: usize,
        _quota_non_paged_pool_usage: usize,
        _pagefile_usage: usize,
        _peak_pagefile_usage: usize,
        private_usage: usize,
        private_working_set_size: usize,
        _shared_commit_usage: u64,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCountersEx2,
            cb: u32,
        ) -> i32;
    }

    let full = size_of::<ProcessMemoryCountersEx2>() as u32;
    // EX layout: the two DWORDs plus nine SIZE_Ts, ending at
    // PrivateUsage — accepted by every Windows this app can run on.
    let ex_size = (size_of::<u32>() * 2 + size_of::<usize>() * 9) as u32;

    let mut counters: ProcessMemoryCountersEx2 = unsafe { std::mem::zeroed() };

    unsafe {
        let process = GetCurrentProcess();

        counters.cb = full;
        if K32GetProcessMemoryInfo(process, &raw mut counters, full) != 0
            && counters.private_working_set_size != 0
        {
            return Some(counters.private_working_set_size as u64);
        }

        counters = std::mem::zeroed();
        counters.cb = ex_size;
        if K32GetProcessMemoryInfo(process, &raw mut counters, ex_size) != 0 {
            return Some(counters.private_usage as u64);
        }
    }

    None
}

/// Linux (and anything else): sysinfo's RSS is already what top/htop
/// show as RES, so there is no divergence to correct.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn native_monitor_memory() -> Option<u64> {
    None
}

/// This process's CPU and memory, from `sysinfo`.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod sampler {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};

    pub struct Sampler {
        sys: sysinfo::System,
        /// Resolved once: a process's id does not change.
        pid: Option<sysinfo::Pid>,
    }

    impl Sampler {
        #[must_use]
        pub fn new() -> Self {
            Self {
                sys: sysinfo::System::new(),
                pid: sysinfo::get_current_pid().ok(),
            }
        }

        /// Resident memory, in bytes.
        pub fn resident(&mut self) -> Option<u64> {
            let pid = self.pid?;
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory(),
            );
            self.sys.process(pid).map(sysinfo::Process::memory)
        }

        /// Percent of one core since the last call, and resident
        /// memory in bytes.
        pub fn sample(&mut self) -> Option<(f32, u64)> {
            let pid = self.pid?;
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_memory().with_cpu(),
            );
            let process = self.sys.process(pid)?;
            Some((process.cpu_usage(), process.memory()))
        }
    }
}

/// This process's CPU and memory, asked of the system directly.
#[cfg(any(target_os = "android", target_os = "ios"))]
mod sampler {
    pub struct Sampler {
        cpu: app_core::usage::CpuMeter,
    }

    // The same methods as the desktop's, which keeps state for both.
    #[allow(clippy::unused_self)]
    impl Sampler {
        #[must_use]
        pub fn new() -> Self {
            Self {
                cpu: app_core::usage::CpuMeter::new(),
            }
        }

        /// The memory the system's own monitor shows, in bytes.
        pub fn resident(&mut self) -> Option<u64> {
            app_core::usage::memory()
        }

        /// Percent of one core since the last call (0 for the first,
        /// which has nothing to measure from), and memory in bytes.
        pub fn sample(&mut self) -> Option<(f32, u64)> {
            let cpu = self.cpu.percent_of_one_core().unwrap_or(0.0);
            Some((cpu, app_core::usage::memory()?))
        }
    }
}

pub use sampler::Sampler;

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}
