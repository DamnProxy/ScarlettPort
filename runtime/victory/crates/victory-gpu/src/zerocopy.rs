//! `VICTORY_ZERO_COPY` (off by default): title memory the card reads and
//! writes where it lies, with no host copy, no refresh, no copy-back and no
//! write watching (the console's one memory).
//!
//! The design: of the three ways D3D12
//! has to let the card see memory the CPU writes, only opening the title's
//! own pages as a heap (`OpenExistingHeapFromAddress1` /
//! `OpenExistingHeapFromFileMapping`, [`crate::inplace`]) is zero copy. A
//! GPU upload heap ([`crate::rebar`]) or a custom heap of ours cannot be put
//! at the title's addresses: the title would still write its own memory and
//! something would copy it over. So this switch widens the in-place route:
//!
//! - `1`: [`crate::inplace`] at its level 2 (windows outside buffers and
//!   title buffers in private memory committed whole), the big buffers
//!   `cards` keeps in the card's memory included
//!   (`VICTORY_ZERO_COPY_BIG=0` keeps them there: their shader reads then
//!   stay at the card's speed, at the cost of their sends and fetches).
//! - `2`: also view memory (the physical page pool): a range that is one
//!   run of one section is a buffer placed at its offset in a heap opened
//!   on the section. Sections are then made with `SEC_COMMIT`
//!   ([`sections`]; `victory-hle` xmem), the only kind the host opens.
//!
//! Anything else falls back to what was there before (a host copy, or
//! [`crate::rebar`]'s, or a card copy), per range, and is counted with its
//! reason ([`Reason`]); the line every five seconds gives the bytes a frame
//! no longer copied and the last ranges that fell back.
//!
//! Exactness: the card reads the bytes the CPU wrote, at the time it runs.
//! Ordering is the in-place route's: refused with late wait
//! (`VICTORY_LATE_WAIT`), where a submission may run after the title wrote
//! the memory again. Placed buffers are row-major buffers only: a texture
//! keeps its host copy (it needs tiling), as do ranges the host refuses.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `VICTORY_ZERO_COPY`: 0, 1 or 2 (read once).
pub(crate) fn level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        let level = level_of(std::env::var("VICTORY_ZERO_COPY").ok().as_deref());
        if level > 0 {
            tracing::warn!(
                "VICTORY_ZERO_COPY={level}: title memory opened in place for the card (no copy, refresh or copy-back); \
                 big buffers {}, view memory {}",
                if big_in_place() { "in place too" } else { "kept on the card" },
                if level >= 2 { "in place where one run of a section" } else { "copied" }
            );
        }
        level
    })
}

pub(crate) fn level_of(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

pub(crate) fn on() -> bool {
    level() > 0
}

/// Whether the big buffers `cards` puts in the card's memory are in place
/// too (`VICTORY_ZERO_COPY_BIG`, 1 by default with the switch).
pub(crate) fn big_in_place() -> bool {
    static BIG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *BIG.get_or_init(|| !std::env::var("VICTORY_ZERO_COPY_BIG").is_ok_and(|v| v.trim() == "0"))
}

/// Whether physical page sections are made with `SEC_COMMIT` so that they
/// can be opened as heaps (level 2).
pub fn sections() -> bool {
    level() >= 2
}

/// How title memory is backed, as far as opening it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backing {
    /// Private memory, committed and read-write over all of the range.
    Private,
    /// One run of one section: whether made with `SEC_COMMIT`.
    SectionRun { committed: bool },
    /// Views of more than one place, or with holes.
    ViewScattered,
    /// Neither private memory committed whole nor views known to us
    /// (placeholders, reserved, guarded or of another protection).
    Other,
}

/// What a range is asked with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ask {
    pub level: u8,
    pub late_wait: bool,
    /// A title buffer `cards` would put in the card's memory.
    pub big: bool,
    pub big_in_place: bool,
    pub backing: Backing,
    /// Bytes in place now, this range's, and the most.
    pub placed: u64,
    pub len: u64,
    pub most: u64,
}

/// Where a range goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// A part of a private allocation opened as a heap.
    Private,
    /// A heap opened on the section, the buffer at the run's offset.
    Section,
}

/// Why a range was not put in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    Off,
    LateWait,
    BigOnCard,
    ViewMemory,
    SectionReserved,
    Scattered,
    NotPrivate,
    OverBudget,
    /// The host refused to open the memory or to place the buffer.
    Refused,
}

pub(crate) const REASONS: usize = 9;

impl Reason {
    pub(crate) fn index(self) -> usize {
        self as usize
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Reason::Off => "off",
            Reason::LateWait => "late wait",
            Reason::BigOnCard => "big, kept on the card",
            Reason::ViewMemory => "view memory (level 1)",
            Reason::SectionReserved => "section not committed",
            Reason::Scattered => "views scattered or with holes",
            Reason::NotPrivate => "not private memory committed whole",
            Reason::OverBudget => "over VICTORY_INPLACE_MB",
            Reason::Refused => "refused by the host",
        }
    }
}

/// The rule (pure: tested below).
pub(crate) fn route(ask: &Ask) -> Result<Route, Reason> {
    if ask.level == 0 {
        return Err(Reason::Off);
    }
    if ask.late_wait {
        return Err(Reason::LateWait);
    }
    if ask.big && !ask.big_in_place {
        return Err(Reason::BigOnCard);
    }
    let route = match ask.backing {
        Backing::Private => Route::Private,
        Backing::SectionRun { .. } if ask.level < 2 => return Err(Reason::ViewMemory),
        Backing::SectionRun { committed: false } => return Err(Reason::SectionReserved),
        Backing::SectionRun { committed: true } => Route::Section,
        Backing::ViewScattered => return Err(if ask.level < 2 { Reason::ViewMemory } else { Reason::Scattered }),
        Backing::Other => return Err(Reason::NotPrivate),
    };
    if ask.placed.saturating_add(ask.len) > ask.most {
        return Err(Reason::OverBudget);
    }
    Ok(route)
}

/// Pages `(section, offset)` in address order, one per 64 KiB page from
/// the range's first: the section and the first page's offset if they are
/// one run of one section (pure: tested below).
pub(crate) fn one_run(pages: &[(usize, u64)], page: u64) -> Option<(usize, u64)> {
    let &(section, first) = pages.first()?;
    pages
        .iter()
        .enumerate()
        .all(|(i, &(s, o))| s == section && o == first + i as u64 * page)
        .then_some((section, first))
}

// What was not copied, and what fell back.

/// Since the start: 0 refresh bytes asked of ranges in place, 1 shader
/// writes declared on them (each once a copy-back and a snapshot before),
/// 2 ranges put in place, 3 their bytes, 4 of them on sections.
static SAVED: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static FELL: [AtomicU64; REASONS] = [const { AtomicU64::new(0) }; REASONS];
/// The last ranges that fell back: start, bytes, reason.
static LAST: Mutex<Vec<(u64, u64, Reason)>> = Mutex::new(Vec::new());
const KEEP: usize = 8;

/// A refresh of `len` bytes asked of a range in place: nothing copied.
#[inline]
pub(crate) fn refresh_saved(len: u64) {
    SAVED[0].fetch_add(len, Relaxed);
}

/// Shader writes of `len` bytes declared on a range in place: no snapshot,
/// no compare, no copy-back.
#[inline]
pub(crate) fn back_saved(len: u64) {
    SAVED[1].fetch_add(len, Relaxed);
}

pub(crate) fn placed(len: u64, route: Route) {
    SAVED[2].fetch_add(1, Relaxed);
    SAVED[3].fetch_add(len, Relaxed);
    if route == Route::Section {
        SAVED[4].fetch_add(1, Relaxed);
    }
}

pub(crate) fn fell_back(start: u64, len: u64, reason: Reason) {
    FELL[reason.index()].fetch_add(1, Relaxed);
    if reason == Reason::Off {
        return;
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.len() == KEEP {
        last.remove(0);
    }
    last.push((start, len, reason));
}

/// Totals between two lines (pure: tested below).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Window {
    pub refresh: u64,
    pub back: u64,
    pub frames: u64,
}

impl Window {
    /// MB a frame not refreshed and not copied back.
    pub(crate) fn per_frame(&self) -> (f64, f64) {
        let f = self.frames.max(1) as f64;
        (self.refresh as f64 / f / 1048576.0, self.back as f64 / f / 1048576.0)
    }
}

/// The line, every five seconds while the switch is on (`crate::inplace::tick`).
pub(crate) fn say() {
    static AT: AtomicU64 = AtomicU64::new(0);
    static BEFORE: Mutex<(u64, u64, u64)> = Mutex::new((0, 0, 0));
    if !on() {
        return;
    }
    let now = crate::host::now_ms();
    let at = AT.load(Relaxed);
    if now < at + 5_000 || AT.compare_exchange(at, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let (refresh, back, frames) = (SAVED[0].load(Relaxed), SAVED[1].load(Relaxed), crate::queue::frames_presented());
    let window = {
        let mut before = BEFORE.lock().unwrap_or_else(|e| e.into_inner());
        let w = Window { refresh: refresh - before.0, back: back - before.1, frames: frames.saturating_sub(before.2) };
        *before = (refresh, back, frames);
        w
    };
    let (mb_refresh, mb_back) = window.per_frame();
    let fell: Vec<String> = (0..REASONS)
        .filter_map(|i| {
            let n = FELL[i].load(Relaxed);
            let r = [
                Reason::Off,
                Reason::LateWait,
                Reason::BigOnCard,
                Reason::ViewMemory,
                Reason::SectionReserved,
                Reason::Scattered,
                Reason::NotPrivate,
                Reason::OverBudget,
                Reason::Refused,
            ][i];
            (n > 0 && r != Reason::Off).then(|| format!("{n} {}", r.label()))
        })
        .collect();
    let last: Vec<String> = LAST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(s, l, r)| format!("{s:#x}+{l:#x} ({})", r.label()))
        .collect();
    // Under the host's target: the A/B runs log only that one at info.
    tracing::info!(
        target: "victory_gpu::host",
        "zero copy per frame: {mb_refresh:.1} MB of refreshes not copied, {mb_back:.1} MB of shader writes not snapshotted or copied back \
         ({} frames); {} ranges in place ({} MB, {} on sections) since the start; fell back: {}; last: {}; ReBAR upload heaps {}",
        window.frames,
        SAVED[2].load(Relaxed),
        SAVED[3].load(Relaxed) >> 20,
        SAVED[4].load(Relaxed),
        if fell.is_empty() { "none".to_string() } else { fell.join(", ") },
        if last.is_empty() { "none".to_string() } else { last.join(", ") },
        if crate::rebar::supported() { "available" } else { "not available" },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(level: u8, backing: Backing) -> Ask {
        Ask { level, late_wait: false, big: false, big_in_place: true, backing, placed: 0, len: 1 << 16, most: 4 << 30 }
    }

    #[test]
    fn level_one_takes_private_memory_only() {
        assert_eq!(route(&ask(1, Backing::Private)), Ok(Route::Private));
        assert_eq!(route(&ask(1, Backing::SectionRun { committed: true })), Err(Reason::ViewMemory));
        assert_eq!(route(&ask(1, Backing::ViewScattered)), Err(Reason::ViewMemory));
        assert_eq!(route(&ask(1, Backing::Other)), Err(Reason::NotPrivate));
        assert_eq!(route(&ask(0, Backing::Private)), Err(Reason::Off));
    }

    #[test]
    fn level_two_takes_committed_section_runs() {
        assert_eq!(route(&ask(2, Backing::SectionRun { committed: true })), Ok(Route::Section));
        assert_eq!(route(&ask(2, Backing::SectionRun { committed: false })), Err(Reason::SectionReserved));
        assert_eq!(route(&ask(2, Backing::ViewScattered)), Err(Reason::Scattered));
        assert_eq!(route(&ask(2, Backing::Private)), Ok(Route::Private));
    }

    #[test]
    fn late_wait_big_and_budget_fall_back() {
        let mut a = ask(2, Backing::Private);
        a.late_wait = true;
        assert_eq!(route(&a), Err(Reason::LateWait));
        let mut a = ask(1, Backing::Private);
        a.big = true;
        assert_eq!(route(&a), Ok(Route::Private));
        a.big_in_place = false;
        assert_eq!(route(&a), Err(Reason::BigOnCard));
        let mut a = ask(1, Backing::Private);
        a.placed = a.most - a.len + 1;
        assert_eq!(route(&a), Err(Reason::OverBudget));
        a.placed = a.most - a.len;
        assert_eq!(route(&a), Ok(Route::Private));
    }

    #[test]
    fn a_run_is_one_section_with_offsets_in_order() {
        const P: u64 = 1 << 16;
        assert_eq!(one_run(&[(7, 3 * P), (7, 4 * P), (7, 5 * P)], P), Some((7, 3 * P)));
        assert_eq!(one_run(&[(7, 3 * P)], P), Some((7, 3 * P)));
        assert_eq!(one_run(&[(7, 3 * P), (7, 5 * P)], P), None);
        assert_eq!(one_run(&[(7, 3 * P), (8, 4 * P)], P), None);
        assert_eq!(one_run(&[(7, 4 * P), (7, 3 * P)], P), None);
        assert_eq!(one_run(&[], P), None);
    }

    #[test]
    fn per_frame_divides_by_frames() {
        let w = Window { refresh: 300 << 20, back: 30 << 20, frames: 3 };
        assert_eq!(w.per_frame(), (100.0, 10.0));
        let w = Window { refresh: 1 << 20, back: 0, frames: 0 };
        assert_eq!(w.per_frame(), (1.0, 0.0));
    }

    #[test]
    fn reasons_index_their_counters() {
        assert_eq!(Reason::Refused.index(), REASONS - 1);
        assert_eq!(Reason::Off.index(), 0);
    }
}
