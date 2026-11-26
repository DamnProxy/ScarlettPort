//! The sums behind `VICTORY_GPU_PROFILE`'s report: no host objects, so it
//! is tested on its own (`rustc --test gpuprof/agg.rs`).

use std::collections::HashMap;
use std::fmt::Write as _;

/// One timed list as read back: its timestamps (first where it began, one
/// at each pipeline change, last where it ended) and, per span between two
/// stamps, the pipeline key and the draws and dispatches in it.
pub(crate) struct ListStamps<'a> {
    pub(crate) stamps: &'a [u64],
    pub(crate) keys: &'a [u32],
    pub(crate) draws: &'a [u32],
}

/// Key of the span before a list's first pipeline (copies, barriers).
pub(crate) const PRELUDE: u32 = u32::MAX;
/// Key of what a list recorded after its stamps ran out.
pub(crate) const OVERFLOW: u32 = u32::MAX - 1;

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sum {
    pub(crate) ticks: u64,
    pub(crate) draws: u64,
    pub(crate) groups: u64,
}

#[derive(Default)]
pub(crate) struct Agg {
    pub(crate) by_key: HashMap<u32, Sum>,
    pub(crate) list_ticks: u64,
    pub(crate) lists: u64,
    /// Lists dropped: never ran, or stamps out of order.
    pub(crate) unread: u64,
    pub(crate) frames: u64,
}

/// Ticks between two stamps; 0 when the second is not after the first or
/// the gap is beyond any frame's (`cap` ticks).
pub(crate) fn span(begin: u64, end: u64, cap: u64) -> u64 {
    if begin == 0 || end <= begin { 0 } else { (end - begin).min(cap) }
}

impl Agg {
    /// Adds one list. `cap`: the most ticks one span may count (a second).
    pub(crate) fn add_list(&mut self, l: ListStamps, cap: u64) {
        let n = l.stamps.len();
        if n < 2 || l.keys.len() != n - 1 || l.draws.len() != n - 1 || l.stamps[0] == 0 || l.stamps[n - 1] == 0 {
            self.unread += 1;
            return;
        }
        self.lists += 1;
        self.list_ticks += span(l.stamps[0], l.stamps[n - 1], cap);
        for i in 0..n - 1 {
            let s = self.by_key.entry(l.keys[i]).or_default();
            s.ticks += span(l.stamps[i], l.stamps[i + 1], cap);
            s.draws += u64::from(l.draws[i]);
            s.groups += 1;
        }
    }

    pub(crate) fn frame(&mut self) {
        self.frames += 1;
    }

    /// The `n` keys with most time, most first: (key, sum).
    pub(crate) fn top(&self, n: usize) -> Vec<(u32, Sum)> {
        let mut v: Vec<(u32, Sum)> = self.by_key.iter().map(|(&k, &s)| (k, s)).collect();
        v.sort_by(|a, b| b.1.ticks.cmp(&a.1.ticks).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// The report: per frame, ms and draws of the top `n` pipelines.
    pub(crate) fn report(&self, n: usize, frequency: u64, label: &dyn Fn(u32) -> String) -> String {
        let frames = self.frames.max(1) as f64;
        let ms = |t: u64| t as f64 * 1e3 / frequency.max(1) as f64 / frames;
        let all: u64 = self.by_key.values().map(|s| s.ticks).sum();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "GPU profile over {} frames: title lists {:.2} ms/frame ({:.1} lists/frame, {} unread), pipeline spans {:.2} ms/frame, {} pipelines",
            self.frames,
            ms(self.list_ticks),
            self.lists as f64 / frames,
            self.unread,
            ms(all),
            self.by_key.len()
        );
        for (i, (k, s)) in self.top(n).into_iter().enumerate() {
            let name = match k {
                PRELUDE => "(list start, before any pipeline)".to_owned(),
                OVERFLOW => "(after the list's stamps ran out)".to_owned(),
                k => label(k),
            };
            let _ = writeln!(
                out,
                "  #{:<2} {:>7.3} ms/frame {:>5.1}%  {:>7.1} draws/frame {:>6.1} groups/frame  {}",
                i + 1,
                ms(s.ticks),
                if all == 0 { 0.0 } else { s.ticks as f64 * 100.0 / all as f64 },
                s.draws as f64 / frames,
                s.groups as f64 / frames,
                name
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(stamps: &[u64], keys: &[u32], draws: &[u32]) -> ListStamps<'static> {
        ListStamps {
            stamps: Box::leak(stamps.to_vec().into_boxed_slice()),
            keys: Box::leak(keys.to_vec().into_boxed_slice()),
            draws: Box::leak(draws.to_vec().into_boxed_slice()),
        }
    }

    #[test]
    fn spans_go_to_their_pipeline_and_lists_to_the_total() {
        let mut a = Agg::default();
        a.add_list(list(&[100, 110, 150, 160], &[PRELUDE, 7, 9], &[0, 3, 1]), 1 << 40);
        a.add_list(list(&[200, 240, 241], &[7, 9], &[2, 5]), 1 << 40);
        a.frame();
        assert_eq!(a.lists, 2);
        assert_eq!(a.list_ticks, 60 + 41);
        assert_eq!(a.by_key[&7], Sum { ticks: 40 + 40, draws: 5, groups: 2 });
        assert_eq!(a.by_key[&9], Sum { ticks: 10 + 1, draws: 6, groups: 2 });
        assert_eq!(a.by_key[&PRELUDE], Sum { ticks: 10, draws: 0, groups: 1 });
        let top = a.top(2);
        assert_eq!(top.iter().map(|t| t.0).collect::<Vec<_>>(), vec![7, 9]);
    }

    #[test]
    fn lists_that_never_ran_or_are_malformed_are_not_counted() {
        let mut a = Agg::default();
        a.add_list(list(&[0, 0, 0], &[1, 2], &[1, 1]), 1000);
        a.add_list(list(&[5, 9], &[1, 2], &[1, 1]), 1000);
        a.add_list(list(&[5], &[], &[]), 1000);
        assert_eq!((a.lists, a.unread), (0, 3));
        assert!(a.by_key.is_empty());
    }

    #[test]
    fn backwards_and_huge_spans_are_bounded() {
        assert_eq!(span(10, 5, 100), 0);
        assert_eq!(span(0, 5, 100), 0);
        assert_eq!(span(1, u64::MAX, 100), 100);
        let mut a = Agg::default();
        a.add_list(list(&[10, 8, 20], &[1, 2], &[1, 1]), 1000);
        assert_eq!(a.by_key[&1].ticks, 0);
        assert_eq!(a.by_key[&2].ticks, 12);
    }

    #[test]
    fn report_is_per_frame_and_names_the_top() {
        let mut a = Agg::default();
        // 1 MHz clock: 1 tick = 1 us.
        for _ in 0..4 {
            a.add_list(list(&[1_000, 9_000, 11_000], &[3, 4], &[10, 2]), 1 << 40);
            a.frame();
        }
        let r = a.report(1, 1_000_000, &|k| format!("cs {k:016x}"));
        let lines: Vec<&str> = r.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("title lists 10.00 ms/frame (1.0 lists/frame"), "{r}");
        assert!(lines[1].contains("8.000 ms/frame"), "{r}");
        assert!(lines[1].contains("10.0 draws/frame"), "{r}");
        assert!(lines[1].contains("cs 0000000000000003"), "{r}");
        assert!(lines[1].contains("80.0%"), "{r}");
    }
}
