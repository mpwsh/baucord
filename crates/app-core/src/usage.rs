//! This process's own CPU time and memory, read from the system
//! directly: what the resource line and `/mem` show, on every
//! platform.
//!
//! Each is one call: `getrusage` (Unix) or `GetProcessTimes`
//! (Windows) for the CPU time; `/proc/self/statm` (Android, Linux),
//! `task_info` (iOS, macOS) or `K32GetProcessMemoryInfo` (Windows) for
//! the memory. The phones carried `sysinfo` for these two numbers
//! (review P3-50), and then the desktop did too, beside a second
//! sampler of its own (review 2026-10, P3-53).

// One system call each, declared by hand where `libc` has no binding.
#![allow(unsafe_code)]

use std::time::{Duration, Instant};

/// Percent of one core used between two looks at the process's CPU
/// time; `None` for the first look, or when no time has passed.
#[must_use]
pub fn percent_of_one_core(
    earlier: Option<(Instant, Duration)>,
    now: Instant,
    used: Duration,
) -> Option<f32> {
    let (then, used_then) = earlier?;
    let wall = now.saturating_duration_since(then).as_secs_f32();
    if wall <= 0.0 {
        return None;
    }
    Some(used.saturating_sub(used_then).as_secs_f32() / wall * 100.0)
}

/// The resident pages in a `/proc/<pid>/statm` line (its second
/// field), as bytes.
#[cfg(any(target_os = "linux", target_os = "android", test))]
#[must_use]
pub(crate) fn statm_resident(statm: &str, page_bytes: u64) -> Option<u64> {
    let pages: u64 = statm.split_ascii_whitespace().nth(1)?.parse().ok()?;
    pages.checked_mul(page_bytes)
}

/// The CPU use of this process from one call to the next.
#[derive(Debug, Default)]
pub struct CpuMeter {
    last: Option<(Instant, Duration)>,
}

impl CpuMeter {
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Percent of one core since the last call. `None` the first
    /// time, and where the system does not say.
    pub fn percent_of_one_core(&mut self) -> Option<f32> {
        let now = Instant::now();
        let used = cpu_time()?;
        let percent = percent_of_one_core(self.last, now, used);
        self.last = Some((now, used));
        percent
    }
}

/// CPU time this process has used so far, in user and kernel mode.
#[cfg(unix)]
#[must_use]
pub(crate) fn cpu_time() -> Option<Duration> {
    // SAFETY: `getrusage` fills the struct it is handed; all-zero is a
    // valid value of it (integers and two `timeval`s).
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) != 0 {
            return None;
        }
        usage
    };
    let time = |tv: libc::timeval| {
        let secs = u64::try_from(tv.tv_sec).ok()?;
        let micros = u32::try_from(tv.tv_usec).ok()?;
        Some(Duration::new(secs, micros.checked_mul(1000)?))
    };
    Some(time(usage.ru_utime)? + time(usage.ru_stime)?)
}

#[cfg(windows)]
#[must_use]
pub(crate) fn cpu_time() -> Option<Duration> {
    /// `FILETIME` (minwinbase.h): 100 ns units, in two halves.
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetProcessTimes(
            process: *mut core::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    let (mut creation, mut exit) = (FileTime::default(), FileTime::default());
    let (mut kernel, mut user) = (FileTime::default(), FileTime::default());
    // SAFETY: four valid out-pointers; the pseudo-handle needs no close.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = |t: &FileTime| (u64::from(t.high) << 32) | u64::from(t.low);
    let total = ticks(&kernel).checked_add(ticks(&user))?;
    Some(Duration::from_nanos(total.checked_mul(100)?))
}

#[cfg(not(any(unix, windows)))]
#[must_use]
pub(crate) fn cpu_time() -> Option<Duration> {
    None
}

/// The memory this process holds, as the platform's own monitor counts
/// it: resident pages on Android and Linux (`top`'s RES), the physical
/// footprint on iOS and macOS (Activity Monitor's "Memory" — private
/// dirty and compressed pages; resident size counts shared framework
/// pages too and reads 1.5–2× higher), the private working set on
/// Windows (Task Manager's "Memory"). `None` where none is had.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[must_use]
pub fn memory() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    // SAFETY: no preconditions. 16 KiB on newer Android devices, so
    // asked, never assumed.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    statm_resident(&statm, u64::try_from(page).ok()?)
}

#[cfg(target_vendor = "apple")]
#[must_use]
pub fn memory() -> Option<u64> {
    // task_vm_info, rev1 layout — ends at phys_footprint (XNU
    // osfmk/mach/task_info.h). 8 + 4 + 4 + 17×8 = 152 bytes, i.e.
    // 38 natural_t units. The kernel fills at most `count` units and
    // writes back how many it actually filled, so requesting the rev1
    // size works on every system new enough to run this app.
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

    /// `#define TASK_VM_INFO 22` (`task_info.h`).
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
    // SAFETY: as above.
    let mut info: TaskVmInfo = unsafe { std::mem::zeroed() };
    // 38 units: it fits.
    #[allow(clippy::cast_possible_truncation)]
    let mut count = (size_of::<TaskVmInfo>() / size_of::<u32>()) as u32;

    // SAFETY: `info` is `count` units long, as the call is told, and
    // the task is our own.
    let kr = unsafe {
        task_info(
            mach_task_self_,
            TASK_VM_INFO,
            (&raw mut info).cast::<i32>(),
            &raw mut count,
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

#[cfg(windows)]
#[must_use]
pub fn memory() -> Option<u64> {
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

    // Both sizes are a few dozen bytes.
    #[allow(clippy::cast_possible_truncation)]
    let full = size_of::<ProcessMemoryCountersEx2>() as u32;
    // EX layout: the two DWORDs plus nine SIZE_Ts, ending at
    // PrivateUsage — accepted by every Windows this app can run on.
    #[allow(clippy::cast_possible_truncation)]
    let ex_size = (size_of::<u32>() * 2 + size_of::<usize>() * 9) as u32;

    // SAFETY: all-integer struct, so zeroed is a valid value; each call
    // is told a size no larger than the struct; the pseudo-handle needs
    // no close.
    unsafe {
        let mut counters: ProcessMemoryCountersEx2 = std::mem::zeroed();
        let process = GetCurrentProcess();

        counters.cb = full;
        if K32GetProcessMemoryInfo(process, &raw mut counters, full) != 0
            && counters.private_working_set_size != 0
        {
            return u64::try_from(counters.private_working_set_size).ok();
        }

        counters = std::mem::zeroed();
        counters.cb = ex_size;
        if K32GetProcessMemoryInfo(process, &raw mut counters, ex_size) != 0 {
            return u64::try_from(counters.private_usage).ok();
        }
    }
    None
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
#[must_use]
pub fn memory() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_is_time_used_over_time_passed() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let used = Duration::from_millis;
        assert_eq!(
            percent_of_one_core(None, at(0), used(5)),
            None,
            "first look"
        );
        // 500 ms of CPU over 2 s: a quarter of a core.
        let quarter = percent_of_one_core(Some((at(0), used(100))), at(2000), used(600)).unwrap();
        assert!((quarter - 25.0).abs() < 0.01, "{quarter}");
        // Two threads busy for the whole second: two cores.
        let two = percent_of_one_core(Some((at(0), used(0))), at(1000), used(2000)).unwrap();
        assert!((two - 200.0).abs() < 0.01, "{two}");
        // No time passed, or a clock that went back: nothing to say.
        assert_eq!(
            percent_of_one_core(Some((at(10), used(1))), at(10), used(2)),
            None
        );
        // CPU time never goes back; if it seems to, that is 0, not a wrap.
        let none = percent_of_one_core(Some((at(0), used(50))), at(1000), used(40)).unwrap();
        assert!(none.abs() < f32::EPSILON);
    }

    #[test]
    fn statm_gives_the_resident_pages_in_bytes() {
        // size resident shared text lib data dt
        assert_eq!(
            statm_resident("52345 6789 1234 56 0 7890 0\n", 4096),
            Some(6789 * 4096)
        );
        assert_eq!(statm_resident("52345 6789 1234", 16384), Some(6789 * 16384));
        assert_eq!(statm_resident("", 4096), None);
        assert_eq!(statm_resident("12", 4096), None);
        assert_eq!(statm_resident("12 many", 4096), None);
        assert_eq!(
            statm_resident("1 18446744073709551615", 4096),
            None,
            "no overflow"
        );
    }

    /// The real calls, on the machine the tests run on (the Android
    /// build uses the same two). Both figures are the whole process's,
    /// and the other tests run beside this one: nothing here depends on
    /// what they do, or on how busy the machine is.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_size_and_its_cpu_time_moves_when_it_works() {
        // Hold and touch 64 MiB: at least that much is resident.
        let block = vec![1u8; 64 * 1024 * 1024];
        let resident = memory().expect("statm");
        assert!(resident >= 64 * 1024 * 1024, "{resident} bytes resident");
        drop(block);

        let mut meter = CpuMeter::new();
        assert_eq!(meter.percent_of_one_core(), None, "first look");
        // Work until the process has used 100 ms more of CPU, however
        // long that takes on a busy machine.
        let start = cpu_time().expect("getrusage");
        let give_up = Instant::now() + Duration::from_secs(30);
        let mut n = 0u64;
        while cpu_time().expect("getrusage") < start + Duration::from_millis(100) {
            assert!(Instant::now() < give_up, "CPU time does not move");
            for _ in 0..100_000 {
                n = std::hint::black_box(n.wrapping_mul(31).wrapping_add(7));
            }
        }
        let busy = meter.percent_of_one_core().expect("second look");
        assert!(busy > 0.0, "{busy}% after 100 ms of work");
    }
}
