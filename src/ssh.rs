//! The console's SSH gateway (section 7 of docs/ARCHITECTURE.md): an SSH server on 127.0.0.1 (default port
//! 2222) for PuTTY, OpenSSH, WinSCP or any SSH client. The host is always 127.0.0.1, so **the username says
//! where to go**: `user+machine` (or just `machine` for its default user; `machine.network` when two
//! networks have the same name). The password or public key comes from the `[[ssh.login]]` rules in
//! `gangli.console`, the most specific match winning. After the login, shells, commands and port
//! forwards become gangli streams to the machine: there is no SSH server on the nodes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use data_encoding::BASE32_NOPAD;
use russh::server::{Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::console::{self, Console};
use crate::proto::{self, sframe, Req, Resp};
use crate::util::{eq_ct, random16, Ctx};

#[derive(Serialize, Deserialize, Clone)]
pub struct SshConf {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default, rename = "login")]
    pub logins: Vec<LoginRule>,
}

fn yes() -> bool {
    true
}

fn default_listen() -> String {
    "127.0.0.1:2222".into()
}

impl Default for SshConf {
    fn default() -> Self {
        SshConf { enabled: true, listen: default_listen(), logins: vec![] }
    }
}

/// Who may log in through the gateway: machines × users, with a password and / or public keys.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LoginRule {
    /// Machine names (`nas`, `nas.home`) or `*`.
    pub hosts: Vec<String>,
    /// Accounts on the machine (`root`, `desktop`, `system`…) or `*`.
    pub users: Vec<String>,
    pub password: String,
    /// OpenSSH public keys (`ssh-ed25519 AAAA… comment`).
    #[serde(default)]
    pub keys: Vec<String>,
}

/// An external program for the per-machine drop-down / `gangli open`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Launcher {
    pub name: String,
    pub command: String,
}

pub fn new_password() -> String {
    let s = BASE32_NOPAD.encode(&random16()).to_ascii_lowercase();
    format!("{}-{}-{}", &s[0..4], &s[4..8], &s[8..12])
}

pub fn default_rule() -> LoginRule {
    LoginRule { hosts: vec!["*".into()], users: vec!["*".into()], password: new_password(), keys: vec![] }
}

/// The ready-made launchers for this OS.
pub fn default_launchers() -> Vec<Launcher> {
    let l = |n: &str, c: &str| Launcher { name: n.into(), command: c.into() };
    let accept = "-o StrictHostKeyChecking=accept-new";
    if cfg!(windows) {
        vec![
            l("PuTTY", "putty.exe -ssh {login}@127.0.0.1 -P {port} -pw {password}"),
            l("OpenSSH", &format!("cmd.exe /c start \"{{node}}\" ssh -p {{port}} {accept} {{login}}@127.0.0.1")),
            l("WinSCP", "winscp.exe sftp://{login_url}:{password_url}@127.0.0.1:{port}/ /hostkey=*"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            l("OpenSSH", &format!("osascript -e 'tell application \"Terminal\" to do script \"ssh -p {{port}} {accept} {{login}}@127.0.0.1\"'")),
            l("PuTTY", "putty -ssh {login}@127.0.0.1 -P {port} -pw {password}"),
        ]
    } else {
        vec![
            l("OpenSSH", &format!("x-terminal-emulator -e ssh -p {{port}} {accept} {{login}}@127.0.0.1")),
            l("PuTTY", "putty -ssh {login}@127.0.0.1 -P {port} -pw {password}"),
        ]
    }
}

/// `user+machine` → (user, machine); `machine` alone → (`default`, machine).
pub fn parse_login(s: &str) -> (String, String) {
    let (u, m, _) = parse_login_full(s);
    (u, m)
}

/// `user+machine+name` also names a persistent terminal to attach to (started if missing).
pub fn parse_login_full(s: &str) -> (String, String, Option<String>) {
    let parts: Vec<&str> = s.split('+').collect();
    let user = |u: &str| if u.is_empty() { "default".to_string() } else { u.to_string() };
    match parts.as_slice() {
        [m] => ("default".into(), m.to_string(), None),
        [u, m] => (user(u), m.to_string(), None),
        [u, m, t, ..] => (user(u), m.to_string(), Some(t.to_string()).filter(|t| !t.is_empty())),
        [] => ("default".into(), String::new(), None),
    }
}

/// The most specific rules for a machine (its names) and a user: exact machine beats `*`, then exact user
/// beats `*`. Several rules can tie; a credential is good if any of them accepts it.
pub fn best_rules<'a>(rules: &'a [LoginRule], names: &[String], user: &str) -> Vec<&'a LoginRule> {
    let host_score = |r: &LoginRule| {
        if r.hosts.iter().any(|h| names.iter().any(|n| n.eq_ignore_ascii_case(h))) {
            Some(2)
        } else if r.hosts.iter().any(|h| h == "*") {
            Some(0)
        } else {
            None
        }
    };
    let user_score = |r: &LoginRule| {
        if r.users.iter().any(|u| u.eq_ignore_ascii_case(user)) {
            Some(1)
        } else if r.users.iter().any(|u| u == "*") {
            Some(0)
        } else {
            None
        }
    };
    let scored: Vec<(i32, &LoginRule)> = rules.iter().filter_map(|r| Some((host_score(r)? + user_score(r)?, r))).collect();
    let Some(best) = scored.iter().map(|(s, _)| *s).max() else { return vec![] };
    scored.into_iter().filter(|(s, _)| *s == best).map(|(_, r)| r).collect()
}

/// The rule whose password applies (the first of the most specific ones that has one).
pub fn rule_for<'a>(rules: &'a [LoginRule], names: &[String], user: &str) -> Option<&'a LoginRule> {
    let best = best_rules(rules, names, user);
    best.iter().find(|r| !r.password.is_empty()).or(best.first()).copied()
}

/// Fill a launcher command; returns program and arguments.
pub fn fill(command: &str, vars: &[(&str, String)]) -> Vec<String> {
    split_words(command)
        .into_iter()
        .map(|w| {
            let mut w = w;
            for (k, v) in vars {
                w = w.replace(&format!("{{{k}}}"), v);
            }
            w
        })
        .collect()
}

/// Words separated by spaces; `'…'` and `"…"` group (no escapes: Windows paths stay as they are).
fn split_words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The gateway's host key: derived from the console key, so it never changes and clients never warn.
fn host_key(c: &Console) -> Result<russh::keys::PrivateKey> {
    let sig = c.secret_key()?.sign(b"gangli-ssh-host-v1").to_bytes();
    let seed: [u8; 32] = sig[..32].try_into().unwrap();
    let kp = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&seed);
    russh::keys::PrivateKey::new(russh::keys::ssh_key::private::KeypairData::from(kp), "gangli").map_err(|e| anyhow!("host key: {e}"))
}

/// Start the gateway of an open console (if enabled).
pub async fn start(c: Arc<Console>) {
    let conf = c.ssh_conf();
    if !conf.enabled {
        return;
    }
    let key = match host_key(&c) {
        Ok(k) => k,
        Err(e) => {
            console::clog(&format!("SSH gateway: {e:#}"));
            return;
        }
    };
    let listener = match TcpListener::bind(conf.listen.as_str()).await {
        Ok(l) => l,
        Err(e) => {
            console::clog(&format!("SSH gateway: cannot listen on {}: {e}", conf.listen));
            return;
        }
    };
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        auth_rejection_time: Duration::from_millis(500),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    console::clog(&format!("SSH gateway on {} (user+machine logins)", conf.listen));
    tokio::spawn(async move {
        use russh::server::Server as _;
        let mut g = Gateway { c };
        let _ = g.run_on_socket(config, &listener).await;
    });
}

#[derive(Clone)]
struct Gateway {
    c: Arc<Console>,
}

impl russh::server::Server for Gateway {
    type Handler = Conn;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Conn {
        Conn { c: self.c.clone(), target: None, chans: HashMap::new() }
    }
}

struct Target {
    net: String,
    node: String,
    user: String,
    /// Persistent terminal to attach to (`user+machine+name`).
    term: Option<String>,
}

#[derive(Default)]
struct Chan {
    pty: Option<(u16, u16)>,
    term: String,
    tx: Option<mpsc::UnboundedSender<ToNode>>,
}

enum ToNode {
    Data(Vec<u8>),
    Resize(u16, u16),
    Eof,
}

struct Conn {
    c: Arc<Console>,
    target: Option<Target>,
    chans: HashMap<ChannelId, Chan>,
}

fn reject() -> Auth {
    Auth::Reject { proceed_with_methods: None, partial_success: false }
}

impl Conn {
    /// Resolve `user+machine` and check the credential against the best rule.
    fn login(&mut self, login: &str, ok: impl Fn(&LoginRule) -> bool) -> Auth {
        let (user, machine, term) = parse_login_full(login);
        let Ok((net, net_name, node)) = console::find_node(&self.c, &machine) else { return reject() };
        let name = self.c.node_name(&net, &node).unwrap_or_default();
        let names = vec![name.clone(), format!("{name}.{net_name}"), node.clone()];
        let rules = self.c.ssh_conf().logins;
        match best_rules(&rules, &names, &user).into_iter().find(|r| ok(r)) {
            Some(_) => {
                console::clog(&format!("SSH login {user}+{name}"));
                self.target = Some(Target { net, node, user, term });
                Auth::Accept
            }
            _ => reject(),
        }
    }

    fn start(&mut self, channel: ChannelId, command: Option<String>, session: &mut Session) -> Result<(), russh::Error> {
        let Some(t) = &self.target else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        let chan = self.chans.entry(channel).or_default();
        let (tx, rx) = mpsc::unbounded_channel();
        chan.tx = Some(tx);
        let spec = Spec {
            // A shell request with a terminal name: attach to the persistent terminal.
            attach: if command.is_none() { t.term.clone() } else { None },
            user: t.user.clone(),
            command,
            pty: chan.pty,
            term: if chan.term.is_empty() { "xterm-256color".into() } else { chan.term.clone() },
        };
        let (c, net, node, handle) = (self.c.clone(), t.net.clone(), t.node.clone(), session.handle());
        session.channel_success(channel)?;
        tokio::spawn(async move {
            let code = match bridge(&c, &net, &node, spec, rx, &handle, channel).await {
                Ok(code) => code,
                Err(e) => {
                    let _ = handle.extended_data(channel, 1, format!("gangli: {e:#}\r\n").into_bytes()).await;
                    1
                }
            };
            let _ = handle.exit_status_request(channel, code as u32).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
        Ok(())
    }
}

/// What an SSH session channel asks the machine for.
struct Spec {
    /// A persistent terminal to attach to (`user+machine+name`, a shell request).
    attach: Option<String>,
    user: String,
    command: Option<String>,
    pty: Option<(u16, u16)>,
    term: String,
}

impl Spec {
    fn req(&self) -> Req {
        match &self.attach {
            Some(name) => {
                let (cols, rows) = self.pty.unwrap_or((80, 24));
                Req::Attach { name: name.clone(), user: self.user.clone(), cols, rows }
            }
            None => Req::Shell { user: self.user.clone(), command: self.command.clone(), pty: self.pty, term: self.term.clone() },
        }
    }

    /// An interactive shell (a terminal, no command): it outlives the machine going away.
    fn interactive(&self) -> bool {
        self.command.is_none() && (self.pty.is_some() || self.attach.is_some())
    }
}

/// One SSH session channel ↔ node session streams. Returns the exit code.
///
/// Each stream is resumable (it survives connection drops). When one cannot be resumed (the machine
/// restarted, or stayed away too long), an interactive shell does not end: the SSH client, connected to this
/// local gateway, keeps its window and its screen; the gateway says the machine is away, waits for it, then
/// goes on in the same channel (the persistent terminal reattached, or a new shell), saying so. A command
/// (`ssh machine cmd`) fails instead: a program must know it did not finish.
async fn bridge(
    c: &Arc<Console>,
    net: &str,
    node: &str,
    mut spec: Spec,
    mut rx: mpsc::UnboundedReceiver<ToNode>,
    handle: &russh::server::Handle,
    channel: ChannelId,
) -> Result<i32> {
    let say = |text: String| async move {
        let _ = handle.data(channel, format!("\r\n\x1b[2m[gangli: {text}]\x1b[0m\r\n").into_bytes()).await;
    };
    let mut why: Option<String> = None;
    let mut waiting_said = false;
    loop {
        let app = match console::open_resumable(c, net, node, spec.req(), Duration::from_secs(24 * 3600)).await {
            Ok((app, Resp::Started)) => app,
            Ok(_) => bail!("unexpected answer"),
            Err(e) if !spec.interactive() || why.is_none() || c.link(node).is_some() => return Err(e),
            Err(_) => {
                if !waiting_said {
                    waiting_said = true;
                    say("the machine is not reachable: this session goes on when it is back".into()).await;
                }
                // Typed meanwhile: dropped (not for the next shell); the size is kept; closing ends it.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                loop {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Err(_) => break,
                        Ok(None) => return Ok(255),
                        Ok(Some(ToNode::Resize(cols, rows))) => spec.pty = Some((cols, rows)),
                        Ok(Some(_)) => {}
                    }
                }
                continue;
            }
        };
        waiting_said = false;
        if let Some(w) = why.take() {
            let what = match &spec.attach {
                Some(n) => format!("{w}: reattached to the persistent terminal {n}"),
                None => format!("{w}: a new shell"),
            };
            say(what).await;
        }
        let (mut recv, mut send) = tokio::io::split(app);
        if let Some((cols, rows)) = spec.pty {
            let _ = sframe::write(&mut send, sframe::RESIZE, &sframe::resize(cols, rows)).await;
        }
        // The reader runs apart: a frame half read must never be cut by the select below.
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            loop {
                let r = sframe::read(&mut recv).await.map_err(|_| ());
                let stop = r.is_err();
                if ev_tx.send(r).is_err() || stop {
                    break;
                }
            }
        });
        // While the stream waits for the machine (it resumes by itself when the machine is back), say so.
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut away = false;
        let exit = loop {
            tokio::select! {
                _ = tick.tick(), if spec.interactive() => {
                    let gone = c.link(node).is_none();
                    if gone && !away {
                        say("the machine is not reachable: this session goes on when it is back".into()).await;
                        waiting_said = true;
                    } else if !gone && away {
                        say("the machine is back".into()).await;
                        waiting_said = false;
                    }
                    away = gone;
                }
                m = rx.recv() => {
                    let Some(m) = m else {
                        reader.abort();
                        return Ok(255);
                    };
                    let r = match m {
                        ToNode::Data(d) => sframe::write(&mut send, sframe::DATA, &d).await,
                        ToNode::Resize(cols, rows) => {
                            spec.pty = Some((cols, rows));
                            sframe::write(&mut send, sframe::RESIZE, &sframe::resize(cols, rows)).await
                        }
                        ToNode::Eof => sframe::write(&mut send, sframe::EOF, &[]).await,
                    };
                    if r.is_err() {
                        break None;
                    }
                }
                e = ev_rx.recv() => match e {
                    Some(Ok((sframe::DATA, body))) => {
                        if handle.data(channel, body).await.is_err() {
                            reader.abort();
                            return Ok(255);
                        }
                    }
                    Some(Ok((sframe::STDERR, body))) => {
                        let _ = handle.extended_data(channel, 1, body).await;
                    }
                    Some(Ok((sframe::EXIT, body))) if body.len() >= 4 => break Some(i32::from_be_bytes(body[..4].try_into().unwrap())),
                    Some(Ok(_)) => {}
                    _ => break None,
                }
            }
        };
        reader.abort();
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut send).await;
        match exit {
            Some(code) => return Ok(code),
            None if spec.interactive() => why = Some("connection lost".into()),
            None => bail!("the connection with the machine was lost: the command may not have finished"),
        }
    }
}

fn key_matches(rule: &LoginRule, key: &russh::keys::PublicKey) -> bool {
    rule.keys.iter().any(|line| russh::keys::PublicKey::from_openssh(line.trim()).map(|k| k.key_data() == key.key_data()).unwrap_or(false))
}

impl russh::server::Handler for Conn {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(reject())
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(self.login(user, |r| !r.password.is_empty() && eq_ct(&r.password, password)))
    }

    async fn auth_publickey(&mut self, user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        let pk = key.public_key.clone();
        Ok(self.login(user, |r| key_matches(r, &pk)))
    }

    async fn channel_open_session(&mut self, channel: Channel<Msg>, reply: ChannelOpenHandle, _session: &mut Session) -> Result<(), Self::Error> {
        self.chans.insert(channel.id(), Chan::default());
        let _ = reply.accept().await;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(t) = &self.target else { return Ok(()) };
        let _ = reply.accept().await;
        let (c, net, node, target) = (self.c.clone(), t.net.clone(), t.node.clone(), format!("{host_to_connect}:{port_to_connect}"));
        tokio::spawn(async move {
            let r: Result<()> = async {
                let (mut remote, resp) = console::open_resumable(&c, &net, &node, Req::Forward { target: target.clone() }, Duration::from_secs(15 * 60)).await?;
                if !matches!(resp, Resp::Forwarding) {
                    bail!("unexpected answer");
                }
                let mut local = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                Ok(())
            }
            .await;
            if let Err(e) = r {
                console::clog(&format!("SSH forward to {target}: {e:#}"));
            }
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let chan = self.chans.entry(channel).or_default();
        // No size from the client (e.g. `ssh -tt` without a terminal): the usual 80×24.
        let (cols, rows) = if col_width == 0 || row_height == 0 { (80, 24) } else { (col_width.min(1000), row_height.min(1000)) };
        chan.pty = Some((cols as u16, rows as u16));
        chan.term = term.to_string();
        session.channel_success(channel)?;
        Ok(())
    }

    async fn env_request(&mut self, channel: ChannelId, _name: &str, _value: &str, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        self.start(channel, None, session)
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        self.start(channel, Some(command), session)
    }

    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        if name == "sftp" {
            // The node runs gangli's own SFTP server as the session user.
            return self.start(channel, Some(crate::sftp::COMMAND.to_string()), session);
        }
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(tx) = self.chans.get(&channel).and_then(|c| c.tx.as_ref()) {
            if col_width > 0 && row_height > 0 {
                let _ = tx.send(ToNode::Resize(col_width.min(1000) as u16, row_height.min(1000) as u16));
            }
        }
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.chans.get(&channel).and_then(|c| c.tx.as_ref()) {
            let _ = tx.send(ToNode::Data(data.to_vec()));
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.chans.get(&channel).and_then(|c| c.tx.as_ref()) {
            let _ = tx.send(ToNode::Eof);
        }
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.chans.remove(&channel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins() {
        assert_eq!(parse_login("root+nas"), ("root".into(), "nas".into()));
        assert_eq!(parse_login("nas.home"), ("default".into(), "nas.home".into()));
        assert_eq!(parse_login("desktop+nonna-pc"), ("desktop".into(), "nonna-pc".into()));
        assert_eq!(parse_login_full("root+nas+work"), ("root".into(), "nas".into(), Some("work".into())));
        assert_eq!(parse_login_full("+nas+work"), ("default".into(), "nas".into(), Some("work".into())));
    }

    #[test]
    fn most_specific_rule_wins() {
        let r = |h: &str, u: &str, p: &str| LoginRule { hosts: vec![h.into()], users: vec![u.into()], password: p.into(), keys: vec![] };
        let rules = vec![r("*", "*", "all"), r("nas", "*", "nas"), r("*", "root", "root"), r("nas", "root", "nasroot")];
        let names = vec!["nas".to_string(), "nas.home".to_string()];
        assert_eq!(rule_for(&rules, &names, "root").unwrap().password, "nasroot");
        assert_eq!(rule_for(&rules, &names, "bob").unwrap().password, "nas");
        assert_eq!(rule_for(&rules, &["pc".to_string()], "root").unwrap().password, "root");
        assert_eq!(rule_for(&rules, &["pc".to_string()], "bob").unwrap().password, "all");
        assert!(rule_for(&rules[1..2], &["pc".to_string()], "bob").is_none());
        // ties: both equally specific rules count
        let tied = vec![r("*", "*", "one"), r("*", "*", "two")];
        assert_eq!(best_rules(&tied, &names, "x").len(), 2);
    }

    #[test]
    fn launcher_filling() {
        let v = fill("\"C:\\Program Files\\PuTTY\\putty.exe\" -ssh {login}@127.0.0.1 -P {port}", &[("login", "root+nas".into()), ("port", "2222".into())]);
        assert_eq!(v, vec!["C:\\Program Files\\PuTTY\\putty.exe", "-ssh", "root+nas@127.0.0.1", "-P", "2222"]);
        assert_eq!(url_encode("root+nas"), "root%2Bnas");
    }
}
