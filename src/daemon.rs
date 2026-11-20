//! The service body: one iroh endpoint (the node key) serving `gangli/ctrl/1`.
//! - As master of a network it admits agents (join codes), pairs clients (pairing codes), records reports
//!   and answers node lists.
//! - As agent it enrolls with its master when a join is pending, then reports its inventory every minute,
//!   and answers `Info` to paired clients.
//! Phase 3: one connection per request. Persistent connections and resumable streams come in phase 4.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, PublicKey};

use crate::cert::{Cert, CertKind, ONE_YEAR, TEN_YEARS};
use crate::code::Kind;
use crate::proto::{self, NodeRec, Req, Resp, CTRL_ALPN};
use crate::state::{self, ClientRec, MasterNet, Role, State};
use crate::util::{eq_ct, now, unique_name, Ctx};
use crate::{inventory, log_line, store};

const REPORT_EVERY: Duration = Duration::from_secs(60);
const JOIN_RETRY: Duration = Duration::from_secs(15);

pub fn run_blocking(stop: Arc<AtomicBool>) {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            log_line(&format!("cannot start the runtime: {e}"));
            return;
        }
    };
    rt.block_on(async {
        // Keep the service alive across transient failures (no network at boot, etc.).
        while !stop.load(Ordering::SeqCst) {
            if let Err(e) = run(stop.clone()).await {
                log_line(&format!("service error: {e:#}; retrying in 10s"));
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    });
}

async fn run(stop: Arc<AtomicBool>) -> Result<()> {
    let sk = state::node_key()?;
    let me = sk.public();
    let ep = Endpoint::builder(presets::N0).secret_key(sk).bind().await.ctx("start endpoint")?;
    let router = Router::builder(ep).accept(CTRL_ALPN, Ctrl { me }).spawn();
    log_line(&format!("{} running, node id {me}", crate::version_line()));
    let mut due: HashMap<String, Instant> = HashMap::new();
    while !stop.load(Ordering::SeqCst) {
        agent_tick(router.endpoint(), &mut due).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    log_line("stopping");
    router.shutdown().await.ctx("shutdown")?;
    Ok(())
}

/// `due` holds the next time each task may run. True (and re-armed `every` later) when that time has come.
fn take_due(due: &mut HashMap<String, Instant>, key: &str, every: Duration) -> bool {
    let now = Instant::now();
    match due.get(key) {
        Some(next) if now < *next => false,
        _ => {
            due.insert(key.to_string(), now + every);
            true
        }
    }
}

async fn agent_tick(ep: &Endpoint, due: &mut HashMap<String, Instant>) {
    let st = match state::load() {
        Ok(s) => s,
        Err(e) => {
            log_line(&format!("cannot read state: {e:#}"));
            return;
        }
    };
    for net in st.networks {
        let Role::Agent(a) = &net.role else { continue };
        let Ok(master) = a.master.parse::<PublicKey>() else { continue };
        if a.cert.is_none() {
            let Some(secret) = &a.secret else { continue };
            if !take_due(due, &format!("join:{}", net.name), JOIN_RETRY) {
                continue;
            }
            let r = join(ep, master, &net.name, secret).await;
            if let Err(e) = r {
                log_line(&format!("join {} failed: {e:#}", net.name));
                set_error(&net.name, Some(format!("{e:#}")));
            }
        } else if let Some(id) = &net.id {
            if !take_due(due, &format!("report:{id}"), REPORT_EVERY) {
                continue;
            }
            match report(ep, master, id).await {
                Ok(()) => {
                    let name = net.name.clone();
                    let _ = state::update(|st| {
                        if let Some(Role::Agent(a)) = st.networks.iter_mut().find(|n| n.name == name).map(|n| &mut n.role) {
                            a.last_report = now();
                            a.last_error = None;
                        }
                        Ok(())
                    });
                }
                Err(e) => {
                    // Retry sooner than a full period.
                    due.insert(format!("report:{id}"), Instant::now() + JOIN_RETRY);
                    if a.last_error.as_deref() != Some(&format!("{e:#}")) {
                        log_line(&format!("report to {} failed: {e:#}", net.name));
                        set_error(&net.name, Some(format!("{e:#}")));
                    }
                }
            }
        }
    }
}

fn set_error(name: &str, err: Option<String>) {
    let _ = state::update(|st| {
        if let Some(Role::Agent(a)) = st.networks.iter_mut().find(|n| n.name == name).map(|n| &mut n.role) {
            a.last_error = err;
        }
        Ok(())
    });
}

async fn join(ep: &Endpoint, master: PublicKey, pending: &str, secret: &str) -> Result<()> {
    let inventory = tokio::task::spawn_blocking(inventory::collect).await?;
    let (resp, _) = proto::call(ep, master, &Req::Join { secret: secret.to_string(), inventory }).await?;
    let Resp::Joined { network, network_name, node_name, cert, master_cert } = resp else {
        bail!("unexpected answer from the master");
    };
    let net_pub: PublicKey = network.parse().ctx("network id")?;
    master_cert.verify(&net_pub, &master, CertKind::Master)?;
    cert.verify(&net_pub, &ep.id(), CertKind::Agent)?;
    let pending = pending.to_string();
    let local = state::update(|st| {
        // A re-join replaces the previous membership of the same network.
        st.networks.retain(|n| !(n.id.as_deref() == Some(network.as_str()) && matches!(n.role, Role::Agent(_))));
        let taken: Vec<String> = st
            .networks
            .iter()
            .filter(|n| matches!(n.role, Role::Agent(_)) && n.name != pending)
            .map(|n| n.name.clone())
            .collect();
        let local = unique_name(&network_name, taken.iter().map(String::as_str));
        let n = st
            .networks
            .iter_mut()
            .find(|n| n.name == pending)
            .ok_or_else(|| anyhow!("the pending join was removed"))?;
        n.name = local.clone();
        n.id = Some(network.clone());
        if let Role::Agent(a) = &mut n.role {
            a.secret = None;
            a.cert = Some(cert);
            a.master_cert = Some(master_cert);
            a.node_name = Some(node_name.clone());
            a.last_error = None;
            a.last_report = now();
        }
        Ok(local)
    })?;
    log_line(&format!("joined network {local} as {node_name}"));
    Ok(())
}

async fn report(ep: &Endpoint, master: PublicKey, network: &str) -> Result<()> {
    let inventory = tokio::task::spawn_blocking(inventory::collect).await?;
    proto::call(ep, master, &Req::Report { network: network.to_string(), inventory }).await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct Ctrl {
    me: PublicKey,
}

impl ProtocolHandler for Ctrl {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let Ok(data) = recv.read_to_end(proto::MAX_MSG).await else { break };
            let resp = match serde_json::from_slice::<Req>(&data) {
                Ok(req) => handle(&self.me, &conn, req).unwrap_or_else(|e| Resp::Error { message: format!("{e:#}") }),
                Err(_) => Resp::Error { message: "bad request".into() },
            };
            let out = serde_json::to_vec(&resp).unwrap_or_default();
            if send.write_all(&out).await.is_err() {
                break;
            }
            let _ = send.finish();
        }
        Ok(())
    }
}

/// Find the master network with this id.
fn master_net<'a>(st: &'a mut State, id: &str) -> Result<&'a mut MasterNet> {
    st.networks
        .iter_mut()
        .filter(|n| n.id.as_deref() == Some(id))
        .find_map(|n| match &mut n.role {
            Role::Master(m) => Some(m),
            _ => None,
        })
        .ok_or_else(|| anyhow!("this machine is not the master of that network"))
}

fn handle(me: &PublicKey, conn: &Connection, req: Req) -> Result<Resp> {
    let peer = conn.remote_id();
    let peer_s = peer.to_string();
    match req {
        Req::Join { secret, inventory } => state::update(|st| {
            let t = now();
            for net in st.networks.iter_mut() {
                let net_id = net.id.clone().unwrap_or_default();
                let net_name = net.name.clone();
                let Role::Master(m) = &mut net.role else { continue };
                let Some(i) = m.codes.iter().position(|c| c.kind == Kind::Join && c.valid(t) && eq_ct(&c.secret, &secret)) else {
                    continue;
                };
                if let Some(u) = m.codes[i].uses_left.as_mut() {
                    *u -= 1;
                }
                if m.codes[i].uses_left == Some(0) {
                    m.codes.remove(i);
                }
                let key = store::secret_from_hex(&m.key)?;
                let name = match m.nodes.iter().find(|n| n.id == peer_s) {
                    Some(n) => n.name.clone(),
                    None => unique_name(&inventory.hostname, m.nodes.iter().map(|n| n.name.as_str())),
                };
                let joined = m.nodes.iter().find(|n| n.id == peer_s).map(|n| n.joined).unwrap_or(t);
                m.nodes.retain(|n| n.id != peer_s);
                m.nodes.push(NodeRec {
                    id: peer_s.clone(),
                    name: name.clone(),
                    joined,
                    last_seen: t,
                    inventory: Some(inventory),
                    path: proto::path_info(conn),
                });
                let cert = Cert::issue(&key, &peer_s, CertKind::Agent, &name, TEN_YEARS);
                let master_cert = Cert::issue(&key, &me.to_string(), CertKind::Master, "master", TEN_YEARS);
                log_line(&format!("node {name} joined network {net_name}"));
                return Ok(Resp::Joined { network: net_id, network_name: net_name, node_name: name, cert, master_cert });
            }
            Ok(Resp::Error { message: "invalid, used or expired join code".into() })
        }),
        Req::Pair { secret, name } => state::update(|st| {
            let t = now();
            for net in st.networks.iter_mut() {
                let net_id = net.id.clone().unwrap_or_default();
                let net_name = net.name.clone();
                let Role::Master(m) = &mut net.role else { continue };
                let Some(i) = m.codes.iter().position(|c| c.kind == Kind::Pair && c.valid(t) && eq_ct(&c.secret, &secret)) else {
                    continue;
                };
                m.codes.remove(i); // pairing codes are single use
                let key = store::secret_from_hex(&m.key)?;
                let name = unique_name(&name, m.clients.iter().filter(|c| c.id != peer_s).map(|c| c.name.as_str()));
                m.clients.retain(|c| c.id != peer_s);
                m.clients.push(ClientRec { id: peer_s.clone(), name: name.clone(), paired: t });
                let cert = Cert::issue(&key, &peer_s, CertKind::Client, &name, ONE_YEAR);
                let master_cert = Cert::issue(&key, &me.to_string(), CertKind::Master, "master", TEN_YEARS);
                log_line(&format!("client {name} paired with network {net_name}"));
                return Ok(Resp::Paired { network: net_id, network_name: net_name, cert, master_cert });
            }
            Ok(Resp::Error { message: "invalid, used or expired pairing code".into() })
        }),
        Req::Report { network, inventory } => state::update(|st| {
            let m = master_net(st, &network)?;
            let n = m.nodes.iter_mut().find(|n| n.id == peer_s).ok_or_else(|| anyhow!("unknown node"))?;
            n.last_seen = now();
            n.inventory = Some(inventory);
            n.path = proto::path_info(conn);
            Ok(Resp::Ok)
        }),
        Req::Nodes { cert } => {
            let mut st = state::load()?;
            let net_id = cert.body.network.clone();
            let m = master_net(&mut st, &net_id)?;
            let net_pub: PublicKey = net_id.parse().ctx("network id")?;
            cert.verify(&net_pub, &peer, CertKind::Client)?;
            if !m.clients.iter().any(|c| c.id == peer_s) {
                bail!("this client is no longer paired");
            }
            Ok(Resp::Nodes { nodes: m.nodes.clone() })
        }
        Req::Info { cert } => {
            let st = state::load()?;
            for net in &st.networks {
                let Role::Agent(a) = &net.role else { continue };
                if net.id.as_deref() != Some(cert.body.network.as_str()) || a.cert.is_none() {
                    continue;
                }
                let net_pub: PublicKey = cert.body.network.parse().ctx("network id")?;
                cert.verify(&net_pub, &peer, CertKind::Client)?;
                return Ok(Resp::Info { node_name: a.node_name.clone().unwrap_or_default(), inventory: inventory::collect() });
            }
            bail!("this node is not a member of that network")
        }
    }
}
