//! gangli — one binary, three roles (agent, master, client). See docs/ARCHITECTURE.md.
//! Phase 2 wires up: install / uninstall of the system service with native elevation, and a placeholder
//! service body. The iroh endpoint, networks and the client live in later phases.

mod config;
mod elevate;
mod paths;
mod service;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

fn build_version() -> String {
    let build = option_env!("GANGLI_BUILD").unwrap_or("dev");
    format!("0.0.{build}")
}

fn version_line() -> String {
    format!(
        "gangli {} ({}-{})",
        build_version(),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

#[derive(Parser)]
#[command(name = "gangli", disable_version_flag = true, about = "Peer-to-peer remote management: agent, control center and client in one binary.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Install the system service and start it at boot (asks for elevation once).
    Install {
        /// Join code to enroll on first start; defaults to a code embedded in the binary, if any.
        #[arg(long)]
        join: Option<String>,
    },
    /// Stop and remove the system service and its state (asks for elevation once).
    Uninstall,
    /// Run the service body. `--service` is set by the OS service manager; not for interactive use.
    Run {
        #[arg(long)]
        service: bool,
    },
    /// Print the version.
    Version,
}

fn main() {
    // Handle the bare version flags before clap so the build number shows (clap's own version is disabled).
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if matches!(raw.first().map(String::as_str), Some("--version") | Some("-V")) {
        println!("{}", version_line());
        return;
    }

    let cli = Cli::parse();
    let result = match cli.command {
        None => {
            // No subcommand: phase 2 has no GUI yet, so show what the binary is.
            println!("{}", version_line());
            println!("usage: gangli <install|uninstall|version> --help");
            Ok(())
        }
        Some(Command::Version) => {
            println!("{}", version_line());
            Ok(())
        }
        Some(Command::Install { join }) => {
            let embedded: Option<&str> = None; // embedded trailer reader lands with `pack` (phase 8)
            let code = join.as_deref().or(embedded);
            service::install(code)
        }
        Some(Command::Uninstall) => service::uninstall(),
        Some(Command::Run { .. }) => service::run(),
    };
    if let Err(e) = result {
        eprintln!("gangli: {e}");
        std::process::exit(1);
    }
}

/// The service body. Placeholder until phase 3: it consumes a pending join code and idles, logging a
/// heartbeat, so the service stays up and install/uninstall can be exercised end to end.
pub(crate) fn agent_run() -> std::io::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    // On unix the service manager terminates the process on stop; honour SIGTERM for a clean exit in tests.
    #[cfg(unix)]
    install_sigterm(stop.clone());
    agent_loop(stop);
    Ok(())
}

/// Shared loop, driven by a stop flag so the Windows service handler can end it.
pub(crate) fn agent_loop(stop: Arc<AtomicBool>) {
    if let Ok(Some(code)) = config::take_pending_join() {
        log_line(&format!("pending join code captured ({} chars); enrollment arrives in phase 3", code.len()));
    }
    log_line(&format!("{} started", version_line()));
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(5));
    }
    log_line("stopping");
}

fn log_line(msg: &str) {
    use std::io::Write;
    let path = paths::state_dir().join("service.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
}

#[cfg(unix)]
static STOP_FLAG: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

#[cfg(unix)]
fn install_sigterm(stop: Arc<AtomicBool>) {
    // The C signal handler can only reach a static, so stash the flag there.
    let _ = STOP_FLAG.set(stop);
    extern "C" fn handler(_sig: libc::c_int) {
        if let Some(f) = STOP_FLAG.get() {
            f.store(true, Ordering::SeqCst);
        }
    }
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_names_the_binary() {
        assert!(super::version_line().starts_with("gangli 0.0."));
    }

    #[test]
    fn cli_parses() {
        use clap::Parser;
        let cli = super::Cli::try_parse_from(["gangli", "install", "--join", "abc"]).unwrap();
        assert!(matches!(cli.command, Some(super::Command::Install { join: Some(_) })));
    }
}
