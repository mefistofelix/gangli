//! Self-update over the sessions (section 11 of docs/ARCHITECTURE.md). A console sends a machine the release
//! binary for its platform; the node checks that it is an executable for itself, puts it in place of the
//! running one and restarts: the service manager starts it again (systemd, launchd, the Windows service
//! recovery action), a node run by hand starts the new binary itself. Only consoles of the network (owners)
//! can send it, and they can already run anything on the machine: no signature on top.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

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

/// The binary is an executable for that platform and architecture (a wrong file would leave the machine
/// without a node).
pub fn check(bytes: &[u8], platform: &str, arch: &str) -> Result<()> {
    let ok = match platform {
        "windows" => bytes.starts_with(b"MZ"),
        "linux" => {
            // ELF, machine at offset 18: 0x3e x86_64, 0xb7 aarch64.
            let machine = bytes.get(18..20).map(|m| u16::from_le_bytes([m[0], m[1]]));
            bytes.starts_with(b"\x7fELF") && machine == Some(if arch == "aarch64" { 0xb7 } else { 0x3e })
        }
        "macos" => matches!(bytes.get(..4), Some([0xca, 0xfe, 0xba, 0xbe]) | Some([0xcf, 0xfa, 0xed, 0xfe])),
        _ => false,
    };
    if !ok {
        bail!("not a gangli binary for {platform} {arch}");
    }
    Ok(())
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

/// Write the new binary next to the running one, swap them, restart. Does not return on success.
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
    fn checks_the_platform() {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[18] = 0x3e;
        assert!(check(&elf, "linux", "x86_64").is_ok());
        assert!(check(&elf, "linux", "aarch64").is_err());
        assert!(check(&elf, "windows", "x86_64").is_err());
        assert!(check(b"MZ\x90\x00", "windows", "x86_64").is_ok());
        assert_eq!(asset_for("linux", "aarch64"), Some("gangli-linux-aarch64"));
    }
}
