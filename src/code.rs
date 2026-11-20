//! A network *is* its key (section 3 of docs/ARCHITECTURE.md). Two codes come out of it:
//! - the **public code** (`gangli-pub-…`): network id + join secret. Given to machines to manage (also
//!   embedded in a binary). It lets a machine become a node, nothing more.
//! - the **private code** (`gangli-key-…`): the network key itself. Pasted into a console, it makes that
//!   console an owner of the network: full control. Never give it away.
//!
//! The join secret and the directory key are derived from the network key with deterministic Ed25519
//! signatures over fixed labels, so every console holding the key derives the same values.

use anyhow::{bail, Result};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use iroh::{PublicKey, SecretKey};

const PUB_PREFIX: &str = "gangli-pub-";
const KEY_PREFIX: &str = "gangli-key-";
const KIND_PUBLIC: u8 = 1;
const KIND_PRIVATE: u8 = 2;

pub enum Code {
    Public { network: PublicKey, join: [u8; 16] },
    Private { key: SecretKey },
}

/// The secret a new node shows to be admitted.
pub fn join_secret(key: &SecretKey) -> [u8; 16] {
    let s = key.sign(b"gangli-join-v1").to_bytes();
    s[..16].try_into().unwrap()
}

pub fn join_secret_hex(key: &SecretKey) -> String {
    HEXLOWER.encode(&join_secret(key))
}

/// Symmetric key for the encrypted part of the public directory (node ids).
pub fn directory_key(key: &SecretKey) -> [u8; 32] {
    let s = key.sign(b"gangli-directory-v1").to_bytes();
    s[..32].try_into().unwrap()
}

pub fn public_code(key: &SecretKey) -> String {
    let mut raw = vec![KIND_PUBLIC];
    raw.extend_from_slice(key.public().as_bytes());
    raw.extend_from_slice(&join_secret(key));
    format!("{PUB_PREFIX}{}", BASE32_NOPAD.encode(&raw).to_ascii_lowercase())
}

/// Network key + the first 4 bytes of its public half, so a mistyped private code is refused instead of
/// silently making another network.
pub fn private_code(key: &SecretKey) -> String {
    let mut raw = vec![KIND_PRIVATE];
    raw.extend_from_slice(&key.to_bytes());
    raw.extend_from_slice(&key.public().as_bytes()[..4]);
    format!("{KEY_PREFIX}{}", BASE32_NOPAD.encode(&raw).to_ascii_lowercase())
}

pub fn decode(s: &str) -> Result<Code> {
    let s: String = s.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let lower = s.to_ascii_lowercase();
    let body = lower
        .strip_prefix(PUB_PREFIX)
        .or_else(|| lower.strip_prefix(KEY_PREFIX))
        .unwrap_or(&lower)
        .to_ascii_uppercase();
    let Ok(raw) = BASE32_NOPAD.decode(body.as_bytes()) else {
        bail!("not a gangli code (bad characters)");
    };
    match (raw.first().copied(), raw.len()) {
        (Some(KIND_PUBLIC), 49) => {
            let id: [u8; 32] = raw[1..33].try_into().unwrap();
            let Ok(network) = PublicKey::from_bytes(&id) else {
                bail!("the code contains an invalid network id (mistyped?)");
            };
            Ok(Code::Public { network, join: raw[33..49].try_into().unwrap() })
        }
        (Some(KIND_PRIVATE), 37) => {
            let k: [u8; 32] = raw[1..33].try_into().unwrap();
            let key = SecretKey::from_bytes(&k);
            if key.public().as_bytes()[..4] != raw[33..37] {
                bail!("the private code is mistyped (checksum)");
            }
            Ok(Code::Private { key })
        }
        _ => bail!("not a gangli code (wrong length)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_roundtrip() {
        let key = SecretKey::generate();
        let s = public_code(&key);
        assert!(s.starts_with("gangli-pub-"));
        assert_eq!(s.len(), 11 + 79);
        let Code::Public { network, join } = decode(&s).unwrap() else { panic!() };
        assert_eq!(network, key.public());
        assert_eq!(join, join_secret(&key));
        // tolerant to case, spaces and a missing prefix
        let messy = format!("  {} ", s.to_uppercase().replace("GANGLI-PUB-", ""));
        assert!(matches!(decode(&messy).unwrap(), Code::Public { .. }));
    }

    #[test]
    fn private_roundtrip_and_checksum() {
        let key = SecretKey::generate();
        let s = private_code(&key);
        assert!(s.starts_with("gangli-key-"));
        let Code::Private { key: back } = decode(&s).unwrap() else { panic!() };
        assert_eq!(back.public(), key.public());
        // flip one character of the body: refused
        let mut chars: Vec<char> = s.chars().collect();
        let i = 20;
        chars[i] = if chars[i] == 'a' { 'b' } else { 'a' };
        let bad: String = chars.into_iter().collect();
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn derivations_are_stable() {
        let key = SecretKey::generate();
        assert_eq!(join_secret(&key), join_secret(&key));
        assert_eq!(directory_key(&key), directory_key(&key));
        assert_ne!(join_secret(&key)[..], directory_key(&key)[..16]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("gangli-pub-hello").is_err());
        assert!(decode("").is_err());
    }
}
