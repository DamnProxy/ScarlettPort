//! `VICTORY_NATIVE_PROBE=1` (diagnostic, off by default): counts CPU reads
//! and writes of the memory milestone 1 would move onto the card
//! (NATIVE-BOUNDARY §5 questions 1 and 2): the render-target pool's mapped
//! pages, the GPU-only reservations (protection 0x801) and the registered
//! page pools (device 0x250), outside the read-back ring.
//!
//! Every 5 s a thread arms each mapped page of those ranges once
//! (`PAGE_NOACCESS`, its protection remembered); the first touch faults
//! here, is counted (read or write, by the title's code or by a host
//! module, by range kind) and the page gets its protection back. Pages
//! the lazy remap table shows without a view (`VICTORY_REMAP_LAZY`) are
//! counted where that handler makes a view on a touch instead. The counts
//! are logged at each tick, then the pages are armed again.
//!
//! Pages are armed only once the world runs (60 frames in a row of 1,500
//! draws or more; boot loads through kernel reads and writes the arming
//! would fail, and run C4N-PROBE stalled on the loading screen). The
//! runtime's own kernel reads and writes ([`release`], from
//! `crate::host` reads and `crate::watch::writing`) give armed pages their
//! access back first and count as host touches.
//!
//! shortcut: "title code" is a faulting instruction outside every module
//! the system loader knows (the title's image is mapped by hand); a page
//! the runtime re-protects meanwhile (its write guards) may lose its arming
//! until the next tick, so counts are a floor.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::pool::PAGE;

/// Range kinds.
pub const RT: usize = 0;
pub const GPU_ONLY: usize = 1;
pub const PAGE_POOL: usize = 2;
const KINDS: [&str; 3] = ["rt-pool", "gpu-only", "page-pool"];

/// [kind][title, host][read, write]
static COUNTS: [[[AtomicU64; 2]; 2]; 3] = [const { [const { [const { AtomicU64::new(0) }; 2] }; 2] }; 3];
/// Touches inside the read-back ring (not counted above), lazy views made,
/// pages armed at the last tick, ticks.
static OTHER: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Armed page → (protection to give back, kind).
static ARMED: Mutex<Option<HashMap<usize, (u32, u8)>>> = Mutex::new(None);
/// Title instruction addresses that touched, with counts (first 64).
static SITES: Mutex<Vec<(u64, u8, u64)>> = Mutex::new(Vec::new());

pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = victory_core::mem2::switch("VICTORY_NATIVE_PROBE");
        if on {
            tracing::warn!("VICTORY_NATIVE_PROBE: CPU touches of the RT pool, GPU-only and page-pool ranges are counted");
            start();
        }
        on
    })
}

fn armed() -> std::sync::MutexGuard<'static, Option<HashMap<usize, (u32, u8)>>> {
    ARMED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether `rip` lies in a module the system loader knows.
fn host_code(rip: u64) -> bool {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
    };
    let mut m = std::ptr::null_mut();
    // SAFETY: a lookup by address; no reference is taken.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            rip as usize as *const u16,
            &mut m,
        ) != 0
    }
}

/// Counts one touch of `at` (`access`: 0 read, 1 write, 8 execute).
fn count(at: usize, kind: usize, access: usize, rip: u64) {
    if crate::readback::tagged_in(at as u64, 1) {
        OTHER[0].fetch_add(1, Relaxed);
        return;
    }
    let host = host_code(rip);
    let w = usize::from(access == 1);
    COUNTS[kind][usize::from(host)][w].fetch_add(1, Relaxed);
    if !host {
        let mut s = SITES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = s.iter_mut().find(|e| e.0 == rip && e.1 == kind as u8) {
            e.2 += 1;
        } else if s.len() < 64 {
            s.push((rip, kind as u8, 1));
        }
    }
}

/// Frames in a row with 1,500 draws or more; the world runs at 60.
static WORLD_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Whether the world runs (60 frames in a row of 1,500 draws or more).
pub(crate) fn world() -> bool {
    WORLD_FRAMES.load(Relaxed) >= 60
}

/// Presents since the start.
static FRAMES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn frames() -> u64 {
    FRAMES.load(Relaxed)
}

/// At each present: `drawn` draws since the start.
pub(crate) fn frame_drew(drawn: u64) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let before = LAST.swap(drawn, Relaxed);
    FRAMES.fetch_add(1, Relaxed);
    if WORLD_FRAMES.load(Relaxed) >= 60 {
        return;
    }
    if drawn.saturating_sub(before) >= 1500 {
        if WORLD_FRAMES.fetch_add(1, Relaxed) + 1 == 60 && on() {
            tracing::warn!("native probe: the world runs; arming from the next tick");
        }
    } else {
        WORLD_FRAMES.store(0, Relaxed);
    }
}

/// The runtime is about to read or write `[at, at + len)` itself (through
/// the kernel, which an armed page would fail): armed pages there get
/// their access back, counted as host touches.
pub(crate) fn release(at: usize, len: usize, write: bool) {
    if len == 0 || WORLD_FRAMES.load(Relaxed) < 60 || !on() {
        return;
    }
    use windows_sys::Win32::System::Memory::VirtualProtect;
    let mut g = armed();
    let Some(a) = g.as_mut() else { return };
    if a.is_empty() {
        return;
    }
    let mut page = at & !(PAGE - 1);
    while page < at + len {
        if let Some((protect, kind)) = a.remove(&page) {
            let mut old = 0;
            // SAFETY: a page this probe took the access of.
            unsafe { VirtualProtect(page as *const _, PAGE, protect, &mut old) };
            COUNTS[kind as usize][1][usize::from(write)].fetch_add(1, Relaxed);
        }
        page += PAGE;
    }
}

/// The lazy remap handler made a view of `at` on a touch.
pub(crate) fn lazy_touch(at: usize, access: usize, rip: u64) {
    // Only the title's own code demotes: the runtime's lazy views, refreshes
    // and detiling touched 1,931 pages in a run the probe counted no title
    // touch in.
    if super::gpumem() && !host_code(rip) {
        super::gpumem::touched(at);
    }
    if !on() {
        return;
    }
    OTHER[1].fetch_add(1, Relaxed);
    if let Some(kind) = super::kind_of(at) {
        count(at, kind, access, rip);
    }
}

unsafe extern "system" fn on_fault(info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH};
    use windows_sys::Win32::System::Memory::VirtualProtect;
    // SAFETY: the system passes the exception being dispatched.
    let (r, rip) = unsafe { (&*(*info).ExceptionRecord, (*(*info).ContextRecord).Rip) };
    if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let at = r.ExceptionInformation[1];
    let page = at & !(PAGE - 1);
    let Some((protect, kind)) = armed().as_mut().and_then(|a| a.remove(&page)) else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let mut old = 0;
    // SAFETY: a page this probe took the access of; it gets it back.
    unsafe { VirtualProtect(page as *const _, PAGE, protect, &mut old) };
    count(at, kind as usize, r.ExceptionInformation[0], rip);
    EXCEPTION_CONTINUE_EXECUTION
}

/// Puts the handler first (again: handlers added later go ahead of it).
fn install(handle: &mut usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, RemoveVectoredExceptionHandler};
    // SAFETY: a handler that lives as long as the process; the old
    // registration is this one's.
    unsafe {
        if *handle != 0 {
            RemoveVectoredExceptionHandler(*handle as *const _);
        }
        *handle = AddVectoredExceptionHandler(1, Some(on_fault)) as usize;
    }
}

/// Arms the mapped pages of `[from, to)` of `kind` that are readable or
/// writable now.
fn arm(from: usize, to: usize, kind: usize, armed: &mut HashMap<usize, (u32, u8)>, n: &mut u64) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualProtect, VirtualQuery,
    };
    let mut at = from & !(PAGE - 1);
    while at < to {
        if armed.contains_key(&at) || crate::readback::tagged_in(at as u64, PAGE as u64) {
            at += PAGE;
            continue;
        }
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query.
        if unsafe { VirtualQuery(at as *const _, &mut m, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            return;
        }
        let end = (m.BaseAddress as usize + m.RegionSize).max(at + 1);
        if m.State != MEM_COMMIT || m.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 || m.Protect == 0 {
            at = end.next_multiple_of(PAGE);
            continue;
        }
        let stop = end.min(to);
        while at + PAGE <= stop {
            let mut old = 0;
            armed.insert(at, (m.Protect, kind as u8));
            // SAFETY: a mapped page of the title's; the handler gives its
            // access back on the first touch.
            if unsafe { VirtualProtect(at as *const _, PAGE, PAGE_NOACCESS, &mut old) } == 0 {
                armed.remove(&at);
            } else {
                *n += 1;
            }
            at += PAGE;
        }
        at = at.max(end.next_multiple_of(PAGE));
    }
}

fn line() {
    let c = |k: usize, h: usize, w: usize| COUNTS[k][h][w].load(Relaxed);
    for (k, name) in KINDS.iter().enumerate() {
        tracing::warn!(
            "native probe {name}: title reads {} writes {}, host reads {} writes {}",
            c(k, 0, 0),
            c(k, 0, 1),
            c(k, 1, 0),
            c(k, 1, 1)
        );
    }
    let s = SITES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut top = s;
    top.sort_by_key(|e| std::cmp::Reverse(e.2));
    let sites: Vec<String> = top.iter().take(8).map(|(rip, k, n)| format!("{rip:#x}/{}x{n}", KINDS[*k as usize])).collect();
    tracing::warn!(
        "native probe: readback-ring touches {}, lazy views {}, pages armed {}, tick {}, ranges {}, rt pool mapped {} (maps/unmaps/other {:?}), title sites [{}]",
        OTHER[0].load(Relaxed),
        OTHER[1].load(Relaxed),
        OTHER[2].load(Relaxed),
        OTHER[3].load(Relaxed),
        super::probe_ranges().len(),
        super::pool().as_ref().map_or(0, |p| p.mapped_len()),
        super::pool().as_ref().map_or([0; 3], |p| p.counts),
        sites.join(" ")
    );
}

fn start() {
    let r = std::thread::Builder::new().name("victory-native-probe".into()).spawn(|| {
        let mut handle = 0usize;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            OTHER[3].fetch_add(1, Relaxed);
            line();
            if WORLD_FRAMES.load(Relaxed) < 60 {
                continue;
            }
            install(&mut handle);
            let ranges = super::probe_ranges();
            let mut n = 0;
            let mut g = armed();
            let a = g.get_or_insert_with(HashMap::new);
            for (from, to, kind) in ranges {
                arm(from, to, kind, a, &mut n);
            }
            OTHER[2].store(a.len() as u64, Relaxed);
        }
    });
    if let Err(e) = r {
        tracing::warn!(%e, "VICTORY_NATIVE_PROBE: no thread");
    }
}
