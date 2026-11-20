//! Service-side state on disk. In phase 2 this only records a join code captured at install time, so the
//! running service can enroll once the iroh layer exists (phase 3). Keys, networks and certificates join it
//! here later. Everything lives under [`crate::paths::state_dir`], owner-readable only.

use std::io;
use std::path::PathBuf;

fn pending_join_path() -> PathBuf {
    crate::paths::state_dir().join("pending-join")
}

/// Persist a join code supplied to `install --join`, to be consumed by the service on first start.
pub fn store_pending_join(code: &str) -> io::Result<()> {
    let dir = crate::paths::state_dir();
    std::fs::create_dir_all(&dir)?;
    let path = pending_join_path();
    std::fs::write(&path, code.trim().as_bytes())?;
    restrict(&path)?;
    Ok(())
}

/// Read and remove a pending join code, if any.
pub fn take_pending_join() -> io::Result<Option<String>> {
    let path = pending_join_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let _ = std::fs::remove_file(&path);
            Ok(Some(s.trim().to_string()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Tighten a secret file to owner-only.
fn restrict(path: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    {
        let _ = path; // ProgramData ACL inheritance covers this for now; hardened in phase 3.
    }
    Ok(())
}
