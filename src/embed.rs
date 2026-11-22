//! Networks carried by the binary itself (section 10 of docs/ARCHITECTURE.md), so a relative only has to run
//! the file: `gangli pack` (or *Networks → Create gangli executable…* in the window) appends
//! `JSON | u32 length (LE) | MAGIC` to a copy of a binary, without recompiling. The JSON lists one or more
//! networks: public code, name, extra relays. A `gangli.networks` file with the same JSON next to the binary
//! works too (needed on macOS, where changing the file breaks its signature until it is signed again).
//! Only public codes: a private one is refused when packing and when reading.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 12] = b"GANGLI-NETS1";
const SIDECAR: &str = "gangli.networks";
const MAX: usize = 16 * 1024;

/// What a binary carries.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Bundle {
    pub networks: Vec<BundledNet>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BundledNet {
    /// The network's public code.
    pub code: String,
    #[serde(default)]
    pub name: String,
    /// Its extra relays (on top of the n0 ones).
    #[serde(default)]
    pub relays: Vec<String>,
}

impl Bundle {
    /// Only public codes, at least one, each network once.
    fn check(&self) -> Result<()> {
        if self.networks.is_empty() {
            bail!("no network to carry");
        }
        let mut seen = vec![];
        for n in &self.networks {
            match crate::code::decode(&n.code)? {
                crate::code::Code::Public { network, .. } => {
                    if seen.contains(&network) {
                        bail!("network {} twice", n.name);
                    }
                    seen.push(network);
                }
                crate::code::Code::Private { .. } => bail!("only public codes can be carried (never a private one)"),
            }
            crate::util::relay_list(&n.relays)?;
        }
        Ok(())
    }

    fn parse(bytes: &[u8]) -> Option<Bundle> {
        let b: Bundle = serde_json::from_slice(bytes).ok()?;
        b.check().ok()?;
        Some(b)
    }
}

/// The networks carried by (or next to) the running binary, if any.
pub fn bundled() -> Option<Bundle> {
    let exe = std::env::current_exe().ok()?;
    read_trailer(&exe).or_else(|| Bundle::parse(&std::fs::read(exe.parent()?.join(SIDECAR)).ok()?))
}

/// The JSON at the end of a binary, and its length.
fn trailer(path: &Path) -> Option<(Vec<u8>, usize)> {
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
    let n = u32::from_le_bytes(tail[..4].try_into().ok()?) as usize;
    if n == 0 || n > MAX || n as u64 + 16 > len {
        return None;
    }
    f.seek(SeekFrom::End(-16 - n as i64)).ok()?;
    let mut body = vec![0u8; n];
    f.read_exact(&mut body).ok()?;
    Some((body, n))
}

fn read_trailer(path: &Path) -> Option<Bundle> {
    trailer(path).and_then(|(b, _)| Bundle::parse(&b))
}

/// Copy `from` to `out` carrying `bundle` (what `from` carried is replaced).
pub fn pack(from: &Path, out: &Path, bundle: &Bundle) -> Result<()> {
    bundle.check()?;
    let mut data = std::fs::read(from).with_context(|| format!("read {}", from.display()))?;
    if let Some((_, n)) = trailer(from) {
        data.truncate(data.len() - 16 - n);
    }
    let json = serde_json::to_vec(bundle)?;
    if json.len() > MAX {
        bail!("too many networks to carry");
    }
    data.extend_from_slice(&json);
    data.extend_from_slice(&(json.len() as u32).to_le_bytes());
    data.extend_from_slice(MAGIC);
    std::fs::write(out, &data).with_context(|| format!("write {}", out.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// The carried networks this machine is not in yet.
pub fn not_joined() -> Vec<BundledNet> {
    let Some(b) = bundled() else { return vec![] };
    let have: Vec<String> = crate::state::load().map(|s| s.networks.iter().map(|m| m.id.clone()).collect()).unwrap_or_default();
    b.networks
        .into_iter()
        .filter(|n| match crate::code::decode(&n.code) {
            Ok(crate::code::Code::Public { network, .. }) => !have.contains(&network.to_string()),
            _ => false,
        })
        .collect()
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
    use iroh::SecretKey;

    fn net(name: &str) -> BundledNet {
        BundledNet { code: crate::code::public_code(&SecretKey::generate()), name: name.into(), relays: vec![] }
    }

    #[test]
    fn pack_and_read() {
        let dir = std::env::temp_dir().join(format!("gangli-embed-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("bin");
        std::fs::write(&src, b"\x7fELF fake binary body").unwrap();
        assert_eq!(read_trailer(&src), None);
        let one = Bundle { networks: vec![BundledNet { relays: vec!["https://relay.example.org".into()], ..net("family") }] };
        let out = dir.join("packed");
        pack(&src, &out, &one).unwrap();
        assert_eq!(read_trailer(&out), Some(one));
        // re-pack replaces what it carried instead of stacking
        let two = Bundle { networks: vec![net("family"), net("office")] };
        let out2 = dir.join("packed2");
        pack(&out, &out2, &two).unwrap();
        assert_eq!(read_trailer(&out2), Some(two.clone()));
        let body = std::fs::read(&src).unwrap().len() as u64;
        let json = serde_json::to_vec(&two).unwrap().len() as u64;
        assert_eq!(std::fs::metadata(&out2).unwrap().len(), body + json + 16);
        // never a private code, never nothing, never the same network twice
        let private = Bundle { networks: vec![BundledNet { code: crate::code::private_code(&SecretKey::generate()), name: "x".into(), relays: vec![] }] };
        assert!(pack(&src, &dir.join("bad"), &private).is_err());
        assert!(pack(&src, &dir.join("bad"), &Bundle::default()).is_err());
        let a = net("a");
        assert!(pack(&src, &dir.join("bad"), &Bundle { networks: vec![a.clone(), a] }).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
