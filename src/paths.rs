//! Fixed install locations and state directory per OS. State holds the keys, the joined networks and the
//! membership certificates; it is created readable only by root / SYSTEM / Administrators (section 9 of
//! docs/ARCHITECTURE.md).

use std::path::PathBuf;

/// Where the service binary is copied to on `install`.
pub fn install_bin() -> PathBuf {
    #[cfg(windows)]
    {
        let pf = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        pf.join("gangli").join("gangli.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/usr/local/bin/gangli")
    }
}

/// Directory for keys, networks and certificates. Owned by the service account, not world-readable.
pub fn state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let pd = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        pd.join("gangli")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/gangli")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        PathBuf::from("/var/lib/gangli")
    }
}

/// Human name of the service as it appears to the OS.
pub const SERVICE_NAME: &str = "gangli";
pub const SERVICE_DISPLAY: &str = "gangli remote management";
/// launchd / reverse-dns label (macOS) and systemd unit base name.
pub const SERVICE_LABEL: &str = "com.mefistofelix.gangli";
