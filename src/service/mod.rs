//! Install, uninstall and run the node's system service, one implementation per OS (section 10 of
//! docs/ARCHITECTURE.md). gangli is portable: the service runs the binary **where it is**, with
//! `gangli.node` next to it. Where that is, and who may write to that folder, is the job of whoever installs
//! it (the service runs as SYSTEM / root, or the account given with `--user`): gangli checks nothing and
//! changes no folder permission. It only keeps `gangli.node` (the node's key) readable by the service
//! alone, and gives it back on `uninstall`; the node keeps its identity.
//!
//! Elevation is asked only here (and for node commands once the service is installed).

use std::io;
use std::path::PathBuf;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{install, installed_at, run, uninstall};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{install, installed_at, run, uninstall};

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(all(unix, not(target_os = "macos")))]
pub use linux::{install, installed_at, run, uninstall};

/// `gangli install` options.
pub struct InstallOpts {
    pub code: Option<String>,
    /// Account the service runs as (default SYSTEM / root).
    pub user: Option<String>,
    /// Its password (Windows service manager only).
    pub password: Option<String>,
}

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Records the settings and the code, keeps `gangli.node` to the service. Returns the binary.
pub(crate) fn prepare_install(o: &InstallOpts) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let user = o.user.clone().unwrap_or_default();
    crate::state::update(|st| {
        st.node.installed = true;
        st.node.service_user = user.clone();
        Ok(())
    })
    .map_err(|e| other(format!("{e:#}")))?;
    crate::state::node_key().map_err(|e| other(format!("{e:#}")))?;
    match &o.code {
        Some(c) => crate::state::add_join(c).map(|_| ()),
        // The networks this binary carries, with their relays.
        None => crate::state::join_bundled().map(|_| ()),
    }
    .map_err(|e| other(format!("{e:#}")))?;
    protect_node_file(o.user.as_deref())?;
    Ok(exe)
}

/// After the service is removed: `gangli.node` is the user's again; the node keeps its identity.
pub(crate) fn finish_uninstall() -> io::Result<()> {
    let _ = crate::state::update(|st| {
        st.node.installed = false;
        Ok(())
    });
    release_node_file()
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

/// gangli.node: SYSTEM + Administrators (+ the service account) only.
#[cfg(windows)]
fn protect_node_file(service_user: Option<&str>) -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    let node = crate::paths::node_file().to_string_lossy().to_string();
    icacls(&[&node, "/inheritance:r", "/grant:r", "*S-1-5-18:F", "*S-1-5-32-544:F", "/q"])?;
    if let Some(u) = service_user {
        let _ = icacls(&[&node, "/grant", &format!("{u}:M"), "/q"]);
    }
    Ok(())
}

/// gangli.node takes the folder's permissions again.
#[cfg(windows)]
fn release_node_file() -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    icacls(&[&crate::paths::node_file().to_string_lossy(), "/reset", "/q"])
}

/// The user who asked for elevation (pkexec / sudo), to give gangli.node back to.
#[cfg(unix)]
fn invoking_uid() -> Option<u32> {
    ["PKEXEC_UID", "SUDO_UID"].iter().find_map(|v| std::env::var(v).ok()?.parse().ok())
}

/// uid of a local account, through the standard `id` tool.
#[cfg(unix)]
fn uid_of(user: &str) -> Option<u32> {
    let out = std::process::Command::new("id").arg("-u").arg(user).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// gangli.node: root (or the service account) only.
#[cfg(unix)]
fn protect_node_file(service_user: Option<&str>) -> io::Result<()> {
    use std::os::unix::fs::{chown, PermissionsExt};
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    let node = crate::paths::node_file();
    let owner = match service_user {
        Some(u) => uid_of(u).ok_or_else(|| other(format!("no local account {u}")))?,
        None => 0,
    };
    chown(&node, Some(owner), None)?;
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn release_node_file() -> io::Result<()> {
    use std::os::unix::fs::chown;
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    if let Some(uid) = invoking_uid() {
        chown(crate::paths::node_file(), Some(uid), None)?;
    }
    Ok(())
}
