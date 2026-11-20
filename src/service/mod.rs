//! Install, uninstall and run the system service, one implementation per OS. Agent and master are the same
//! service (section 2 of docs/ARCHITECTURE.md); roles are added later as networks. `install` copies the
//! running binary to the fixed location and registers the service to start at boot; the service body is
//! [`crate::daemon`].

use std::io;
use std::path::Path;

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

/// Copy the currently running executable to `dest`, creating its parent directory. Shared by every OS.
pub(crate) fn copy_self_to(dest: &Path) -> io::Result<()> {
    let src = std::env::current_exe()?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Replacing a running service binary is done while the service is stopped, so a plain copy is fine.
    if src != dest {
        std::fs::copy(&src, dest)?;
    }
    Ok(())
}

/// Create the state directory readable only by root / SYSTEM / Administrators.
pub fn secure_state_dir() -> anyhow::Result<()> {
    let dir = crate::paths::state_dir();
    crate::store::ensure_dir(&dir)?;
    #[cfg(windows)]
    if crate::paths::dev_state_override().is_none() {
        // ProgramData lets every user read subfolders; replace the inherited ACL with SYSTEM + Administrators.
        let d = dir.to_string_lossy().to_string();
        let _ = std::process::Command::new("icacls")
            .args([d.as_str(), "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/q"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(())
}

/// Record a join code given to `install --join`.
pub(crate) fn record_join(code: Option<&str>) -> io::Result<()> {
    if let Some(c) = code {
        crate::state::add_join(c).map_err(|e| io::Error::other(format!("{e:#}")))?;
    }
    Ok(())
}

pub(crate) fn secure_dir_io() -> io::Result<()> {
    secure_state_dir().map_err(|e| io::Error::other(format!("{e:#}")))
}
