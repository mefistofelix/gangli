//! The node body (section 5 of docs/ARCHITECTURE.md): one iroh endpoint with the node key. It runs as the
//! system service (`gangli install`) or by hand as the user (`gangli node`), same code.
//!
//! Nothing here polls. A node:
//! - serves the sessions consoles open to it (they dial it by id, or answer its knock);
//! - knocks on its consoles once when it starts, when its LAN addresses change and shortly after a session
//!   drops, so a running console sees it come online at once; when none of the known consoles answers it
//!   looks the network directory up once for new ones;
//! - while it is not admitted yet, looks the directory up and knocks with a capped backoff;
//! - checks its own inventory every minute, locally, and pushes it only when its stable part changed.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::PublicKey;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::cert::{Cert, CertKind};
use crate::inventory::{self, Inventory};
use crate::log_line;
use crate::proto::{self, Frame, Req, Resp};
use crate::state::{self, Membership};
use crate::util::{now, unique_name, Ctx};

const LOCAL_CHECK: Duration = Duration::from_secs(60);
/// A console pings every 25 s; without any frame for this long the session is considered dead.
const SESSION_IDLE: Duration = Duration::from_secs(90);
const PENDING_BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn run_blocking(stop: Arc<AtomicBool>) {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            log_line(&format!("cannot start the runtime: {e}"));
            return;
        }
    };
    rt.block_on(async {
        // SIGTERM / SIGINT (service manager, Ctrl+C) stop the node cleanly.
        #[cfg(unix)]
        {
            let stop = stop.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{signal, SignalKind};
                if let (Ok(mut t), Ok(mut i)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
                    tokio::select! {
                        _ = t.recv() => {}
                        _ = i.recv() => {}
                    }
                    stop.store(true, Ordering::SeqCst);
                }
            });
        }
        // Keep the node alive across transient failures (no network at boot, etc.).
        while !stop.load(Ordering::SeqCst) {
            if let Err(e) = run(stop.clone()).await {
                log_line(&format!("node error: {e:#}; retrying in 10s"));
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    });
}

#[derive(Debug)]
struct Session {
    network: String,
    tx: mpsc::UnboundedSender<Frame>,
}

/// Open console sessions, shared by the accept handler and the main loop.
#[derive(Debug, Default)]
struct Sessions {
    next: AtomicU64,
    map: Mutex<HashMap<u64, Session>>,
    /// Networks whose session ended since the main loop last looked.
    ended: Mutex<Vec<String>>,
}

impl Sessions {
    fn add(&self, s: Session) -> u64 {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.map.lock().unwrap().insert(id, s);
        id
    }
    fn remove(&self, id: u64) {
        if let Some(s) = self.map.lock().unwrap().remove(&id) {
            self.ended.lock().unwrap().push(s.network);
        }
    }
    fn has_network(&self, net: &str) -> bool {
        self.map.lock().unwrap().values().any(|s| s.network == net)
    }
    fn broadcast(&self, f: &Frame) {
        for s in self.map.lock().unwrap().values() {
            let _ = s.tx.send(f.clone());
        }
    }
    fn take_ended(&self) -> Vec<String> {
        std::mem::take(&mut *self.ended.lock().unwrap())
    }
}

#[derive(Debug, Clone)]
struct NodeHandler {
    sessions: Arc<Sessions>,
}

impl ProtocolHandler for NodeHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        serve(conn, self.sessions.clone()).await;
        Ok(())
    }
}

async fn collect() -> Inventory {
    tokio::task::spawn_blocking(inventory::collect).await.unwrap_or_default()
}

async fn run(stop: Arc<AtomicBool>) -> Result<()> {
    let sk = state::node_key()?;
    let me = sk.public();
    let ep = iroh::Endpoint::builder(presets::N0).secret_key(sk).bind().await.ctx("start endpoint")?;
    let sessions = Arc::new(Sessions::default());
    let router = Router::builder(ep).accept(proto::ALPN, NodeHandler { sessions: sessions.clone() }).spawn();
    log_line(&format!("{} node running, id {me}", crate::version_line()));

    let inflight: Arc<Mutex<HashSet<String>>> = Arc::default();
    // Networks to knock on, when, and whether to look the directory up if no known console answers.
    let mut want_knock: HashMap<String, (Instant, bool)> = HashMap::new();
    let mut last_check: Option<Instant> = None;
    let mut last_fp = String::new();
    let mut last_ips: Vec<String> = vec![];
    let mut inv = Inventory::default();
    let mut first = true;

    while !stop.load(Ordering::SeqCst) {
        let st = match state::load() {
            Ok(s) => s,
            Err(e) => {
                log_line(&format!("cannot read state: {e:#}"));
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Local inventory check: no network traffic unless something changed.
        if last_check.is_none_or(|t| t.elapsed() >= LOCAL_CHECK) {
            last_check = Some(Instant::now());
            inv = collect().await;
            let fp = inv.fingerprint();
            if !first && fp != last_fp {
                sessions.broadcast(&Frame::Inventory { inventory: inv.clone() });
            }
            let ips_changed = !first && inv.ips != last_ips;
            last_fp = fp;
            last_ips = inv.ips.clone();
            if first || ips_changed {
                for m in st.networks.iter().filter(|m| m.cert.is_some()) {
                    want_knock.insert(m.id.clone(), (Instant::now(), true));
                }
            }
            first = false;
        }
        for net in sessions.take_ended() {
            want_knock.entry(net).or_insert_with(|| (Instant::now() + Duration::from_secs(5), false));
        }

        for m in &st.networks {
            if m.cert.is_none() {
                if m.join.is_some() {
                    spawn_pending(&router, &sessions, &inflight, m, inv.clone(), stop.clone());
                }
                continue;
            }
            let Some((at, use_dir)) = want_knock.get(&m.id).copied() else { continue };
            if Instant::now() < at {
                continue;
            }
            want_knock.remove(&m.id);
            if !sessions.has_network(&m.id) {
                spawn_knock_round(&router, &sessions, &inflight, m, inv.clone(), use_dir);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    log_line("stopping");
    router.shutdown().await.ctx("shutdown")?;
    Ok(())
}

/// Consoles listed in the network's public directory.
async fn directory_consoles(network: &str) -> Vec<String> {
    match network.parse::<PublicKey>() {
        Ok(id) => crate::directory::resolve(&id, None).await.map(|d| d.consoles).unwrap_or_default(),
        Err(_) => vec![],
    }
}

/// Knock on the known consoles of an admitted network; if none answers and `use_dir`, on the consoles of
/// the directory. Failures are normal (consoles closed): nothing is retried until the next event.
fn spawn_knock_round(router: &Router, sessions: &Arc<Sessions>, inflight: &Arc<Mutex<HashSet<String>>>, m: &Membership, inv: Inventory, use_dir: bool) {
    let key = format!("knock:{}", m.id);
    if !inflight.lock().unwrap().insert(key.clone()) {
        return;
    }
    let (router, sessions, inflight, m) = (router.clone(), sessions.clone(), inflight.clone(), m.clone());
    tokio::spawn(async move {
        let mut tried: HashSet<String> = HashSet::new();
        let mut targets = m.consoles.clone();
        let mut looked_up = false;
        loop {
            let mut tasks = vec![];
            for c in targets.iter().filter(|c| tried.insert((*c).clone())) {
                let Ok(peer) = c.parse::<PublicKey>() else { continue };
                let (router, sessions, m, inv) = (router.clone(), sessions.clone(), m.clone(), inv.clone());
                // Each successful knock serves its session until it ends; the round only waits for the
                // knock itself to be answered.
                let (ok_tx, ok_rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let _ = knock(&router, sessions, &m, peer, inv, Some(ok_tx)).await;
                });
                tasks.push(ok_rx);
            }
            let mut any = false;
            for t in tasks {
                if matches!(t.await, Ok(true)) {
                    any = true;
                }
            }
            if any || !use_dir || looked_up {
                break;
            }
            looked_up = true;
            targets = directory_consoles(&m.id).await;
        }
        inflight.lock().unwrap().remove(&key);
    });
}

/// Keep knocking on the directory's consoles until one admits this node (capped backoff), then serve it.
fn spawn_pending(router: &Router, sessions: &Arc<Sessions>, inflight: &Arc<Mutex<HashSet<String>>>, m: &Membership, inv: Inventory, stop: Arc<AtomicBool>) {
    let key = format!("pending:{}", m.id);
    if !inflight.lock().unwrap().insert(key.clone()) {
        return;
    }
    let (router, sessions, inflight, id) = (router.clone(), sessions.clone(), inflight.clone(), m.id.clone());
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(5);
        let mut last_err = String::new();
        'outer: while !stop.load(Ordering::SeqCst) {
            // Re-read: the membership may have been removed (`gangli leave`) or admitted meanwhile.
            let Some(m) = state::load().ok().and_then(|s| s.networks.into_iter().find(|n| n.id == id)) else { break };
            if m.cert.is_some() || m.join.is_none() {
                break;
            }
            let mut consoles = directory_consoles(&id).await;
            consoles.extend(m.consoles.iter().cloned());
            consoles.dedup();
            let mut err = if consoles.is_empty() { "no console has published this network yet".to_string() } else { String::new() };
            for c in &consoles {
                let Ok(peer) = c.parse::<PublicKey>() else { continue };
                let (ok_tx, ok_rx) = tokio::sync::oneshot::channel();
                let (router2, sessions2, m2, inv2) = (router.clone(), sessions.clone(), m.clone(), inv.clone());
                let h = tokio::spawn(async move { knock(&router2, sessions2, &m2, peer, inv2, Some(ok_tx)).await });
                match ok_rx.await {
                    Ok(true) => break 'outer, // admitted; the task keeps serving the session
                    _ => {
                        if let Ok(Err(e)) = h.await {
                            err = format!("{e:#}");
                        }
                    }
                }
            }
            if err != last_err {
                log_line(&format!("waiting to be admitted: {err}"));
                set_error(&id, Some(err.clone()));
                last_err = err;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(PENDING_BACKOFF_MAX);
        }
        inflight.lock().unwrap().remove(&key);
    });
}

fn set_error(id: &str, err: Option<String>) {
    let _ = state::update(|st| {
        if let Some(m) = st.networks.iter_mut().find(|n| n.id == id) {
            m.last_error = err;
        }
        Ok(())
    });
}

/// Knock, record the admission if this node was pending, signal `ok` (answered), then serve the session
/// the console opens on the same connection.
async fn knock(router: &Router, sessions: Arc<Sessions>, m: &Membership, console: PublicKey, inventory: Inventory, ok: Option<tokio::sync::oneshot::Sender<bool>>) -> Result<()> {
    let r = async {
        let ep = router.endpoint();
        let conn = proto::connect(ep, console).await?;
        let req = Req::Knock {
            network: m.id.clone(),
            cert: m.cert.clone(),
            join: if m.cert.is_none() { m.join.clone() } else { None },
            inventory,
        };
        let Resp::Knocked { admission } = proto::request(&conn, &req).await? else {
            bail!("unexpected answer");
        };
        if m.cert.is_none() {
            let a = admission.ok_or_else(|| anyhow!("the console did not admit this node"))?;
            let net_pub: PublicKey = m.id.parse().ctx("network id")?;
            a.cert.verify(&net_pub, &ep.id(), CertKind::Node)?;
            admit(&m.id, a, &console)?;
        }
        Ok(conn)
    }
    .await;
    match r {
        Ok(conn) => {
            if let Some(ok) = ok {
                let _ = ok.send(true);
            }
            serve(conn, sessions).await;
            Ok(())
        }
        Err(e) => {
            if let Some(ok) = ok {
                let _ = ok.send(false);
            }
            Err(e)
        }
    }
}

fn admit(id: &str, a: proto::Admission, console: &PublicKey) -> Result<()> {
    let id = id.to_string();
    let node_name = a.node_name.clone();
    let local = state::update(|st| {
        let taken: Vec<String> = st.networks.iter().filter(|n| n.id != id).map(|n| n.name.clone()).collect();
        let local = unique_name(&a.network_name, taken.iter().map(String::as_str));
        let m = st.networks.iter_mut().find(|n| n.id == id).ok_or_else(|| anyhow!("the membership was removed"))?;
        let mut consoles = a.consoles.clone();
        if !consoles.contains(&console.to_string()) {
            consoles.push(console.to_string());
        }
        let code = m.code.clone();
        *m = Membership {
            name: local.clone(),
            code,
            id: id.clone(),
            join: None,
            cert: Some(a.cert.clone()),
            node_name: Some(a.node_name.clone()),
            consoles,
            last_error: None,
            joined: now(),
        };
        Ok(local)
    })?;
    log_line(&format!("admitted into network {local} as {node_name}"));
    Ok(())
}

/// The console's certificate must be signed by the key of a network this node belongs to and name the
/// connecting endpoint.
fn check_console(peer: &PublicKey, cert: &Cert) -> Result<(String, String)> {
    let st = state::load()?;
    let m = st
        .networks
        .iter()
        .find(|m| m.cert.is_some() && m.id == cert.body.network)
        .ok_or_else(|| anyhow!("not a member of that network"))?;
    let net_pub: PublicKey = cert.body.network.parse().ctx("network id")?;
    cert.verify(&net_pub, peer, CertKind::Console)?;
    Ok((cert.body.network.clone(), m.node_name.clone().unwrap_or_default()))
}

/// Remember the consoles to knock on (the list comes from an authenticated console).
fn remember_consoles(net: &str, peer: &PublicKey, mut ids: Vec<String>) {
    if !ids.contains(&peer.to_string()) {
        ids.insert(0, peer.to_string());
    }
    ids.dedup();
    let net = net.to_string();
    let _ = state::update(|st| {
        if let Some(m) = st.networks.iter_mut().find(|m| m.id == net) {
            if m.consoles != ids {
                m.consoles = ids;
            }
        }
        Ok(())
    });
}

/// Serve the streams a console opens on `conn`: first `Hello`, then requests.
async fn serve(conn: Connection, sessions: Arc<Sessions>) {
    let peer = conn.remote_id();
    let mut authed = false;
    let mut my_name = String::new();
    loop {
        let accepted = if authed {
            conn.accept_bi().await
        } else {
            match tokio::time::timeout(Duration::from_secs(30), conn.accept_bi()).await {
                Ok(r) => r,
                Err(_) => break,
            }
        };
        let Ok((mut send, mut recv)) = accepted else { break };
        let req: Req = match proto::read_frame(&mut recv).await {
            Ok(r) => r,
            Err(_) if authed => continue,
            Err(_) => break,
        };
        match req {
            Req::Hello { cert, consoles } => match check_console(&peer, &cert) {
                Ok((net, node_name)) => {
                    remember_consoles(&net, &peer, consoles);
                    let inventory = collect().await;
                    if proto::write_frame(&mut send, &Resp::Welcome { node_name: node_name.clone(), inventory }).await.is_err() {
                        break;
                    }
                    authed = true;
                    my_name = node_name;
                    let (tx, rx) = mpsc::unbounded_channel();
                    let id = sessions.add(Session { network: net.clone(), tx: tx.clone() });
                    tokio::spawn(session(send, recv, tx, rx, sessions.clone(), id, net, peer));
                }
                Err(e) => {
                    let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{e:#}") }).await;
                    let _ = send.finish();
                    break;
                }
            },
            Req::Forward { target } if authed => {
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(15), tokio::net::TcpStream::connect(target.as_str())).await {
                        Ok(Ok(mut tcp)) => {
                            if proto::write_frame(&mut send, &Resp::Forwarding).await.is_ok() {
                                let mut remote = tokio::io::join(recv, send);
                                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut remote).await;
                            }
                        }
                        Ok(Err(e)) => {
                            let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{target}: {e}") }).await;
                            let _ = send.finish();
                        }
                        Err(_) => {
                            let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{target}: no answer") }).await;
                            let _ = send.finish();
                        }
                    }
                });
            }
            Req::Stream { id, inner } if authed => {
                tokio::spawn(open_stream(id, *inner, send, recv, peer.to_string()));
            }
            Req::Resume { id, received } if authed => {
                let entry = streams().lock().unwrap().get(&id).cloned();
                match entry {
                    Some((console, attach)) if console == peer.to_string() => {
                        let mine = attach.received.load(Ordering::SeqCst);
                        if proto::write_frame(&mut send, &Resp::Resumed { received: mine }).await.is_ok() {
                            let _ = attach.tx.send(crate::resume::Transport { send: Box::new(send), recv: Box::new(recv), peer_received: received });
                            log_line(&format!("stream {} resumed", &id[..8.min(id.len())]));
                        }
                    }
                    _ => {
                        let _ = proto::write_frame(&mut send, &Resp::Error { message: "unknown stream".into() }).await;
                        let _ = send.finish();
                    }
                }
            }
            Req::Shell { user, command, pty, term } if authed => {
                let term = if term.is_empty() { "xterm-256color".to_string() } else { term };
                tokio::spawn(run_shell(send, recv, crate::proc::Spec { user, command, pty, term }));
            }
            Req::Info if authed => {
                let inventory = collect().await;
                let _ = proto::write_frame(&mut send, &Resp::Info { node_name: my_name.clone(), inventory }).await;
                let _ = send.finish();
            }
            _ => {
                let _ = proto::write_frame(&mut send, &Resp::Error { message: "not allowed".into() }).await;
                let _ = send.finish();
                if !authed {
                    break;
                }
            }
        }
    }
    if !authed {
        // Give the error answer a moment to leave before the connection goes away.
        tokio::time::sleep(Duration::from_millis(300)).await;
        conn.close(0u32.into(), b"not allowed");
    }
}

/// One console session: answer pings, take console lists, carry pushed frames.
#[allow(clippy::too_many_arguments)]
async fn session<W, R>(
    mut send: W,
    mut recv: R,
    tx: mpsc::UnboundedSender<Frame>,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    sessions: Arc<Sessions>,
    id: u64,
    net: String,
    peer: PublicKey,
) where
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    let writer = tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            if proto::write_frame(&mut send, &f).await.is_err() {
                break;
            }
        }
    });
    loop {
        match tokio::time::timeout(SESSION_IDLE, proto::read_frame::<_, Frame>(&mut recv)).await {
            Ok(Ok(Frame::Ping)) => {
                let _ = tx.send(Frame::Pong);
            }
            Ok(Ok(Frame::Consoles { ids })) => remember_consoles(&net, &peer, ids),
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    sessions.remove(id);
    writer.abort();
}

/// A console session: the process runs as the requested user; its stdin / stdout / stderr / exit travel as
/// binary frames. The session going away kills the process.
async fn run_shell<W, R>(mut send: W, mut recv: R, spec: crate::proc::Spec)
where
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    use crate::proc::{In, Out};
    use crate::proto::sframe;
    let who = spec.user.clone();
    let child = match tokio::task::spawn_blocking(move || crate::proc::spawn(&spec)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{e:#}") }).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
            return;
        }
        Err(_) => return,
    };
    if proto::write_frame(&mut send, &Resp::Started).await.is_err() {
        return;
    }
    log_line(&format!("session opened as {who}"));
    let crate::proc::Child { input, mut output } = child;
    let reader = tokio::spawn(async move {
        loop {
            let msg = match sframe::read(&mut recv).await {
                Ok((sframe::DATA, d)) => In::Data(d),
                Ok((sframe::RESIZE, b)) if b.len() == 4 => In::Resize(u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]])),
                Ok((sframe::EOF, _)) => In::Eof,
                Ok(_) => continue,
                Err(_) => break,
            };
            if input.send(msg).is_err() {
                break;
            }
        }
        // `input` dropped here: the process is killed if still running.
    });
    while let Some(o) = output.recv().await {
        let r = match o {
            Out::Stdout(d) => sframe::write(&mut send, sframe::DATA, &d).await,
            Out::Stderr(d) => sframe::write(&mut send, sframe::STDERR, &d).await,
            Out::Exit(code) => {
                let _ = sframe::write(&mut send, sframe::EXIT, &code.to_be_bytes()).await;
                break;
            }
        };
        if r.is_err() {
            break;
        }
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
    reader.abort();
}


/// Grace periods: a session (its process) waits a day for its console to come back, a forward 15 minutes.
const SHELL_GRACE: Duration = Duration::from_secs(24 * 3600);
const FORWARD_GRACE: Duration = Duration::from_secs(15 * 60);

type StreamMap = std::sync::Mutex<HashMap<String, (String, crate::resume::Attach)>>;

/// Resumable streams of this node: id → (console that opened it, attach handle).
fn streams() -> &'static StreamMap {
    static S: std::sync::OnceLock<StreamMap> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Answer the inner request on the raw stream, then run it over a resumable pump.
async fn open_stream<W, R>(id: String, inner: Req, mut send: W, recv: R, console: String)
where
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    use crate::resume::{self, Transport};
    match inner {
        Req::Shell { user, command, pty, term } => {
            let term = if term.is_empty() { "xterm-256color".to_string() } else { term };
            let who = user.clone();
            let spec = crate::proc::Spec { user, command, pty, term };
            let child = match tokio::task::spawn_blocking(move || crate::proc::spawn(&spec)).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{e:#}") }).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
                    return;
                }
                Err(_) => return,
            };
            if proto::write_frame(&mut send, &Resp::Started).await.is_err() {
                return;
            }
            log_line(&format!("session opened as {who} (resumable)"));
            let (app, attach, task) = resume::start(Transport { send: Box::new(send), recv: Box::new(recv), peer_received: 0 }, SHELL_GRACE, None);
            register(id, console, attach, task);
            let (r, w) = tokio::io::split(app);
            pipe_child(w, r, child).await;
        }
        Req::Forward { target } => {
            let tcp = match tokio::time::timeout(Duration::from_secs(15), tokio::net::TcpStream::connect(target.as_str())).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{target}: {e}") }).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
                    return;
                }
                Err(_) => {
                    let _ = proto::write_frame(&mut send, &Resp::Error { message: format!("{target}: no answer") }).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
                    return;
                }
            };
            if proto::write_frame(&mut send, &Resp::Forwarding).await.is_err() {
                return;
            }
            let (mut app, attach, task) = resume::start(Transport { send: Box::new(send), recv: Box::new(recv), peer_received: 0 }, FORWARD_GRACE, None);
            register(id, console, attach, task);
            let mut tcp = tcp;
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut app).await;
        }
        _ => {
            let _ = proto::write_frame(&mut send, &Resp::Error { message: "not a stream request".into() }).await;
        }
    }
}

fn register(id: String, console: String, attach: crate::resume::Attach, task: tokio::task::JoinHandle<()>) {
    streams().lock().unwrap().insert(id.clone(), (console, attach));
    tokio::spawn(async move {
        let _ = task.await;
        streams().lock().unwrap().remove(&id);
    });
}

/// stdin / stdout / stderr / exit of a process as session frames over `w` / `r`.
async fn pipe_child<W, R>(mut send: W, mut recv: R, child: crate::proc::Child)
where
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    use crate::proc::{In, Out};
    use crate::proto::sframe;
    let crate::proc::Child { input, mut output } = child;
    let reader = tokio::spawn(async move {
        loop {
            let msg = match sframe::read(&mut recv).await {
                Ok((sframe::DATA, d)) => In::Data(d),
                Ok((sframe::RESIZE, b)) if b.len() == 4 => In::Resize(u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]])),
                Ok((sframe::EOF, _)) => In::Eof,
                Ok(_) => continue,
                Err(_) => break,
            };
            if input.send(msg).is_err() {
                break;
            }
        }
    });
    while let Some(o) = output.recv().await {
        let r = match o {
            Out::Stdout(d) => sframe::write(&mut send, sframe::DATA, &d).await,
            Out::Stderr(d) => sframe::write(&mut send, sframe::STDERR, &d).await,
            Out::Exit(code) => {
                let _ = sframe::write(&mut send, sframe::EXIT, &code.to_be_bytes()).await;
                break;
            }
        };
        if r.is_err() {
            break;
        }
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
    reader.abort();
}
