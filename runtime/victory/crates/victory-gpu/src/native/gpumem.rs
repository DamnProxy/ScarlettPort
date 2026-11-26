//! `VICTORY_NATIVE_GPUMEM=1` (off by default, boot only): milestone 1 of
//! NATIVE-BOUNDARY.md §4. Memory the CPU never touches lives on the card:
//!
//! - The render-target pool: one host heap the pool's size and one reserved
//!   buffer spanning the title's reservation the pool's pages are mapped
//!   into. Each page map or unmap of the title is a tile mapping of that
//!   buffer (queued, `UpdateTileMappings` before the next submission), so
//!   addresses that show one page show one tile, as on the console.
//!   Buffers, root views, vertex/index views, indirect arguments and copies
//!   at pool addresses resolve to it (`crate::shadow` `lookup`/`exact`
//!   consult [`find`] first) as a copy "in place": nothing refreshed,
//!   watched, snapshotted or copied back.
//! - Unordered-access buffers the title makes in a GPU-only reservation
//!   (0x801) whose title bytes are all zero at first use: a buffer of the
//!   card's own, the same way.
//! - Target/depth/unordered-access textures made by device 0x3f8: no title
//!   memory (their contents were the card's alone already); their address
//!   comes from a reserved range that is never committed.
//!
//! Anything else, and anything ambiguous (a range only partly in the pool,
//! a page not mapped, an excluded or demoted range, a read-back buffer,
//! title bytes present), takes today's path and is counted.
//! `VICTORY_NATIVE_GPUMEM_EXCLUDE=<start-end|start+len,...>` (hex) keeps
//! ranges on today's path; a CPU touch the lazy remap handler sees in a
//! native range demotes its page to today's path from then on (what the
//! card holds there is not in title memory: counted, not exact).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

use super::table::{Bump, Ranges, Table};
use crate::shadow::Shadow;

/// Counts since the start (logged every 5 s at a submission).
pub(crate) static STATS: [AtomicU64; 19] = [const { AtomicU64::new(0) }; 19];
const RT_HIT: usize = 0;
const RT_FALLBACK: usize = 1;
const BUF_NATIVE: usize = 2;
const BUF_FALLBACK: usize = 3;
const TEX_NATIVE: usize = 4;
const TEX_FALLBACK: usize = 5;
const TILE_UPDATES: usize = 6;
const TILE_CALLS: usize = 7;
const DEMOTED: usize = 8;
const EXCLUDED: usize = 9;
const BUF_HIT: usize = 10;
const NOT_MADE: usize = 11;
const BUF_DATA: usize = 12;
const TEX_BYTES: usize = 13;
const COPIES: usize = 14;
const COPY_FALLBACK: usize = 15;
const TEX_POOL_ASKED: usize = 16;
const TEX_POOL: usize = 17;
const TEX_POOL_FALLBACK: usize = 18;
const NAMES: [&str; 19] = [
    "rt hits", "rt fallbacks", "buffers made", "buffer fallbacks", "textures made", "texture fallbacks",
    "tile updates", "tile calls", "demoted pages", "excluded asks", "buffer hits", "pool not made yet",
    "buffers with title bytes", "texture bytes", "copies on the card", "copies dropped", "pool textures asked", "pool textures made", "pool texture fallbacks",
];

/// Buffer fallbacks by reason.
static WHY: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const WHY_NAMES: [&str; 7] = ["no UAV flag", "read-back", "excluded", "demoted", "title bytes", "no heap", "overlaps native"];

/// The pool's pages have no virtual range of their own: `PhysPool_Init`
/// (title+0x22163d0) only allocates physical pages, and the title maps them
/// wherever it places targets. So the card side is one heap the pool's size
/// and, per 256 MiB window of addresses a pool page was mapped in, one
/// reserved buffer over that window whose tiles follow the title's maps.
const WINDOW: u64 = 256 << 20;

struct RtWindow {
    buffer: ID3D12Resource,
    shadow: Arc<Shadow>,
    /// Something bound it: its tiles follow the title's maps from then on.
    /// Until then no tile call is made for it (7,671 calls in a run with
    /// no hit cost prep and card time).
    live: std::sync::atomic::AtomicBool,
    /// Its tiles are behind the title's maps (it was not live).
    stale: bool,
}

/// A target texture whose memory is all pool pages: a reserved texture
/// whose tiles are the pool's, in the order of its pages.
struct RtTexture {
    texture: ID3D12Resource,
    start: u64,
    tiles: u32,
    /// Mapped once at a submission.
    mapped: bool,
}

struct State {
    /// The pool's heap on the card, once made.
    heap: Option<ID3D12Heap>,
    /// Reserved buffers by window base.
    windows: std::collections::BTreeMap<u64, RtWindow>,
    pool_textures: Vec<RtTexture>,
    /// Making the pool failed: never tried again.
    rt_failed: bool,
    /// Native buffers by title start: (resource, shadow).
    buffers: Table<(usize, Arc<Shadow>)>,
    demoted: Ranges,
    textures: Option<Bump>,
    last_line: Option<std::time::Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    heap: None,
    windows: std::collections::BTreeMap::new(),
    pool_textures: Vec::new(),
    rt_failed: false,
    buffers: Table::new(),
    demoted: Ranges::new(),
    textures: None,
    last_line: None,
});
/// Windows made, read without the lock.
static WINDOWS: AtomicU64 = AtomicU64::new(0);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for State {}
/// Native buffers, read without the lock.
static BUFFERS: AtomicU64 = AtomicU64::new(0);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn on() -> bool {
    super::gpumem()
}

fn excluded(start: u64, end: u64) -> bool {
    static EX: OnceLock<Ranges> = OnceLock::new();
    let ex = EX.get_or_init(|| {
        let r = Ranges::parse(&std::env::var("VICTORY_NATIVE_GPUMEM_EXCLUDE").unwrap_or_default());
        if !r.is_empty() {
            tracing::warn!(ranges = ?r, "VICTORY_NATIVE_GPUMEM_EXCLUDE");
        }
        r
    });
    ex.meets(start, end)
}

/// A placeholder reservation of the title's (no longer needed).
pub(crate) fn reserved(_at: u64, _len: u64) {}

/// The pool's first page was mapped at `at`.
pub(crate) fn pool_page_at(at: u64) {
    if on() {
        tracing::warn!(first_page = format_args!("{at:#x}"), "native: the RT pool's first page is mapped");
    }
}

/// A CPU touch of `at` (the lazy remap handler made a view): a native page
/// there is demoted.
pub(crate) fn touched(at: usize) {
    if !on() {
        return;
    }
    let page = at as u64 & !0xffff;
    let in_span = WINDOWS.load(Relaxed) != 0 && super::pool().as_ref().is_some_and(|p| p.covers(page as usize, 1));
    let mut s = state();
    if in_span || s.buffers.find(page, 1).is_some() {
        if !s.demoted.meets(page, page + 0x1_0000) {
            s.demoted.add(page, page + 0x1_0000);
            let n = STATS[DEMOTED].fetch_add(1, Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(n, page = format_args!("{page:#x}"), "native: a CPU touch demoted a page to today's path");
            }
        }
    }
}

/// What serves `[address, address + len)` on the card, if it is native:
/// the pool's span or a native buffer.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let r = find_inner(address, len);
    static SEEN: AtomicU64 = AtomicU64::new(0);
    if WINDOWS.load(Relaxed) != 0 && SEEN.fetch_add(1, Relaxed) < 10 {
        tracing::warn!(address = format_args!("{address:#x}"), len, hit = r.is_some(),
            windows = WINDOWS.load(Relaxed), "native: lookup");
    }
    r
}

fn find_inner(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let base = address & !(WINDOW - 1);
    let windowed = WINDOWS.load(Relaxed) != 0;
    if !windowed && BUFFERS.load(Relaxed) == 0 {
        return None;
    }
    // A pool page: every page of the range mapped from the pool, one window.
    let in_pool = windowed && super::pool().as_ref().is_some_and(|p| p.covers(address as usize, 1));
    let s = state();
    if !s.demoted.is_empty() && s.demoted.meets(address & !0xffff, end) {
        return None;
    }
    if in_pool {
        let whole = (end - 1) & !(WINDOW - 1) == base
            && super::pool().as_ref().is_some_and(|p| p.covers(address as usize, (end - address) as usize));
        if whole
            && !excluded(address, end)
            && let Some(w) = s.windows.get(&base)
        {
            STATS[RT_HIT].fetch_add(1, Relaxed);
            w.live.store(true, Relaxed);
            return Some(w.shadow.clone());
        }
        STATS[RT_FALLBACK].fetch_add(1, Relaxed);
        return None;
    }
    let (_, (_, shadow)) = s.buffers.find(address, end - address)?;
    STATS[BUF_HIT].fetch_add(1, Relaxed);
    Some(shadow.clone())
}

/// A title buffer `[start, start + len)` (resource `resource`, flags
/// `flags`) wants a host side: a buffer of the card's own if it may be.
pub(crate) fn buffer(resource: usize, start: u64, len: u64, flags: u32) -> Option<Arc<Shadow>> {
    if !on() || len == 0 {
        return None;
    }
    let end = start.checked_add(len)?;
    {
        let s = state();
        if let Some((at, (r, shadow))) = s.buffers.find(start, len)
            && at == start
            && *r == resource
        {
            return Some(shadow.clone());
        }
    }
    const UNORDERED: u32 = 0x4;
    let gpu_only = crate::watch::gpu_only_ranges().iter().any(|&(a, b)| a as u64 <= start && end <= b as u64);
    if !gpu_only {
        return None;
    }
    let fallback = |why: usize| {
        STATS[BUF_FALLBACK].fetch_add(1, Relaxed);
        WHY[why].fetch_add(1, Relaxed);
        if WHY[why].load(Relaxed) <= 3 {
            tracing::warn!(why = WHY_NAMES[why], start = format_args!("{start:#x}"), len, flags = format_args!("{flags:#x}"), "native: buffer fallback");
        }
        None
    };
    if flags & UNORDERED == 0 {
        return fallback(0);
    }
    if crate::readback::tagged_in(start, len) {
        return fallback(1);
    }
    if excluded(start, end) {
        STATS[EXCLUDED].fetch_add(1, Relaxed);
        return fallback(2);
    }
    if state().demoted.meets(start, end) {
        return fallback(3);
    }
    // Title bytes there (an initial fill through the CPU view): today's path.
    let mut bytes = vec![0u8; len as usize];
    if !crate::host::read_into(start as *const u8, &mut bytes) || bytes.iter().any(|&b| b != 0) {
        STATS[BUF_DATA].fetch_add(1, Relaxed);
        return fallback(4);
    }
    drop(bytes);
    let host = crate::host::host()?;
    let size = len.next_multiple_of(1 << 16);
    let Some((heap, buffer)) = heap_and_buffer(&host.device, size, size, false) else {
        return fallback(5);
    };
    crate::host::name(&buffer, || format!("native buffer {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let shadow = Arc::new(Shadow::native(buffer, heap, gpu, start, len));
    let mut s = state();
    if !s.buffers.insert(start, end, (resource, shadow.clone())) {
        // Overlaps a native buffer (an alias of another size): today's path.
        return fallback(6);
    }
    BUFFERS.fetch_add(1, Relaxed);
    STATS[BUF_NATIVE].fetch_add(1, Relaxed);
    Some(shadow)
}

/// The title let go of buffer `resource` at `start`.
pub(crate) fn released(resource: usize, start: u64) {
    if BUFFERS.load(Relaxed) == 0 {
        return;
    }
    let mut s = state();
    if s.buffers.find(start, 1).is_some_and(|(at, (r, _))| at == start && *r == resource) {
        s.buffers.remove(start);
        BUFFERS.fetch_sub(1, Relaxed);
    }
}

/// An address for a card-only texture of `size` bytes (device 0x3f8).
pub(crate) fn texture_address(size: u64, align: u64) -> Option<u64> {
    if !on() {
        return None;
    }
    let mut s = state();
    if s.textures.is_none() {
        use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
        const LEN: usize = 64 << 30;
        // SAFETY: a reservation of our own, never committed.
        let at = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE, PAGE_NOACCESS) } as u64;
        if at == 0 {
            STATS[TEX_FALLBACK].fetch_add(1, Relaxed);
            return None;
        }
        tracing::warn!(at = format_args!("{at:#x}"), "native: card-only texture addresses reserved");
        s.textures = Some(Bump::new(at, LEN as u64));
    }
    let bump = s.textures.as_mut()?;
    match bump.take(size, align) {
        Some(at) => {
            STATS[TEX_NATIVE].fetch_add(1, Relaxed);
            STATS[TEX_BYTES].fetch_add(size, Relaxed);
            Some(at)
        }
        None => {
            STATS[TEX_FALLBACK].fetch_add(1, Relaxed);
            None
        }
    }
}

fn make_heap(device: &ID3D12Device, bytes: u64) -> Option<ID3D12Heap> {
    let hdesc = D3D12_HEAP_DESC {
        SizeInBytes: bytes,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: 0,
        // Buffers and textures (pool targets) where the card allows it.
        Flags: if textures_ok() { D3D12_HEAP_FLAG_NONE } else { D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS },
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain creation into an out pointer of ours.
    match unsafe { device.CreateHeap(&raw const hdesc, &raw mut heap) } {
        Ok(()) => heap,
        Err(e) => {
            tracing::warn!(error = %e, bytes, "native: no heap for the RT pool; today's path");
            None
        }
    }
}

fn make_reserved(device: &ID3D12Device, len: u64) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation into an out pointer of ours.
    match unsafe { device.CreateReservedResource(&raw const desc, D3D12_RESOURCE_STATE_COMMON, None, &raw mut buffer) } {
        Ok(()) => buffer,
        Err(e) => {
            tracing::warn!(error = %e, len, "native: no reserved buffer for an RT pool window");
            None
        }
    }
}

/// A heap of `heap_bytes` and a buffer of `len` bytes over it: placed, or
/// reserved (its tiles mapped later).
fn heap_and_buffer(device: &ID3D12Device, heap_bytes: u64, len: u64, reserved: bool) -> Option<(ID3D12Heap, ID3D12Resource)> {
    let hdesc = D3D12_HEAP_DESC {
        SizeInBytes: heap_bytes,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut heap: Option<ID3D12Heap> = None;
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creations into out pointers of ours.
    let made = unsafe {
        device.CreateHeap(&raw const hdesc, &raw mut heap).and_then(|()| {
            if reserved {
                device.CreateReservedResource(&raw const desc, D3D12_RESOURCE_STATE_COMMON, None, &raw mut buffer)
            } else {
                let h = heap.as_ref().ok_or_else(windows::core::Error::empty)?;
                device.CreatePlacedResource(h, 0, &raw const desc, D3D12_RESOURCE_STATE_COMMON, None, &raw mut buffer)
            }
        })
    };
    match made {
        Ok(()) => Some((heap?, buffer?)),
        Err(e) => {
            tracing::warn!(error = %e, heap_bytes, len, reserved, "native: no heap or buffer; today's path");
            None
        }
    }
}

/// Before a submission, on its queue: makes the pool on the card once its
/// span is known, and applies the title's page maps since the last one as
/// tile mappings.
pub(crate) fn flush(device: &ID3D12Device, queue: &ID3D12CommandQueue) {
    if !on() {
        return;
    }
    let mut s = state();
    line(&mut s);
    if s.rt_failed {
        return;
    }
    let mut pool = super::pool();
    let Some(p) = pool.as_mut() else { return };
    if p.mapped_len() == 0 && s.heap.is_none() {
        return;
    }
    let updates: Vec<super::pool::Update> = if s.heap.is_none() {
        let heap_bytes = p.tiles() as u64 * 0x1_0000;
        let Some(heap) = make_heap(device, heap_bytes) else {
            s.rt_failed = true;
            STATS[NOT_MADE].fetch_add(1, Relaxed);
            return;
        };
        tracing::warn!(tiles = p.tiles(), "native: the RT pool's heap is on the card");
        s.heap = Some(heap);
        let _ = p.take_updates();
        p.mapped().map(|(at, tile)| super::pool::Update { at, tile: Some(tile) }).collect()
    } else {
        p.take_updates()
    };
    drop(pool);
    if updates.is_empty() {
        let heap = s.heap.clone().expect("made above");
        textures_flush(&mut s, queue, &heap, &[]);
        return;
    }
    let heap = s.heap.clone().expect("made above");
    let changed: Vec<u64> = updates.iter().map(|u| u.at as u64).collect();
    let mut by_window: std::collections::BTreeMap<u64, Vec<super::pool::Update>> = std::collections::BTreeMap::new();
    for u in updates {
        by_window.entry(u.at as u64 & !(WINDOW - 1)).or_default().push(u);
    }
    for (base, updates) in by_window {
        if !s.windows.contains_key(&base) {
            if updates.iter().all(|u| u.tile.is_none()) {
                continue;
            }
            let Some(buffer) = make_reserved(device, WINDOW) else { continue };
            crate::host::name(&buffer, || format!("native RT window {base:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            let shadow = Arc::new(Shadow::native(buffer.clone(), heap.clone(), gpu, base, WINDOW));
            s.windows.insert(base, RtWindow { buffer, shadow, live: std::sync::atomic::AtomicBool::new(false), stale: true });
            let n = WINDOWS.fetch_add(1, Relaxed) + 1;
            tracing::warn!(base = format_args!("{base:#x}"), windows = n, "native: an RT pool window is on the card");
        }
        let w = s.windows.get_mut(&base).expect("made above");
        if !w.live.load(Relaxed) {
            w.stale = true;
            continue;
        }
        if w.stale {
            continue; // mapped whole below
        }
        map_tiles(queue, &w.buffer, &heap, base, &updates);
    }
    // Windows that went live behind the title's maps: all their pages now.
    let stale: Vec<u64> = s.windows.iter().filter(|(_, w)| w.stale && w.live.load(Relaxed)).map(|(&b, _)| b).collect();
    if !stale.is_empty() {
        let all: Vec<super::pool::Update> = super::pool()
            .as_ref()
            .map(|p| p.mapped().map(|(at, tile)| super::pool::Update { at, tile: Some(tile) }).collect())
            .unwrap_or_default();
        for base in stale {
            let w = s.windows.get_mut(&base).expect("listed");
            w.stale = false;
            let mine: Vec<_> = all.iter().filter(|u| u.at as u64 & !(WINDOW - 1) == base).map(|u| super::pool::Update { at: u.at, tile: u.tile }).collect();
            if !mine.is_empty() {
                map_tiles(queue, &w.buffer, &heap, base, &mine);
            }
        }
    }
    textures_flush(&mut s, queue, &heap, &changed);
}

/// Pool textures: new ones mapped whole, live ones whose pages the title
/// mapped again given those pages' tiles. One call per texture.
fn textures_flush(s: &mut State, queue: &ID3D12CommandQueue, heap: &ID3D12Heap, changed: &[u64]) {
    s.pool_textures.retain(|t| crate::host::references(&t.texture) > 1);
    if s.pool_textures.is_empty() {
        return;
    }
    let pool = super::pool();
    let Some(p) = pool.as_ref() else { return };
    for t in &mut s.pool_textures {
        let end = t.start + u64::from(t.tiles) * 0x1_0000;
        let pages: Vec<u32> = if t.mapped {
            changed.iter().filter(|&&a| a >= t.start && a < end).map(|&a| ((a - t.start) >> 16) as u32).collect()
        } else {
            (0..t.tiles).collect()
        };
        if pages.is_empty() {
            continue;
        }
        t.mapped = true;
        let coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> = pages
            .iter()
            .map(|&i| D3D12_TILED_RESOURCE_COORDINATE { X: i, Y: 0, Z: 0, Subresource: 0 })
            .collect();
        let tiles: Vec<Option<u32>> = pages.iter().map(|&i| p.lookup((t.start + u64::from(i) * 0x1_0000) as usize).map(|(tile, _)| tile)).collect();
        // Region i: tile i of the resource counted across subresources
        // (D3D12 numbers a reserved resource's tiles in subresource order
        // when a region does not use a box).
        let sizes = vec![D3D12_TILE_REGION_SIZE { NumTiles: 1, ..Default::default() }; pages.len()];
        let flags: Vec<D3D12_TILE_RANGE_FLAGS> =
            tiles.iter().map(|t| if t.is_some() { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_NULL }).collect();
        let starts: Vec<u32> = tiles.iter().map(|t| t.unwrap_or(0)).collect();
        let counts = vec![1u32; pages.len()];
        // SAFETY: a live reserved texture and heap of ours; arrays of one
        // length that live through the call; tile indices inside the heap.
        unsafe {
            queue.UpdateTileMappings(
                &t.texture,
                coords.len() as u32,
                Some(coords.as_ptr()),
                Some(sizes.as_ptr()),
                heap,
                starts.len() as u32,
                Some(flags.as_ptr()),
                Some(starts.as_ptr()),
                Some(counts.as_ptr()),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
        STATS[TILE_CALLS].fetch_add(1, Relaxed);
        STATS[TILE_UPDATES].fetch_add(pages.len() as u64, Relaxed);
    }
}

/// Whether a target texture's memory `[start, start + bytes)` is all pool
/// pages, the pool is on the card in a heap textures may use: it is then
/// made reserved ([`texture_on_pool`]).
pub(crate) fn texture_wants_pool(start: u64, bytes: u64) -> bool {
    if !on() || bytes == 0 || !textures_ok() {
        return false;
    }
    let covered = super::pool().as_ref().is_some_and(|p| p.covers(start as usize, bytes as usize));
    if covered {
        STATS[TEX_POOL_ASKED].fetch_add(1, Relaxed);
    }
    covered && state().heap.is_some()
}

/// `texture` (reserved) shows the pool pages at `start`: its tiles are
/// mapped at the next submission. False if its tiles do not fit the
/// title's memory (`bytes`): the caller makes it as before.
pub(crate) fn texture_on_pool(texture: &ID3D12Resource, start: u64, bytes: u64) -> bool {
    let Some(host) = crate::host::host() else { return false };
    let mut total = 0u32;
    // SAFETY: a live reserved texture; only the total is asked for.
    unsafe { host.device.GetResourceTiling(texture, Some(&raw mut total), None, None, None, 0, std::ptr::null_mut()) };
    if total == 0 || u64::from(total) * 0x1_0000 > bytes.next_multiple_of(0x1_0000) {
        STATS[TEX_POOL_FALLBACK].fetch_add(1, Relaxed);
        return false;
    }
    state().pool_textures.push(RtTexture { texture: texture.clone(), start, tiles: total, mapped: false });
    STATS[TEX_POOL].fetch_add(1, Relaxed);
    true
}

/// Whether the card lets one heap hold buffers and textures (resource heap
/// tier 2): pool textures need it.
fn textures_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let Some(host) = crate::host::host() else { return false };
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: a plain query into a struct of its own size.
        let ok = unsafe { host.device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, (&raw mut o).cast(), size_of_val(&o) as u32) }.is_ok()
            && o.ResourceHeapTier.0 >= D3D12_RESOURCE_HEAP_TIER_2.0
            && o.TiledResourcesTier.0 >= D3D12_TILED_RESOURCES_TIER_2.0;
        tracing::warn!(ok, "native: pool textures (resource heap tier 2, tiled resources tier 2)");
        ok
    })
}

fn map_tiles(queue: &ID3D12CommandQueue, buffer: &ID3D12Resource, heap: &ID3D12Heap, rt_start: u64, updates: &[super::pool::Update]) {
    let coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> = updates
        .iter()
        .map(|u| D3D12_TILED_RESOURCE_COORDINATE { X: ((u.at as u64 - rt_start) >> 16) as u32, Y: 0, Z: 0, Subresource: 0 })
        .collect();
    let sizes = vec![D3D12_TILE_REGION_SIZE { NumTiles: 1, ..Default::default() }; updates.len()];
    let flags: Vec<D3D12_TILE_RANGE_FLAGS> =
        updates.iter().map(|u| if u.tile.is_some() { D3D12_TILE_RANGE_FLAG_NONE } else { D3D12_TILE_RANGE_FLAG_NULL }).collect();
    let starts: Vec<u32> = updates.iter().map(|u| u.tile.unwrap_or(0)).collect();
    let counts = vec![1u32; updates.len()];
    // SAFETY: a live reserved buffer and heap of ours; `n` regions of one
    // tile each and `n` ranges of one tile each, in arrays that live through
    // the call; every tile index is inside the heap (the pool's size).
    unsafe {
        queue.UpdateTileMappings(
            buffer,
            coords.len() as u32,
            Some(coords.as_ptr()),
            Some(sizes.as_ptr()),
            heap,
            starts.len() as u32,
            Some(flags.as_ptr()),
            Some(starts.as_ptr()),
            Some(counts.as_ptr()),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
    }
    STATS[TILE_UPDATES].fetch_add(updates.len() as u64, Relaxed);
    STATS[TILE_CALLS].fetch_add(1, Relaxed);
}

fn line(s: &mut State) {
    let now = std::time::Instant::now();
    if s.last_line.is_some_and(|t| now - t < std::time::Duration::from_secs(5)) {
        return;
    }
    s.last_line = Some(now);
    let parts: Vec<String> = NAMES.iter().zip(&STATS).map(|(n, v)| format!("{n} {}", v.load(Relaxed))).collect();
    tracing::warn!(
        "native gpumem: {}; native buffers {}, demoted ranges {}; buffer fallbacks by reason: {}",
        parts.join(", "),
        s.buffers.len(),
        s.demoted.len(),
        WHY_NAMES.iter().zip(&WHY).map(|(n, v)| format!("{n} {}", v.load(Relaxed))).collect::<Vec<_>>().join(", ")
    );
}

/// A buffer copy or `CopyMemoryToMemoryX` of `len` bytes from `src` to
/// `dst` (title addresses): if either side is native, it is recorded on
/// the card here (the other side's host copy refreshed first, or its
/// shader-written range declared) and true is returned; otherwise false
/// (today's copy at submission).
pub(crate) fn copy_on_card(list: &crate::host::HostList, dst: u64, src: u64, len: u64) -> bool {
    if !on() || len == 0 || (WINDOWS.load(Relaxed) == 0 && BUFFERS.load(Relaxed) == 0) {
        return false;
    }
    let (to_native, from_native) = (find(dst, len), find(src, len));
    if to_native.is_none() && from_native.is_none() {
        return false;
    }
    let to = to_native.or_else(|| crate::shadow::find(dst, len));
    let from = from_native.or_else(|| crate::shadow::find(src, len));
    let (Some(to), Some(from)) = (to, from) else {
        // A side with no host side at all: nothing exact to do.
        STATS[COPY_FALLBACK].fetch_add(1, Relaxed);
        return true;
    };
    if to.left(dst) < len || from.left(src) < len {
        STATS[COPY_FALLBACK].fetch_add(1, Relaxed);
        return true;
    }
    from.refresh(list, src, len);
    to.gpu_writes(list, dst, len);
    // SAFETY: an open list; both buffers live (the list holds the copies
    // through `refresh`/`gpu_writes`; native ones live in the table).
    unsafe { list.list.CopyBufferRegion(&to.buffer, to.offset(dst), &from.buffer, from.offset(src), len) };
    STATS[COPIES].fetch_add(1, Relaxed);
    true
}
