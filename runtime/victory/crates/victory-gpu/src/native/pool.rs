//! The render-target pool's page bookkeeping (NATIVE-BOUNDARY §4 #1-#3).
//!
//! The title allocates the pool as one batch of physical pages
//! (`XMemAllocatePhysicalPages`, 0xDC00000 bytes) and maps them one page per
//! call into its own reservation. Here each pool page is a tile index (its
//! position in the batch), and each mapped address remembers its tile, so
//! a reserved D3D12 resource the size of the pool can show the same bytes
//! at the same addresses: an address maps to a tile, a tile's contents live
//! on the card only. Updates are queued for the next submission
//! ([`Pool::take_updates`]), in the title's order.

use std::collections::BTreeMap;

/// 64 KiB, the pool's page and D3D12's tile.
pub const PAGE: usize = 0x1_0000;
/// The pool's size the title asks for (`PhysPool_Init`).
pub const DEFAULT_BYTES: usize = 0xDC0_0000;

/// One queued tile-mapping change: the title address's page now shows
/// `tile`, or nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Update {
    pub at: usize,
    pub tile: Option<u32>,
}

#[derive(Default)]
pub struct Pool {
    /// First page number of the batch and the step between numbers (page
    /// numbers count 4 KiB frames).
    first: usize,
    step: usize,
    tiles: usize,
    /// Mapped title page address → tile.
    at: BTreeMap<usize, u32>,
    /// Tile → how many addresses show it (the title aliases transient
    /// targets: two addresses may show one page).
    shown: Vec<u16>,
    pending: Vec<Update>,
    /// Since the start: maps, unmaps, maps of a page outside the pool.
    pub counts: [u64; 3],
}

impl Pool {
    pub fn new(first: usize, step: usize, tiles: usize) -> Self {
        Self { first, step: step.max(1), tiles, shown: vec![0; tiles], ..Self::default() }
    }

    pub fn tiles(&self) -> usize {
        self.tiles
    }

    /// The tile of page number `page`, if it is one of the pool's.
    pub fn tile_of(&self, page: usize) -> Option<u32> {
        let d = page.checked_sub(self.first)?;
        (d % self.step == 0 && d / self.step < self.tiles).then(|| (d / self.step) as u32)
    }

    /// `page` (a page number) mapped at `at` (64 KiB aligned). False if it
    /// is not a pool page (an address that showed a pool page shows none
    /// then).
    pub fn map(&mut self, at: usize, page: usize) -> bool {
        let at = at & !(PAGE - 1);
        let Some(tile) = self.tile_of(page) else {
            self.unmap(at, 1);
            self.counts[2] += 1;
            return false;
        };
        if let Some(old) = self.at.insert(at, tile) {
            self.shown[old as usize] -= 1;
        }
        self.shown[tile as usize] += 1;
        self.pending.push(Update { at, tile: Some(tile) });
        self.counts[0] += 1;
        true
    }

    /// `n` pages from `at` show nothing now.
    pub fn unmap(&mut self, at: usize, n: usize) {
        let at = at & !(PAGE - 1);
        for k in 0..n {
            let a = at + k * PAGE;
            if let Some(old) = self.at.remove(&a) {
                self.shown[old as usize] -= 1;
                self.pending.push(Update { at: a, tile: None });
                self.counts[1] += 1;
            }
        }
    }

    /// The tile and the offset in it of title address `va`.
    pub fn lookup(&self, va: usize) -> Option<(u32, usize)> {
        self.at.get(&(va & !(PAGE - 1))).map(|&t| (t, va & (PAGE - 1)))
    }

    /// Whether `[va, va + len)` is mapped to pool pages throughout.
    pub fn covers(&self, va: usize, len: usize) -> bool {
        len > 0 && (va & !(PAGE - 1)..va + len).step_by(PAGE).all(|p| self.at.contains_key(&p))
    }

    /// Mapped addresses, in order.
    pub fn mapped(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        self.at.iter().map(|(&a, &t)| (a, t))
    }

    pub fn mapped_len(&self) -> usize {
        self.at.len()
    }

    /// How many addresses show `tile`.
    pub fn aliases(&self, tile: u32) -> u16 {
        self.shown.get(tile as usize).copied().unwrap_or(0)
    }

    /// The queued changes, joined: only the last change of each address
    /// counts, in the order of first change.
    pub fn take_updates(&mut self) -> Vec<Update> {
        let mut last: BTreeMap<usize, (usize, Option<u32>)> = BTreeMap::new();
        for (i, u) in self.pending.drain(..).enumerate() {
            let e = last.entry(u.at).or_insert((i, u.tile));
            e.1 = u.tile;
        }
        let mut out: Vec<(usize, Update)> = last.into_iter().map(|(at, (i, tile))| (i, Update { at, tile })).collect();
        out.sort_by_key(|&(i, _)| i);
        out.into_iter().map(|(_, u)| u).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: usize = 0x2_0000_0000;

    #[test]
    fn pages_are_tiles_in_batch_order() {
        let p = Pool::new(0x1000, 16, 4);
        assert_eq!(p.tile_of(0x1000), Some(0));
        assert_eq!(p.tile_of(0x1030), Some(3));
        assert_eq!(p.tile_of(0x1040), None, "past the pool");
        assert_eq!(p.tile_of(0x1008), None, "not a page start");
        assert_eq!(p.tile_of(0xff0), None);
    }

    #[test]
    fn map_remap_unmap_and_alias() {
        let mut p = Pool::new(0x1000, 16, 4);
        assert!(p.map(BASE, 0x1010));
        assert!(p.map(BASE + PAGE, 0x1010), "two addresses, one page");
        assert_eq!(p.aliases(1), 2);
        assert_eq!(p.lookup(BASE + 0x123), Some((1, 0x123)));
        assert!(p.covers(BASE + 10, 2 * PAGE - 20));
        assert!(!p.covers(BASE, 3 * PAGE));
        assert!(p.map(BASE, 0x1020), "remap");
        assert_eq!((p.aliases(1), p.aliases(2)), (1, 1));
        assert!(!p.map(BASE + PAGE, 0x9999), "not a pool page: unmapped");
        assert_eq!(p.aliases(1), 0);
        assert_eq!(p.lookup(BASE + PAGE), None);
        p.unmap(BASE, 4);
        assert_eq!(p.mapped_len(), 0);
        assert_eq!(p.counts, [3, 2, 1]);
    }

    #[test]
    fn updates_join_to_the_last_per_address() {
        let mut p = Pool::new(0, 16, 8);
        p.map(BASE + PAGE, 16);
        p.map(BASE, 32);
        p.map(BASE + PAGE, 48);
        p.unmap(BASE, 1);
        p.map(BASE + 2 * PAGE, 0);
        assert_eq!(
            p.take_updates(),
            vec![
                Update { at: BASE + PAGE, tile: Some(3) },
                Update { at: BASE, tile: None },
                Update { at: BASE + 2 * PAGE, tile: Some(0) },
            ]
        );
        assert!(p.take_updates().is_empty());
    }
}
