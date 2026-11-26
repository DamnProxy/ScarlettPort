//! A small immediate-mode GUI: widgets are calls made every frame that draw
//! and answer at once, and the result is a list of textured triangles for
//! the renderer in `victory-gpu` (the font atlas of [`super::font`] is the
//! only texture; rectangles sample its solid cell).
//!
//! Pure data and arithmetic, so the menu is tested (and its pictures drawn
//! by `tools` in the tests) without a GPU.

use std::collections::HashMap;

use super::font;

/// Most vertices one frame may hold; a list stops growing at it.
pub const MAX_VERTS: usize = 49_152;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(
            self.x + d,
            self.y + d,
            (self.w - 2.0 * d).max(0.0),
            (self.h - 2.0 * d).max(0.0),
        )
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = (self.right().min(o.right()), self.bottom().min(o.bottom()));
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

/// Straight (not premultiplied) RGBA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8, pub u8);

impl Color {
    pub fn with_alpha(self, a: u8) -> Color {
        Color(self.0, self.1, self.2, a)
    }

    /// Premultiplied, as four bytes in memory order R, G, B, A.
    pub fn packed(self) -> u32 {
        let a = u32::from(self.3);
        let p = |c: u8| (u32::from(c) * a + 127) / 255;
        p(self.0) | p(self.1) << 8 | p(self.2) << 16 | a << 24
    }

    pub fn mix(self, o: Color, t: f32) -> Color {
        let m = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Color(
            m(self.0, o.0),
            m(self.1, o.1),
            m(self.2, o.2),
            m(self.3, o.3),
        )
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: u32,
}

const _: () = assert!(size_of::<Vertex>() == 20);

/// Triangles in pixels of the window (y down), clipped on the CPU to the
/// current clip rectangle (scroll areas), six vertices a quad.
pub struct DrawList {
    pub verts: Vec<Vertex>,
    clip: Rect,
}

impl Default for DrawList {
    fn default() -> DrawList {
        DrawList {
            verts: Vec::new(),
            clip: Rect::new(0.0, 0.0, f32::MAX / 4.0, f32::MAX / 4.0),
        }
    }
}

impl DrawList {
    pub fn clear(&mut self, clip: Rect) {
        self.verts.clear();
        self.clip = clip;
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    pub fn set_clip(&mut self, clip: Rect) -> Rect {
        std::mem::replace(&mut self.clip, clip)
    }

    /// A quad of `r` showing `uv` (texels of the atlas).
    fn quad(&mut self, r: Rect, uv: Rect, c: Color) {
        if c.3 == 0 || r.w <= 0.0 || r.h <= 0.0 || self.verts.len() + 6 > MAX_VERTS {
            return;
        }
        let Some(v) = r.intersect(&self.clip) else {
            return;
        };
        let (fx0, fy0) = ((v.x - r.x) / r.w, (v.y - r.y) / r.h);
        let (fx1, fy1) = ((v.right() - r.x) / r.w, (v.bottom() - r.y) / r.h);
        let (aw, ah) = (font::ATLAS_W as f32, font::ATLAS_H as f32);
        let (u0, u1) = ((uv.x + uv.w * fx0) / aw, (uv.x + uv.w * fx1) / aw);
        let (v0, v1) = ((uv.y + uv.h * fy0) / ah, (uv.y + uv.h * fy1) / ah);
        let color = c.packed();
        let p = |x: f32, y: f32, u: f32, w: f32| Vertex {
            pos: [x, y],
            uv: [u, w],
            color,
        };
        let (a, b, c2, d) = (
            p(v.x, v.y, u0, v0),
            p(v.right(), v.y, u1, v0),
            p(v.right(), v.bottom(), u1, v1),
            p(v.x, v.bottom(), u0, v1),
        );
        self.verts.extend_from_slice(&[a, b, c2, a, c2, d]);
    }

    pub fn rect(&mut self, r: Rect, c: Color) {
        let (sx, sy) = font::solid_cell();
        self.quad(r, Rect::new(sx as f32 + 2.0, sy as f32 + 2.0, 4.0, 4.0), c);
    }

    /// The border of `r`, `t` pixels thick, inside it.
    pub fn outline(&mut self, r: Rect, t: f32, c: Color) {
        self.rect(Rect::new(r.x, r.y, r.w, t), c);
        self.rect(Rect::new(r.x, r.bottom() - t, r.w, t), c);
        self.rect(Rect::new(r.x, r.y + t, t, r.h - 2.0 * t), c);
        self.rect(Rect::new(r.right() - t, r.y + t, t, r.h - 2.0 * t), c);
    }

    /// `text` with its top left at `x`, `y`, `scale` pixels to a font
    /// pixel (whole numbers keep it sharp). Returns its width.
    pub fn text(&mut self, x: f32, y: f32, text: &str, scale: f32, c: Color) -> f32 {
        let mut pen = x;
        for ch in text.chars() {
            if ch != ' ' {
                let (cx, cy) = font::cell_of(ch);
                self.quad(
                    Rect::new(pen, y, 5.0 * scale, 8.0 * scale),
                    Rect::new(cx as f32, cy as f32, 5.0, 8.0),
                    c,
                );
            }
            pen += font::ADVANCE as f32 * scale;
        }
        pen - x
    }
}

/// What the user did since the last frame.
#[derive(Clone, Debug, Default)]
pub struct Input {
    /// The pointer, in pixels of the window.
    pub mouse: (f32, f32),
    pub left_down: bool,
    pub left_pressed: bool,
    pub left_released: bool,
    pub right_pressed: bool,
    /// Wheel steps, up positive.
    pub wheel: f32,
    /// Keys (virtual key codes; mouse buttons are keys 1, 2, 4, 5, 6) that
    /// went down this frame.
    pub keys_pressed: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub dim: Color,
    pub panel: Color,
    pub bar: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub button: Color,
    pub hover: Color,
    pub pressed: Color,
    pub accent: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            dim: Color(0, 0, 0, 120),
            panel: Color(18, 20, 26, 240),
            bar: Color(30, 34, 44, 255),
            border: Color(70, 78, 98, 255),
            text: Color(232, 236, 244, 255),
            muted: Color(140, 148, 166, 255),
            button: Color(44, 50, 64, 255),
            hover: Color(62, 72, 94, 255),
            pressed: Color(36, 120, 200, 255),
            accent: Color(36, 120, 200, 255),
            good: Color(96, 200, 120, 255),
            warn: Color(240, 180, 60, 255),
            bad: Color(236, 90, 80, 255),
        }
    }
}

/// How a slider answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Moved {
    /// The value changed this frame.
    pub changed: bool,
    /// The user let go of it: the change is final.
    pub committed: bool,
}

fn hash(label: &str) -> u64 {
    label.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Widgets and the state they keep between frames.
pub struct Ui {
    pub draw: DrawList,
    pub theme: Theme,
    /// Pixels to a font pixel.
    pub scale: f32,
    pub input: Input,
    /// Nothing answers the pointer (a key is being waited for).
    pub blocked: bool,
    active: Option<u64>,
    scroll: HashMap<u64, f32>,
}

impl Default for Ui {
    fn default() -> Ui {
        Ui {
            draw: DrawList::default(),
            theme: Theme::default(),
            scale: 1.0,
            input: Input::default(),
            blocked: false,
            active: None,
            scroll: HashMap::new(),
        }
    }
}

impl Ui {
    /// Starts a frame on a window of `size` pixels.
    pub fn begin(&mut self, input: Input, scale: f32, size: (f32, f32)) {
        self.input = input;
        self.scale = scale;
        self.blocked = false;
        self.draw.clear(Rect::new(0.0, 0.0, size.0, size.1));
        if !self.input.left_down && !self.input.left_released {
            self.active = None;
        }
    }

    /// A length of `n` font pixels.
    pub fn u(&self, n: f32) -> f32 {
        n * self.scale
    }

    pub fn hover(&self, r: Rect) -> bool {
        !self.blocked && r.contains(self.input.mouse) && self.draw.clip().contains(self.input.mouse)
    }

    pub fn text(&mut self, x: f32, y: f32, text: &str, c: Color) -> f32 {
        let s = self.scale;
        self.draw.text(x, y, text, s, c)
    }

    /// `text` vertically centred in `r`, from its left (`align` 0), centre
    /// (1) or right (2) edge.
    pub fn text_in(&mut self, r: Rect, align: u8, text: &str, c: Color) {
        let w = font::text_width(text, self.scale);
        let x = match align {
            0 => r.x,
            1 => r.x + (r.w - w) / 2.0,
            _ => r.right() - w,
        };
        let y = r.y + (r.h - 8.0 * self.scale) / 2.0;
        self.text(x.floor(), y.floor(), text, c);
    }

    /// A button; true when it was clicked (pressed and released on it).
    pub fn button(&mut self, id: &str, r: Rect, label: &str, selected: bool) -> bool {
        let id = hash(id);
        let hover = self.hover(r);
        if hover && self.input.left_pressed {
            self.active = Some(id);
        }
        let down = self.active == Some(id) && self.input.left_down && hover;
        let t = self.theme;
        let fill = if selected {
            t.accent
        } else if down {
            t.pressed
        } else if hover {
            t.hover
        } else {
            t.button
        };
        self.draw.rect(r, fill);
        self.draw.outline(
            r,
            1.0f32.max((self.scale / 2.0).floor()),
            if hover { t.muted } else { t.border },
        );
        self.text_in(r, 1, label, t.text);
        let clicked = self.input.left_released && self.active == Some(id) && hover;
        if self.input.left_released && self.active == Some(id) {
            self.active = None;
        }
        clicked
    }

    /// A row with `label` and an on/off switch; true when it was flipped.
    pub fn toggle(&mut self, id: &str, r: Rect, label: &str, on: &mut bool) -> bool {
        let sw = Rect::new(
            r.right() - self.u(44.0),
            r.y + self.u(1.0),
            self.u(44.0),
            r.h - self.u(2.0),
        );
        self.text_in(
            Rect::new(r.x, r.y, r.w - self.u(50.0), r.h),
            0,
            label,
            self.theme.text,
        );
        let flipped = self.button(id, sw, if *on { "ON" } else { "OFF" }, *on);
        if flipped {
            *on = !*on;
        }
        flipped
    }

    /// A row with `label` and one button for each of `options`; true when
    /// another was chosen.
    pub fn choice(
        &mut self,
        id: &str,
        r: Rect,
        label: &str,
        options: &[&str],
        chosen: &mut usize,
    ) -> bool {
        let widths: Vec<f32> = options
            .iter()
            .map(|o| font::text_width(o, self.scale) + self.u(12.0))
            .collect();
        let total: f32 =
            widths.iter().sum::<f32>() + self.u(2.0) * options.len().saturating_sub(1) as f32;
        self.text_in(
            Rect::new(r.x, r.y, (r.w - total - self.u(6.0)).max(0.0), r.h),
            0,
            label,
            self.theme.text,
        );
        let mut x = r.right() - total;
        let mut picked = None;
        for (i, o) in options.iter().enumerate() {
            let b = Rect::new(x, r.y + self.u(1.0), widths[i], r.h - self.u(2.0));
            if self.button(&format!("{id}#{i}"), b, o, i == *chosen) {
                picked = Some(i);
            }
            x += widths[i] + self.u(2.0);
        }
        match picked {
            Some(i) if i != *chosen => {
                *chosen = i;
                true
            }
            _ => false,
        }
    }

    /// A row with `label`, a track dragged to set `value` between `lo` and
    /// `hi` (in steps of `step`) and `shown` (its text) at the right.
    pub fn slider(
        &mut self,
        id: &str,
        r: Rect,
        label: &str,
        shown: &str,
        value: &mut f32,
        (lo, hi, step): (f32, f32, f32),
    ) -> Moved {
        let id = hash(id);
        let t = self.theme;
        let text_w = self.u(54.0);
        let track = Rect::new(
            r.x + r.w * 0.45,
            r.y + (r.h - self.u(6.0)) / 2.0,
            r.w * 0.55 - text_w - self.u(8.0),
            self.u(6.0),
        );
        self.text_in(
            Rect::new(r.x, r.y, r.w * 0.45 - self.u(6.0), r.h),
            0,
            label,
            t.text,
        );
        self.text_in(
            Rect::new(r.right() - text_w, r.y, text_w, r.h),
            2,
            shown,
            t.muted,
        );
        let grab = Rect::new(track.x, r.y, track.w, r.h);
        let hover = self.hover(grab);
        if hover && self.input.left_pressed {
            self.active = Some(id);
        }
        let mut moved = Moved::default();
        if self.active == Some(id) && !self.blocked {
            if self.input.left_down {
                let f = ((self.input.mouse.0 - track.x) / track.w).clamp(0.0, 1.0);
                let mut v = lo + f * (hi - lo);
                if step > 0.0 {
                    v = (lo + ((v - lo) / step).round() * step).clamp(lo, hi);
                }
                if (v - *value).abs() > f32::EPSILON {
                    *value = v;
                    moved.changed = true;
                }
            }
            if self.input.left_released {
                moved.committed = true;
                self.active = None;
            }
        }
        let f = if hi > lo {
            ((*value - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.draw.rect(track, t.button);
        self.draw
            .rect(Rect::new(track.x, track.y, track.w * f, track.h), t.accent);
        let knob = Rect::new(
            track.x + track.w * f - self.u(2.0),
            r.y + self.u(2.0),
            self.u(4.0),
            r.h - self.u(4.0),
        );
        self.draw.rect(
            knob,
            if hover || self.active == Some(id) {
                t.text
            } else {
                t.muted
            },
        );
        moved
    }

    /// Frame times in ms as bars, newest at the right, with guide lines at
    /// `guides` ms; the top of the box is `top_ms`.
    pub fn graph(&mut self, r: Rect, samples: &[f32], top_ms: f32, guides: &[f32]) {
        let t = self.theme;
        self.draw.rect(r, t.bar);
        let n = samples.len();
        if n > 0 {
            let w = r.w / n as f32;
            for (i, &ms) in samples.iter().enumerate() {
                let h = (ms / top_ms).clamp(0.0, 1.0) * r.h;
                let colour = if ms <= 17.5 {
                    t.good
                } else if ms <= 34.0 {
                    t.warn
                } else {
                    t.bad
                };
                let x = (r.x + w * i as f32).floor();
                let right = (r.x + w * (i + 1) as f32).floor().max(x + 1.0);
                self.draw.rect(
                    Rect::new(x, r.bottom() - h.max(1.0), right - x, h.max(1.0)),
                    colour,
                );
            }
        }
        for &g in guides {
            let y = r.bottom() - (g / top_ms).clamp(0.0, 1.0) * r.h;
            self.draw
                .rect(Rect::new(r.x, y.floor(), r.w, 1.0), t.muted.with_alpha(150));
            let label = format!("{g:.1}");
            self.text(
                r.x + self.u(2.0),
                (y - self.u(9.0)).max(r.y),
                &label,
                t.muted,
            );
        }
        self.draw.outline(r, 1.0, t.border);
    }

    /// Starts a scrolling column in `r` whose content is `content_h`
    /// tall: returns the clip it replaced and the scroll offset (move
    /// content up by it). Draw the content, then call [`Ui::scroll_end`].
    pub fn scroll_begin(&mut self, id: &str, r: Rect, content_h: f32, step: f32) -> (Rect, f32) {
        let key = hash(id);
        let range = (content_h - r.h).max(0.0);
        let mut offset = *self.scroll.get(&key).unwrap_or(&0.0);
        if self.hover(r) && self.input.wheel != 0.0 {
            offset -= self.input.wheel * step;
        }
        offset = offset.clamp(0.0, range);
        self.scroll.insert(key, offset);
        let prev = self.draw.clip();
        let clipped = prev.intersect(&r).unwrap_or_default();
        self.draw.set_clip(clipped);
        (prev, offset)
    }

    /// Ends a scrolling column: puts the clip back and draws the bar.
    pub fn scroll_end(&mut self, (prev, offset): (Rect, f32), r: Rect, content_h: f32) {
        self.draw.set_clip(prev);
        if content_h > r.h + 0.5 {
            let t = self.theme;
            let track = Rect::new(r.right() - self.u(3.0), r.y, self.u(3.0), r.h);
            self.draw.rect(track, t.button);
            let thumb_h = (r.h / content_h * r.h).max(self.u(10.0));
            let y = r.y + (r.h - thumb_h) * offset / (content_h - r.h);
            self.draw
                .rect(Rect::new(track.x, y, track.w, thumb_h), t.muted);
        }
    }

    /// Sets a scroll offset (a page change goes back to the top).
    pub fn scroll_to(&mut self, id: &str, offset: f32) {
        self.scroll.insert(hash(id), offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui(input: Input) -> Ui {
        let mut ui = Ui::default();
        ui.begin(input, 2.0, (800.0, 600.0));
        ui
    }

    fn click_at(x: f32, y: f32) -> (Input, Input) {
        let at = |down, pressed, released| Input {
            mouse: (x, y),
            left_down: down,
            left_pressed: pressed,
            left_released: released,
            ..Default::default()
        };
        (at(true, true, false), at(false, false, true))
    }

    #[test]
    fn colours_are_premultiplied() {
        assert_eq!(Color(255, 128, 0, 255).packed(), 0xFF00_80FF);
        assert_eq!(Color(200, 200, 200, 0).packed(), 0);
        let half = Color(255, 255, 255, 128).packed();
        assert_eq!(half >> 24, 128);
        assert_eq!(half & 0xFF, 128);
    }

    #[test]
    fn a_quad_is_clipped_with_its_texture_coordinates() {
        let mut d = DrawList::default();
        d.clear(Rect::new(0.0, 0.0, 10.0, 10.0));
        d.text(5.0, 0.0, "H", 2.0, Color(255, 255, 255, 255));
        // The glyph is 10 wide and 16 tall at scale 2; half of it shows.
        assert_eq!(d.verts.len(), 6);
        let right = d.verts.iter().map(|v| v.pos[0]).fold(0.0, f32::max);
        assert_eq!(right, 10.0);
        let (cx, _) = font::cell_of('H');
        let u_max = d.verts.iter().map(|v| v.uv[0]).fold(0.0, f32::max);
        assert!((u_max - (cx as f32 + 2.5) / font::ATLAS_W as f32).abs() < 1e-6);
        // Wholly outside: nothing.
        let mut d = DrawList::default();
        d.clear(Rect::new(0.0, 0.0, 10.0, 10.0));
        d.rect(Rect::new(20.0, 20.0, 5.0, 5.0), Color(1, 2, 3, 255));
        assert!(d.verts.is_empty());
    }

    #[test]
    fn a_list_stops_at_its_limit() {
        let mut d = DrawList::default();
        for _ in 0..MAX_VERTS {
            d.rect(Rect::new(0.0, 0.0, 1.0, 1.0), Color(1, 1, 1, 255));
        }
        assert!(d.verts.len() <= MAX_VERTS);
        assert_eq!(d.verts.len() % 6, 0);
    }

    #[test]
    fn a_button_clicks_on_release_over_it_only() {
        let r = Rect::new(10.0, 10.0, 100.0, 30.0);
        let (down, up) = click_at(20.0, 20.0);
        let mut u = Ui::default();
        u.begin(down, 2.0, (800.0, 600.0));
        assert!(!u.button("b", r, "Go", false));
        u.begin(up.clone(), 2.0, (800.0, 600.0));
        assert!(u.button("b", r, "Go", false));
        // Pressed on it, let go elsewhere: no click.
        let (down, _) = click_at(20.0, 20.0);
        u.begin(down, 2.0, (800.0, 600.0));
        u.button("b", r, "Go", false);
        let away = Input {
            mouse: (400.0, 400.0),
            left_released: true,
            ..Default::default()
        };
        u.begin(away, 2.0, (800.0, 600.0));
        assert!(!u.button("b", r, "Go", false));
        // Blocked (waiting for a key): nothing answers.
        let (down, up) = click_at(20.0, 20.0);
        let mut u = Ui::default();
        u.begin(down, 2.0, (800.0, 600.0));
        u.blocked = true;
        u.button("b", r, "Go", false);
        u.begin(up, 2.0, (800.0, 600.0));
        u.blocked = true;
        assert!(!u.button("b", r, "Go", false));
    }

    #[test]
    fn a_toggle_flips_and_a_choice_picks() {
        let r = Rect::new(0.0, 0.0, 400.0, 30.0);
        let mut on = false;
        let (down, up) = click_at(400.0 - 20.0, 15.0);
        let mut u = Ui::default();
        u.begin(down, 2.0, (800.0, 600.0));
        assert!(!u.toggle("t", r, "Thing", &mut on));
        u.begin(up, 2.0, (800.0, 600.0));
        assert!(u.toggle("t", r, "Thing", &mut on));
        assert!(on);
        // The last of three options sits at the right edge.
        let mut chosen = 0;
        let (down, up) = click_at(400.0 - 10.0, 15.0);
        u.begin(down, 2.0, (800.0, 600.0));
        u.choice("c", r, "Mode", &["A", "B", "C"], &mut chosen);
        u.begin(up, 2.0, (800.0, 600.0));
        assert!(u.choice("c", r, "Mode", &["A", "B", "C"], &mut chosen));
        assert_eq!(chosen, 2);
    }

    #[test]
    fn a_slider_follows_the_pointer_in_steps_and_commits_on_release() {
        let r = Rect::new(0.0, 0.0, 400.0, 30.0);
        let mut v = 1.0;
        let mut u = Ui::default();
        // The track runs from 45% of the row to the value text.
        let track_x = 400.0 * 0.45;
        let track_w = 400.0 * 0.55 - 2.0 * 54.0 - 16.0;
        let press = Input {
            mouse: (track_x + track_w * 0.5, 15.0),
            left_down: true,
            left_pressed: true,
            ..Default::default()
        };
        u.begin(press, 2.0, (800.0, 600.0));
        let m = u.slider("s", r, "Sens", "x", &mut v, (0.0, 2.0, 0.1));
        assert!(!m.committed);
        assert!(m.changed || (v - 1.0).abs() < 1e-6);
        assert!((v - 1.0).abs() < 0.11, "{v}");
        let drag = Input {
            mouse: (track_x + track_w * 2.0, 15.0),
            left_down: true,
            ..Default::default()
        };
        u.begin(drag, 2.0, (800.0, 600.0));
        let m = u.slider("s", r, "Sens", "x", &mut v, (0.0, 2.0, 0.1));
        assert!(m.changed);
        assert_eq!(v, 2.0, "clamped to the end");
        let release = Input {
            mouse: (0.0, 15.0),
            left_released: true,
            ..Default::default()
        };
        u.begin(release, 2.0, (800.0, 600.0));
        let m = u.slider("s", r, "Sens", "x", &mut v, (0.0, 2.0, 0.1));
        assert!(m.committed);
    }

    #[test]
    fn a_scroll_area_scrolls_within_its_range_and_clips() {
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let mut u = ui(Input {
            mouse: (50.0, 50.0),
            wheel: -2.0,
            ..Default::default()
        });
        let (prev, off) = u.scroll_begin("s", r, 300.0, 20.0);
        assert_eq!(off, 40.0);
        u.draw
            .rect(Rect::new(0.0, 0.0, 100.0, 500.0), Color(1, 1, 1, 255));
        assert!(u.draw.verts.iter().all(|v| v.pos[1] <= 100.0));
        u.scroll_end((prev, off), r, 300.0);
        // Scrolling past the end stops at it.
        let mut u2 = Ui::default();
        for _ in 0..20 {
            u2.begin(
                Input {
                    mouse: (50.0, 50.0),
                    wheel: -5.0,
                    ..Default::default()
                },
                2.0,
                (800.0, 600.0),
            );
            let s = u2.scroll_begin("s", r, 300.0, 20.0);
            u2.scroll_end(s, r, 300.0);
        }
        u2.begin(
            Input {
                mouse: (50.0, 50.0),
                ..Default::default()
            },
            2.0,
            (800.0, 600.0),
        );
        let (_, off) = u2.scroll_begin("s", r, 300.0, 20.0);
        assert_eq!(off, 200.0);
        // The pointer elsewhere: no scrolling.
        let mut u3 = ui(Input {
            mouse: (500.0, 500.0),
            wheel: -3.0,
            ..Default::default()
        });
        assert_eq!(u3.scroll_begin("s", r, 300.0, 20.0).1, 0.0);
    }
}
