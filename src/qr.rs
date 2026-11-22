//! QR codes for pairing from a phone (section 10 of docs/ARCHITECTURE.md): the module matrix (qrcodegen) and a
//! rendering for terminals in half blocks, black on a forced white background so a phone camera reads it on
//! any terminal theme (also over SSH).

use qrcodegen::{QrCode, QrCodeEcc};

/// The QR code of `text`: its side in modules and the dark modules row by row (no quiet zone).
pub fn matrix(text: &str) -> (usize, Vec<bool>) {
    let q = QrCode::encode_text(text, QrCodeEcc::Medium).expect("a pairing URL fits a QR code");
    let n = q.size() as usize;
    let mut v = Vec::with_capacity(n * n);
    for y in 0..n as i32 {
        for x in 0..n as i32 {
            v.push(q.get_module(x, y));
        }
    }
    (n, v)
}

/// The QR code as terminal lines, two module rows per line (▀ ▄ █), with a light margin around it.
pub fn terminal(text: &str) -> String {
    let (n, m) = matrix(text);
    let margin = 3isize;
    let dark = |x: isize, y: isize| x >= 0 && y >= 0 && (x as usize) < n && (y as usize) < n && m[y as usize * n + x as usize];
    let mut out = String::new();
    let mut y = -margin;
    while y < n as isize + margin {
        // black on bright white, whatever the terminal's colours
        out.push_str("\x1b[30;107m");
        for x in -margin..n as isize + margin {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    out
}

/// Windows consoles show the colours above only with virtual terminal processing on.
pub fn enable_terminal_colours() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        for h in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(h);
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_and_terminal() {
        let (n, m) = matrix("https://example.org/#pair=abc");
        assert!(n >= 21 && n % 4 == 1, "{n}");
        assert_eq!(m.len(), n * n);
        // the three finder patterns: top-left corner is dark
        assert!(m[0]);
        let t = terminal("https://example.org/#pair=abc");
        assert_eq!(t.lines().count(), (n + 6).div_ceil(2));
    }
}
