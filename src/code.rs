//! Join codes (enroll a node) and pairing codes (pair a client), section 3 of docs/ARCHITECTURE.md.
//! Layout: kind (1 byte) | master endpoint id (32) | secret (16), base32 without padding, lowercase, with a
//! `gangli-` prefix: 7 + 79 characters. The master id is the trust anchor (iroh authenticates it); the master
//! hands out the network id and its own certificate during the join.

use anyhow::{bail, Result};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use iroh::PublicKey;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "gangli-";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Join,
    Pair,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Join => 1,
            Kind::Pair => 2,
        }
    }
}

pub struct Code {
    pub kind: Kind,
    pub master: PublicKey,
    pub secret: [u8; 16],
}

impl Code {
    pub fn secret_hex(&self) -> String {
        HEXLOWER.encode(&self.secret)
    }
}

pub fn encode(kind: Kind, master: &PublicKey, secret: &[u8; 16]) -> String {
    let mut raw = Vec::with_capacity(49);
    raw.push(kind.byte());
    raw.extend_from_slice(master.as_bytes());
    raw.extend_from_slice(secret);
    format!("{PREFIX}{}", BASE32_NOPAD.encode(&raw).to_ascii_lowercase())
}

pub fn decode(s: &str) -> Result<Code> {
    let s: String = s.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let body = s.strip_prefix(PREFIX).unwrap_or(&s).to_ascii_uppercase();
    let Ok(raw) = BASE32_NOPAD.decode(body.as_bytes()) else {
        bail!("not a gangli code (bad characters)");
    };
    if raw.len() != 49 {
        bail!("not a gangli code (wrong length)");
    }
    let kind = match raw[0] {
        1 => Kind::Join,
        2 => Kind::Pair,
        _ => bail!("unknown code type"),
    };
    let master_bytes: [u8; 32] = raw[1..33].try_into().unwrap();
    let Ok(master) = PublicKey::from_bytes(&master_bytes) else {
        bail!("code contains an invalid master id");
    };
    let secret: [u8; 16] = raw[33..49].try_into().unwrap();
    Ok(Code { kind, master, secret })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let master = iroh::SecretKey::generate().public();
        let secret = [7u8; 16];
        let s = encode(Kind::Join, &master, &secret);
        assert!(s.starts_with("gangli-"));
        assert_eq!(s.len(), 7 + 79);
        let c = decode(&s).unwrap();
        assert_eq!(c.kind, Kind::Join);
        assert_eq!(c.master, master);
        assert_eq!(c.secret, secret);
        // tolerant to case, spaces and a missing prefix
        let messy = format!("  {} ", s.to_uppercase().replace("GANGLI-", ""));
        assert_eq!(decode(&messy).unwrap().master, master);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("gangli-hello").is_err());
        assert!(decode("").is_err());
    }
}
