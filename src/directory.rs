//! The network's public directory (section 5 of docs/ARCHITECTURE.md): one small pkarr record signed by the
//! network key, published on the Mainline DHT and on pkarr HTTP relays (no server of ours). It lets
//! - a console that only has the private code find the other consoles and the nodes;
//! - a new node that only has the public code find a console to knock on.
//!
//! Content: network name and up to [`MAX_CONSOLES`] console ids in clear (nodes need them), up to
//! [`MAX_NODES`] node ids encrypted with a key derived from the network key (only consoles read them).
//! The record is written only by consoles; it is read on events (start, pending admission, all known
//! consoles unreachable), never polled.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use data_encoding::BASE32_NOPAD;
use iroh::{PublicKey, SecretKey};
use pkarr::{Client, Keypair, ResolvePolicy, SignedPacket};

use crate::util::{random16, Ctx};

pub const MAX_CONSOLES: usize = 3;
/// More nodes than this are found through the other consoles (later: extra pages under derived keys).
pub const MAX_NODES: usize = 8;
const MAX_NAME: usize = 16;
const VERSION: u8 = 1;
const NAME: &str = "_gangli";
const TTL: u32 = 300;
const CHUNK: usize = 240;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Directory {
    pub name: String,
    /// Most recently active first.
    pub consoles: Vec<String>,
    /// Most recently seen first; empty when read without the network key.
    pub nodes: Vec<String>,
}

fn client() -> Result<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| Client::builder().build().ok())
        .as_ref()
        .ok_or_else(|| anyhow!("cannot start the directory client"))
}

fn ids_to_bytes(ids: &[String], max: usize) -> Vec<u8> {
    let mut out = vec![];
    for id in ids.iter().filter_map(|s| s.parse::<PublicKey>().ok()).take(max) {
        out.extend_from_slice(id.as_bytes());
    }
    out
}

fn bytes_to_ids(b: &[u8]) -> Vec<String> {
    b.chunks_exact(32)
        .filter_map(|c| <[u8; 32]>::try_from(c).ok())
        .filter_map(|a| PublicKey::from_bytes(&a).ok())
        .map(|k| k.to_string())
        .collect()
}

/// `version | name_len | name | n_consoles | consoles | nonce(12) | encrypted nodes`
pub fn encode(key: &SecretKey, d: &Directory) -> Result<Vec<u8>> {
    let name: String = d.name.chars().take(MAX_NAME).collect();
    let consoles = ids_to_bytes(&d.consoles, MAX_CONSOLES);
    let nodes = ids_to_bytes(&d.nodes, MAX_NODES);
    let mut out = vec![VERSION, name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out.push((consoles.len() / 32) as u8);
    out.extend_from_slice(&consoles);
    let nonce: [u8; 12] = random16()[..12].try_into().unwrap();
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&crate::code::directory_key(key)));
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), nodes.as_slice())
        .map_err(|_| anyhow!("cannot encrypt the directory"))?;
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

pub fn decode(blob: &[u8], key: Option<&SecretKey>) -> Result<Directory> {
    let bad = || anyhow!("malformed directory");
    if blob.first() != Some(&VERSION) {
        bail!("unknown directory version");
    }
    let name_len = *blob.get(1).ok_or_else(bad)? as usize;
    let name = std::str::from_utf8(blob.get(2..2 + name_len).ok_or_else(bad)?).map_err(|_| bad())?.to_string();
    let mut p = 2 + name_len;
    let nc = *blob.get(p).ok_or_else(bad)? as usize;
    p += 1;
    let consoles = bytes_to_ids(blob.get(p..p + nc * 32).ok_or_else(bad)?);
    p += nc * 32;
    let mut nodes = vec![];
    if let Some(key) = key {
        let nonce = blob.get(p..p + 12).ok_or_else(bad)?;
        let sealed = blob.get(p + 12..).ok_or_else(bad)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&crate::code::directory_key(key)));
        let plain = cipher.decrypt(Nonce::from_slice(nonce), sealed).map_err(|_| anyhow!("directory not readable with this key"))?;
        nodes = bytes_to_ids(&plain);
    }
    Ok(Directory { name, consoles, nodes })
}

/// Sign and publish (DHT + relays). Consoles only.
pub async fn publish(key: &SecretKey, d: &Directory) -> Result<()> {
    let text = BASE32_NOPAD.encode(&encode(key, d)?);
    let mut b = SignedPacket::builder();
    // TXT strings are limited to 255 bytes: split, each part prefixed with its index letter.
    for (i, chunk) in text.as_bytes().chunks(CHUNK).enumerate() {
        let part = format!("{}{}", (b'a' + i as u8) as char, std::str::from_utf8(chunk)?);
        b = b.txt(NAME.try_into().ctx("record name")?, part.as_str().try_into().ctx("record text")?, TTL);
    }
    let kp = Keypair::from_secret_key(&key.to_bytes());
    let packet = b.sign(&kp).ctx("sign directory")?;
    tokio::time::timeout(Duration::from_secs(30), client()?.publish(&packet))
        .await
        .map_err(|_| anyhow!("directory publish timed out"))?
        .ctx("publish directory")?;
    Ok(())
}

/// Look the directory up (fresh from the network). `key` decrypts the node ids (consoles).
pub async fn resolve(network: &PublicKey, key: Option<&SecretKey>) -> Result<Directory> {
    let pk = pkarr::PublicKey::try_from(network.as_bytes()).ctx("directory key")?;
    let packet = tokio::time::timeout(Duration::from_secs(20), client()?.resolve(&pk, ResolvePolicy::NetworkOnly))
        .await
        .map_err(|_| anyhow!("directory lookup timed out"))?
        .ctx("directory lookup")?;
    let mut parts: Vec<String> = vec![];
    for rr in packet.fresh_resource_records(NAME) {
        if let pkarr::dns::rdata::RData::TXT(t) = &rr.rdata {
            if let Ok(s) = String::try_from(t.clone()) {
                parts.push(s);
            }
        }
    }
    if parts.is_empty() {
        bail!("no directory published for this network yet");
    }
    parts.sort();
    let text: String = parts.iter().map(|p| &p[1..]).collect();
    let blob = BASE32_NOPAD.decode(text.as_bytes()).map_err(|_| anyhow!("malformed directory"))?;
    decode(&blob, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_privacy() {
        let key = SecretKey::generate();
        let ids = |n: usize| (0..n).map(|_| SecretKey::generate().public().to_string()).collect::<Vec<_>>();
        let d = Directory { name: "home".into(), consoles: ids(2), nodes: ids(8) };
        let blob = encode(&key, &d).unwrap();
        // full lists fit the 1000-byte record even without DNS name compression
        let full = Directory { name: "x".repeat(40), consoles: ids(9), nodes: ids(30) };
        let n = BASE32_NOPAD.encode(&encode(&key, &full).unwrap()).len();
        assert!(n < 660, "{n}");
        assert_eq!(decode(&blob, Some(&key)).unwrap(), d);
        // without the key: consoles yes, nodes no
        let public = decode(&blob, None).unwrap();
        assert_eq!(public.consoles, d.consoles);
        assert!(public.nodes.is_empty());
        // another key cannot read the nodes
        assert!(decode(&blob, Some(&SecretKey::generate())).is_err());
    }

    #[test]
    fn caps_lists() {
        let key = SecretKey::generate();
        let ids = |n: usize| (0..n).map(|_| SecretKey::generate().public().to_string()).collect::<Vec<_>>();
        let d = Directory { name: "a-very-long-network-name-that-is-cut".into(), consoles: ids(9), nodes: ids(30) };
        let back = decode(&encode(&key, &d).unwrap(), Some(&key)).unwrap();
        assert_eq!(back.consoles.len(), MAX_CONSOLES);
        assert_eq!(back.nodes.len(), MAX_NODES);
        assert_eq!(back.name.len(), MAX_NAME);
    }
}
