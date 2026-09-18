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
// (XNU osfmk/mach/task_info.h; psapi.h), zero new dependencies.

// The workspace otherwise avoids `unsafe`; these two single-syscall
// wrappers are the deliberate exception (see module docs).
#![allow(unsafe_code)]

/// Bytes of memory as the platform's own task monitor would show for
/// this process, or `None` when sysinfo's RSS already matches the
/// local convention (Linux) or the syscall fails.
#[cfg(target_os = "macos")]
pub fn native_monitor_memory() -> Option<u64> {
    // task_vm_info, rev1 layout — ends at phys_footprint (XNU
    // osfmk/mach/task_info.h). 8 + 4 + 4 + 17×8 = 152 bytes, i.e.
    // 38 natural_t units. The kernel fills at most `count` units and
    // writes back how many it actually filled, so requesting the rev1
    // size works on every macOS new enough to run this app.
    #[repr(C)]
    struct TaskVmInfo {
        _virtual_size: u64,
        _region_count: i32,
        _page_size: i32,
        _resident_size: u64,
        _resident_size_peak: u64,
        _device: u64,
        _device_peak: u64,
        _internal: u64,
        _internal_peak: u64,
        _external: u64,
        _external_peak: u64,
        _reusable: u64,
        _reusable_peak: u64,
        _purgeable_volatile_pmap: u64,
        _purgeable_volatile_resident: u64,
        _purgeable_volatile_virtual: u64,
        _compressed: u64,
        _compressed_peak: u64,
        _compressed_lifetime: u64,
        phys_footprint: u64,
    }

    /// `#define TASK_VM_INFO 22` (task_info.h).
    const TASK_VM_INFO: u32 = 22;
    const KERN_SUCCESS: i32 = 0;

    unsafe extern "C" {
        /// The calling task's port — what the `mach_task_self()` macro
        /// expands to in C.
        static mach_task_self_: u32;
        fn task_info(
            target_task: u32,
            flavor: u32,
            task_info_out: *mut i32,
            task_info_out_cnt: *mut u32,
        ) -> i32;
    }

    // All-integer struct: zeroed is a valid initial value.
    let mut info: TaskVmInfo = unsafe { std::mem::zeroed() };
    let mut count = (size_of::<TaskVmInfo>() / size_of::<u32>()) as u32;

    let kr = unsafe {
        task_info(
            mach_task_self_,
            TASK_VM_INFO,
            std::ptr::from_mut(&mut info).cast::<i32>(),
            &mut count,
        )
    };

    // Guard against a kernel that filled fewer units than our struct —
    // phys_footprint would then still be our zero, not data.
    if kr == KERN_SUCCESS && count as usize * size_of::<u32>() >= size_of::<TaskVmInfo>() {
        Some(info.phys_footprint)
    } else {
        None
    }
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
