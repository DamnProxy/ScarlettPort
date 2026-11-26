//! `VICTORY_NATIVE_BUFFERS=1` (off by default): GPU-owned buffers, the
//! re-scoped milestone 1. A title buffer's host copy starts on today's
//! path. Once [`STREAK`] copy-backs of it in a row ran with no title CPU
//! write in it since the one before (write watch) and it is not a read-back
//! buffer, it is promoted: the copy is the truth from then on. Its ranges
//! shaders write are still declared (flights stay ordered) but get no
//! snapshot and no copy-back, and nothing is refreshed from title memory.
//! The first title CPU write seen in it demotes it. The copy's bytes go into
//! title memory for every page the title did not write, then it is on
//! today's path again with its streak at 0.
//!
//! Not exact:
//! - A title CPU *read* of a promoted buffer outside the read-back ring is
//!   not seen; it reads bytes from the promotion on.
//! - On demotion, a page the title wrote keeps the title's bytes whole
//!   (shader bytes elsewhere in that page are lost).
//! - Shader writes of a flight still in the air at demotion are not copied
//!   back.
//!
//! Only plain host copies (not in the card's memory, not ReBAR, not in
//! place): their bytes are in mapped memory the CPU reads at demotion.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

/// Copy-backs in a row with no title write before promotion
/// (`VICTORY_NATIVE_BUFFERS_STREAK`, default 120: a couple of seconds).
pub(crate) fn streak() -> u32 {
    static N: OnceLock<u32> = OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_NATIVE_BUFFERS_STREAK").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(120))
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_NATIVE_BUFFERS").is_some_and(|v| v == "1");
        if on {
            tracing::warn!(streak = streak(), "VICTORY_NATIVE_BUFFERS: GPU-owned buffers are promoted off the copy-back path");
        }
        on
    })
}

/// Promotions, demotions, copy-backs skipped, bytes they covered, bytes
/// put into title memory at demotion, promoted now, bytes promoted now.
pub(crate) static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
pub(crate) const PROMOTED: usize = 0;
pub(crate) const DEMOTED: usize = 1;
pub(crate) const SKIPPED: usize = 2;
pub(crate) const SKIPPED_BYTES: usize = 3;
pub(crate) const MATERIALIZED: usize = 4;
pub(crate) const LIVE: usize = 5;
pub(crate) const LIVE_BYTES: usize = 6;

/// Every 5 s at a submission: the counts and the copy-back avoided per
/// frame since the last line.
pub(crate) fn line() {
    if !on() {
        return;
    }
    static LAST: std::sync::Mutex<Option<(std::time::Instant, u64, u64)>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let frames = crate::native::probe::frames();
    let bytes = STATS[SKIPPED_BYTES].load(Relaxed);
    let Some((at, f0, b0)) = *last else {
        *last = Some((now, frames, bytes));
        return;
    };
    if now - at < std::time::Duration::from_secs(5) {
        return;
    }
    *last = Some((now, frames, bytes));
    let per_frame = (bytes - b0) as f64 / (frames.saturating_sub(f0)).max(1) as f64 / 1e6;
    tracing::warn!(
        "native buffers: promoted {} (now {}, {:.1} MB), demoted {}, copy-backs skipped {}, copy-back avoided {:.2} MB/frame, put back at demotion {:.1} MB",
        STATS[PROMOTED].load(Relaxed),
        STATS[LIVE].load(Relaxed),
        STATS[LIVE_BYTES].load(Relaxed) as f64 / 1e6,
        STATS[DEMOTED].load(Relaxed),
        STATS[SKIPPED].load(Relaxed),
        per_frame,
        STATS[MATERIALIZED].load(Relaxed) as f64 / 1e6,
    );
}
