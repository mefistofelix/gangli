//! A small deterministic avatar for a machine or console, drawn from its id (its key) with egui: a 5×5
//! symmetric grid (the left three columns mirrored, GitHub-style) in a colour derived from the id. Same id →
//! same picture everywhere, so the local computer and each remote machine get a stable face.

use egui::{Color32, Rect, Ui, Vec2};

fn hash64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn hsl(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Color32::from_rgb(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

/// The identicon's main colour (also a stable per-machine accent).
pub fn color(id: &str) -> Color32 {
    hsl((hash64(id) % 360) as f32, 0.52, 0.55)
}

/// Draw the identicon for `id` inside `rect`, on a soft tile of its own colour.
pub fn paint(ui: &Ui, rect: Rect, id: &str) {
    let p = ui.painter();
    let fg = color(id);
    p.rect_filled(rect, rect.width() * 0.18, fg.gamma_multiply(0.16));
    // 15 bits (3 columns × 5 rows) decide the pattern, decorrelated from the hue.
    let bits = hash64(id).rotate_left(21) ^ 0x9e37_79b9_7f4a_7c15;
    let inset = rect.width() * 0.12;
    let cell = (rect.width() - 2.0 * inset) / 5.0;
    let origin = rect.min + Vec2::splat(inset);
    for col in 0..3u32 {
        for row in 0..5u32 {
            if (bits >> (col * 5 + row)) & 1 == 1 {
                for c in [col, 4 - col] {
                    let at = origin + Vec2::new(c as f32 * cell, row as f32 * cell);
                    p.rect_filled(Rect::from_min_size(at, Vec2::splat(cell)), 0.0, fg);
                }
            }
        }
    }
}

/// An identicon the size of a line, with a small online / offline dot in the corner when `online` is set.
pub fn avatar(ui: &mut Ui, id: &str, size: f32, online: Option<bool>) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    paint(ui, rect, id);
    if let Some(on) = online {
        let c = rect.right_bottom() - Vec2::splat(size * 0.14);
        let r = (size * 0.2).max(4.0);
        ui.painter().circle_filled(c, r + 1.5, ui.visuals().panel_fill);
        ui.painter().circle_filled(c, r, if on { super::app::GREEN } else { super::app::GREY });
    }
}
