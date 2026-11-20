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
    Sync { cert: Cert, nodes: Vec<NodeRec>, consoles: Vec<ConsoleRec>, removed: Vec<String> },
    Hello { cert: Cert, consoles: Vec<String> },
    Info,
    /// Console → node: connect to `target` (the machine's LAN or localhost); after `Forwarding` the stream
    /// carries the raw bytes both ways.
    Forward { target: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Resp {
    Knocked {
        #[serde(default)]
        admission: Option<Admission>,
    },
    Synced { nodes: Vec<NodeRec>, consoles: Vec<ConsoleRec>, removed: Vec<String> },
    Welcome { node_name: String, inventory: Inventory },
    Info { node_name: String, inventory: Inventory },
    Forwarding,
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
    /// Console → node: the consoles the node may knock on.
    Consoles { ids: Vec<String> },
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
