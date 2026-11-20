//! egui drawn on the CPU, for Linux (a static binary has no GL): the triangle meshes egui tessellates are
//! filled into an RGB buffer, texture and vertex colours multiplied and blended in premultiplied sRGB, like
//! egui's own GL painter does. Also writes the buffer as a PNG (`GANGLI_GUI_SCREENSHOT`, for tests).

use std::collections::HashMap;

use egui::epaint::textures::TextureFilter;
use egui::epaint::{ClippedPrimitive, ImageData, ImageDelta, Primitive, TextureId, Vertex};

struct Texture {
    w: usize,
    h: usize,
    /// Premultiplied RGBA.
    px: Vec<[u8; 4]>,
    linear: bool,
}

impl Texture {
    #[inline]
    fn texel(&self, x: isize, y: isize) -> [f32; 4] {
        let x = x.clamp(0, self.w as isize - 1) as usize;
        let y = y.clamp(0, self.h as isize - 1) as usize;
        let p = self.px[y * self.w + x];
        [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
    }

    #[inline]
    fn sample(&self, u: f32, v: f32) -> [f32; 4] {
        let (fx, fy) = (u * self.w as f32 - 0.5, v * self.h as f32 - 0.5);
        if !self.linear {
            return self.texel((fx + 0.5).floor() as isize, (fy + 0.5).floor() as isize);
        }
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let (a, b, c, d) = (self.texel(x0, y0), self.texel(x0 + 1, y0), self.texel(x0, y0 + 1), self.texel(x0 + 1, y0 + 1));
        let mut o = [0.0; 4];
        for i in 0..4 {
            let top = a[i] + (b[i] - a[i]) * tx;
            let bot = c[i] + (d[i] - c[i]) * tx;
            o[i] = top + (bot - top) * ty;
        }
        o
    }
}

pub struct Raster {
    textures: HashMap<TextureId, Texture>,
    pub w: usize,
    pub h: usize,
    /// 0x00RRGGBB per pixel.
    pub buf: Vec<u32>,
}

#[inline]
fn blend(dst: u32, s: [f32; 4]) -> u32 {
    if s[3] >= 254.5 {
        return ((s[0] as u32).min(255) << 16) | ((s[1] as u32).min(255) << 8) | (s[2] as u32).min(255);
    }
    let ia = 1.0 - s[3] / 255.0;
    let r = s[0] + ((dst >> 16) & 0xff) as f32 * ia;
    let g = s[1] + ((dst >> 8) & 0xff) as f32 * ia;
    let b = s[2] + (dst & 0xff) as f32 * ia;
    (((r + 0.5) as u32).min(255) << 16) | (((g + 0.5) as u32).min(255) << 8) | ((b + 0.5) as u32).min(255)
}

#[inline]
fn edge(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Pixels exactly on an edge belong to one of the two triangles sharing it, never both (no seams, no
/// double blending).
#[inline]
fn owns_boundary(a: (f32, f32), b: (f32, f32)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    dy > 0.0 || (dy == 0.0 && dx < 0.0)
}

impl Default for Raster {
    fn default() -> Self {
        Raster { textures: HashMap::new(), w: 0, h: 0, buf: vec![] }
    }
}

impl Raster {
    pub fn resize(&mut self, w: usize, h: usize) {
        if (w, h) != (self.w, self.h) {
            self.w = w;
            self.h = h;
            self.buf = vec![0; w * h];
        }
    }

    pub fn set_textures(&mut self, delta: &egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas.iter() {
                self.set(*id, d);
            }
        }
    }

    pub fn free_texture(&mut self, id: &TextureId) {
        self.textures.remove(id);
    }

    fn set(&mut self, id: TextureId, d: &ImageDelta) {
        let ImageData::Color(img) = &d.image;
        let (w, h) = (img.size[0], img.size[1]);
        let px: Vec<[u8; 4]> = img.pixels.iter().map(|c| c.to_array()).collect();
        let linear = d.options.magnification == TextureFilter::Linear;
        match d.pos {
            None => {
                self.textures.insert(id, Texture { w, h, px, linear });
            }
            Some([x0, y0]) => {
                let Some(t) = self.textures.get_mut(&id) else { return };
                for y in 0..h {
                    for x in 0..w {
                        let (tx, ty) = (x0 + x, y0 + y);
                        if tx < t.w && ty < t.h {
                            t.px[ty * t.w + tx] = px[y * w + x];
                        }
                    }
                }
            }
        }
    }

    pub fn paint(&mut self, prims: &[ClippedPrimitive], ppp: f32, clear: u32) {
        self.buf.fill(clear);
        for p in prims {
            let Primitive::Mesh(m) = &p.primitive else { continue };
            let c = p.clip_rect;
            let clip = (
                ((c.min.x * ppp).round().max(0.0) as usize).min(self.w),
                ((c.min.y * ppp).round().max(0.0) as usize).min(self.h),
                ((c.max.x * ppp).round().max(0.0) as usize).min(self.w),
                ((c.max.y * ppp).round().max(0.0) as usize).min(self.h),
            );
            if clip.0 >= clip.2 || clip.1 >= clip.3 {
                continue;
            }
            let tex = self.textures.remove(&m.texture_id);
            for t in m.indices.chunks_exact(3) {
                let (Some(a), Some(b), Some(cc)) = (m.vertices.get(t[0] as usize), m.vertices.get(t[1] as usize), m.vertices.get(t[2] as usize)) else { continue };
                self.triangle([a, b, cc], tex.as_ref(), clip, ppp);
            }
            if let Some(tex) = tex {
                self.textures.insert(m.texture_id, tex);
            }
        }
    }

    fn triangle(&mut self, v: [&Vertex; 3], tex: Option<&Texture>, clip: (usize, usize, usize, usize), ppp: f32) {
        let mut p = [(v[0].pos.x * ppp, v[0].pos.y * ppp), (v[1].pos.x * ppp, v[1].pos.y * ppp), (v[2].pos.x * ppp, v[2].pos.y * ppp)];
        let mut v = v;
        let mut area = edge(p[0], p[1], p[2]);
        if area.abs() < 1e-6 {
            return;
        }
        if area < 0.0 {
            p.swap(1, 2);
            v.swap(1, 2);
            area = -area;
        }
        let minx = p[0].0.min(p[1].0).min(p[2].0).floor().max(clip.0 as f32) as usize;
        let maxx = (p[0].0.max(p[1].0).max(p[2].0).ceil() as isize).clamp(0, clip.2 as isize) as usize;
        let miny = p[0].1.min(p[1].1).min(p[2].1).floor().max(clip.1 as f32) as usize;
        let maxy = (p[0].1.max(p[1].1).max(p[2].1).ceil() as isize).clamp(0, clip.3 as isize) as usize;
        if minx >= maxx || miny >= maxy {
            return;
        }
        let col = |c: egui::Color32| [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32];
        let cs = [col(v[0].color), col(v[1].color), col(v[2].color)];
        let same_color = v[0].color == v[1].color && v[1].color == v[2].color;
        let same_uv = v[0].uv == v[1].uv && v[1].uv == v[2].uv;
        // Edges opposite each vertex, with the ownership of their boundary pixels.
        let e = [(p[1], p[2]), (p[2], p[0]), (p[0], p[1])];
        let own = [owns_boundary(e[0].0, e[0].1), owns_boundary(e[1].0, e[1].1), owns_boundary(e[2].0, e[2].1)];
        // Constant source (flat colour, one texel): the common case for panels and frames.
        let constant = if same_color && same_uv {
            let t = tex.map(|t| t.sample(v[0].uv.x, v[0].uv.y)).unwrap_or([255.0; 4]);
            let c = cs[0];
            Some([t[0] * c[0] / 255.0, t[1] * c[1] / 255.0, t[2] * c[2] / 255.0, t[3] * c[3] / 255.0])
        } else {
            None
        };
        if constant.is_some_and(|c| c[3] <= 0.0) {
            return;
        }
        let inv = 1.0 / area;
        for y in miny..maxy {
            let py = y as f32 + 0.5;
            let row = y * self.w;
            for x in minx..maxx {
                let pt = (x as f32 + 0.5, py);
                let w0 = edge(e[0].0, e[0].1, pt);
                let w1 = edge(e[1].0, e[1].1, pt);
                let w2 = edge(e[2].0, e[2].1, pt);
                let inside = |w: f32, o: bool| w > 0.0 || (w == 0.0 && o);
                if !(inside(w0, own[0]) && inside(w1, own[1]) && inside(w2, own[2])) {
                    continue;
                }
                let src = match constant {
                    Some(c) => c,
                    None => {
                        let (b0, b1, b2) = (w0 * inv, w1 * inv, w2 * inv);
                        let mut c = [0.0; 4];
                        for i in 0..4 {
                            c[i] = cs[0][i] * b0 + cs[1][i] * b1 + cs[2][i] * b2;
                        }
                        if let Some(t) = tex {
                            let u = v[0].uv.x * b0 + v[1].uv.x * b1 + v[2].uv.x * b2;
                            let vv = v[0].uv.y * b0 + v[1].uv.y * b1 + v[2].uv.y * b2;
                            let s = t.sample(u, vv);
                            for i in 0..4 {
                                c[i] = c[i] * s[i] / 255.0;
                            }
                        }
                        if c[3] <= 0.0 {
                            continue;
                        }
                        c
                    }
                };
                let d = &mut self.buf[row + x];
                *d = blend(*d, src);
            }
        }
    }

    /// The buffer as a PNG file.
    pub fn png(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(self.w * self.h * 3);
        for &p in &self.buf {
            rgb.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
        super::png::encode(self.w, self.h, &rgb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_a_ui_without_seams() {
        let ctx = egui::Context::default();
        let mut r = Raster::default();
        r.resize(200, 100);
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0)));
        let out = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default_margins().show(ui, |ui| {
                ui.label("gangli");
                let _ = ui.button("ok");
            });
        });
        r.set_textures(&out.textures_delta);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        r.paint(&prims, out.pixels_per_point, 0);
        // The panel covers everything: no pixel left at the clear colour, text drawn.
        let panel = r.buf[99 * 200 + 199];
        assert!(r.buf.iter().all(|&p| p != 0), "uncovered pixels");
        assert!(r.buf.iter().any(|&p| p != panel), "nothing drawn on the panel");
        assert_eq!(&r.png()[1..4], b"PNG");
    }
}
