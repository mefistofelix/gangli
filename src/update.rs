//! Signed self-update (section 11 of docs/ARCHITECTURE.md). Releases are signed with the owner's release key
//! (Ed25519, `.tools/release.key`, never in the repository); its public half is built into every binary
//! (`GANGLI_RELEASE_PUB`). A console sends a machine the binary for its platform with its signature; the
//! node checks the signature against its built-in key, puts the new binary in place of the running one and
//! restarts: the service manager starts it again (systemd, launchd, the Windows service recovery action),
//! a node run by hand starts the new binary itself.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use data_encoding::HEXLOWER;
use iroh::{PublicKey, SecretKey};

/// The release key this binary trusts (absent in development builds: updates refused).
pub fn release_key() -> Option<PublicKey> {
    option_env!("GANGLI_RELEASE_PUB").and_then(|s| s.trim().parse().ok())
}

/// The release asset for a platform (`std::env::consts::OS`) and architecture.
pub fn asset_for(platform: &str, arch: &str) -> Option<&'static str> {
    match (platform, arch) {
        ("windows", "x86_64") => Some("gangli-windows-x86_64.exe"),
        ("linux", "x86_64") => Some("gangli-linux-x86_64"),
        ("linux", "aarch64") => Some("gangli-linux-aarch64"),
        ("macos", _) => Some("gangli-macos-universal"),
        _ => None,
    }
}

fn message(bytes: &[u8]) -> Vec<u8> {
    let mut m = b"gangli-release-v1\0".to_vec();
    m.extend_from_slice(bytes);
    m
}

/// `gangli release keygen`: a new release key (hex) into `out`; returns its public half.
pub fn keygen(out: &Path) -> Result<String> {
    if out.exists() {
        bail!("{} already exists: a release key is made once", out.display());
    }
    let k = SecretKey::generate();
    std::fs::write(out, HEXLOWER.encode(&k.to_bytes())).with_context(|| format!("write {}", out.display()))?;
    Ok(k.public().to_string())
}

/// `gangli release sign`: `<file>.sig` (hex signature) next to each file.
pub fn sign(key_file: &Path, files: &[PathBuf]) -> Result<()> {
    let key = crate::store::secret_from_hex(&std::fs::read_to_string(key_file).context("read the release key")?)?;
    for f in files {
        let bytes = std::fs::read(f).with_context(|| format!("read {}", f.display()))?;
        let sig = key.sign(&message(&bytes)).to_bytes();
        let mut out = f.as_os_str().to_owned();
        out.push(".sig");
        std::fs::write(PathBuf::from(out), HEXLOWER.encode(&sig))?;
    }
    Ok(())
}

pub fn verify(bytes: &[u8], sig_hex: &str) -> Result<()> {
    let key = release_key().ok_or_else(|| anyhow!("this gangli has no built-in release key: it cannot accept updates"))?;
    let raw = HEXLOWER.decode(sig_hex.trim().as_bytes()).map_err(|_| anyhow!("malformed signature"))?;
    let sig = TryFrom::try_from(&raw[..]).map_err(|_| anyhow!("malformed signature"))?;
    key.verify(&message(bytes), &sig).map_err(|_| anyhow!("bad signature: not a release signed by the owner"))
}

fn old_path() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let mut o = exe.as_os_str().to_owned();
    o.push(".old");
    Ok(PathBuf::from(o))
}

/// At start: the binary replaced by the last update can go now.
pub fn cleanup() {
    if let Ok(p) = old_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Write a verified binary next to the running one, swap them, restart. Does not return on success.
pub fn install_and_restart(bytes: &[u8]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut n = exe.as_os_str().to_owned();
    n.push(".new");
    let new = PathBuf::from(n);
    std::fs::write(&new, bytes).with_context(|| format!("write {}", new.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    // A running binary can be renamed (Windows) or replaced (Unix), not overwritten in place.
    let old = old_path()?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).context("move the running binary aside")?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e).context("put the new binary in place");
    }
    crate::log_line("updated: restarting with the new binary");
    if crate::paths::service_installed() && crate::elevate::is_root() {
        // The service manager starts it again (a failure exit triggers the Windows recovery action).
        std::process::exit(if cfg!(windows) { 1 } else { 0 });
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::Command::new(&exe).args(args).spawn().context("start the new binary")?;
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_with_a_key() {
        let k = SecretKey::generate();
        let raw = k.sign(&message(b"binary")).to_bytes();
        let sig = TryFrom::try_from(&raw[..]).unwrap();
        assert!(k.public().verify(&message(b"binary"), &sig).is_ok());
        assert!(k.public().verify(&message(b"other"), &sig).is_err());
        assert_eq!(asset_for("linux", "aarch64"), Some("gangli-linux-aarch64"));
    }
}
