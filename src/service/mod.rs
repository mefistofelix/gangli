//! Install, uninstall and run the node's system service, one implementation per OS (section 10 of
//! docs/ARCHITECTURE.md). gangli is portable: the service runs the binary **where it is**, with its data in
//! `gangli-data/` next to it. Because a service runs as SYSTEM / root, its folder must not be writable by
//! ordinary users (anyone could swap the binary): `install` requires a folder of its own and makes it
//! writable by administrators only, leaving `gangli-data/console/` to the user who installs; `uninstall`
//! gives the folder back.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{install, run, uninstall};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{install, run, uninstall};

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::{install, run, uninstall};

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Files that may share gangli's folder: its binaries (any OS), a `gangli.code`, its data.
fn belongs_to_gangli(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("gangli") || n == ".ds_store" || n == "desktop.ini"
}

/// Checks the folder, makes it admin-only, records the code. Returns the binary to register.
pub(crate) fn prepare_install(code: Option<&str>) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = crate::paths::exe_dir();
    if crate::paths::dev_override().is_none() {
        let strangers: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !belongs_to_gangli(n))
            .take(3)
            .collect();
        if !strangers.is_empty() {
            return Err(other(format!(
                "{} also holds other files ({}…). The service runs gangli from its folder, which then becomes \
                 writable by administrators only: move gangli into a folder of its own (for example {}) and \
                 install from there",
                dir.display(),
                strangers.join(", "),
                if cfg!(windows) { r"C:\gangli" } else { "/opt/gangli" }
            )));
        }
    }
    for d in [crate::paths::node_dir(), crate::paths::console_dir()] {
        std::fs::create_dir_all(&d)?;
    }
    harden(&dir)?;
    std::fs::write(crate::paths::service_marker(), b"installed\n")?;
    if let Some(c) = code {
        crate::state::add_join(c).map_err(|e| other(format!("{e:#}")))?;
    }
    Ok(exe)
}

/// After the service is removed: the folder is the user's again; the node keeps its identity.
pub(crate) fn finish_uninstall() -> io::Result<()> {
    let _ = std::fs::remove_file(crate::paths::service_marker());
    unharden(&crate::paths::exe_dir())
}

#[cfg(windows)]
fn icacls(args: &[&str]) -> io::Result<()> {
    let st = std::process::Command::new("icacls")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !st.success() {
        return Err(other(format!("icacls {} failed", args.first().unwrap_or(&""))));
    }
    Ok(())
}

#[cfg(windows)]
fn harden(dir: &Path) -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    let d = dir.to_string_lossy().to_string();
    // Folder and binary: SYSTEM + Administrators full, Users read & execute.
    icacls(&[&d, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "*S-1-5-32-545:(OI)(CI)RX", "/t", "/q"])?;
    // Node state: SYSTEM + Administrators only.
    let node = crate::paths::node_dir().to_string_lossy().to_string();
    icacls(&[&node, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/t", "/q"])?;
    // Console state: also the user who installs.
    if let Ok(user) = std::env::var("USERNAME") {
        let who = match std::env::var("USERDOMAIN") {
            Ok(dom) => format!("{dom}\\{user}:(OI)(CI)M"),
            Err(_) => format!("{user}:(OI)(CI)M"),
        };
        let console = crate::paths::console_dir().to_string_lossy().to_string();
        let _ = icacls(&[&console, "/grant", &who, "/t", "/q"]);
    }
    Ok(())
}

#[cfg(windows)]
fn unharden(dir: &Path) -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    let d = dir.to_string_lossy().to_string();
    icacls(&[&d, "/reset", "/t", "/q"])
}

/// The user who asked for elevation (pkexec / sudo), to give the folder back to.
#[cfg(unix)]
fn invoking_uid() -> Option<u32> {
    ["PKEXEC_UID", "SUDO_UID"].iter().find_map(|v| std::env::var(v).ok()?.parse().ok())
}

#[cfg(unix)]
fn chown_tree(p: &Path, uid: u32, gid: Option<u32>) {
    let _ = std::os::unix::fs::chown(p, Some(uid), gid);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                chown_tree(&path, uid, gid);
            } else {
                let _ = std::os::unix::fs::chown(&path, Some(uid), gid);
            }
        }
    }
}

#[cfg(unix)]
fn harden(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    // Folder, binaries and node state: root's; console state: the invoking user's.
    chown_tree(dir, 0, Some(0));
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    std::fs::set_permissions(crate::paths::data_dir(), std::fs::Permissions::from_mode(0o755))?;
    std::fs::set_permissions(crate::paths::node_dir(), std::fs::Permissions::from_mode(0o700))?;
    if let Some(uid) = invoking_uid() {
        chown_tree(&crate::paths::console_dir(), uid, None);
    }
    Ok(())
}

#[cfg(unix)]
fn unharden(dir: &Path) -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    if let Some(uid) = invoking_uid() {
        chown_tree(dir, uid, None);
    }
    Ok(())
}
