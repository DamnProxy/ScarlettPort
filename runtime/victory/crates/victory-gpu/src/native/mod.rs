//! Native GPU memory (NATIVE-BOUNDARY.md §4, milestone 1).
//!
//! - [`pool`]: the render-target pool's pages as tiles.
//! - [`probe`]: `VICTORY_NATIVE_PROBE=1`, counts CPU touches of the ranges
//!   milestone 1 moves onto the card.

pub mod pool;
#[cfg(windows)]
pub(crate) mod gpumem;
#[cfg(windows)]
pub(crate) mod buffers;
pub mod table;
#[cfg(windows)]
pub mod probe;

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};

/// The render-target pool, once the title allocated it.
static POOL: Mutex<Option<pool::Pool>> = Mutex::new(None);
/// Its section (the physical pages' handle), read without the lock; 0: none.
static POOL_SECTION: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn pool() -> MutexGuard<'static, Option<pool::Pool>> {
    POOL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pool's size in bytes (`VICTORY_NATIVE_RTPOOL_BYTES` overrides).
fn pool_bytes() -> usize {
    static B: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        std::env::var("VICTORY_NATIVE_RTPOOL_BYTES")
            .ok()
            .and_then(|v| {
                let v = v.trim();
                v.strip_prefix("0x").map_or_else(|| v.parse().ok(), |h| usize::from_str_radix(h, 16).ok())
            })
            .unwrap_or(pool::DEFAULT_BYTES)
    })
}

/// Whether anything here needs the page mappings.
pub fn tracking() -> bool {
    #[cfg(windows)]
    if probe::on() {
        return true;
    }
    gpumem()
}

/// `VICTORY_NATIVE_GPUMEM=1` (off by default, boot only): milestone 1.
pub fn gpumem() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = victory_core::mem2::switch("VICTORY_NATIVE_GPUMEM");
        if on {
            tracing::warn!("VICTORY_NATIVE_GPUMEM: GPU-only resources live in D3D12 memory");
        }
        on
    })
}

/// `XMemAllocatePhysicalPages` gave `n` pages, one section (`section`,
/// its handle): the render-target pool if its size matches.
pub fn physical_pages(section: usize, n: usize) {
    if !tracking() {
        return;
    }
    let bytes = n * pool::PAGE;
    if bytes != pool_bytes() {
        tracing::debug!(n, bytes, "native: physical pages, not the RT pool");
        return;
    }
    let mut p = pool();
    if p.is_some() {
        tracing::warn!(n, "native: a second batch the RT pool's size, ignored");
        return;
    }
    *p = Some(pool::Pool::new(0, 1, n));
    POOL_SECTION.store(section, Relaxed);
    tracing::info!(n, "native: the RT pool's physical pages");
}

/// `[at, at + len)` shows the section's pages from `index` on.
pub fn shown(at: usize, len: usize, section: usize, index: usize) {
    let pool_section = POOL_SECTION.load(Relaxed);
    if pool_section == 0 {
        return;
    }
    let first = {
        let mut g = pool();
        let Some(p) = g.as_mut() else { return };
        let first = p.mapped_len() == 0 && section == pool_section;
        for k in 0..len / pool::PAGE {
            let a = at + k * pool::PAGE;
            if section == pool_section {
                p.map(a, index + k);
            } else if p.lookup(a).is_some() {
                p.unmap(a, 1);
            }
        }
        first
    };
    // (Not under the pool's lock: the native side takes its own first.)
    #[cfg(windows)]
    if first {
        gpumem::pool_page_at(at as u64);
    }
    let _ = first;
}

/// `[at, at + len)` shows nothing now.
pub fn unmapped(at: usize, len: usize) {
    if POOL_SECTION.load(Relaxed) == 0 {
        return;
    }
    if let Some(p) = pool().as_mut() {
        p.unmap(at, len.div_ceil(pool::PAGE));
    }
}

/// Which probed kind `at` is in.
pub(crate) fn kind_of(at: usize) -> Option<usize> {
    #[cfg(windows)]
    {
        if pool().as_ref().is_some_and(|p| p.lookup(at).is_some()) {
            return Some(probe::RT);
        }
        if crate::watch::gpu_only_ranges().iter().any(|&(a, b)| (a..b).contains(&at)) {
            return Some(probe::GPU_ONLY);
        }
        if crate::resource::page_pool_ranges().iter().any(|&(a, b)| (a..b).contains(&(at as u64))) {
            return Some(probe::PAGE_POOL);
        }
    }
    let _ = at;
    None
}

/// The probed ranges: mapped RT-pool pages joined into runs, GPU-only
/// reservations, page pools.
#[cfg(windows)]
pub(crate) fn probe_ranges() -> Vec<(usize, usize, usize)> {
    let mut out: Vec<(usize, usize, usize)> = Vec::new();
    if let Some(p) = pool().as_ref() {
        for (a, _) in p.mapped() {
            match out.last_mut() {
                Some(l) if l.1 == a => l.1 = a + pool::PAGE,
                _ => out.push((a, a + pool::PAGE, probe::RT)),
            }
        }
    }
    out.extend(crate::watch::gpu_only_ranges().into_iter().map(|(a, b)| (a, b, probe::GPU_ONLY)));
    out.extend(
        crate::resource::page_pool_ranges().into_iter().map(|(a, b)| (a as usize, b as usize, probe::PAGE_POOL)),
    );
    out
}
