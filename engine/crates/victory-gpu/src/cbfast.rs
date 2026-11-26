//! fx-cbfast: faster, exact variants of the copy-back merge's CPU work, each
//! behind its own switch (all off by default). A child of `shadow`, so the
//! tests compare every variant with the old code byte for byte.
//!
//! - `VICTORY_CB_RUNS=1`: the scans that cut changed bytes into runs
//!   ([`super::find_changed_runs`], used by `merge_under_writes`, and the
//!   mask walk at the end of [`super::back_piece`]) go eight bytes at a
//!   time through both equal and changed stretches (the old code went
//!   eight at a time only through equal stretches, and byte by byte
//!   through every changed one and through the whole mask).
//! - `VICTORY_CB_RELAY_SPAN=1`: `relay_merge` finds the first and last
//!   byte the card changed by comparing the copy in place, and copies only
//!   that span out of the mapping, not the whole piece.
//! - `VICTORY_CB_WIDE_MERGE=1`: `copy_back_with` reads title memory and
//!   merges its differing stretches on several threads; the writes to
//!   title memory (guards, `write_mapped`) stay on the submitting thread,
//!   in the same order.
//! - `VICTORY_CB_ALIASWRITE=1`: `copy_back_with` writes each changed run
//!   into title memory with a plain store whose fault is caught
//!   (`crate::host::write_run`), not one `NtWriteVirtualMemory` a run.

use victory_core::live::Switch;

// All four are live (`victory_core::live`): exact variants of the same
// work, so they can be flipped at a frame boundary (`VICTORY_AB`).
fn switch(cell: &'static Switch) -> bool {
    cell.on()
}

/// `VICTORY_CB_RUNS=1`.
pub(crate) fn runs_on() -> bool {
    static ON: Switch = Switch::new("CB_RUNS", 0);
    switch(&ON)
}

/// `VICTORY_CB_RELAY_SPAN=1`.
pub(crate) fn relay_span_on() -> bool {
    static ON: Switch = Switch::new("CB_RELAY_SPAN", 0);
    switch(&ON)
}

/// `VICTORY_CB_WIDE_MERGE=1`.
pub(crate) fn wide_merge_on() -> bool {
    static ON: Switch = Switch::new("CB_WIDE_MERGE", 0);
    switch(&ON)
}

/// `VICTORY_CB_ALIASWRITE=1`.
pub(crate) fn aliaswrite_on() -> bool {
    static ON: Switch = Switch::new("CB_ALIASWRITE", 0);
    switch(&ON)
}

/// Runs written in place, and runs that faulted and went the old way.
pub(crate) static ALIASWRITE: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

const LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;

#[inline(always)]
fn word(s: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(s[i..i + 8].try_into().expect("eight bytes"))
}

/// The high bit of every byte of `x` that is zero, and no other bit (exact,
/// no carries between bytes).
#[inline(always)]
fn zero_bytes(x: u64) -> u64 {
    !(((x & LOW7).wrapping_add(LOW7)) | x | LOW7)
}

/// [`super::find_changed_runs`], word at a time. Same result for every
/// input.
pub(crate) fn find_changed_runs_fast(now: &[u8], was: &[u8], gap: usize, runs: &mut Vec<(usize, usize)>) {
    runs.clear();
    let n = now.len().min(was.len());
    let (now, was) = (&now[..n], &was[..n]);
    let mut i = 0;
    while i < n {
        // Equal bytes: 32 at a time, then 8, then one.
        while i + 32 <= n && now[i..i + 32] == was[i..i + 32] {
            i += 32;
        }
        while i + 8 <= n {
            let x = word(now, i) ^ word(was, i);
            if x == 0 {
                i += 8;
            } else {
                i += x.trailing_zeros() as usize / 8;
                break;
            }
        }
        while i < n && now[i] == was[i] {
            i += 1;
        }
        if i >= n {
            break;
        }
        let first = i;
        // Changed bytes: 8 at a time while none of them is equal.
        while i + 8 <= n {
            let z = zero_bytes(word(now, i) ^ word(was, i));
            if z == 0 {
                i += 8;
            } else {
                i += z.trailing_zeros() as usize / 8;
                break;
            }
        }
        while i < n && now[i] != was[i] {
            i += 1;
        }
        if gap != 0
            && let Some(last) = runs.last_mut()
            && first - (last.0 + last.1) <= gap
        {
            last.1 = i - last.0;
        } else {
            runs.push((first, i - first));
        }
    }
}

/// The runs of nonzero bytes of `mask`, as (offset, bytes) in order, word
/// at a time (the walk at the end of [`super::back_piece`]).
pub(crate) fn nonzero_runs_fast(mask: &[u8], runs: &mut Vec<(usize, usize)>) {
    let n = mask.len();
    let mut i = 0;
    while i < n {
        while i + 32 <= n && mask[i..i + 32] == [0u8; 32] {
            i += 32;
        }
        while i + 8 <= n {
            let x = word(mask, i);
            if x == 0 {
                i += 8;
            } else {
                i += x.trailing_zeros() as usize / 8;
                break;
            }
        }
        while i < n && mask[i] == 0 {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        while i + 8 <= n {
            let z = zero_bytes(word(mask, i));
            if z == 0 {
                i += 8;
            } else {
                i += z.trailing_zeros() as usize / 8;
                break;
            }
        }
        while i < n && mask[i] != 0 {
            i += 1;
        }
        runs.push((start, i - start));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{back_piece, find_changed_runs, first_difference, last_difference, merge_piece};
    use super::*;

    /// A small xorshift, so the tests need no crate.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Pieces like the card's: mostly equal, words and records written,
    /// some dense stretches, single bytes, lengths not a multiple of 8.
    fn pair(rng: &mut Rng, len: usize, kind: usize) -> (Vec<u8>, Vec<u8>) {
        let was: Vec<u8> = (0..len).map(|_| rng.next() as u8 & if kind % 2 == 0 { 0 } else { 0xff }).collect();
        let mut now = was.clone();
        let edits = match kind % 4 {
            0 => 0,
            1 => 3,
            2 => len / 64,
            _ => len / 8,
        };
        for _ in 0..edits {
            if len == 0 {
                break;
            }
            let at = rng.below(len);
            let n = [1, 2, 4, 8, 16, 37, 300][rng.below(7)].min(len - at);
            for b in &mut now[at..at + n] {
                // Sometimes the same value: a store that changed nothing.
                *b = if rng.below(5) == 0 { *b } else { rng.next() as u8 };
            }
        }
        (now, was)
    }

    fn reference_mask_runs(mask: &[u8]) -> Vec<(usize, usize)> {
        let (mut out, n, mut i) = (Vec::new(), mask.len(), 0);
        while i < n {
            while i < n && mask[i] == 0 {
                i += 1;
            }
            let s = i;
            while i < n && mask[i] != 0 {
                i += 1;
            }
            if i > s {
                out.push((s, i - s));
            }
        }
        out
    }

    #[test]
    fn zero_bytes_is_exact() {
        let mut rng = Rng(7);
        for _ in 0..100_000 {
            let mut x = rng.next();
            for b in 0..8 {
                if rng.below(3) == 0 {
                    x &= !(0xff << (b * 8));
                }
                if rng.below(5) == 0 {
                    x |= 0x80 << (b * 8);
                    x &= !(0x7f << (b * 8));
                }
            }
            let want = (0..8).fold(0u64, |m, b| if (x >> (b * 8)) & 0xff == 0 { m | 0x80 << (b * 8) } else { m });
            assert_eq!(zero_bytes(x), want, "{x:#x}");
        }
    }

    #[test]
    fn fast_runs_match_the_old_scan() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for round in 0..4000 {
            let len = [0, 1, 7, 8, 9, 31, 32, 33, 63, 65, 1000, 4096, 65536][round % 13];
            let (now, was) = pair(&mut rng, len, round / 13);
            // Unequal lengths too: only the common part counts.
            let cut = if round % 7 == 0 && len > 0 { len - rng.below(len) } else { len };
            for gap in [0, 1, 3, 8, 64] {
                find_changed_runs(&now[..cut], &was, gap, &mut a);
                b.push((1, 1));
                find_changed_runs_fast(&now[..cut], &was, gap, &mut b);
                assert_eq!(a, b, "len {len} cut {cut} gap {gap}");
            }
            let mask: Vec<u8> = now.iter().zip(&was).map(|(x, y)| x ^ y).collect();
            b.clear();
            nonzero_runs_fast(&mask, &mut b);
            assert_eq!(reference_mask_runs(&mask), b, "mask len {len}");
        }
    }

    /// `back_piece` with and without the switch: same title, host, runs.
    #[test]
    fn back_piece_is_the_same_with_fast_runs() {
        let mut rng = Rng(42);
        for round in 0..1500 {
            let len = [1, 33, 4096, 65536][round % 4];
            let (host, old) = pair(&mut rng, len, round / 4 + 1);
            // Title memory: the old bytes, with some the title rewrote.
            let (mut tmem, _) = pair(&mut rng, 0, 0);
            tmem.extend_from_slice(&old);
            for _ in 0..rng.below(20) {
                let at = rng.below(len);
                tmem[at] = rng.next() as u8;
            }
            let run = |fast: bool| {
                let mut host = host.clone();
                let (mut t, mut m, mut r) = (Vec::new(), Vec::new(), Vec::new());
                let got = super::super::back_piece_with(&mut host, &old, &mut t, &mut m, &mut r, fast, |f, d: &mut [u8]| {
                    d.copy_from_slice(&tmem[f..f + d.len()]);
                    true
                });
                (got, host, t, r)
            };
            let slow = run(false);
            assert_eq!(slow, run(true), "round {round}");
            // And the switch-off path is the old function.
            let mut host2 = host.clone();
            let (mut t, mut m, mut r) = (Vec::new(), Vec::new(), Vec::new());
            let old_got = back_piece(&mut host2, &old, &mut t, &mut m, &mut r, |f, d: &mut [u8]| {
                d.copy_from_slice(&tmem[f..f + d.len()]);
                true
            });
            assert_eq!((old_got, host2, t, r), slow);
        }
    }

    /// The relay's span: copying only `[first, last]` of the piece and
    /// merging that gives the same title bytes and snapshot as copying the
    /// whole piece.
    #[test]
    fn relay_span_is_the_same() {
        let mut rng = Rng(99);
        for round in 0..1500 {
            let len = [1, 40, 4096, 65536][round % 4];
            let (copy, held0) = pair(&mut rng, len, round / 4);
            let tmem: Vec<u8> = held0.iter().map(|b| if rng.below(50) == 0 { rng.next() as u8 } else { *b }).collect();
            // Old: copy all, find the span on the copy.
            let mut held_a = held0.clone();
            let now = copy.clone();
            let old = first_difference(&now, &held_a).map(|first| {
                let last = last_difference(&now, &held_a).unwrap_or(first);
                let mut title = tmem[first..=last].to_vec();
                let changed = merge_piece(&now[first..=last], &mut held_a[first..=last], &mut title);
                (first, title, changed)
            });
            // New: the span found in place, only it copied.
            let mut held_b = held0.clone();
            let new = super::super::relay_span(&copy, &held_b).map(|(first, last)| {
                let now = copy[first..=last].to_vec();
                let mut title = tmem[first..=last].to_vec();
                let changed = merge_piece(&now, &mut held_b[first..=last], &mut title);
                (first, title, changed)
            });
            assert_eq!(old, new, "round {round}");
            assert_eq!(held_a, held_b);
        }
    }

    /// The wide merge's per-stretch work, run on threads and put in order,
    /// gives what the stretches give one after another.
    #[test]
    fn wide_merge_matches_one_by_one() {
        let mut rng = Rng(5);
        for round in 0..40 {
            let len = 1 << 20;
            let (host0, old) = pair(&mut rng, len, round + 1);
            let tmem: Vec<u8> = old.iter().map(|b| if rng.below(100) == 0 { rng.next() as u8 } else { *b }).collect();
            let mut stretches = Vec::new();
            let mut at = 0;
            while at < len {
                let n = (rng.below(5) + 1) * 4096;
                let n = n.min(len - at);
                stretches.push((at, n));
                at += n + rng.below(3) * 4096;
            }
            let read = |f: usize, d: &mut [u8]| {
                d.copy_from_slice(&tmem[f..f + d.len()]);
                true
            };
            let mut host_a = host0.clone();
            let mut serial = Vec::new();
            for &(a, n) in &stretches {
                if a + n > len {
                    continue;
                }
                let (mut t, mut m, mut r) = (Vec::new(), Vec::new(), Vec::new());
                let got = back_piece(&mut host_a[a..a + n], &old[a..a + n], &mut t, &mut m, &mut r, |f, d: &mut [u8]| read(a + f, d));
                if let Some((first, n)) = got {
                    serial.push((a + first, t[..n].to_vec(), r.clone()));
                }
            }
            let mut host_b = host0.clone();
            let stretches: Vec<_> = stretches.into_iter().filter(|&(a, n)| a + n <= len).collect();
            let wide = super::super::merge_stretches_wide(host_b.as_mut_ptr() as usize, &old, 0, &stretches, false, &|f, d| read(f, d));
            let wide: Vec<_> = wide.into_iter().map(|w| (w.at, w.title, w.runs)).collect();
            assert_eq!(serial, wide, "round {round}");
            assert_eq!(host_a, host_b);
        }
    }

    // ---- benches: `cargo test --release -p victory-gpu cbfast::tests::bench -- --ignored --nocapture`

    fn time<R>(label: &str, bytes: usize, reps: usize, mut f: impl FnMut() -> R) -> f64 {
        let _ = std::hint::black_box(f());
        let t = std::time::Instant::now();
        for _ in 0..reps {
            std::hint::black_box(f());
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
        println!("{label:<44} {us:>10.3} us  {:>7.2} GB/s", bytes as f64 / us / 1e3);
        us
    }

    /// 57 MB of 64 KiB pieces (Belobug #49's copy-back), at three densities.
    #[test]
    #[ignore]
    fn bench_copy_back_parts() {
        let total = 57 << 20;
        let piece = 64 << 10;
        for (name, kind) in [("sparse (3 edits/piece)", 1), ("medium (1/64 B)", 2), ("dense (1/8 B)", 3)] {
            let mut rng = Rng(1234);
            let pieces: Vec<_> = (0..total / piece).map(|_| pair(&mut rng, piece, kind)).collect();
            let tmem: Vec<Vec<u8>> = pieces.iter().map(|(_, was)| was.clone()).collect();
            println!("--- {name}, {} pieces", pieces.len());
            time("compare pieces (memcmp, differing)", 2 * total, 5, || pieces.iter().filter(|(a, b)| a != b).count());
            time("first/last_difference", 2 * total, 5, || {
                pieces.iter().map(|(a, b)| first_difference(a, b).unwrap_or(0) + last_difference(a, b).unwrap_or(0)).sum::<usize>()
            });
            let mut runs = Vec::new();
            time("find_changed_runs (old)", 2 * total, 5, || {
                pieces.iter().map(|(a, b)| {
                    find_changed_runs(a, b, 0, &mut runs);
                    runs.len()
                }).sum::<usize>()
            });
            time("find_changed_runs_fast", 2 * total, 5, || {
                pieces.iter().map(|(a, b)| {
                    find_changed_runs_fast(a, b, 0, &mut runs);
                    runs.len()
                }).sum::<usize>()
            });
            for fast in [false, true] {
                let mut hosts: Vec<Vec<u8>> = pieces.iter().map(|(a, _)| a.clone()).collect();
                let (mut t, mut m, mut r) = (Vec::new(), Vec::new(), Vec::new());
                time(if fast { "back_piece (fast runs)" } else { "back_piece (old)" }, 3 * total, 5, || {
                    let mut k = 0;
                    for (i, (_, was)) in pieces.iter().enumerate() {
                        let h = &mut hosts[i];
                        h.copy_from_slice(&pieces[i].0);
                        let tm = &tmem[i];
                        if super::super::back_piece_with(h, was, &mut t, &mut m, &mut r, fast, |f, d: &mut [u8]| {
                            d.copy_from_slice(&tm[f..f + d.len()]);
                            true
                        })
                        .is_some()
                        {
                            k += r.len();
                        }
                    }
                    k
                });
            }
            let mut now = Vec::with_capacity(piece);
            time("relay: copy whole piece + span (old)", 2 * total, 5, || {
                pieces.iter().map(|(a, b)| {
                    now.clear();
                    now.extend_from_slice(a);
                    first_difference(&now, b).map_or(0, |f| last_difference(&now, b).unwrap_or(f) - f)
                }).sum::<usize>()
            });
            time("relay: span in place, copy span (new)", 2 * total, 5, || {
                pieces.iter().map(|(a, b)| {
                    super::super::relay_span(a, b).map_or(0, |(f, l)| {
                        now.clear();
                        now.extend_from_slice(&a[f..=l]);
                        l - f
                    })
                }).sum::<usize>()
            });
            let mut big_host: Vec<u8> = pieces.iter().flat_map(|(a, _)| a.iter().copied()).collect();
            let big_old: Vec<u8> = pieces.iter().flat_map(|(_, b)| b.iter().copied()).collect();
            let big_t: Vec<u8> = tmem.iter().flatten().copied().collect();
            let stretches: Vec<_> = (0..pieces.len()).map(|i| (i * piece, piece)).collect();
            let base = big_host.clone();
            for wide in [false, true] {
                time(if wide { "merge all stretches (wide)" } else { "merge all stretches (one thread)" }, 3 * total, 3, || {
                    big_host.copy_from_slice(&base);
                    super::super::merge_stretches_wide(big_host.as_mut_ptr() as usize, &big_old, 0, &stretches, !wide, &|f, d| {
                        d.copy_from_slice(&big_t[f..f + d.len()]);
                        true
                    })
                    .len()
                });
            }
        }
    }

    /// The kernel write per run that the merge pays (`write_mapped`): one
    /// `WriteProcessMemory` a run.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn bench_write_process_memory() {
        use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut dst = vec![0u8; 1 << 20];
        let src = vec![1u8; 1 << 20];
        for len in [4usize, 64, 4096, 65536] {
            let reps = 20_000;
            time(&format!("WriteProcessMemory {len} B"), len, reps, || {
                let mut done = 0;
                // SAFETY: both buffers are ours and at least `len` long.
                unsafe { WriteProcessMemory(GetCurrentProcess(), dst.as_mut_ptr().cast(), src.as_ptr().cast(), len, &mut done) }
            });
        }
    }

    /// `VICTORY_CB_ALIASWRITE`: a run through `write_run`, old (the kernel)
    /// and in place, into write-watched memory as title memory is.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn bench_write_run() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let size = 1 << 20;
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let dst = unsafe { VirtualAlloc(std::ptr::null(), size, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) } as *mut u8;
        assert!(!dst.is_null());
        let src = vec![1u8; size];
        for len in [4usize, 64, 4096, 65536] {
            for inplace in [false, true] {
                let reps = 20_000;
                let name = format!("write_run {len} B ({})", if inplace { "in place" } else { "kernel" });
                let mut k = 0usize;
                time(&name, len, reps, || {
                    // Spread over the allocation, as runs are.
                    let at = (k * 4096 * 7) % (size - len);
                    k += 1;
                    crate::host::write_run_with(inplace, dst.wrapping_add(at), src.as_ptr(), len);
                });
            }
        }
        // SAFETY: our allocation.
        unsafe { VirtualFree(dst.cast(), 0, MEM_RELEASE) };
    }
}
