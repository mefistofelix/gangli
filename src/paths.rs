//! Fixed locations per OS. The service state (keys, networks, certificates) is owner-only: root / SYSTEM /
//! Administrators (section 9 of docs/ARCHITECTURE.md). The client state lives in the user's config dir.
//!
//! Development / test override: `GANGLI_STATE_DIR` points the service state elsewhere and makes the state
//! commands run without elevation (a user-level instance, e.g. two agents and a master on one PC);
//! `GANGLI_CLIENT_DIR` does the same for the client.

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

pub fn dev_state_override() -> Option<PathBuf> {
    std::env::var_os("GANGLI_STATE_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Directory for keys, networks and certificates. Owned by the service account, not world-readable.
pub fn state_dir() -> PathBuf {
    if let Some(p) = dev_state_override() {
        return p;
    }
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

/// The client's own state, per user.
pub fn client_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("GANGLI_CLIENT_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Roaming"))
            .join("gangli")
    }
    #[cfg(target_os = "macos")]
    {
        home().join("Library").join("Application Support").join("gangli")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("gangli")
    }
}

/// Human name of the service as it appears to the OS.
pub const SERVICE_NAME: &str = "gangli";
pub const SERVICE_DISPLAY: &str = "gangli remote management";
/// launchd / reverse-dns label (macOS) and systemd unit base name.
pub const SERVICE_LABEL: &str = "com.mefistofelix.gangli";
