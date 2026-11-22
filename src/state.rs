//! `gangli.node`: the node's key, the networks it is managed through, service settings. Shared between the
//! running node and the CLI (`join`, `leave`, `status`, `install`) through [`crate::store`]'s lock; the
//! running node rereads it every 2 seconds, so hand edits apply by themselves.

use anyhow::{bail, Result};
use data_encoding::HEXLOWER;
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

use crate::cert::Cert;
use crate::code::{self, Code};
use crate::store;

const HEADER: &str = "gangli.node — this machine as a managed node (see docs/ARCHITECTURE.md).
It holds the node's secret key: keep it private. gangli rewrites this file when its state changes
(comments other than this header are not kept) and picks up edits by hand within 2 seconds.
Join a network: `gangli join <public code>` or add a [[network]] with only `code = \"<public code>\"`.";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    #[serde(default)]
    pub node: NodeConf,
    #[serde(default, rename = "network")]
    pub networks: Vec<Membership>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct NodeConf {
    /// The node's secret key (hex), made on first start.
    #[serde(default)]
    pub key: String,
    /// Set by `gangli install`: this folder's node runs as the system service.
    #[serde(default)]
    pub installed: bool,
    /// Account of the service (empty: SYSTEM / root).
    #[serde(default)]
    pub service_user: String,
    /// Own iroh relays (`https://relay.example.org`); empty: the public n0 relays (docs/RELAY.md).
    #[serde(default)]
    pub relays: Vec<String>,
    /// With own relays, keep the n0 ones as a fallback.
    #[serde(default = "yes")]
    pub relays_n0_fallback: bool,
    /// Without the service: the gangli window runs the node while it is open (on by default; *Off* in
    /// This computer turns it off).
    #[serde(default = "yes")]
    pub window_node: bool,
    /// Port of the node's status on 127.0.0.1 (read-only; also keeps a second node from starting). Default
    /// 7374; none in the development mode (`GANGLI_DATA_DIR`) unless set here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_port: Option<u16>,
}

pub const STATUS_PORT: u16 = 7374;

impl NodeConf {
    pub fn status_port(&self) -> Option<u16> {
        self.status_port.or_else(|| crate::paths::dev_override().is_none().then_some(STATUS_PORT))
    }
}

/// Where to ask the running node for its status: this folder's setting, else the default (the service's
/// `gangli.node` is readable by the system only).
pub fn status_port() -> Option<u16> {
    match load() {
        Ok(st) => st.node.status_port(),
        Err(_) => Some(STATUS_PORT),
    }
}

fn yes() -> bool {
    true
}

impl Default for NodeConf {
    fn default() -> Self {
        NodeConf {
            key: String::new(),
            installed: false,
            service_user: String::new(),
            relays: vec![],
            relays_n0_fallback: true,
            window_node: true,
            status_port: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Membership {
    /// Local name: the network's name once admitted, `pending-xxxxxx` before.
    #[serde(default)]
    pub name: String,
    /// The public code this membership comes from (network id + join secret).
    #[serde(default)]
    pub code: String,
    /// Network id (public half of the network key), from the code.
    #[serde(default)]
    pub id: String,
    /// Join secret from the code, until a console admits this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(default)]
    pub joined: u64,
    /// Consoles this node knocks on when it comes online.
    #[serde(default)]
    pub consoles: Vec<String>,
    /// Extra iroh relays the network's consoles sent (used on top of the n0 ones and `[node] relays`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relays: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// This node's certificate, signed by the network key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<Cert>,
}

/// Fill in what a hand-written `[[network]]` with only `code = …` lacks.
fn normalize(st: &mut State) {
    for m in st.networks.iter_mut() {
        if m.id.is_empty() {
            if let Ok(Code::Public { network, join }) = code::decode(&m.code) {
                m.id = network.to_string();
                if m.cert.is_none() {
                    m.join = Some(HEXLOWER.encode(&join));
                }
            }
        }
        if m.name.is_empty() && !m.id.is_empty() {
            m.name = format!("pending-{}", &m.id[..6]);
        }
    }
    st.networks.retain(|m| !m.id.is_empty());
}

pub fn load() -> Result<State> {
    let mut st: State = store::load(&crate::paths::node_file())?;
    normalize(&mut st);
    Ok(st)
}

pub fn update<R>(f: impl FnOnce(&mut State) -> Result<R>) -> Result<R> {
    store::update(&crate::paths::node_file(), HEADER, |st: &mut State| {
        normalize(st);
        f(st)
    })
}

/// This machine's node key (its iroh endpoint id), made on first use.
pub fn node_key() -> Result<SecretKey> {
    if let Ok(st) = load() {
        if let Ok(k) = store::secret_from_hex(&st.node.key) {
            return Ok(k);
        }
    }
    update(|st| {
        if let Ok(k) = store::secret_from_hex(&st.node.key) {
            return Ok(k);
        }
        let k = SecretKey::generate();
        st.node.key = store::secret_to_hex(&k);
        Ok(k)
    })
}

/// Join a network with its public code and extra relays (the ones a carried network brings; a console sends
/// its own later). Returns the local name of the membership.
pub fn add_join_with(code_str: &str, relays: &[String]) -> Result<String> {
    let name = add_join(code_str)?;
    let relays = crate::util::relay_list(relays)?;
    if !relays.is_empty() {
        if let Code::Public { network, .. } = code::decode(code_str)? {
            let id = network.to_string();
            update(|st| {
                if let Some(m) = st.networks.iter_mut().find(|m| m.id == id) {
                    for r in &relays {
                        if !m.relays.contains(r) {
                            m.relays.push(r.clone());
                        }
                    }
                }
                Ok(())
            })?;
        }
    }
    Ok(name)
}

/// First start of a binary that carries networks (embed.rs): when there is no `gangli.node` yet, it is made
/// with those networks and their relays. Only then: networks left later stay left; deleting `gangli.node`
/// makes the next start put them back.
pub fn seed_from_bundle() -> Result<()> {
    if crate::paths::node_file().exists() {
        return Ok(());
    }
    let Some(b) = crate::embed::bundled() else { return Ok(()) };
    for n in &b.networks {
        add_join_with(&n.code, &n.relays)?;
    }
    Ok(())
}

/// Join a network with its public code; the running node asks a console to admit it. Returns the local
/// name of the membership (`pending-…` until admitted). Joining a network already joined is a no-op.
pub fn add_join(code_str: &str) -> Result<String> {
    let Code::Public { network, join } = code::decode(code_str)? else {
        bail!("this is a private network code: it belongs in a console (`gangli network add`), never on a managed machine");
    };
    let id = network.to_string();
    let code_str = code_str.trim().to_string();
    update(|st| {
        if let Some(m) = st.networks.iter().find(|m| m.id == id) {
            return Ok(m.name.clone());
        }
        let name = format!("pending-{}", &id[..6]);
        st.networks.push(Membership { name: name.clone(), code: code_str, id, join: Some(HEXLOWER.encode(&join)), ..Default::default() });
        Ok(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip_and_hand_written_code() {
        let key = SecretKey::generate();
        let text = format!("[node]\nkey = \"\"\n\n[[network]]\ncode = \"{}\"\n", code::public_code(&key));
        let mut st: State = toml::from_str(&text).unwrap();
        normalize(&mut st);
        assert_eq!(st.networks[0].id, key.public().to_string());
        assert!(st.networks[0].join.is_some());
        assert!(st.networks[0].name.starts_with("pending-"));
        let again: State = toml::from_str(&toml::to_string_pretty(&st).unwrap()).unwrap();
        assert_eq!(again.networks[0].id, st.networks[0].id);
    }
}
