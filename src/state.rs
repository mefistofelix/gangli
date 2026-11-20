//! Node state: the networks this machine is managed through. Stored in `state.json` under
//! [`crate::paths::node_dir`] (next to the binary),
//! next to the node key `node.key`. Shared between the running node and the CLI (`join`, `leave`,
//! `status`) through [`crate::store`]'s lock.

use std::path::PathBuf;

use anyhow::{bail, Result};
use data_encoding::HEXLOWER;
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

use crate::cert::Cert;
use crate::code::{self, Code};
use crate::store;

pub const FILE: &str = "state.json";
pub const KEY_FILE: &str = "node.key";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    #[serde(default)]
    pub networks: Vec<Membership>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Membership {
    /// Local name: the network's name once admitted, `pending-xxxxxx` before.
    pub name: String,
    /// Network id (public half of the network key), from the public code.
    pub id: String,
    /// Join secret from the public code, kept until a console admits this node.
    #[serde(default)]
    pub join: Option<String>,
    /// This node's certificate, signed by the network key.
    #[serde(default)]
    pub cert: Option<Cert>,
    #[serde(default)]
    pub node_name: Option<String>,
    /// Consoles this node knocks on when it comes online.
    #[serde(default)]
    pub consoles: Vec<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub joined: u64,
}

pub fn dir() -> PathBuf {
    crate::paths::node_dir()
}

fn prepare() -> Result<()> {
    store::ensure_dir(&dir())
}

pub fn load() -> Result<State> {
    store::load(&dir(), FILE)
}

pub fn update<R>(f: impl FnOnce(&mut State) -> Result<R>) -> Result<R> {
    prepare()?;
    store::update(&dir(), FILE, f)
}

/// This machine's node key (its iroh endpoint id), created on first use.
pub fn node_key() -> Result<SecretKey> {
    prepare()?;
    store::secret_key(&dir(), KEY_FILE)
}

/// Join a network with its public code; the running node asks a console to admit it. Returns the local
/// name of the membership (`pending-…` until admitted). Joining a network already joined is a no-op.
pub fn add_join(code_str: &str) -> Result<String> {
    let Code::Public { network, join } = code::decode(code_str)? else {
        bail!("this is a private network code: it belongs in a console (`gangli network add`), never on a managed machine");
    };
    let id = network.to_string();
    update(|st| {
        if let Some(m) = st.networks.iter().find(|m| m.id == id) {
            return Ok(m.name.clone());
        }
        let name = format!("pending-{}", &id[..6]);
        st.networks.push(Membership { name: name.clone(), id, join: Some(HEXLOWER.encode(&join)), ..Default::default() });
        Ok(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_roundtrip() {
        let st = State {
            networks: vec![Membership { name: "pending-123456".into(), id: "abc".into(), join: Some("s".into()), ..Default::default() }],
        };
        let back: State = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back.networks[0].name, "pending-123456");
        assert!(back.networks[0].join.is_some());
    }
}
