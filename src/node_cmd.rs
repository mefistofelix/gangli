//! Node-side CLI commands (`node`, `join`, `leave`, `status`). The node state is the system one when the
//! service is installed (these commands then ask for elevation once), otherwise the user's own.

use anyhow::{anyhow, bail, Result};
use serde_json::json;

use crate::state;
use crate::util::human_ago;

fn admin() -> Result<()> {
    crate::elevate::ensure_admin().map_err(|e| anyhow!("{e}"))
}

fn print_json(v: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

pub fn join(code_str: &str, json: bool) -> Result<()> {
    admin()?;
    let name = state::add_join(code_str)?;
    if json {
        print_json(json!({ "membership": name }));
    } else if crate::paths::service_installed() {
        println!("joined ({name}): the gangli service asks a console of the network to admit this machine");
    } else {
        println!("joined ({name}); start the node with `gangli node` (or install it as a service: `gangli install`)");
    }
    Ok(())
}

/// `gangli node [--code CODE]`: run the node now, in the foreground, as the current user (or root). The code
/// (or the one embedded in the binary) is joined first if this machine is not in that network yet.
pub fn run(code: Option<String>) -> Result<()> {
    if crate::paths::service_installed() && !crate::elevate::is_root() {
        bail!("the gangli service is installed and already runs the node on this machine (see `gangli status`)");
    }
    if let Some(c) = code.or_else(crate::embed::embedded_code) {
        let name = state::add_join(&c)?;
        eprintln!("network membership: {name}");
    }
    if state::load()?.networks.is_empty() {
        bail!("no network to join: `gangli node --code <public code>`");
    }
    if !crate::elevate::is_root() {
        eprintln!("running as this user only: consoles can act as this user; `gangli install` gives full access");
    }
    crate::agent_run().map_err(|e| anyhow!("{e}"))
}

pub fn leave(network: &str, json: bool) -> Result<()> {
    admin()?;
    let removed = state::update(|st| {
        let before = st.networks.len();
        st.networks.retain(|n| n.name != network && !n.id.starts_with(network));
        Ok(before - st.networks.len())
    })?;
    if removed == 0 {
        bail!("this machine is not in a network named {network}");
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
    let t = crate::util::now();
    let mode = if crate::paths::service_installed() { "service" } else { "user" };
    let nets: Vec<_> = st
        .networks
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "id": m.id,
                "state": if m.cert.is_some() { "admitted" } else { "waiting for a console" },
                "node_name": m.node_name,
                "consoles": m.consoles.len(),
                "joined": m.joined,
                "error": m.last_error,
            })
        })
        .collect();
    if json {
        print_json(json!({ "version": crate::build_version(), "node_id": me.to_string(), "mode": mode, "networks": nets }));
        return Ok(());
    }
    println!("{}", crate::version_line());
    println!("node id {me} ({mode} mode, state in {})", state::dir().display());
    if nets.is_empty() {
        println!("not in any network: `gangli join <public code>`");
    }
    for (m, v) in st.networks.iter().zip(&nets) {
        let mut line = format!("{:<16} {}", m.name, v["state"].as_str().unwrap_or(""));
        if let Some(n) = &m.node_name {
            line += &format!(" as {n}");
        }
        if m.joined > 0 {
            line += &format!(", since {} ago, {} console(s)", human_ago(t.saturating_sub(m.joined)), m.consoles.len());
        }
        if let (None, Some(e)) = (&m.cert, &m.last_error) {
            line += &format!(" ({e})");
        }
        println!("{line}");
    }
    Ok(())
}
