//! The gangli icon (window and tray), drawn in code: three linked machines on a teal rounded square.

fn smooth(d: f32, px: f32) -> f32 {
    (0.5 - d / px).clamp(0.0, 1.0)
}

fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    ((pa.0 - ba.0 * h).powi(2) + (pa.1 - ba.1 * h).powi(2)).sqrt()
}

/// `size`×`size` RGBA pixels (not premultiplied).
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let px = 1.0 / s;
    let nodes = [(0.28, 0.68), (0.72, 0.68), (0.50, 0.30)];
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) / s, (y as f32 + 0.5) / s);
            // Rounded square, margin 0.03, corner radius 0.22.
            let (hx, hy, r) = (0.47, 0.47, 0.22);
            let q = ((p.0 - 0.5).abs() - hx + r, (p.1 - 0.5).abs() - hy + r);
            let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt() + q.0.max(q.1).min(0.0) - r;
            let bg = smooth(outside, px);
            let t = p.1;
            let (r0, g0, b0) = (20.0 + 0.0 * t, 160.0 - 60.0 * t, 150.0 - 40.0 * t);
            let mut white: f32 = 0.0;
            for i in 0..3 {
                let (a, b) = (nodes[i], nodes[(i + 1) % 3]);
                white = white.max(smooth(seg_dist(p, a, b) - 0.035, px));
            }
            for n in nodes {
                let d = ((p.0 - n.0).powi(2) + (p.1 - n.1).powi(2)).sqrt();
                white = white.max(smooth(d - 0.11, px));
            }
            let mix = |c: f32| c * (1.0 - white) + 255.0 * white;
            out.extend_from_slice(&[mix(r0) as u8, mix(g0) as u8, mix(b0) as u8, (bg * 255.0) as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_has_the_right_size() {
        let p = super::rgba(32);
        assert_eq!(p.len(), 32 * 32 * 4);
        assert_eq!(p[3], 0, "corner is transparent");
        assert_eq!(p[(16 * 32 + 16) * 4 + 3], 255, "center is opaque");
    }
}
