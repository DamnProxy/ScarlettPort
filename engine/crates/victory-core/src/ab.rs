//! In-session A/B: boot once, then cycle variants of live switches
//! ([`crate::live`]) in timed windows and measure each window.
//!
//! - `VICTORY_AB="A:;B:COPY_NOQUERY=1;C:COPY_NOQUERY=1,CB_RUNS=1"`: the
//!   variants, in order (`A:` is the boot values). A bare name means `=1`.
//! - `VICTORY_AB_WINDOW_S` (20): seconds per window; the first
//!   `VICTORY_AB_SETTLE_S` (1) of each are not measured.
//! - `VICTORY_AB_WARMUP_S` (30): seconds in the world before the first window.
//! - `VICTORY_AB_CYCLES` (0, no end): stop after this many A..Z cycles.
//!
//! Windows start once the title is in the world (fast boot's detector, or
//! 1,500+ draws a frame for 60 frames). A change is asked of the caller
//! ([`Scheduler::frame`] returns the variant), which settles every flight
//! and applies it ([`apply`]) at the frame boundary. One `ab window` line
//! per window; an `ab summary` table after every full cycle and at the end.
//! The logic here is platform-neutral and tested; `victory-gpu` drives it.

use std::fmt::Write as _;

/// One variant: a name and the live switches it sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub sets: Vec<(String, u64)>,
}

/// Parses a schedule; every name must be live ([`crate::live::check`]).
pub fn parse_schedule(s: &str) -> Result<Vec<Variant>, String> {
    let mut out = Vec::new();
    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, body) = part.split_once(':').unwrap_or((part, ""));
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("variant without a name: {part:?}"));
        }
        let mut sets = Vec::new();
        for kv in body.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, "1"));
            let k = crate::live::short(k).to_string();
            crate::live::check(&k).map_err(|e| format!("variant {name}: {e}"))?;
            let v = crate::live::parse(v).ok_or_else(|| format!("variant {name}: {k}={v} is not a number"))?;
            sets.push((k, v));
        }
        if out.iter().any(|o: &Variant| o.name == name) {
            return Err(format!("variant {name} twice"));
        }
        out.push(Variant { name: name.to_string(), sets });
    }
    if out.len() < 2 {
        return Err("an A/B schedule needs two variants or more".into());
    }
    Ok(out)
}

/// Every switch any variant sets is put back to boot, then `v`'s are set.
pub fn apply(all: &[Variant], v: &Variant) {
    for o in all {
        for (k, _) in &o.sets {
            crate::live::reset(k);
        }
    }
    for (k, val) in &v.sets {
        let _ = crate::live::set(k, *val);
    }
}

/// One second of the frame-rate meter (per-frame averages over it).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Second {
    pub frames: f64,
    pub prep_ms: f64,
    pub gpu_ms: f64,
    pub back_ms: f64,
    pub copies_mb: f64,
    pub dropped: f64,
}

/// One measured window.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub variant: usize,
    pub frames: usize,
    pub fps: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub prep_ms: f64,
    pub gpu_ms: f64,
    pub back_ms: f64,
    pub copies_mb: f64,
    pub dropped: f64,
    pub removed: bool,
}

/// Nearest-rank percentile of sorted `v` (`q` in 0..=100).
pub fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((q / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Median (mean of the middle two for an even count).
pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    match s.len() {
        0 => 0.0,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

/// Closes a window from its frame times and meter seconds.
pub fn close(variant: usize, mut ms: Vec<f64>, secs: &[Second], measured_s: f64, removed: bool) -> Window {
    ms.sort_by(f64::total_cmp);
    let f: f64 = secs.iter().map(|s| s.frames).sum();
    let avg = |g: fn(&Second) -> f64| if f > 0.0 { secs.iter().map(|s| g(s) * s.frames).sum::<f64>() / f } else { 0.0 };
    Window {
        variant,
        frames: ms.len(),
        fps: if measured_s > 0.0 { ms.len() as f64 / measured_s } else { 0.0 },
        p50: percentile(&ms, 50.0),
        p95: percentile(&ms, 95.0),
        p99: percentile(&ms, 99.0),
        prep_ms: avg(|s| s.prep_ms),
        gpu_ms: avg(|s| s.gpu_ms),
        back_ms: avg(|s| s.back_ms),
        copies_mb: avg(|s| s.copies_mb),
        dropped: avg(|s| s.dropped),
        removed,
    }
}

/// The `ab window` line.
pub fn window_line(v: &[Variant], w: &Window, index: usize) -> String {
    format!(
        "ab window {index} variant={} frames={} fps={:.1} p50={:.2} p95={:.2} p99={:.2} prep={:.2} gpu={:.2} copyback={:.2} copiesMB={:.1} dropped={:.1} device-removed={}",
        v[w.variant].name,
        w.frames,
        w.fps,
        w.p50,
        w.p95,
        w.p99,
        w.prep_ms,
        w.gpu_ms,
        w.back_ms,
        w.copies_mb,
        w.dropped,
        u8::from(w.removed)
    )
}

/// Per variant over its windows: median of window values, spread (max-min).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub variant: usize,
    pub windows: usize,
    pub fps: f64,
    pub fps_spread: f64,
    pub p50: f64,
    pub p50_spread: f64,
    pub p95: f64,
    pub p99: f64,
    pub prep_ms: f64,
    pub gpu_ms: f64,
    pub back_ms: f64,
    pub copies_mb: f64,
    pub dropped: f64,
    pub removed: usize,
}

pub fn aggregate(n_variants: usize, windows: &[Window]) -> Vec<Row> {
    (0..n_variants)
        .map(|i| {
            let ws: Vec<&Window> = windows.iter().filter(|w| w.variant == i).collect();
            let col = |g: fn(&Window) -> f64| ws.iter().map(|w| g(w)).collect::<Vec<_>>();
            let spread = |v: &[f64]| {
                v.iter().copied().fold(f64::NEG_INFINITY, f64::max) - v.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let fps = col(|w| w.fps);
            let p50 = col(|w| w.p50);
            Row {
                variant: i,
                windows: ws.len(),
                fps: median(&fps),
                fps_spread: if ws.is_empty() { 0.0 } else { spread(&fps) },
                p50: median(&p50),
                p50_spread: if ws.is_empty() { 0.0 } else { spread(&p50) },
                p95: median(&col(|w| w.p95)),
                p99: median(&col(|w| w.p99)),
                prep_ms: median(&col(|w| w.prep_ms)),
                gpu_ms: median(&col(|w| w.gpu_ms)),
                back_ms: median(&col(|w| w.back_ms)),
                copies_mb: median(&col(|w| w.copies_mb)),
                dropped: median(&col(|w| w.dropped)),
                removed: ws.iter().filter(|w| w.removed).count(),
            }
        })
        .collect()
}

/// The summary table (one line per variant, after a header).
pub fn summary(v: &[Variant], windows: &[Window]) -> String {
    let mut s = String::from(
        "ab summary: variant windows fps(spread) p50ms(spread) p95 p99 prep gpu copyback copiesMB dropped removed | switches\n",
    );
    for r in aggregate(v.len(), windows) {
        let sets: Vec<String> = v[r.variant].sets.iter().map(|(k, x)| format!("{k}={x}")).collect();
        let _ = writeln!(
            s,
            "ab summary: {} {} {:.1}(±{:.1}) {:.2}(±{:.2}) {:.2} {:.2} {:.2} {:.2} {:.2} {:.1} {:.1} {} | {}",
            v[r.variant].name,
            r.windows,
            r.fps,
            r.fps_spread / 2.0,
            r.p50,
            r.p50_spread / 2.0,
            r.p95,
            r.p99,
            r.prep_ms,
            r.gpu_ms,
            r.back_ms,
            r.copies_mb,
            r.dropped,
            r.removed,
            if sets.is_empty() { "boot".into() } else { sets.join(",") }
        );
    }
    s
}

#[derive(Debug)]
enum Phase {
    /// Not in the world yet.
    Waiting,
    /// In the world, variant 0 applied, until `until` ns.
    Warmup { until: u64 },
    /// Measuring `variant` from `start`; frames before `measure_from` skipped.
    Window { variant: usize, start: u64, measure_from: u64 },
    Done,
}

/// What the caller does at this frame boundary.
#[derive(Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Settle every flight, then [`apply`] this variant.
    Apply(usize),
    /// A window closed (its line), maybe followed by a summary.
    Closed { line: String, summary: Option<String>, next: Option<usize> },
}

/// The timing configuration, in ns.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub window: u64,
    pub settle: u64,
    pub warmup: u64,
    pub cycles: u64,
}

impl Timing {
    pub fn from_env() -> Self {
        let num = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(d);
        let ns = |s: f64| (s.max(0.0) * 1e9) as u64;
        let window = ns(num("VICTORY_AB_WINDOW_S", 20.0).max(2.0));
        Timing {
            window,
            settle: ns(num("VICTORY_AB_SETTLE_S", 1.0)).min(window / 2),
            warmup: ns(num("VICTORY_AB_WARMUP_S", 30.0)),
            cycles: num("VICTORY_AB_CYCLES", 0.0) as u64,
        }
    }
}

pub struct Scheduler {
    pub variants: Vec<Variant>,
    t: Timing,
    phase: Phase,
    ms: Vec<f64>,
    secs: Vec<Second>,
    last_frame: Option<u64>,
    pub windows: Vec<Window>,
    removed: bool,
}

impl Scheduler {
    pub fn new(variants: Vec<Variant>, t: Timing) -> Self {
        Scheduler { variants, t, phase: Phase::Waiting, ms: Vec::new(), secs: Vec::new(), last_frame: None, windows: Vec::new(), removed: false }
    }

    pub fn done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// A frame was presented at `now` ns; `in_world` from the detector.
    pub fn frame(&mut self, now: u64, in_world: bool) -> Step {
        let dt = self.last_frame.replace(now).map(|l| now.saturating_sub(l) as f64 / 1e6);
        match self.phase {
            Phase::Waiting if in_world => {
                self.phase = Phase::Warmup { until: now + self.t.warmup };
                Step::Apply(0)
            }
            Phase::Waiting | Phase::Done => Step::Nothing,
            Phase::Warmup { until } => {
                if now >= until {
                    self.open(0, now);
                }
                Step::Nothing
            }
            Phase::Window { variant, start, measure_from } => {
                if now >= measure_from && let Some(dt) = dt {
                    self.ms.push(dt);
                }
                if now < start + self.t.window && !self.removed {
                    return Step::Nothing;
                }
                let measured = now.saturating_sub(measure_from) as f64 / 1e9;
                let w = close(variant, std::mem::take(&mut self.ms), &std::mem::take(&mut self.secs), measured, self.removed);
                self.windows.push(w);
                let line = window_line(&self.variants, self.windows.last().expect("pushed"), self.windows.len() - 1);
                let n = self.variants.len();
                let cycle_end = variant + 1 == n;
                let cycles_done = self.windows.len() as u64 / n as u64;
                let stop = self.removed || (cycle_end && self.t.cycles > 0 && cycles_done >= self.t.cycles);
                let summary = (cycle_end || stop).then(|| summary(&self.variants, &self.windows));
                if stop {
                    self.phase = Phase::Done;
                    return Step::Closed { line, summary, next: (variant != 0 && !self.removed).then_some(0) };
                }
                let next = (variant + 1) % n;
                self.open(next, now);
                Step::Closed { line, summary, next: Some(next) }
            }
        }
    }

    fn open(&mut self, variant: usize, now: u64) {
        self.ms.clear();
        self.secs.clear();
        self.phase = Phase::Window { variant, start: now, measure_from: now + self.t.settle };
    }

    /// A meter second ending at `now`: kept if it lies wholly in the
    /// measured part of the current window.
    pub fn second(&mut self, now: u64, s: Second) {
        if let Phase::Window { measure_from, .. } = self.phase
            && s.frames > 0.0
            && now.saturating_sub(1_000_000_000) >= measure_from
        {
            self.secs.push(s);
        }
    }

    /// The host device was removed: the window closes at the next frame
    /// (if one comes) marked, and the schedule ends.
    pub fn device_removed(&mut self) {
        self.removed = true;
    }

    /// The summary so far (at exit).
    pub fn summary(&self) -> String {
        summary(&self.variants, &self.windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn sched(window_s: u64, warmup_s: u64, cycles: u64) -> Scheduler {
        let v = parse_schedule("A:;B:COPY_NOQUERY=1;C:COPY_NOQUERY=1,CB_RUNS").unwrap();
        Scheduler::new(v, Timing { window: window_s * S, settle: S, warmup: warmup_s * S, cycles })
    }

    #[test]
    fn parses_and_refuses() {
        let v = parse_schedule("A:;B:VICTORY_COPY_NOQUERY=1;C:COPY_NOQUERY=1,CB_RUNS").unwrap();
        assert_eq!(v.len(), 3);
        assert!(v[0].sets.is_empty());
        assert_eq!(v[1].sets, vec![("COPY_NOQUERY".into(), 1)]);
        assert_eq!(v[2].sets, vec![("COPY_NOQUERY".into(), 1), ("CB_RUNS".into(), 1)]);
        assert!(parse_schedule("A:;B:REMAP_LAZY=1").unwrap_err().contains("boot-only"));
        assert!(parse_schedule("A:;B:NOPE=1").unwrap_err().contains("not a known"));
        assert!(parse_schedule("A:").is_err());
        assert!(parse_schedule("A:;A:CB_RUNS").is_err());
    }

    #[test]
    fn percentiles_and_median() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 95.0), 95.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    }

    /// Runs `secs` seconds at `fps`, returning the steps that were not Nothing.
    fn run(s: &mut Scheduler, from: u64, secs: u64, fps: u64, in_world: bool) -> (u64, Vec<Step>) {
        let mut out = Vec::new();
        let dt = S / fps;
        let mut t = from;
        while t < from + secs * S {
            t += dt;
            let st = s.frame(t, in_world);
            if st != Step::Nothing {
                out.push(st);
            }
        }
        (t, out)
    }

    #[test]
    fn waits_for_world_warms_up_then_cycles() {
        let mut s = sched(10, 5, 2);
        let (t, steps) = run(&mut s, 0, 3, 50, false);
        assert!(steps.is_empty());
        let (t, steps) = run(&mut s, t, 5, 50, true);
        assert_eq!(steps, vec![Step::Apply(0)]);
        // 2 cycles of 3 windows of 10 s.
        let (_, steps) = run(&mut s, t, 70, 50, true);
        let nexts: Vec<Option<usize>> = steps
            .iter()
            .map(|st| match st {
                Step::Closed { next, .. } => *next,
                _ => panic!("{st:?}"),
            })
            .collect();
        // The last goes back to A (boot values) and the schedule ends.
        assert_eq!(nexts, vec![Some(1), Some(2), Some(0), Some(1), Some(2), Some(0)]);
        assert!(s.done());
        assert_eq!(s.windows.len(), 6);
        let order: Vec<usize> = s.windows.iter().map(|w| w.variant).collect();
        assert_eq!(order, vec![0, 1, 2, 0, 1, 2]);
        // 9 measured seconds at 50 fps (the settle second skipped).
        for w in &s.windows {
            assert!((w.frames as i64 - 450).abs() <= 2, "{w:?}");
            assert!((w.fps - 50.0).abs() < 0.5, "{w:?}");
            assert!((w.p50 - 20.0).abs() < 0.01);
        }
        let summaries = steps.iter().filter(|st| matches!(st, Step::Closed { summary: Some(_), .. })).count();
        assert_eq!(summaries, 2);
    }

    #[test]
    fn device_removed_ends_and_marks() {
        let mut s = sched(10, 0, 0);
        let (t, _) = run(&mut s, 0, 1, 50, true);
        let (t, _) = run(&mut s, t, 12, 50, true);
        s.device_removed();
        let st = s.frame(t + S / 50, true);
        match st {
            Step::Closed { line, summary, next } => {
                assert!(line.contains("device-removed=1"), "{line}");
                assert!(summary.is_some());
                assert_eq!(next, None);
            }
            _ => panic!("{st:?}"),
        }
        assert!(s.done());
    }

    #[test]
    fn aggregates_median_of_windows_and_spread() {
        let w = |variant, fps: f64, p50: f64| Window {
            variant,
            frames: 100,
            fps,
            p50,
            p95: p50 * 2.0,
            p99: p50 * 3.0,
            prep_ms: 1.0,
            gpu_ms: 2.0,
            back_ms: 3.0,
            copies_mb: 4.0,
            dropped: 0.0,
            removed: false,
        };
        let ws = vec![w(0, 30.0, 33.0), w(1, 40.0, 25.0), w(0, 34.0, 29.0), w(1, 42.0, 24.0), w(0, 31.0, 32.0)];
        let rows = aggregate(2, &ws);
        assert_eq!(rows[0].windows, 3);
        assert_eq!(rows[0].fps, 31.0);
        assert_eq!(rows[0].fps_spread, 4.0);
        assert_eq!(rows[0].p50, 32.0);
        assert_eq!(rows[1].fps, 41.0);
        assert_eq!(rows[1].p50_spread, 1.0);
        let v = parse_schedule("A:;B:CB_RUNS=1").unwrap();
        let s = summary(&v, &ws);
        assert_eq!(s.lines().count(), 3);
        assert!(s.contains("ab summary: B 2 41.0(±1.0)"), "{s}");
        assert!(s.contains("| CB_RUNS=1"));
    }

    #[test]
    fn meter_seconds_weighted_by_frames() {
        let secs = [
            Second { frames: 10.0, prep_ms: 1.0, gpu_ms: 0.0, back_ms: 0.0, copies_mb: 2.0, dropped: 0.0 },
            Second { frames: 30.0, prep_ms: 5.0, gpu_ms: 0.0, back_ms: 0.0, copies_mb: 2.0, dropped: 1.0 },
        ];
        let w = close(0, vec![10.0, 20.0], &secs, 1.0, false);
        assert_eq!(w.prep_ms, 4.0);
        assert_eq!(w.copies_mb, 2.0);
        assert_eq!(w.dropped, 0.75);
    }

    #[test]
    fn live_registry_set_reset_and_refuse() {
        static SW: crate::live::Switch = crate::live::Switch::new("CB_WIDE_MERGE", 0);
        static HOIST: crate::live::Switch = crate::live::Switch::new("HOIST_CLAMPS", 1);
        // Set before first read: taken at the first read; boot stays the default.
        crate::live::set("VICTORY_HOIST_CLAMPS", 0).unwrap();
        assert_eq!(HOIST.get(), 0);
        assert_eq!(HOIST.boot(), 1);
        crate::live::reset("HOIST_CLAMPS");
        assert!(HOIST.on());
        assert!(!SW.on());
        crate::live::set("CB_WIDE_MERGE", 1).unwrap();
        assert!(SW.on());
        assert_eq!(crate::live::value("CB_WIDE_MERGE"), Some(1));
        crate::live::reset("CB_WIDE_MERGE");
        assert!(!SW.on());
        assert!(matches!(crate::live::set("REMAP_LAZY", 1), Err(crate::live::Refused::BootOnly(_))));
        assert!(matches!(crate::live::set("XYZ", 1), Err(crate::live::Refused::Unknown(_))));
        assert_eq!(crate::live::parse("on"), Some(1));
        assert_eq!(crate::live::parse(" 250 "), Some(250));
    }

    #[test]
    fn apply_resets_others_first() {
        static NQ: crate::live::Switch = crate::live::Switch::new("MERGE_GAP", 0);
        static RUNS: crate::live::Switch = crate::live::Switch::new("MERGE_RACE", 0);
        let v = parse_schedule("A:;B:MERGE_GAP=8;C:MERGE_RACE").unwrap();
        apply(&v, &v[1]);
        assert_eq!((NQ.get(), RUNS.get()), (8, 0));
        apply(&v, &v[2]);
        assert_eq!((NQ.get(), RUNS.get()), (0, 1));
        apply(&v, &v[0]);
        assert_eq!((NQ.get(), RUNS.get()), (0, 0));
    }
}
