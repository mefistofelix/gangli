//! systemd integration. The unit runs `gangli run` as root, restarts on failure, and waits for the network.

use std::io::{self, Write};
use std::process::Command;

use crate::paths::{install_bin, SERVICE_NAME};

fn unit_path() -> String {
    format!("/etc/systemd/system/{SERVICE_NAME}.service")
}

pub fn install(join: Option<&str>) -> io::Result<()> {
    crate::elevate::ensure_root()?;
    let bin = install_bin();
    super::copy_self_to(&bin)?;
    super::create_state_dir()?;
    if let Some(code) = join {
        crate::config::store_pending_join(code)?;
    }

    let unit = format!(
        "[Unit]\n\
         Description=gangli remote management\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} run\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = bin.display()
    );
    let mut f = std::fs::File::create(unit_path())?;
    f.write_all(unit.as_bytes())?;

    run_ok("systemctl", &["daemon-reload"])?;
    run_ok("systemctl", &["enable", "--now", SERVICE_NAME])?;
    println!("installed and started the gangli service");
    Ok(())
}

pub fn uninstall() -> io::Result<()> {
    crate::elevate::ensure_root()?;
    // Ignore errors from stop/disable: the service may already be gone.
    let _ = Command::new("systemctl").args(["disable", "--now", SERVICE_NAME]).status();
    let _ = std::fs::remove_file(unit_path());
    let _ = Command::new("systemctl").arg("daemon-reload").status();
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
