//! The card's memory budget (`VICTORY_VRAM_BUDGET=1`).
//!
//! The layer makes every card object committed (one heap each) and never
//! calls `MakeResident` / `Evict`: when the title's world outgrows the card,
//! the system demotes our objects to the PC's memory behind our back and,
//! on small cards, the driver can lose the device. With this switch on:
//!
//! - the budget is read with `IDXGIAdapter3::QueryVideoMemoryInfo`, again
//!   at once when the system says it changed (the budget-change
//!   notification) and at most every 100 ms otherwise;
//! - every 5 s a memory line says use against budget and what was let go;
//! - near the budget the sweeps (`crate::resource::let_go_idle`,
//!   `crate::shadow::let_go_idle`) run more often and take host copies and
//!   cached textures that nothing used in the last N frames
//!   (`VICTORY_VRAM_IDLE_FRAMES`, 8) sooner. They never take what a list
//!   or a descriptor slot still holds, and what they let go is dropped only
//!   once every running submission has finished (`crate::host::retire`):
//!   nothing in flight is touched.
//!
//! Off (the default) the sweeps keep their own timings and nothing here is
//! read, except the eviction counters.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether the budget policy is on (`VICTORY_VRAM_BUDGET=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VRAM_BUDGET").is_some_and(|v| v == "1"))
}

/// The policy's knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    /// Use at or above this share of the budget (percent) is "near".
    pub near_pct: u64,
    /// At or above this share it is "over": the policy acts hardest.
    pub over_pct: u64,
    /// N: frames a cached object must have gone unused before it may go.
    pub idle_frames: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { near_pct: 85, over_pct: 95, idle_frames: 8 }
    }
}

impl Config {
    /// From the environment: `VICTORY_VRAM_NEAR_PCT`, `VICTORY_VRAM_OVER_PCT`,
    /// `VICTORY_VRAM_IDLE_FRAMES`.
    fn from_env() -> Self {
        let get = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let d = Self::default();
        let near = get("VICTORY_VRAM_NEAR_PCT").unwrap_or(d.near_pct).clamp(10, 100);
        let over = get("VICTORY_VRAM_OVER_PCT").unwrap_or(d.over_pct).clamp(near, 200);
        let frames = get("VICTORY_VRAM_IDLE_FRAMES").unwrap_or(d.idle_frames).max(2);
        Self { near_pct: near, over_pct: over, idle_frames: frames }
    }
}

fn config() -> Config {
    static C: OnceLock<Config> = OnceLock::new();
    *C.get_or_init(Config::from_env)
}

/// How close the card is to its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Pressure {
    Calm,
    Near,
    Over,
}

/// The pressure for `used` bytes of a `budget` (no budget: calm).
pub(crate) fn pressure(budget: u64, used: u64, cfg: &Config) -> Pressure {
    if budget == 0 {
        return Pressure::Calm;
    }
    let pct = u128::from(used) * 100 / u128::from(budget);
    if pct >= u128::from(cfg.over_pct) {
        Pressure::Over
    } else if pct >= u128::from(cfg.near_pct) {
        Pressure::Near
    } else {
        Pressure::Calm
    }
}

/// Shortest idle time ever used: a host texture handed out and not yet
/// recorded into a list or slot is protected by idle time alone
/// (`crate::resource::host_resource`), and every caller records it within
/// the title's call.
pub(crate) const FLOOR_MS: u64 = 1_000;

/// What a sweep does at a pressure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// An object may go once unused this long ...
    pub idle_ms: u64,
    /// ... and this many frames.
    pub idle_frames: u64,
    /// The sweeps run at most this often.
    pub sweep_every_ms: u64,
}

/// The sweep plan: calm keeps the sweep's own `default_ms` / `default_frames`
/// and its once-a-second pace; near takes what was unused for N frames (and
/// [`FLOOR_MS`]) twice a second; over takes what was unused for N/2 frames
/// four times a second. Idle times never fall below [`FLOOR_MS`] nor two
/// frames.
pub(crate) fn plan(p: Pressure, cfg: &Config, default_ms: u64, default_frames: u64) -> Plan {
    match p {
        Pressure::Calm => Plan { idle_ms: default_ms, idle_frames: default_frames, sweep_every_ms: 1_000 },
        Pressure::Near => Plan {
            idle_ms: default_ms.min(2 * FLOOR_MS).max(FLOOR_MS),
            idle_frames: cfg.idle_frames.max(2),
            sweep_every_ms: 500,
        },
        Pressure::Over => Plan {
            idle_ms: FLOOR_MS,
            idle_frames: (cfg.idle_frames / 2).max(2),
            sweep_every_ms: 250,
        },
    }
}

/// Whether a cached object may go: unused for the plan's time and frames,
/// and held by nobody but its cache (`holders` beyond the cache's own: lists
/// not yet reset, descriptor slots, a submission comparing it). Anything
/// held is in flight or about to be, and stays.
pub(crate) fn may_evict(plan: &Plan, ms_since: u64, frames_since: u64, holders: u64) -> bool {
    holders == 0 && ms_since >= plan.idle_ms && frames_since >= plan.idle_frames
}

/// The last look: budget, use of the card's own memory, use of the PC's
/// memory shared with the card (bytes), and when (ms, `crate::host::now_ms`).
static LOOK: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Budget-change notifications received, and changes of 64 MB or more seen.
static CHANGES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Let go by the sweeps since the start: objects, bytes; and of them under
/// pressure (near or over).
static EVICTED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Highest use seen, bytes; looks at near and at over.
static PEAK: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A sweep let `count` objects of `bytes` go (counted on or off).
pub(crate) fn evicted(count: u64, bytes: u64) {
    if count == 0 {
        return;
    }
    EVICTED[0].fetch_add(count, Relaxed);
    EVICTED[1].fetch_add(bytes, Relaxed);
    if on() && current() > Pressure::Calm {
        EVICTED[2].fetch_add(count, Relaxed);
        EVICTED[3].fetch_add(bytes, Relaxed);
    }
}

/// The pressure as of the last look (calm when off or never looked).
pub(crate) fn current() -> Pressure {
    if !on() {
        return Pressure::Calm;
    }
    pressure(LOOK[0].load(Relaxed), LOOK[1].load(Relaxed), &config())
}

/// The sweep plan for a sweep whose own defaults are `default_ms` and
/// `default_frames`: those as they are when off.
pub(crate) fn sweep_plan(default_ms: u64, default_frames: u64) -> Plan {
    plan(current(), &config(), default_ms, default_frames)
}

/// The last look, for the device-removed line: budget, use, shared use.
pub(crate) fn last_look() -> [u64; 3] {
    [0, 1, 2].map(|i| LOOK[i].load(Relaxed))
}

/// Reads the budget now and keeps it.
#[cfg(windows)]
fn refresh() {
    let Some(host) = crate::host::host() else { return };
    let Some([budget, used, _, shared]) = host.video_memory() else { return };
    let was = LOOK[0].swap(budget, Relaxed);
    if was != 0 && was.abs_diff(budget) >= 64 << 20 {
        CHANGES[1].fetch_add(1, Relaxed);
    }
    LOOK[1].store(used, Relaxed);
    LOOK[2].store(shared, Relaxed);
    LOOK[3].store(crate::host::now_ms(), Relaxed);
    PEAK[0].fetch_max(used, Relaxed);
    match pressure(budget, used, &config()) {
        Pressure::Calm => {}
        Pressure::Near => {
            PEAK[1].fetch_add(1, Relaxed);
        }
        Pressure::Over => {
            PEAK[2].fetch_add(1, Relaxed);
        }
    }
}

/// Registers for the system's budget-change notification, once: a thread
/// waits for it and reads the budget again at once.
#[cfg(windows)]
fn listen() {
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let Some(host) = crate::host::host() else { return };
        // SAFETY: a plain event, kept for the run.
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else { return };
        let Some(cookie) = host.budget_notification(event) else {
            tracing::warn!("vram: the system refused the budget-change notification; the budget is read every 100 ms instead");
            return;
        };
        let raw = event.0 as usize;
        let spawned = std::thread::Builder::new().name("victory-vram-budget".into()).spawn(move || {
            let event = windows::Win32::Foundation::HANDLE(raw as *mut _);
            loop {
                // SAFETY: the event lives for the run.
                unsafe { WaitForSingleObject(event, INFINITE) };
                CHANGES[0].fetch_add(1, Relaxed);
                let before = LOOK[0].load(Relaxed);
                refresh();
                let after = LOOK[0].load(Relaxed);
                if before != after {
                    tracing::info!(
                        "vram: the system changed the card's budget {} MB -> {} MB (in use {} MB, frame {})",
                        before >> 20,
                        after >> 20,
                        LOOK[1].load(Relaxed) >> 20,
                        crate::frame::current()
                    );
                }
            }
        });
        if spawned.is_err() {
            tracing::warn!("vram: no thread for the budget-change notification");
        }
        tracing::info!(cookie, cfg = ?config(), "vram: budget policy on (VICTORY_VRAM_BUDGET=1)");
    });
}

/// Before a sweep, on the submitting thread: reads the budget (at most
/// every 100 ms) and says the memory line every 5 s. Nothing when off.
#[cfg(windows)]
pub(crate) fn tick() {
    if !on() {
        return;
    }
    listen();
    let now = crate::host::now_ms();
    if now >= LOOK[3].load(Relaxed) + 100 {
        refresh();
    }
    static LINE: AtomicU64 = AtomicU64::new(0);
    static LAST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let last = LINE.load(Relaxed);
    if now < last + 5_000 || LINE.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let [budget, used, shared] = last_look();
    let all = [EVICTED[0].load(Relaxed), EVICTED[1].load(Relaxed)];
    let before = [0, 1].map(|i| LAST[i].swap(all[i], Relaxed));
    tracing::info!(
        "vram: used {} / budget {} MB ({}%, {:?}), shared {} MB, peak {} MB; evictions last 5 s {} ({} MB), since start {} ({} MB), {} under pressure; looks near {} over {}; budget notifications {}, changes {}",
        used >> 20,
        budget >> 20,
        if budget == 0 { 0 } else { u128::from(used) * 100 / u128::from(budget) },
        current(),
        shared >> 20,
        PEAK[0].load(Relaxed) >> 20,
        all[0] - before[0],
        (all[1] - before[1]) >> 20,
        all[0],
        all[1] >> 20,
        EVICTED[2].load(Relaxed),
        PEAK[1].load(Relaxed),
        PEAK[2].load(Relaxed),
        CHANGES[0].load(Relaxed),
        CHANGES[1].load(Relaxed),
    );
}

/// What most likely removed the device, from its reason (an HRESULT),
/// whether DRED saw a page fault, and the card's use against its budget at
/// the last look.
pub(crate) fn removal_cause(reason: u32, page_fault: bool, budget: u64, used: u64) -> &'static str {
    let over = budget != 0 && used >= budget;
    match reason {
        0x8007_000E => "out of memory (E_OUTOFMEMORY)",
        _ if page_fault && over => "page fault with the card over its budget: likely an object demoted or freed under pressure",
        _ if page_fault => "page fault: a shader or copy touched memory that is not there",
        0x887A_0006 if over => "GPU hang (DEVICE_HUNG / TDR) with the card over its budget: likely paging",
        0x887A_0006 => "GPU hang (DEVICE_HUNG / TDR): work ran too long or never finished",
        0x887A_0007 => "device reset (DEVICE_RESET): badly formed commands",
        0x887A_0020 => "driver internal error (DRIVER_INTERNAL_ERROR)",
        0x887A_0001 => "invalid call (INVALID_CALL)",
        _ if over => "device removed with the card over its budget",
        _ => "device removed, cause not named by the reason",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn pressure_tiers() {
        let c = Config::default();
        assert_eq!(pressure(12 * GIB, 0, &c), Pressure::Calm);
        assert_eq!(pressure(100, 84, &c), Pressure::Calm);
        assert_eq!(pressure(100, 85, &c), Pressure::Near);
        assert_eq!(pressure(100, 94, &c), Pressure::Near);
        assert_eq!(pressure(100, 95, &c), Pressure::Over);
        assert_eq!(pressure(6 * GIB, 7 * GIB, &c), Pressure::Over);
        assert_eq!(pressure(0, 5 * GIB, &c), Pressure::Calm);
        // No overflow at huge numbers.
        assert_eq!(pressure(u64::MAX, u64::MAX, &c), Pressure::Over);
    }

    #[test]
    fn calm_keeps_the_sweeps_own_timing() {
        let c = Config::default();
        let p = plan(Pressure::Calm, &c, 5_000, 4);
        assert_eq!(p, Plan { idle_ms: 5_000, idle_frames: 4, sweep_every_ms: 1_000 });
    }

    #[test]
    fn pressure_tightens_but_never_below_the_floor() {
        let c = Config { idle_frames: 8, ..Config::default() };
        let near = plan(Pressure::Near, &c, 5_000, 4);
        assert_eq!(near.idle_frames, 8);
        assert_eq!(near.idle_ms, 2_000);
        assert_eq!(near.sweep_every_ms, 500);
        let over = plan(Pressure::Over, &c, 5_000, 4);
        assert_eq!(over.idle_frames, 4);
        assert_eq!(over.idle_ms, FLOOR_MS);
        assert_eq!(over.sweep_every_ms, 250);
        // A sweep with a short default of its own is not lowered below
        // the floor, and tiny N stays at two frames.
        let tiny = Config { idle_frames: 2, ..Config::default() };
        assert_eq!(plan(Pressure::Near, &tiny, 0, 1).idle_ms, FLOOR_MS);
        assert_eq!(plan(Pressure::Over, &tiny, 0, 1).idle_frames, 2);
        for p in [Pressure::Near, Pressure::Over] {
            let x = plan(p, &tiny, 0, 0);
            assert!(x.idle_ms >= FLOOR_MS && x.idle_frames >= 2);
        }
    }

    #[test]
    fn never_evicts_what_is_held() {
        let c = Config::default();
        let over = plan(Pressure::Over, &c, 5_000, 4);
        // Idle for ages but still held by a list or slot: stays.
        assert!(!may_evict(&over, u64::MAX, u64::MAX, 1));
        assert!(!may_evict(&over, u64::MAX, u64::MAX, 3));
        // Unheld but used within N frames or the floor: stays.
        assert!(!may_evict(&over, FLOOR_MS, over.idle_frames - 1, 0));
        assert!(!may_evict(&over, FLOOR_MS - 1, 100, 0));
        // Unheld and idle: goes.
        assert!(may_evict(&over, FLOOR_MS, over.idle_frames, 0));
        // The same object at calm waits for the sweep's own time.
        let calm = plan(Pressure::Calm, &c, 5_000, 4);
        assert!(!may_evict(&calm, 2_000, 10, 0));
        assert!(may_evict(&calm, 5_000, 4, 0));
    }

    #[test]
    fn removal_causes() {
        assert!(removal_cause(0x8007_000E, false, 0, 0).contains("out of memory"));
        assert!(removal_cause(0x887A_0006, false, 10, 11).contains("over its budget"));
        assert!(removal_cause(0x887A_0006, false, 10, 5).starts_with("GPU hang"));
        assert!(removal_cause(0x887A_0005, true, 10, 5).starts_with("page fault:"));
        assert!(removal_cause(0x887A_0005, true, 10, 10).contains("demoted"));
        assert!(removal_cause(0x887A_0020, false, 0, 0).contains("DRIVER_INTERNAL"));
        assert!(removal_cause(0x887A_0005, false, 0, 0).contains("not named"));
    }
}
