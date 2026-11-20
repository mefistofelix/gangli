//! Control protocol `gangli/ctrl/1`: one bidirectional stream per request, JSON request, JSON answer.
//! Who may send what (enforced by the receiver, see daemon.rs):
//! - anyone → master: `Join` / `Pair`, accepted only with a valid code secret;
//! - enrolled agent → master: `Report` (the master knows the agent by its endpoint id);
//! - paired client → master: `Nodes`; paired client → agent: `Info` (both carry the client certificate,
//!   which must name the connecting endpoint).

use std::time::Duration;

use anyhow::{anyhow, Result};
use iroh::endpoint::Connection;
use iroh::{Endpoint, PublicKey};
use serde::{Deserialize, Serialize};

use crate::cert::Cert;
use crate::inventory::Inventory;
use crate::util::Ctx;

pub const CTRL_ALPN: &[u8] = b"gangli/ctrl/1";
pub const MAX_MSG: usize = 4 << 20;
const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// How a connection currently reaches the peer.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PathInfo {
    pub relay: bool,
    pub rtt_ms: u64,
    pub remote: String,
}

/// A node as the master knows it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeRec {
    pub id: String,
    pub name: String,
    pub joined: u64,
    pub last_seen: u64,
    #[serde(default)]
    pub inventory: Option<Inventory>,
    /// Path of the agent's last report to the master (direct / relay, public address).
    #[serde(default)]
    pub path: Option<PathInfo>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Req {
    Join { secret: String, inventory: Inventory },
    Pair { secret: String, name: String },
    Report { network: String, inventory: Inventory },
    Nodes { cert: Cert },
    Info { cert: Cert },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Resp {
    Joined { network: String, network_name: String, node_name: String, cert: Cert, master_cert: Cert },
    Paired { network: String, network_name: String, cert: Cert, master_cert: Cert },
    Ok,
    Nodes { nodes: Vec<NodeRec> },
    Info { node_name: String, inventory: Inventory },
    Error { message: String },
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

/// Send one request and wait for the answer. Errors answered by the peer become `Err`.
pub async fn call(ep: &Endpoint, peer: PublicKey, req: &Req) -> Result<(Resp, Option<PathInfo>)> {
    let body = serde_json::to_vec(req)?;
    let fut = async {
        let conn = ep.connect(peer, CTRL_ALPN).await.ctx("connect")?;
        let (mut send, mut recv) = conn.open_bi().await.ctx("open stream")?;
        send.write_all(&body).await.ctx("send")?;
        send.finish().ctx("send")?;
        let data = recv.read_to_end(MAX_MSG).await.ctx("receive")?;
        let path = path_info(&conn);
        conn.close(0u32.into(), b"done");
        let resp: Resp = serde_json::from_slice(&data)?;
        Ok::<_, anyhow::Error>((resp, path))
    };
    let (resp, path) = tokio::time::timeout(CALL_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("no answer from {} (offline?)", peer.fmt_short()))??;
    if let Resp::Error { message } = resp {
        return Err(anyhow!("{message}"));
    }
    Ok((resp, path))
}
