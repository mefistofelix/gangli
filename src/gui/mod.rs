//! The GUI (section 12 of docs/ARCHITECTURE.md): one window for the console and this computer's node, with a
//! tray icon; closing the window keeps gangli in the tray.
//!
//! The window content (`app.rs`, egui) is the same on every OS. It runs on eframe (winit + OpenGL) on Windows
//! and macOS. On Linux the binary is static and cannot load the system's X11 / GL libraries, so it has its own
//! small backend: the X11 protocol in pure Rust (`x11.rs`, x11rb), egui drawn by a CPU rasterizer
//! (`raster.rs`), the tray over D-Bus (`sni.rs`, ksni).
//!
//! The GUI is a console: it opens one in this process (owning the local port, the SSH gateway, the tunnels),
//! or, when a console is already open in this folder (`gangli console run`), works through its local API.
//! A second GUI started in the same folder only brings the first one's window up.

mod app;
mod autostart;
mod client;
mod icon;
mod png;
mod tr;

#[cfg(any(windows, target_os = "macos"))]
mod native;
#[cfg(target_os = "linux")]
mod raster;
#[cfg(target_os = "linux")]
mod sni;
mod term;
#[cfg(target_os = "linux")]
mod x11;

pub use app::App;

use anyhow::Result;

/// What the tray menu asks for.
#[derive(Clone, Debug)]
pub enum TrayAction {
    Show,
    /// Open a terminal on this machine with the first launcher.
    Open(String),
    Quit,
}

/// What the tray shows; rebuilt by the platform layer when it changes.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TrayModel {
    pub tooltip: String,
    /// Online machines (name, network).
    pub online: Vec<String>,
    pub open_label: String,
    pub show_label: String,
    pub quit_label: String,
    pub machines_label: String,
}

/// Wakes the window loop from another thread (a console answer arrived, the tray was used).
pub type Waker = std::sync::Arc<dyn Fn() + Send + Sync>;

/// True when `gangli` was started with no command from a desktop (double click), not from a terminal: then
/// the GUI opens instead of the help text.
pub fn started_from_desktop() -> bool {
    #[cfg(windows)]
    {
        // A console made just for us (Explorer), not a terminal we share with a shell.
        let mut ids = [0u32; 4];
        let n = unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
        n == 1
    }
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() || std::env::var_os("SSH_CONNECTION").is_some() {
            return false;
        }
        cfg!(target_os = "macos") || std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
}

/// `gangli gui` (and `gangli` from a desktop). `hidden`: start in the tray (at login).
pub fn run(hidden: bool) -> Result<()> {
    // Another GUI open in this folder: bring it up and leave.
    if let Ok(Some(v)) = crate::ipc::call_open(crate::ipc::IpcOp::GuiShow) {
        if v["shown"].as_bool() == Some(true) {
            return Ok(());
        }
    }
    #[cfg(windows)]
    hide_own_console();
    tr::init();
    let client = client::Client::start()?;
    #[cfg(any(windows, target_os = "macos"))]
    return native::run(client, hidden);
    #[cfg(target_os = "linux")]
    return x11::run(client, hidden);
}

/// Double-clicked on Windows: the console window made for us has nothing to show.
#[cfg(windows)]
fn hide_own_console() {
    if started_from_desktop() {
        unsafe {
            windows_sys::Win32::System::Console::FreeConsole();
        }
    }
}

/// A line in `gangli.log`.
pub(crate) fn glog(msg: &str) {
    crate::log_to_file(&format!("gui: {msg}"));
}
