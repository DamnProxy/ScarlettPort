//! The in-game settings overlay: F1 opens a menu drawn by the runtime over
//! the game (controls and key bindings, display and pacing, performance),
//! and the game's input stops while it is open.
//!
//! * [`font`], [`gui`]: a bitmap font and a small immediate-mode GUI that
//!   produces textured triangles.
//! * [`menu`]: the pages, as a state machine over plain data.
//! * [`store`]: what is saved where; [`perf`]: frame times and memory.
//! * This module: the one overlay of the process, the input it is given
//!   each present and the effects it has on the rest of the runtime.
//!
//! The window side lives in `victory-gpu` (it samples the keyboard and
//! mouse each present, draws the triangles into the present's copy list and
//! changes the window's mode); `victory-hle` pauses the title's input while
//! [`is_open`].

pub mod font;
pub mod gui;
pub mod menu;
pub mod perf;
#[cfg(test)]
pub mod preview;
pub mod store;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use gui::{Input, Vertex};
use menu::{Ctx, Menu};
use perf::{Perf, SysInfo};
use store::{Display, Model, PaceModel, SaveJob, UpscaleModel};

/// The key that opens and closes the overlay: F1.
pub const TOGGLE_KEY: u8 = 0x70;

static OPEN: AtomicBool = AtomicBool::new(false);
static WHEEL: AtomicI32 = AtomicI32::new(0);

/// Whether the overlay is open: the title's input is paused meanwhile.
pub fn is_open() -> bool {
    OPEN.load(Ordering::Acquire)
}

/// Wheel steps (away from the user positive) the input thread saw while
/// the overlay was open; they scroll its lists.
pub fn add_wheel(steps: i32) {
    WHEEL.fetch_add(steps, Ordering::Relaxed);
}

struct Paths {
    settings: PathBuf,
}

static PATHS: OnceLock<Paths> = OnceLock::new();
static DISPLAY: Mutex<Option<(u64, Display)>> = Mutex::new(None);
static DISPLAY_VERSION: AtomicU64 = AtomicU64::new(0);
static HINTS: OnceLock<crate::hints::Mode> = OnceLock::new();

fn hints_mode() -> crate::hints::Mode {
    HINTS.get().copied().unwrap_or_default()
}

/// `[display]` as it is now with a count of changes (the window side acts
/// on a new one).
pub fn display() -> (u64, Display) {
    let g = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    g.unwrap_or((0, Display::default()))
}

fn set_display(d: Display) {
    let v = DISPLAY_VERSION.fetch_add(1, Ordering::AcqRel) + 1;
    *DISPLAY.lock().unwrap_or_else(|e| e.into_inner()) = Some((v, d));
}

/// Takes `[display]`, `[pace]` and the places of the settings and key map
/// files from the configuration. Call it before `kbm::configure`. Returns
/// what could not be read.
pub fn configure(cfg: &crate::Config) -> Vec<String> {
    let mut problems = Vec::new();
    crate::pace::configure(cfg.pace.as_ref());
    crate::upscale::configure(cfg.upscale.as_ref());
    crate::kbm::set_keymap_file(cfg.data_dir.join("keymap.txt"));
    let settings = cfg.source.clone().unwrap_or_else(|| {
        let p = std::env::var_os("VICTORY_CONFIG")
            .map_or_else(|| PathBuf::from("victory.toml"), PathBuf::from);
        std::path::absolute(&p).unwrap_or(p)
    });
    let _ = PATHS.set(Paths { settings });
    let (mode, p) = crate::hints::Mode::from_sources(cfg.game.as_ref(), &|n| std::env::var(n).ok());
    problems.extend(p);
    let _ = HINTS.set(mode);
    problems.extend(crate::rumble::configure(cfg.game.as_ref()));
    if let Some(t) = &cfg.display {
        let (d, p) = Display::parse(t);
        problems.extend(p);
        set_display(d);
    }
    problems
}

/// What the window side gives the overlay each present.
pub struct Raw<'a> {
    /// A steady clock, nanoseconds.
    pub now_ns: u64,
    /// The window's client size, pixels.
    pub size: (u32, u32),
    /// The pointer in the client area, pixels.
    pub mouse: (f32, f32),
    /// Every key and mouse button (1, 2, 4, 5, 6) held now. Empty while
    /// the window is not the one in front.
    pub down: &'a [u8],
}

struct Overlay {
    menu: Menu,
    perf: Perf,
    sys: SysInfo,
    down: Vec<u8>,
    open: bool,
    /// The left button was down at the last present (for its release).
    prev_left: bool,
    /// When the keyboard hints show (`hints`).
    hints: crate::hints::Tracker,
}

static OVERLAY: Mutex<Option<Overlay>> = Mutex::new(None);
static SAVE_RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);
static SAVE_PENDING: Mutex<Option<SaveJob>> = Mutex::new(None);
static SAVE_LOCK: Mutex<()> = Mutex::new(());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes the files, off the present thread; of several saves asked for
/// in a row the newest wins.
fn save(job: SaveJob) {
    *lock(&SAVE_PENDING) = Some(job);
    let spawned = std::thread::Builder::new()
        .name("victory-save".into())
        .spawn(|| {
            let _one_at_a_time = lock(&SAVE_LOCK);
            if let Some(job) = lock(&SAVE_PENDING).take() {
                let result = store::save(&job);
                if let Err(e) = &result {
                    tracing::warn!("settings not saved: {e}");
                } else {
                    tracing::info!(file = %job.settings_path.display(), "settings saved");
                }
                *lock(&SAVE_RESULT) = Some(result);
            }
        });
    if spawned.is_err() {
        *lock(&SAVE_RESULT) = Some(Err("no thread to save on".into()));
    }
}

impl Overlay {
    fn new() -> Overlay {
        let controls = crate::kbm::current();
        let kbm_on = crate::kbm::settings().enabled;
        let model = Model {
            controls,
            display: display().1,
            pace: PaceModel::current(),
            upscale: UpscaleModel::current(),
        };
        Overlay {
            menu: Menu::new(model, PaceModel::at_start(), kbm_on),
            perf: Perf::default(),
            sys: SysInfo::default(),
            down: Vec::new(),
            open: false,
            prev_left: false,
            hints: crate::hints::Tracker::new(hints_mode()),
        }
    }

    fn apply(&mut self, fx: menu::Effects) {
        if let Some(c) = fx.controls {
            crate::kbm::publish(c);
        }
        if let Some(d) = fx.display {
            set_display(d);
        }
        if let Some(p) = fx.pace {
            crate::pace::set_live(p.pace, p.fps, p.input);
        }
        if let Some(u) = fx.upscale {
            crate::upscale::set_live(u.upscaler, u.framegen, u.sharpness);
        }
        if fx.save {
            match PATHS.get() {
                Some(paths) => save(SaveJob {
                    settings_path: paths.settings.clone(),
                    edits: store::edits(&self.menu.model),
                    keymap_path: crate::kbm::keymap_file(),
                    keymap_text: self.menu.model.controls.keymap_text(),
                }),
                None => *lock(&SAVE_RESULT) = Some(Err("no settings file is configured".into())),
            }
        }
        if fx.close {
            self.set_open(false);
        }
    }

    fn set_open(&mut self, open: bool) {
        if self.open && !open {
            self.menu.cancel_capture();
        }
        self.open = open;
        OPEN.store(open, Ordering::Release);
        tracing::info!(open, "settings overlay");
    }

    fn frame(&mut self, raw: &Raw, sys: Option<SysInfo>, draw: &mut dyn FnMut(&[Vertex])) {
        self.perf.present(raw.now_ns);
        if let Some(s) = sys {
            self.sys = s;
        }
        if let Some(result) = lock(&SAVE_RESULT).take() {
            self.menu.saved(result);
        }
        let pressed: Vec<u8> = raw
            .down
            .iter()
            .copied()
            .filter(|k| !self.down.contains(k))
            .collect();
        let held = |k: u8| raw.down.contains(&k);
        let left = held(0x01);
        let wheel = WHEEL.swap(0, Ordering::Relaxed);
        let mut pressed = pressed;
        if pressed.contains(&TOGGLE_KEY) {
            self.set_open(!self.open);
            pressed.retain(|&k| k != TOGGLE_KEY);
        }
        let input = Input {
            mouse: raw.mouse,
            left_down: left,
            left_pressed: pressed.contains(&0x01),
            left_released: self.prev_left && !left,
            right_pressed: pressed.contains(&0x02),
            wheel: wheel as f32,
            keys_pressed: pressed,
        };
        self.prev_left = left;
        self.down = raw.down.to_vec();
        let get = |n: &str| crate::env::env(n);
        let ctx = Ctx {
            size: (raw.size.0 as f32, raw.size.1 as f32),
            perf: &self.perf,
            sys: &self.sys,
            env: &get,
            now_ms: raw.now_ns / 1_000_000,
        };
        if self.open {
            let fx = self.menu.frame(&ctx, input);
            self.apply(fx);
            draw(&self.menu.draw_list().verts);
        } else {
            let show = self.hints.update(raw.now_ns / 1_000_000, raw.down);
            if show || self.menu.model.display.graph {
                let legend = show.then(|| crate::hints::legend(&self.menu.model.controls));
                self.menu.hud_with(&ctx, legend.as_deref());
                draw(&self.menu.draw_list().verts);
            }
        }
    }
}

/// One present's worth of overlay: reads `raw`, runs the menu (or the hud
/// graph) and hands the triangles to `draw` when there is something to draw.
/// `sys` is new memory figures, when the caller has them.
pub fn frame(raw: &Raw, sys: Option<SysInfo>, mut draw: impl FnMut(&[Vertex])) {
    let mut guard = lock(&OVERLAY);
    guard
        .get_or_insert_with(Overlay::new)
        .frame(raw, sys, &mut draw);
}

/// Whether the caller should read the memory figures this present: they
/// cost a few system calls, so only while something shows them.
pub fn wants_sys() -> bool {
    is_open()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw<'a>(now_ms: u64, down: &'a [u8]) -> Raw<'a> {
        Raw {
            now_ns: now_ms * 1_000_000,
            size: (1280, 720),
            mouse: (10.0, 10.0),
            down,
        }
    }

    // One test: the overlay is the process's, and open/closed is global.
    #[test]
    fn f1_opens_and_closes_it_and_esc_closes_it() {
        let run = |ms: u64, down: &[u8]| {
            let mut n = 0;
            frame(&raw(ms, down), None, |v| n = v.len());
            n
        };
        assert_eq!(run(0, &[]), 0, "closed and no graph: nothing drawn");
        assert!(!is_open());
        // F1 goes down: it opens, and draws that same frame.
        assert!(run(16, &[TOGGLE_KEY]) > 0);
        assert!(is_open());
        // Held, it does not toggle again.
        assert!(run(32, &[TOGGLE_KEY]) > 0);
        assert!(is_open());
        run(48, &[]);
        assert!(is_open());
        // Esc closes (after drawing the frame it was pressed in).
        run(64, &[0x1B]);
        assert!(!is_open());
        assert_eq!(run(80, &[]), 0);
        // F1 again, then F1 closes.
        run(96, &[TOGGLE_KEY]);
        assert!(is_open());
        run(112, &[]);
        run(128, &[TOGGLE_KEY]);
        assert!(!is_open());
        // Wheel steps are taken, not kept for later.
        add_wheel(3);
        run(144, &[]);
        assert_eq!(WHEEL.load(Ordering::Relaxed), 0);
    }
}
