//! The upscaler and frame-generation switches, and the plumbing between
//! the title's upscale dispatch and the host upscaler that answers it.
//!
//! The title's Series S profile renders below its output size and calls the
//! console FidelityFX upscaler with everything a temporal upscaler needs:
//! colour, depth, motion vectors, the camera jitter, the motion-vector
//! scale, the render and output sizes and the camera planes
//! (`ffxDispatchDescUpscale`). `victory_hle::modules::ffx` takes that call;
//! this module decides what answers it.
//!
//! * `VICTORY_UPSCALER=fsr` (the default on that path): AMD FSR 3.1 through
//!   the PC FidelityFX API DLL (`victory_gpu::fsr`), loaded at run time.
//! * `VICTORY_UPSCALER=dlss`: NVIDIA DLSS Super Resolution from a
//!   user-provided `nvngx_dlss.dll`. shortcut: not wired yet; it falls back
//!   to `fsr` with a warning (see [`Upscaler::effective`]).
//! * `VICTORY_UPSCALER=title`: the title's own TAA upscaler (the
//!   `VICTORY_OWN_UPSCALER` patch; no FidelityFX dispatch happens at all).
//! * `VICTORY_UPSCALER=stretch`: the plain bilinear stretch (the old
//!   stand-in, `victory_gpu::over`).
//! * `VICTORY_UPSCALE_QUALITY=quality|balanced|performance|ultra|native`.
//! * `VICTORY_UPSCALE_SHARPNESS=0..1` (default 0.2, the title's `fsrSharpen`).
//! * `VICTORY_FRAMEGEN=1`: FSR 3 frame generation (any DX12 card, RTX 30
//!   included). Only worth it when the base rate is at least
//!   [`FRAMEGEN_MIN_BASE_FPS`]; below that the gate keeps it off. DLSS frame
//!   generation needs an RTX 40 and is never offered.
//!
//! `[upscale]` in the config file has the same keys (`upscaler`, `quality`,
//! `sharpness`, `framegen`); the environment wins.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{LazyLock, OnceLock};

pub const UPSCALER_ENV: &str = "VICTORY_UPSCALER";
pub const QUALITY_ENV: &str = "VICTORY_UPSCALE_QUALITY";
pub const SHARPNESS_ENV: &str = "VICTORY_UPSCALE_SHARPNESS";
pub const FRAMEGEN_ENV: &str = "VICTORY_FRAMEGEN";
/// The older switch for [`Upscaler::Title`]; still honoured.
pub const OWN_UPSCALER_ENV: &str = "VICTORY_OWN_UPSCALER";

/// Below this base (rendered) rate frame generation is kept off: the
/// interpolated frames are built from inputs too far apart in time, so
/// they smear and the added latency of one base frame is large.
pub const FRAMEGEN_MIN_BASE_FPS: f32 = 15.0;
/// Once off, it comes back on only above this rate (hysteresis).
pub const FRAMEGEN_RESUME_FPS: f32 = 17.0;

/// What makes the output image from the title's render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upscaler {
    Dlss,
    Fsr,
    Title,
    Stretch,
}

impl Upscaler {
    pub const ALL: [Upscaler; 4] = [Upscaler::Fsr, Upscaler::Dlss, Upscaler::Title, Upscaler::Stretch];

    pub fn parse(text: &str) -> Option<Upscaler> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "dlss" => Upscaler::Dlss,
            "fsr" | "fsr3" | "fsr2" => Upscaler::Fsr,
            "title" | "own" | "taa" => Upscaler::Title,
            "stretch" | "bilinear" => Upscaler::Stretch,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Upscaler::Dlss => "dlss",
            Upscaler::Fsr => "fsr",
            Upscaler::Title => "title",
            Upscaler::Stretch => "stretch",
        }
    }

    /// Whether the title's own upscaler must be forced (the
    /// `VICTORY_OWN_UPSCALER` patch). Every other choice needs the title on
    /// its FidelityFX path, which hands us the inputs.
    pub fn wants_own_upscaler(self) -> bool {
        self == Upscaler::Title
    }

    /// What actually answers the dispatch. DLSS is not wired yet, so it is
    /// answered by FSR, which takes the same inputs.
    pub fn effective(self) -> Upscaler {
        match self {
            Upscaler::Dlss => Upscaler::Fsr,
            u => u,
        }
    }

    fn index(self) -> u8 {
        Upscaler::ALL.iter().position(|u| *u == self).unwrap_or(0) as u8
    }

    fn from_index(i: u8) -> Upscaler {
        Upscaler::ALL[usize::from(i).min(Upscaler::ALL.len() - 1)]
    }
}

/// The FidelityFX (and DLSS) quality modes: a per-axis upscale ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    NativeAa,
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
}

impl Quality {
    pub const ALL: [Quality; 5] = [
        Quality::NativeAa,
        Quality::Quality,
        Quality::Balanced,
        Quality::Performance,
        Quality::UltraPerformance,
    ];

    pub fn parse(text: &str) -> Option<Quality> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "native" | "nativeaa" | "dlaa" | "0" => Quality::NativeAa,
            "quality" | "1" => Quality::Quality,
            "balanced" | "2" => Quality::Balanced,
            "performance" | "3" => Quality::Performance,
            "ultra" | "ultraperformance" | "ultra_performance" | "4" => Quality::UltraPerformance,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Quality::NativeAa => "native",
            Quality::Quality => "quality",
            Quality::Balanced => "balanced",
            Quality::Performance => "performance",
            Quality::UltraPerformance => "ultra",
        }
    }

    /// `FfxApiUpscaleQualityMode`.
    pub fn ffx_mode(self) -> u32 {
        Quality::ALL.iter().position(|q| *q == self).unwrap_or(1) as u32
    }

    /// Output size over render size, per axis.
    pub fn ratio(self) -> f32 {
        match self {
            Quality::NativeAa => 1.0,
            Quality::Quality => 1.5,
            Quality::Balanced => 1.7,
            Quality::Performance => 2.0,
            Quality::UltraPerformance => 3.0,
        }
    }

    /// The render scale (`VICTORY_RENDER_SCALE`) this mode stands for.
    pub fn render_scale(self) -> f32 {
        1.0 / self.ratio()
    }

    /// The render size for an output size, as FidelityFX computes it.
    pub fn render_size(self, display: [u32; 2]) -> [u32; 2] {
        let r = self.ratio();
        [
            ((display[0] as f32 / r) as u32).max(1),
            ((display[1] as f32 / r) as u32).max(1),
        ]
    }
}

/// The switches, read once at start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Switches {
    pub upscaler: Upscaler,
    pub quality: Quality,
    pub sharpness: f32,
    pub framegen: bool,
}

impl Default for Switches {
    fn default() -> Switches {
        Switches {
            upscaler: Upscaler::Fsr,
            quality: Quality::Quality,
            sharpness: 0.2,
            framegen: false,
        }
    }
}

fn truthy(v: &str) -> bool {
    !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "off" | "no")
}

impl Switches {
    /// From a lookup of variable names (the environment over the file).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Switches {
        let mut s = Switches::default();
        if get(OWN_UPSCALER_ENV).is_some_and(|v| truthy(&v)) {
            s.upscaler = Upscaler::Title;
        }
        if let Some(v) = get(UPSCALER_ENV) {
            match Upscaler::parse(&v) {
                Some(u) => s.upscaler = u,
                None => tracing::warn!(value = v, "{UPSCALER_ENV} wants dlss, fsr, title or stretch"),
            }
        }
        if let Some(v) = get(QUALITY_ENV) {
            match Quality::parse(&v) {
                Some(q) => s.quality = q,
                None => tracing::warn!(
                    value = v,
                    "{QUALITY_ENV} wants native, quality, balanced, performance or ultra"
                ),
            }
        }
        if let Some(v) = get(SHARPNESS_ENV).and_then(|v| v.trim().parse::<f32>().ok()) {
            s.sharpness = v.clamp(0.0, 1.0);
        }
        if let Some(v) = get(FRAMEGEN_ENV) {
            let t = v.trim().to_ascii_lowercase();
            if t == "dlss" {
                tracing::warn!("DLSS frame generation needs an RTX 40 or newer and is not offered; FSR 3 frame generation is used");
            }
            s.framegen = truthy(&v);
        }
        s
    }
}

static FILE: OnceLock<toml::Table> = OnceLock::new();

/// Takes `[upscale]` from the config file; call before [`switches`].
pub fn configure(table: Option<&toml::Table>) {
    if let Some(t) = table {
        let _ = FILE.set(t.clone());
    }
}

/// The file's value for variable `name`, written as the variable would be.
pub fn file_value(name: &str) -> Option<String> {
    let key = match name {
        UPSCALER_ENV => "upscaler",
        QUALITY_ENV => "quality",
        SHARPNESS_ENV => "sharpness",
        FRAMEGEN_ENV => "framegen",
        _ => return None,
    };
    match FILE.get()?.get(key)? {
        toml::Value::Boolean(b) => Some(if *b { "1" } else { "0" }.to_string()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The process's switches at start.
pub fn switches() -> &'static Switches {
    static S: OnceLock<Switches> = OnceLock::new();
    S.get_or_init(|| Switches::from_lookup(|n| std::env::var(n).ok().or_else(|| file_value(n))))
}

/// What the overlay can change while the title runs.
struct Live {
    upscaler: AtomicU8,
    framegen: AtomicBool,
    /// Sharpness, as f32 bits.
    sharpness: AtomicU32,
    /// A history reset is due (the upscaler was switched).
    reset: AtomicBool,
}

fn live() -> &'static Live {
    static L: LazyLock<Live> = LazyLock::new(|| {
        let s = switches();
        Live {
            upscaler: AtomicU8::new(s.upscaler.index()),
            framegen: AtomicBool::new(s.framegen),
            sharpness: AtomicU32::new(s.sharpness.to_bits()),
            reset: AtomicBool::new(false),
        }
    });
    &L
}

/// The upscaler chosen now. Switching between `fsr`, `dlss` and `stretch`
/// is live; to or from `title` needs a restart (it is a patch of the
/// title's code made at load).
pub fn live_upscaler() -> Upscaler {
    Upscaler::from_index(live().upscaler.load(Ordering::Relaxed))
}

pub fn live_framegen() -> bool {
    live().framegen.load(Ordering::Relaxed)
}

pub fn live_sharpness() -> f32 {
    f32::from_bits(live().sharpness.load(Ordering::Relaxed))
}

/// Sets what the `live_*` functions answer.
pub fn set_live(upscaler: Upscaler, framegen: bool, sharpness: f32) {
    let l = live();
    let before = l.upscaler.swap(upscaler.index(), Ordering::Relaxed);
    if before != upscaler.index() {
        l.reset.store(true, Ordering::Relaxed);
    }
    l.framegen.store(framegen, Ordering::Relaxed);
    l.sharpness.store(sharpness.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

/// Whether a history reset is due, clearing it.
pub fn take_reset() -> bool {
    live().reset.swap(false, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Jitter
// ---------------------------------------------------------------------------

/// The radical inverse of `index` in `base` (the Halton sequence).
pub fn halton(mut index: u32, base: u32) -> f32 {
    let mut f = 1.0f32;
    let mut r = 0.0f32;
    while index > 0 {
        f /= base as f32;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

/// The jitter sequence length FidelityFX uses for a render and output
/// width: `ceil(8 * ratio^2)`.
pub fn jitter_phase_count(render_width: u32, display_width: u32) -> u32 {
    if render_width == 0 {
        return 8;
    }
    let ratio = display_width as f32 / render_width as f32;
    ((8.0 * ratio * ratio).ceil() as u32).max(1)
}

/// The sub-pixel jitter (in render pixels, each axis in -0.5..0.5) of frame
/// `index` in a sequence of `phases`: Halton(2, 3), skipping its zero.
pub fn jitter_offset(index: u32, phases: u32) -> [f32; 2] {
    let i = index % phases.max(1) + 1;
    [halton(i, 2) - 0.5, halton(i, 3) - 0.5]
}

/// A pixel jitter as the offset added to a projection matrix's third
/// column (clip space): x scales by 2/width, y by -2/height.
pub fn jitter_to_clip(jitter: [f32; 2], render: [u32; 2]) -> [f32; 2] {
    [
        2.0 * jitter[0] / render[0].max(1) as f32,
        -2.0 * jitter[1] / render[1].max(1) as f32,
    ]
}

/// Watches the jitter the title passes. A temporal upscaler fed a camera
/// that never jitters only blurs; that is worth one warning.
#[derive(Debug, Default)]
pub struct JitterWatch {
    frames: u32,
    still: u32,
    last: [f32; 2],
}

/// What [`JitterWatch::see`] concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitterVerdict {
    Fine,
    /// The title's jitter is outside one pixel: not in pixels, or garbage.
    OutOfRange,
    /// The jitter has not moved for this many frames.
    Still(u32),
}

impl JitterWatch {
    /// The frames without movement after which the jitter is called still.
    pub const STILL_AFTER: u32 = 16;

    pub fn see(&mut self, jitter: [f32; 2]) -> JitterVerdict {
        self.frames += 1;
        if !jitter[0].is_finite() || !jitter[1].is_finite() || jitter[0].abs() > 1.0 || jitter[1].abs() > 1.0 {
            return JitterVerdict::OutOfRange;
        }
        if jitter == self.last {
            self.still += 1;
        } else {
            self.still = 0;
        }
        self.last = jitter;
        if self.still >= Self::STILL_AFTER {
            JitterVerdict::Still(self.still)
        } else {
            JitterVerdict::Fine
        }
    }
}

// ---------------------------------------------------------------------------
// The dispatch parameters
// ---------------------------------------------------------------------------

/// The numbers of the title's upscale dispatch (everything but resources).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleParams {
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub render: [u32; 2],
    pub upscale: [u32; 2],
    pub sharpening: bool,
    pub sharpness: f32,
    pub frame_ms: f32,
    pub pre_exposure: f32,
    pub reset: bool,
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    pub meters: f32,
    pub flags: u32,
}

/// What the host upscaler is dispatched with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HostParams {
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub render: [u32; 2],
    pub upscale: [u32; 2],
    pub sharpening: bool,
    pub sharpness: f32,
    pub frame_ms: f32,
    pub pre_exposure: f32,
    pub reset: bool,
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    pub meters: f32,
    pub flags: u32,
}

/// Keeps what carries over from one dispatch to the next.
#[derive(Debug, Default)]
pub struct Plumbing {
    last_render: [u32; 2],
    last_upscale: [u32; 2],
    pub watch: JitterWatch,
}

impl Plumbing {
    /// The host dispatch for the title's, with the sizes of the textures we
    /// hold (`colour` the render input, `output` the output):
    /// - sizes of 0 (the API's "use the maximum") and sizes past the
    ///   textures are taken from the textures;
    /// - the history is reset when the output size changes or the user
    ///   switched upscaler, besides when the title asks;
    /// - frame time is kept to 1..=200 ms (a 0 from the title, a hitch);
    /// - a pre-exposure of 0 or less is 1 (the API needs > 0);
    /// - the sharpness is the user's (the overlay's) when sharpening is on;
    /// - camera planes must be positive and differ, else FSR is told 0.1
    ///   and 10000 (it uses them for its depth clip only);
    /// - the jitter and motion scale go through as the title wrote them:
    ///   it wrote them for the same API.
    pub fn host(
        &mut self,
        t: &TitleParams,
        colour: [u32; 2],
        output: [u32; 2],
        sharpness: f32,
        switched: bool,
    ) -> HostParams {
        let fit = |v: u32, max: u32| if v == 0 || v > max { max } else { v };
        let render = [fit(t.render[0], colour[0]), fit(t.render[1], colour[1])];
        let upscale = [fit(t.upscale[0], output[0]), fit(t.upscale[1], output[1])];
        let resized = self.last_upscale != [0, 0] && self.last_upscale != upscale;
        self.last_render = render;
        self.last_upscale = upscale;
        let (near, far) = if t.near > 0.0 && t.far > 0.0 && (t.near - t.far).abs() > f32::EPSILON {
            (t.near, t.far)
        } else {
            (0.1, 10000.0)
        };
        HostParams {
            jitter: t.jitter,
            motion_scale: t.motion_scale,
            render,
            upscale,
            sharpening: sharpness > 0.0,
            sharpness: sharpness.clamp(0.0, 1.0),
            frame_ms: if t.frame_ms.is_finite() { t.frame_ms.clamp(1.0, 200.0) } else { 16.7 },
            pre_exposure: if t.pre_exposure > 0.0 && t.pre_exposure.is_finite() { t.pre_exposure } else { 1.0 },
            reset: t.reset || resized || switched,
            near,
            far,
            fov_y: if t.fov_y > 0.0 && t.fov_y < std::f32::consts::PI { t.fov_y } else { 1.0 },
            meters: if t.meters > 0.0 { t.meters } else { 1.0 },
            flags: t.flags,
        }
    }

    pub fn last_render(&self) -> [u32; 2] {
        self.last_render
    }
}

// ---------------------------------------------------------------------------
// Frame generation gate
// ---------------------------------------------------------------------------

/// Decides each frame whether generated frames are presented, from the
/// base (rendered) frame time. Off below [`FRAMEGEN_MIN_BASE_FPS`], back on
/// above [`FRAMEGEN_RESUME_FPS`]; the rate is averaged over a few frames so
/// one hitch does not flip it.
#[derive(Debug)]
pub struct FramegenGate {
    avg_ms: f32,
    on: bool,
}

impl Default for FramegenGate {
    fn default() -> FramegenGate {
        FramegenGate { avg_ms: 0.0, on: false }
    }
}

impl FramegenGate {
    /// Feeds one base frame time (ms); answers whether generation is on.
    pub fn frame(&mut self, wanted: bool, base_ms: f32) -> bool {
        if base_ms.is_finite() && base_ms > 0.0 {
            self.avg_ms = if self.avg_ms == 0.0 { base_ms } else { self.avg_ms * 0.9 + base_ms * 0.1 };
        }
        let fps = if self.avg_ms > 0.0 { 1000.0 / self.avg_ms } else { 0.0 };
        self.on = wanted
            && if self.on {
                fps >= FRAMEGEN_MIN_BASE_FPS
            } else {
                fps >= FRAMEGEN_RESUME_FPS
            };
        self.on
    }

    pub fn base_fps(&self) -> f32 {
        if self.avg_ms > 0.0 { 1000.0 / self.avg_ms } else { 0.0 }
    }
}

/// Live state the overlay shows: what answered the last dispatch.
pub mod status {
    use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

    static ANSWERED: AtomicU8 = AtomicU8::new(0);
    static RENDER: AtomicU32 = AtomicU32::new(0);
    static OUTPUT: AtomicU32 = AtomicU32::new(0);
    static FRAMEGEN: AtomicU8 = AtomicU8::new(0);

    /// What answered: 0 nothing yet, 1 FSR, 2 stretch, 3 the title's DLL.
    pub fn set_answered(by: u8, render: [u32; 2], output: [u32; 2]) {
        ANSWERED.store(by, Ordering::Relaxed);
        RENDER.store(render[0] << 16 | render[1] & 0xffff, Ordering::Relaxed);
        OUTPUT.store(output[0] << 16 | output[1] & 0xffff, Ordering::Relaxed);
    }

    /// 0 off, 1 on, 2 wanted but held off by the base rate, 3 unavailable.
    pub fn set_framegen(state: u8) {
        FRAMEGEN.store(state, Ordering::Relaxed);
    }

    pub fn text() -> String {
        let by = match ANSWERED.load(Ordering::Relaxed) {
            0 => "no upscale dispatch yet",
            1 => "FSR 3",
            2 => "stretch",
            _ => "title DLL",
        };
        let (r, o) = (RENDER.load(Ordering::Relaxed), OUTPUT.load(Ordering::Relaxed));
        let fg = match FRAMEGEN.load(Ordering::Relaxed) {
            1 => "frame gen on",
            2 => "frame gen held (base < 15 fps)",
            3 => "frame gen unavailable",
            _ => "frame gen off",
        };
        format!("{by} {}x{} -> {}x{}, {fg}", r >> 16, r & 0xffff, o >> 16, o & 0xffff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |n| m.get(n).cloned()
    }

    #[test]
    fn defaults_are_fsr_quality_without_framegen() {
        let s = Switches::from_lookup(lookup(&[]));
        assert_eq!(s, Switches::default());
        assert_eq!(s.upscaler, Upscaler::Fsr);
        assert!(!s.framegen);
    }

    #[test]
    fn every_upscaler_parses_and_names_itself() {
        for u in Upscaler::ALL {
            assert_eq!(Upscaler::parse(u.name()), Some(u));
            assert_eq!(Upscaler::from_index(u.index()), u);
        }
        assert_eq!(Upscaler::parse(" DLSS "), Some(Upscaler::Dlss));
        assert_eq!(Upscaler::parse("xess"), None);
    }

    #[test]
    fn the_old_own_upscaler_switch_means_title_and_the_new_one_wins() {
        let s = Switches::from_lookup(lookup(&[(OWN_UPSCALER_ENV, "1")]));
        assert_eq!(s.upscaler, Upscaler::Title);
        assert!(s.upscaler.wants_own_upscaler());
        let s = Switches::from_lookup(lookup(&[(OWN_UPSCALER_ENV, "1"), (UPSCALER_ENV, "fsr")]));
        assert_eq!(s.upscaler, Upscaler::Fsr);
        assert!(!s.upscaler.wants_own_upscaler());
        let s = Switches::from_lookup(lookup(&[(OWN_UPSCALER_ENV, "0")]));
        assert_eq!(s.upscaler, Upscaler::Fsr);
    }

    #[test]
    fn a_bad_value_keeps_the_default() {
        let s = Switches::from_lookup(lookup(&[(UPSCALER_ENV, "magic"), (QUALITY_ENV, "best")]));
        assert_eq!(s.upscaler, Upscaler::Fsr);
        assert_eq!(s.quality, Quality::Quality);
    }

    #[test]
    fn dlss_is_answered_by_fsr_until_it_is_wired() {
        assert_eq!(Upscaler::Dlss.effective(), Upscaler::Fsr);
        assert_eq!(Upscaler::Stretch.effective(), Upscaler::Stretch);
    }

    #[test]
    fn quality_modes_match_fidelityfx() {
        let modes: Vec<u32> = Quality::ALL.iter().map(|q| q.ffx_mode()).collect();
        assert_eq!(modes, [0, 1, 2, 3, 4]);
        assert_eq!(Quality::Quality.render_size([1920, 1080]), [1280, 720]);
        assert_eq!(Quality::Performance.render_size([1920, 1080]), [960, 540]);
        assert_eq!(Quality::UltraPerformance.render_size([3840, 2160]), [1280, 720]);
        assert_eq!(Quality::NativeAa.render_size([1920, 1080]), [1920, 1080]);
        assert!((Quality::Quality.render_scale() - 0.6667).abs() < 1e-3);
        for q in Quality::ALL {
            assert_eq!(Quality::parse(q.name()), Some(q));
        }
    }

    #[test]
    fn sharpness_is_clamped_and_framegen_is_a_switch() {
        let s = Switches::from_lookup(lookup(&[(SHARPNESS_ENV, "3"), (FRAMEGEN_ENV, "1")]));
        assert_eq!(s.sharpness, 1.0);
        assert!(s.framegen);
        let s = Switches::from_lookup(lookup(&[(FRAMEGEN_ENV, "off")]));
        assert!(!s.framegen);
        // DLSS FG is not offered: asking for it still means FSR 3 FG.
        let s = Switches::from_lookup(lookup(&[(FRAMEGEN_ENV, "dlss")]));
        assert!(s.framegen);
    }

    #[test]
    fn halton_is_the_radical_inverse() {
        assert_eq!(halton(1, 2), 0.5);
        assert_eq!(halton(2, 2), 0.25);
        assert_eq!(halton(3, 2), 0.75);
        assert!((halton(1, 3) - 1.0 / 3.0).abs() < 1e-6);
        assert!((halton(2, 3) - 2.0 / 3.0).abs() < 1e-6);
        assert!((halton(4, 3) - 4.0 / 9.0).abs() < 1e-6);
        assert_eq!(halton(0, 2), 0.0);
    }

    #[test]
    fn the_phase_count_is_fidelityfx_s() {
        assert_eq!(jitter_phase_count(1920, 1920), 8);
        assert_eq!(jitter_phase_count(1280, 1920), 18); // 8 * 1.5^2
        assert_eq!(jitter_phase_count(960, 1920), 32); // 8 * 2^2
        assert_eq!(jitter_phase_count(640, 1920), 72); // 8 * 3^2
        assert_eq!(jitter_phase_count(0, 1920), 8);
    }

    #[test]
    fn jitter_stays_inside_a_pixel_wraps_and_never_repeats_within_a_cycle() {
        let phases = jitter_phase_count(1280, 1920);
        let all: Vec<[f32; 2]> = (0..phases).map(|i| jitter_offset(i, phases)).collect();
        for j in &all {
            assert!(j[0] > -0.5 && j[0] < 0.5 && j[1] > -0.5 && j[1] < 0.5);
        }
        for (a, x) in all.iter().enumerate() {
            for y in &all[a + 1..] {
                assert_ne!(x, y);
            }
        }
        assert_eq!(jitter_offset(phases, phases), all[0]);
        assert_eq!(jitter_offset(0, phases), [0.0, 1.0 / 3.0 - 0.5]);
        // Centred: the mean over a cycle is near zero.
        let mean: f32 = all.iter().map(|j| j[0]).sum::<f32>() / phases as f32;
        assert!(mean.abs() < 0.05, "{mean}");
    }

    #[test]
    fn jitter_in_clip_space_flips_y() {
        assert_eq!(jitter_to_clip([0.5, 0.5], [1000, 500]), [0.001, -0.002]);
    }

    #[test]
    fn a_still_or_wild_jitter_is_noticed() {
        let mut w = JitterWatch::default();
        for i in 0..40 {
            assert_eq!(w.see(jitter_offset(i, 18)), JitterVerdict::Fine);
        }
        let mut w = JitterWatch::default();
        let mut last = JitterVerdict::Fine;
        for _ in 0..JitterWatch::STILL_AFTER {
            last = w.see([0.0, 0.0]);
        }
        assert_eq!(last, JitterVerdict::Still(JitterWatch::STILL_AFTER));
        assert_eq!(w.see([3.0, 0.0]), JitterVerdict::OutOfRange);
        assert_eq!(w.see([f32::NAN, 0.0]), JitterVerdict::OutOfRange);
    }

    fn title() -> TitleParams {
        TitleParams {
            jitter: [0.25, -0.125],
            motion_scale: [-1280.0, 720.0],
            render: [1280, 720],
            upscale: [1920, 1080],
            sharpening: true,
            sharpness: 0.2,
            frame_ms: 33.3,
            pre_exposure: 1.0,
            reset: false,
            near: 0.05,
            far: 15000.0,
            fov_y: 0.9,
            meters: 1.0,
            flags: 0,
        }
    }

    #[test]
    fn jitter_and_motion_scale_go_through_untouched() {
        let mut p = Plumbing::default();
        let h = p.host(&title(), [1920, 1080], [1920, 1080], 0.3, false);
        assert_eq!(h.jitter, [0.25, -0.125]);
        assert_eq!(h.motion_scale, [-1280.0, 720.0]);
        assert_eq!(h.render, [1280, 720]);
        assert_eq!(h.upscale, [1920, 1080]);
        assert_eq!((h.near, h.far, h.fov_y), (0.05, 15000.0, 0.9));
        assert_eq!(h.sharpness, 0.3);
        assert!(h.sharpening);
        assert!(!h.reset);
        assert_eq!(p.last_render(), [1280, 720]);
    }

    #[test]
    fn sizes_fall_back_to_the_textures() {
        let mut p = Plumbing::default();
        let mut t = title();
        t.render = [0, 0];
        t.upscale = [4000, 4000];
        let h = p.host(&t, [1280, 720], [1920, 1080], 0.0, false);
        assert_eq!(h.render, [1280, 720]);
        assert_eq!(h.upscale, [1920, 1080]);
        assert!(!h.sharpening);
    }

    #[test]
    fn history_resets_on_resize_switch_or_request() {
        let mut p = Plumbing::default();
        assert!(!p.host(&title(), [1920, 1080], [1920, 1080], 0.2, false).reset);
        assert!(!p.host(&title(), [1920, 1080], [1920, 1080], 0.2, false).reset);
        let mut t = title();
        t.upscale = [2560, 1440];
        assert!(p.host(&t, [1920, 1080], [2560, 1440], 0.2, false).reset);
        assert!(!p.host(&t, [1920, 1080], [2560, 1440], 0.2, false).reset);
        assert!(p.host(&t, [1920, 1080], [2560, 1440], 0.2, true).reset);
        t.reset = true;
        assert!(p.host(&t, [1920, 1080], [2560, 1440], 0.2, false).reset);
    }

    #[test]
    fn bad_numbers_are_made_safe() {
        let mut p = Plumbing::default();
        let mut t = title();
        t.frame_ms = 0.0;
        t.pre_exposure = 0.0;
        t.near = 0.0;
        t.fov_y = 0.0;
        t.meters = -1.0;
        let h = p.host(&t, [1920, 1080], [1920, 1080], 5.0, false);
        assert_eq!(h.frame_ms, 1.0);
        assert_eq!(h.pre_exposure, 1.0);
        assert_eq!((h.near, h.far), (0.1, 10000.0));
        assert_eq!(h.fov_y, 1.0);
        assert_eq!(h.meters, 1.0);
        assert_eq!(h.sharpness, 1.0);
        t.frame_ms = f32::NAN;
        assert_eq!(p.host(&t, [1920, 1080], [1920, 1080], 0.2, false).frame_ms, 16.7);
    }

    #[test]
    fn framegen_only_runs_at_15_fps_base_or_more() {
        let mut g = FramegenGate::default();
        // 30 fps base: on.
        for _ in 0..50 {
            assert!(g.frame(true, 33.3));
        }
        // Not wanted: off at any rate.
        assert!(!g.frame(false, 10.0));
        let mut g = FramegenGate::default();
        // 10 fps base: never on.
        for _ in 0..100 {
            assert!(!g.frame(true, 100.0));
        }
        // 16 fps: between the thresholds, still off (it was off).
        let mut g = FramegenGate::default();
        for _ in 0..200 {
            assert!(!g.frame(true, 62.5));
        }
    }

    #[test]
    fn framegen_has_hysteresis_and_ignores_one_hitch() {
        let mut g = FramegenGate::default();
        for _ in 0..100 {
            g.frame(true, 50.0); // 20 fps
        }
        assert!(g.frame(true, 50.0));
        // One 200 ms hitch does not turn it off.
        assert!(g.frame(true, 200.0));
        // A sustained drop to 12 fps does.
        let mut on = true;
        for _ in 0..200 {
            on = g.frame(true, 83.3);
        }
        assert!(!on);
        // Back at 16 fps: below the resume rate, stays off.
        for _ in 0..200 {
            on = g.frame(true, 62.5);
        }
        assert!(!on);
        // 20 fps: on again.
        for _ in 0..200 {
            on = g.frame(true, 50.0);
        }
        assert!(on);
        assert!((g.base_fps() - 20.0).abs() < 0.5);
    }

    #[test]
    fn a_live_switch_asks_for_one_reset() {
        let s = *switches();
        set_live(Upscaler::Stretch, false, 0.5);
        assert_eq!(live_upscaler(), Upscaler::Stretch);
        assert_eq!(live_sharpness(), 0.5);
        assert!(take_reset());
        assert!(!take_reset());
        set_live(s.upscaler, s.framegen, s.sharpness);
        let _ = take_reset();
    }

    #[test]
    fn file_values_read_as_variables() {
        // FILE is process-wide; only check the key mapping on an unset file.
        assert_eq!(file_value("VICTORY_NOTHING"), None);
    }

    #[test]
    fn status_text_names_what_answered() {
        status::set_answered(1, [1280, 720], [1920, 1080]);
        status::set_framegen(2);
        let t = status::text();
        assert!(t.starts_with("FSR 3 1280x720 -> 1920x1080"), "{t}");
        assert!(t.contains("base < 15 fps"));
    }
}
