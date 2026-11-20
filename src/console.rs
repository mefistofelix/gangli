//! The console (section 5 of docs/ARCHITECTURE.md): the owner's side, a user process, never a service.
//! It lives inside whatever uses it:
//! - **open** — the GUI / tray, or `gangli console run` in a terminal: keeps one live session per node, so
//!   the node list is current without asking anybody, serves the local HTTP API (CLI, GUI, programs) and
//!   the saved tunnels;
//! - **one-shot** — a CLI command when no console is open: connects, answers, exits.
//!
//! Every console holding a network's key (the private code) is an owner of it, with equal rights: it admits
//! nodes showing the join secret, keeps the public directory up to date, and syncs node, console and
//! removed lists with the other consoles it finds there.
//!
//! Idle cost per online node: a ping every 25 s. Offline nodes are retried quickly right after a drop, then
//! only every 5 minutes as a safety net (their own knock is what normally brings them back).
//!
//! State: `gangli.console` next to the binary (TOML). The open console owns its local port: a second one
//! cannot bind it, so there is no lock file.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Result};
use data_encoding::HEXLOWER;
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Notify};

use crate::cert::{Cert, CertKind, TEN_YEARS};
use crate::code::{self, Code};
use crate::directory::{self, Directory};
use crate::inventory::Inventory;
use crate::ipc::{IpcOp, NodeView};
use crate::proto::{self, Admission, ConsoleRec, Frame, NodeRec, Req, Resp};
use crate::store;
use crate::util::{eq_ct, now, random16, slug, unique_name, Ctx};

const HEADER: &str = "gangli.console — your console: networks and their PRIVATE keys, machines, API, tunnels.
Whoever has this file owns every machine of its networks: keep it private, never put it on a managed machine.
gangli rewrites this file when its state changes (comments other than this header are not kept) and picks
up edits by hand within 2 seconds while a console is open. See docs/ARCHITECTURE.md.";
const PING_EVERY: Duration = Duration::from_secs(25);
const PONG_TIMEOUT_SECS: u64 = 70;
const SWEEP_EVERY: Duration = Duration::from_secs(300);
const RECENT: u64 = 600;
const REPUBLISH_EVERY: Duration = Duration::from_secs(1800);
const ONE_SHOT_WAIT: Duration = Duration::from_secs(6);

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ConsoleState {
    #[serde(default)]
    pub console: ConsoleConf,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub ssh: crate::ssh::SshConf,
    #[serde(default, rename = "launcher")]
    pub launchers: Vec<crate::ssh::Launcher>,
    #[serde(default, rename = "tunnel")]
    pub tunnels: Vec<Tunnel>,
    #[serde(default, rename = "network")]
    pub networks: Vec<ConsoleNet>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ConsoleConf {
    /// This console's secret key (hex), made on first start.
    #[serde(default)]
    pub key: String,
    /// Local HTTP port (127.0.0.1) of the open console: CLI, GUI and, when enabled, the API.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Token of the local CLI / GUI (made on first start).
    #[serde(default)]
    pub token: String,
}

fn default_port() -> u16 {
    7373
}

impl Default for ConsoleConf {
    fn default() -> Self {
        ConsoleConf { key: String::new(), port: default_port(), token: String::new() }
    }
}

/// The API for programs and LLM agents on the console's local port: off unless enabled (CLI or GUI), with
/// named tokens. The local CLI / GUI use the console's own token and never need this.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct ApiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, rename = "token")]
    pub tokens: Vec<ApiToken>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ApiToken {
    pub name: String,
    /// `read` or `admin`.
    pub scope: String,
    pub token: String,
    pub created: u64,
}

/// A saved tunnel: active whenever a console is open.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Tunnel {
    pub name: String,
    /// Machine name (or name.network).
    pub node: String,
    /// On this computer, e.g. `127.0.0.1:8080`.
    pub listen: String,
    /// As seen from the machine: its LAN or localhost, e.g. `192.168.1.1:80`.
    pub target: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ConsoleNet {
    pub id: String,
    pub name: String,
    /// The network key (hex): what makes this console an owner.
    pub key: String,
    /// Machines removed with `forget`: their knocks are refused.
    #[serde(default)]
    pub removed: Vec<String>,
    /// This console's certificate in the network (signed with the key above).
    pub cert: Cert,
    #[serde(default, rename = "console")]
    pub consoles: Vec<ConsoleRec>,
    #[serde(default, rename = "node")]
    pub nodes: Vec<NodeRec>,
}

fn path() -> std::path::PathBuf {
    crate::paths::console_file()
}

pub(crate) fn clog(msg: &str) {
    crate::log_to_file(&format!("console: {msg}"));
}

struct Live {
    conn: Arc<Connection>,
    since: u64,
    tx: mpsc::UnboundedSender<Frame>,
}

pub struct Console {
    me: PublicKey,
    open: bool,
    router: OnceLock<Router>,
    state: Mutex<ConsoleState>,
    live: Mutex<HashMap<String, Live>>,
    dialing: Mutex<HashSet<String>>,
    /// Node id → (next attempt, backoff).
    retry: Mutex<HashMap<String, (Instant, Duration)>>,
    /// Network id → directory as last read or published.
    published: Mutex<HashMap<String, Directory>>,
    started: Instant,
    pub stop: Notify,
    /// Modification time of `gangli.console` after our last write (to notice hand edits).
    saved_at: Mutex<Option<SystemTime>>,
    /// Running tunnels: name → (definition, listener task).
    tunnels: Mutex<HashMap<String, (Tunnel, tokio::task::JoinHandle<()>)>>,
}

#[derive(Clone)]
struct Handler {
    c: Arc<Console>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConsoleHandler")
    }
}

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        incoming(self.c.clone(), Arc::new(conn)).await;
        Ok(())
    }
}

/// Load `gangli.console`, making the console key and the local token on first use.
pub fn load_state() -> Result<ConsoleState> {
    let st: ConsoleState = store::load(&path())?;
    if store::secret_from_hex(&st.console.key).is_ok() && !st.console.token.is_empty() && !st.ssh.logins.is_empty() && !st.launchers.is_empty() {
        return Ok(st);
    }
    store::update(&path(), HEADER, |st: &mut ConsoleState| {
        if store::secret_from_hex(&st.console.key).is_err() {
            st.console.key = store::secret_to_hex(&SecretKey::generate());
        }
        if st.console.token.is_empty() {
            st.console.token = HEXLOWER.encode(&random16());
        }
        if st.ssh.logins.is_empty() {
            st.ssh.logins.push(crate::ssh::default_rule());
        }
        if st.launchers.is_empty() {
            st.launchers = crate::ssh::default_launchers();
        }
        Ok(st.clone())
    })
}

/// Start a console in this process. `open`: own the local port, keep sessions, refresh the directory,
/// serve the API and the tunnels.
pub async fn start(open: bool) -> Result<Arc<Console>> {
    let state = load_state()?;
    let sk = store::secret_from_hex(&state.console.key)?;
    let me = sk.public();
    let port = state.console.port;
    // The port is the single-instance guard: bind it before anything else.
    let listener = if open {
        Some(TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            anyhow!("cannot open the console on 127.0.0.1:{port} ({e}): another console is already open, or change [console] port in gangli.console")
        })?)
    } else {
        None
    };
    let ep = Endpoint::builder(presets::N0).secret_key(sk).bind().await.ctx("start endpoint")?;
    let c = Arc::new(Console {
        me,
        open,
        router: OnceLock::new(),
        state: Mutex::new(state),
        live: Mutex::default(),
        dialing: Mutex::default(),
        retry: Mutex::default(),
        published: Mutex::default(),
        started: Instant::now(),
        stop: Notify::new(),
        saved_at: Mutex::new(store::modified(&path())),
        tunnels: Mutex::default(),
    });
    let router = Router::builder(ep).accept(proto::ALPN, Handler { c: c.clone() }).spawn();
    let _ = c.router.set(router);
    if let Some(listener) = listener {
        spawn_http(c.clone(), listener);
        sync_tunnels(&c);
        crate::ssh::start(c.clone()).await;
        tokio::spawn(manager(c.clone()));
        tokio::spawn(directory_loop(c.clone()));
        tokio::spawn(watch_file(c.clone()));
        clog(&format!("{} open on 127.0.0.1:{port}, id {me}", crate::version_line()));
    }
    Ok(c)
}

/// Hand edits of `gangli.console` while open: reload (keeping the live inventories) and apply.
async fn watch_file(c: Arc<Console>) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let m = store::modified(&path());
        if m.is_none() || m == *c.saved_at.lock().unwrap() {
            continue;
        }
        match store::load::<ConsoleState>(&path()) {
            Ok(mut fresh) => {
                {
                    let mut st = c.state.lock().unwrap();
                    for n in fresh.networks.iter_mut() {
                        for x in n.nodes.iter_mut() {
                            x.inventory = st
                                .networks
                                .iter()
                                .flat_map(|o| o.nodes.iter())
                                .find(|o| o.id == x.id)
                                .and_then(|o| o.inventory.clone());
                        }
                    }
                    // Keep the identity and local token of the running console.
                    fresh.console.key = st.console.key.clone();
                    fresh.console.token = st.console.token.clone();
                    *st = fresh;
                }
                *c.saved_at.lock().unwrap() = m;
                clog("gangli.console changed by hand: applied");
                sync_tunnels(&c);
            }
            Err(e) => {
                // Half-written or mistyped: say it once, keep the running state.
                *c.saved_at.lock().unwrap() = m;
                clog(&format!("gangli.console not applied: {e:#}"));
            }
        }
    }
}

impl Console {
    pub(crate) fn ssh_conf(&self) -> crate::ssh::SshConf {
        self.state.lock().unwrap().ssh.clone()
    }

    pub(crate) fn secret_key(&self) -> Result<SecretKey> {
        store::secret_from_hex(&self.state.lock().unwrap().console.key)
    }

    pub(crate) fn node_name(&self, net: &str, node: &str) -> Option<String> {
        let st = self.state.lock().unwrap();
        st.networks.iter().find(|n| n.id == net)?.nodes.iter().find(|x| x.id == node).map(|x| x.name.clone())
    }

    fn endpoint(&self) -> &Endpoint {
        self.router.get().expect("router set at start").endpoint()
    }

    pub async fn shutdown(&self) {
        for (_, (_, task)) in self.tunnels.lock().unwrap().drain() {
            task.abort();
        }
        for l in self.live.lock().unwrap().values() {
            l.conn.close(0u32.into(), b"console closed");
        }
        if let Some(r) = self.router.get() {
            let _ = r.shutdown().await;
        }
    }

    /// Write `gangli.console` (live inventories stay in memory: they are not configuration).
    fn save(&self) {
        let mut snapshot = self.state.lock().unwrap().clone();
        for n in snapshot.networks.iter_mut() {
            for x in n.nodes.iter_mut() {
                x.inventory = None;
            }
        }
        match store::save(&path(), HEADER, &snapshot) {
            Ok(()) => *self.saved_at.lock().unwrap() = store::modified(&path()),
            Err(e) => clog(&format!("cannot save gangli.console: {e:#}")),
        }
    }

    fn console_ids(&self, n: &ConsoleNet) -> Vec<String> {
        let mut v: Vec<&ConsoleRec> = n.consoles.iter().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.seen));
        let mut ids: Vec<String> = vec![self.me.to_string()];
        ids.extend(v.into_iter().map(|c| c.id.clone()).filter(|i| *i != self.me.to_string()));
        ids
    }

    fn hello_for(&self, net: &str) -> Option<(Cert, Vec<String>)> {
        let st = self.state.lock().unwrap();
        st.networks.iter().find(|n| n.id == net).map(|n| (n.cert.clone(), self.console_ids(n)))
    }

    fn key_of(&self, net: &str) -> Option<SecretKey> {
        let st = self.state.lock().unwrap();
        st.networks.iter().find(|n| n.id == net).and_then(|n| store::secret_from_hex(&n.key).ok())
    }

    fn on_online(&self, node: &str, net: &str, conn: Arc<Connection>, tx: mpsc::UnboundedSender<Frame>, node_name: String, inventory: Inventory) {
        let t = now();
        if let Some(old) = self.live.lock().unwrap().insert(node.to_string(), Live { conn, since: t, tx }) {
            old.conn.close(0u32.into(), b"replaced by a newer session");
        }
        self.retry.lock().unwrap().remove(node);
        {
            let mut st = self.state.lock().unwrap();
            if let Some(n) = st.networks.iter_mut().find(|n| n.id == net) {
                let others: Vec<String> = n.nodes.iter().filter(|x| x.id != node).map(|x| x.name.clone()).collect();
                if let Some(x) = n.nodes.iter_mut().find(|x| x.id == node) {
                    x.last_seen = t;
                    x.inventory = Some(inventory);
                    // The name in the node's certificate is the network-wide one.
                    if !node_name.is_empty() && x.name != node_name {
                        x.name = unique_name(&node_name, others.iter().map(String::as_str));
                    }
                }
            }
        }
        self.save();
    }

    fn on_offline(&self, node: &str, conn: &Arc<Connection>) {
        let mut live = self.live.lock().unwrap();
        if live.get(node).is_some_and(|l| Arc::ptr_eq(&l.conn, conn)) {
            live.remove(node);
            drop(live);
            let t = now();
            {
                let mut st = self.state.lock().unwrap();
                for n in st.networks.iter_mut() {
                    if let Some(x) = n.nodes.iter_mut().find(|x| x.id == node) {
                        x.last_seen = t;
                    }
                }
            }
            self.save();
            self.retry.lock().unwrap().insert(node.to_string(), (Instant::now() + Duration::from_secs(3), Duration::from_secs(3)));
        }
    }

    fn on_inventory(&self, node: &str, inventory: Inventory) {
        {
            let mut st = self.state.lock().unwrap();
            for n in st.networks.iter_mut() {
                if let Some(x) = n.nodes.iter_mut().find(|x| x.id == node) {
                    x.inventory = Some(inventory.clone());
                    x.last_seen = now();
                }
            }
        }
        self.save();
    }

    /// Tell the live nodes of a network about the current console list.
    fn push_consoles(&self, net: &str) {
        let (ids, nodes): (Vec<String>, Vec<String>) = {
            let st = self.state.lock().unwrap();
            match st.networks.iter().find(|n| n.id == net) {
                Some(n) => (self.console_ids(n), n.nodes.iter().map(|x| x.id.clone()).collect()),
                None => return,
            }
        };
        let live = self.live.lock().unwrap();
        for id in nodes {
            if let Some(l) = live.get(&id) {
                let _ = l.tx.send(Frame::Consoles { ids: ids.clone() });
            }
        }
    }

    /// A node knocks: an admitted node (certificate) or a new one (join secret of the public code).
    fn knock(&self, peer: &PublicKey, network: &str, cert: Option<Cert>, join: Option<String>, inventory: Inventory) -> Result<Option<Admission>> {
        let peer_s = peer.to_string();
        let t = now();
        let res = {
            let mut st = self.state.lock().unwrap();
            let me = self.me.to_string();
            let n = st.networks.iter_mut().find(|n| n.id == network).ok_or_else(|| anyhow!("unknown network"))?;
            let key = store::secret_from_hex(&n.key)?;
            if let Some(cert) = cert {
                if n.removed.contains(&peer_s) {
                    bail!("this machine was removed from the network");
                }
                cert.verify(&key.public(), peer, CertKind::Node)?;
                if let Some(x) = n.nodes.iter_mut().find(|x| x.id == peer_s) {
                    x.inventory = Some(inventory);
                    if x.cert.is_none() {
                        x.cert = Some(cert);
                    }
                } else {
                    // Admitted by another console of the network: its certificate is enough.
                    let name = unique_name(&cert.body.name, n.nodes.iter().map(|x| x.name.as_str()));
                    n.nodes.push(NodeRec { id: peer_s.clone(), name, joined: t, last_seen: t, inventory: Some(inventory), cert: Some(cert) });
                }
                None
            } else {
                let join = join.ok_or_else(|| anyhow!("bad knock"))?;
                if !eq_ct(&join, &code::join_secret_hex(&key)) {
                    bail!("wrong public code for this network");
                }
                n.removed.retain(|r| r != &peer_s);
                let old = n.nodes.iter().find(|x| x.id == peer_s).cloned();
                let name = match &old {
                    Some(x) => x.name.clone(),
                    None => unique_name(&inventory.hostname, n.nodes.iter().map(|x| x.name.as_str())),
                };
                let cert = Cert::issue(&key, &peer_s, CertKind::Node, &name, TEN_YEARS);
                n.nodes.retain(|x| x.id != peer_s);
                n.nodes.push(NodeRec {
                    id: peer_s.clone(),
                    name: name.clone(),
                    joined: old.map(|x| x.joined).unwrap_or(t),
                    last_seen: t,
                    inventory: Some(inventory),
                    cert: Some(cert.clone()),
                });
                clog(&format!("machine {name} admitted into {}", n.name));
                let mut consoles = vec![me.clone()];
                consoles.extend(n.consoles.iter().map(|c| c.id.clone()).filter(|i| *i != me));
                Some(Admission { network_name: n.name.clone(), node_name: name, cert, consoles })
            }
        };
        self.save();
        Ok(res)
    }

    /// Merge what another console (or the directory) knows.
    fn merge(&self, net: &str, nodes: Vec<NodeRec>, consoles: Vec<ConsoleRec>, removed: Vec<String>) -> bool {
        let mut changed = false;
        {
            let mut st = self.state.lock().unwrap();
            let Some(n) = st.networks.iter_mut().find(|n| n.id == net) else { return false };
            for r in removed {
                if !n.removed.contains(&r) {
                    n.removed.push(r);
                    changed = true;
                }
            }
            let removed = n.removed.clone();
            n.nodes.retain(|x| !removed.contains(&x.id));
            for x in nodes.into_iter().filter(|x| !removed.contains(&x.id)) {
                match n.nodes.iter_mut().find(|y| y.id == x.id) {
                    Some(y) => {
                        if x.last_seen > y.last_seen {
                            y.last_seen = x.last_seen;
                            if x.inventory.is_some() {
                                y.inventory = x.inventory;
                            }
                        }
                        if let Some(c) = x.cert {
                            if y.cert.is_none() {
                                y.name = c.body.name.clone();
                                y.cert = Some(c);
                                changed = true;
                            }
                        }
                    }
                    None => {
                        let name = x.cert.as_ref().map(|c| c.body.name.clone()).unwrap_or(x.name.clone());
                        let name = unique_name(&name, n.nodes.iter().map(|y| y.name.as_str()));
                        n.nodes.push(NodeRec { name, ..x });
                        changed = true;
                    }
                }
            }
            for c in consoles {
                match n.consoles.iter_mut().find(|d| d.id == c.id) {
                    Some(d) => {
                        d.seen = d.seen.max(c.seen);
                        if d.name.is_empty() {
                            d.name = c.name;
                        }
                    }
                    None => {
                        n.consoles.push(c);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.save();
        }
        changed
    }

    fn sync_payload(&self, net: &str) -> Option<(Cert, Vec<NodeRec>, Vec<ConsoleRec>, Vec<String>)> {
        let st = self.state.lock().unwrap();
        let n = st.networks.iter().find(|n| n.id == net)?;
        let mut consoles = n.consoles.clone();
        if let Some(me) = consoles.iter_mut().find(|c| c.id == self.me.to_string()) {
            me.seen = now();
        }
        Some((n.cert.clone(), n.nodes.clone(), consoles, n.removed.clone()))
    }

    fn sync_answer(&self, peer: &PublicKey, cert: &Cert, nodes: Vec<NodeRec>, consoles: Vec<ConsoleRec>, removed: Vec<String>) -> Result<Resp> {
        let net = cert.body.network.clone();
        let key = self.key_of(&net).ok_or_else(|| anyhow!("unknown network"))?;
        cert.verify(&key.public(), peer, CertKind::Console)?;
        self.merge(&net, nodes, consoles, removed);
        let (_, nodes, consoles, removed) = self.sync_payload(&net).ok_or_else(|| anyhow!("unknown network"))?;
        Ok(Resp::Synced { nodes, consoles, removed })
    }

    fn known_nodes(&self) -> Vec<(String, String, u64)> {
        let st = self.state.lock().unwrap();
        st.networks
            .iter()
            .flat_map(|n| n.nodes.iter().filter(|x| !n.removed.contains(&x.id)).map(|x| (n.id.clone(), x.id.clone(), x.last_seen)))
            .collect()
    }

    fn live_conn(&self, node: &str) -> Option<Arc<Connection>> {
        self.live.lock().unwrap().get(node).map(|l| l.conn.clone())
    }

    /// The directory this console would publish for `net` now.
    fn directory_now(&self, net: &str) -> Option<(SecretKey, Directory)> {
        let st = self.state.lock().unwrap();
        let live = self.live.lock().unwrap();
        let n = st.networks.iter().find(|n| n.id == net)?;
        let key = store::secret_from_hex(&n.key).ok()?;
        let mut nodes: Vec<&NodeRec> = n.nodes.iter().filter(|x| !n.removed.contains(&x.id)).collect();
        nodes.sort_by_key(|x| std::cmp::Reverse((live.contains_key(&x.id), x.last_seen)));
        Some((key, Directory { name: n.name.clone(), consoles: self.console_ids(n), nodes: nodes.into_iter().map(|x| x.id.clone()).collect() }))
    }
}

/// Read the directory, merge it, sync with the consoles listed there, publish if what we know differs.
async fn refresh_network(c: &Arc<Console>, net: &str, publish: bool) {
    let Some(key) = c.key_of(net) else { return };
    let found = directory::resolve(&key.public(), Some(&key)).await.ok();
    if let Some(d) = &found {
        let t = now();
        let nodes = d.nodes.iter().map(|id| NodeRec { id: id.clone(), name: id[..8].to_string(), joined: t, last_seen: 0, inventory: None, cert: None }).collect();
        let consoles = d.consoles.iter().map(|id| ConsoleRec { id: id.clone(), name: String::new(), seen: 0 }).collect();
        c.merge(net, nodes, consoles, vec![]);
        // Sync with the other consoles (both sides merge).
        let mut tasks = vec![];
        for other in d.consoles.iter().filter(|o| **o != c.me.to_string()) {
            let Ok(peer) = other.parse::<PublicKey>() else { continue };
            let Some((cert, nodes, consoles, removed)) = c.sync_payload(net) else { continue };
            let (c2, net2) = (c.clone(), net.to_string());
            tasks.push(tokio::spawn(async move {
                if let Ok(Resp::Synced { nodes, consoles, removed }) = proto::call(c2.endpoint(), peer, &Req::Sync { cert, nodes, consoles, removed }).await {
                    c2.merge(&net2, nodes, consoles, removed);
                }
            }));
        }
        for t in tasks {
            let _ = t.await;
        }
    }
    if publish {
        if let Some((key, now_dir)) = c.directory_now(net) {
            let same = found.as_ref() == Some(&now_dir) || c.published.lock().unwrap().get(net) == Some(&now_dir);
            if !same {
                match directory::publish(&key, &now_dir).await {
                    Ok(()) => {
                        c.published.lock().unwrap().insert(net.to_string(), now_dir);
                    }
                    Err(e) => clog(&format!("directory publish failed: {e:#}")),
                }
            } else if let Some(d) = found {
                c.published.lock().unwrap().insert(net.to_string(), d);
            }
        }
    }
}

/// Open console: refresh the directories at start, then republish when something changes and every 30 min
/// (records on the DHT expire after a few hours).
async fn directory_loop(c: Arc<Console>) {
    let mut last_full: Option<Instant> = None;
    loop {
        let nets: Vec<String> = c.state.lock().unwrap().networks.iter().map(|n| n.id.clone()).collect();
        let full = last_full.is_none_or(|t| t.elapsed() >= REPUBLISH_EVERY);
        for net in nets {
            if full {
                c.published.lock().unwrap().remove(&net);
                refresh_network(&c, &net, true).await;
            } else if let Some((key, d)) = c.directory_now(&net) {
                if c.published.lock().unwrap().get(&net) != Some(&d) {
                    if directory::publish(&key, &d).await.is_ok() {
                        c.published.lock().unwrap().insert(net.clone(), d);
                    }
                }
            }
        }
        if full {
            last_full = Some(Instant::now());
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// Open the session on `conn`: Hello / Welcome, then reader, writer and pinger tasks.
async fn start_session(c: Arc<Console>, conn: Arc<Connection>, net: String, node: String) -> Result<()> {
    let (cert, consoles) = c.hello_for(&net).ok_or_else(|| anyhow!("unknown network"))?;
    let (mut send, mut recv) = conn.open_bi().await.ctx("open session")?;
    proto::write_frame(&mut send, &Req::Hello { cert, consoles }).await?;
    let resp: Resp = tokio::time::timeout(Duration::from_secs(20), proto::read_frame(&mut recv))
        .await
        .map_err(|_| anyhow!("no answer to hello"))??;
    let (node_name, inventory) = match resp {
        Resp::Welcome { node_name, inventory } => (node_name, inventory),
        Resp::Error { message } => bail!("{message}"),
        _ => bail!("unexpected answer to hello"),
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    c.on_online(&node, &net, conn.clone(), tx.clone(), node_name, inventory);

    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if proto::write_frame(&mut send, &f).await.is_err() {
                break;
            }
        }
    });
    let last = Arc::new(AtomicU64::new(now()));
    {
        let (c, conn, last, node) = (c.clone(), conn.clone(), last.clone(), node.clone());
        tokio::spawn(async move {
            while let Ok(f) = proto::read_frame::<_, Frame>(&mut recv).await {
                last.store(now(), Ordering::Relaxed);
                if let Frame::Inventory { inventory } = f {
                    c.on_inventory(&node, inventory);
                }
            }
            conn.close(0u32.into(), b"session ended");
            c.on_offline(&node, &conn);
        });
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PING_EVERY).await;
            if tx.send(Frame::Ping).is_err() {
                break;
            }
            if now().saturating_sub(last.load(Ordering::Relaxed)) > PONG_TIMEOUT_SECS {
                conn.close(0u32.into(), b"no pong");
                break;
            }
        }
    });
    Ok(())
}

/// Dial a node and open a session, unless one exists or is being opened.
async fn dial(c: Arc<Console>, net: String, node: String) -> Result<()> {
    if c.live.lock().unwrap().contains_key(&node) || !c.dialing.lock().unwrap().insert(node.clone()) {
        return Ok(());
    }
    let r = async {
        let peer: PublicKey = node.parse().ctx("node id")?;
        let conn = Arc::new(proto::connect(c.endpoint(), peer).await?);
        start_session(c.clone(), conn, net, node.clone()).await
    }
    .await;
    c.dialing.lock().unwrap().remove(&node);
    r
}

/// A live connection to `node`, dialing it now if needed.
pub(crate) async fn ensure_live(c: &Arc<Console>, net: &str, node: &str) -> Result<Arc<Connection>> {
    if let Some(conn) = c.live_conn(node) {
        return Ok(conn);
    }
    let r = dial(c.clone(), net.to_string(), node.to_string()).await;
    let t = Instant::now();
    while t.elapsed() < proto::DIAL_TIMEOUT + Duration::from_secs(5) {
        if let Some(conn) = c.live_conn(node) {
            return Ok(conn);
        }
        if !c.dialing.lock().unwrap().contains(node) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    r?;
    c.live_conn(node).ok_or_else(|| anyhow!("machine offline"))
}

/// Opens sessions to nodes that have none: right away at start, quickly after a drop, rarely otherwise.
async fn manager(c: Arc<Console>) {
    loop {
        for (net, node, last_seen) in c.known_nodes() {
            if c.live.lock().unwrap().contains_key(&node) || c.dialing.lock().unwrap().contains(&node) {
                continue;
            }
            let due = c.retry.lock().unwrap().get(&node).copied();
            if due.is_some_and(|(next, _)| Instant::now() < next) {
                continue;
            }
            let prev = due.map(|(_, b)| b);
            let c2 = c.clone();
            tokio::spawn(async move {
                if dial(c2.clone(), net, node.clone()).await.is_err() {
                    let recent = now().saturating_sub(last_seen) < RECENT;
                    let backoff = match (recent, prev) {
                        (true, Some(b)) => (b * 2).min(Duration::from_secs(60)),
                        (true, None) => Duration::from_secs(3),
                        (false, _) => SWEEP_EVERY,
                    };
                    c2.retry.lock().unwrap().insert(node, (Instant::now() + backoff, backoff));
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A connection some peer opened to this console.
async fn incoming(c: Arc<Console>, conn: Arc<Connection>) {
    let peer = conn.remote_id();
    let Ok(Ok((mut send, mut recv))) = tokio::time::timeout(Duration::from_secs(30), conn.accept_bi()).await else { return };
    let Ok(req) = proto::read_frame::<_, Req>(&mut recv).await else { return };
    let answer = match req {
        Req::Knock { network, cert, join, inventory } => match c.knock(&peer, &network, cert, join, inventory) {
            Ok(admission) => {
                if proto::write_frame(&mut send, &Resp::Knocked { admission }).await.is_err() {
                    return;
                }
                let _ = send.finish();
                match start_session(c.clone(), conn.clone(), network, peer.to_string()).await {
                    // Keep this task (and the connection) alive while the session lasts.
                    Ok(()) => {
                        conn.closed().await;
                    }
                    Err(e) => clog(&format!("session with {} failed: {e:#}", peer.fmt_short())),
                }
                return;
            }
            Err(e) => Resp::Error { message: format!("{e:#}") },
        },
        Req::Sync { cert, nodes, consoles, removed } => {
            c.sync_answer(&peer, &cert, nodes, consoles, removed).unwrap_or_else(|e| Resp::Error { message: format!("{e:#}") })
        }
        _ => Resp::Error { message: "not allowed".into() },
    };
    if proto::write_frame(&mut send, &answer).await.is_ok() {
        let _ = send.finish();
        // The dialer closes once it has the answer.
        let _ = tokio::time::timeout(Duration::from_secs(10), conn.closed()).await;
    }
}

/// Accept loop of a local HTTP listener. A plain fn with a boxed future: `handle` can restart the API
/// listener, and a direct async recursion would make the future type infinite.
fn spawn_http(c: Arc<Console>, listener: TcpListener) -> tokio::task::JoinHandle<()> {
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> = Box::pin(async move {
        loop {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(http_conn(c.clone(), sock));
            }
        }
    });
    tokio::spawn(fut)
}

/// Start the listeners of the enabled tunnels, stop those removed, disabled or changed.
fn sync_tunnels(c: &Arc<Console>) {
    if !c.open {
        return;
    }
    let wanted: Vec<Tunnel> = c.state.lock().unwrap().tunnels.iter().filter(|t| t.enabled).cloned().collect();
    let mut running = c.tunnels.lock().unwrap();
    running.retain(|name, (t, task)| {
        let keep = wanted.iter().any(|w| w == t) && !task.is_finished();
        if !keep {
            task.abort();
            clog(&format!("tunnel {name} stopped"));
        }
        keep
    });
    for t in wanted {
        if running.contains_key(&t.name) {
            continue;
        }
        let task = tokio::spawn(run_tunnel(c.clone(), t.clone()));
        running.insert(t.name.clone(), (t, task));
    }
}

/// One tunnel: every local connection goes to the machine over its session (dialed again if it dropped)
/// and on to the target from there.
async fn run_tunnel(c: Arc<Console>, t: Tunnel) {
    let listener = match TcpListener::bind(t.listen.as_str()).await {
        Ok(l) => l,
        Err(e) => {
            clog(&format!("tunnel {}: cannot listen on {}: {e}", t.name, t.listen));
            return;
        }
    };
    clog(&format!("tunnel {}: {} → {} via {}", t.name, t.listen, t.target, t.node));
    loop {
        let Ok((sock, _)) = listener.accept().await else { continue };
        let (c, t) = (c.clone(), t.clone());
        tokio::spawn(async move {
            if let Err(e) = forward_one(&c, &t, sock).await {
                clog(&format!("tunnel {}: {e:#}", t.name));
            }
        });
    }
}

async fn forward_one(c: &Arc<Console>, t: &Tunnel, mut sock: tokio::net::TcpStream) -> Result<()> {
    let (net, _, node) = find_node(c, &t.node)?;
    let (mut remote, resp) = open_resumable(c, &net, &node, Req::Forward { target: t.target.clone() }, Duration::from_secs(15 * 60)).await?;
    if !matches!(resp, Resp::Forwarding) {
        bail!("unexpected answer");
    }
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut remote).await;
    Ok(())
}

fn tunnels_json(c: &Console) -> Value {
    let st = c.state.lock().unwrap();
    let running = c.tunnels.lock().unwrap();
    json!(st
        .tunnels
        .iter()
        .map(|t| json!({
            "name": t.name, "node": t.node, "listen": t.listen, "target": t.target, "enabled": t.enabled,
            "active": running.get(&t.name).is_some_and(|(_, task)| !task.is_finished()),
        }))
        .collect::<Vec<_>>())
}

enum Caller {
    /// The local CLI / GUI (internal token).
    Local,
    /// A program with an API token of this scope.
    Token(String),
}

fn caller(c: &Console, token: Option<&str>) -> Option<Caller> {
    let token = token?;
    let st = c.state.lock().unwrap();
    if !st.console.token.is_empty() && eq_ct(&st.console.token, token) {
        return Some(Caller::Local);
    }
    if !st.api.enabled {
        return None;
    }
    st.api.tokens.iter().find(|t| eq_ct(&t.token, token)).map(|t| Caller::Token(t.scope.clone()))
}

/// Routes: `POST /v1/op` (body: an operation as JSON, e.g. `{"op":"nodes"}`), and for convenience
/// `GET /v1/status`, `GET /v1/nodes`, `GET /v1/nodes/<name>`.
async fn http_conn(c: Arc<Console>, sock: tokio::net::TcpStream) {
    let (r, mut w) = sock.into_split();
    let Ok(req) = crate::http::read_request(r).await else {
        let _ = crate::http::write_response(&mut w, 400, &json!({ "ok": false, "error": "bad request" })).await;
        return;
    };
    let Some(who) = caller(&c, req.token.as_deref()) else {
        let _ = crate::http::write_response(&mut w, 401, &json!({ "ok": false, "error": "missing or unknown token" })).await;
        return;
    };
    let op: Result<IpcOp, u16> = match (req.method.as_str(), req.path.trim_end_matches('/')) {
        ("POST", "/v1/op") => serde_json::from_slice(&req.body).map_err(|_| 400),
        ("GET", "/v1/status") => Ok(IpcOp::Status),
        ("GET", "/v1/nodes") => Ok(IpcOp::Nodes { network: None }),
        ("GET", p) if p.starts_with("/v1/nodes/") => Ok(IpcOp::Info { node: p["/v1/nodes/".len()..].to_string() }),
        _ => Err(404),
    };
    let op = match op {
        Ok(op) => op,
        Err(code) => {
            let _ = crate::http::write_response(&mut w, code, &json!({ "ok": false, "error": if code == 404 { "no such route" } else { "bad operation" } })).await;
            return;
        }
    };
    if let Caller::Token(scope) = &who {
        let allowed = match op.scope() {
            Some("read") => scope == "read" || scope == "admin",
            Some("admin") => scope == "admin",
            _ => false,
        };
        if !allowed {
            let _ = crate::http::write_response(&mut w, 403, &json!({ "ok": false, "error": "this token's scope does not allow that" })).await;
            return;
        }
    }
    let body = match handle(&c, op).await {
        Ok(v) => json!({ "ok": true, "data": v }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    };
    let _ = crate::http::write_response(&mut w, 200, &body).await;
}

fn api_json(api: &ApiConfig, port: u16) -> Value {
    json!({
        "enabled": api.enabled,
        "port": port,
        "url": format!("http://127.0.0.1:{port}/v1"),
        "tokens": api.tokens.iter().map(|t| json!({ "name": t.name, "scope": t.scope, "created": t.created })).collect::<Vec<_>>(),
    })
}

pub(crate) fn find_node(c: &Console, target: &str) -> Result<(String, String, String)> {
    let st = c.state.lock().unwrap();
    let (node, net) = match target.rsplit_once('.') {
        Some((a, b)) if st.networks.iter().any(|n| n.name == b) => (a, Some(b)),
        _ => (target, None),
    };
    let mut hits = vec![];
    for n in &st.networks {
        if net.is_some_and(|w| w != n.name) {
            continue;
        }
        for x in n.nodes.iter().filter(|x| !n.removed.contains(&x.id)) {
            if x.name == node || (node.len() >= 8 && x.id.starts_with(node)) {
                hits.push((n.id.clone(), n.name.clone(), x.id.clone()));
            }
        }
    }
    match hits.len() {
        0 => Err(anyhow!("no machine {target} (see `gangli nodes`)")),
        1 => Ok(hits.remove(0)),
        _ => Err(anyhow!("{target} is in more than one network: use name.network")),
    }
}

fn node_views(c: &Console, network: Option<&str>) -> Vec<NodeView> {
    let st = c.state.lock().unwrap();
    let live = c.live.lock().unwrap();
    let mut out = vec![];
    for n in &st.networks {
        if network.is_some_and(|w| w != n.name) {
            continue;
        }
        for x in n.nodes.iter().filter(|x| !n.removed.contains(&x.id)) {
            let l = live.get(&x.id);
            out.push(NodeView {
                network: n.name.clone(),
                id: x.id.clone(),
                name: x.name.clone(),
                online: l.is_some(),
                since: l.map(|l| l.since),
                last_seen: x.last_seen,
                path: l.and_then(|l| proto::path_info(&l.conn)),
                inventory: x.inventory.clone(),
            });
        }
    }
    out.sort_by(|a, b| (b.online, &a.network, &a.name).cmp(&(a.online, &b.network, &b.name)));
    out
}

fn codes_json(name: &str, key: &SecretKey) -> Value {
    json!({
        "network": name,
        "id": key.public().to_string(),
        "public_code": code::public_code(key),
        "private_code": code::private_code(key),
    })
}

fn add_network(c: &Console, key: SecretKey, name: Option<String>) -> Result<String> {
    let id = key.public().to_string();
    let host = slug(&crate::inventory::collect().hostname);
    let me = c.me.to_string();
    let mut st = c.state.lock().unwrap();
    if let Some(n) = st.networks.iter().find(|n| n.id == id) {
        return Ok(n.name.clone());
    }
    let name = unique_name(&name.unwrap_or_else(|| "net".into()), st.networks.iter().map(|n| n.name.as_str()));
    st.networks.push(ConsoleNet {
        id,
        name: name.clone(),
        key: store::secret_to_hex(&key),
        cert: Cert::issue(&key, &me, CertKind::Console, &host, TEN_YEARS),
        nodes: vec![],
        consoles: vec![ConsoleRec { id: me, name: host, seen: now() }],
        removed: vec![],
    });
    Ok(name)
}

/// One operation, from the IPC (open console) or from a one-shot CLI console.
#[derive(PartialEq)]
enum OpKind {
    ApiEnable,
    Other,
}

pub async fn handle(c: &Arc<Console>, op: IpcOp) -> Result<Value> {
    let op_kind = if matches!(op, IpcOp::ApiEnable) { OpKind::ApiEnable } else { OpKind::Other };
    match op {
        IpcOp::Status => {
            let st = c.state.lock().unwrap();
            let live = c.live.lock().unwrap();
            let nets: Vec<Value> = st
                .networks
                .iter()
                .map(|n| {
                    json!({
                        "name": n.name,
                        "id": n.id,
                        "nodes": n.nodes.iter().filter(|x| !n.removed.contains(&x.id)).count(),
                        "online": n.nodes.iter().filter(|x| live.contains_key(&x.id)).count(),
                        "consoles": n.consoles.len(),
                    })
                })
                .collect();
            Ok(json!({ "version": crate::build_version(), "console_id": c.me.to_string(), "open": c.open, "networks": nets }))
        }
        IpcOp::NetworkCreate { name } => {
            let key = SecretKey::generate();
            let name = add_network(c, key.clone(), Some(slug(&name)))?;
            c.save();
            clog(&format!("network {name} created"));
            refresh_network(c, &key.public().to_string(), true).await;
            Ok(codes_json(&name, &key))
        }
        IpcOp::NetworkAdd { code: s, name } => {
            let Code::Private { key } = code::decode(&s)? else {
                bail!("this is a public code: it is for the machines to manage (`gangli node --code`), a console needs the private one");
            };
            let net = key.public().to_string();
            // The directory may carry the network's name.
            let dir_name = directory::resolve(&key.public(), Some(&key)).await.ok().map(|d| d.name).filter(|n| !n.is_empty());
            let name = add_network(c, key, name.or(dir_name))?;
            c.save();
            clog(&format!("network {name} added"));
            refresh_network(c, &net, true).await;
            let count = c.state.lock().unwrap().networks.iter().find(|n| n.id == net).map(|n| n.nodes.len()).unwrap_or(0);
            Ok(json!({ "network": name, "id": net, "machines": count }))
        }
        IpcOp::NetworkCodes { name } => {
            let st = c.state.lock().unwrap();
            let n = st.networks.iter().find(|n| n.name == name).ok_or_else(|| anyhow!("no network {name} here"))?;
            Ok(codes_json(&n.name, &store::secret_from_hex(&n.key)?))
        }
        IpcOp::NetworkRemove { name } => {
            let ids: Vec<String> = {
                let mut st = c.state.lock().unwrap();
                let Some(i) = st.networks.iter().position(|n| n.name == name) else { bail!("no network {name} here") };
                let n = st.networks.remove(i);
                n.nodes.into_iter().map(|x| x.id).collect()
            };
            for id in ids {
                if let Some(conn) = c.live_conn(&id) {
                    conn.close(0u32.into(), b"network removed from this console");
                }
            }
            c.save();
            Ok(json!({ "removed": name }))
        }
        IpcOp::Nodes { network } => {
            if !c.open {
                one_shot_round(c).await;
            } else {
                // Right after the console opened, give the first round of sessions a moment.
                while c.started.elapsed() < Duration::from_secs(5) && !c.dialing.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
            Ok(serde_json::to_value(node_views(c, network.as_deref()))?)
        }
        IpcOp::Info { node } => {
            let (net_id, net_name, node_id) = find_node(c, &node)?;
            let conn = ensure_live(c, &net_id, &node_id).await?;
            let Resp::Info { node_name, inventory } = proto::request(&conn, &Req::Info).await? else {
                bail!("unexpected answer");
            };
            Ok(json!({ "network": net_name, "node": node_name, "id": node_id, "path": proto::path_info(&conn), "inventory": inventory }))
        }
        IpcOp::Forget { node } => {
            let (net_id, _, node_id) = find_node(c, &node)?;
            {
                let mut st = c.state.lock().unwrap();
                if let Some(n) = st.networks.iter_mut().find(|n| n.id == net_id) {
                    n.nodes.retain(|x| x.id != node_id);
                    if !n.removed.contains(&node_id) {
                        n.removed.push(node_id.clone());
                    }
                }
            }
            c.save();
            if let Some(conn) = c.live_conn(&node_id) {
                conn.close(0u32.into(), b"removed");
            }
            if !c.open {
                refresh_network(c, &net_id, true).await;
            }
            Ok(json!({ "forgot": node }))
        }
        IpcOp::ApiStatus => {
            let st = c.state.lock().unwrap();
            Ok(api_json(&st.api, st.console.port))
        }
        IpcOp::ApiEnable | IpcOp::ApiDisable => {
            let on = matches!(op_kind, OpKind::ApiEnable);
            c.state.lock().unwrap().api.enabled = on;
            c.save();
            let st = c.state.lock().unwrap();
            Ok(api_json(&st.api, st.console.port))
        }
        IpcOp::ApiTokenAdd { name, scope } => {
            if scope != "read" && scope != "admin" {
                bail!("scope must be read or admin");
            }
            let token = format!("gat_{}", HEXLOWER.encode(&random16()));
            let (enabled, port) = {
                let mut st = c.state.lock().unwrap();
                st.api.tokens.retain(|t| t.name != name);
                st.api.tokens.push(ApiToken { name: name.clone(), scope: scope.clone(), token: token.clone(), created: now() });
                (st.api.enabled, st.console.port)
            };
            c.save();
            Ok(json!({ "name": name, "scope": scope, "token": token, "url": format!("http://127.0.0.1:{port}/v1"), "enabled": enabled }))
        }
        IpcOp::ApiTokenRemove { name } => {
            let removed = {
                let mut st = c.state.lock().unwrap();
                let before = st.api.tokens.len();
                st.api.tokens.retain(|t| t.name != name);
                before - st.api.tokens.len()
            };
            if removed == 0 {
                bail!("no token named {name}");
            }
            c.save();
            Ok(json!({ "removed": name }))
        }
        IpcOp::TunnelList => Ok(tunnels_json(c)),
        IpcOp::TunnelAdd { name, node, target, listen } => {
            find_node(c, &node)?;
            let listen = match listen {
                Some(l) if l.parse::<u16>().is_ok() => format!("127.0.0.1:{l}"),
                Some(l) => l,
                None => {
                    // A free local port, kept from now on.
                    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
                    format!("127.0.0.1:{}", l.local_addr()?.port())
                }
            };
            let target = if target.parse::<u16>().is_ok() { format!("localhost:{target}") } else { target };
            {
                let mut st = c.state.lock().unwrap();
                st.tunnels.retain(|t| t.name != name);
                st.tunnels.push(Tunnel { name: name.clone(), node, listen: listen.clone(), target, enabled: true });
            }
            c.save();
            sync_tunnels(c);
            Ok(json!({ "tunnel": name, "listen": listen, "open": c.open }))
        }
        IpcOp::TunnelRemove { name } => {
            let removed = {
                let mut st = c.state.lock().unwrap();
                let before = st.tunnels.len();
                st.tunnels.retain(|t| t.name != name);
                before - st.tunnels.len()
            };
            if removed == 0 {
                bail!("no tunnel named {name}");
            }
            c.save();
            sync_tunnels(c);
            Ok(json!({ "removed": name }))
        }
        IpcOp::TunnelSet { name, enabled } => {
            {
                let mut st = c.state.lock().unwrap();
                let t = st.tunnels.iter_mut().find(|t| t.name == name).ok_or_else(|| anyhow!("no tunnel named {name}"))?;
                t.enabled = enabled;
            }
            c.save();
            sync_tunnels(c);
            Ok(tunnels_json(c))
        }
        IpcOp::SshStatus => {
            let st = c.state.lock().unwrap();
            Ok(json!({
                "enabled": st.ssh.enabled,
                "listen": st.ssh.listen,
                "open": c.open,
                "logins": st.ssh.logins.iter().enumerate().map(|(i, r)| json!({
                    "index": i + 1, "hosts": r.hosts, "users": r.users, "password": r.password, "keys": r.keys.len(),
                })).collect::<Vec<_>>(),
                "launchers": st.launchers.iter().map(|l| json!({ "name": l.name, "command": l.command })).collect::<Vec<_>>(),
            }))
        }
        IpcOp::SshPassword { node, user } => {
            let (login, password, port, _) = login_for(c, &node, user.as_deref())?;
            Ok(json!({ "login": login, "password": password, "host": "127.0.0.1", "port": port }))
        }
        IpcOp::SshLoginAdd { hosts, users, password, key } => {
            let password_given = password.is_some();
            if let Some(k) = &key {
                russh::keys::PublicKey::from_openssh(k.trim()).map_err(|e| anyhow!("not an OpenSSH public key: {e}"))?;
            }
            let rule = crate::ssh::LoginRule {
                hosts: if hosts.is_empty() { vec!["*".into()] } else { hosts },
                users: if users.is_empty() { vec!["*".into()] } else { users },
                password: password.unwrap_or_else(crate::ssh::new_password),
                keys: key.into_iter().map(|k| k.trim().to_string()).collect(),
            };
            let same = |a: &[String], b: &[String]| {
                let (mut a, mut b) = (a.to_vec(), b.to_vec());
                a.sort();
                b.sort();
                a == b
            };
            let (index, shown) = {
                let mut st = c.state.lock().unwrap();
                // Same machines and users as an existing rule: add the key / set the password there.
                if let Some(i) = st.ssh.logins.iter().position(|r| same(&r.hosts, &rule.hosts) && same(&r.users, &rule.users)) {
                    let existing = &mut st.ssh.logins[i];
                    for k in &rule.keys {
                        if !existing.keys.contains(k) {
                            existing.keys.push(k.clone());
                        }
                    }
                    if password_given {
                        existing.password = rule.password.clone();
                    }
                    (i + 1, existing.clone())
                } else {
                    st.ssh.logins.push(rule.clone());
                    (st.ssh.logins.len(), rule)
                }
            };
            c.save();
            Ok(json!({ "index": index, "hosts": shown.hosts, "users": shown.users, "password": shown.password }))
        }
        IpcOp::SshLoginRemove { index } => {
            {
                let mut st = c.state.lock().unwrap();
                if index == 0 || index > st.ssh.logins.len() {
                    bail!("no login rule {index} (see `gangli ssh status`)");
                }
                st.ssh.logins.remove(index - 1);
            }
            c.save();
            Ok(json!({ "removed": index }))
        }
        IpcOp::Launch { node, launcher, user } => {
            let (login, password, port, node_name) = login_for(c, &node, user.as_deref())?;
            let (name, command) = {
                let st = c.state.lock().unwrap();
                let l = match &launcher {
                    Some(n) => st.launchers.iter().find(|l| l.name.eq_ignore_ascii_case(n)),
                    None => st.launchers.first(),
                }
                .ok_or_else(|| anyhow!("no launcher {} in gangli.console", launcher.clone().unwrap_or_default()))?;
                (l.name.clone(), l.command.clone())
            };
            let (u, _) = crate::ssh::parse_login(&login);
            let argv = crate::ssh::fill(
                &command,
                &[
                    ("login", login.clone()),
                    ("login_url", crate::ssh::url_encode(&login)),
                    ("password", password.clone()),
                    ("password_url", crate::ssh::url_encode(&password)),
                    ("port", port.to_string()),
                    ("node", node_name.clone()),
                    ("user", u),
                    ("host", "127.0.0.1".into()),
                ],
            );
            Ok(json!({ "launcher": name, "argv": argv, "gateway_open": c.open }))
        }
        IpcOp::DebugDrop { node } => {
            let (_, _, id) = find_node(c, &node)?;
            let conn = c.live_conn(&id).ok_or_else(|| anyhow!("no live session with {node}"))?;
            conn.close(0u32.into(), b"debug drop");
            Ok(json!({ "dropped": node }))
        }
        IpcOp::Stop => {
            c.stop.notify_one();
            Ok(json!({ "stopping": true }))
        }
    }
}

/// One-shot console: read the directories and dial every known machine in parallel, for a few seconds.
async fn one_shot_round(c: &Arc<Console>) {
    let nets: Vec<String> = c.state.lock().unwrap().networks.iter().map(|n| n.id.clone()).collect();
    let mut dirs = vec![];
    for net in nets {
        let c2 = c.clone();
        dirs.push(tokio::spawn(async move { refresh_network(&c2, &net, false).await }));
    }
    let dial_all = |c: &Arc<Console>| {
        c.known_nodes()
            .into_iter()
            .map(|(net, node, _)| {
                let c2 = c.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(ONE_SHOT_WAIT, dial(c2, net, node)).await;
                })
            })
            .collect::<Vec<_>>()
    };
    let first = dial_all(c);
    let _ = tokio::time::timeout(ONE_SHOT_WAIT, async {
        for d in dirs {
            let _ = d.await;
        }
    })
    .await;
    // Machines learned from the directory or another console.
    let second = dial_all(c);
    for t in first.into_iter().chain(second) {
        let _ = t.await;
    }
}

/// `gangli console run`: an open console in the foreground, until `console stop` or the process ends.
pub fn run_foreground() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(async {
        let c = start(true).await?;
        eprintln!("console open (id {}); stop with `gangli console stop`", c.me);
        c.stop.notified().await;
        c.shutdown().await;
        Ok(())
    });
    r
}

/// A CLI command with no open console: run it in a one-shot console in this process.
pub fn run_once(op: IpcOp) -> Result<Value> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(async {
        let c = start(false).await?;
        let r = handle(&c, op).await;
        c.shutdown().await;
        r
    });
    r
}

/// The login string, password and gateway port for `user` on a machine (best `[[ssh.login]]` rule).
fn login_for(c: &Console, node: &str, user: Option<&str>) -> Result<(String, String, u16, String)> {
    let (net, net_name, id) = find_node(c, node)?;
    let name = c.node_name(&net, &id).unwrap_or_default();
    let st = c.state.lock().unwrap();
    let ambiguous = st.networks.iter().filter(|n| n.nodes.iter().any(|x| x.name == name)).count() > 1;
    let target = if ambiguous { format!("{name}.{net_name}") } else { name.clone() };
    let login = match user {
        Some(u) => format!("{u}+{target}"),
        None => target,
    };
    let names = vec![name.clone(), format!("{name}.{net_name}"), id];
    let rule = crate::ssh::rule_for(&st.ssh.logins, &names, user.unwrap_or("default"))
        .ok_or_else(|| anyhow!("no [[ssh.login]] rule allows {login}: `gangli ssh login add`"))?;
    let port = st.ssh.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(2222);
    Ok((login, rule.password.clone(), port, name))
}

/// Open `inner` (a `Shell` or a `Forward`) on a machine as a resumable stream: the returned pipe survives
/// connection drops for `grace` (the machine is dialed again and the stream reattached). Also returns the
/// machine's first answer (`Started` / `Forwarding`).
pub(crate) async fn open_resumable(c: &Arc<Console>, net: &str, node: &str, inner: Req, grace: Duration) -> Result<(tokio::io::DuplexStream, Resp)> {
    use crate::resume::{self, Reconnect, Transport};
    let id = HEXLOWER.encode(&random16());
    let conn = ensure_live(c, net, node).await?;
    let (mut send, mut recv) = conn.open_bi().await.ctx("open stream")?;
    proto::write_frame(&mut send, &Req::Stream { id: id.clone(), inner: Box::new(inner) }).await?;
    let resp: Resp = tokio::time::timeout(Duration::from_secs(30), proto::read_frame(&mut recv))
        .await
        .map_err(|_| anyhow!("the machine did not answer"))??;
    if let Resp::Error { message } = resp {
        bail!("{message}");
    }
    let (c2, net2, node2) = (c.clone(), net.to_string(), node.to_string());
    let rc: Reconnect = Arc::new(move |received| {
        let (c, net, node, id) = (c2.clone(), net2.clone(), node2.clone(), id.clone());
        Box::pin(async move {
            let conn = ensure_live(&c, &net, &node).await?;
            let (mut send, mut recv) = conn.open_bi().await.ctx("open stream")?;
            proto::write_frame(&mut send, &Req::Resume { id, received }).await?;
            match tokio::time::timeout(Duration::from_secs(20), proto::read_frame::<_, Resp>(&mut recv)).await {
                Ok(Ok(Resp::Resumed { received })) => {
                    clog("stream resumed after a drop");
                    Ok(Some(Transport { send: Box::new(send), recv: Box::new(recv), peer_received: received }))
                }
                Ok(Ok(Resp::Error { .. })) => Ok(None),
                Ok(Ok(_)) => Ok(None),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow!("no answer to resume")),
            }
        })
    });
    let (app, _attach, _task) = resume::start(Transport { send: Box::new(send), recv: Box::new(recv), peer_received: 0 }, grace, Some(rc));
    Ok((app, resp))
}
