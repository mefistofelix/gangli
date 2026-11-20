//! systemd integration. The unit runs `gangli run` from gangli's own folder as root, restarts on failure,
//! and waits for the network. The unit file is the only thing written outside that folder.

use std::io::{self, Write};
use std::process::Command;

use crate::paths::SERVICE_NAME;

fn unit_path() -> String {
    format!("/etc/systemd/system/{SERVICE_NAME}.service")
}

pub fn install(join: Option<&str>) -> io::Result<()> {
    crate::elevate::ensure_root()?;
    let bin = super::prepare_install(join)?;

    let unit = format!(
        "[Unit]\n\
         Description=gangli remote management\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{bin}\" run\n\
         WorkingDirectory={dir}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = bin.display(),
        dir = crate::paths::exe_dir().display()
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
    super::finish_uninstall()?;
    println!("removed the gangli service; the node keeps its identity in gangli-data/node (delete the folder to remove everything)");
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
