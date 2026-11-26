//! Which cores of a hybrid processor our threads ask for (fxceless
//! victory-fork 0b0074e). Off by default here: `VICTORY_FAST_CORES=1` keeps
//! the submit, relay, late-wait and copy-back threads on the fast cores;
//! `VICTORY_DISK_CORES=slow` keeps the DirectStorage workers on the slow
//! ones. A set of processors selected for a thread is a preference the
//! scheduler keeps unless nothing of it is free.

/// The system's processor sets of group 0: each one's id, its logical
/// processor and its efficiency class (higher is faster).
#[cfg(windows)]
fn cpu_sets() -> Vec<(u32, u8, u8)> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            sets.push((set.Id, set.LogicalProcessorIndex, set.EfficiencyClass));
        }
        at += info.Size as usize;
    }
    sets
}

/// The set ids of the fastest class, or of every slower one; empty where
/// all are alike.
fn pick(sets: &[(u32, u8, u8)], fast: bool) -> Vec<u32> {
    let best = sets.iter().map(|c| c.2).max().unwrap_or(0);
    let picked: Vec<u32> = sets
        .iter()
        .filter(|c| (c.2 == best) == fast)
        .map(|c| c.0)
        .collect();
    if picked.len() == sets.len() {
        return Vec::new();
    }
    picked
}

/// Whether `VICTORY_FAST_CORES=1` (off by default; fx 0b0074e has it on
/// and `=0` for off).
pub fn fast_cores_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FAST_CORES").is_some_and(|v| v == "1"))
}

/// Asks the system to run the calling thread on the fast cores only, where
/// the processor has two kinds and `VICTORY_FAST_CORES=1`.
pub fn keep_on_fast_cores(what: &str) {
    if !fast_cores_on() {
        return;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
        let sets = cpu_sets();
        let fast = pick(&sets, true);
        if fast.is_empty() {
            return;
        }
        // SAFETY: our own thread and a list of the system's own set ids.
        let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), fast.as_ptr(), fast.len() as u32) };
        if ok == 0 {
            tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
        } else {
            tracing::info!(what, fast = fast.len(), of = sets.len(), "the thread is kept on the fast cores (VICTORY_FAST_CORES=1)");
        }
    }
    #[cfg(not(windows))]
    let _ = what;
}

/// Asks the system to run the calling thread on the slow cores only, where
/// the processor has two kinds and at least eight slow logical processors.
/// True if it was asked. The caller checks `VICTORY_DISK_CORES=slow`.
pub fn keep_on_slow_cores(what: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
        let sets = cpu_sets();
        let slow = pick(&sets, false);
        if slow.len() < 8 {
            return false;
        }
        // SAFETY: our own thread and a list of the system's own set ids.
        let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), slow.as_ptr(), slow.len() as u32) } != 0;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(what, slow = slow.len(), of = sets.len(), ok, "helper threads are kept on the slow cores: the first one");
        });
        ok
    }
    #[cfg(not(windows))]
    {
        let _ = what;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::pick;

    #[test]
    fn picks_by_efficiency_class() {
        // 8 fast (class 1) and 12 slow (class 0).
        let sets: Vec<(u32, u8, u8)> = (0..20).map(|i| (256 + i, i as u8, u8::from(i < 8))).collect();
        assert_eq!(pick(&sets, true), (256..264).collect::<Vec<_>>());
        assert_eq!(pick(&sets, false).len(), 12);
        // All alike: nothing to ask for.
        let alike: Vec<(u32, u8, u8)> = (0..8).map(|i| (i, i as u8, 0)).collect();
        assert!(pick(&alike, true).is_empty());
        assert!(pick(&alike, false).is_empty());
        assert!(!super::fast_cores_on() || std::env::var_os("VICTORY_FAST_CORES").is_some());
    }
}
