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
Join a network: `gangli join <public code>` or add a [[network]] with only `code = \"gangli-pub-…\"`.";

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
}

fn yes() -> bool {
    true
}

impl Default for NodeConf {
    fn default() -> Self {
        NodeConf { key: String::new(), installed: false, service_user: String::new(), relays: vec![], relays_n0_fallback: true }
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
