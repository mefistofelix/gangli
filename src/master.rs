//! CLI commands that change this machine's service state: create a network (become its master), make join
//! and pairing codes, list nodes, join / leave a network, status. They run elevated (the state is owner-only)
//! and talk to the running service only through the locked state file.

use anyhow::{anyhow, bail, Result};
use serde_json::json;

use crate::code::{self, Kind};
use crate::proto::NodeRec;
use crate::state::{self, CodeRec, MasterNet, Network, Role};
use crate::util::{human_ago, human_bytes, now, random16};
use crate::{elevate, store};

fn admin() -> Result<()> {
    elevate::ensure_admin().map_err(|e| anyhow!("{e}"))
}

fn print_json(v: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

pub fn create(name: &str, json: bool) -> Result<()> {
    admin()?;
    let name = crate::util::slug(name);
    let key = iroh::SecretKey::generate();
    let id = key.public().to_string();
    state::node_key()?; // make sure the node has its identity
    state::update(|st| {
        if st.networks.iter().any(|n| n.name == name && matches!(n.role, Role::Master(_))) {
            bail!("a network named {name} already exists here");
        }
        st.networks.push(Network {
            name: name.clone(),
            id: Some(id.clone()),
            role: Role::Master(MasterNet { key: store::secret_to_hex(&key), codes: vec![], nodes: vec![], clients: vec![] }),
        });
        Ok(())
    })?;
    if json {
        print_json(json!({ "network": name, "id": id }));
    } else {
        println!("network {name} created, this machine is its master");
        println!("next: `gangli master code {name}` for the machines, `gangli master pair {name}` for your client");
    }
    Ok(())
}

fn new_code(network: &str, kind: Kind, expires: Option<u64>, uses: Option<u32>, label: Option<String>) -> Result<String> {
    admin()?;
    let master = state::node_key()?.public();
    let secret = random16();
    let rec = CodeRec { kind, secret: data_encoding::HEXLOWER.encode(&secret), created: now(), expires, uses_left: uses, label };
    state::update(|st| {
        let m = st
            .networks
            .iter_mut()
            .filter(|n| n.name == network)
            .find_map(|n| match &mut n.role {
                Role::Master(m) => Some(m),
                _ => None,
            })
            .ok_or_else(|| anyhow!("no network {network} is managed here (see `gangli status`)"))?;
        let t = now();
        m.codes.retain(|c| c.valid(t)); // drop the expired / used ones
        m.codes.push(rec);
        Ok(())
    })?;
    Ok(code::encode(kind, &master, &secret))
}

pub fn join_code(network: &str, uses: Option<u32>, days: Option<u64>, label: Option<String>, json: bool) -> Result<()> {
    let expires = days.map(|d| now() + d * 86400);
    let c = new_code(network, Kind::Join, expires, uses, label)?;
    if json {
        print_json(json!({ "code": c, "network": network, "expires": expires, "uses": uses }));
    } else {
        println!("{c}");
        eprintln!("on the machine to add: gangli install --join {c}");
    }
    Ok(())
}

pub fn pair_code(network: &str, json: bool) -> Result<()> {
    let expires = now() + 15 * 60;
    let c = new_code(network, Kind::Pair, Some(expires), Some(1), None)?;
    if json {
        print_json(json!({ "code": c, "network": network, "expires": expires }));
    } else {
        println!("{c}");
        eprintln!("valid 15 minutes, once. On your client: gangli pair {c}");
    }
    Ok(())
}

pub fn nodes(network: Option<&str>, json: bool) -> Result<()> {
    admin()?;
    let st = state::load()?;
    let mut rows: Vec<(String, NodeRec)> = vec![];
    for n in &st.networks {
        if let Role::Master(m) = &n.role {
            if network.is_none_or(|w| w == n.name) {
                rows.extend(m.nodes.iter().map(|x| (n.name.clone(), x.clone())));
            }
        }
    }
    if json {
        let v: Vec<_> = rows.iter().map(|(net, n)| json!({ "network": net, "node": n })).collect();
        print_json(json!(v));
    } else {
        print_nodes(&rows);
    }
    Ok(())
}

/// Shared by `master nodes` and the client's `nodes`.
pub fn print_nodes(rows: &[(String, NodeRec)]) {
    if rows.is_empty() {
        println!("no nodes yet");
        return;
    }
    println!(
        "{:<16} {:<10} {:<8} {:<24} {:<16} {:>7} {:>15} {:<10} {}",
        "NAME", "NETWORK", "STATE", "OS", "LAN IP", "RAM", "DISK FREE", "ACCESS", "SEEN"
    );
    let t = now();
    for (net, n) in rows {
        let inv = n.inventory.clone().unwrap_or_default();
        let online = t.saturating_sub(n.last_seen) < 150;
        let os = format!("{} {}", inv.os, inv.os_version.trim_start_matches(&inv.os).trim());
        let disk = inv
            .disks
            .iter()
            .max_by_key(|d| d.total)
            .map(|d| format!("{}/{}", human_bytes(d.avail), human_bytes(d.total)))
            .unwrap_or_default();
        let access = if inv.full_access { "full".to_string() } else { inv.service_user.clone() };
        let path = n.path.as_ref().map(|p| if p.relay { "relay" } else { "direct" }).unwrap_or("");
        println!(
            "{:<16} {:<10} {:<8} {:<24} {:<16} {:>7} {:>15} {:<10} {} {}",
            n.name,
            net,
            if online { "online" } else { "offline" },
            truncate(&os, 24),
            inv.ips.first().cloned().unwrap_or_default(),
            human_bytes(inv.mem_total),
            disk,
            access,
            human_ago(t.saturating_sub(n.last_seen)),
            path
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

pub fn join(code_str: &str, json: bool) -> Result<()> {
    admin()?;
    let name = state::add_join(code_str)?;
    if json {
        print_json(json!({ "pending": name }));
    } else {
        println!("join recorded ({name}); the gangli service enrolls with the master within a few seconds");
        println!("check with: gangli status");
    }
    Ok(())
}

pub fn leave(network: &str, json: bool) -> Result<()> {
    admin()?;
    let removed = state::update(|st| {
        let before = st.networks.len();
        st.networks.retain(|n| !(n.name == network && matches!(n.role, Role::Agent(_))));
        Ok(before - st.networks.len())
    })?;
    if removed == 0 {
        bail!("this machine is not a member of {network}");
    }
    if json {
        print_json(json!({ "left": network }));
    } else {
        println!("left {network}");
    }
    Ok(())
}

pub fn status(json: bool) -> Result<()> {
    admin()?;
    let me = state::node_key()?.public();
    let st = state::load()?;
    let t = now();
    let mut nets = vec![];
    for n in &st.networks {
        match &n.role {
            Role::Agent(a) => nets.push(json!({
                "name": n.name,
                "role": "agent",
                "id": n.id,
                "state": if a.cert.is_some() { "joined" } else { "pending" },
                "node_name": a.node_name,
                "last_report": a.last_report,
                "error": a.last_error,
            })),
            Role::Master(m) => nets.push(json!({
                "name": n.name,
                "role": "master",
                "id": n.id,
                "nodes": m.nodes.len(),
                "online": m.nodes.iter().filter(|x| t.saturating_sub(x.last_seen) < 150).count(),
                "clients": m.clients.len(),
                "codes": m.codes.iter().filter(|c| c.valid(t)).count(),
            })),
        }
    }
    if json {
        print_json(json!({ "version": crate::build_version(), "node_id": me.to_string(), "networks": nets }));
        return Ok(());
    }
    println!("{}", crate::version_line());
    println!("node id {me}");
    if nets.is_empty() {
        println!("no networks: `gangli join <code>` to be managed, `gangli master create <name>` to manage");
    }
    for v in nets {
        if v["role"] == "agent" {
            let mut line = format!("agent  {:<16} {}", v["name"].as_str().unwrap_or(""), v["state"].as_str().unwrap_or(""));
            if let Some(nn) = v["node_name"].as_str() {
                line += &format!(" as {nn}");
            }
            if let Some(r) = v["last_report"].as_u64().filter(|r| *r > 0) {
                line += &format!(", last report {} ago", human_ago(t.saturating_sub(r)));
            }
            if let Some(e) = v["error"].as_str() {
                line += &format!(", error: {e}");
            }
            println!("{line}");
        } else {
            println!(
                "master {:<16} {} nodes ({} online), {} clients, {} active codes",
                v["name"].as_str().unwrap_or(""),
                v["nodes"],
                v["online"],
                v["clients"],
                v["codes"]
            );
        }
    }
    Ok(())
}
