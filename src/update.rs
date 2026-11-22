//! Updating gangli in place (section 11 of docs/ARCHITECTURE.md), the same way on every OS and in every way
//! gangli runs (service, node by hand, window, `console run`), with a way back.
//!
//! The convention: **the new binary is put next to the running one as `<binary>.update`** (`gangli.update`,
//! `gangli.exe.update`): by hand, by the API / CLI (`self_update`), or by a node receiving one from its
//! console. Then:
//! 1. **Trial**: the candidate must run `version --json` within 20 s and name this OS and architecture;
//!    otherwise it is set aside as `<binary>.failed` and nothing changes.
//! 2. **Swap**: the running binary becomes `<binary>.rollback`, the candidate takes its name (dated now), and
//!    the process restarts on it (a service exits and its manager starts it again; anything else starts the
//!    new binary with the same arguments, which waits for the old process to be gone).
//! 3. **Probation**: the update is kept as soon as a long-running gangli on the new binary has run 30 s or
//!    ended normally: `.rollback` becomes `<binary>.previous` (the last version, for a rollback by hand). If
//!    three minutes after the swap none has (it keeps failing: a service is restarted every 5 s), the next
//!    start puts the previous binary back (the new one becomes `<binary>.failed`) and restarts on it.
//! 4. **Watch**: every long-running gangli looks every 30 s, locally, for a `.update` to apply or for its
//!    binary replaced by another gangli of the same folder (the window updating while the service runs from
//!    the same file), and restarts on the new one.
//!
//! A console can update its machines (`update`); only consoles of the network (owners) can do it, and they
//! can already run anything on the machine: no signature on top. From a release on the Internet (`latest`,
//! `build-N`, an https folder) the console only sends the address and the SHA-256 from the release's
//! `SHA256SUMS`: the machine downloads the binary itself (fast, not through a relay) and checks it. From a
//! local folder the console sends the bytes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};

/// The release asset for a platform (`std::env::consts::OS`) and architecture.
pub fn asset_for(platform: &str, arch: &str) -> Option<&'static str> {
    match (platform, arch) {
        ("windows", "x86_64") => Some("gangli-windows-x86_64.exe"),
        ("linux", "x86_64") => Some("gangli-linux-x86_64"),
        ("linux", "aarch64") => Some("gangli-linux-aarch64"),
        ("macos", _) => Some("gangli-macos-universal"),
        _ => None,
    }
}

/// The binary is an executable for that platform and architecture (a first look, before the trial run).
pub fn check(bytes: &[u8], platform: &str, arch: &str) -> Result<()> {
    let ok = match platform {
        "windows" => bytes.starts_with(b"MZ"),
        "linux" => {
            // ELF, machine at offset 18: 0x3e x86_64, 0xb7 aarch64.
            let machine = bytes.get(18..20).map(|m| u16::from_le_bytes([m[0], m[1]]));
            bytes.starts_with(b"\x7fELF") && machine == Some(if arch == "aarch64" { 0xb7 } else { 0x3e })
        }
        "macos" => matches!(bytes.get(..4), Some([0xca, 0xfe, 0xba, 0xbe]) | Some([0xcf, 0xfa, 0xed, 0xfe])),
        _ => false,
    };
    if !ok {
        bail!("not a gangli binary for {platform} {arch}");
    }
    Ok(())
}

/// The binary this process started from (a rename later must not change what we call "our binary").
fn exe() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gangli")))
}

/// `<binary><suffix>` next to it.
fn sibling(suffix: &str) -> PathBuf {
    let mut s = exe().as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Where a new binary is put to be applied.
pub fn staged_path() -> PathBuf {
    sibling(".update")
}

/// The binary as it was when this process started (to notice another process replacing it).
fn fingerprint() -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(exe()).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// Put `bytes` next to the running binary as `<binary>.update` (checked first).
pub fn stage(bytes: &[u8]) -> Result<()> {
    check(bytes, std::env::consts::OS, std::env::consts::ARCH)?;
    let p = staged_path();
    std::fs::write(&p, bytes).with_context(|| format!("write {} (is the folder writable? the service's folder needs an administrator)", p.display()))?;
    make_executable(&p)
}

/// Stage a binary from a file, or from a release folder (the asset for this OS).
pub fn stage_from(from: &Path) -> Result<()> {
    let file = if from.is_dir() {
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| anyhow!("no release for this system"))?;
        from.join(asset)
    } else {
        from.to_path_buf()
    };
    let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
    stage(&bytes)
}

/// gangli's releases (GitHub Actions of the public repository; README).
pub const RELEASES: &str = "https://github.com/mefistofelix/gangli/releases";

/// Where a release is on the Internet, from what the owner wrote: `latest` (or nothing), `build-N`, or the
/// https address of a folder with the release files (`SHA256SUMS` and the binaries). `None`: a local path.
pub fn release_url(from: &str) -> Option<String> {
    let f = from.trim().trim_end_matches('/');
    if f.is_empty() || f == "latest" {
        Some(format!("{RELEASES}/latest/download"))
    } else if f.starts_with("build-") && !f.contains(['/', '\\']) {
        Some(format!("{RELEASES}/download/{f}"))
    } else if f.starts_with("https://") || f.starts_with("http://") {
        Some(f.to_string())
    } else {
        None
    }
}

/// An HTTPS client (rustls with ring: two crypto providers are in the tree, the default must be named).
pub fn http_client() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().timeout(Duration::from_secs(600)).user_agent("gangli").build().context("HTTPS client")
}

/// A release's `SHA256SUMS`: asset name → its SHA-256 (hex).
pub async fn release_sums(http: &reqwest::Client, base: &str) -> Result<Vec<(String, String)>> {
    let url = format!("{base}/SHA256SUMS");
    let body = http.get(&url).send().await.with_context(|| format!("GET {url}"))?.error_for_status()?.bytes().await?;
    let text = String::from_utf8_lossy(&body);
    let sums: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (h, n) = (f.next()?, f.next()?.trim_start_matches('*'));
            (h.len() == 64).then(|| (n.to_string(), h.to_lowercase()))
        })
        .collect();
    if sums.is_empty() {
        bail!("no SHA256SUMS at {url}");
    }
    Ok(sums)
}

/// Download a binary and check it against its SHA-256 (from the release's `SHA256SUMS`).
pub async fn download(http: &reqwest::Client, url: &str, sha256: &str) -> Result<Vec<u8>> {
    let r = http.get(url).send().await.with_context(|| format!("GET {url}"))?.error_for_status()?;
    if r.content_length().is_some_and(|n| n > 200 << 20) {
        bail!("{url}: too big");
    }
    let bytes = r.bytes().await.with_context(|| format!("download {url}"))?.to_vec();
    let got = data_encoding::HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
    if !crate::util::eq_ct(&got, &sha256.to_lowercase()) {
        bail!("{url}: SHA-256 does not match the release's SHA256SUMS");
    }
    Ok(bytes)
}

/// The binary for this computer from a release on the Internet (`release_url`).
pub async fn fetch_own(base: &str) -> Result<Vec<u8>> {
    let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| anyhow!("no release for this system"))?;
    let http = http_client()?;
    let sums = release_sums(&http, base).await?;
    let sha = sums.iter().find(|(n, _)| n == asset).map(|(_, h)| h.clone()).ok_or_else(|| anyhow!("{asset} not in the release"))?;
    download(&http, &format!("{base}/{asset}"), &sha).await
}

fn make_executable(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = p;
    Ok(())
}

/// Run the candidate: it must answer `version --json` for this OS and architecture within 20 s.
fn trial(candidate: &Path) -> Result<String> {
    let mut cmd = std::process::Command::new(candidate);
    cmd.args(["--json", "version"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().context("the new binary does not start")?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            let mut out = String::new();
            if let Some(mut o) = child.stdout.take() {
                use std::io::Read;
                let _ = o.read_to_string(&mut out);
            }
            if !status.success() {
                bail!("the new binary fails ({status})");
            }
            let v: serde_json::Value = serde_json::from_str(out.trim()).map_err(|_| anyhow!("the new binary does not answer as gangli"))?;
            if v["os"] != std::env::consts::OS || v["arch"] != std::env::consts::ARCH {
                bail!("the new binary is for {} {}", v["os"], v["arch"]);
            }
            return Ok(v["version"].as_str().unwrap_or("?").to_string());
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("the new binary does not answer");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Apply `<binary>.update` if there is one: trial, then swap. `Ok(true)`: swapped, restart now.
pub fn apply_staged() -> Result<bool> {
    let new = staged_path();
    if !new.exists() {
        return Ok(false);
    }
    let version = match trial(&new) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::rename(&new, sibling(".failed"));
            crate::log_line(&format!("update refused, the binary is kept as it was: {e:#}"));
            return Err(e);
        }
    };
    // A running binary can be renamed (Windows too), not overwritten in place. During a probation the
    // `.rollback` one is the good one: the unconfirmed binary is set aside instead.
    let back = sibling(".rollback");
    let aside = if back.exists() { sibling(".failed") } else { back.clone() };
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(exe(), &aside).context("move the running binary aside (is the folder writable?)")?;
    if let Err(e) = std::fs::rename(&new, exe()) {
        let _ = std::fs::rename(&aside, exe());
        return Err(e).context("put the new binary in place");
    }
    // Dated now: the probation counts from the swap (a copied file may keep an old date).
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(exe()) {
        let _ = f.set_modified(SystemTime::now());
    }
    crate::log_line(&format!("updated to {version}: restarting on it (the previous binary is kept until it has run 30 s)"));
    Ok(true)
}

/// Keep the update this process runs on, if it is on probation (it ran long enough, or ended normally).
pub fn confirm() {
    if std::fs::rename(sibling(".rollback"), sibling(".previous")).is_ok() {
        crate::log_line("update kept (the previous binary is the .previous file)");
    }
}

/// Restart on the binary now in place. A service exits (its manager starts it again: systemd, launchd, the
/// Windows recovery action); anything else starts the new binary with the same arguments, telling it to wait
/// until this process is gone (the window's single instance, the node's status port). Does not return.
pub fn restart() -> ! {
    if in_service() {
        std::process::exit(if cfg!(windows) { 1 } else { 0 });
    }
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // The window (double-clicked: no arguments) comes back as `gangli gui`, without a console window.
    let window = args.is_empty() || args[0] == "gui";
    if args.is_empty() {
        args.push("gui".into());
    }
    let mut cmd = std::process::Command::new(exe());
    cmd.args(args).env("GANGLI_WAIT_PID", std::process::id().to_string());
    #[cfg(windows)]
    if window {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = window;
    if let Err(e) = cmd.spawn() {
        crate::log_line(&format!("cannot start the new binary: {e}"));
    }
    std::process::exit(0);
}

/// This process is the service (`gangli run --service`).
fn in_service() -> bool {
    let a: Vec<String> = std::env::args().collect();
    a.iter().any(|x| x == "--service")
}

/// Started by a restart: wait (up to 20 s) for the process we replace to be gone.
pub fn wait_for_predecessor() {
    let Some(pid) = std::env::var("GANGLI_WAIT_PID").ok().and_then(|p| p.parse::<u32>().ok()) else { return };
    // SAFETY: before any thread is started.
    unsafe { std::env::remove_var("GANGLI_WAIT_PID") };
    let pid = sysinfo::Pid::from_u32(pid);
    let deadline = Instant::now() + Duration::from_secs(20);
    let Ok(mut sys) = sysinfo::System::new() else {
        std::thread::sleep(Duration::from_secs(3));
        return;
    };
    while Instant::now() < deadline {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if sys.process(pid).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// At the start of a long-running gangli (service, node, window, `console run`): apply a staged update,
/// keep track of a probation (and roll back after three short attempts), then watch for updates.
pub fn on_start() {
    let _ = exe();
    match apply_staged() {
        Ok(true) => restart(),
        Ok(false) | Err(_) => {}
    }
    let back = sibling(".rollback");
    if back.exists() {
        let since_swap = fingerprint().and_then(|(_, t)| t.elapsed().ok()).unwrap_or_default();
        if since_swap > Duration::from_secs(180) {
            // Three minutes and no gangli kept running on the new binary: back to the previous one.
            let _ = std::fs::rename(exe(), sibling(".failed"));
            if std::fs::rename(&back, exe()).is_ok() {
                crate::log_line("the updated binary did not keep running: back to the previous one (kept as .failed)");
                restart();
            }
        }
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(30));
            confirm();
        });
    }
    let start = fingerprint();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(30));
        match apply_staged() {
            Ok(true) => restart(),
            Ok(false) | Err(_) => {}
        }
        // Another gangli of this folder replaced the binary (it ran the trial): restart on it.
        let now = fingerprint();
        if start.is_some() && now.is_some() && now != start {
            crate::log_line("the binary was updated: restarting on it");
            restart();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_sources() {
        assert_eq!(release_url("").unwrap(), format!("{RELEASES}/latest/download"));
        assert_eq!(release_url("latest").unwrap(), format!("{RELEASES}/latest/download"));
        assert_eq!(release_url("build-14").unwrap(), format!("{RELEASES}/download/build-14"));
        assert_eq!(release_url("https://example.org/rel/").unwrap(), "https://example.org/rel");
        assert!(release_url(r"C:\Users\me\rel").is_none());
        assert!(release_url("out/rel14").is_none());
    }

    #[test]
    fn checks_the_platform() {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[18] = 0x3e;
        assert!(check(&elf, "linux", "x86_64").is_ok());
        assert!(check(&elf, "linux", "aarch64").is_err());
        assert!(check(&elf, "windows", "x86_64").is_err());
        assert!(check(b"MZ\x90\x00", "windows", "x86_64").is_ok());
        assert_eq!(asset_for("linux", "aarch64"), Some("gangli-linux-aarch64"));
    }

    #[test]
    fn a_binary_that_is_not_gangli_fails_the_trial() {
        // This test binary is no gangli: it does not answer `version --json` as one.
        let me = std::env::current_exe().unwrap();
        assert!(trial(&me).is_err());
    }
}
