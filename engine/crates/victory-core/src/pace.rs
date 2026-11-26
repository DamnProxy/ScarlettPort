//! Frame pacing and latency switches, the pacer's arithmetic and the
//! per-second pacing statistics. The host side (swap chain, timer waits)
//! lives in `victory-gpu`; the input side in `victory-hle`. Everything here
//! is plain arithmetic on nanoseconds so it can be tested without a GPU.
//!
//! Switches (all off by default; nothing here changes what is drawn):
//! * `VICTORY_PACE=1`: release presents on an even cadence ([`Pacer`]).
//!   `VICTORY_PACE_FPS=30|60` fixes the base rate; otherwise it follows the
//!   title's frame interval.
//! * `VICTORY_PACE_LATENCY=1|2`: waitable swap chain, at most that many
//!   frames queued.
//! * `VICTORY_PACE_TEARING=1`: allow tearing (VRR) when the system supports it.
//! * `VICTORY_PACE_INPUT=1`: sample input when the title reads it (on by
//!   default with `VICTORY_PACE=1`; `VICTORY_PACE_INPUT=0` turns it off).
//! * `VICTORY_PACE_FEEDBACK=1`: hand the title real display times and lock
//!   its frame clock to them (on by default with pacing or latency).
//! * `VICTORY_PACE_STATS=1`: the per-second statistics line alone (on with
//!   any of the above), for the baseline of an A/B run.
//! * `VICTORY_LOW_LATENCY=1`: the low-latency defaults: a waitable swap
//!   chain with one frame queued at most, waited on at the title's frame
//!   start (the origin wait) instead of before the copy to the back buffer,
//!   so the title begins (and reads input) as late as possible; tearing when
//!   no cap is set at boot (sync interval is always 0); late input sampling
//!   and the statistics line. Each `VICTORY_PACE_*` still wins when set.
//! * `VICTORY_FPS_CAP=N` (live): a frame-time limiter at the title's frame
//!   start, N frames a second, high-resolution waitable timer plus a short
//!   spin. `VICTORY_LIMITER=0` (live) turns it off with the cap kept. Both
//!   are A/B live switches ([`crate::live`]).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::Instant;

/// The pacing switches, read once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Switches {
    pub pace: bool,
    /// Fixed base rate, frames per second (None: the title's interval).
    pub fps: Option<u32>,
    /// Maximum frame latency for a waitable swap chain (0: off).
    pub latency: u32,
    pub tearing: bool,
    pub input: bool,
    pub feedback: bool,
    pub stats: bool,
    /// `VICTORY_LOW_LATENCY=1`: the swap chain wait moves to the frame start.
    pub low_latency: bool,
}

impl Switches {
    /// From a variable lookup (`get(name)`), for tests and [`switches`].
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Switches {
        let on = |name: &str| get(name).map(|v| v.trim() == "1");
        let pace = on("VICTORY_PACE").unwrap_or(false);
        let low_latency = on("VICTORY_LOW_LATENCY").unwrap_or(false);
        let capped = get("VICTORY_FPS_CAP").and_then(|v| v.trim().parse::<u32>().ok()).is_some_and(|c| c > 0);
        let fps = get("VICTORY_PACE_FPS")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|f| (1..=1000).contains(f));
        let latency = get("VICTORY_PACE_LATENCY")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map_or(if low_latency { 1 } else { 0 }, |n| n.min(2));
        let tearing = on("VICTORY_PACE_TEARING").unwrap_or(low_latency && !capped);
        let input = on("VICTORY_PACE_INPUT").unwrap_or(pace || low_latency);
        let feedback = on("VICTORY_PACE_FEEDBACK").unwrap_or(pace || latency > 0);
        let stats = on("VICTORY_PACE_STATS").unwrap_or(false)
            || pace
            || latency > 0
            || tearing
            || input
            || feedback
            || low_latency;
        Switches { pace, fps, latency, tearing, input, feedback, stats, low_latency }
    }
}

static FILE: OnceLock<toml::Table> = OnceLock::new();

/// Takes `[pace]` from the config file: `pace`, `fps`, `latency`,
/// `tearing`, `input`, `feedback`, each the file's say for the variable of
/// the same name (`VICTORY_PACE_FPS` for `fps`); the environment wins. Call
/// before [`switches`] is first read; later calls change nothing.
pub fn configure(table: Option<&toml::Table>) {
    if let Some(t) = table {
        let _ = FILE.set(t.clone());
    }
}

/// The file's value for the variable `name`, written as the variable would be.
pub fn file_value(name: &str) -> Option<String> {
    let key = match name {
        "VICTORY_PACE" => "pace",
        "VICTORY_PACE_FPS" => "fps",
        "VICTORY_PACE_LATENCY" => "latency",
        "VICTORY_PACE_TEARING" => "tearing",
        "VICTORY_PACE_INPUT" => "input",
        "VICTORY_PACE_FEEDBACK" => "feedback",
        _ => return None,
    };
    match FILE.get()?.get(key)? {
        toml::Value::Boolean(b) => Some(if *b { "1" } else { "0" }.to_string()),
        toml::Value::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

/// The process's switches at start (the environment over the config file).
pub fn switches() -> &'static Switches {
    static S: OnceLock<Switches> = OnceLock::new();
    S.get_or_init(|| Switches::from_lookup(|n| std::env::var(n).ok().or_else(|| file_value(n))))
}

/// What the overlay can change while the title runs: pacing on or off, its
/// fixed rate (0: the title's interval) and the late input sample. The rest
/// of [`Switches`] shapes the swap chain and needs a restart.
struct Live {
    pace: AtomicBool,
    fps: AtomicU32,
    input: AtomicBool,
}

fn live() -> &'static Live {
    static L: LazyLock<Live> = LazyLock::new(|| {
        let s = switches();
        Live {
            pace: AtomicBool::new(s.pace),
            fps: AtomicU32::new(s.fps.unwrap_or(0)),
            input: AtomicBool::new(s.input),
        }
    });
    &L
}

/// Whether presents are paced now.
pub fn live_pace() -> bool {
    live().pace.load(Ordering::Relaxed)
}

/// The fixed base rate now (None: the title's interval).
pub fn live_fps() -> Option<u32> {
    Some(live().fps.load(Ordering::Relaxed)).filter(|&f| f > 0)
}

/// Whether input is sampled again when the title reads it.
pub fn live_input() -> bool {
    live().input.load(Ordering::Relaxed)
}

/// Sets what [`live_pace`], [`live_fps`] and [`live_input`] answer.
pub fn set_live(pace: bool, fps: Option<u32>, input: bool) {
    let l = live();
    l.pace.store(pace, Ordering::Relaxed);
    l.fps.store(fps.filter(|f| (1..=1000).contains(f)).unwrap_or(0), Ordering::Relaxed);
    l.input.store(input, Ordering::Relaxed);
}

/// `VICTORY_FPS_CAP` (live): the limiter's rate, 0 for none.
pub static FPS_CAP: crate::live::Switch = crate::live::Switch::new("FPS_CAP", 0);
/// `VICTORY_LIMITER` (live, on by default): whether the cap is applied.
pub static LIMITER: crate::live::Switch = crate::live::Switch::new("LIMITER", 1);

/// The limiter's period now (None: no cap, or the limiter is off).
pub fn limiter_period() -> Option<u64> {
    let cap = FPS_CAP.get().min(1000) as u32;
    (cap > 0 && LIMITER.on()).then(|| period_of_fps(cap))
}

/// A frame-time limiter: frame starts on a grid of `period`. A start that
/// is early waits for the next grid point; a little late (under half a
/// period) goes at once and keeps the grid; later than that (a stall, a
/// load) starts a new grid from now, so no burst of short frames follows.
#[derive(Clone, Debug, Default)]
pub struct Limiter {
    next: Option<u64>,
    period: u64,
}

impl Limiter {
    /// A frame wants to start at `now`; when it may (`>= now`).
    pub fn start_at(&mut self, now: u64, period: u64) -> u64 {
        if period != self.period {
            self.period = period;
            self.next = None;
        }
        let at = match self.next {
            Some(t) if now <= t => t,
            _ => now,
        };
        // The grid goes on from the target when on time, from now after a stall.
        let base = match self.next {
            Some(t) if now < t + period / 2 => t,
            _ => at,
        };
        self.next = Some(base + period);
        at
    }
}

/// ns the title's frame starts waited, since last taken: for the swap
/// chain (`VICTORY_LOW_LATENCY`), by the limiter, and the swap chain waits
/// that timed out.
pub static START_WAIT_NS: AtomicU64 = AtomicU64::new(0);
pub static LIMIT_WAIT_NS: AtomicU64 = AtomicU64::new(0);
pub static START_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Nanoseconds on one process-wide monotonic clock (shared by input and present).
pub fn now_ns() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

/// [`now_ns`] of an instant (0 if before the clock started).
pub fn ns_of(at: Instant) -> u64 {
    at.checked_duration_since(*EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// When the host's input was last sampled ([`now_ns`]).
static SAMPLED: AtomicU64 = AtomicU64::new(0);
/// The sample time behind the newest input the title read.
static READ: AtomicU64 = AtomicU64::new(0);
/// The present that last consumed [`READ`] (so latency is counted once per read).
static CONSUMED: AtomicU64 = AtomicU64::new(0);

/// The host's input was sampled at `at` ([`now_ns`]).
pub fn input_sampled(at: u64) {
    SAMPLED.fetch_max(at, Ordering::Relaxed);
}

/// When input was last sampled ([`now_ns`], 0: never).
pub fn last_sampled() -> u64 {
    SAMPLED.load(Ordering::Relaxed)
}

/// The title read input: it now holds what was sampled last.
pub fn input_read() {
    READ.fetch_max(SAMPLED.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// A present at `now`: the input-to-present latency of the newest input
/// the title read, if it read some since the last present.
pub fn input_latency(now: u64) -> Option<u64> {
    let read = READ.load(Ordering::Relaxed);
    if read == 0 || CONSUMED.swap(read, Ordering::Relaxed) == read {
        return None;
    }
    Some(now.saturating_sub(read))
}

/// The base period for a rate.
pub fn period_of_fps(fps: u32) -> u64 {
    1_000_000_000 / u64::from(fps.max(1))
}

/// Releases presents on an even cadence: a grid of `period` (a whole
/// multiple of the base period, enough for the title's recent frame cost),
/// waiting for the next grid point when a frame is early. A frame a little
/// late goes at once and keeps the grid; one late by half a period or more
/// (a stall) starts a new grid from now, so frames queued behind a stall are
/// not let out back to back.
#[derive(Clone, Debug)]
pub struct Pacer {
    base: u64,
    multiple: u64,
    /// The last grid point (target) and the last release.
    target: Option<u64>,
    released: u64,
    /// Smoothed cost of a frame: from one release to the next frame ready.
    cost: f64,
    /// Consecutive frames whose cost fits a smaller multiple.
    fits_lower: u32,
}

/// The most base periods one frame may be paced to.
pub const MAX_MULTIPLE: u64 = 8;
/// Weight of a new frame in the smoothed cost.
const COST_GAIN: f64 = 0.125;
/// Frames that must fit a smaller multiple before it is taken.
const LOWER_AFTER: u32 = 30;

impl Pacer {
    pub fn new(base_ns: u64) -> Pacer {
        Pacer { base: base_ns.max(1), multiple: 1, target: None, released: 0, cost: 0.0, fits_lower: 0 }
    }

    /// A new base period (the title changed its interval); the grid restarts.
    pub fn set_base(&mut self, base_ns: u64) {
        if base_ns.max(1) != self.base {
            self.base = base_ns.max(1);
            self.multiple = 1;
            self.target = None;
            self.fits_lower = 0;
        }
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    /// The paced period now.
    pub fn period(&self) -> u64 {
        self.base * self.multiple
    }

    /// The smallest multiple of the base that holds `cost` with a little room.
    fn multiple_for(&self, cost: f64) -> u64 {
        let m = (cost * 1.05 / self.base as f64).ceil() as u64;
        m.clamp(1, MAX_MULTIPLE)
    }

    /// A frame is ready to present at `ready`; when to release it (`>= ready`).
    pub fn release_at(&mut self, ready: u64) -> u64 {
        let Some(last) = self.target else {
            self.target = Some(ready);
            self.released = ready;
            return ready;
        };
        let frame_cost = ready.saturating_sub(self.released) as f64;
        self.cost = if self.cost == 0.0 { frame_cost } else { self.cost + COST_GAIN * (frame_cost - self.cost) };
        // The multiple goes up at once, down only after it has fit for a while.
        let want = self.multiple_for(self.cost);
        let mut regrid = false;
        if want > self.multiple {
            self.multiple = want;
            self.fits_lower = 0;
            regrid = true;
        } else if want < self.multiple {
            self.fits_lower += 1;
            if self.fits_lower >= LOWER_AFTER {
                self.multiple -= 1;
                self.fits_lower = 0;
                regrid = true;
            }
        } else {
            self.fits_lower = 0;
        }
        let period = self.period();
        // A new period continues from the last release.
        let grid = if regrid { self.released } else { last };
        let target = grid + period;
        let release = if ready <= target {
            self.target = Some(target);
            target
        } else if ready - target < period / 2 {
            // Late by a little: now, on the same grid.
            self.target = Some(target);
            ready
        } else {
            // A stall: a new grid from now.
            self.target = Some(ready);
            ready
        };
        self.released = release;
        release
    }
}

/// The signed distance (ns) from `at` to the nearest tick of a grid that
/// started at 0 with `interval`: positive when `at` is after the tick.
pub fn phase_error(at: u64, interval: u64) -> i64 {
    let interval = interval.max(1);
    let r = at % interval;
    if r * 2 < interval { r as i64 } else { r as i64 - interval as i64 }
}

/// The nudge (ns, to add to a clock's start) that moves its ticks a step
/// toward an event observed `elapsed` after the start: a fraction of the
/// phase error, so one late display moves the clock little.
pub fn phase_nudge(elapsed: u64, interval: u64) -> i64 {
    phase_error(elapsed, interval) / 8
}

/// The `q`-th quantile (0..=1) of sorted samples, nearest rank.
pub fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q.clamp(0.0, 1.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// One second of presents.
#[derive(Default, Debug)]
pub struct Window {
    last: Option<u64>,
    intervals: Vec<u64>,
    latencies: Vec<u64>,
    /// ns waited for the swap chain and by the pacer.
    pub latency_wait: u64,
    pub pace_wait: u64,
    started: Option<u64>,
}

/// What a [`Window`] says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub frames: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    /// Mean of |interval - previous interval|.
    pub jitter_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_wait_ms: f64,
    pub pace_wait_ms: f64,
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

impl Window {
    /// A present at `now`, with the input latency it showed, if any.
    pub fn present(&mut self, now: u64, latency: Option<u64>) {
        if let Some(last) = self.last {
            self.intervals.push(now.saturating_sub(last));
        }
        self.started.get_or_insert(now);
        self.last = Some(now);
        self.latencies.extend(latency);
    }

    /// After a second or more: the summary, and the window starts over.
    pub fn take_if_due(&mut self, now: u64) -> Option<Summary> {
        let started = self.started?;
        if now.saturating_sub(started) < 1_000_000_000 {
            return None;
        }
        let s = self.summary();
        let last = self.last;
        *self = Window { last, started: Some(now), ..Window::default() };
        Some(s)
    }

    pub fn summary(&self) -> Summary {
        let mut sorted = self.intervals.clone();
        sorted.sort_unstable();
        let mut lat = self.latencies.clone();
        lat.sort_unstable();
        let jitter = if self.intervals.len() > 1 {
            let sum: u64 = self.intervals.windows(2).map(|w| w[0].abs_diff(w[1])).sum();
            ms(sum) / (self.intervals.len() - 1) as f64
        } else {
            0.0
        };
        Summary {
            frames: self.intervals.len(),
            p50_ms: ms(quantile(&sorted, 0.50)),
            p95_ms: ms(quantile(&sorted, 0.95)),
            p99_ms: ms(quantile(&sorted, 0.99)),
            jitter_ms: jitter,
            latency_p50_ms: ms(quantile(&lat, 0.50)),
            latency_p95_ms: ms(quantile(&lat, 0.95)),
            latency_wait_ms: ms(self.latency_wait),
            pace_wait_ms: ms(self.pace_wait),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const P30: u64 = 33_333_333;

    /// Runs the pacer over frames that each need `costs[i]` after release.
    fn run(p: &mut Pacer, costs: &[u64]) -> Vec<u64> {
        let mut now = 0;
        let mut out = Vec::new();
        for &c in costs {
            now += c;
            let r = p.release_at(now);
            assert!(r >= now);
            now = r;
            out.push(r);
        }
        out
    }

    fn gaps(r: &[u64]) -> Vec<u64> {
        r.windows(2).map(|w| w[1] - w[0]).collect()
    }

    #[test]
    fn fast_frames_wait_for_the_grid() {
        let mut p = Pacer::new(P30);
        let r = run(&mut p, &[5 * MS; 20]);
        assert!(gaps(&r).iter().all(|&g| g == P30), "{:?}", gaps(&r));
    }

    #[test]
    fn jittery_frames_come_out_even() {
        let mut p = Pacer::new(P30);
        let costs: Vec<u64> = (0..60).map(|i| [4, 28, 12, 30, 9][i % 5] * MS).collect();
        let r = run(&mut p, &costs);
        assert!(gaps(&r).iter().all(|&g| g == P30));
    }

    #[test]
    fn a_stall_starts_a_new_grid_without_a_burst() {
        let mut p = Pacer::new(P30);
        let mut costs = vec![10 * MS; 10];
        costs.push(120 * MS);
        costs.extend([MS; 10]);
        let r = run(&mut p, &costs);
        let g = gaps(&r);
        // After the stall no two presents are closer than a period.
        assert!(g.iter().all(|&x| x >= P30), "{g:?}");
        assert!(g[10..].iter().all(|&x| x == P30));
    }

    #[test]
    fn slightly_late_keeps_the_grid() {
        let mut p = Pacer::new(P30);
        run(&mut p, &[5 * MS; 40]);
        let t0 = p.released;
        // 5 ms late: released at once.
        let late = t0 + P30 + 5 * MS;
        assert_eq!(p.release_at(late), late);
        // The next early frame lands back on the original grid.
        assert_eq!(p.release_at(late + MS), t0 + 2 * P30);
    }

    #[test]
    fn slow_frames_pace_to_a_whole_multiple() {
        let mut p = Pacer::new(P30 / 2); // 60 fps base
        let r = run(&mut p, &[45 * MS; 60]);
        let g = gaps(&r);
        assert_eq!(p.period(), 3 * (P30 / 2));
        assert!(g[10..].iter().all(|&x| x == p.period()), "{g:?}");
    }

    #[test]
    fn the_multiple_comes_down_only_after_frames_fit() {
        let mut p = Pacer::new(P30 / 2);
        run(&mut p, &[40 * MS; 40]);
        assert_eq!(p.multiple, 3);
        run(&mut p, &[5 * MS; 5]);
        assert_eq!(p.multiple, 3, "one fast frame must not drop the multiple");
        run(&mut p, &[5 * MS; 200]);
        assert_eq!(p.multiple, 1);
    }

    #[test]
    fn multiple_is_capped() {
        let mut p = Pacer::new(P30);
        run(&mut p, &[2_000 * MS; 10]);
        assert_eq!(p.multiple, MAX_MULTIPLE);
    }

    #[test]
    fn phase_errors_point_to_the_nearest_tick() {
        assert_eq!(phase_error(0, 100), 0);
        assert_eq!(phase_error(10, 100), 10);
        assert_eq!(phase_error(90, 100), -10);
        assert_eq!(phase_error(250, 100), 50 - 100);
        assert_eq!(phase_nudge(1_016, 1_000), 2);
        // Repeated nudges converge on the event.
        let (mut start, event, interval) = (0i64, 7_400i64, 1_000u64);
        for _ in 0..100 {
            start += phase_nudge((event - start) as u64, interval);
        }
        assert!(phase_error((event - start) as u64, interval).abs() <= 8);
    }

    #[test]
    fn quantiles_and_window() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(quantile(&v, 0.5), 50);
        assert_eq!(quantile(&v, 0.95), 95);
        assert_eq!(quantile(&v, 0.99), 99);
        assert_eq!(quantile(&v, 1.0), 100);
        assert_eq!(quantile(&[], 0.5), 0);
        let mut w = Window::default();
        for (i, t) in [0, 30, 70, 100].iter().enumerate() {
            w.present(t * MS, (i > 0).then_some(20 * MS));
        }
        let s = w.summary();
        assert_eq!(s.frames, 3);
        assert_eq!(s.p50_ms, 30.0);
        assert_eq!(s.p99_ms, 40.0);
        assert!((s.jitter_ms - 10.0).abs() < 1e-9);
        assert_eq!(s.latency_p50_ms, 20.0);
        assert!(w.take_if_due(500 * MS).is_none());
        assert!(w.take_if_due(1_000 * MS).is_some());
        assert_eq!(w.summary().frames, 0);
    }

    #[test]
    fn switches_from_the_environment() {
        let none = Switches::from_lookup(|_| None);
        assert_eq!(none, Switches::default());
        let s = Switches::from_lookup(|n| match n {
            "VICTORY_PACE" => Some("1".into()),
            "VICTORY_PACE_LATENCY" => Some("5".into()),
            "VICTORY_PACE_FPS" => Some("60".into()),
            _ => None,
        });
        assert!(s.pace && s.input && s.feedback && s.stats);
        assert_eq!((s.latency, s.fps), (2, Some(60)));
        let s = Switches::from_lookup(|n| (n == "VICTORY_PACE_STATS").then(|| "1".into()));
        assert!(s.stats && !s.pace && !s.input && !s.feedback);
    }

    #[test]
    fn limiter_holds_an_even_grid_and_resyncs_after_a_stall() {
        let mut l = Limiter::default();
        let mut now = 0;
        let mut starts = Vec::new();
        for c in [5, 20, 3, 30, 10, 5].map(|c| c * MS) {
            let at = l.start_at(now, P30);
            starts.push(at);
            now = at + c;
        }
        assert!(gaps(&starts).iter().all(|&g| g == P30), "{:?}", gaps(&starts));
        // 5 ms late: at once, grid kept.
        let t = *starts.last().unwrap() + P30;
        assert_eq!(l.start_at(t + 5 * MS, P30), t + 5 * MS);
        assert_eq!(l.start_at(t + 6 * MS, P30), t + P30);
        // A stall: a new grid from now, then a full period.
        let s = t + 500 * MS;
        assert_eq!(l.start_at(s, P30), s);
        assert_eq!(l.start_at(s + MS, P30), s + P30);
    }

    #[test]
    fn low_latency_defaults() {
        let s = Switches::from_lookup(|n| (n == "VICTORY_LOW_LATENCY").then(|| "1".into()));
        assert!(s.low_latency && s.tearing && s.input && s.stats && s.feedback);
        assert_eq!(s.latency, 1);
        let s = Switches::from_lookup(|n| match n {
            "VICTORY_LOW_LATENCY" => Some("1".into()),
            "VICTORY_FPS_CAP" => Some("30".into()),
            "VICTORY_PACE_LATENCY" => Some("2".into()),
            _ => None,
        });
        assert!(!s.tearing);
        assert_eq!(s.latency, 2);
    }

    #[test]
    fn latency_counts_each_read_once() {
        input_sampled(10 * MS);
        input_read();
        assert_eq!(input_latency(40 * MS), Some(30 * MS));
        assert_eq!(input_latency(50 * MS), None);
    }
}
