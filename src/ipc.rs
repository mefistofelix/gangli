//! CLI ↔ console. When a console is open (GUI / tray, or `gangli console run`), the CLI asks its local
//! HTTP API on 127.0.0.1 with the internal token from `console.port` (in `gangli-data/console`, owner-only;
//! a new token every time the console opens), and gets the live view at once. Otherwise the command runs in a one-shot console inside the CLI process.
//! No background process is ever started.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::inventory::Inventory;
use crate::proto::PathInfo;

pub const PORT_FILE: &str = "console.port";

#[derive(Serialize, Deserialize)]
pub struct PortFile {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcOp {
    Status,
    NetworkCreate { name: String },
    NetworkAdd { code: String, name: Option<String> },
    NetworkCodes { name: String },
    NetworkRemove { name: String },
    Nodes { network: Option<String> },
    Info { node: String },
    Forget { node: String },
    /// API for programs and LLM agents (token management: CLI / GUI only, never through an API token).
    ApiStatus,
    ApiEnable { port: Option<u16> },
    ApiDisable,
    ApiTokenAdd { name: String, scope: String },
    ApiTokenRemove { name: String },
    Stop,
}

impl IpcOp {
    /// Scope an API token needs for this operation; `None`: only the local CLI / GUI may do it.
    pub fn scope(&self) -> Option<&'static str> {
        match self {
            IpcOp::Status | IpcOp::Nodes { .. } | IpcOp::Info { .. } => Some("read"),
            IpcOp::NetworkCreate { .. } | IpcOp::NetworkAdd { .. } | IpcOp::NetworkCodes { .. } | IpcOp::NetworkRemove { .. } | IpcOp::Forget { .. } => Some("admin"),
            IpcOp::ApiStatus | IpcOp::ApiEnable { .. } | IpcOp::ApiDisable | IpcOp::ApiTokenAdd { .. } | IpcOp::ApiTokenRemove { .. } | IpcOp::Stop => None,
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

fn read_port_file() -> Option<PortFile> {
    let p = crate::paths::console_dir().join(PORT_FILE);
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// The open console, if any.
fn connect_open() -> Option<(TcpStream, PortFile)> {
    let pf = read_port_file()?;
    let addr: SocketAddr = ([127, 0, 0, 1], pf.port).into();
    let s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    Some((s, pf))
}

fn ask(mut s: TcpStream, pf: PortFile, op: IpcOp) -> Result<Value> {
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
