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
//! A second gangli started on the same computer, from any folder, only brings the first one's window up.
//! The window opens with its last size and place (`[gui]` in `gangli.console`), or about three quarters of
//! the screen the first time.

mod app;
mod autostart;
mod client;
mod icon;
mod identicon;
mod png;

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

/// The window's size and place, in points (logical pixels).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Geometry {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The size and place saved at the last run (`[gui] window` in `gangli.console`), if sensible.
pub fn saved_geometry() -> Option<Geometry> {
    let st: crate::console::ConsoleState = crate::store::load(&crate::paths::console_file()).ok()?;
    let [x, y, w, h] = st.gui.window?;
    (w >= MIN_SIZE.0 && h >= MIN_SIZE.1 && w < 20000.0 && h < 20000.0 && x.abs() < 50000.0 && y.abs() < 50000.0).then_some(Geometry { x, y, w, h })
}

/// The smallest window.
pub const MIN_SIZE: (f32, f32) = (640.0, 420.0);

/// A first size for a screen of `sw × sh` points (a 1920×1080 screen at 150% is 1280×720 points): about
/// three quarters of it, not huge on a big screen, never more than fits.
pub fn default_size(sw: f32, sh: f32) -> (f32, f32) {
    let w = (sw * 0.75).clamp(800.0, 1280.0).min(sw * 0.94).max(MIN_SIZE.0);
    let h = (sh * 0.78).clamp(560.0, 860.0).min(sh * 0.88).max(MIN_SIZE.1);
    (w.round(), h.round())
}

/// `gangli gui` (and `gangli` from a desktop). `hidden`: start in the tray (at login).
pub fn run(hidden: bool) -> Result<()> {
    // A gangli window already open on this computer (from any folder): bring it up and leave.
    #[cfg(windows)]
    unsafe {
        // We were just started by the user, so we may hand the foreground to the window we wake.
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY);
    }
    if crate::ipc::show_running_gui() == Some(true) {
        glog("another gangli window is open: brought it up");
        return Ok(());
    }
    #[cfg(windows)]
    hide_own_console();
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

#[cfg(test)]
mod tests {
    #[test]
    fn default_sizes() {
        // a 1920×1080 screen at 150%, a small laptop, a big screen
        assert_eq!(super::default_size(1280.0, 720.0), (960.0, 562.0));
        assert_eq!(super::default_size(1366.0, 768.0), (1025.0, 599.0));
        assert_eq!(super::default_size(2560.0, 1440.0), (1280.0, 860.0));
        let (w, h) = super::default_size(800.0, 600.0);
        assert!(w <= 800.0 && h <= 600.0);
    }
}

/// A line in `gangli.log`.
pub(crate) fn glog(msg: &str) {
    crate::log_to_file(&format!("gui: {msg}"));
}
