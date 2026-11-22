//! Windows: the gangli icon embedded in the executable (Explorer, the desktop, shortcuts, the taskbar before
//! the window draws its own). The same drawing as the window's icon (src/gui/icon.rs), in several sizes,
//! written as a compiled resource file (.res) that the MSVC linker takes as an input: no resource compiler
//! or other tool needed.

use std::path::PathBuf;

#[path = "src/gui/icon.rs"]
#[allow(dead_code)]
mod icon;

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/gui/icon.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("gangli-icon.res");
    std::fs::write(&out, res()).expect("write the icon resource");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// One icon image as a DIB: BITMAPINFOHEADER (height doubled for the mask), BGRA rows bottom-up, then an
/// all-zero AND mask (the alpha channel does the transparency).
fn dib(size: u32) -> Vec<u8> {
    let rgba = icon::rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut d = Vec::new();
    for v in [40u32, size, size * 2] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    d.extend_from_slice(&1u16.to_le_bytes()); // planes
    d.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    for v in [0u32, size * size * 4 + mask_row * size, 0, 0, 0, 0] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            d.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    d.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    d
}

/// A .res file: the empty first entry, one RT_ICON per size, and the RT_GROUP_ICON that lists them
/// (id 1: the icon Windows shows for the executable).
fn res() -> Vec<u8> {
    let mut f = Vec::new();
    entry(&mut f, 0, 0, &[], 0, 0);
    let images: Vec<Vec<u8>> = SIZES.iter().map(|s| dib(*s)).collect();
    for (i, img) in images.iter().enumerate() {
        entry(&mut f, 3, i as u16 + 1, img, 0x1010, 0x0409);
    }
    let mut group = Vec::new();
    for v in [0u16, 1, SIZES.len() as u16] {
        group.extend_from_slice(&v.to_le_bytes());
    }
    for (i, (size, img)) in SIZES.iter().zip(&images).enumerate() {
        let side = if *size >= 256 { 0 } else { *size as u8 };
        group.extend_from_slice(&[side, side, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(img.len() as u32).to_le_bytes());
        group.extend_from_slice(&(i as u16 + 1).to_le_bytes());
    }
    entry(&mut f, 14, 1, &group, 0x1030, 0x0409);
    f
}

/// RESOURCEHEADER with numeric type and name, then the data padded to 4 bytes.
fn entry(f: &mut Vec<u8>, kind: u16, name: u16, data: &[u8], flags: u16, lang: u16) {
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&32u32.to_le_bytes());
    for v in [0xFFFFu16, kind, 0xFFFF, name] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    f.extend_from_slice(&0u32.to_le_bytes()); // data version
    f.extend_from_slice(&flags.to_le_bytes());
    f.extend_from_slice(&lang.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes()); // version
    f.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    f.extend_from_slice(data);
    while f.len() % 4 != 0 {
        f.push(0);
    }
}
