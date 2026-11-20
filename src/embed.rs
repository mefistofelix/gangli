//! A public network code carried by the binary itself (section 10 of docs/ARCHITECTURE.md), so a relative
//! only has to run the file: `gangli pack` appends `code | u32 length (LE) | MAGIC` to a copy of a release
//! binary, without recompiling. A `gangli.code` file next to the binary works too (needed on macOS, where
//! changing the file breaks its signature until it is signed again).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

const MAGIC: &[u8; 12] = b"GANGLI-CODE1";
const SIDECAR: &str = "gangli.code";

/// The code embedded in (or next to) the running binary, if any.
pub fn embedded_code() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    read_trailer(&exe).or_else(|| {
        let side = exe.parent()?.join(SIDECAR);
        let s = std::fs::read_to_string(side).ok()?;
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    })
}

fn read_trailer(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len < 16 {
        return None;
    }
    f.seek(SeekFrom::End(-16)).ok()?;
    let mut tail = [0u8; 16];
    f.read_exact(&mut tail).ok()?;
    if &tail[4..] != MAGIC {
        return None;
    }
    let n = u32::from_le_bytes(tail[..4].try_into().ok()?) as u64;
    if n == 0 || n > 512 || n + 16 > len {
        return None;
    }
    f.seek(SeekFrom::End(-16 - n as i64)).ok()?;
    let mut code = vec![0u8; n as usize];
    f.read_exact(&mut code).ok()?;
    String::from_utf8(code).ok()
}

/// Copy `from` to `out` with `code` appended (an existing trailer is replaced).
pub fn pack(from: &Path, out: &Path, code: &str) -> Result<()> {
    let mut data = std::fs::read(from).with_context(|| format!("read {}", from.display()))?;
    if let Some(old) = read_trailer(from) {
        data.truncate(data.len() - 16 - old.len());
    }
    if code.len() > 512 {
        bail!("code too long");
    }
    data.extend_from_slice(code.as_bytes());
    data.extend_from_slice(&(code.len() as u32).to_le_bytes());
    data.extend_from_slice(MAGIC);
    std::fs::write(out, &data).with_context(|| format!("write {}", out.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// True for a Mach-O file (macOS): packing it needs a new ad-hoc signature.
pub fn is_macho(path: &Path) -> bool {
    let mut head = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok()
        && matches!(head, [0xca, 0xfe, 0xba, 0xbe] | [0xcf, 0xfa, 0xed, 0xfe] | [0xce, 0xfa, 0xed, 0xfe])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_read() {
        let dir = std::env::temp_dir().join(format!("gangli-embed-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("bin");
        std::fs::write(&src, b"\x7fELF fake binary body").unwrap();
        assert_eq!(read_trailer(&src), None);
        let out = dir.join("packed");
        pack(&src, &out, "gangli-pub-abc").unwrap();
        assert_eq!(read_trailer(&out).as_deref(), Some("gangli-pub-abc"));
        // re-pack replaces the code instead of stacking
        let out2 = dir.join("packed2");
        pack(&out, &out2, "gangli-pub-xyz").unwrap();
        assert_eq!(read_trailer(&out2).as_deref(), Some("gangli-pub-xyz"));
        assert_eq!(std::fs::metadata(&out2).unwrap().len(), std::fs::metadata(&out).unwrap().len());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
