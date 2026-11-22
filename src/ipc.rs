//! CLI ↔ console. When a console is open (GUI / tray, or `gangli console run`), the CLI asks its local
//! HTTP API on 127.0.0.1 (port and token in the `[console]` table of `gangli.console`, an owner-only file),
//! and gets the live view at once. Otherwise the command runs in a one-shot console inside the CLI process.
//! No background process is ever started.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::inventory::Inventory;
use crate::proto::PathInfo;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcOp {
    Status,
    NetworkCreate { name: String },
    NetworkAdd { code: String, name: Option<String> },
    NetworkCodes { name: String },
    NetworkRemove { name: String },
    /// A network's extra relays (on top of the n0 ones): shown, or replaced with `set` (an empty list clears
    /// them); sent to its machines.
    NetworkRelays { name: String, #[serde(default)] set: Option<Vec<String>> },
    Nodes { network: Option<String> },
    Info { node: String },
    Forget { node: String },
    /// API for programs and LLM agents (token management: CLI / GUI only, never through an API token).
    ApiStatus,
    ApiEnable,
    ApiDisable,
    ApiTokenAdd { name: String, scope: String },
    ApiTokenRemove { name: String },
    TunnelList,
    TunnelAdd { name: String, node: String, target: String, listen: Option<String> },
    TunnelRemove { name: String },
    TunnelSet { name: String, enabled: bool },
    SshStatus,
    SshPassword { node: String, user: Option<String> },
    SshLoginAdd { hosts: Vec<String>, users: Vec<String>, password: Option<String>, key: Option<String> },
    SshLoginRemove { index: usize },
    Launch { node: String, launcher: Option<String>, user: Option<String>, #[serde(default)] term: Option<String> },
    TermList { node: String },
    TermKill { node: String, name: String },
    Exec { node: String, user: Option<String>, command: String, timeout: Option<u64> },
    JobStart { node: String, user: Option<String>, command: String },
    JobList { node: String },
    JobOutput { node: String, job: String, from: Option<u64> },
    JobKill { node: String, job: String },
    Update { nodes: Vec<String>, from: String },
    DebugDrop { node: String },
    /// Bring the GUI window of this console up (a second `gangli` was started in the folder).
    GuiShow,
    Stop,
}

impl IpcOp {
    /// Scope an API token needs for this operation; `None`: only the local CLI / GUI may do it.
    pub fn scope(&self) -> Option<&'static str> {
        match self {
            IpcOp::Status | IpcOp::Nodes { .. } | IpcOp::Info { .. } => Some("read"),
            IpcOp::NetworkRelays { set: None, .. } => Some("read"),
            IpcOp::NetworkRelays { .. } => Some("admin"),
            IpcOp::NetworkCreate { .. } | IpcOp::NetworkAdd { .. } | IpcOp::NetworkCodes { .. } | IpcOp::NetworkRemove { .. } | IpcOp::Forget { .. } => Some("admin"),
            IpcOp::TunnelList => Some("read"),
            IpcOp::TermList { .. } | IpcOp::TermKill { .. } => Some("exec"),
            IpcOp::Exec { .. } | IpcOp::JobStart { .. } | IpcOp::JobList { .. } | IpcOp::JobOutput { .. } | IpcOp::JobKill { .. } => Some("exec"),
            IpcOp::TunnelAdd { .. } | IpcOp::TunnelRemove { .. } | IpcOp::TunnelSet { .. } => Some("admin"),
            // They show passwords.
            IpcOp::SshStatus | IpcOp::SshPassword { .. } | IpcOp::Launch { .. } | IpcOp::Update { .. } => Some("admin"),
            // An admin token can do what the CLI and the GUI do (an AI agent managing the machines), except
            // the API itself and its tokens.
            IpcOp::SshLoginAdd { .. } | IpcOp::SshLoginRemove { .. } | IpcOp::Stop => Some("admin"),
            IpcOp::DebugDrop { .. } | IpcOp::GuiShow => None,
            IpcOp::ApiStatus | IpcOp::ApiEnable | IpcOp::ApiDisable | IpcOp::ApiTokenAdd { .. } | IpcOp::ApiTokenRemove { .. } => None,
        }
    }
}

/// A machine as the console shows it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeView {
    pub network: String,
    pub id: String,
    pub name: String,
    pub online: bool,
    /// Session start while online.
    #[serde(default)]
    pub since: Option<u64>,
    pub last_seen: u64,
    #[serde(default)]
    pub path: Option<PathInfo>,
    #[serde(default)]
    pub inventory: Option<Inventory>,
}

/// Port and token of this folder's console.
struct Local {
    port: u16,
    token: String,
}

/// The open console, if any.
fn connect_open() -> Option<(TcpStream, Local)> {
    let st: crate::console::ConsoleState = crate::store::load(&crate::paths::console_file()).ok()?;
    if st.console.token.is_empty() {
        return None;
    }
    let addr: SocketAddr = ([127, 0, 0, 1], st.console.port).into();
    let s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    Some((s, Local { port: st.console.port, token: st.console.token }))
}

fn ask(mut s: TcpStream, pf: Local, op: IpcOp) -> Result<Value> {
    let _ = pf.port;
    s.set_read_timeout(Some(Duration::from_secs(120)))?;
    let v = crate::http::post_blocking(&mut s, "/v1/op", &pf.token, &serde_json::to_string(&op)?)?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v["data"].clone())
    } else {
        Err(anyhow!("{}", v["error"].as_str().unwrap_or("console error")))
    }
}

/// Run one console operation: on the open console if there is one, else in a one-shot console here.
pub fn call(op: IpcOp) -> Result<Value> {
    match connect_open() {
        Some((s, pf)) => ask(s, pf, op),
        None => crate::console::run_once(op),
    }
}

/// Only on an open console (`console stop`).
pub fn call_open(op: IpcOp) -> Result<Option<Value>> {
    match connect_open() {
        Some((s, pf)) => ask(s, pf, op).map(Some),
        None => Ok(None),
    }
}
