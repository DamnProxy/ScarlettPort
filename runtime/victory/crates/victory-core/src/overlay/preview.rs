//! A software copy of the overlay's shader, for tests: draws a vertex list
//! onto an RGBA picture the way the GPU does (the font atlas sampled with
//! points, the premultiplied colour scaled by it, blended over what is
//! there), so the pages can be looked at without a GPU.

use std::path::Path;

use super::font;
use super::gui::Vertex;

/// Draws `verts` (quads of six vertices, as `DrawList` makes them) over
/// `px` (`size.0` by `size.1`, RGBA, opaque).
pub fn rasterize(verts: &[Vertex], size: (u32, u32), px: &mut [u8]) {
    let atlas = font::atlas();
    for quad in verts.chunks_exact(6) {
        let (a, c) = (quad[0], quad[2]);
        let (x0, y0, x1, y1) = (a.pos[0], a.pos[1], c.pos[0], c.pos[1]);
        let color = a.color;
        let src = [
            color & 0xFF,
            color >> 8 & 0xFF,
            color >> 16 & 0xFF,
            color >> 24,
        ]
        .map(|b| b as f32 / 255.0);
        for y in (y0.floor() as u32)..(y1.ceil() as u32).min(size.1) {
            for x in (x0.floor() as u32)..(x1.ceil() as u32).min(size.0) {
                let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                if cx < x0 || cx >= x1 || cy < y0 || cy >= y1 {
                    continue;
                }
                let u = a.uv[0] + (cx - x0) / (x1 - x0) * (c.uv[0] - a.uv[0]);
                let v = a.uv[1] + (cy - y0) / (y1 - y0) * (c.uv[1] - a.uv[1]);
                let tx = ((u * font::ATLAS_W as f32) as u32).min(font::ATLAS_W - 1);
                let ty = ((v * font::ATLAS_H as f32) as u32).min(font::ATLAS_H - 1);
                let cover = atlas[(ty * font::ATLAS_W + tx) as usize] as f32 / 255.0;
                let out = src.map(|s| s * cover);
                let i = ((y * size.0 + x) * 4) as usize;
                for k in 0..3 {
                    let d = px[i + k] as f32 / 255.0;
                    px[i + k] =
                        ((out[k] + d * (1.0 - out[3])).clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
        }
    }
}

/// Writes `px` (RGBA) as a binary PPM.
pub fn write_ppm(path: &Path, size: (u32, u32), px: &[u8]) {
    let mut out = format!("P6\n{} {}\n255\n", size.0, size.1).into_bytes();
    for p in px.chunks_exact(4) {
        out.extend_from_slice(&p[..3]);
    }
    std::fs::write(path, out).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::gui::{Color, DrawList, Rect};

    #[test]
    fn text_and_rectangles_land_where_they_were_drawn() {
        let mut d = DrawList::default();
        d.clear(Rect::new(0.0, 0.0, 64.0, 32.0));
        d.rect(Rect::new(0.0, 0.0, 10.0, 10.0), Color(255, 0, 0, 255));
        d.text(20.0, 4.0, "I", 2.0, Color(255, 255, 255, 255));
        let mut px = vec![0u8; 64 * 32 * 4];
        for p in px.chunks_exact_mut(4) {
            p[3] = 255;
        }
        rasterize(&d.verts, (64, 32), &mut px);
        let at = |x: usize, y: usize| &px[(y * 64 + x) * 4..(y * 64 + x) * 4 + 3];
        assert_eq!(at(3, 3), &[255, 0, 0]);
        assert_eq!(at(30, 20), &[0, 0, 0]);
        // The stem of an `I` is its middle column: glyph column 2 of 5, scale 2.
        assert_eq!(at(20 + 4, 4 + 2 * 2), &[255, 255, 255]);
        assert_eq!(at(20 + 1, 4 + 2 * 2), &[0, 0, 0]);
    }
}
