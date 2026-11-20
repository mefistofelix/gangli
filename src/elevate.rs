//! One elevation prompt, the native way per OS: Windows UAC, macOS authorization dialog, Linux polkit.
//! Commands that need it call [`ensure_root`] (service install / uninstall) or [`ensure_admin`] (state
//! commands; skipped in the `GANGLI_STATE_DIR` development mode). When the process is not elevated it
//! re-launches itself elevated with the same arguments and exits with the child's status.
//!
//! Windows: the elevated copy runs hidden and writes its output to a temp file that this process prints,
//! so the user sees the answer (a join code, a status) in the same terminal after one UAC prompt.

#[cfg(unix)]
use std::process::Command;

/// Returns normally if already elevated. Otherwise re-launches self elevated and exits the process.
pub fn ensure_root() -> std::io::Result<()> {
    if is_root() {
        return Ok(());
    }
    relaunch_elevated()
}

/// For node state commands: elevate only when the node state is the system one (service installed).
/// A node run by hand keeps its state in the user's folder and never asks.
pub fn ensure_admin() -> std::io::Result<()> {
    if !crate::paths::node_needs_admin() {
        return Ok(());
    }
    ensure_root()
}

#[cfg(windows)]
pub fn is_root() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Effective uid 0, from `/proc/self/status` (no libc binding needed).
#[cfg(all(unix, not(target_os = "macos")))]
pub fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(|l| l.split_whitespace().nth(2) == Some("0")))
        .unwrap_or(false)
}

/// Effective uid 0: the owner of a file this process creates (macOS has no /proc).
#[cfg(target_os = "macos")]
pub fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    let p = std::env::temp_dir().join(format!(".gangli-uid-{}", std::process::id()));
    let uid = std::fs::write(&p, b"").and_then(|_| std::fs::metadata(&p)).map(|m| m.uid()).ok();
    let _ = std::fs::remove_file(&p);
    uid == Some(0)
}

/// Quote one argument for a Windows command line (CommandLineToArgvW rules).
#[cfg(windows)]
fn win_quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
fn relaunch_elevated() -> std::io::Result<()> {
    use std::io::Read;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
    // The elevated copy writes its output into a named pipe that this process reads: nothing on disk.
    let pipe_name = format!(r"\.\pipe\gangli-elevated-{}-{}", std::process::id(), data_encoding::HEXLOWER.encode(&crate::util::random16()[..6]));
    const PIPE_ACCESS_INBOUND: u32 = 0x1;
    let pipe = unsafe { CreateNamedPipeW(wide(&pipe_name).as_ptr(), PIPE_ACCESS_INBOUND, 0, 1, 0, 65536, 0, std::ptr::null()) };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let exe = std::env::current_exe()?;
    let mut params = format!("--elevated-output {}", win_quote(&pipe_name));
    for a in std::env::args().skip(1) {
        params.push(' ');
        params.push_str(&win_quote(&a));
    }
    let verb = wide("runas");
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let params_w = wide(&params);

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = SW_HIDE;
    // Shows the UAC consent prompt.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        unsafe { CloseHandle(pipe) };
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "administrator rights are needed and the prompt was refused",
        ));
    }
    let raw = pipe as usize;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let h = raw as windows_sys::Win32::Foundation::HANDLE;
        unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        let mut f = unsafe { std::fs::File::from_raw_handle(h as _) };
        let mut out = Vec::new();
        let _ = f.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let mut code: u32 = 1;
    if !info.hProcess.is_null() {
        unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
        }
    }
    if let Ok(out) = rx.recv_timeout(std::time::Duration::from_secs(3)) {
        print!("{}", String::from_utf8_lossy(&out));
    }
    std::process::exit(code as i32);
}

/// In the elevated copy: `--elevated-output <pipe>` (added by [`relaunch_elevated`]) sends stdout and stderr
/// to the pipe of the process that asked for elevation. Removes the two arguments so clap never sees them.
#[cfg(windows)]
pub fn adopt_elevated_output(args: &mut Vec<std::ffi::OsString>) {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    if args.len() < 3 || args[1].to_str() != Some("--elevated-output") {
        return;
    }
    let path = args.remove(2);
    args.remove(1);
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
        let h = f.into_raw_handle();
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, h as _);
            SetStdHandle(STD_ERROR_HANDLE, h as _);
        }
    }
}

#[cfg(target_os = "macos")]
fn relaunch_elevated() -> std::io::Result<()> {
    // osascript shows the standard macOS authentication dialog and runs the command as root; it prints the
    // command's output when done.
    let exe = std::env::current_exe()?;
    let mut inner = shell_quote(&exe.to_string_lossy());
    for a in std::env::args().skip(1) {
        inner.push(' ');
        inner.push_str(&shell_quote(&a));
    }
    inner.push_str(" 2>&1");
    let applescript = format!(
        "do shell script \"{}\" with administrator privileges",
        inner.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let status = Command::new("osascript").arg("-e").arg(applescript).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(all(unix, not(target_os = "macos")))]
fn relaunch_elevated() -> std::io::Result<()> {
    // polkit (graphical or terminal prompt); fall back to sudo when pkexec is absent.
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    for launcher in ["pkexec", "sudo"] {
        let mut cmd = Command::new(launcher);
        cmd.arg(&exe).args(&args);
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "neither pkexec nor sudo is available; run again as root",
    ))
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn quoting() {
        assert_eq!(super::win_quote("abc"), "abc");
        assert_eq!(super::win_quote("a b"), "\"a b\"");
        assert_eq!(super::win_quote(r"C:\x y\"), "\"C:\\x y\\\\\"");
        assert_eq!(super::win_quote("a\"b"), "\"a\\\"b\"");
    }
}
