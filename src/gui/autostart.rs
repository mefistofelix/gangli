//! "Start gangli at login" (an option the user turns on in the GUI): the window starts hidden in the tray
//! (`gangli gui --hidden`). It is a login item of the user, never a service: Windows `Run` key of the user,
//! XDG autostart entry on Linux, LaunchAgent (no KeepAlive) on macOS. Like the service registration, the only
//! thing gangli writes outside its folder, and only when asked.

use anyhow::{Context, Result};

fn command_line() -> Result<Vec<String>> {
    let exe = std::env::current_exe()?;
    Ok(vec![exe.display().to_string(), "gui".into(), "--hidden".into()])
}

#[cfg(windows)]
mod imp {
    use anyhow::{bail, Result};
    use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "gangli";

    pub fn enabled() -> bool {
        let mut size = 0u32;
        let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, w(KEY).as_ptr(), w(NAME).as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
        r == 0
    }

    pub fn set(on: bool) -> Result<()> {
        let r = if on {
            let line = super::command_line()?;
            let value = format!("\"{}\" {}", line[0], line[1..].join(" "));
            let data = w(&value);
            unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, w(KEY).as_ptr(), w(NAME).as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) }
        } else {
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, w(KEY).as_ptr(), w(NAME).as_ptr()) }
        };
        if r != 0 && on {
            bail!("cannot write the login item (error {r})");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::Result;

    fn path() -> Option<std::path::PathBuf> {
        Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/LaunchAgents/com.mefistofelix.gangli.gui.plist"))
    }

    pub fn enabled() -> bool {
        path().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool) -> Result<()> {
        let p = path().ok_or_else(|| anyhow::anyhow!("no HOME"))?;
        if !on {
            let _ = std::fs::remove_file(p);
            return Ok(());
        }
        let args: String = super::command_line()?.iter().map(|a| format!("\t\t<string>{a}</string>\n")).collect();
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\
             \t<string>com.mefistofelix.gangli.gui</string>\n\t<key>ProgramArguments</key>\n\t<array>\n{args}\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n"
        );
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, plist)?;
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use anyhow::Result;

    fn path() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart/gangli.desktop"))
    }

    pub fn enabled() -> bool {
        path().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool) -> Result<()> {
        let p = path().ok_or_else(|| anyhow::anyhow!("no HOME"))?;
        if !on {
            let _ = std::fs::remove_file(p);
            return Ok(());
        }
        let line = super::command_line()?;
        let exec = format!("\"{}\" {}", line[0], line[1..].join(" "));
        let entry = format!("[Desktop Entry]\nType=Application\nName=gangli\nComment=Remote management (tray)\nExec={exec}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n");
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, entry)?;
        Ok(())
    }
}

pub fn enabled() -> bool {
    imp::enabled()
}

pub fn set(on: bool) -> Result<()> {
    imp::set(on).context("login item")
}
