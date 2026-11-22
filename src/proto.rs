//! Wire protocol `gangli/2` (section 5 of docs/ARCHITECTURE.md). Every stream carries length-prefixed JSON
//! frames (u32 big endian + JSON). Whoever dialed, **the console opens the streams and the node serves
//! them**: a node that knocks on a console then waits for the console's `Hello` on the same connection.
//!
//! Who may send what (enforced by the receiver):
//! - node → console, first stream of a connection the node opened: `Knock` (an admitted node shows its node
//!   certificate; a new node shows the join secret of the public code);
//! - console → console: `Sync` (console certificate; both sides merge node, console and removed lists);
//! - console → node: `Hello` (console certificate naming the connecting endpoint) opens the session stream,
//!   which then carries [`Frame`]s both ways; further streams (`Info`, ...) only after a valid `Hello`.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use iroh::endpoint::Connection;
use iroh::{Endpoint, PublicKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::cert::Cert;
use crate::inventory::Inventory;
use crate::util::Ctx;

pub const ALPN: &[u8] = b"gangli/2";
pub const MAX_FRAME: usize = 4 << 20;
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// How a connection currently reaches the peer.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PathInfo {
    pub relay: bool,
    pub rtt_ms: u64,
    pub remote: String,
}

/// A node as the consoles know it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeRec {
    pub id: String,
    pub name: String,
    pub joined: u64,
    /// Last time a console had a session with it (or now, while it has one).
    pub last_seen: u64,
    #[serde(default)]
    pub inventory: Option<Inventory>,
    /// Signed by the network key: lets another console accept the node it never admitted itself.
    #[serde(default)]
    pub cert: Option<Cert>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConsoleRec {
    pub id: String,
    pub name: String,
    /// Last time this console was seen running.
    pub seen: u64,
}

/// A persistent terminal on a node.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TermInfo {
    pub name: String,
    pub user: String,
    pub started: u64,
    pub attached: usize,
}

/// A background job on a node.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobInfo {
    pub id: String,
    pub user: String,
    pub command: String,
    pub started: u64,
    pub running: bool,
    pub exit: Option<i32>,
    /// Bytes of output so far (stdout and stderr together).
    pub output: u64,
}

/// A network's extra iroh relays (docs/RELAY.md), on top of the n0 ones. Set on a console; between consoles
/// the most recent setting wins (`at`, unix seconds; 0: never set).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Relays {
    pub urls: Vec<String>,
    pub at: u64,
}

/// What a new node gets back when its join secret is accepted.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Admission {
    pub network_name: String,
    pub node_name: String,
    pub cert: Cert,
    /// Consoles the node may knock on.
    pub consoles: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Req {
    Knock {
        network: String,
        #[serde(default)]
        cert: Option<Cert>,
        #[serde(default)]
        join: Option<String>,
        inventory: Inventory,
    },
    Sync {
        cert: Cert,
        nodes: Vec<NodeRec>,
        consoles: Vec<ConsoleRec>,
        removed: Vec<String>,
        #[serde(default)]
        relays: Option<Relays>,
    },
    /// `relays`: the network's extra relays, when a console set them.
    Hello {
        cert: Cert,
        consoles: Vec<String>,
        #[serde(default)]
        relays: Option<Vec<String>>,
    },
    Info,
    /// Console → node: connect to `target` (the machine's LAN or localhost); after `Forwarding` the stream
    /// carries the raw bytes both ways.
    Forward { target: String },
    /// Console → node: a shell (no `command`) or a command, with a terminal (`pty`) or plain pipes, as `user`
    /// (`system`, `desktop` or an account name). After `Started` the stream carries [`SFrame`]s.
    Shell {
        user: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        pty: Option<(u16, u16)>,
        #[serde(default)]
        term: String,
    },
    /// Console → node: `inner` (a `Shell` or a `Forward`) as a resumable stream named `id` (see resume.rs).
    Stream { id: String, inner: Box<Req> },
    /// Console → node, after a drop: reattach stream `id`; `received` = bytes the console got so far.
    Resume { id: String, received: u64 },
    /// Console → node: background jobs, which keep running (and buffering their output) without a console.
    JobStart { user: String, command: String },
    JobList,
    JobOutput { id: String, from: u64 },
    JobKill { id: String },
    /// Console → node: a new binary follows (`size` raw bytes after this frame), signed with the release key.
    Update { size: u64 },
    /// Console → node, inside a `Stream`: attach to the persistent terminal `name` (started if missing).
    Attach { name: String, user: String, cols: u16, rows: u16 },
    TermList,
    TermKill { name: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Resp {
    Knocked {
        #[serde(default)]
        admission: Option<Admission>,
    },
    Synced {
        nodes: Vec<NodeRec>,
        consoles: Vec<ConsoleRec>,
        removed: Vec<String>,
        #[serde(default)]
        relays: Option<Relays>,
    },
    Welcome { node_name: String, inventory: Inventory },
    Info { node_name: String, inventory: Inventory },
    Forwarding,
    Started,
    Resumed { received: u64 },
    Updating,
    Terms { terms: Vec<TermInfo> },
    Job { id: String },
    Jobs { jobs: Vec<JobInfo> },
    JobOutput { data: String, from: u64, next: u64, running: bool, exit: Option<i32> },
    Error { message: String },
}

/// On the session stream, after `Hello` / `Welcome`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    Ping,
    Pong,
    /// Node → console, only when the stable part of the inventory changed.
    Inventory { inventory: Inventory },
    /// Console → node: the consoles the node may knock on and, when a console set them, the network's extra
    /// relays (`None`: never set, the node keeps what it has).
    Consoles {
        ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relays: Option<Vec<String>>,
    },
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> Result<()> {
    let body = serde_json::to_vec(v)?;
    if body.len() > MAX_FRAME {
        bail!("message too large");
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await.ctx("send")?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.ctx("receive")?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("message too large");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.ctx("receive")?;
    Ok(serde_json::from_slice(&body)?)
}

pub fn path_info(conn: &Connection) -> Option<PathInfo> {
    let paths = conn.paths();
    let p = paths.iter().find(|p| p.is_selected())?;
    let remote = format!("{:?}", p.remote_addr());
    Some(PathInfo { relay: p.is_relay(), rtt_ms: p.rtt().as_millis() as u64, remote: tidy_addr(&remote) })
}

/// `Ip(1.2.3.4:5)` → `1.2.3.4:5`; relay addresses stay as they are.
fn tidy_addr(s: &str) -> String {
    s.strip_prefix("Ip(").and_then(|r| r.strip_suffix(')')).unwrap_or(s).to_string()
}

pub async fn connect(ep: &Endpoint, peer: PublicKey) -> Result<Connection> {
    tokio::time::timeout(DIAL_TIMEOUT, ep.connect(peer, ALPN))
        .await
        .map_err(|_| anyhow!("{} not reachable (offline?)", peer.fmt_short()))?
        .ctx("connect")
}

/// One request on a new stream of an open connection. Error answers become `Err`.
pub async fn request(conn: &Connection, req: &Req) -> Result<Resp> {
    let fut = async {
        let (mut send, mut recv) = conn.open_bi().await.ctx("open stream")?;
        write_frame(&mut send, req).await?;
        let resp: Resp = read_frame(&mut recv).await?;
        let _ = send.finish();
        Ok::<_, anyhow::Error>(resp)
    };
    let resp = tokio::time::timeout(CALL_TIMEOUT, fut).await.map_err(|_| anyhow!("no answer"))??;
    if let Resp::Error { message } = resp {
        return Err(anyhow!("{message}"));
    }
    Ok(resp)
}

/// Dial, ask once, close.
pub async fn call(ep: &Endpoint, peer: PublicKey, req: &Req) -> Result<Resp> {
    let conn = connect(ep, peer).await?;
    let r = request(&conn, req).await;
    conn.close(0u32.into(), b"done");
    r
}

/// Session stream frames (binary: kind, u32 length, payload). Console → node: `Data` (stdin), `Resize`,
/// `Eof`. Node → console: `Data` (stdout / terminal), `Stderr`, `Exit`.
pub mod sframe {
    use anyhow::{bail, Result};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub const DATA: u8 = 1;
    pub const STDERR: u8 = 2;
    pub const RESIZE: u8 = 3;
    pub const EOF: u8 = 4;
    pub const EXIT: u8 = 5;

    pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(5 + payload.len());
        buf.push(kind);
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        w.write_all(&buf).await?;
        Ok(())
    }

    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u8, Vec<u8>)> {
        let mut head = [0u8; 5];
        r.read_exact(&mut head).await?;
        let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
        if len > (4 << 20) {
            bail!("frame too large");
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok((head[0], body))
    }

    pub fn resize(cols: u16, rows: u16) -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&cols.to_be_bytes());
        b[2..].copy_from_slice(&rows.to_be_bytes());
        b
    }
}
