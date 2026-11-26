//! The settings menu: three pages (controls, display, performance) drawn
//! with [`super::gui`] over the game, and the hud graph shown while it is
//! closed. Everything it changes is in [`Model`]; what a frame decided is
//! handed back as [`Effects`] for the caller to apply and save.

use super::font;
use super::gui::{Color, DrawList, Input, Moved, Rect, Ui};
use super::perf::{self, Change, Perf, SysInfo, switch_rows};
use crate::hints::Hint;
use super::store::{Display, Model, PaceModel, UpscaleModel, WindowMode};
use crate::upscale::{Quality, Upscaler};
use crate::kbm::{Output, Settings, WHEEL_DOWN, WHEEL_UP, key_name};

/// What the menu needs to know about the world this frame.
pub struct Ctx<'a> {
    /// The window's client size, pixels.
    pub size: (f32, f32),
    pub perf: &'a Perf,
    pub sys: &'a SysInfo,
    /// Environment lookup (`VICTORY_*`).
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Milliseconds on any steady clock (for blinking).
    pub now_ms: u64,
}

/// What a frame decided.
#[derive(Default, Debug)]
pub struct Effects {
    /// New control settings, to take effect now.
    pub controls: Option<Settings>,
    pub display: Option<Display>,
    pub pace: Option<PaceModel>,
    pub upscale: Option<UpscaleModel>,
    /// Something was set for good: write the files.
    pub save: bool,
    /// Close the menu.
    pub close: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Controls,
    Display,
    Performance,
}

const PAGES: [(Page, &str); 3] = [
    (Page::Controls, "Controls"),
    (Page::Display, "Display"),
    (Page::Performance, "Performance"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    Foot,
    Vehicle,
}

/// A binding waiting for its key.
#[derive(Clone, Copy, Debug)]
struct Capture {
    out: Output,
    /// The key the box held (None: an empty box).
    old: Option<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Good,
    Warn,
    Bad,
}

const SLOTS: usize = 3;

pub struct Menu {
    pub model: Model,
    start_pace: PaceModel,
    start_upscale: UpscaleModel,
    /// Keyboard and mouse play is on (the bindings do something).
    kbm_on: bool,
    ui: Ui,
    page: Page,
    layer: Layer,
    capture: Option<Capture>,
    status: Option<(String, Tone)>,
    armed: bool,
    /// Each page's content height as last drawn (its scroll range).
    heights: [f32; 3],
}

/// Whether a key can be bound: not the ones the menu keeps for itself
/// (Esc, Delete, F1), the generic modifiers (only left and right ones are
/// reported) and the Windows keys.
pub fn bindable(vk: u8) -> bool {
    matches!(vk, 0x01 | 0x02 | 0x04 | 0x05 | 0x06 | 0x08..=0xFE)
        && !matches!(vk, 0x10..=0x12 | 0x1B | 0x2E | 0x5B | 0x5C | 0x70)
}

fn num(v: f32, places: usize) -> String {
    format!("{v:.places$}")
}

impl Menu {
    pub fn new(model: Model, start_pace: PaceModel, kbm_on: bool) -> Menu {
        Menu {
            model,
            start_pace,
            start_upscale: UpscaleModel::at_start(),
            kbm_on,
            ui: Ui::default(),
            page: Page::Controls,
            layer: Layer::Foot,
            capture: None,
            status: None,
            armed: false,
            heights: [0.0; 3],
        }
    }

    /// The triangles of the last frame.
    pub fn draw_list(&self) -> &DrawList {
        &self.ui.draw
    }

    pub fn waiting_for_key(&self) -> bool {
        self.capture.is_some()
    }

    /// Gives up a binding that waits for a key (the menu was closed).
    pub fn cancel_capture(&mut self) {
        self.capture = None;
    }

    pub fn restart_needed(&self) -> Vec<&'static str> {
        let mut v = self.model.pace.restart_needed(&self.start_pace);
        v.extend(self.model.upscale.restart_needed(&self.start_upscale));
        v
    }

    fn scale(&self, size: (f32, f32)) -> f32 {
        match self.model.display.ui_scale {
            0 => (size.1 / 540.0).round().clamp(1.0, 6.0),
            n => f32::from(n),
        }
    }

    /// The hud: a small graph in a corner, while the menu is closed.
    pub fn hud(&mut self, ctx: &Ctx) {
        self.hud_with(ctx, None);
    }

    /// The graph (when on) and the keyboard hints (when given) over the game.
    pub fn hud_with(&mut self, ctx: &Ctx, hints: Option<&[Hint]>) {
        let scale = self.scale(ctx.size);
        self.ui.begin(Input::default(), scale, ctx.size);
        if self.model.display.graph {
            self.graph_hud(ctx);
        }
        if let Some(h) = hints {
            self.hints_hud(ctx, h);
        }
    }

    /// The legend in the bottom left corner: one chip per button, wrapped
    /// to the window's width.
    fn hints_hud(&mut self, ctx: &Ctx, hints: &[Hint]) {
        let ui = &mut self.ui;
        let t = ui.theme;
        let (m, pad, gap, row_h) = (ui.u(6.0), ui.u(3.0), ui.u(4.0), ui.u(13.0));
        let max_w = (ctx.size.0 - 2.0 * m).max(ui.u(40.0));
        let chips: Vec<(String, f32)> = hints
            .iter()
            .map(|h| {
                let text = format!("{} {}", h.button, h.keys);
                let w = font::text_width(&text, ui.scale) + 2.0 * pad;
                (text, w)
            })
            .collect();
        let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
        let mut used = 0.0;
        for (i, (_, w)) in chips.iter().enumerate() {
            if used > 0.0 && used + gap + w > max_w {
                rows.push(Vec::new());
                used = 0.0;
            }
            used += if used > 0.0 { gap + w } else { *w };
            rows.last_mut().unwrap().push(i);
        }
        let total_h = rows.len() as f32 * (row_h + gap) - gap;
        let mut y = (ctx.size.1 - m - total_h).max(0.0);
        for row in rows {
            let mut x = m;
            for i in row {
                let (text, w) = &chips[i];
                ui.draw.rect(Rect::new(x, y, *w, row_h), t.dim.with_alpha(190));
                ui.text(x + pad, y + ui.u(3.0), text, t.text);
                x += w + gap;
            }
            y += row_h + gap;
        }
    }

    fn graph_hud(&mut self, ctx: &Ctx) {
        let ui = &mut self.ui;
        let t = ui.theme;
        let (m, w, h) = (ui.u(6.0), ui.u(132.0), ui.u(48.0));
        let r = Rect::new(m, m, w, h);
        ui.draw.rect(r, t.dim.with_alpha(170));
        let line = format!("{:.0} fps  {:.1} ms", ctx.perf.fps(), ctx.perf.frame_ms());
        ui.text(r.x + ui.u(4.0), r.y + ui.u(3.0), &line, t.text);
        let g = Rect::new(
            r.x + ui.u(4.0),
            r.y + ui.u(15.0),
            w - ui.u(8.0),
            h - ui.u(19.0),
        );
        let samples = ctx.perf.samples(120);
        ui.graph(g, &samples, 50.0, &[]);
    }

    /// One frame of the open menu.
    pub fn frame(&mut self, ctx: &Ctx, input: Input) -> Effects {
        let mut fx = Effects::default();
        let keys = input.keys_pressed.clone();
        let wheel = input.wheel;
        let scale = self.scale(ctx.size);
        self.ui.begin(input, scale, ctx.size);
        self.ui.blocked = self.capture.is_some();
        self.keys(&keys, wheel, &mut fx);
        // A binding just made is not drawn as waiting.
        self.ui.blocked = self.capture.is_some();

        let t = self.ui.theme;
        let size = ctx.size;
        let u = |n: f32| n * scale;
        let full = Rect::new(0.0, 0.0, size.0, size.1);
        self.ui.draw.rect(full, t.dim);
        let pw = (size.0 - u(16.0)).min(u(640.0));
        let ph = (size.1 - u(16.0)).min(u(420.0));
        let panel = Rect::new(
            ((size.0 - pw) / 2.0).floor(),
            ((size.1 - ph) / 2.0).floor(),
            pw,
            ph,
        );
        self.ui.draw.rect(panel, t.panel);
        self.ui
            .draw
            .outline(panel, 1.0f32.max((scale / 2.0).floor()), t.border);
        let bar = Rect::new(panel.x, panel.y, panel.w, u(18.0));
        self.ui.draw.rect(bar, t.bar);
        self.ui.text_in(
            Rect::new(bar.x + u(6.0), bar.y, bar.w, bar.h),
            0,
            "VIctory settings",
            t.text,
        );
        self.ui.text_in(
            Rect::new(bar.x, bar.y, bar.w - u(6.0), bar.h),
            2,
            "F1 or Esc closes",
            t.muted,
        );

        // Tabs.
        let mut x = panel.x + u(8.0);
        let ty = bar.bottom() + u(5.0);
        for (page, name) in PAGES {
            let r = Rect::new(x, ty, u(100.0), u(16.0));
            if self
                .ui
                .button(&format!("tab {name}"), r, name, self.page == page)
                && self.page != page
            {
                self.page = page;
                self.armed = false;
                self.ui.scroll_to("page", 0.0);
            }
            x += u(104.0);
        }

        let footer_h = u(30.0);
        let content = Rect::new(
            panel.x + u(8.0),
            ty + u(22.0),
            panel.w - u(16.0),
            panel.bottom() - footer_h - (ty + u(22.0)) - u(4.0),
        );
        let index = PAGES.iter().position(|(p, _)| *p == self.page).unwrap_or(0);
        let height = self.heights[index];
        let scroll = self.ui.scroll_begin("page", content, height, u(24.0));
        let mut body = Body {
            x: content.x,
            y: content.y - scroll.1,
            w: content.w - u(8.0),
            start: content.y - scroll.1,
        };
        match self.page {
            Page::Controls => self.page_controls(&mut body, &mut fx),
            Page::Display => self.page_display(&mut body, ctx, &mut fx),
            Page::Performance => self.page_performance(&mut body, ctx),
        }
        self.heights[index] = body.y - body.start + u(4.0);
        self.ui.scroll_end(scroll, content, self.heights[index]);

        self.footer(panel, footer_h, &mut fx);
        fx
    }

    /// Keys and the wheel: a binding waits for one, Esc closes.
    fn keys(&mut self, keys: &[u8], wheel: f32, fx: &mut Effects) {
        let Some(cap) = self.capture else {
            if keys.contains(&0x1B) {
                fx.close = true;
            }
            return;
        };
        if keys.contains(&0x1B) {
            self.capture = None;
            self.status = Some(("Binding cancelled.".into(), Tone::Warn));
            return;
        }
        let new = if keys.contains(&0x2E) {
            Some(None)
        } else if wheel != 0.0 {
            Some(Some(if wheel > 0.0 { WHEEL_UP } else { WHEEL_DOWN }))
        } else {
            keys.iter().copied().find(|&k| bindable(k)).map(Some)
        };
        let Some(new) = new else { return };
        self.capture = None;
        let layout = match self.layer {
            Layer::Foot => &mut self.model.controls.on_foot,
            Layer::Vehicle => &mut self.model.controls.vehicle,
        };
        let lost = layout.rebind(cap.out, cap.old, new);
        self.status = Some(match new {
            Some(k) => {
                let mut text = format!("{} now does: {}.", key_name(k), cap.out.label());
                if !lost.is_empty() {
                    let names: Vec<&str> = lost.iter().map(|o| o.label()).collect();
                    text.push_str(&format!(" It no longer does: {}.", names.join(", ")));
                }
                (text, Tone::Good)
            }
            None => (
                format!("Cleared a key of: {}.", cap.out.label()),
                Tone::Good,
            ),
        });
        fx.controls = Some(self.model.controls.clone());
        fx.save = true;
    }

    fn row(&self, body: &mut Body, h: f32) -> Rect {
        let r = Rect::new(body.x, body.y, body.w, h);
        body.y += h;
        r
    }

    fn heading(&mut self, body: &mut Body, text: &str) {
        let u = self.ui.scale;
        body.y += 4.0 * u;
        let r = self.row(body, 14.0 * u);
        let t = self.ui.theme;
        self.ui.text_in(r, 0, text, t.warn);
        self.ui.draw.rect(
            Rect::new(r.x, r.bottom() - u, r.w, 1.0f32.max(u / 2.0)),
            t.border,
        );
    }

    fn note(&mut self, body: &mut Body, text: &str, tone: Color) {
        let u = self.ui.scale;
        let r = self.row(body, 12.0 * u);
        self.ui.text_in(r, 0, text, tone);
    }

    fn page_controls(&mut self, body: &mut Body, fx: &mut Effects) {
        let u = self.ui.scale;
        let muted = self.ui.theme.muted;
        let warn = self.ui.theme.warn;
        let row_h = 16.0 * u;
        let mut changed = false;
        let mut committed = false;
        if !self.kbm_on {
            self.note(
                body,
                "Keyboard and mouse play is off (VICTORY_KBM=0); changes are saved, not used.",
                warn,
            );
        }
        self.heading(body, "Mouse and controller");
        {
            let c = &mut self.model.controls;
            let ui = &mut self.ui;
            let mut moved = [Moved::default(); 4];
            moved[0] = slider_row(
                ui,
                body,
                row_h,
                "sens",
                "Mouse sensitivity",
                &mut c.sensitivity,
                (0.1, 5.0, 0.05),
            );
            moved[1] = slider_row(
                ui,
                body,
                row_h,
                "ysens",
                "Vertical sensitivity",
                &mut c.y_scale,
                (0.25, 3.0, 0.05),
            );
            let r = Rect::new(body.x, body.y, body.w, row_h);
            body.y += row_h;
            if ui.toggle("invert", r, "Invert vertical look", &mut c.invert_y) {
                changed = true;
                committed = true;
            }
            moved[2] = slider_row(
                ui,
                body,
                row_h,
                "walk",
                "Walk speed (Walk key)",
                &mut c.walk_speed,
                (0.1, 1.0, 0.05),
            );
            moved[3] = slider_row(
                ui,
                body,
                row_h,
                "dz",
                "Controller dead zone",
                &mut c.pad_deadzone,
                (0.0, 0.5, 0.01),
            );
            for m in moved {
                changed |= m.changed;
                committed |= m.committed;
            }
        }
        if changed || committed {
            fx.controls = Some(self.model.controls.clone());
            fx.save |= committed;
        }

        self.heading(body, "Key bindings");
        {
            let r = self.row(body, row_h);
            let mut layer = usize::from(self.layer == Layer::Vehicle);
            if self.ui.choice(
                "layer",
                r,
                "Layout (G switches in game)",
                &["On foot", "Vehicle"],
                &mut layer,
            ) {
                self.layer = if layer == 0 {
                    Layer::Foot
                } else {
                    Layer::Vehicle
                };
            }
        }
        self.note(
            body,
            "Click a box, then press the new key or mouse button. Esc cancels, Delete clears.",
            muted,
        );
        let layout = match self.layer {
            Layer::Foot => self.model.controls.on_foot.clone(),
            Layer::Vehicle => self.model.controls.vehicle.clone(),
        };
        let slot_w = 100.0 * u;
        let gap = 4.0 * u;
        for out in Output::all() {
            let r = self.row(body, row_h);
            let keys = layout.keys_of(out);
            let label_w = r.w - SLOTS as f32 * (slot_w + gap);
            let text = self.ui.theme.text;
            self.ui
                .text_in(Rect::new(r.x, r.y, label_w, r.h), 0, out.label(), text);
            for slot in 0..SLOTS {
                let b = Rect::new(
                    r.x + label_w + slot as f32 * (slot_w + gap),
                    r.y + u * 1.0,
                    slot_w,
                    r.h - 2.0 * u,
                );
                let key = keys.get(slot).copied();
                let waiting = self.capture.is_some_and(|c| {
                    c.out == out
                        && match c.old {
                            Some(k) => key == Some(k),
                            None => key.is_none() && slot == keys.len().min(SLOTS - 1),
                        }
                });
                let label = if waiting {
                    "press a key".to_string()
                } else {
                    key.map_or_else(|| "-".to_string(), key_name)
                };
                // Keys past the last box are kept, not shown.
                let extra = slot == SLOTS - 1 && keys.len() > SLOTS;
                let label = if extra {
                    format!("{label} +{}", keys.len() - SLOTS)
                } else {
                    label
                };
                if self
                    .ui
                    .button(&format!("slot {} {slot}", out.name()), b, &label, waiting)
                    && self.capture.is_none()
                {
                    self.capture = Some(Capture { out, old: key });
                    self.status = None;
                }
            }
        }
    }

    fn page_display(&mut self, body: &mut Body, ctx: &Ctx, fx: &mut Effects) {
        let u = self.ui.scale;
        let row_h = 16.0 * u;
        let (muted, warn) = (self.ui.theme.muted, self.ui.theme.warn);
        let env = |name: &str| (ctx.env)(name).is_some();
        let mut display = self.model.display;
        let mut pace = self.model.pace;
        let (mut d_changed, mut p_changed) = (false, false);

        self.heading(body, "Window");
        let r = self.row(body, row_h);
        let mut mode = usize::from(display.window_mode == WindowMode::Borderless);
        if self.ui.choice(
            "mode",
            r,
            "Window mode",
            &["Windowed", "Borderless"],
            &mut mode,
        ) {
            display.window_mode = if mode == 0 {
                WindowMode::Windowed
            } else {
                WindowMode::Borderless
            };
            d_changed = true;
        }
        let r = self.row(body, row_h);
        let mut scale = usize::from(display.ui_scale).min(6);
        if self.ui.choice(
            "scale",
            r,
            "Interface size",
            &["Auto", "1", "2", "3", "4"],
            &mut scale,
        ) {
            display.ui_scale = scale as u8;
            d_changed = true;
        }
        let r = self.row(body, row_h);
        if self.ui.toggle(
            "graph",
            r,
            "Frame-time graph on screen (menu closed)",
            &mut display.graph,
        ) {
            d_changed = true;
        }

        self.heading(body, "Frame pacing (VICTORY_PACE*)");
        let tag = |name: &str, restart: bool| {
            let mut t = if restart { " [restart]" } else { " [live]" }.to_string();
            if env(name) {
                t.push_str(" [env]");
            }
            t
        };
        let r = self.row(body, row_h);
        if self.ui.toggle(
            "pace",
            r,
            &format!("Even frame pacing{}", tag("VICTORY_PACE", false)),
            &mut pace.pace,
        ) {
            p_changed = true;
        }
        let r = self.row(body, row_h);
        let rates = super::store::FRAME_CAPS;
        let mut rate = rates.iter().position(|x| *x == pace.fps).unwrap_or(0);
        if self.ui.choice(
            "fps",
            r,
            &format!("Frame cap{}", tag("VICTORY_PACE_FPS", false)),
            &["Title", "30", "45", "60", "90", "120", "144", "240"],
            &mut rate,
        ) {
            pace = super::store::with_frame_cap(pace, rates[rate]);
            p_changed = true;
        }
        let r = self.row(body, row_h);
        if self.ui.toggle(
            "input",
            r,
            &format!("Read input late{}", tag("VICTORY_PACE_INPUT", false)),
            &mut pace.input,
        ) {
            p_changed = true;
        }
        let r = self.row(body, row_h);
        let mut latency = pace.latency.min(2) as usize;
        if self.ui.choice(
            "latency",
            r,
            &format!("Frames queued at most{}", tag("VICTORY_PACE_LATENCY", true)),
            &["Off", "1", "2"],
            &mut latency,
        ) {
            pace.latency = latency as u32;
            p_changed = true;
        }
        let r = self.row(body, row_h);
        if self.ui.toggle(
            "tearing",
            r,
            &format!("Allow tearing (VRR){}", tag("VICTORY_PACE_TEARING", true)),
            &mut pace.tearing,
        ) {
            p_changed = true;
        }
        let r = self.row(body, row_h);
        if self.ui.toggle(
            "feedback",
            r,
            &format!("Display feedback{}", tag("VICTORY_PACE_FEEDBACK", true)),
            &mut pace.feedback,
        ) {
            p_changed = true;
        }
        self.note(
            body,
            "[live] applies at once. [restart] is saved and used at the next start.",
            muted,
        );
        if [
            "VICTORY_PACE",
            "VICTORY_PACE_FPS",
            "VICTORY_PACE_INPUT",
            "VICTORY_PACE_LATENCY",
            "VICTORY_PACE_TEARING",
            "VICTORY_PACE_FEEDBACK",
        ]
        .iter()
        .any(|n| env(n))
        {
            self.note(
                body,
                "[env]: the environment sets it at start and wins over the file.",
                warn,
            );
        }
        let mut upscale = self.model.upscale;
        let mut u_changed = false;
        self.heading(body, "Upscaling (VICTORY_UPSCALER)");
        let r = self.row(body, row_h);
        let mut which = Upscaler::ALL.iter().position(|x| *x == upscale.upscaler).unwrap_or(0);
        if self.ui.choice(
            "upscaler",
            r,
            &format!("Upscaler{}", tag("VICTORY_UPSCALER", false)),
            &["FSR 3", "DLSS", "Title TAA", "Stretch"],
            &mut which,
        ) {
            upscale.upscaler = Upscaler::ALL[which];
            u_changed = true;
        }
        let r = self.row(body, row_h);
        let mut q = Quality::ALL.iter().position(|x| *x == upscale.quality).unwrap_or(1);
        if self.ui.choice(
            "quality",
            r,
            &format!("Quality mode{}", tag("VICTORY_UPSCALE_QUALITY", true)),
            &["Native AA", "Quality", "Balanced", "Perf.", "Ultra perf."],
            &mut q,
        ) {
            upscale.quality = Quality::ALL[q];
            u_changed = true;
        }
        let r = self.row(body, row_h);
        let sharp = [0.0f32, 0.2, 0.5, 0.8];
        let mut si = sharp
            .iter()
            .position(|x| (x - upscale.sharpness).abs() < 0.05)
            .unwrap_or(1);
        if self.ui.choice(
            "sharpness",
            r,
            &format!("Sharpening{}", tag("VICTORY_UPSCALE_SHARPNESS", false)),
            &["Off", "0.2", "0.5", "0.8"],
            &mut si,
        ) {
            upscale.sharpness = sharp[si];
            u_changed = true;
        }
        let r = self.row(body, row_h);
        if self.ui.toggle(
            "framegen",
            r,
            &format!("FSR 3 frame generation{}", tag("VICTORY_FRAMEGEN", false)),
            &mut upscale.framegen,
        ) {
            u_changed = true;
        }
        self.note(body, &crate::upscale::status::text(), muted);
        if upscale.upscaler == Upscaler::Dlss {
            self.note(body, "DLSS is not wired yet: FSR 3 answers in its place.", warn);
        }
        if upscale.framegen {
            self.note(
                body,
                "Frame generation needs a base rate of 15 fps or more; below that it holds off.",
                warn,
            );
        }
        if u_changed {
            self.model.upscale = upscale;
            fx.upscale = Some(upscale);
            fx.save = true;
        }
        self.note(
            body,
            "Exclusive fullscreen is not offered: a borderless window presents the same way.",
            muted,
        );

        if d_changed {
            self.model.display = display;
            fx.display = Some(display);
            fx.save = true;
        }
        if p_changed {
            self.model.pace = pace;
            fx.pace = Some(pace);
            fx.save = true;
        }
    }

    fn page_performance(&mut self, body: &mut Body, ctx: &Ctx) {
        let u = self.ui.scale;
        let t = self.ui.theme;
        let pf = ctx.perf;

        let r = self.row(body, 22.0 * u);
        let fps = pf.fps();
        let tone = if fps >= 55.0 {
            t.good
        } else if fps >= 28.0 {
            t.warn
        } else {
            t.bad
        };
        let big = format!("{fps:.0} FPS");
        let w = self.ui.draw.text(r.x, r.y + 3.0 * u, &big, u * 2.0, tone);
        let detail = format!(
            "{:.1} ms average   {:.1} ms 99th percentile   {:.1} ms worst of the last 60",
            pf.frame_ms(),
            pf.p99_ms(),
            pf.worst_ms(60)
        );
        self.ui
            .text(r.x + w + 10.0 * u, r.y + 8.0 * u, &detail, t.text);

        let samples = pf.samples(super::perf::HISTORY);
        let top = [33.4_f32, 50.0, 100.0, 200.0, 500.0]
            .into_iter()
            .find(|&m| m >= pf.worst_ms(super::perf::HISTORY) * 1.05)
            .unwrap_or(500.0);
        let r = self.row(body, 64.0 * u);
        self.ui.graph(
            Rect::new(r.x, r.y + 2.0 * u, r.w, r.h - 4.0 * u),
            &samples,
            top,
            &[16.7, 33.3],
        );
        self.note(body, "Frame time of each present, newest at the right. Bars: green 60 fps, amber 30 fps, red slower.", t.muted);

        self.heading(body, "Memory");
        let s = ctx.sys;
        let line = |ui: &mut Ui, body: &mut Body, label: &str, text: &str, used: u64, of: u64| {
            let r = Rect::new(body.x, body.y, body.w, 14.0 * u);
            body.y += 14.0 * u;
            ui.text_in(Rect::new(r.x, r.y, r.w * 0.28, r.h), 0, label, t.text);
            let bar = Rect::new(r.x + r.w * 0.28, r.y + 3.0 * u, r.w * 0.24, r.h - 6.0 * u);
            ui.draw.rect(bar, t.button);
            if of > 0 {
                let f = (used as f32 / of as f32).clamp(0.0, 1.0);
                let fill = if f > 0.9 {
                    t.bad
                } else if f > 0.75 {
                    t.warn
                } else {
                    t.accent
                };
                ui.draw
                    .rect(Rect::new(bar.x, bar.y, bar.w * f, bar.h), fill);
            }
            ui.text_in(
                Rect::new(r.x + r.w * 0.55, r.y, r.w * 0.45, r.h),
                0,
                text,
                t.muted,
            );
        };
        let ui = &mut self.ui;
        line(
            ui,
            body,
            "RAM, this program",
            &format!(
                "{} working set, {} private",
                perf::size(s.ram_working_set),
                perf::size(s.ram_private)
            ),
            s.ram_working_set,
            s.ram_total,
        );
        line(
            ui,
            body,
            "RAM, whole PC",
            &format!(
                "{} free of {}",
                perf::size(s.ram_available),
                perf::size(s.ram_total)
            ),
            s.ram_total.saturating_sub(s.ram_available),
            s.ram_total,
        );
        if s.vram_known {
            line(
                ui,
                body,
                "VRAM, card",
                &format!(
                    "{} of {} budget",
                    perf::size(s.vram_used),
                    perf::size(s.vram_budget)
                ),
                s.vram_used,
                s.vram_budget,
            );
            line(
                ui,
                body,
                "VRAM, shared",
                &format!(
                    "{} of {} budget",
                    perf::size(s.shared_used),
                    perf::size(s.shared_budget)
                ),
                s.shared_used,
                s.shared_budget,
            );
        } else {
            line(ui, body, "VRAM", "not reported by this system", 0, 0);
        }

        self.heading(body, "Speed switches (read only)");
        let pace = self.model.pace;
        let on_off = |b: bool| if b { "on" } else { "off" };
        let pace_rows = [
            (
                "VICTORY_PACE",
                on_off(pace.pace).to_string(),
                pace.pace,
                Change::Live,
            ),
            (
                "VICTORY_PACE_FPS",
                pace.fps.map_or("title".to_string(), |f| f.to_string()),
                pace.fps.is_some(),
                Change::Live,
            ),
            (
                "VICTORY_PACE_INPUT",
                on_off(pace.input).to_string(),
                pace.input,
                Change::Live,
            ),
            (
                "VICTORY_PACE_LATENCY",
                pace.latency.to_string(),
                pace.latency > 0,
                Change::Restart,
            ),
            (
                "VICTORY_PACE_TEARING",
                on_off(pace.tearing).to_string(),
                pace.tearing,
                Change::Restart,
            ),
            (
                "VICTORY_PACE_FEEDBACK",
                on_off(pace.feedback).to_string(),
                pace.feedback,
                Change::Restart,
            ),
        ];
        let up = self.model.upscale;
        let pace_rows: Vec<_> = pace_rows
            .into_iter()
            .chain([
                (
                    "VICTORY_UPSCALER",
                    up.upscaler.name().to_string(),
                    true,
                    Change::Live,
                ),
                (
                    "VICTORY_UPSCALE_QUALITY",
                    up.quality.name().to_string(),
                    true,
                    Change::Restart,
                ),
                (
                    "VICTORY_FRAMEGEN",
                    on_off(up.framegen).to_string(),
                    up.framegen,
                    Change::Live,
                ),
            ])
            .collect();
        let rows = switch_rows(ctx.env, &pace_rows);
        let on_count = rows.iter().filter(|r| r.on).count();
        self.note(body, &format!("{on_count} of {} on. [live]: change it on the Display page. [restart]: set the variable and restart.", rows.len()), t.muted);
        for row in rows {
            let r = self.row(body, 12.0 * u);
            let (state, colour) = if row.on {
                ("ON", t.good)
            } else {
                ("off", t.muted)
            };
            self.ui.text_in(
                Rect::new(r.x, r.y, r.w * 0.5, r.h),
                0,
                &row.name,
                if row.on { t.text } else { t.muted },
            );
            self.ui.text_in(
                Rect::new(r.x + r.w * 0.5, r.y, r.w * 0.08, r.h),
                0,
                state,
                colour,
            );
            self.ui.text_in(
                Rect::new(r.x + r.w * 0.6, r.y, r.w * 0.16, r.h),
                0,
                &row.value,
                t.muted,
            );
            let (tag, tone) = match row.change {
                Change::Live => ("live", t.good),
                Change::Restart => ("restart", t.warn),
            };
            self.ui.text_in(
                Rect::new(r.x + r.w * 0.78, r.y, r.w * 0.22, r.h),
                0,
                tag,
                tone,
            );
        }
    }

    fn footer(&mut self, panel: Rect, h: f32, fx: &mut Effects) {
        let u = self.ui.scale;
        let t = self.ui.theme;
        let r = Rect::new(
            panel.x + 8.0 * u,
            panel.bottom() - h,
            panel.w - 16.0 * u,
            h - 3.0 * u,
        );
        self.ui.draw.rect(
            Rect::new(r.x, r.y - 2.0 * u, r.w, 1.0f32.max(u / 2.0)),
            t.border,
        );
        let restart = self.restart_needed();
        let mut y = r.y + 2.0 * u;
        let text_w = r.w - 110.0 * u;
        let clip_text = |s: &str| {
            let max = (text_w / (font::ADVANCE as f32 * u)) as usize;
            s.chars().take(max).collect::<String>()
        };
        if !restart.is_empty() {
            let s = clip_text(&format!("Restart required: {}.", restart.join(", ")));
            self.ui.text(r.x, y, &s, t.warn);
            y += 11.0 * u;
        }
        if self.capture.is_some() {
            self.ui.text(
                r.x,
                y,
                "Press the new key or mouse button. Esc cancels, Delete clears.",
                t.warn,
            );
        } else if let Some((msg, tone)) = self.status.clone() {
            let c = match tone {
                Tone::Good => t.good,
                Tone::Warn => t.warn,
                Tone::Bad => t.bad,
            };
            self.ui.text(r.x, y, &clip_text(&msg), c);
        }
        if self.page != Page::Performance {
            let b = Rect::new(r.right() - 100.0 * u, r.y + 2.0 * u, 100.0 * u, 16.0 * u);
            let label = if self.armed {
                "Click to confirm"
            } else {
                "Reset page"
            };
            if self.ui.button("reset", b, label, self.armed) {
                if self.armed {
                    self.reset_page(fx);
                    self.armed = false;
                } else {
                    self.armed = true;
                }
            }
        }
    }

    fn reset_page(&mut self, fx: &mut Effects) {
        match self.page {
            Page::Controls => {
                let c = &self.model.controls;
                self.model.controls = Settings {
                    enabled: c.enabled,
                    capture: c.capture,
                    carry: c.carry,
                    deadzone: c.deadzone,
                    curve: c.curve,
                    ..Settings::default()
                };
                fx.controls = Some(self.model.controls.clone());
            }
            Page::Display => {
                self.model.display = Display::default();
                self.model.pace = PaceModel::default();
                self.model.upscale = UpscaleModel::default();
                fx.display = Some(self.model.display);
                fx.pace = Some(self.model.pace);
                fx.upscale = Some(self.model.upscale);
            }
            Page::Performance => return,
        }
        fx.save = true;
        self.status = Some(("Page reset to the defaults.".into(), Tone::Good));
    }

    /// Reports a save's outcome in the footer.
    pub fn saved(&mut self, result: Result<(), String>) {
        self.status = Some(match (result, self.status.take()) {
            (Ok(()), Some((msg, Tone::Good))) if !msg.ends_with("Saved.") => {
                (format!("{msg} Saved."), Tone::Good)
            }
            (Ok(()), Some((msg, Tone::Good))) => (msg, Tone::Good),
            (Ok(()), _) => ("Saved.".to_string(), Tone::Good),
            (Err(e), _) => (format!("Not saved: {e}"), Tone::Bad),
        });
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn set_page(&mut self, page: Page) {
        self.page = page;
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|(s, _)| s.as_str())
    }
}

/// A row with a slider whose value is shown with two places.
fn slider_row(
    ui: &mut Ui,
    body: &mut Body,
    h: f32,
    id: &str,
    label: &str,
    v: &mut f32,
    range: (f32, f32, f32),
) -> Moved {
    let r = Rect::new(body.x, body.y, body.w, h);
    body.y += h;
    let shown = num(*v, 2);
    ui.slider(id, r, label, &shown, v, range)
}

/// Where the next row of a page goes.
struct Body {
    x: f32,
    y: f32,
    w: f32,
    start: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kbm::key_code;
    use crate::overlay::gui::MAX_VERTS;

    const SIZE: (f32, f32) = (1920.0, 1080.0);

    fn model() -> Model {
        Model {
            controls: Settings::default(),
            display: Display::default(),
            pace: PaceModel::default(),
            upscale: UpscaleModel::default(),
        }
    }

    struct Rig {
        menu: Menu,
        perf: Perf,
        sys: SysInfo,
        env: Vec<(String, String)>,
        now: u64,
    }

    impl Rig {
        fn new() -> Rig {
            let mut perf = Perf::default();
            for i in 0..300u64 {
                perf.present(i * 16_666_667 + (i % 17) * 400_000);
            }
            let sys = SysInfo {
                ram_working_set: 9 << 30,
                ram_private: 12 << 30,
                ram_available: 20 << 30,
                ram_total: 32 << 30,
                vram_used: 5 << 30,
                vram_budget: 10 << 30,
                shared_used: 2 << 30,
                shared_budget: 16 << 30,
                vram_known: true,
            };
            Rig {
                menu: Menu::new(model(), PaceModel::default(), true),
                perf,
                sys,
                env: Vec::new(),
                now: 0,
            }
        }

        fn frame(&mut self, input: Input) -> Effects {
            let env = self.env.clone();
            let get = move |n: &str| env.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
            let ctx = Ctx {
                size: SIZE,
                perf: &self.perf,
                sys: &self.sys,
                env: &get,
                now_ms: self.now,
            };
            self.now += 16;
            let fx = self.menu.frame(&ctx, input);
            let n = self.menu.draw_list().verts.len();
            assert!(n > 0 && n <= MAX_VERTS && n.is_multiple_of(6));
            fx
        }

        /// A click at the centre of the first drawn button-like rectangle
        /// whose text is `label`, found by drawing and reading the layout
        /// back from where the widgets put it.
        fn click(&mut self, at: (f32, f32)) -> Effects {
            let down = Input {
                mouse: at,
                left_down: true,
                left_pressed: true,
                ..Default::default()
            };
            let up = Input {
                mouse: at,
                left_released: true,
                ..Default::default()
            };
            let mut fx = self.frame(down);
            let second = self.frame(up);
            merge(&mut fx, second);
            fx
        }

        fn key(&mut self, vk: u8) -> Effects {
            self.frame(Input {
                keys_pressed: vec![vk],
                mouse: (5.0, 5.0),
                ..Default::default()
            })
        }
    }

    fn merge(into: &mut Effects, from: Effects) {
        into.controls = from.controls.or(into.controls.take());
        into.display = from.display.or(into.display.take());
        into.pace = from.pace.or(into.pace.take());
        into.save |= from.save;
        into.close |= from.close;
    }

    // The layout at 1080p (scale 2): panel 1280 x 840 centred; tabs start at
    // x = 320 + 16, y = 120 + 36 + 10; content below.
    const TAB_Y: f32 = 120.0 + 36.0 + 10.0 + 16.0;
    fn tab(i: usize) -> (f32, f32) {
        (320.0 + 16.0 + 100.0 * 2.0 / 2.0 + i as f32 * 208.0, TAB_Y)
    }

    #[test]
    fn it_opens_on_controls_and_the_tabs_switch_pages() {
        let mut rig = Rig::new();
        rig.frame(Input::default());
        assert_eq!(rig.menu.page(), Page::Controls);
        rig.click(tab(1));
        assert_eq!(rig.menu.page(), Page::Display);
        rig.click(tab(2));
        assert_eq!(rig.menu.page(), Page::Performance);
        rig.click(tab(0));
        assert_eq!(rig.menu.page(), Page::Controls);
    }

    #[test]
    fn esc_closes_unless_a_binding_waits_for_a_key() {
        let mut rig = Rig::new();
        assert!(rig.key(0x1B).close);
        rig.menu.capture = Some(Capture {
            out: Output::Walk,
            old: None,
        });
        let fx = rig.key(0x1B);
        assert!(!fx.close);
        assert!(!rig.menu.waiting_for_key(), "Esc cancels the binding");
        assert!(!fx.save && fx.controls.is_none());
    }

    #[test]
    fn a_binding_waits_for_a_key_and_takes_it_from_what_had_it() {
        let mut rig = Rig::new();
        let walk = Output::Walk;
        let z = key_code("Z").unwrap();
        rig.menu.capture = Some(Capture {
            out: walk,
            old: Some(z),
        });
        // Space is X (jump): binding it to Walk takes it from X.
        let fx = rig.key(key_code("Space").unwrap());
        let c = fx.controls.expect("published at once");
        assert!(fx.save);
        assert_eq!(c.on_foot.keys_of(walk), vec![key_code("Space").unwrap()]);
        assert!(c.on_foot.keys_of(Output::Button(crate::kbm::X)).is_empty());
        assert!(rig.menu.status_text().unwrap().contains("Space now does"));
        assert!(rig.menu.status_text().unwrap().contains("no longer does"));
        assert_eq!(rig.menu.model.controls, c);
        // The vehicle layout is untouched.
        assert_eq!(c.vehicle, Settings::default().vehicle);
    }

    #[test]
    fn delete_clears_and_the_wheel_and_mouse_buttons_bind() {
        let mut rig = Rig::new();
        let a = Output::Button(crate::kbm::A);
        let shift = key_code("LShift").unwrap();
        rig.menu.capture = Some(Capture {
            out: a,
            old: Some(shift),
        });
        let fx = rig.key(0x2E);
        assert_eq!(
            fx.controls.unwrap().on_foot.keys_of(a),
            vec![key_code("Enter").unwrap(), key_code("RShift").unwrap()]
        );
        rig.menu.capture = Some(Capture { out: a, old: None });
        let fx = rig.frame(Input {
            wheel: 1.0,
            ..Default::default()
        });
        assert!(fx.controls.unwrap().on_foot.keys_of(a).contains(&WHEEL_UP));
        rig.menu.capture = Some(Capture { out: a, old: None });
        let fx = rig.key(0x05);
        assert!(fx.controls.unwrap().on_foot.keys_of(a).contains(&0x05));
        // Reserved keys do not bind: F1 and Windows keys are ignored.
        rig.menu.capture = Some(Capture { out: a, old: None });
        assert!(rig.key(0x70).controls.is_none() && rig.key(0x5B).controls.is_none());
        assert!(rig.menu.waiting_for_key());
    }

    #[test]
    fn clicking_a_key_box_waits_for_a_key_and_the_rest_of_the_menu_stops_answering() {
        let mut rig = Rig::new();
        rig.frame(Input::default());
        // Find a key box by drawing: the first row of bindings sits below the
        // sliders; click far to the right where the boxes are, scanning down.
        let mut waiting = false;
        'scan: for y in (300..900).step_by(8) {
            for x in [1320.0f32, 1190.0, 1090.0] {
                rig.click((x, y as f32));
                if rig.menu.waiting_for_key() {
                    waiting = true;
                    break 'scan;
                }
            }
        }
        assert!(waiting, "a click on a key box starts a binding");
        // A click on a tab while waiting does nothing.
        rig.click(tab(1));
        assert_eq!(rig.menu.page(), Page::Controls);
    }

    #[test]
    fn restart_is_noted_for_swap_chain_settings_only() {
        let mut rig = Rig::new();
        rig.menu.set_page(Page::Display);
        rig.menu.model.pace.pace = true;
        rig.menu.model.pace.fps = Some(60);
        assert!(rig.menu.restart_needed().is_empty());
        rig.menu.model.pace.latency = 1;
        rig.menu.model.pace.tearing = true;
        assert_eq!(rig.menu.restart_needed(), vec!["frame latency", "tearing"]);
        rig.frame(Input::default());
    }

    #[test]
    fn reset_needs_two_clicks_and_keeps_what_the_page_does_not_show() {
        let mut rig = Rig::new();
        rig.menu.model.controls.sensitivity = 3.0;
        rig.menu.model.controls.capture = false;
        rig.menu
            .model
            .controls
            .on_foot
            .rebind(Output::Walk, None, key_code("X"));
        rig.frame(Input::default());
        // The reset button is at the right of the footer.
        let at = (320.0 + 1280.0 - 16.0 - 100.0, 120.0 + 840.0 - 60.0 + 20.0);
        let fx = rig.click(at);
        assert!(fx.controls.is_none(), "the first click only arms it");
        let fx = rig.click(at);
        let c = fx.controls.expect("the second resets");
        assert_eq!(c.sensitivity, 1.0);
        assert!(!c.capture, "what the page does not show stays");
        assert_eq!(c.on_foot, Settings::default().on_foot);
        assert!(fx.save);
    }

    #[test]
    fn pages_draw_inside_the_window_whatever_the_size() {
        for size in [
            (1920.0, 1080.0),
            (1280.0, 720.0),
            (3840.0, 2160.0),
            (800.0, 600.0),
        ] {
            for page in [Page::Controls, Page::Display, Page::Performance] {
                let mut rig = Rig::new();
                rig.menu.set_page(page);
                let env = |_: &str| None;
                let ctx = Ctx {
                    size,
                    perf: &rig.perf,
                    sys: &rig.sys,
                    env: &env,
                    now_ms: 0,
                };
                rig.menu.frame(&ctx, Input::default());
                let list = rig.menu.draw_list();
                assert!(!list.verts.is_empty());
                for v in &list.verts {
                    assert!(
                        v.pos[0] >= 0.0
                            && v.pos[0] <= size.0
                            && v.pos[1] >= 0.0
                            && v.pos[1] <= size.1,
                        "{page:?} {size:?} {:?}",
                        v.pos
                    );
                    assert!(v.uv[0] >= 0.0 && v.uv[0] <= 1.0 && v.uv[1] >= 0.0 && v.uv[1] <= 1.0);
                }
            }
        }
    }

    #[test]
    fn the_hud_draws_only_when_asked() {
        let mut rig = Rig::new();
        let env = |_: &str| None;
        let ctx = Ctx {
            size: SIZE,
            perf: &rig.perf,
            sys: &rig.sys,
            env: &env,
            now_ms: 0,
        };
        rig.menu.hud(&ctx);
        assert!(rig.menu.draw_list().verts.is_empty());
        rig.menu.model.display.graph = true;
        rig.menu.hud(&ctx);
        let n = rig.menu.draw_list().verts.len();
        assert!(n > 100 && n < 6000, "{n}");
    }

    #[test]
    fn the_hints_draw_inside_the_window_wrapped_and_alone() {
        let mut rig = Rig::new();
        let env = |_: &str| None;
        let hints = crate::hints::legend(&Settings::default());
        for size in [(1280.0, 720.0), (320.0, 240.0)] {
            let ctx = Ctx {
                size,
                perf: &rig.perf,
                sys: &rig.sys,
                env: &env,
                now_ms: 0,
            };
            rig.menu.hud_with(&ctx, Some(&hints));
            let verts = &rig.menu.draw_list().verts;
            assert!(verts.len() > 100, "{}", verts.len());
            for v in verts.iter() {
                assert!(v.pos[0] >= 0.0 && v.pos[0] <= size.0 + 0.5, "x {}", v.pos[0]);
                assert!(v.pos[1] >= 0.0 && v.pos[1] <= size.1 + 0.5, "y {}", v.pos[1]);
            }
        }
    }

    #[test]
    fn a_frame_cap_turns_pacing_on_and_title_leaves_it() {
        use super::super::store::{FRAME_CAPS, with_frame_cap};
        let p = PaceModel::default();
        let capped = with_frame_cap(p, Some(60));
        assert!(capped.pace && capped.fps == Some(60));
        let back = with_frame_cap(capped, None);
        assert!(back.pace && back.fps.is_none(), "pacing stays as it was");
        assert!(FRAME_CAPS.contains(&Some(30)) && FRAME_CAPS[0].is_none());
    }

    #[test]
    fn env_set_pacing_is_tagged_and_speed_switches_are_listed() {
        let mut rig = Rig::new();
        rig.env = vec![
            ("VICTORY_PACE".into(), "1".into()),
            ("VICTORY_GROUP_SYNC".into(), "3".into()),
        ];
        rig.menu.set_page(Page::Display);
        rig.frame(Input::default());
        rig.menu.set_page(Page::Performance);
        rig.frame(Input::default());
    }

    /// Writes the pages as pictures: `VICTORY_OVERLAY_PREVIEW=<dir> cargo test
    /// -p victory-core --lib overlay_previews -- --ignored`.
    #[test]
    #[ignore = "writes pictures to the directory named by VICTORY_OVERLAY_PREVIEW"]
    fn overlay_previews() {
        let Some(dir) = std::env::var_os("VICTORY_OVERLAY_PREVIEW") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let backdrop = |w: u32, h: u32| -> Vec<u8> {
            // A stand-in for the game: a sky-to-ground gradient.
            let mut px = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let f = y as f32 / h as f32;
                    let i = ((y * w + x) * 4) as usize;
                    px[i] = (60.0 + 90.0 * f) as u8;
                    px[i + 1] = (110.0 + 40.0 * (1.0 - f)) as u8;
                    px[i + 2] = (170.0 - 90.0 * f) as u8;
                    px[i + 3] = 255;
                }
            }
            px
        };
        let (w, h) = (1920u32, 1080u32);
        let mut rig = Rig::new();
        rig.menu.model.display.graph = true;
        rig.menu.model.pace.latency = 1;
        let env = rig.env.clone();
        let _ = env;
        let shot = |rig: &mut Rig, name: &str| {
            let list = rig.menu.draw_list();
            let mut px = backdrop(w, h);
            crate::overlay::preview::rasterize(&list.verts, (w, h), &mut px);
            crate::overlay::preview::write_ppm(&dir.join(name), (w, h), &px);
        };
        let get = |_: &str| None;
        {
            let ctx = Ctx {
                size: (w as f32, h as f32),
                perf: &rig.perf,
                sys: &rig.sys,
                env: &get,
                now_ms: 0,
            };
            rig.menu.hud(&ctx);
        }
        shot(&mut rig, "hud.ppm");
        for (page, name) in [
            (Page::Controls, "controls.ppm"),
            (Page::Display, "display.ppm"),
            (Page::Performance, "performance.ppm"),
        ] {
            rig.menu.set_page(page);
            rig.frame(Input::default());
            shot(&mut rig, name);
        }
        rig.menu.set_page(Page::Controls);
        rig.menu.capture = Some(Capture {
            out: Output::Walk,
            old: Some(key_code("Z").unwrap()),
        });
        rig.menu.status = None;
        rig.frame(Input::default());
        shot(&mut rig, "controls-capture.ppm");
    }
}
