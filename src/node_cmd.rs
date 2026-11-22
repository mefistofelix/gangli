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

/// Join every code (all checked first: nothing is joined if one is bad), then say what happens next.
fn join_all(codes: &[String], json: bool) -> Result<()> {
    for c in codes {
        if let Ok(crate::code::Code::Private { .. }) = crate::code::decode(c) {
            bail!("this is a private network code: it belongs in a console (`gangli network add`), never on a managed machine");
        }
        crate::code::decode(c)?;
    }
    let names: Vec<String> = codes.iter().map(|c| state::add_join(c)).collect::<Result<_>>()?;
    let list = names.join(", ");
    if json {
        print_json(json!({ "memberships": names }));
    } else if crate::paths::service_installed() {
        println!("joined ({list}): the gangli service asks a console of each network to admit this machine");
    } else {
        println!("joined ({list}); start the node with `gangli node` (or install it as a service: `gangli install`)");
    }
    Ok(())
}

pub fn join(codes: &[String], json: bool) -> Result<()> {
    if codes.is_empty() {
        bail!("give the public code of a network: `gangli join <public code>`");
    }
    admin()?;
    join_all(codes, json)
}

/// `gangli pair`: join networks from a phone (pair.rs). Shows a QR (stdout; stderr with `--json`), waits for
/// the phone's answer, asks to confirm (in a terminal, unless `--yes`), then joins like `gangli join`.
pub fn pair(minutes: u64, yes: bool, json: bool) -> Result<()> {
    admin()?;
    let p = crate::pair::Pairing::new();
    let url = p.url();
    let show = format!(
        "{}{url}\n\nScan it with your phone's camera: the gangli wallet opens; tick the networks for this machine.\nWaiting up to {minutes} min (Ctrl+C to stop)…\n",
        crate::qr::terminal(&url)
    );
    if json {
        eprint!("{show}");
    } else {
        print!("{show}");
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let offers = rt.block_on(p.wait(std::time::Duration::from_secs(minutes * 60), None))?;
    let labels: Vec<String> = offers.iter().map(|o| o.label()).collect();
    use std::io::IsTerminal;
    if !yes && !json && std::io::stdin().is_terminal() {
        print!("\nThe phone sent: {}. Join? [Y/n] ", labels.join(", "));
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if matches!(answer.trim().to_lowercase().as_str(), "n" | "no") {
            println!("not joined");
            return Ok(());
        }
    }
    let codes: Vec<String> = offers.into_iter().map(|o| o.code).collect();
    join_all(&codes, json)
}

/// `gangli node [--code CODE]`: run the node now, in the foreground, as the current user (or root). The code
/// (or the networks the binary carries) is joined first if this machine is not in that network yet.
pub fn run(code: Option<String>) -> Result<()> {
    if crate::paths::service_installed() && !crate::elevate::is_root() {
        bail!("the gangli service is installed and already runs the node on this machine (see `gangli status`)");
    }
    let joined = match code {
        Some(c) => vec![state::add_join(&c)?],
        // The networks this binary carries, with their relays.
        None => state::join_bundled()?,
    };
    if !joined.is_empty() {
        eprintln!("network membership: {}", joined.join(", "));
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
        println!("left {network} (a running node tells the network's connected consoles, which forget this machine)");
    }
    Ok(())
}

pub fn status(json: bool) -> Result<()> {
    admin()?;
    let me = state::node_key()?.public();
    let st = state::load()?;
    let t = crate::util::now();
    let mode = if crate::paths::service_installed() { "service" } else { "user" };
    // The running node (if any) says which consoles are connected now.
    let live = crate::ipc::node_status();
    let connected = |id: &str| -> Vec<serde_json::Value> {
        live.as_ref()
            .and_then(|v| v["networks"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .find(|n| n["id"].as_str() == Some(id))
            .and_then(|n| n["connected"].as_array().cloned())
            .unwrap_or_default()
    };
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
                "connected": connected(&m.id),
                "joined": m.joined,
                "error": m.last_error,
            })
        })
        .collect();
    if json {
        print_json(json!({
            "version": crate::build_version(),
            "node_id": me.to_string(),
            "mode": mode,
            "running": live.is_some(),
            "networks": nets,
        }));
        return Ok(());
    }
    println!("{}", crate::version_line());
    println!("node id {me} ({mode} mode, {})", crate::paths::node_file().display());
    println!("{}", if live.is_some() { "the node is running" } else { "the node is not running (`gangli node`, or `gangli install`)" });
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
        let now_on: Vec<String> = v["connected"].as_array().map(|a| a.iter().map(|c| c["name"].as_str().unwrap_or("?").to_string()).collect()).unwrap_or_default();
        if !now_on.is_empty() {
            line += &format!("; connected now: {}", now_on.join(", "));
        }
        println!("{line}");
    }
    Ok(())
}
