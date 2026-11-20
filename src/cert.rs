//! Certificates signed by a network key (section 3 of docs/ARCHITECTURE.md). A certificate binds an endpoint
//! id (the subject) to a role in one network. The signature covers a domain-separated JSON encoding of the
//! body; serde_json writes struct fields in declaration order, so the verifier re-encodes the same bytes.

use anyhow::{anyhow, bail, Result};
use data_encoding::HEXLOWER;
use iroh::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

use crate::util::now;

pub const TEN_YEARS: u64 = 10 * 365 * 86400;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CertKind {
    /// A managed machine.
    Node,
    /// One of the owner's consoles.
    Console,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CertBody {
    pub network: String,
    pub subject: String,
    pub kind: CertKind,
    pub name: String,
    pub issued: u64,
    pub expires: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cert {
    // `sig` first: TOML wants plain values before tables.
    pub sig: String,
    pub body: CertBody,
}

fn message(body: &CertBody) -> Vec<u8> {
    let mut m = b"gangli-cert-v1\0".to_vec();
    m.extend(serde_json::to_vec(body).expect("cert body serializes"));
    m
}

impl Cert {
    pub fn issue(key: &SecretKey, subject: &str, kind: CertKind, name: &str, valid_secs: u64) -> Cert {
        let issued = now();
        let body = CertBody {
            network: key.public().to_string(),
            subject: subject.to_string(),
            kind,
            name: name.to_string(),
            issued,
            expires: issued + valid_secs,
        };
        let sig = key.sign(&message(&body));
        Cert {
            sig: HEXLOWER.encode(&sig.to_bytes()),
            body,
        }
    }

    /// Checks network, subject, kind, expiry and signature.
    pub fn verify(&self, network: &PublicKey, subject: &PublicKey, kind: CertKind) -> Result<()> {
        let b = &self.body;
        if b.network != network.to_string() {
            bail!("certificate is for another network");
        }
        if b.subject != subject.to_string() {
            bail!("certificate belongs to another endpoint");
        }
        if b.kind != kind {
            bail!("certificate has the wrong role");
        }
        if b.expires <= now() {
            bail!("certificate expired");
        }
        let raw = HEXLOWER
            .decode(self.sig.as_bytes())
            .map_err(|_| anyhow!("malformed signature"))?;
        // The signature type is inferred from `verify`'s parameter.
        let sig = TryFrom::try_from(&raw[..]).map_err(|_| anyhow!("malformed signature"))?;
        network
            .verify(&message(b), &sig)
            .map_err(|_| anyhow!("bad certificate signature"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_and_verify() {
        let net = SecretKey::generate();
        let node = SecretKey::generate().public();
        let c = Cert::issue(&net, &node.to_string(), CertKind::Node, "nas", TEN_YEARS);
        c.verify(&net.public(), &node, CertKind::Node).unwrap();
        assert!(c.verify(&net.public(), &node, CertKind::Console).is_err());
        let other = SecretKey::generate().public();
        assert!(c.verify(&net.public(), &other, CertKind::Node).is_err());
        let mut tampered = c.clone();
        tampered.body.name = "evil".into();
        assert!(tampered.verify(&net.public(), &node, CertKind::Node).is_err());
        let foreign = SecretKey::generate();
        assert!(c.verify(&foreign.public(), &node, CertKind::Node).is_err());
    }

    #[test]
    fn survives_json_roundtrip() {
        let net = SecretKey::generate();
        let node = SecretKey::generate().public();
        let c = Cert::issue(&net, &node.to_string(), CertKind::Console, "laptop", TEN_YEARS);
        let back: Cert = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        back.verify(&net.public(), &node, CertKind::Console).unwrap();
    }
}
