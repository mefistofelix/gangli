//! gangli is portable **[decided 2026-11-20]**: everything it keeps lives next to the executable, in
//! `gangli-data/`, never in system or user folders:
//!
//! ```text
//! gangli(.exe)
//! gangli-data/
//!   node/      node key, memberships, node.log         (admin-only once the service is installed)
//!   console/   console key, networks, API, console.log (the user's)
//! ```
//!
//! The only things outside are what the OS itself requires to register a service (SCM entry, systemd unit,
//! LaunchDaemon plist), added by `install` and removed by `uninstall`.
//!
//! Tests / development: `GANGLI_DATA_DIR` replaces `gangli-data/` (one per simulated machine).

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

pub fn data_dir() -> PathBuf {
    dev_override().unwrap_or_else(|| exe_dir().join("gangli-data"))
}

pub fn node_dir() -> PathBuf {
    data_dir().join("node")
}

pub fn console_dir() -> PathBuf {
    data_dir().join("console")
}

/// Marker written by `install`: the node of this folder runs as the system service.
pub fn service_marker() -> PathBuf {
    node_dir().join("service")
}

pub fn service_installed() -> bool {
    service_marker().exists()
}

/// True when the node state belongs to the installed service and this process is not elevated.
pub fn node_needs_admin() -> bool {
    service_installed() && !crate::elevate::is_root()
}

/// Human name of the service as it appears to the OS.
pub const SERVICE_NAME: &str = "gangli";
pub const SERVICE_DISPLAY: &str = "gangli remote management";
/// launchd label (macOS).
pub const SERVICE_LABEL: &str = "com.mefistofelix.gangli";
