//! Refresh traffic of host copies (branch fx-mem): what the per-frame
//! "copies N ms M MB" is made of, and one exact cut of it.
//!
//! - Counters, always on, logged with the frame-rate line ("refresh bytes
//!   per frame by kind"): bytes copied on pages the refresh holds whole,
//!   bytes copied on pages it holds only in part, bytes of memory that is
//!   not write-watched (copied whole every time), bytes of copies in the
//!   card's memory among them, and the bytes `VICTORY_MEM_SUBPAGE` left
//!   out.
//! - `VICTORY_MEM_SUBPAGE=1` (off by default): a 64 KiB write-watch page
//!   that a refresh holds only in part was never noted as taken
//!   (`Shadow::copy_stale`: `copied[page]` is set only for pages held
//!   whole), so every refresh of a small range (a constant buffer, a root
//!   view cut to its bound by `VICTORY_NARROW_ROOT`) copied its bytes
//!   again, written or not. With the switch each such page keeps the byte
//!   ranges taken and the asking each was taken at; a later refresh copies
//!   only what is not held by a range taken at or after the page's last
//!   write ([`gaps`]). The rule is the whole page's, applied to byte
//!   ranges: exact by the same write-watch evidence.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `VICTORY_MEM_SUBPAGE=1`.
pub(crate) fn subpage_on() -> bool {
    // Live (`victory_core::live`, `VICTORY_AB`): ranges kept while on are
    // dropped by the page's write asking as ever; while off none are kept.
    static ON: victory_core::live::Switch = victory_core::live::Switch::new("MEM_SUBPAGE", 0);
    ON.on()
}

/// Most ranges kept for one page; past it the oldest are let go (a range
/// let go is copied again once: never a byte missed).
const MOST: usize = 32;

/// One page's taken ranges: `(from, to, asking)`, byte offsets in the page.
pub(crate) type Parts = Vec<(u32, u32, u64)>;

/// The pieces of `[lo, hi)` (offsets in one page) to copy now: those not
/// held by a range of `parts` taken at an asking `>= write` (and not 0).
/// Ranges taken before `write` are dropped; `[lo, hi)` is then noted taken
/// at `now` (the caller copies the pieces returned, the rest is held
/// already). `now >= write` (the asking that reported `write`).
pub(crate) fn gaps(parts: &mut Parts, lo: u32, hi: u32, write: u64, now: u64) -> Vec<(u32, u32)> {
    parts.retain(|&(_, _, at)| at != 0 && at >= write);
    let mut held: Vec<(u32, u32)> = parts.iter().filter(|&&(a, e, _)| a < hi && e > lo).map(|&(a, e, _)| (a.max(lo), e.min(hi))).collect();
    held.sort_unstable();
    let mut out = Vec::new();
    let mut at = lo;
    for (a, e) in held {
        if a > at {
            out.push((at, a));
        }
        at = at.max(e);
    }
    if at < hi {
        out.push((at, hi));
    }
    // Noted at `now`: ranges it covers whole go (theirs is the older note).
    parts.retain(|&(a, e, _)| !(a >= lo && e <= hi));
    parts.push((lo, hi, now));
    if parts.len() > MOST {
        parts.sort_unstable_by_key(|&(_, _, at)| at);
        parts.drain(..parts.len() - MOST);
    }
    out
}

/// Counters since the last line, bytes.
pub(crate) enum K {
    /// Copied on pages the refresh held whole.
    Whole,
    /// Copied on pages held only in part.
    Part,
    /// Memory not write-watched, copied whole.
    Unwatched,
    /// Of all the above, into copies in the card's memory.
    Card,
    /// Left out by `VICTORY_MEM_SUBPAGE` (held already).
    SubpageSaved,
    /// Refresh calls ([`crate::shadow::Shadow::update`] with bytes to look at).
    Calls,
}

static COUNT: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub(crate) fn add(k: K, n: u64) {
    COUNT[k as usize].fetch_add(n, Relaxed);
}

/// The line, with the frame-rate line (`frames`: frames since the last).
pub(crate) fn line(frames: f64) {
    let c: Vec<f64> = COUNT.iter().map(|a| a.swap(0, Relaxed) as f64).collect();
    let mb = |x: f64| x / frames / 1e6;
    tracing::info!(
        "refresh bytes per frame by kind: {:.1} MB on pages held whole, {:.1} MB on pages held in part, {:.1} MB of memory not write-watched; {:.1} MB of these into copies on the card; {:.0} refreshes; {:.1} MB held already, left out (VICTORY_MEM_SUBPAGE={})",
        mb(c[0]),
        mb(c[1]),
        mb(c[2]),
        mb(c[3]),
        c[5] / frames,
        mb(c[4]),
        u8::from(subpage_on()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 65536;

    /// A model of one page: title bytes, the copy, the page's last write
    /// (as the write watch numbers askings) and the copy's whole-page note.
    /// Refreshes of random ranges with `gaps` must leave the copy equal to
    /// title memory over the range every time, as copying the range whole
    /// does, and copy no byte twice between two writes.
    #[test]
    fn gaps_refresh_is_byte_identical_to_copying_whole() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _case in 0..200 {
            let mut title = vec![0u8; PAGE];
            let mut copy = vec![0xAAu8; PAGE];
            let mut whole = vec![0xAAu8; PAGE];
            let mut parts = Parts::new();
            let (mut asking, mut write) = (1u64, 0u64);
            // Bytes a whole copy would take, less those `gaps` took.
            let mut saved = 0i64;
            for _step in 0..300 {
                if rnd(4) == 0 {
                    // A title write: some bytes change, the page is written
                    // after the current asking.
                    let at = rnd(PAGE as u64) as usize;
                    let n = (1 + rnd(512) as usize).min(PAGE - at);
                    for b in &mut title[at..at + n] {
                        *b = b.wrapping_add(1 + rnd(255) as u8);
                    }
                    write = asking + 1;
                }
                // A refresh: a new asking, then the gaps of its range.
                asking = asking.max(write) + 1;
                let lo = rnd(PAGE as u64) as u32;
                let hi = (lo + 1 + rnd(24576) as u32).min(PAGE as u32);
                for (a, e) in gaps(&mut parts, lo, hi, write, asking) {
                    saved -= (e - a) as i64;
                    copy[a as usize..e as usize].copy_from_slice(&title[a as usize..e as usize]);
                }
                saved += i64::from(hi - lo);
                whole[lo as usize..hi as usize].copy_from_slice(&title[lo as usize..hi as usize]);
                assert_eq!(copy[lo as usize..hi as usize], title[lo as usize..hi as usize]);
                assert_eq!(copy[lo as usize..hi as usize], whole[lo as usize..hi as usize]);
                assert!(parts.len() <= MOST);
            }
            assert!(saved > 0);
        }
    }

    #[test]
    fn a_write_after_the_note_takes_the_range_again() {
        let mut parts = Parts::new();
        assert_eq!(gaps(&mut parts, 100, 200, 0, 5), [(100, 200)]);
        assert!(gaps(&mut parts, 120, 180, 0, 6).is_empty());
        assert_eq!(gaps(&mut parts, 150, 250, 0, 7), [(200, 250)]);
        // Written after asking 7: all again.
        assert_eq!(gaps(&mut parts, 100, 250, 8, 9), [(100, 250)]);
        // Written at the asking of the note: still held (as the whole
        // page's rule, `write > copied` is stale).
        assert!(gaps(&mut parts, 100, 250, 9, 10).is_empty());
    }
}
