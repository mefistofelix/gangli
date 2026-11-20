//! gangli — one binary, two roles: the **node** (a managed machine; run by hand or installed as a system
//! service) and the **console** (the owner's side; never a service). See docs/ARCHITECTURE.md.

mod cert;
mod code;
mod console;
mod console_cmd;
mod directory;
mod elevate;
mod embed;
mod http;
mod inventory;
mod ipc;
mod node;
mod node_cmd;
mod paths;
mod proto;
mod service;
mod state;
mod store;
mod util;

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
    about = "Remote management of your machines and your family's, peer to peer.\n\
             node: a machine to manage (public code). console: where you manage them (private code)."
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
    /// node: run the node now as this user, no install (uses the embedded public code by default).
    Node {
        /// Public network code.
        #[arg(long)]
        code: Option<String>,
    },
    /// node: install the node as a system service, started at boot, full access (one elevation prompt).
    Install {
        /// Public network code; default: the one embedded in the binary, if any.
        #[arg(long, alias = "invite", alias = "join")]
        code: Option<String>,
        /// Run the service as this account instead of SYSTEM / root (sessions only as that user).
        #[arg(long)]
        user: Option<String>,
        /// The account's password (Windows only, for the service manager).
        #[arg(long)]
        password: Option<String>,
    },
    /// node: stop and remove the service and its state (one elevation prompt).
    Uninstall,
    /// node: the service body (used by the OS service manager).
    #[command(hide = true)]
    Run {
        #[arg(long)]
        service: bool,
    },
    /// node: join a network with its public code.
    Join { code: String },
    /// node: leave a network.
    Leave { network: String },
    /// node: this machine's networks.
    Status,
    /// console: networks.
    #[command(subcommand)]
    Network(NetworkCmd),
    /// console: the machines (live when a console is open).
    Nodes { network: Option<String> },
    /// console: details of a machine, asked to it now (`name` or `name.network`).
    Info { node: String },
    /// console: remove a machine from the network.
    Forget { node: String },
    /// console: keep a console open without the GUI (in this terminal).
    #[command(subcommand)]
    Console(ConsoleCmd),
    /// console: the local API for programs and LLM agents (off by default, tokens).
    #[command(subcommand)]
    Api(ApiCmd),
    /// console: saved tunnels to a machine's LAN, active while a console is open.
    #[command(subcommand)]
    Tunnel(TunnelCmd),
    /// Copy a gangli binary with a public code embedded: running it is enough to become a node.
    Pack {
        /// Public network code to embed.
        #[arg(long)]
        code: String,
        /// Binary to copy (default: this one); e.g. a release for another OS.
        #[arg(long)]
        from: Option<std::path::PathBuf>,
        /// Output file.
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Print the version.
    Version,
}

#[derive(Subcommand)]
enum NetworkCmd {
    /// Create a network: prints its public code (for machines) and private code (for consoles).
    Create { name: String },
    /// Add a network to this console with its private code.
    Add {
        code: String,
        /// Local name (default: the network's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Show the public and private codes of a network.
    Codes { name: String },
    /// Remove a network from this console only.
    Remove { name: String },
}

#[derive(Subcommand)]
enum ApiCmd {
    /// Whether it is enabled, its address, the tokens.
    Status,
    /// Enable it (on the console's local port, 127.0.0.1:7373 by default).
    Enable,
    /// Disable it (the tokens are kept).
    Disable,
    /// Access tokens.
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand)]
enum TunnelCmd {
    /// Save a tunnel: this computer's `--listen` port goes to `target` as seen from the machine.
    Add {
        name: String,
        /// The machine (name or name.network).
        #[arg(long)]
        node: String,
        /// Where to connect from the machine: `host:port` in its LAN, or just a port for its localhost.
        #[arg(long)]
        target: String,
        /// Local address or port (default: a free port, kept).
        #[arg(long)]
        listen: Option<String>,
    },
    /// Remove a tunnel.
    Remove { name: String },
    /// The saved tunnels.
    List,
    /// Turn a tunnel on.
    On { name: String },
    /// Turn a tunnel off (kept in the list).
    Off { name: String },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Make a token (printed once).
    Add {
        name: String,
        /// read: status, machines, details. admin: also networks and removing machines.
        #[arg(long, default_value = "read")]
        scope: String,
    },
    /// Revoke a token.
    Remove { name: String },
    /// The tokens (names and scopes, never the values).
    List,
}

#[derive(Subcommand)]
enum ConsoleCmd {
    /// Open a console in the foreground (live sessions, CLI answers at once).
    Run,
    /// Close the open console.
    Stop,
    /// Console id and networks.
    Status,
}

fn io_err(e: std::io::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

fn pack(code_s: &str, from: Option<std::path::PathBuf>, out: &std::path::Path, json: bool) -> anyhow::Result<()> {
    let code::Code::Public { .. } = code::decode(code_s)? else {
        anyhow::bail!("only a public code can be embedded (never the private one)");
    };
    let from = match from {
        Some(p) => p,
        None => std::env::current_exe()?,
    };
    embed::pack(&from, out, code_s.trim())?;
    let macho = embed::is_macho(out);
    if json {
        println!("{}", serde_json::json!({ "packed": out, "needs_signing": macho }));
    } else {
        println!("{} ready: running it makes the machine a node", out.display());
        if macho {
            println!("macOS binary: sign it again on a Mac (`codesign -s - {}`), or ship the original with a gangli.code file next to it", out.display());
        }
    }
    Ok(())
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
            if embed::embedded_code().is_some() {
                println!("this binary carries a network code: `gangli node` to start, `gangli install` to install");
            }
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
        Some(Command::Node { code }) => node_cmd::run(code),
        Some(Command::Install { code, user, password }) => {
            let code = code.or_else(embed::embedded_code);
            match code.as_deref().map(code::decode) {
                Some(Err(e)) => Err(e),
                Some(Ok(code::Code::Private { .. })) => Err(anyhow::anyhow!("that is a private code: never put it on a managed machine")),
                _ => service::install(&service::InstallOpts { code, user, password }).map_err(io_err),
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
        Some(Command::Join { code }) => node_cmd::join(&code, json),
        Some(Command::Leave { network }) => node_cmd::leave(&network, json),
        Some(Command::Status) => node_cmd::status(json),
        Some(Command::Network(NetworkCmd::Create { name })) => console_cmd::network_create(&name, json),
        Some(Command::Network(NetworkCmd::Add { code, name })) => console_cmd::network_add(&code, name, json),
        Some(Command::Network(NetworkCmd::Codes { name })) => console_cmd::network_codes(&name, json),
        Some(Command::Network(NetworkCmd::Remove { name })) => console_cmd::network_remove(&name, json),
        Some(Command::Nodes { network }) => console_cmd::nodes(network, json),
        Some(Command::Info { node }) => console_cmd::info(&node, json),
        Some(Command::Forget { node }) => console_cmd::forget(&node, json),
        Some(Command::Console(ConsoleCmd::Run)) => console::run_foreground(),
        Some(Command::Console(ConsoleCmd::Stop)) => console_cmd::stop(json),
        Some(Command::Console(ConsoleCmd::Status)) => console_cmd::status(json),
        Some(Command::Api(a)) => console_cmd::api(
            match a {
                ApiCmd::Status | ApiCmd::Token(TokenCmd::List) => ipc::IpcOp::ApiStatus,
                ApiCmd::Enable => ipc::IpcOp::ApiEnable,
                ApiCmd::Disable => ipc::IpcOp::ApiDisable,
                ApiCmd::Token(TokenCmd::Add { name, scope }) => ipc::IpcOp::ApiTokenAdd { name, scope },
                ApiCmd::Token(TokenCmd::Remove { name }) => ipc::IpcOp::ApiTokenRemove { name },
            },
            json,
        ),
        Some(Command::Tunnel(t)) => console_cmd::tunnel(
            match t {
                TunnelCmd::Add { name, node, target, listen } => ipc::IpcOp::TunnelAdd { name, node, target, listen },
                TunnelCmd::Remove { name } => ipc::IpcOp::TunnelRemove { name },
                TunnelCmd::List => ipc::IpcOp::TunnelList,
                TunnelCmd::On { name } => ipc::IpcOp::TunnelSet { name, enabled: true },
                TunnelCmd::Off { name } => ipc::IpcOp::TunnelSet { name, enabled: false },
            },
            json,
        ),
        Some(Command::Pack { code, from, out }) => pack(&code, from, &out, json),
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

/// The node in the foreground (Linux / macOS service, `gangli node`).
pub(crate) fn agent_run() -> std::io::Result<()> {
    agent_loop(Arc::new(AtomicBool::new(false)));
    Ok(())
}

/// Shared by the foreground run and the Windows service handler, which sets `stop`.
pub(crate) fn agent_loop(stop: Arc<AtomicBool>) {
    node::run_blocking(stop);
}

/// Node log line: stderr (visible when run by hand) and `gangli.log`.
pub(crate) fn log_line(msg: &str) {
    eprintln!("{} {msg}", util::now());
    log_to_file(&format!("node: {msg}"));
}

/// Append a timestamped line to `gangli.log` next to the binary, restarting it past 1 MB.
pub(crate) fn log_to_file(msg: &str) {
    use std::io::Write;
    let path = paths::log_file();
    if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::write(&path, b"");
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", util::now());
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
        let cli = super::Cli::try_parse_from(["gangli", "install", "--code", "abc"]).unwrap();
        assert!(matches!(cli.command, Some(super::Command::Install { code: Some(_), .. })));
        let cli = super::Cli::try_parse_from(["gangli", "install", "--invite", "abc"]).unwrap();
        assert!(matches!(cli.command, Some(super::Command::Install { code: Some(_), .. })));
        let cli = super::Cli::try_parse_from(["gangli", "network", "add", "k", "--json"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Some(super::Command::Network(super::NetworkCmd::Add { .. }))));
    }
}
