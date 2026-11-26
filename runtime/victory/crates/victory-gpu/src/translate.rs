//! `VICTORY_MEM2_GPU_ONLY_NOVIEW=1` (off by default): page mappings into the
//! title's GPU-only reservations are not host views (MEMORY.md §3.3 4).
//!
//! The title's texture streamer maps each streamed page twice: once into
//! its CPU pool view (`XMemMapPhysicalPagesScatter`), once at the tiled
//! resource's address (`CopyPageMappingsBatchX`), which lies in a
//! reservation the console's CPU cannot touch (protection 0x801). The
//! runtime made a host view for both, 3-4 system calls and a TLB shootdown
//! a page. With the switch, `victory-hle` xmem records the second as a
//! translation instead: destination page → the page's bytes in a view of
//! the whole section that never moves ([`title_memory_translated`]).
//!
//! Every read of title memory the runtime makes through the kernel or the
//! mapping record translates (`crate::host::read`, `crate::regions::copy`,
//! the texture refresh); a write of the runtime's own goes through the
//! table too (`crate::host::write_mapped`). Anything else that touches such
//! a page, the title's code included (which the console would not allow),
//! faults: the handler here then maps the page as a view after all, as
//! without the switch ([`materialize`]), tells the mapping record, and lets
//! the access run again. A kernel call about to write title memory
//! (`crate::watch::writing`) has its pages made views first the same way.
//!
//! `VICTORY_REMAP_LAZY=1` (off by default, [`lazy`]) uses the same table for
//! every page the title's streamer maps (`XMemMapPhysicalPagesScatter`,
//! `CopyPageMappingsBatchX`, single-page maps), CPU-reachable or not: a
//! remap is a table update, and a page gets a host view only when the CPU
//! touches it (the fault above). With it:
//! - views taken away (a page made a view, then remapped or unmapped) are
//!   unmapped at the end of the title's call ([`flush_unmaps`]), each
//!   address once, not in the middle of it; a page about to be made a view
//!   again first loses its old one;
//! - an address unmapped this way stays a lone 64 KiB placeholder and is
//!   remembered ([`LONE`]), so making it a view again needs no split: each
//!   address is split out of the title's reservation once per session;
//! - the sections are committed whole (`SEC_COMMIT`), so a view is no
//!   commit either.

use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};

use victory_core::mem2::{PAGE, PageTable};

/// A destination page shown through the table.
struct Entry {
    /// The page's first byte in the section's own view (unmoving while the
    /// entry lives: the runtime keeps the section until it removes it).
    src: usize,
    /// The section and the page's offset in it, to map it as a view.
    section: usize,
    offset: u64,
    /// The section is committed whole: a view of it needs no commit.
    committed: bool,
    /// 0: no view; 1: being mapped; 2: mapped as a view ([`materialize`]).
    state: AtomicU8,
}

static TABLE: RwLock<PageTable<Entry>> = RwLock::new(PageTable::new());
/// Entries in [`TABLE`], read without its lock to pass by at once.
static PAGES: AtomicUsize = AtomicUsize::new(0);

/// Since the start: destination pages recorded instead of mapped, of those
/// made views later after all, and bytes read and written through the table.
pub(crate) static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

thread_local! {
    /// This thread holds [`TABLE`]'s lock (its fault handler must not take
    /// it again).
    static HOLDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `VICTORY_REMAP_LAZY=1` (off by default, read once): every page mapping of
/// the title's streamer goes through the table (module docs).
pub fn lazy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = victory_core::mem2::switch("VICTORY_REMAP_LAZY");
        if on {
            tracing::warn!("VICTORY_REMAP_LAZY: page mappings are table updates, views made when the CPU touches a page");
        }
        on
    })
}

/// Since the last frame line ([`lazy`]): 0 pages put in the table, 1 taken
/// out, 2 views made lazily (any cause), 3 of them made by a fault of the
/// CPU, 4 views unmapped at the end of a call, 5 placeholder splits left
/// out, 6 faults passed on (a page a view already: a guard's).
pub(crate) static LAZY: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Views taken out of the table and not unmapped yet ([`lazy`]): unmapped at
/// the end of the title's call ([`flush_unmaps`]), or just before the page
/// is made a view again.
static PENDING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
static PENDING_N: AtomicUsize = AtomicUsize::new(0);

/// Addresses known to be lone 64 KiB placeholders (a view of one page was
/// unmapped there): making a view there needs no split ([`lazy`]).
static LONE: std::sync::Mutex<std::collections::BTreeSet<usize>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

#[cfg(test)]
pub(crate) static LAZY_TEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn deferring() -> bool {
    #[cfg(test)]
    if LAZY_TEST.load(Relaxed) {
        return true;
    }
    lazy()
}

/// Unmaps the view at `page` (made by [`materialize_one`]), back to a
/// placeholder of its own.
fn unmap_view(page: usize) {
    use windows_sys::Win32::System::Memory::{MEM_PRESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile2};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a view [`materialize_one`] mapped there and no entry stands
    // for any more.
    let ok = unsafe {
        UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: page as *mut _ }, MEM_PRESERVE_PLACEHOLDER)
    } != 0;
    if ok {
        LONE.lock().unwrap_or_else(|e| e.into_inner()).insert(page);
    }
}

/// A view no entry stands for any more: unmapped now, or at the end of the
/// call ([`lazy`]). `section` and `offset` are what it shows (kept for
/// [`park`]).
fn retire_view(page: usize, section: usize, offset: u64) {
    if deferring() {
        let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        p.push(page);
        PENDING_N.store(p.len(), Relaxed);
        if parking() {
            SHOWS.lock().unwrap_or_else(|e| e.into_inner()).insert(page, (section, offset));
        }
    } else {
        unmap_view(page);
    }
}

/// `VICTORY_REMAP_PARK=1` (off by default; with `VICTORY_REMAP_LAZY`): a
/// view taken away is not unmapped at the end of the call but parked, out
/// of the table and the mapping record, still mapped; when the same
/// physical page (section and offset) is made a view at the same address
/// again it is that view, with no system call. The title's render-target
/// pool is trimmed every frame and mostly given its pages back at the same
/// addresses. A parked view of another page there is unmapped first.
/// Counted on the frame line ("parked views reused / unmapped").
pub fn parking() -> bool {
    #[cfg(test)]
    if PARK_TEST.load(Relaxed) {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = victory_core::mem2::switch("VICTORY_REMAP_PARK");
        if on {
            tracing::warn!("VICTORY_REMAP_PARK: views taken away stay mapped, reused for the same page at the same address");
        }
        on
    })
}

#[cfg(test)]
pub(crate) static PARK_TEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Since the last frame line: 0 parked views reused, 1 parked views
/// unmapped (another page wanted there), 2 views parked.
pub(crate) static PARK: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// What each retired view shows, by address ([`parking`]).
static SHOWS: std::sync::Mutex<std::collections::BTreeMap<usize, (usize, u64)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
/// Views parked: address to what they show ([`parking`]).
static PARKED: std::sync::Mutex<std::collections::BTreeMap<usize, (usize, u64)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Parks the retired view at `page` instead of unmapping it; false if
/// parking is off. Under [`PENDING`]'s lock.
fn park(page: usize) -> bool {
    if !parking() {
        return false;
    }
    let Some(shows) = SHOWS.lock().unwrap_or_else(|e| e.into_inner()).remove(&page) else {
        return false;
    };
    // No access while parked: the title's CPU touching the address meets
    // the fault (and the table's entry, if any) instead of the old page.
    if !protect(page, windows_sys::Win32::System::Memory::PAGE_NOACCESS) {
        return false;
    }
    PARKED.lock().unwrap_or_else(|e| e.into_inner()).insert(page, shows);
    PARK[2].fetch_add(1, Relaxed);
    true
}

/// Sets the protection of the 64 KiB view at `page`.
fn protect(page: usize, to: u32) -> bool {
    let mut old = 0;
    // SAFETY: a whole view of ours.
    unsafe { windows_sys::Win32::System::Memory::VirtualProtect(page as *const _, PAGE, to, &mut old) != 0 }
}

/// Takes the parked view at `page`, if any: what it shows.
fn unpark(page: usize) -> Option<(usize, u64)> {
    if !parking() {
        return None;
    }
    PARKED.lock().unwrap_or_else(|e| e.into_inner()).remove(&page)
}

/// A retired view at `page` still on the list that shows `e`'s page: kept
/// as `e`'s view (true). Under [`PENDING`]'s lock, the page taken off it.
fn reuse(page: usize, e: &Entry) -> bool {
    if !parking() {
        return false;
    }
    let shows = SHOWS.lock().unwrap_or_else(|e| e.into_inner()).remove(&page);
    if shows == Some((e.section, e.offset)) {
        PARK[0].fetch_add(1, Relaxed);
        return true;
    }
    false
}

/// Parked views (for the frame line).
pub(crate) fn parked() -> usize {
    PARKED.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Unmaps the views retired during the title's call, each address once.
/// The caller holds the bytes against reads in place (its remapping covers
/// every retired page: they were in the ranges it changed). Returns the
/// views unmapped.
///
/// A page leaves the list only once its view is unmapped, under the list's
/// lock, a few at a time: a thread about to map a view at a page of the
/// list ([`materialize_one`]) finds it there, or finds it unmapped. (The
/// list used to be emptied before the unmaps, and a view mapped meanwhile
/// at a page still showing its old one failed with error 487.) Callers
/// need not hold the bytes: a retired page is out of the table and of the
/// mapping record already, so nothing reads or writes it in place.
pub fn flush_unmaps() -> usize {
    const CHUNK: usize = 64;
    if PENDING_N.load(Relaxed) == 0 {
        return 0;
    }
    {
        let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        // Highest first, so the chunks below come off the end in order.
        p.sort_unstable_by(|a, b| b.cmp(a));
        p.dedup();
        PENDING_N.store(p.len(), Relaxed);
    }
    race_pause(usize::MAX);
    let mut n = 0;
    loop {
        let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        if p.is_empty() {
            break;
        }
        let keep = p.len().saturating_sub(CHUNK);
        for page in p.split_off(keep) {
            if !park(page) {
                unmap_view(page);
            }
            n += 1;
        }
        PENDING_N.store(p.len(), Relaxed);
    }
    LAZY[4].fetch_add(n as u64, Relaxed);
    n
}

/// Calls of page mappings under way ([`lazy_call`]).
static CALLS: AtomicUsize = AtomicUsize::new(0);
/// Bounds of every address ever put in the table: only a fault there waits
/// for a call (any other is no business of ours).
static LO: AtomicUsize = AtomicUsize::new(usize::MAX);
static HI: AtomicUsize = AtomicUsize::new(0);

/// Held by `victory-hle` xmem over a call of page mappings
/// (`VICTORY_REMAP_LAZY`): a CPU fault on a page with no entry meanwhile
/// waits for the call's end instead of being taken for a bad access.
pub struct LazyCall(());

impl Drop for LazyCall {
    fn drop(&mut self) {
        CALLS.fetch_sub(1, Relaxed);
    }
}

pub fn lazy_call() -> LazyCall {
    CALLS.fetch_add(1, Relaxed);
    LazyCall(())
}

/// Pages retired and not unmapped yet (for tests and the frame line).
pub(crate) fn pending() -> usize {
    PENDING_N.load(Relaxed)
}

/// Whether the switch is on (read once).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = victory_core::mem2::switch("VICTORY_MEM2_GPU_ONLY_NOVIEW");
        if on {
            tracing::warn!("VICTORY_MEM2_GPU_ONLY_NOVIEW: page mappings into GPU-only reservations are translated, not mapped");
        }
        on
    })
}

/// Whether any page of `[at, at + len)` is shown through the table.
#[inline]
pub(crate) fn any(at: usize, len: usize) -> bool {
    PAGES.load(Relaxed) != 0 && len != 0 && table().any_in(at, len)
}

/// Bytes of `[at, at + len)` shown through the table.
pub(crate) fn bytes_in(at: usize, len: usize) -> usize {
    if PAGES.load(Relaxed) == 0 || len == 0 {
        return 0;
    }
    table().pieces(at, len).iter().filter(|p| p.page.is_some()).map(|p| p.len).sum()
}

/// `[at, at + len)` (64 KiB pages) as one run of one section through the
/// table, every page in it: the section, the offset of `at` in it, and
/// whether the section was made committed (`crate::zerocopy`).
pub(crate) fn run_of(at: usize, len: usize) -> Option<(usize, u64, bool)> {
    if PAGES.load(Relaxed) == 0 || len == 0 {
        return None;
    }
    let t = table();
    let first = at / PAGE * PAGE;
    let mut pages = Vec::new();
    let mut committed = true;
    let mut page = first;
    while page < at + len {
        let (_, e) = t.get(page)?;
        pages.push((e.section, e.offset / PAGE as u64 * PAGE as u64));
        committed &= e.committed;
        page += PAGE;
    }
    drop(t);
    let (section, offset) = crate::zerocopy::one_run(&pages, PAGE as u64)?;
    Some((section, offset + (at - first) as u64, committed))
}

/// Pages shown through the table now.
pub(crate) fn live() -> usize {
    PAGES.load(Relaxed)
}

struct Held(std::sync::RwLockReadGuard<'static, PageTable<Entry>>);

impl std::ops::Deref for Held {
    type Target = PageTable<Entry>;
    fn deref(&self) -> &PageTable<Entry> {
        &self.0
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HOLDING.with(|h| h.set(false));
    }
}

fn table() -> Held {
    let g = TABLE.read().unwrap_or_else(|e| e.into_inner());
    HOLDING.with(|h| h.set(true));
    Held(g)
}

/// The runtime shows the page at `src` (in a view of section `section`, at
/// `offset` in it) at destination page `at`, without a view there. Called
/// under the runtime's page lock, after what `at` showed was taken away.
pub fn title_memory_translated(at: usize, src: usize, section: usize, offset: u64, committed: bool) {
    install();
    LO.fetch_min(at / PAGE * PAGE, Relaxed);
    HI.fetch_max(at / PAGE * PAGE + PAGE, Relaxed);
    let entry = Entry { src, section, offset, committed, state: AtomicU8::new(0) };
    let mut t = TABLE.write().unwrap_or_else(|e| e.into_inner());
    let old = t.insert(at, entry);
    race_pause(at);
    match old {
        None => {
            PAGES.fetch_add(1, Relaxed);
        }
        // The page was a view of what the entry showed before: it must not
        // go on showing it. Retired before the lock goes: a thread that
        // makes the new entry a view (under the read lock) then finds the
        // old view on the list and unmaps it first, instead of mapping over
        // it (error 487) and leaving it mapped, untracked.
        Some(e) if settled(&e) == 2 => retire_view(at / PAGE * PAGE, e.section, e.offset),
        Some(_) => {}
    }
    drop(t);
    COUNTS[0].fetch_add(1, Relaxed);
    LAZY[0].fetch_add(1, Relaxed);
}

/// Tests widen the race windows: a pause at `at` (an address given to
/// [`title_memory_translated`], or `usize::MAX` in [`flush_unmaps`]).
#[cfg(test)]
pub(crate) static PAUSE_AT: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn race_pause(_at: usize) {
    #[cfg(test)]
    if PAUSE_AT.load(Relaxed) == _at {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// An entry's state once no thread is mapping it: 0 or 2. Under the
/// table's write lock no thread can be (a view is mapped under the read
/// lock), so this does not wait in practice; it guards against an entry
/// replaced or removed while being mapped, whose view would stay mapped
/// and untracked.
fn settled(e: &Entry) -> u8 {
    loop {
        match e.state.load(Relaxed) {
            1 => std::hint::spin_loop(),
            s => return s,
        }
    }
}

/// The runtime takes away what the table shows in `[at, at + len)`: the
/// entries go, and a page made a view meanwhile is a placeholder again.
/// Returns the pages removed. Called under the runtime's page lock and its
/// hold on the bytes, before it lets go of the sections.
pub fn title_memory_untranslated(at: usize, len: usize) -> usize {
    if PAGES.load(Relaxed) == 0 {
        return 0;
    }
    let mut t = TABLE.write().unwrap_or_else(|e| e.into_inner());
    let gone = t.remove_range(at, len);
    for (page, e) in &gone {
        if settled(e) == 2 {
            // The view [`materialize`] mapped there (retired under the
            // lock, as in [`title_memory_translated`]).
            retire_view(*page, e.section, e.offset);
        }
    }
    drop(t);
    PAGES.fetch_sub(gone.len(), Relaxed);
    LAZY[1].fetch_add(gone.len() as u64, Relaxed);
    gone.len()
}

/// Copies `[at, at + len)` of title memory to `dst`: the pages the table
/// shows from their sections, the rest with `plain(at, dst, n)`. True if
/// every piece was read (`plain`'s answers).
pub(crate) fn read(at: usize, dst: *mut u8, len: usize, mut plain: impl FnMut(usize, *mut u8, usize) -> bool) -> bool {
    let pieces = table().pieces(at, len);
    let mut ok = true;
    for p in pieces {
        let to = dst.wrapping_add(p.at - at);
        let done = p.page.is_some_and(|page| {
            let t = table();
            let Some((_, e)) = t.get(page) else {
                return false;
            };
            // SAFETY: the entry's page lies in a committed view of its
            // section that stays while the table holds the entry (held);
            // the destination is the caller's `len` bytes.
            unsafe { std::ptr::copy_nonoverlapping((e.src + (p.at - page)) as *const u8, to, p.len) };
            true
        });
        if done {
            COUNTS[2].fetch_add(p.len as u64, Relaxed);
        } else {
            ok &= plain(p.at, to, p.len);
        }
    }
    ok
}

/// Copies `len` bytes from `src` into title memory `[at, at + len)`: the
/// pages the table shows into their sections (the same physical pages, as
/// a write through a view would), the rest with `plain(at, src, n)`.
pub(crate) fn write(at: usize, src: *const u8, len: usize, mut plain: impl FnMut(usize, *const u8, usize)) {
    let pieces = table().pieces(at, len);
    for p in pieces {
        let from = src.wrapping_add(p.at - at);
        let done = p.page.is_some_and(|page| {
            let t = table();
            let Some((_, e)) = t.get(page) else {
                return false;
            };
            // SAFETY: as in [`read`]; the source is the caller's.
            unsafe { std::ptr::copy_nonoverlapping(from, (e.src + (p.at - page)) as *mut u8, p.len) };
            true
        });
        if done {
            COUNTS[3].fetch_add(p.len as u64, Relaxed);
        } else {
            plain(p.at, from, p.len);
        }
    }
}

/// Whether a write of the runtime's own into title memory `[at, at + len)`
/// (a DirectStorage destination, a file read) goes through the table
/// ([`write_through`]) instead of in place: some page of it is shown
/// through the table. No page is made a view for such a write, so a lazily
/// mapped page stays a table entry and nothing is left for a later remap to
/// retire and unmap.
pub fn shown(at: usize, len: usize) -> bool {
    any(at, len)
}

/// Writes `data` at title memory `at`: the pages the table shows into their
/// sections, the rest in place from user mode (a guarded block faults into
/// the guard like the title's own writes). The guards are opened and the
/// GPU side told as for a kernel write.
pub fn write_through(at: usize, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    crate::watch::writing_soft(at, data.len());
    write(at, data.as_ptr(), data.len(), |to, from, n| {
        // SAFETY: title memory the caller writes, `n` bytes of `data`.
        unsafe { std::ptr::copy_nonoverlapping(from, to as *mut u8, n) }
    });
}

/// After the mapping record forgot the views of `[at, at + len)`
/// (`crate::title_memory_remapped`): the pages of it the table shows that
/// are views are views still.
pub(crate) fn remapped(at: usize, len: usize) {
    if PAGES.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let views: Vec<usize> = table().iter_in(at, len).filter(|(_, e)| e.state.load(Relaxed) == 2).map(|(p, _)| p).collect();
    for page in views {
        crate::regions::mapped(page, PAGE);
    }
}

/// Makes the pages of `[at, at + len)` the table shows host views, as the
/// runtime maps them without the switch: before something reads or writes
/// them in place or through the kernel.
pub(crate) fn materialize(at: usize, len: usize) {
    if PAGES.load(Relaxed) == 0 || len == 0 || HOLDING.with(|h| h.get()) {
        return;
    }
    let pages: Vec<usize> = table().iter_in(at, len).map(|(p, _)| p).collect();
    for page in pages {
        let made = {
            let t = table();
            t.get(page).is_some_and(|(page, e)| materialize_one(page, e) == Made::Now)
        };
        if made {
            // A view now, for the mapping record (its lock is never taken
            // with the table's held).
            crate::regions::mapped(page, PAGE);
        }
    }
}

/// What [`materialize_one`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Made {
    /// Mapped the view now (or waited for the thread that did).
    Now,
    /// The page was a view already.
    Before,
    /// It could not be mapped.
    Failed,
}

/// Maps `e`'s page as a view at `page`, once (another thread doing it is
/// waited for).
fn materialize_one(page: usize, e: &Entry) -> Made {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile3, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc, VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    loop {
        match e.state.compare_exchange(0, 1, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(2) => return Made::Before,
            Err(_) => {
                // Another thread maps it: that is this thread's view too.
                while e.state.load(Relaxed) == 1 {
                    std::thread::yield_now();
                }
                if e.state.load(Relaxed) == 2 {
                    return Made::Now;
                }
            }
        }
    }
    // A view of what the address showed before, retired and not unmapped
    // yet (`VICTORY_REMAP_LAZY`): it goes first.
    if PENDING_N.load(Relaxed) != 0 {
        let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let before = p.len();
        p.retain(|&q| q != page);
        if p.len() != before {
            PENDING_N.store(p.len(), Relaxed);
            // The same physical page parked here (`VICTORY_REMAP_PARK`):
            // that view is this entry's view.
            if reuse(page, e) {
                drop(p);
                e.state.store(2, Relaxed);
                COUNTS[1].fetch_add(1, Relaxed);
                LAZY[2].fetch_add(1, Relaxed);
                return Made::Now;
            }
            unmap_view(page);
            LAZY[4].fetch_add(1, Relaxed);
        }
    }
    // A view parked here at an earlier end of a call (`VICTORY_REMAP_PARK`).
    if let Some(old) = unpark(page) {
        if old == (e.section, e.offset) && protect(page, windows_sys::Win32::System::Memory::PAGE_READWRITE) {
            e.state.store(2, Relaxed);
            PARK[0].fetch_add(1, Relaxed);
            COUNTS[1].fetch_add(1, Relaxed);
            LAZY[2].fetch_add(1, Relaxed);
            return Made::Now;
        }
        unmap_view(page);
        PARK[1].fetch_add(1, Relaxed);
    }
    let map = || {
        // SAFETY: maps the section's page in place of a placeholder.
        unsafe {
            MapViewOfFile3(
                e.section as _,
                GetCurrentProcess(),
                page as *const _,
                e.offset,
                PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        }
        .Value as usize
            == page
    };
    // A lone placeholder already: no split. Otherwise (or if that was
    // stale: the reservation was freed and made again) split it out first.
    let lone = LONE.lock().unwrap_or_else(|e| e.into_inner()).remove(&page);
    let mut mapped = lone && map();
    if lone && mapped {
        LAZY[5].fetch_add(1, Relaxed);
    }
    if !mapped {
        // SAFETY: `page` is a page of a placeholder no view shows (the
        // table's entry stands for it): split it out; acts on placeholders
        // only (fails harmlessly on a lone one).
        unsafe { VirtualFree(page as *mut _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) };
        mapped = map();
    }
    // SAFETY: the view just mapped; committing it, or back to a placeholder.
    let ok = mapped
        && (e.committed
            || unsafe { !VirtualAlloc(page as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE).is_null() }
            || unsafe {
                UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: page as *mut _ }, MEM_PRESERVE_PLACEHOLDER);
                false
            });
    e.state.store(if ok { 2 } else { 0 }, Relaxed);
    if ok {
        COUNTS[1].fetch_add(1, Relaxed);
        LAZY[2].fetch_add(1, Relaxed);
    } else {
        let error = std::io::Error::last_os_error();
        let n = FAILED.fetch_add(1, Relaxed);
        // Every failure up to 32, then every 1024th.
        if n < 32 || n.is_multiple_of(1024) {
            let (state, kind, size, protect) = region(page);
            tracing::warn!(
                page = format_args!("{page:#x}"),
                %error,
                failures = n + 1,
                state = format_args!("{state:#x}"),
                kind = format_args!("{kind:#x}"),
                size = format_args!("{size:#x}"),
                protect = format_args!("{protect:#x}"),
                "VICTORY_MEM2_GPU_ONLY_NOVIEW: a translated page could not be mapped as a view"
            );
        }
    }
    if ok { Made::Now } else { Made::Failed }
}

/// Views that could not be mapped, since the start.
pub(crate) static FAILED: AtomicU64 = AtomicU64::new(0);

/// What `VirtualQuery` says of the region at `page`: state, type, size and
/// protection (zeros if it says nothing).
fn region(page: usize) -> (u32, u32, usize, u32) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    if unsafe { VirtualQuery(page as *const _, &mut info, size_of_val(&info)) } == 0 {
        return (0, 0, 0, 0);
    }
    (info.State, info.Type, info.RegionSize, info.Protect)
}

/// Recoveries in the fault handler, since the start: 0 a view that could not
/// be mapped, mapped on a later try; 1 an entry said to be a view whose
/// page was a placeholder again, made a view anew; 2 a view not committed,
/// committed on the access.
pub(crate) static HEALED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// [`materialize_one`] for a CPU fault on `page`, which must not end in a
/// crash while the page has an entry:
/// - a view that could not be mapped is tried again (another thread's
///   operation on the same placeholder made it fail for a moment: the
///   remap_lazy racing test, about one run in three, a retry always
///   mapped it);
/// - an entry said to be a view whose page is a placeholder again (its view
///   was taken away under it) is made a view anew;
/// - a view of a section not committed there is committed on the access,
///   read or write (sections are no longer committed whole under
///   `VICTORY_REMAP_LAZY`).
/// A view that is mapped and committed but not writable is left to the
/// guards (`Made::Before`).
fn fault_in(page: usize, e: &Entry) -> Made {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    let mut made = materialize_one(page, e);
    for _ in 0..10_000 {
        match made {
            Made::Failed => {
                std::thread::yield_now();
                made = materialize_one(page, e);
                if made == Made::Now {
                    HEALED[0].fetch_add(1, Relaxed);
                }
            }
            Made::Before => {
                let (state, kind, _, _) = region(page);
                if state == MEM_RESERVE && kind == MEM_PRIVATE {
                    // A placeholder: the view is gone. Mapped anew.
                    if e.state.compare_exchange(2, 0, Relaxed, Relaxed).is_ok() {
                        HEALED[1].fetch_add(1, Relaxed);
                    }
                    made = materialize_one(page, e);
                } else if state == MEM_RESERVE && kind == MEM_MAPPED {
                    // A view, not committed.
                    // SAFETY: commits the view of the entry's page in place.
                    if unsafe { !VirtualAlloc(page as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE).is_null() } {
                        HEALED[2].fetch_add(1, Relaxed);
                    }
                    return Made::Before;
                } else {
                    return Made::Before;
                }
            }
            Made::Now => return Made::Now,
        }
    }
    made
}

/// Whether `page` is mapped read-write now (a view another thread made
/// after this one faulted).
fn read_write(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(page as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

/// Registers the handler that maps a translated page when something
/// touches it in place, once, ahead of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || (PAGES.load(Relaxed) == 0 && CALLS.load(Relaxed) == 0)
            || HOLDING.with(|h| h.get())
            // SAFETY: the system passes the context with the record.
            || crate::safe_copy::is_write_at(unsafe { (*(*info).ContextRecord).Rip })
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        let made = {
            let t = table();
            t.get(at).map(|(page, e)| (page, fault_in(page, e)))
        };
        match made {
            Some((page, Made::Now)) => {
                // A view now, for the mapping record: copies read it in
                // place, the guards take its writes as they do any view's.
                crate::regions::mapped(page, PAGE);
                LAZY[3].fetch_add(1, Relaxed);
                EXCEPTION_CONTINUE_EXECUTION
            }
            // A view already: made by another thread after this one
            // faulted (run again), or the fault is not ours (a guard's
            // read-only block: its handler's). Saying "run again" to the
            // latter looped for ever.
            Some((page, Made::Before)) if read_write(page) => EXCEPTION_CONTINUE_EXECUTION,
            Some((_, Made::Before)) => {
                LAZY[6].fetch_add(1, Relaxed);
                EXCEPTION_CONTINUE_SEARCH
            }
            // Not mapped (logged there), but the page is read-write now all
            // the same (another thread's view of it): run again.
            Some((page, Made::Failed)) if read_write(page) => EXCEPTION_CONTINUE_EXECUTION,
            // No entry while a call of page mappings is under way
            // (`VICTORY_REMAP_LAZY`): the page may be between its old entry
            // and its new one. Wait for the call to end and run again; a
            // page that shows nothing then faults again and is not ours.
            None if CALLS.load(Relaxed) != 0 && (LO.load(Relaxed)..HI.load(Relaxed)).contains(&at) => {
                while CALLS.load(Relaxed) != 0 && !table().any_in(at, 1) {
                    std::thread::yield_now();
                }
                EXCEPTION_CONTINUE_EXECUTION
            }
            _ => EXCEPTION_CONTINUE_SEARCH,
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler touches only the table and maps placeholders.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_fault));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Destination pages far from anything mapped: the table is only read
    /// for them, never touched in place.
    const AT: usize = 0x7f30_0000_0000;

    #[test]
    fn reads_and_writes_go_through_the_table_and_the_rest_through_plain() {
        let mut page1 = vec![0u8; PAGE];
        let mut page2 = vec![0u8; PAGE];
        for (i, b) in page1.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        page2.fill(0xab);
        // Pages 1 and 2 of four shown through the table.
        title_memory_translated(AT + PAGE, page1.as_mut_ptr() as usize, 0, 0, true);
        title_memory_translated(AT + 2 * PAGE, page2.as_mut_ptr() as usize, 0, 0, true);
        assert!(any(AT, PAGE + 1));
        assert!(!any(AT, PAGE));
        assert_eq!(bytes_in(AT + 100, 4 * PAGE), 2 * PAGE);
        // A read from inside page 0 to inside page 3.
        let (from, len) = (AT + PAGE - 10, 2 * PAGE + 20);
        let mut out = vec![0u8; len];
        let mut plain = Vec::new();
        let ok = read(from, out.as_mut_ptr(), len, |at, d, n| {
            plain.push((at, n));
            // SAFETY: inside `out`.
            unsafe { d.write_bytes(0xee, n) };
            true
        });
        assert!(ok);
        assert_eq!(plain, vec![(from, 10), (AT + 3 * PAGE, 10)]);
        assert_eq!(&out[..10], &[0xee; 10]);
        assert_eq!(&out[10..10 + PAGE], &page1[..]);
        assert!(out[10 + PAGE..10 + 2 * PAGE].iter().all(|&b| b == 0xab));
        assert_eq!(&out[10 + 2 * PAGE..], &[0xee; 10]);
        // A plain piece that fails fails the read.
        assert!(!read(AT, out.as_mut_ptr(), PAGE + 1, |_, _, _| false));
        // The kernel-path read of the runtime translates too.
        let mut two = [0u8; 2];
        assert!(crate::host::read_into((AT + PAGE + 5) as *const u8, &mut two));
        assert_eq!(two, [5, 6]);
        // Writes land in the pages, the rest goes to plain.
        let data = vec![0x11u8; 30];
        let mut wrote = Vec::new();
        write(AT + 2 * PAGE - 15, data.as_ptr(), 30, |at, _, n| wrote.push((at, n)));
        assert_eq!(wrote, Vec::<(usize, usize)>::new());
        assert!(page1[PAGE - 15..].iter().all(|&b| b == 0x11));
        assert!(page2[..15].iter().all(|&b| b == 0x11) && page2[15] == 0xab);
        write(AT + 3 * PAGE - 1, data.as_ptr(), 2, |at, _, n| wrote.push((at, n)));
        assert_eq!(wrote, vec![(AT + 3 * PAGE, 1)]);
        assert_eq!(page2[PAGE - 1], 0x11);
        // Untranslated: gone, and the read is all plain.
        assert_eq!(title_memory_untranslated(AT, 4 * PAGE), 2);
        assert!(!any(AT, 4 * PAGE));
        let mut plain = 0;
        assert!(read(AT, out.as_mut_ptr(), PAGE, |_, _, n| {
            plain += n;
            true
        }));
        assert_eq!(plain, PAGE);
        assert_eq!(title_memory_untranslated(AT, 4 * PAGE), 0);
    }

    /// A section of `n` 64 KiB pages, committed, each page tagged with its
    /// index, and a view of the whole of it: (section, view).
    fn tagged_section(n: usize) -> (usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE, SEC_COMMIT};
        // SAFETY: plain section creation and a view of it.
        let section = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_COMMIT, 0, (n * PAGE) as u32, std::ptr::null())
        };
        assert!(!section.is_null());
        let view = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, n * PAGE) }.Value as usize;
        assert_ne!(view, 0);
        for k in 0..n {
            // SAFETY: inside the view.
            unsafe { ((view + k * PAGE) as *mut u64).write(0x7a90_0000 + k as u64) };
        }
        (section as usize, view)
    }

    /// A placeholder of `n` pages.
    fn placeholder(n: usize) -> usize {
        use windows_sys::Win32::System::Memory::{MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, VirtualAlloc2};
        // SAFETY: a fresh reservation of our own.
        let at = unsafe {
            VirtualAlloc2(0 as _, std::ptr::null(), n * PAGE, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, std::ptr::null_mut(), 0)
        } as usize;
        assert_ne!(at, 0);
        at
    }

    fn tag_at(page: usize) -> u64 {
        // SAFETY: a view the test made there.
        unsafe { (page as *const u64).read_volatile() }
    }

    /// The test serializes on this: the switches below are process-wide.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// An entry replaced while another thread makes the new one a view: the
    /// old view must be retired before the table lets go, or the other
    /// thread maps over it (error 487, `Made::Failed`) and the old view
    /// stays mapped, untracked: the RL-C crash's 487.
    #[test]
    fn lazy_race_replace_while_mapping() {
        use std::sync::atomic::AtomicBool;
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        LAZY_TEST.store(true, Relaxed);
        let (section, view) = tagged_section(2);
        let at = placeholder(1);
        let failed = FAILED.load(Relaxed);
        title_memory_translated(at, view, section, 0, true);
        materialize(at, PAGE);
        assert_eq!(tag_at(at), 0x7a90_0000);
        PAUSE_AT.store(at, Relaxed);
        for round in 0..40usize {
            let (from, to) = (round % 2, (round + 1) % 2);
            assert_eq!(tag_at(at), 0x7a90_0000 + from as u64);
            let stop = AtomicBool::new(false);
            std::thread::scope(|s| {
                s.spawn(|| {
                    while !stop.load(Relaxed) {
                        materialize(at, PAGE);
                    }
                });
                title_memory_translated(at, view + to * PAGE, section, (to * PAGE) as u64, true);
                materialize(at, PAGE);
                stop.store(true, Relaxed);
            });
            flush_unmaps();
            materialize(at, PAGE);
            assert_eq!(FAILED.load(Relaxed), failed, "a view mapped over a retired one (round {round})");
        }
        PAUSE_AT.store(0, Relaxed);
        title_memory_untranslated(at, PAGE);
        flush_unmaps();
        LAZY_TEST.store(false, Relaxed);
    }

    /// The end of a call unmaps retired views while another thread makes
    /// new entries at the same addresses views: each page is either still
    /// on the list (unmapped first) or unmapped already, never neither.
    #[test]
    fn lazy_race_flush_while_mapping() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        LAZY_TEST.store(true, Relaxed);
        const N: usize = 256;
        let (section, view) = tagged_section(2);
        let at = placeholder(N);
        let failed = FAILED.load(Relaxed);
        for round in 0..6usize {
            let (from, to) = (round % 2, (round + 1) % 2);
            for k in 0..N {
                title_memory_translated(at + k * PAGE, view + from * PAGE, section, (from * PAGE) as u64, true);
            }
            materialize(at, N * PAGE);
            for k in 0..N {
                assert_eq!(tag_at(at + k * PAGE), 0x7a90_0000 + from as u64);
                title_memory_translated(at + k * PAGE, view + to * PAGE, section, (to * PAGE) as u64, true);
            }
            assert!(pending() >= N);
            PAUSE_AT.store(usize::MAX, Relaxed);
            std::thread::scope(|s| {
                s.spawn(flush_unmaps);
                std::thread::sleep(std::time::Duration::from_millis(5));
                for k in (0..N).rev() {
                    materialize(at + k * PAGE, PAGE);
                }
            });
            PAUSE_AT.store(0, Relaxed);
            flush_unmaps();
            assert_eq!(FAILED.load(Relaxed), failed, "round {round}");
            for k in 0..N {
                assert_eq!(tag_at(at + k * PAGE), 0x7a90_0000 + to as u64, "page {k} round {round}");
            }
            title_memory_untranslated(at, N * PAGE);
            flush_unmaps();
        }
        LAZY_TEST.store(false, Relaxed);
    }

    /// `VICTORY_REMAP_PARK`: a view taken away is parked (no access), and
    /// reused when the same page is shown at the same address again; a
    /// different page there unmaps it first.
    #[test]
    fn lazy_park_reuses_the_same_page_only() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        LAZY_TEST.store(true, Relaxed);
        PARK_TEST.store(true, Relaxed);
        let (section, view) = tagged_section(2);
        let at = placeholder(1);
        let failed = FAILED.load(Relaxed);
        title_memory_translated(at, view, section, 0, true);
        materialize(at, PAGE);
        title_memory_untranslated(at, PAGE);
        flush_unmaps();
        assert!(PARKED.lock().unwrap().contains_key(&at));
        assert!(!read_write(at), "a parked view is not accessible");
        let reused = PARK[0].load(Relaxed);
        title_memory_translated(at, view, section, 0, true);
        materialize(at, PAGE);
        assert_eq!(PARK[0].load(Relaxed), reused + 1);
        assert_eq!(tag_at(at), 0x7a90_0000);
        title_memory_untranslated(at, PAGE);
        flush_unmaps();
        let other = PARK[1].load(Relaxed);
        title_memory_translated(at, view + PAGE, section, PAGE as u64, true);
        materialize(at, PAGE);
        assert_eq!(PARK[1].load(Relaxed), other + 1);
        assert_eq!(tag_at(at), 0x7a90_0001);
        // Replaced before the end of the call by the same page: kept.
        title_memory_translated(at, view, section, 0, true);
        title_memory_translated(at, view + PAGE, section, PAGE as u64, true);
        let reused = PARK[0].load(Relaxed);
        materialize(at, PAGE);
        assert_eq!(PARK[0].load(Relaxed), reused + 1);
        assert_eq!(tag_at(at), 0x7a90_0001);
        assert_eq!(FAILED.load(Relaxed), failed);
        title_memory_untranslated(at, PAGE);
        flush_unmaps();
        PARK_TEST.store(false, Relaxed);
        LAZY_TEST.store(false, Relaxed);
    }

    /// A CPU read, in place, of a page shown through the table whose
    /// section is reserved, not committed (`VICTORY_REMAP_LAZY` without
    /// `_COMMIT`): the fault makes it a committed view and the read sees the
    /// page. Then the view is taken away under its entry (what the racing
    /// test caught): the next read makes it a view anew instead of crashing.
    #[test]
    fn lazy_cpu_read_of_an_uncommitted_page() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            PAGE_READWRITE, SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: plain section creation and a view of it.
        let section = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE | SEC_RESERVE, 0, (2 * PAGE) as u32, std::ptr::null())
        };
        assert!(!section.is_null());
        let whole = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * PAGE) }.Value as usize;
        assert_ne!(whole, 0);
        // The runtime's own copy of page 1 is committed (as xmem's
        // `translate` does); a view of it elsewhere is not.
        assert!(!unsafe { VirtualAlloc((whole + PAGE) as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE) }.is_null());
        // SAFETY: inside the committed page.
        unsafe { ((whole + PAGE + 8) as *mut u64).write(0x1a2b_3c4d) };
        let at = placeholder(4) + PAGE;
        title_memory_translated(at, whole + PAGE, section as usize, PAGE as u64, false);
        assert_eq!(tag_at(at + 8), 0x1a2b_3c4d, "read through the fault");
        assert!(read_write(at), "a committed, writable view now");
        // The view taken away under its entry.
        let healed = HEALED[1].load(Relaxed);
        // SAFETY: the view the fault made.
        assert_ne!(
            unsafe { UnmapViewOfFile2(GetCurrentProcess(), MEMORY_MAPPED_VIEW_ADDRESS { Value: at as *mut _ }, MEM_PRESERVE_PLACEHOLDER) },
            0
        );
        assert_eq!(tag_at(at + 8), 0x1a2b_3c4d, "made a view anew");
        assert_eq!(HEALED[1].load(Relaxed), healed + 1);
        title_memory_untranslated(at, PAGE);
        flush_unmaps();
    }
}
