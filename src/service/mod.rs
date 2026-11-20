//! Install, uninstall and run the node's system service, one implementation per OS (section 10 of
//! docs/ARCHITECTURE.md). gangli is portable: the service runs the binary **where it is**, with
//! `gangli.node` next to it. Because the service runs as SYSTEM / root (or the account given with `--user`),
//! its folder must not be writable by ordinary users (anyone could swap the binary): `install` requires a
//! folder of its own and makes it writable by administrators only, `gangli.node` readable only by them
//! (and the service account), `gangli.console` still the installing user's. `uninstall` gives the folder
//! back; the node keeps its identity.
//!
//! Elevation is asked only here (and for node commands once the service is installed).

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

/// Files that may share gangli's folder: its binaries (any OS), its files, a `gangli.code`.
fn belongs_to_gangli(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("gangli") || n == ".ds_store" || n == "desktop.ini"
}

/// Checks the folder, records the settings and the code, locks the folder down. Returns the binary.
pub(crate) fn prepare_install(o: &InstallOpts) -> io::Result<PathBuf> {
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
    let user = o.user.clone().unwrap_or_default();
    crate::state::update(|st| {
        st.node.installed = true;
        st.node.service_user = user.clone();
        Ok(())
    })
    .map_err(|e| other(format!("{e:#}")))?;
    crate::state::node_key().map_err(|e| other(format!("{e:#}")))?;
    if let Some(c) = &o.code {
        crate::state::add_join(c).map_err(|e| other(format!("{e:#}")))?;
    }
    // gangli.console must exist to keep the user's rights on it.
    let console = crate::paths::console_file();
    if !console.exists() {
        std::fs::write(&console, b"")?;
    }
    harden(&dir, o.user.as_deref())?;
    Ok(exe)
}

/// After the service is removed: the folder is the user's again; the node keeps its identity.
pub(crate) fn finish_uninstall() -> io::Result<()> {
    let _ = crate::state::update(|st| {
        st.node.installed = false;
        Ok(())
    });
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
fn harden(dir: &Path, service_user: Option<&str>) -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    let s = |p: &Path| p.to_string_lossy().to_string();
    // Folder and binary: SYSTEM + Administrators full, Users read & execute.
    icacls(&[&s(dir), "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "*S-1-5-32-545:(OI)(CI)RX", "/t", "/q"])?;
    // gangli.node: SYSTEM + Administrators (+ the service account) only.
    let node = s(&crate::paths::node_file());
    icacls(&[&node, "/inheritance:r", "/grant:r", "*S-1-5-18:F", "*S-1-5-32-544:F", "/q"])?;
    if let Some(u) = service_user {
        let _ = icacls(&[&node, "/grant", &format!("{u}:M"), "/q"]);
    }
    // gangli.console: also the user who installs.
    if let Ok(user) = std::env::var("USERNAME") {
        let who = match std::env::var("USERDOMAIN") {
            Ok(dom) => format!("{dom}\\{user}:M"),
            Err(_) => format!("{user}:M"),
        };
        let _ = icacls(&[&s(&crate::paths::console_file()), "/grant", &who, "/q"]);
    }
    Ok(())
}

#[cfg(windows)]
fn unharden(dir: &Path) -> io::Result<()> {
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    icacls(&[&dir.to_string_lossy(), "/reset", "/t", "/q"])
}

/// The user who asked for elevation (pkexec / sudo), to give the folder back to.
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

#[cfg(unix)]
fn harden(dir: &Path, service_user: Option<&str>) -> io::Result<()> {
    use std::os::unix::fs::{chown, PermissionsExt};
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
    // Folder and binaries: root's, readable by all.
    chown(dir, Some(0), Some(0))?;
    mode(dir, 0o755)?;
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let _ = chown(&p, Some(0), Some(0));
    }
    // gangli.node: root (or the service account) only.
    let node = crate::paths::node_file();
    let owner = match service_user {
        Some(u) => uid_of(u).ok_or_else(|| other(format!("no local account {u}")))?,
        None => 0,
    };
    chown(&node, Some(owner), None)?;
    mode(&node, 0o600)?;
    // gangli.console: the user who installs.
    let console = crate::paths::console_file();
    if let Some(uid) = invoking_uid() {
        chown(&console, Some(uid), None)?;
    }
    mode(&console, 0o600)?;
    Ok(())
}

#[cfg(unix)]
fn unharden(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::chown;
    if crate::paths::dev_override().is_some() {
        return Ok(());
    }
    if let Some(uid) = invoking_uid() {
        chown(dir, Some(uid), None)?;
        for e in std::fs::read_dir(dir)?.flatten() {
            let _ = chown(e.path(), Some(uid), None);
        }
    }
    Ok(())
}
