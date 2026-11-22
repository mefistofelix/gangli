//! gangli — one binary, two roles: the **node** (a managed machine; run by hand or installed as a system
//! service) and the **console** (the owner's side; never a service). See docs/ARCHITECTURE.md.

mod cert;
mod code;
mod console;
mod console_cmd;
mod directory;
mod elevate;
mod embed;
mod gui;
mod http;
mod inventory;
mod ipc;
mod node;
mod node_cmd;
mod pair;
mod paths;
mod proc;
mod proto;
mod qr;
mod resume;
mod service;
mod sftp;
mod ssh;
mod state;
mod store;
mod term;
mod update;
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
    /// The window: machines, networks, tunnels, this computer (also what a double click opens).
    Gui {
        /// Start in the tray, without the window (used at login).
        #[arg(long)]
        hidden: bool,
    },
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
    /// node: join a network from your phone: shows a QR; scan it with the phone (gangli wallet), pick the network.
    Pair {
        /// Minutes to wait for the phone.
        #[arg(long, default_value_t = 10)]
        minutes: u64,
    },
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
    /// console: the SSH gateway (127.0.0.1:2222, logins `user+machine`).
    #[command(subcommand)]
    Ssh(SshCmd),
    /// console: open a machine with an external program from the launchers (PuTTY, OpenSSH, WinSCP…).
    Open {
        node: String,
        /// Launcher name (default: the first in gangli.console).
        #[arg(long = "with")]
        launcher: Option<String>,
        /// Account on the machine (default: its desktop user, else the service account).
        #[arg(long)]
        user: Option<String>,
        /// Attach to (or start) the persistent terminal with this name.
        #[arg(long)]
        term: Option<String>,
    },
    /// console: persistent terminals of a machine (they keep running when you disconnect).
    #[command(subcommand)]
    Term(TermCmd),
    /// node: SFTP server on stdin / stdout (started by the node for SFTP sessions).
    #[command(hide = true, name = "sftp-server")]
    SftpServer,
    /// console: run a command on a machine and wait: its output and exit code (for scripts and LLMs).
    Exec {
        node: String,
        /// Account on the machine (default: its desktop user, else the service account).
        #[arg(long)]
        user: Option<String>,
        /// Give up after this many seconds (default 300).
        #[arg(long)]
        timeout: Option<u64>,
        /// The command (after `--`).
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// console: background jobs on a machine: they keep running and keep their output without a console.
    #[command(subcommand)]
    Job(JobCmd),
    /// console: send the machines a new release (from a folder: `gh release download build-N -D <dir>`).
    Update {
        /// Machines (default: every machine online now).
        nodes: Vec<String>,
        /// Folder with the release binaries (gangli-linux-x86_64, gangli-windows-x86_64.exe, …).
        #[arg(long)]
        from: String,
    },
    /// Tests: cut the open console's connection to a machine (resumable streams must survive it).
    #[command(hide = true, name = "debug-drop")]
    DebugDrop { node: String },
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
    Codes {
        name: String,
        /// Also show the network as a QR for the phone (gangli wallet; the public code only).
        #[arg(long)]
        qr: bool,
    },
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
enum TermCmd {
    /// The persistent terminals of a machine.
    List { node: String },
    /// End a persistent terminal (its shell is killed).
    Kill { node: String, name: String },
}

#[derive(Subcommand)]
enum JobCmd {
    /// Start a command in the background.
    Start {
        node: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// The jobs of a machine.
    List { node: String },
    /// A job's output (`--follow` until it ends).
    Output {
        node: String,
        job: String,
        #[arg(long)]
        follow: bool,
    },
    /// Stop a job.
    Kill { node: String, job: String },
}

#[derive(Subcommand)]
enum SshCmd {
    /// Gateway address, login rules (with their passwords) and launchers.
    Status,
    /// The login and password that apply to a machine (and user).
    Password {
        node: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Login rules.
    #[command(subcommand)]
    Login(LoginCmd),
}

#[derive(Subcommand)]
enum LoginCmd {
    /// Add a rule (default: every machine, every user, a generated password).
    Add {
        /// Machines, comma separated (`nas,nonna-pc`), or `*`.
        #[arg(long, value_delimiter = ',')]
        hosts: Vec<String>,
        /// Accounts, comma separated (`root,desktop`), or `*`.
        #[arg(long, value_delimiter = ',')]
        users: Vec<String>,
        #[arg(long)]
        password: Option<String>,
        /// An OpenSSH public key (`ssh-ed25519 AAAA…`).
        #[arg(long)]
        key: Option<String>,
    },
    /// Remove a rule by its number (`gangli ssh status`).
    Remove { index: usize },
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
        /// read: machines and their details. exec: also commands, jobs, terminals. admin: everything the CLI and
        /// the GUI do but the API tokens (for an AI agent that manages the machines).
        #[arg(long, default_value = "read")]
        scope: String,
        /// Write the token with instructions for an AI agent into this file (readable by you only).
        #[arg(long)]
        save: Option<std::path::PathBuf>,
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
    qr::enable_terminal_colours();

    // Bare version flags before clap so the build number shows (clap's own version flag is disabled).
    if matches!(args.get(1).and_then(|a| a.to_str()), Some("--version") | Some("-V")) {
        println!("{}", version_line());
        return;
    }

    let cli = Cli::parse_from(args);
    let json = cli.json;
    let result: anyhow::Result<()> = match cli.command {
        None if gui::started_from_desktop() => gui::run(false),
        Some(Command::Gui { hidden }) => gui::run(hidden),
        None => {
            println!("{}", version_line());
            if embed::embedded_code().is_some() {
                println!("this binary carries a network code: `gangli node` to start, `gangli install` to install");
            }
            println!("`gangli gui` opens the window; `gangli --help` lists the commands");
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
        Some(Command::Pair { minutes }) => node_cmd::pair(minutes, json),
        Some(Command::Leave { network }) => node_cmd::leave(&network, json),
        Some(Command::Status) => node_cmd::status(json),
        Some(Command::Network(NetworkCmd::Create { name })) => console_cmd::network_create(&name, json),
        Some(Command::Network(NetworkCmd::Add { code, name })) => console_cmd::network_add(&code, name, json),
        Some(Command::Network(NetworkCmd::Codes { name, qr })) => console_cmd::network_codes(&name, qr, json),
        Some(Command::Network(NetworkCmd::Remove { name })) => console_cmd::network_remove(&name, json),
        Some(Command::Nodes { network }) => console_cmd::nodes(network, json),
        Some(Command::Info { node }) => console_cmd::info(&node, json),
        Some(Command::Forget { node }) => console_cmd::forget(&node, json),
        Some(Command::Console(ConsoleCmd::Run)) => console::run_foreground(),
        Some(Command::Console(ConsoleCmd::Stop)) => console_cmd::stop(json),
        Some(Command::Console(ConsoleCmd::Status)) => console_cmd::status(json),
        Some(Command::Api(a)) => {
            let save = match &a {
                ApiCmd::Token(TokenCmd::Add { save, .. }) => save.clone(),
                _ => None,
            };
            let op = match a {
                ApiCmd::Status | ApiCmd::Token(TokenCmd::List) => ipc::IpcOp::ApiStatus,
                ApiCmd::Enable => ipc::IpcOp::ApiEnable,
                ApiCmd::Disable => ipc::IpcOp::ApiDisable,
                ApiCmd::Token(TokenCmd::Add { name, scope, .. }) => ipc::IpcOp::ApiTokenAdd { name, scope },
                ApiCmd::Token(TokenCmd::Remove { name }) => ipc::IpcOp::ApiTokenRemove { name },
            };
            console_cmd::api(op, json, save)
        }
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
        Some(Command::Ssh(s)) => console_cmd::ssh(
            match s {
                SshCmd::Status => ipc::IpcOp::SshStatus,
                SshCmd::Password { node, user } => ipc::IpcOp::SshPassword { node, user },
                SshCmd::Login(LoginCmd::Add { hosts, users, password, key }) => ipc::IpcOp::SshLoginAdd { hosts, users, password, key },
                SshCmd::Login(LoginCmd::Remove { index }) => ipc::IpcOp::SshLoginRemove { index },
            },
            json,
        ),
        Some(Command::Open { node, launcher, user, term }) => console_cmd::open(&node, launcher, user, term, json),
        Some(Command::Term(TermCmd::List { node })) => console_cmd::term(ipc::IpcOp::TermList { node }, json),
        Some(Command::Term(TermCmd::Kill { node, name })) => console_cmd::term(ipc::IpcOp::TermKill { node, name }, json),
        Some(Command::SftpServer) => sftp::serve().map_err(io_err),
        Some(Command::Exec { node, user, timeout, command }) => console_cmd::exec(&node, user, timeout, command.join(" "), json),
        Some(Command::Job(j)) => match j {
            JobCmd::Start { node, user, command } => console_cmd::job(ipc::IpcOp::JobStart { node, user, command: command.join(" ") }, json),
            JobCmd::List { node } => console_cmd::job(ipc::IpcOp::JobList { node }, json),
            JobCmd::Output { node, job, follow } => console_cmd::job_output(&node, &job, follow, json),
            JobCmd::Kill { node, job } => console_cmd::job(ipc::IpcOp::JobKill { node, job }, json),
        },
        Some(Command::Update { nodes, from }) => console_cmd::update(nodes, from, json),
        Some(Command::DebugDrop { node }) => ipc::call(ipc::IpcOp::DebugDrop { node }).map(|v| println!("{v}")),
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
