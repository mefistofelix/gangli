//! gangli — one binary, three roles (agent, master, client). See docs/ARCHITECTURE.md.

mod cert;
mod client;
mod code;
mod daemon;
mod elevate;
mod inventory;
mod master;
mod paths;
mod proto;
mod service;
mod state;
mod store;
mod util;

use std::future::Future;
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Parser, Subcommand};

pub(crate) fn build_version() -> String {
    let build = option_env!("GANGLI_BUILD").unwrap_or("dev");
    format!("0.0.{build}")
}

pub(crate) fn version_line() -> String {
    format!("gangli {} ({}-{})", build_version(), std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(Parser)]
#[command(
    name = "gangli",
    disable_version_flag = true,
    about = "Peer-to-peer remote management: agent, control center and client in one binary."
)]
struct Cli {
    /// Machine-readable output (for scripts and LLM agents).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Install the system service and start it at boot (asks for elevation once).
    Install {
        /// Join code to enroll with; defaults to a code embedded in the binary, if any.
        #[arg(long)]
        join: Option<String>,
    },
    /// Stop and remove the system service and its state (asks for elevation once).
    Uninstall,
    /// Run the service in the foreground. `--service` is set by the Windows service manager.
    Run {
        #[arg(long, hide = true)]
        service: bool,
    },
    /// This machine's networks and their state.
    Status,
    /// Let a master manage this machine (join code from `gangli master code`).
    Join { code: String },
    /// Stop being managed through a network.
    Leave { network: String },
    /// Control center: networks run by this machine.
    #[command(subcommand)]
    Master(MasterCmd),
    /// Client: pair with a network once (code from `gangli master pair`).
    Pair {
        code: String,
        /// Name of this client as the master will list it (default: hostname).
        #[arg(long)]
        name: Option<String>,
    },
    /// Client: list the machines of every paired network.
    Nodes,
    /// Client: ask a machine directly for its details (`name` or `name.network`).
    Info { node: String },
    /// Print the version.
    Version,
}

#[derive(Subcommand)]
enum MasterCmd {
    /// Create a network; this machine becomes its master.
    Create { name: String },
    /// Make a join code for machines to manage.
    Code {
        network: String,
        /// How many machines may use it (default: unlimited).
        #[arg(long)]
        uses: Option<u32>,
        /// Valid for this many days (default: forever).
        #[arg(long)]
        days: Option<u64>,
        /// A note shown in the code list.
        #[arg(long)]
        label: Option<String>,
    },
    /// Make a one-time pairing code (15 minutes) for your client.
    Pair { network: String },
    /// Machines of the networks run here.
    Nodes { network: Option<String> },
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("cannot start the async runtime")
        .block_on(f)
}

fn io_err(e: std::io::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

fn main() {
    #[allow(unused_mut)]
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    #[cfg(windows)]
    elevate::adopt_elevated_output(&mut args);

    // Bare version flags before clap so the build number shows (clap's own version flag is disabled).
    if matches!(args.get(1).and_then(|a| a.to_str()), Some("--version") | Some("-V")) {
        println!("{}", version_line());
        return;
    }

    let cli = Cli::parse_from(args);
    let json = cli.json;
    let result: anyhow::Result<()> = match cli.command {
        None => {
            println!("{}", version_line());
            println!("run `gangli --help` for the commands");
            Ok(())
        }
        Some(Command::Version) => {
            if json {
                println!("{}", serde_json::json!({ "version": build_version(), "os": std::env::consts::OS, "arch": std::env::consts::ARCH }));
            } else {
                println!("{}", version_line());
            }
            Ok(())
        }
        Some(Command::Install { join }) => {
            let embedded: Option<String> = None; // embedded trailer reader lands with `pack` (phase 8)
            let code = join.or(embedded);
            match code.as_deref().map(code::decode) {
                Some(Err(e)) => Err(e),
                Some(Ok(c)) if c.kind != code::Kind::Join => Err(anyhow::anyhow!("that is a client pairing code, not a join code")),
                _ => service::install(code.as_deref()).map_err(io_err),
            }
        }
        Some(Command::Uninstall) => service::uninstall().map_err(io_err),
        Some(Command::Run { service }) => {
            if service {
                service::run().map_err(io_err)
            } else {
                agent_run().map_err(io_err)
            }
        }
        Some(Command::Status) => master::status(json),
        Some(Command::Join { code }) => master::join(&code, json),
        Some(Command::Leave { network }) => master::leave(&network, json),
        Some(Command::Master(m)) => match m {
            MasterCmd::Create { name } => master::create(&name, json),
            MasterCmd::Code { network, uses, days, label } => master::join_code(&network, uses, days, label, json),
            MasterCmd::Pair { network } => master::pair_code(&network, json),
            MasterCmd::Nodes { network } => master::nodes(network.as_deref(), json),
        },
        Some(Command::Pair { code, name }) => block_on(client::pair(&code, name, json)),
        Some(Command::Nodes) => block_on(client::nodes(json)),
        Some(Command::Info { node }) => block_on(client::info(&node, json)),
    };
    if let Err(e) = result {
        if json {
            println!("{}", serde_json::json!({ "error": format!("{e:#}") }));
        } else {
            eprintln!("gangli: {e:#}");
        }
        std::process::exit(1);
    }
}

/// The service body in the foreground (Linux / macOS service, or `gangli run` by hand).
pub(crate) fn agent_run() -> std::io::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    install_sigterm(stop.clone());
    agent_loop(stop);
    Ok(())
}

/// Shared by the foreground run and the Windows service handler, which sets `stop`.
pub(crate) fn agent_loop(stop: Arc<AtomicBool>) {
    daemon::run_blocking(stop);
}

/// Timestamped line to `service.log` in the state dir, and to stderr (visible when run by hand).
pub(crate) fn log_line(msg: &str) {
    use std::io::Write;
    let line = format!("{} {msg}", util::now());
    eprintln!("{line}");
    let path = paths::state_dir().join("service.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
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
    use clap::Parser;

    #[test]
    fn version_names_the_binary() {
        assert!(super::version_line().starts_with("gangli 0.0."));
    }

    #[test]
    fn cli_parses() {
        let cli = super::Cli::try_parse_from(["gangli", "install", "--join", "abc"]).unwrap();
        assert!(matches!(cli.command, Some(super::Command::Install { join: Some(_) })));
        let cli = super::Cli::try_parse_from(["gangli", "master", "code", "home", "--uses", "3", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Some(super::Command::Master(super::MasterCmd::Code { uses: Some(3), .. }))));
    }
}
