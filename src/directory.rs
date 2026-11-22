//! The network's public directory (section 5 of docs/ARCHITECTURE.md): one small pkarr record signed by the
//! network key, published on the Mainline DHT and on pkarr HTTP relays (no server of ours). It lets
//! - a console that only has the private code find the other consoles and the nodes;
//! - a new node that only has the public code find a console to knock on.
//!
//! **Only what is needed to connect** is published, nothing descriptive (no network name, no inventory): up
//! to [`MAX_CONSOLES`] console ids in clear (nodes need them to knock), the first [`MAX_NODES`] node ids
//! encrypted with a key derived from the network key (only consoles read them), and the number of pages:
//! further records, under keys derived from the network key, with 12 more encrypted node ids each. Everything
//! about a machine (name, system, disks…) is learned by a console over the authenticated session, never here.
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
/// Node ids in the main record; the others go to pages (records under keys derived from the network key).
pub const MAX_NODES: usize = 8;
const PAGE_NODES: usize = 12;
const MAX_PAGES: usize = 16;
const VERSION: u8 = 3;
const NAME: &str = "_gangli";
const TTL: u32 = 300;
const CHUNK: usize = 240;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Directory {
    /// Most recently active first.
    pub consoles: Vec<String>,
    /// Most recently seen first; only the first [`MAX_NODES`] are readable without the pages.
    pub nodes: Vec<String>,
}

fn client() -> Result<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| Client::builder().build().ok())
        .as_ref()
        .ok_or_else(|| anyhow!("cannot start the directory client"))
}

fn ids_to_bytes(ids: &[String]) -> Vec<u8> {
    let mut out = vec![];
    for id in ids.iter().filter_map(|s| s.parse::<PublicKey>().ok()) {
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

fn seal(key: &SecretKey, plain: &[u8]) -> Result<Vec<u8>> {
    let nonce: [u8; 12] = random16()[..12].try_into().unwrap();
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&crate::code::directory_key(key)));
    let sealed = cipher.encrypt(Nonce::from_slice(&nonce), plain).map_err(|_| anyhow!("cannot encrypt the directory"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(out)
}

fn open(key: &SecretKey, b: &[u8]) -> Result<Vec<u8>> {
    if b.len() < 12 {
        bail!("malformed directory");
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&crate::code::directory_key(key)));
    cipher.decrypt(Nonce::from_slice(&b[..12]), &b[12..]).map_err(|_| anyhow!("directory not readable with this key"))
}

/// The key of page `n` (1-based), derived from the network key.
fn page_key(key: &SecretKey, n: usize) -> SecretKey {
    let s = key.sign(format!("gangli-directory-page-{n}").as_bytes()).to_bytes();
    SecretKey::from_bytes(&s[..32].try_into().unwrap())
}

fn pages_of(d: &Directory) -> Vec<Vec<String>> {
    d.nodes.iter().skip(MAX_NODES).cloned().collect::<Vec<_>>().chunks(PAGE_NODES).take(MAX_PAGES).map(|c| c.to_vec()).collect()
}

/// Main record: `version | n_consoles | consoles | n_pages | nonce | encrypted nodes`.
pub fn encode(key: &SecretKey, d: &Directory) -> Result<Vec<u8>> {
    let consoles: Vec<String> = d.consoles.iter().take(MAX_CONSOLES).cloned().collect();
    let consoles = ids_to_bytes(&consoles);
    let first: Vec<String> = d.nodes.iter().take(MAX_NODES).cloned().collect();
    let mut out = vec![VERSION];
    out.push((consoles.len() / 32) as u8);
    out.extend_from_slice(&consoles);
    out.push(pages_of(d).len() as u8);
    out.extend(seal(key, &ids_to_bytes(&first))?);
    Ok(out)
}

/// The main record; also returns the number of pages.
pub fn decode_main(blob: &[u8], key: Option<&SecretKey>) -> Result<(Directory, usize)> {
    let bad = || anyhow!("malformed directory");
    if blob.first() != Some(&VERSION) {
        bail!("unknown directory version");
    }
    let mut p = 1;
    let nc = *blob.get(p).ok_or_else(bad)? as usize;
    p += 1;
    let consoles = bytes_to_ids(blob.get(p..p + nc * 32).ok_or_else(bad)?);
    p += nc * 32;
    let pages = *blob.get(p).ok_or_else(bad)? as usize;
    p += 1;
    let nodes = match key {
        Some(k) => bytes_to_ids(&open(k, blob.get(p..).ok_or_else(bad)?)?),
        None => vec![],
    };
    Ok((Directory { consoles, nodes }, pages))
}

pub fn decode(blob: &[u8], key: Option<&SecretKey>) -> Result<Directory> {
    decode_main(blob, key).map(|(d, _)| d)
}

async fn publish_blob(signer: &SecretKey, blob: &[u8]) -> Result<()> {
    let text = BASE32_NOPAD.encode(blob);
    let mut b = SignedPacket::builder();
    // TXT strings are limited to 255 bytes: split, each part prefixed with its index letter.
    for (i, chunk) in text.as_bytes().chunks(CHUNK).enumerate() {
        let part = format!("{}{}", (b'a' + i as u8) as char, std::str::from_utf8(chunk)?);
        b = b.txt(NAME.try_into().ctx("record name")?, part.as_str().try_into().ctx("record text")?, TTL);
    }
    let kp = Keypair::from_secret_key(&signer.to_bytes());
    let packet = b.sign(&kp).ctx("sign directory")?;
    tokio::time::timeout(Duration::from_secs(30), client()?.publish(&packet))
        .await
        .map_err(|_| anyhow!("directory publish timed out"))?
        .ctx("publish directory")?;
    Ok(())
}

pub(crate) async fn resolve_blob(id: &PublicKey) -> Result<Vec<u8>> {
    let pk = pkarr::PublicKey::try_from(id.as_bytes()).ctx("directory key")?;
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
    BASE32_NOPAD.decode(text.as_bytes()).map_err(|_| anyhow!("malformed directory"))
}

/// Sign and publish the main record and its pages (DHT + relays). Consoles only.
pub async fn publish(key: &SecretKey, d: &Directory) -> Result<()> {
    publish_blob(key, &encode(key, d)?).await?;
    for (i, page) in pages_of(d).iter().enumerate() {
        let mut blob = vec![VERSION];
        blob.extend(seal(key, &ids_to_bytes(page))?);
        publish_blob(&page_key(key, i + 1), &blob).await?;
    }
    Ok(())
}

/// Look the directory up (fresh from the network). `key` decrypts the node ids and reads the pages
/// (consoles); without it only the console ids are readable (nodes).
pub async fn resolve(network: &PublicKey, key: Option<&SecretKey>) -> Result<Directory> {
    let (mut d, pages) = decode_main(&resolve_blob(network).await?, key)?;
    if let Some(k) = key {
        let lookups: Vec<_> = (1..=pages.min(MAX_PAGES)).map(|n| page_key(k, n).public()).collect();
        let mut tasks = vec![];
        for id in lookups {
            tasks.push(tokio::spawn(async move { resolve_blob(&id).await }));
        }
        for t in tasks {
            if let Ok(Ok(blob)) = t.await {
                if blob.first() == Some(&VERSION) {
                    if let Ok(plain) = open(k, &blob[1..]) {
                        d.nodes.extend(bytes_to_ids(&plain));
                    }
                }
            }
        }
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_privacy() {
        let key = SecretKey::generate();
        let ids = |n: usize| (0..n).map(|_| SecretKey::generate().public().to_string()).collect::<Vec<_>>();
        let d = Directory { consoles: ids(2), nodes: ids(8) };
        let blob = encode(&key, &d).unwrap();
        // full lists fit the 1000-byte record even without DNS name compression
        let full = Directory { consoles: ids(9), nodes: ids(30) };
        let n = BASE32_NOPAD.encode(&encode(&key, &full).unwrap()).len();
        assert!(n < 640, "{n}");
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
        let d = Directory { consoles: ids(9), nodes: ids(30) };
        let back = decode(&encode(&key, &d).unwrap(), Some(&key)).unwrap();
        assert_eq!(back.consoles.len(), MAX_CONSOLES);
        assert_eq!(back.nodes.len(), MAX_NODES);
    }

    #[test]
    fn pages_hold_the_rest_and_fit() {
        let key = SecretKey::generate();
        let ids = |n: usize| (0..n).map(|_| SecretKey::generate().public().to_string()).collect::<Vec<_>>();
        let d = Directory { consoles: ids(1), nodes: ids(30) };
        let pages = pages_of(&d);
        assert_eq!(pages.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![12, 10]);
        let (main, n) = decode_main(&encode(&key, &d).unwrap(), Some(&key)).unwrap();
        assert_eq!((main.nodes.len(), n), (MAX_NODES, 2));
        // a full page stays well inside a 1000-byte record
        let mut blob = vec![VERSION];
        blob.extend(seal(&key, &ids_to_bytes(&pages[0])).unwrap());
        assert!(BASE32_NOPAD.encode(&blob).len() < 700);
        let plain = open(&key, &blob[1..]).unwrap();
        assert_eq!(bytes_to_ids(&plain), pages[0]);
        // page keys are stable and distinct
        assert_eq!(page_key(&key, 1).public(), page_key(&key, 1).public());
        assert_ne!(page_key(&key, 1).public(), page_key(&key, 2).public());
    }
}
