//! launchd integration. A LaunchDaemon runs `gangli run` as root at boot and is kept alive.

use std::io::{self, Write};
use std::process::Command;

use crate::paths::{install_bin, SERVICE_LABEL};

fn plist_path() -> String {
    format!("/Library/LaunchDaemons/{SERVICE_LABEL}.plist")
}

pub fn install(join: Option<&str>) -> io::Result<()> {
    crate::elevate::ensure_root()?;
    let bin = install_bin();
    super::copy_self_to(&bin)?;
    super::create_state_dir()?;
    if let Some(code) = join {
        crate::config::store_pending_join(code)?;
    }

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{bin}</string>\n\t\t<string>run</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         </dict>\n\
         </plist>\n",
        label = SERVICE_LABEL,
        bin = bin.display()
    );
    let path = plist_path();
    let mut f = std::fs::File::create(&path)?;
    f.write_all(plist.as_bytes())?;
    // LaunchDaemons must be owned by root and not group/world writable.
    run_ok("chown", &["root:wheel", &path])?;
    run_ok("chmod", &["644", &path])?;

    // bootstrap is the modern launchctl verb; fall back to load -w on older systems.
    if Command::new("launchctl")
        .args(["bootstrap", "system", &path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        println!("installed and started the gangli service");
    } else {
        run_ok("launchctl", &["load", "-w", &path])?;
        println!("installed and started the gangli service");
    }
    Ok(())
}

pub fn uninstall() -> io::Result<()> {
    crate::elevate::ensure_root()?;
    let path = plist_path();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("system/{SERVICE_LABEL}")])
        .status();
    let _ = Command::new("launchctl").args(["unload", "-w", &path]).status();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(crate::paths::state_dir());
    let _ = std::fs::remove_file(install_bin());
    println!("removed the gangli service");
    Ok(())
}

pub fn run() -> io::Result<()> {
    crate::agent_run()
}

fn run_ok(cmd: &str, args: &[&str]) -> io::Result<()> {
    let status = Command::new(cmd).args(args).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("{cmd} {} failed", args.join(" "))));
    }
    Ok(())
}
