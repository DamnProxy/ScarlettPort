//! Host memory and the texture pass (after codec_d's cool_fox
//! "pareto-residency", ab75ac3e and 18bdf793, GPL-3.0): a refused
//! allocation holds back growth for a moment, and the speculative texture
//! pass leaves Windows commit to the title.
//!
//! Every behaviour is behind a live switch, off by default:
//! - `VICTORY_OOM_BACKOFF=1`: an `E_OUTOFMEMORY` from an upload buffer, a
//!   host texture or a heap holds back new texture heaps and staging blocks
//!   for 500 ms (what is free in those already made is still used), and the
//!   texture pass stops; its slots stay due.
//! - `VICTORY_COMMIT_GATE=1`: the texture pass stops while the available
//!   commit is under 512 MiB (sampled at most every 32 ms), and new texture
//!   heaps and staging blocks are not made under 1 GiB.
//! - `VICTORY_PREFETCH_CAP=1`: the pass's bytes per frame at most 128 MiB on
//!   a PC with 32 GB or less, 64 MiB under host pressure (or on such a PC
//!   with 1 GiB and more of staging alive), 16 MiB when commit is critical
//!   (under 1.5 GiB, or in a back-off).
#![cfg(windows)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

static OOM_BACKOFF: victory_core::live::Switch = victory_core::live::Switch::new("OOM_BACKOFF", 0);
static COMMIT_GATE: victory_core::live::Switch = victory_core::live::Switch::new("COMMIT_GATE", 0);
static PREFETCH_CAP: victory_core::live::Switch = victory_core::live::Switch::new("PREFETCH_CAP", 0);

/// Since the start: refusals seen, passes stopped by the back-off, by the
/// commit gate, growths held back.
pub(crate) static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

static PRESSURED: AtomicBool = AtomicBool::new(false);
static NEXT_REFRESH: AtomicU64 = AtomicU64::new(0);
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_AT: AtomicU64 = AtomicU64::new(0);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the call fills a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: as above.
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

fn now() -> u64 {
    crate::host::now_ms()
}

/// Notes an allocation's error; an `E_OUTOFMEMORY` starts a back-off
/// (whether the switch is on or not; only what reads it asks the switch).
pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        COUNTS[0].fetch_add(1, Ordering::Relaxed);
        RETRY_AT.fetch_max(now().saturating_add(500), Ordering::Relaxed);
    }
}

fn backing_off() -> bool {
    OOM_BACKOFF.on() && now() < RETRY_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64) -> bool {
    commit >= 512 * MIB
}

fn has_growth_reserve(commit: u64) -> bool {
    commit >= GIB
}

/// Whether the texture pass may go on (asked before it and per slot).
pub(crate) fn admit_texture_pass() -> bool {
    if backing_off() {
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if COMMIT_GATE.on() && !has_upload_reserve(sample_commit(now())) {
        COUNTS[2].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

/// Whether a new texture heap or staging block may be made (a refusal
/// makes the caller use what it would without them).
pub(crate) fn allow_growth() -> bool {
    let held = backing_off() || (COMMIT_GATE.on() && !has_growth_reserve(sample_commit(now())));
    if held {
        COUNTS[3].fetch_add(1, Ordering::Relaxed);
    }
    !held
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was { physical < 5 * GIB || commit < 6 * GIB } else { physical <= 3 * GIB || commit <= 4 * GIB }
}

/// At most once a second: whether the host is under memory pressure.
fn refresh() {
    let t = now();
    let next = NEXT_REFRESH.load(Ordering::Relaxed);
    if t < next || NEXT_REFRESH.compare_exchange(next, t + 1000, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    let was = PRESSURED.load(Ordering::Relaxed);
    let pressure = next_pressure(was, physical, commit);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        tracing::warn!(
            pressure,
            physical_free_mib = physical >> 20,
            available_commit_mib = commit >> 20,
            "host memory pressure (VICTORY_PREFETCH_CAP)"
        );
    }
}

fn critical_commit() -> bool {
    let t = now();
    t < RETRY_AT.load(Ordering::Relaxed) || sample_commit(t) < 1536 * MIB
}

fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    let bytes = if limited { bytes.min(128 * MIB) } else { bytes };
    if critical {
        bytes.min(16 * MIB)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 * MIB)
    } else {
        bytes
    }
}

/// The texture pass's bytes for this frame (`VICTORY_PREFETCH_CAP`);
/// `staging` is the bytes of staging buffers alive. A single texture can
/// cross it; slots not served stay due.
pub(crate) fn per_frame(bytes: u64, staging: u64) -> u64 {
    if !PREFETCH_CAP.on() {
        return bytes;
    }
    refresh();
    prefetch_limit(bytes, PRESSURED.load(Ordering::Relaxed), critical_commit(), limited_host_ram(), staging)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves() {
        assert!(!has_upload_reserve(256 * MIB));
        assert!(has_upload_reserve(512 * MIB));
        assert!(!has_growth_reserve(768 * MIB));
        assert!(has_growth_reserve(GIB));
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn prefetch_limits() {
        assert_eq!(prefetch_limit(512 * MIB, false, false, true, 0), 128 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, false, false, true, GIB), 64 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, false, false, false, 2 * GIB), 512 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, true, false, false, 0), 64 * MIB);
        assert_eq!(prefetch_limit(512 * MIB, true, true, false, 0), 16 * MIB);
        assert_eq!(prefetch_limit(8 * MIB, true, true, false, 0), 8 * MIB);
    }
}
