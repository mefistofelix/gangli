//! One elevation prompt, the native way per OS: Windows UAC, macOS authorization dialog, Linux polkit.
//! `install` and `uninstall` call [`ensure_root`] first; if the process is not elevated it re-launches
//! itself elevated with the same arguments and the current process exits with the child's status.

use std::process::Command;

/// Returns normally if already elevated. Otherwise re-launches self elevated and exits the process.
pub fn ensure_root() -> std::io::Result<()> {
    if is_root() {
        return Ok(());
    }
    relaunch_elevated()
}

#[cfg(windows)]
pub fn is_root() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
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

#[cfg(unix)]
pub fn is_root() -> bool {
    // Safe FFI: geteuid never fails and takes no arguments.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn relaunch_elevated() -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_NORMAL;

    let exe = std::env::current_exe()?;
    // Everything after the program name, quoted so paths with spaces survive.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let params = args
        .iter()
        .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");

    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain([0]).collect() };
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file = wide(exe.as_os_str());
    let params_w: Vec<u16> = params.encode_utf16().chain([0]).collect();

    // ShellExecuteW with the "runas" verb triggers the UAC consent prompt.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params_w.as_ptr(),
            std::ptr::null(),
            SW_NORMAL,
        )
    };
    // The return value is >32 on success. The elevated copy owns the console from here.
    if (r as isize) <= 32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the administrator prompt was refused",
        ));
    }
    println!("continuing in the elevated window...");
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
fn relaunch_elevated() -> std::io::Result<()> {
    // osascript shows the standard macOS authentication dialog and runs the command as root.
    let exe = std::env::current_exe()?;
    let mut inner = format!("{}", shell_quote(&exe.to_string_lossy()));
    for a in std::env::args().skip(1) {
        inner.push(' ');
        inner.push_str(&shell_quote(&a));
    }
    // Escape for the AppleScript string literal.
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
