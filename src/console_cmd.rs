//! Console CLI commands: they run on the open console (GUI / tray / `console run`) when there is one, or in
//! a one-shot console in this process ([`crate::ipc::call`]), with human or `--json` output.

use anyhow::Result;
use serde_json::{json, Value};

use crate::inventory::Inventory;
use crate::ipc::{self, IpcOp, NodeView};
use crate::proto::PathInfo;
use crate::util::{human_ago, human_bytes, now};

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn print_codes(v: &Value) {
    let n = v["network"].as_str().unwrap_or("");
    println!("network {n}");
    println!();
    println!("public code — for the machines to manage (`gangli node --code …`, `gangli install --code …`):");
    println!("  {}", v["public_code"].as_str().unwrap_or(""));
    println!();
    println!("private code — full control of the network, for your consoles only (`gangli network add …`):");
    println!("  {}", v["private_code"].as_str().unwrap_or(""));
    println!("  keep it secret: whoever has it owns every machine of the network");
}

pub fn network_create(name: &str, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::NetworkCreate { name: name.to_string() })?;
    if json {
        print_json(&v);
    } else {
        print_codes(&v);
    }
    Ok(())
}

pub fn network_add(code: &str, name: Option<String>, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::NetworkAdd { code: code.to_string(), name })?;
    if json {
        print_json(&v);
    } else {
        println!("network {} added ({} machines found); see them with `gangli nodes`", v["network"].as_str().unwrap_or(""), v["machines"]);
    }
    Ok(())
}

pub fn network_codes(name: &str, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::NetworkCodes { name: name.to_string() })?;
    if json {
        print_json(&v);
    } else {
        print_codes(&v);
    }
    Ok(())
}

pub fn network_remove(name: &str, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::NetworkRemove { name: name.to_string() })?;
    if json {
        print_json(&v);
    } else {
        println!("network {name} removed from this console (the machines and the other consoles keep it)");
    }
    Ok(())
}

pub fn nodes(network: Option<String>, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::Nodes { network })?;
    if json {
        print_json(&v);
        return Ok(());
    }
    let rows: Vec<NodeView> = serde_json::from_value(v)?;
    print_nodes(&rows);
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

fn link(p: &Option<PathInfo>) -> String {
    match p {
        Some(p) => format!("{} {}ms", if p.relay { "relay" } else { "direct" }, p.rtt_ms),
        None => String::new(),
    }
}

pub fn print_nodes(rows: &[NodeView]) {
    if rows.is_empty() {
        println!("no machines yet: run `gangli node --code <public code>` on them (`gangli network codes <name>`)");
        return;
    }
    println!(
        "{:<16} {:<10} {:<8} {:<22} {:<16} {:>6} {:>15} {:<8} {:<13} {}",
        "NAME", "NETWORK", "STATE", "OS", "LAN IP", "RAM", "DISK FREE", "ACCESS", "LINK", "SINCE"
    );
    let t = now();
    for n in rows {
        let inv = n.inventory.clone().unwrap_or_default();
        let disk = inv
            .disks
            .iter()
            .max_by_key(|d| d.total)
            .map(|d| format!("{}/{}", human_bytes(d.avail), human_bytes(d.total)))
            .unwrap_or_default();
        let access = if inv.full_access { "full".to_string() } else { inv.service_user.clone() };
        let since = if n.online { n.since.unwrap_or(t) } else { n.last_seen };
        let ago = if since == 0 { "never".to_string() } else { human_ago(t.saturating_sub(since)) };
        println!(
            "{:<16} {:<10} {:<8} {:<22} {:<16} {:>6} {:>15} {:<8} {:<13} {}",
            truncate(&n.name, 16),
            truncate(&n.network, 10),
            if n.online { "online" } else { "offline" },
            truncate(&inv.os_label(), 22),
            inv.ips.first().cloned().unwrap_or_default(),
            if inv.mem_total > 0 { human_bytes(inv.mem_total) } else { String::new() },
            disk,
            truncate(&access, 8),
            link(&n.path),
            ago
        );
    }
}

pub fn info(node: &str, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::Info { node: node.to_string() })?;
    if json {
        print_json(&v);
        return Ok(());
    }
    let inv: Inventory = serde_json::from_value(v["inventory"].clone())?;
    let path: Option<PathInfo> = serde_json::from_value(v["path"].clone()).unwrap_or(None);
    let id = v["id"].as_str().unwrap_or("");
    println!("{} ({}) in {}", v["node"].as_str().unwrap_or(node), &id[..12.min(id.len())], v["network"].as_str().unwrap_or(""));
    println!("  os        {} ({}), kernel {}", inv.os_label(), inv.arch, inv.kernel);
    println!("  host      {}", inv.hostname);
    println!("  cpu       {} x {}", inv.cpus, inv.cpu);
    println!("  memory    {} free of {}", human_bytes(inv.mem_avail), human_bytes(inv.mem_total));
    for d in &inv.disks {
        println!("  disk      {} {} free of {}", d.mount, human_bytes(d.avail), human_bytes(d.total));
    }
    println!("  lan       {}", inv.ips.join(", "));
    println!(
        "  access    {}",
        if inv.full_access { format!("full (runs as {})", inv.service_user) } else { format!("only {}", inv.service_user) }
    );
    println!("  users     {}", inv.users.join(", "));
    println!("  uptime    {}", human_ago(inv.uptime));
    println!("  gangli    {}", inv.version);
    if let Some(p) = path {
        println!("  link      {} {} ms, {}", if p.relay { "relay" } else { "direct" }, p.rtt_ms, p.remote);
    }
    Ok(())
}

pub fn forget(node: &str, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::Forget { node: node.to_string() })?;
    if json {
        print_json(&v);
    } else {
        println!("{node} removed: it is refused from now on (it can come back only with the public code)");
    }
    Ok(())
}

pub fn status(json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::Status)?;
    if json {
        print_json(&v);
        return Ok(());
    }
    println!("{}", crate::version_line());
    println!(
        "console id {} ({})",
        v["console_id"].as_str().unwrap_or(""),
        if v["open"].as_bool() == Some(true) { "open" } else { "not open: one-shot" }
    );
    let nets = v["networks"].as_array().cloned().unwrap_or_default();
    if nets.is_empty() {
        println!("no networks: `gangli network create <name>`, or `gangli network add <private code>`");
    }
    for n in nets {
        println!("{:<16} {} machines, {} consoles", n["name"].as_str().unwrap_or(""), n["nodes"], n["consoles"]);
    }
    Ok(())
}

pub fn stop(json: bool) -> Result<()> {
    let r = ipc::call_open(IpcOp::Stop)?;
    if json {
        print_json(&json!({ "stopped": r.is_some() }));
    } else if r.is_some() {
        println!("console closed");
    } else {
        println!("no console open");
    }
    Ok(())
}

fn print_api(v: &Value) {
    let on = v["enabled"].as_bool() == Some(true);
    println!("API {} — {}", if on { "enabled" } else { "disabled" }, v["url"].as_str().unwrap_or(""));
    if on {
        println!("  (served while a console is open: GUI / tray or `gangli console run`)");
    }
    let tokens = v["tokens"].as_array().cloned().unwrap_or_default();
    if tokens.is_empty() {
        println!("  no tokens: `gangli api token add <name> --scope read|admin`");
    }
    for t in tokens {
        println!("  token {:<16} scope {}", t["name"].as_str().unwrap_or(""), t["scope"].as_str().unwrap_or(""));
    }
}

pub fn api(op: IpcOp, json: bool) -> Result<()> {
    let adding = matches!(op, IpcOp::ApiTokenAdd { .. });
    let v = ipc::call(op)?;
    if json {
        print_json(&v);
    } else if adding {
        println!("{}", v["token"].as_str().unwrap_or(""));
        eprintln!(
            "token {} (scope {}): shown only now. Use it as `Authorization: Bearer <token>` on {}{}",
            v["name"].as_str().unwrap_or(""),
            v["scope"].as_str().unwrap_or(""),
            v["url"].as_str().unwrap_or(""),
            if v["enabled"].as_bool() == Some(true) { "" } else { " — the API is disabled: `gangli api enable`" }
        );
    } else if v.get("removed").is_some() {
        println!("token {} removed", v["removed"].as_str().unwrap_or(""));
    } else {
        print_api(&v);
    }
    Ok(())
}

pub fn tunnel(op: IpcOp, json: bool) -> Result<()> {
    let v = ipc::call(op)?;
    if json {
        print_json(&v);
        return Ok(());
    }
    if let Some(name) = v.get("tunnel").and_then(|n| n.as_str()) {
        println!("tunnel {name} saved: {}", v["listen"].as_str().unwrap_or(""));
        if v["open"].as_bool() != Some(true) {
            println!("active while a console is open (GUI / tray or `gangli console run`)");
        }
        return Ok(());
    }
    if let Some(name) = v.get("removed").and_then(|n| n.as_str()) {
        println!("tunnel {name} removed");
        return Ok(());
    }
    let list = v.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no tunnels: `gangli tunnel add <name> --node <machine> --target <host:port>`");
    }
    for t in list {
        let state = match (t["enabled"].as_bool(), t["active"].as_bool()) {
            (Some(false), _) => "off",
            (_, Some(true)) => "active",
            _ => "on (console closed)",
        };
        println!(
            "{:<14} {:<22} → {:<22} via {:<14} {state}",
            t["name"].as_str().unwrap_or(""),
            t["listen"].as_str().unwrap_or(""),
            t["target"].as_str().unwrap_or(""),
            t["node"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

pub fn ssh(op: IpcOp, json: bool) -> Result<()> {
    let v = ipc::call(op)?;
    if json {
        print_json(&v);
        return Ok(());
    }
    if let Some(login) = v.get("login").and_then(|l| l.as_str()) {
        println!("login     {login}");
        println!("password  {}", v["password"].as_str().unwrap_or(""));
        println!("address   127.0.0.1:{}", v["port"]);
        println!("example   ssh -p {} {login}@127.0.0.1", v["port"]);
        return Ok(());
    }
    if let Some(i) = v.get("removed") {
        println!("login rule {i} removed");
        return Ok(());
    }
    if v.get("logins").is_none() {
        println!("login rule {} added: machines {}, users {}, password {}", v["index"], v["hosts"], v["users"], v["password"].as_str().unwrap_or(""));
        return Ok(());
    }
    println!(
        "SSH gateway {} on {}{}",
        if v["enabled"].as_bool() == Some(true) { "enabled" } else { "disabled" },
        v["listen"].as_str().unwrap_or(""),
        if v["open"].as_bool() == Some(true) { "" } else { " (runs while a console is open)" }
    );
    println!("log in as user+machine, e.g. ssh -p 2222 root+nas@127.0.0.1");
    println!();
    println!("login rules (the most specific match wins):");
    for r in v["logins"].as_array().cloned().unwrap_or_default() {
        println!(
            "  {:>2}. machines {:<20} users {:<16} password {}{}",
            r["index"],
            r["hosts"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default(),
            r["users"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default(),
            r["password"].as_str().unwrap_or(""),
            match r["keys"].as_u64() {
                Some(0) | None => String::new(),
                Some(n) => format!(", {n} key(s)"),
            }
        );
    }
    println!();
    println!("launchers (`gangli open <machine> --with <name>`):");
    for l in v["launchers"].as_array().cloned().unwrap_or_default() {
        println!("  {:<10} {}", l["name"].as_str().unwrap_or(""), l["command"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// Fill the launcher and start the program (detached from this terminal).
pub fn open(node: &str, launcher: Option<String>, user: Option<String>, json: bool) -> Result<()> {
    let v = ipc::call(IpcOp::Launch { node: node.to_string(), launcher, user })?;
    let argv: Vec<String> = serde_json::from_value(v["argv"].clone())?;
    let Some((prog, args)) = argv.split_first() else { anyhow::bail!("empty launcher command") };
    std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start {prog}: {e} (fix its path in the [[launcher]] of gangli.console)"))?;
    if json {
        print_json(&json!({ "started": v["launcher"], "gateway_open": v["gateway_open"] }));
    } else {
        println!("{} started for {node}", v["launcher"].as_str().unwrap_or(""));
        if v["gateway_open"].as_bool() != Some(true) {
            eprintln!("note: the SSH gateway runs while a console is open (GUI / tray or `gangli console run`)");
        }
    }
    Ok(())
}
