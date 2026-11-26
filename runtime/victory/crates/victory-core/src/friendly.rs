//! Sharing the PC with the desktop while a title runs.
//!
//! Defaults (always on): background pools never take every logical
//! processor ([`background_cap`]: at most N-2), and the threads that do
//! background work (decoding, pipeline making, loading) run below normal
//! ([`background_thread`]) so the title's own threads, our render and
//! submit threads, and the desktop come first.
//!
//! `VICTORY_FRIENDLY=1` goes further, for a PC that is also used for
//! other things while a title runs:
//! * the process's default CPU sets leave out the last two logical
//!   processors (one SMT core pair), so the title's own threads (which
//!   call Windows directly) cannot take them either; explicit affinity the
//!   title sets still wins;
//! * background pools at most half the logical processors;
//! * a memory line every 30 s: working set, private bytes, the PC's free
//!   memory and commit left, page faults a second.
//!
//! The process priority class is left at Normal on purpose: Above Normal
//! raises every thread of the process (the title's dozens of workers
//! too), which is exactly what starves the desktop.

use std::sync::OnceLock;

/// `VICTORY_FRIENDLY=1`.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_FRIENDLY").is_ok_and(|v| v.trim() == "1"))
}

/// Logical processors.
pub fn logical() -> usize {
    std::thread::available_parallelism().map_or(4, usize::from)
}

/// The most threads a background pool may have: N-2 (at least 1); with
/// `VICTORY_FRIENDLY=1`, N/2.
pub fn background_cap() -> usize {
    cap_for(logical(), enabled())
}

fn cap_for(n: usize, friendly: bool) -> usize {
    if friendly { (n / 2).max(1) } else { n.saturating_sub(2).max(1) }
}

/// `wanted` held to [`background_cap`].
pub fn background_threads(wanted: usize) -> usize {
    wanted.clamp(1, background_cap())
}

/// The calling thread below normal priority (background work).
pub fn background_thread() {
    #[cfg(windows)]
    // SAFETY: a plain call on our own thread's pseudo handle.
    unsafe {
        sys::SetThreadPriority(sys::GetCurrentThread(), -1);
    }
}

/// The calling thread below normal only with `VICTORY_FRIENDLY=1` (pools
/// that sit near the frame path by default).
pub fn background_thread_if_friendly() {
    if enabled() {
        background_thread();
    }
}

/// Once at startup: with `VICTORY_FRIENDLY=1`, the CPU sets and the memory
/// line. Logs what it did either way.
pub fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let n = logical();
        if !enabled() {
            tracing::info!(logical = n, background_cap = background_cap(), "friendly: off (VICTORY_FRIENDLY=1 leaves two logical processors to the desktop)");
            return;
        }
        #[cfg(windows)]
        {
            let left_out = sys::leave_out_last(2);
            tracing::info!(logical = n, background_cap = background_cap(), left_out = ?left_out, "friendly: on: the process's default CPU sets leave out the last logical processors, background pools capped, process priority class left Normal");
            let _ = std::thread::Builder::new().name("victory-friendly-memory".into()).spawn(|| {
                sys::background();
                let mut faults = sys::memory().map_or(0, |m| m.faults);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                    if let Some(m) = sys::memory() {
                        let mb = |b: u64| b >> 20;
                        tracing::info!(
                            "friendly memory: working set {} MB, private {} MB; the PC: {} MB of {} MB free, {} MB commit left; {:.0} page faults/s",
                            mb(m.working), mb(m.private), mb(m.free), mb(m.total), mb(m.commit_left),
                            m.faults.saturating_sub(faults) as f64 / 30.0
                        );
                        faults = m.faults;
                    }
                }
            });
        }
    });
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(super) fn GetCurrentThread() -> *mut c_void;
        pub(super) fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn GetSystemCpuSetInformation(info: *mut u8, len: u32, returned: *mut u32, process: *mut c_void, flags: u32) -> i32;
        fn SetProcessDefaultCpuSets(process: *mut c_void, ids: *const u32, count: u32) -> i32;
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut ProcessMemoryCounters, cb: u32) -> i32;
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }

    /// Lowest priority and background mode for a watcher thread.
    pub(super) fn background() {
        // SAFETY: our own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), -2) };
    }

    /// The process's default CPU sets: every set but the `k` with the
    /// highest logical processor index in group 0. Returns the logical
    /// processors left out, or None when nothing was changed.
    pub(super) fn leave_out_last(k: usize) -> Option<Vec<u8>> {
        let mut len = 0u32;
        // SAFETY: a size query.
        unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, GetCurrentProcess(), 0) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        // SAFETY: a buffer of the size asked for.
        if unsafe { GetSystemCpuSetInformation(buf.as_mut_ptr(), len, &mut len, GetCurrentProcess(), 0) } == 0 {
            return None;
        }
        // SYSTEM_CPU_SET_INFORMATION: Size u32, Type u32, Id u32, Group u16,
        // LogicalProcessorIndex u8, ...
        let mut sets = Vec::new();
        let mut at = 0usize;
        while at + 15 <= len as usize {
            let size = u32::from_le_bytes(buf[at..at + 4].try_into().ok()?) as usize;
            let kind = u32::from_le_bytes(buf[at + 4..at + 8].try_into().ok()?);
            if size == 0 {
                break;
            }
            if kind == 0 {
                let id = u32::from_le_bytes(buf[at + 8..at + 12].try_into().ok()?);
                let group = u16::from_le_bytes(buf[at + 12..at + 14].try_into().ok()?);
                sets.push((group, buf[at + 14], id));
            }
            at += size;
        }
        if sets.len() <= k + 2 {
            return None;
        }
        sets.sort();
        let (kept, out) = sets.split_at(sets.len() - k);
        if out.iter().any(|s| s.0 != 0) {
            return None;
        }
        let ids: Vec<u32> = kept.iter().map(|s| s.2).collect();
        // SAFETY: ids the system gave us.
        let ok = unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), ids.as_ptr(), ids.len() as u32) } != 0;
        ok.then(|| out.iter().map(|s| s.1).collect())
    }

    pub(super) struct Memory {
        pub working: u64,
        pub private: u64,
        pub faults: u64,
        pub free: u64,
        pub total: u64,
        pub commit_left: u64,
    }

    pub(super) fn memory() -> Option<Memory> {
        let mut c = ProcessMemoryCounters { cb: size_of::<ProcessMemoryCounters>() as u32, ..Default::default() };
        // SAFETY: our own process and a sized structure.
        if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
            return None;
        }
        // SAFETY: plain data the call fills in.
        let mut m: MemoryStatusEx = unsafe { std::mem::zeroed() };
        m.length = size_of::<MemoryStatusEx>() as u32;
        // SAFETY: a sized structure.
        if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
            return None;
        }
        Some(Memory {
            working: c.working_set as u64,
            private: c.pagefile_usage as u64,
            faults: u64::from(c.page_fault_count),
            free: m.avail_phys,
            total: m.total_phys,
            commit_left: m.avail_page_file,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::cap_for;

    #[test]
    fn caps_leave_the_desktop_room() {
        assert_eq!(cap_for(12, false), 10);
        assert_eq!(cap_for(12, true), 6);
        assert_eq!(cap_for(2, false), 1);
        assert_eq!(cap_for(1, true), 1);
    }
}
