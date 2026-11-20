//! The client role (no service, runs as the user): pair once with a master, list nodes, query a node
//! directly. State in [`crate::paths::client_dir`]: `client.key` (this client's endpoint key) and
//! `client.json` (paired networks, their certificates and the last node list).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use iroh::endpoint::presets;
use iroh::{Endpoint, PublicKey};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cert::{Cert, CertKind};
use crate::code::{self, Kind};
use crate::proto::{self, NodeRec, Req, Resp};
use crate::store;
use crate::util::{human_ago, human_bytes, now, Ctx};

const FILE: &str = "client.json";
const KEY_FILE: &str = "client.key";

#[derive(Serialize, Deserialize, Default)]
struct ClientState {
    #[serde(default)]
    networks: Vec<ClientNet>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ClientNet {
    id: String,
    name: String,
    master: String,
    cert: Cert,
    #[serde(default)]
    nodes: Vec<NodeRec>,
    #[serde(default)]
    updated: u64,
}

fn dir() -> PathBuf {
    crate::paths::client_dir()
}

async fn endpoint() -> Result<Endpoint> {
    let sk = store::secret_key(&dir(), KEY_FILE)?;
    let ep = Endpoint::builder(presets::N0).secret_key(sk).bind().await.ctx("start endpoint")?;
    // A relay connection makes the first dial faster; do not wait forever when offline.
    let _ = tokio::time::timeout(Duration::from_secs(10), ep.online()).await;
    Ok(ep)
}

fn print_json(v: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

pub async fn pair(code_str: &str, name: Option<String>, json: bool) -> Result<()> {
    let c = code::decode(code_str)?;
    if c.kind != Kind::Pair {
        bail!("this is a join code for a machine to manage: use `gangli install --join <code>` there");
    }
    let name = name.unwrap_or_else(|| crate::inventory::collect().hostname);
    let ep = endpoint().await?;
    let r = proto::call(&ep, c.master, &Req::Pair { secret: c.secret_hex(), name }).await;
    ep.close().await;
    let Resp::Paired { network, network_name, cert, master_cert } = r?.0 else {
        bail!("unexpected answer from the master");
    };
    let net_pub: PublicKey = network.parse().ctx("network id")?;
    master_cert.verify(&net_pub, &c.master, CertKind::Master)?;
    cert.verify(&net_pub, &ep.id(), CertKind::Client)?;
    store::update(&dir(), FILE, |st: &mut ClientState| {
        st.networks.retain(|n| n.id != network);
        st.networks.push(ClientNet {
            id: network.clone(),
            name: network_name.clone(),
            master: c.master.to_string(),
            cert,
            nodes: vec![],
            updated: 0,
        });
        Ok(())
    })?;
    if json {
        print_json(json!({ "paired": network_name, "id": network }));
    } else {
        println!("paired with network {network_name}; list its machines with `gangli nodes`");
    }
    Ok(())
}

/// Ask every paired master for its node list and cache it. Networks whose master does not answer keep the
/// cached list, and the error is returned next to it.
async fn refresh(ep: &Endpoint) -> Result<Vec<(ClientNet, Option<String>)>> {
    let st: ClientState = store::load(&dir(), FILE)?;
    if st.networks.is_empty() {
        bail!("not paired with any network yet: on the master run `gangli master pair <network>`, then `gangli pair <code>` here");
    }
    let mut out = vec![];
    for mut net in st.networks {
        let master: PublicKey = net.master.parse().ctx("master id")?;
        match proto::call(ep, master, &Req::Nodes { cert: net.cert.clone() }).await {
            Ok((Resp::Nodes { nodes }, _)) => {
                net.nodes = nodes;
                net.updated = now();
                out.push((net, None));
            }
            Ok(_) => out.push((net, Some("unexpected answer".into()))),
            Err(e) => out.push((net, Some(format!("{e:#}")))),
        }
    }
    let fresh: Vec<ClientNet> = out.iter().map(|(n, _)| n.clone()).collect();
    store::update(&dir(), FILE, |st: &mut ClientState| {
        st.networks = fresh;
        Ok(())
    })?;
    Ok(out)
}

pub async fn nodes(json: bool) -> Result<()> {
    let ep = endpoint().await?;
    let r = refresh(&ep).await;
    ep.close().await;
    let nets = r?;
    if json {
        let v: Vec<_> = nets
            .iter()
            .map(|(n, err)| json!({ "network": n.name, "id": n.id, "updated": n.updated, "error": err, "nodes": n.nodes }))
            .collect();
        print_json(json!(v));
        return Ok(());
    }
    let mut rows = vec![];
    for (n, err) in &nets {
        if let Some(e) = err {
            eprintln!(
                "network {}: master not reachable ({e}); showing the list from {} ago",
                n.name,
                human_ago(now().saturating_sub(n.updated))
            );
        }
        rows.extend(n.nodes.iter().map(|x| (n.name.clone(), x.clone())));
    }
    crate::master::print_nodes(&rows);
    Ok(())
}

/// Resolve `name` or `name.network` (or an endpoint id prefix) in the cached lists.
fn find(st: &ClientState, target: &str) -> Result<(ClientNet, NodeRec)> {
    let (node, net) = match target.rsplit_once('.') {
        Some((a, b)) if st.networks.iter().any(|n| n.name == b) => (a, Some(b)),
        _ => (target, None),
    };
    let mut hits = vec![];
    for n in &st.networks {
        if net.is_some_and(|w| w != n.name) {
            continue;
        }
        for x in &n.nodes {
            if x.name == node || (node.len() >= 8 && x.id.starts_with(node)) {
                hits.push((n.clone(), x.clone()));
            }
        }
    }
    match hits.len() {
        0 => Err(anyhow!("no node {target}")),
        1 => Ok(hits.remove(0)),
        _ => Err(anyhow!("{target} is in more than one network: use name.network")),
    }
}

pub async fn info(target: &str, json: bool) -> Result<()> {
    let ep = endpoint().await?;
    let r = info_inner(&ep, target).await;
    ep.close().await;
    let (net, node, node_name, inv, path) = r?;
    if json {
        print_json(json!({ "network": net.name, "node": node_name, "id": node.id, "path": path, "inventory": inv }));
        return Ok(());
    }
    println!("{} ({}) in {}", node_name, &node.id[..12.min(node.id.len())], net.name);
    println!("  os        {} {} ({}), kernel {}", inv.os, inv.os_version, inv.arch, inv.kernel);
    println!("  host      {}", inv.hostname);
    println!("  cpu       {} x {}", inv.cpus, inv.cpu);
    println!("  memory    {} free of {}", human_bytes(inv.mem_avail), human_bytes(inv.mem_total));
    for d in &inv.disks {
        println!("  disk      {} {} free of {}", d.mount, human_bytes(d.avail), human_bytes(d.total));
    }
    println!("  lan       {}", inv.ips.join(", "));
    if let Some(p) = node.path.as_ref() {
        println!("  public    {} (as seen by the master)", p.remote);
    }
    println!(
        "  access    {}",
        if inv.full_access { format!("full (service runs as {})", inv.service_user) } else { format!("only {}", inv.service_user) }
    );
    println!("  users     {}", inv.users.join(", "));
    println!("  uptime    {}", human_ago(inv.uptime));
    println!("  agent     {}", inv.version);
    if let Some(p) = path {
        println!("  link      {} {} ms", if p.relay { "relay" } else { "direct" }, p.rtt_ms);
    }
    Ok(())
}

type InfoResult = (ClientNet, NodeRec, String, crate::inventory::Inventory, Option<proto::PathInfo>);

async fn info_inner(ep: &Endpoint, target: &str) -> Result<InfoResult> {
    let st: ClientState = store::load(&dir(), FILE)?;
    let (net, node) = match find(&st, target) {
        Ok(x) => x,
        Err(_) => {
            // Not in the cache (new node?): refresh from the masters and look again.
            refresh(ep).await?;
            let st: ClientState = store::load(&dir(), FILE)?;
            find(&st, target)?
        }
    };
    let peer: PublicKey = node.id.parse().ctx("node id")?;
    let (resp, path) = proto::call(ep, peer, &Req::Info { cert: net.cert.clone() }).await?;
    let Resp::Info { node_name, inventory } = resp else {
        bail!("unexpected answer from {target}");
    };
    Ok((net, node, node_name, inventory, path))
}
