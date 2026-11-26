//! Drives the in-session A/B schedule (`VICTORY_AB`, [`victory_core::ab`])
//! from the present path: frame times per present, the frame-rate meter's
//! per-second sums, device removal; and changes variant at a frame boundary
//! with every flight settled.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use victory_core::ab::{Scheduler, Step, Timing, apply, parse_schedule};

pub(crate) use victory_core::ab::Second;

static STATE: Mutex<Option<Option<Scheduler>>> = Mutex::new(None);
static REMOVED: AtomicBool = AtomicBool::new(false);

fn with<R>(f: impl FnOnce(&mut Scheduler) -> R) -> Option<R> {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| {
        let text = std::env::var("VICTORY_AB").ok().filter(|t| !t.trim().is_empty())?;
        match parse_schedule(&text) {
            Ok(v) => {
                let t = Timing::from_env();
                tracing::warn!(
                    "ab: {} variants, windows of {:.0} s (first {:.1} s not measured), warm-up {:.0} s, cycles {} ({text})",
                    v.len(),
                    t.window as f64 / 1e9,
                    t.settle as f64 / 1e9,
                    t.warmup as f64 / 1e9,
                    if t.cycles == 0 { "until exit".to_string() } else { t.cycles.to_string() }
                );
                // SAFETY: registers a function with the host CRT's exit table.
                unsafe { atexit(at_exit) };
                Some(Scheduler::new(v, t))
            }
            Err(e) => {
                tracing::error!("ab: VICTORY_AB refused, no A/B this run: {e}");
                None
            }
        }
    });
    s.as_mut().map(f)
}

fn now_ns() -> u64 {
    victory_core::pace::now_ns()
}

/// Settles every flight and what the queues hold, then applies variant `i`.
fn change(s: &Scheduler, i: usize) {
    crate::queue::kick_flush();
    crate::host::settle_for_present();
    apply(&s.variants, &s.variants[i]);
    let sets: Vec<String> = s.variants[i].sets.iter().map(|(k, v)| format!("{k}={v}")).collect();
    tracing::warn!("ab: variant {} now ({})", s.variants[i].name, if sets.is_empty() { "boot values".into() } else { sets.join(",") });
}

/// Without fast boot's detector: [`WORLD_DRAWS`] draws or more a frame for
/// [`WORLD_FRAMES`] frames in a row (menus draw 40-100, the world 1,500+:
/// the bench's own rule, by the second).
const WORLD_DRAWS: u64 = 1500;
const WORLD_FRAMES: u32 = 60;

fn world_by_draws() -> bool {
    use std::sync::atomic::{AtomicU32, AtomicU64};
    static LAST: AtomicU64 = AtomicU64::new(0);
    static RUN: AtomicU32 = AtomicU32::new(0);
    let now = crate::draw::drawn();
    let d = now.saturating_sub(LAST.swap(now, Ordering::Relaxed));
    let run = if d >= WORLD_DRAWS { RUN.load(Ordering::Relaxed) + 1 } else { 0 };
    RUN.store(run, Ordering::Relaxed);
    run >= WORLD_FRAMES
}

/// A frame was presented (after its settle).
pub(crate) fn presented() {
    if REMOVED.swap(false, Ordering::Relaxed) {
        with(Scheduler::device_removed);
    }
    let in_world = victory_core::fastboot::in_world() || world_by_draws();
    with(|s| match s.frame(now_ns(), in_world) {
        Step::Nothing => {}
        Step::Apply(i) => change(s, i),
        Step::Closed { line, summary, next } => {
            tracing::warn!("{line}");
            if let Some(t) = summary {
                for l in t.lines() {
                    tracing::warn!("{l}");
                }
            }
            if s.done() {
                tracing::warn!("ab: schedule done");
            }
            if let Some(i) = next {
                change(s, i);
            }
        }
    });
}

/// The frame-rate meter's second just ended.
pub(crate) fn second(s: Second) {
    let now = now_ns();
    with(|sch| sch.second(now, s));
}

/// The host device was removed (taken at the next present, if any).
pub(crate) fn device_removed() {
    REMOVED.store(true, Ordering::Relaxed);
    // No present may come: log what there is now.
    if let Ok(g) = STATE.try_lock()
        && let Some(Some(s)) = g.as_ref()
    {
        for l in s.summary().lines() {
            tracing::warn!("{l} (at device removal)");
        }
    }
}

unsafe extern "C" {
    fn atexit(f: extern "C" fn()) -> i32;
}

/// At process exit: the summary of every window measured.
extern "C" fn at_exit() {
    if let Ok(g) = STATE.try_lock()
        && let Some(Some(s)) = g.as_ref()
    {
        let t = s.summary();
        for l in t.lines() {
            tracing::warn!("{l} (at exit)");
        }
        eprint!("{t}");
    }
}
