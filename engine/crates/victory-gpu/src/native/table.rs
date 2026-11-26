//! The native range table: title address range → what serves it on the
//! card, and plain range sets (exclusions, demotions). Pure, so tested
//! without a card.

use std::collections::BTreeMap;

/// Disjoint `[start, end)` ranges, each with a value.
pub struct Table<T> {
    at: BTreeMap<u64, (u64, T)>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self { at: BTreeMap::new() }
    }
}

impl<T> Table<T> {
    pub const fn new() -> Self {
        Self { at: BTreeMap::new() }
    }

    /// Adds `[start, end)`; false (nothing added) if it is empty or
    /// overlaps an entry.
    pub fn insert(&mut self, start: u64, end: u64, value: T) -> bool {
        if start >= end || self.overlaps(start, end) {
            return false;
        }
        self.at.insert(start, (end, value));
        true
    }

    pub fn remove(&mut self, start: u64) -> Option<T> {
        self.at.remove(&start).map(|(_, v)| v)
    }

    /// The entry holding all of `[address, address + len)` (`len` 0 counts
    /// as 1): its start and value.
    pub fn find(&self, address: u64, len: u64) -> Option<(u64, &T)> {
        let end = address.checked_add(len.max(1))?;
        let (&start, (e, v)) = self.at.range(..=address).next_back()?;
        (end <= *e).then_some((start, v))
    }

    /// Whether any entry meets `[start, end)`.
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        self.at.range(..end).next_back().is_some_and(|(_, (e, _))| *e > start)
    }

    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

/// A set of ranges that may overlap as added; asked only whether they meet
/// a range.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    pub fn add(&mut self, start: u64, end: u64) {
        if start < end {
            self.0.push((start, end));
        }
    }

    pub fn meets(&self, start: u64, end: u64) -> bool {
        self.0.iter().any(|&(a, b)| a < end && start < b)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `VICTORY_NATIVE_GPUMEM_EXCLUDE`: comma-separated `start-end` or
    /// `start+len`, hexadecimal with or without `0x`. Bad items are skipped.
    pub fn parse(text: &str) -> Self {
        let num = |s: &str| {
            let s = s.trim();
            u64::from_str_radix(s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s), 16).ok()
        };
        let mut r = Self::new();
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((a, b)) = item.split_once('+') {
                if let (Some(a), Some(n)) = (num(a), num(b)) {
                    r.add(a, a.saturating_add(n));
                }
            } else if let Some((a, b)) = item.split_once('-')
                && let (Some(a), Some(b)) = (num(a), num(b))
            {
                r.add(a, b);
            }
        }
        r
    }
}

/// A bump allocator over a reserved address range that is never committed:
/// addresses for resources that live on the card alone.
pub struct Bump {
    next: u64,
    end: u64,
}

impl Bump {
    pub const fn new(start: u64, len: u64) -> Self {
        Self { next: start, end: start.saturating_add(len) }
    }

    /// `size` bytes aligned to `align` (a power of two; at least 64 KiB is
    /// used, so no two resources share a page).
    pub fn take(&mut self, size: u64, align: u64) -> Option<u64> {
        let align = align.max(1 << 16);
        if !align.is_power_of_two() || size == 0 {
            return None;
        }
        let at = self.next.checked_next_multiple_of(align)?;
        let end = at.checked_add(size)?.checked_next_multiple_of(1 << 16)?;
        if end > self.end {
            return None;
        }
        self.next = end;
        Some(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_finds_only_whole_ranges() {
        let mut t = Table::new();
        assert!(t.insert(0x1000, 0x2000, 'a'));
        assert!(t.insert(0x3000, 0x5000, 'b'));
        assert!(!t.insert(0x1800, 0x3800, 'c'), "overlap refused");
        assert!(!t.insert(0x6000, 0x6000, 'c'), "empty refused");
        assert!(t.insert(0x2000, 0x3000, 'd'), "touching is fine");
        assert_eq!(t.find(0x1000, 0x1000), Some((0x1000, &'a')));
        assert_eq!(t.find(0x1fff, 0), Some((0x1000, &'a')));
        assert_eq!(t.find(0x1fff, 2), None, "straddles two entries");
        assert_eq!(t.find(0x4000, 0x1000), Some((0x3000, &'b')));
        assert_eq!(t.find(0x4000, 0x1001), None);
        assert_eq!(t.find(0x500, 1), None);
        assert_eq!(t.find(u64::MAX, 1), None);
        assert_eq!(t.remove(0x3000), Some('b'));
        assert_eq!(t.find(0x4000, 1), None);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn exclusions_parse_and_meet() {
        let r = Ranges::parse("0x1000-0x2000, 3000+100 ,junk, 0x10-zz");
        assert_eq!(r, Ranges(vec![(0x1000, 0x2000), (0x3000, 0x3100)]));
        assert!(r.meets(0x1fff, 0x2001));
        assert!(!r.meets(0x2000, 0x3000));
        assert!(r.meets(0x30ff, 0x3100));
        assert!(Ranges::parse("").is_empty());
    }

    #[test]
    fn bump_aligns_and_ends() {
        let mut b = Bump::new(0x10_0000, 0x40_0000);
        assert_eq!(b.take(0x100, 0x1000), Some(0x10_0000));
        assert_eq!(b.take(0x100, 0x1000), Some(0x11_0000), "own page");
        assert_eq!(b.take(0x10_0000, 0x40_0000), Some(0x40_0000), "4 MiB alignment");
        assert_eq!(b.take(1, 1), None, "full");
        assert_eq!(b.take(0, 1), None);
    }
}
