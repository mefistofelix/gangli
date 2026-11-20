//! gangli is portable **[decided 2026-11-20]**: what it keeps is two files next to the executable, never
//! anything in system or user folders:
//!
//! ```text
//! gangli(.exe)
//! gangli.node      the node: key, networks, service settings   (admin-only once the service is installed)
//! gangli.console   the console: key, networks, machines, API, SSH, launchers, tunnels   (the user's)
//! gangli.log       the node's log, size-capped
//! ```
//!
//! The only things outside are what the OS itself requires to register a service (SCM entry, systemd unit,
//! LaunchDaemon plist), added by `install` and removed by `uninstall`.
//!
//! Tests / development: `GANGLI_DATA_DIR` puts the three files in another folder (one per simulated machine).

use std::path::PathBuf;

/// The folder of the running executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn dev_override() -> Option<PathBuf> {
    std::env::var_os("GANGLI_DATA_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Where the files live: next to the binary (or the development override).
pub fn base_dir() -> PathBuf {
    let d = dev_override().unwrap_or_else(exe_dir);
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn node_file() -> PathBuf {
    base_dir().join("gangli.node")
}

pub fn console_file() -> PathBuf {
    base_dir().join("gangli.console")
}

pub fn log_file() -> PathBuf {
    base_dir().join("gangli.log")
}

/// The node of this folder runs as the system service. An unreadable node file means it does (it is
/// admin-only once installed).
pub fn service_installed() -> bool {
    match std::fs::read_to_string(node_file()) {
        Ok(_) => crate::state::load().map(|s| s.node.installed).unwrap_or(false),
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
    }
}

/// True when the node state belongs to the installed service and this process is not elevated.
pub fn node_needs_admin() -> bool {
    !crate::elevate::is_root() && service_installed()
}

/// Human name of the service as it appears to the OS.
pub const SERVICE_NAME: &str = "gangli";
pub const SERVICE_DISPLAY: &str = "gangli remote management";
/// launchd label (macOS).
pub const SERVICE_LABEL: &str = "com.mefistofelix.gangli";
