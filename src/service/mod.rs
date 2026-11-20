//! Install, uninstall and run the system service, one implementation per OS. Agent and master are the same
//! service (section 2 of docs/ARCHITECTURE.md); roles are added later as networks. `install` copies the
//! running binary to the fixed location and registers the service to start at boot; `run` is the service
//! body (a placeholder loop until the iroh endpoint lands in phase 3).

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

/// Create the state directory with restrictive permissions (owner only). Called from install.
pub(crate) fn create_state_dir() -> io::Result<()> {
    let dir = crate::paths::state_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // On Windows the directory inherits ProgramData's ACL; it is tightened when secrets are written (phase 3).
    Ok(())
}
