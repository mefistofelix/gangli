//! What a node reports about itself (section 8 of docs/ARCHITECTURE.md).

use serde::{Deserialize, Serialize};
use sysinfo::{Disks, Networks, System};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Inventory {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub kernel: String,
    pub arch: String,
    /// `windows`, `linux`, `macos` (picks the release asset for updates).
    #[serde(default)]
    pub platform: String,
    pub cpus: usize,
    pub cpu: String,
    /// Bytes.
    pub mem_total: u64,
    pub mem_avail: u64,
    pub disks: Vec<DiskInfo>,
    /// LAN addresses (no loopback, no link-local).
    pub ips: Vec<String>,
    /// Local accounts a session can be opened as.
    pub users: Vec<String>,
    /// Account the service runs as.
    pub service_user: String,
    /// True when the service runs as root / SYSTEM: any local user is reachable.
    pub full_access: bool,
    /// Seconds.
    pub uptime: u64,
    pub version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DiskInfo {
    pub mount: String,
    pub total: u64,
    pub avail: u64,
}

impl Inventory {
    /// The part that changes rarely (not free memory, free disk, uptime). A node pushes its inventory to
    /// the consoles only when this changes, so an idle node sends nothing.
    pub fn fingerprint(&self) -> String {
        let disks: Vec<(&str, u64)> = self.disks.iter().map(|d| (d.mount.as_str(), d.total)).collect();
        serde_json::json!([
            self.hostname, self.os, self.os_version, self.kernel, self.arch, self.cpus, self.cpu,
            self.mem_total, disks, self.ips, self.users, self.service_user, self.full_access, self.version
        ])
        .to_string()
    }

    /// "Windows 11 Home", "Ubuntu 24.04 LTS", "macOS 15.1": the OS name without repeating it.
    pub fn os_label(&self) -> String {
        let v = self.os_version.trim();
        if v.is_empty() {
            self.os.clone()
        } else if v.to_lowercase().contains(&self.os.to_lowercase()) || self.os.is_empty() {
            v.to_string()
        } else {
            format!("{} {v}", self.os)
        }
    }
}

/// sysinfo returns `Result` or `Option` depending on the function and version; accept both.
trait Lenient<T> {
    fn lenient(self) -> T;
}
impl Lenient<String> for Option<String> {
    fn lenient(self) -> String {
        self.unwrap_or_default()
    }
}
impl<E> Lenient<String> for Result<String, E> {
    fn lenient(self) -> String {
        self.unwrap_or_default()
    }
}
impl Lenient<String> for String {
    fn lenient(self) -> String {
        self
    }
}
impl Lenient<u64> for u64 {
    fn lenient(self) -> u64 {
        self
    }
}
impl<E> Lenient<u64> for Result<u64, E> {
    fn lenient(self) -> u64 {
        self.unwrap_or(0)
    }
}

pub fn collect() -> Inventory {
    let mut inv = Inventory {
        hostname: System::host_name().lenient(),
        os: System::name().lenient(),
        os_version: System::long_os_version().lenient(),
        kernel: System::kernel_version().lenient(),
        arch: std::env::consts::ARCH.to_string(),
        platform: std::env::consts::OS.to_string(),
        uptime: System::uptime().lenient(),
        version: crate::build_version(),
        full_access: crate::elevate::is_root(),
        service_user: service_user(),
        users: local_users(),
        ..Default::default()
    };
    if inv.hostname.is_empty() {
        inv.hostname = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "node".into());
    }

    if let Ok(mut sys) = System::new_all() {
        sys.refresh_all();
        inv.mem_total = sys.total_memory();
        inv.mem_avail = sys.available_memory();
        inv.cpus = sys.cpus().len();
        inv.cpu = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    }

    if let Ok(disks) = Disks::new_with_refreshed_list() {
        for d in disks.iter() {
            if d.total_space() == 0 {
                continue;
            }
            let mount = d.mount_point().to_string_lossy().to_string();
            if !useful_mount(&mount) || inv.disks.iter().any(|x| x.mount == mount) {
                continue;
            }
            // Bind mounts of one file system (same size, same free space): the first one says it all.
            if inv.disks.iter().any(|x| x.total == d.total_space() && x.avail == d.available_space()) {
                continue;
            }
            inv.disks.push(DiskInfo { mount, total: d.total_space(), avail: d.available_space() });
        }
    }

    if let Ok(nets) = Networks::new_with_refreshed_list() {
        for (name, data) in nets.iter() {
            // The loopback interface can carry extra addresses (WSL puts 10.255.255.254 there).
            let n = name.to_string_lossy();
            if n == "lo" || n.starts_with("lo0") {
                continue;
            }
            for n in data.ip_networks().iter() {
                let a = n.addr;
                let skip = a.is_loopback()
                    || a.is_unspecified()
                    || match a {
                        std::net::IpAddr::V4(v4) => v4.is_link_local(),
                        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
                    };
                let s = a.to_string();
                if !skip && !inv.ips.contains(&s) {
                    inv.ips.push(s);
                }
            }
        }
        // Home-network IPv4 first (what people type), then other IPv4, then IPv6.
        inv.ips.sort_by_key(|s| match s.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(v)) if v.octets()[0] == 192 && v.octets()[1] == 168 => 0,
            Ok(std::net::IpAddr::V4(v)) if v.is_private() => 1,
            Ok(std::net::IpAddr::V4(_)) => 2,
            _ => 3,
        });
    }
    inv
}

/// Disks a person cares about: not the system's pseudo or bind mounts (containers, WSL, snaps, files).
fn useful_mount(mount: &str) -> bool {
    if cfg!(windows) {
        return true;
    }
    const SKIP: [&str; 10] = ["/proc", "/sys", "/dev", "/run", "/snap", "/boot/efi", "/usr/lib/", "/mnt/wslg", "/var/lib/docker", "/private/var/vm"];
    mount == "/" || (!SKIP.iter().any(|p| mount.starts_with(p)) && std::path::Path::new(mount).is_dir())
}

fn service_user() -> String {
    #[cfg(windows)]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "SYSTEM".into())
    }
    #[cfg(unix)]
    {
        if crate::elevate::is_root() {
            "root".into()
        } else {
            std::env::var("USER").unwrap_or_default()
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn local_users() -> Vec<String> {
    // Real login accounts: root and uid >= 1000 with a real shell.
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else { return vec![] };
    let mut out = vec![];
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let uid: u32 = f[2].parse().unwrap_or(u32::MAX);
        let shell = f[6];
        let login = !(shell.ends_with("nologin") || shell.ends_with("false") || shell.is_empty());
        if (uid == 0 || (1000..65534).contains(&uid)) && login {
            out.push(f[0].to_string());
        }
    }
    out
}

#[cfg(any(windows, target_os = "macos"))]
fn local_users() -> Vec<String> {
    let skip = ["DefaultAccount", "WDAGUtilityAccount", "Guest", "nobody", "daemon"];
    let Ok(users) = sysinfo::Users::new_with_refreshed_list() else { return vec![] };
    let mut out: Vec<String> = users
        .iter()
        .map(|u| u.name().to_string())
        .filter(|n| !n.starts_with('_') && !skip.contains(&n.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}
