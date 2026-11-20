//! A PNG writer for the GUI's own screenshots (`GANGLI_GUI_SCREENSHOT`, used by the tests): RGB, deflate
//! stored blocks, no dependency.

/// `rgb`: `w`×`h` pixels, 3 bytes each, rows top to bottom.
pub fn encode(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for row in rgb.chunks(w * 3).take(h) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push(if i + 1 == chunks.len() { 1 } else { 0 });
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = vec![];
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    for (kind, data) in [(&b"IHDR"[..], &ihdr[..]), (b"IDAT", &z[..]), (b"IEND", &[][..])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    }
    out
}

fn crc32(d: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in d {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Where and when to save a screenshot: `GANGLI_GUI_SCREENSHOT=<file.png>`, after
/// `GANGLI_GUI_SCREENSHOT_AFTER` seconds (default 4); then the GUI quits.
pub fn screenshot_request() -> Option<(String, std::time::Duration)> {
    let path = std::env::var("GANGLI_GUI_SCREENSHOT").ok().filter(|p| !p.is_empty())?;
    let after = std::env::var("GANGLI_GUI_SCREENSHOT_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    Some((path, std::time::Duration::from_secs(after)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc_of_iend() {
        // The IEND chunk's well-known CRC.
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
        let p = super::encode(2, 1, &[255, 0, 0, 0, 255, 0]);
        assert_eq!(&p[1..4], b"PNG");
    }
}
