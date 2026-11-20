//! Service state: the networks this machine belongs to (agent) or runs (master). Stored in
//! `state.json` under [`crate::paths::state_dir`], next to the node key `node.key`. Shared between the
//! running service and elevated CLI commands through [`crate::store`]'s lock.
//! No `Debug` on these types: they hold network keys, which must never end up in a log.

use std::path::PathBuf;

use anyhow::{bail, Result};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

use crate::cert::Cert;
use crate::code::{self, Kind};
use crate::proto::NodeRec;
use crate::store;

pub const FILE: &str = "state.json";
pub const KEY_FILE: &str = "node.key";

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    #[serde(default)]
    pub networks: Vec<Network>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Network {
    /// Local name: the network's name once joined, `pending-xxxxxx` before.
    pub name: String,
    /// The network id (public half of the network key); unknown until an agent has joined.
    #[serde(default)]
    pub id: Option<String>,
    pub role: Role,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Role {
    Agent(AgentNet),
    Master(MasterNet),
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct AgentNet {
    /// Endpoint id of the master (from the join code).
    pub master: String,
    /// Join secret, kept only until the join succeeds.
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub cert: Option<Cert>,
    #[serde(default)]
    pub master_cert: Option<Cert>,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_report: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct MasterNet {
    /// The network private key (hex). This is what makes this machine the master.
    pub key: String,
    #[serde(default)]
    pub codes: Vec<CodeRec>,
    #[serde(default)]
    pub nodes: Vec<NodeRec>,
    #[serde(default)]
    pub clients: Vec<ClientRec>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CodeRec {
    pub kind: Kind,
    pub secret: String,
    pub created: u64,
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub uses_left: Option<u32>,
    #[serde(default)]
    pub label: Option<String>,
}

impl CodeRec {
    pub fn valid(&self, now: u64) -> bool {
        self.expires.is_none_or(|e| e > now) && self.uses_left != Some(0)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClientRec {
    pub id: String,
    pub name: String,
    pub paired: u64,
}

pub fn dir() -> PathBuf {
    crate::paths::state_dir()
}

fn prepare() -> Result<()> {
    if !dir().exists() {
        crate::service::secure_state_dir()?;
    }
    Ok(())
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

/// Record a join code; the running service enrolls with the master. Returns the pending local name.
pub fn add_join(code_str: &str) -> Result<String> {
    let c = code::decode(code_str)?;
    if c.kind != Kind::Join {
        bail!("this is a client pairing code: use `gangli pair <code>` on your client instead");
    }
    let secret = c.secret_hex();
    update(|st| {
        if let Some(n) = st.networks.iter().find(|n| match &n.role {
            Role::Agent(a) => a.secret.as_deref() == Some(secret.as_str()),
            _ => false,
        }) {
            return Ok(n.name.clone());
        }
        let name = format!("pending-{}", &secret[..6]);
        st.networks.push(Network {
            name: name.clone(),
            id: None,
            role: Role::Agent(AgentNet {
                master: c.master.to_string(),
                secret: Some(secret),
                ..Default::default()
            }),
        });
        Ok(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_roundtrip() {
        let st = State {
            networks: vec![
                Network {
                    name: "home".into(),
                    id: Some("abc".into()),
                    role: Role::Master(MasterNet {
                        key: "00".into(),
                        codes: vec![],
                        nodes: vec![],
                        clients: vec![],
                    }),
                },
                Network {
                    name: "pending-123456".into(),
                    id: None,
                    role: Role::Agent(AgentNet {
                        master: "m".into(),
                        secret: Some("s".into()),
                        ..Default::default()
                    }),
                },
            ],
        };
        let back: State = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert!(matches!(back.networks[0].role, Role::Master(_)));
        assert!(matches!(back.networks[1].role, Role::Agent(_)));
    }

    #[test]
    fn code_validity() {
        let c = CodeRec { kind: Kind::Join, secret: "x".into(), created: 0, expires: Some(10), uses_left: Some(1), label: None };
        assert!(c.valid(5));
        assert!(!c.valid(10));
        let used = CodeRec { uses_left: Some(0), expires: None, ..c };
        assert!(!used.valid(5));
    }
}
