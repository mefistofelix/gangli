//! Joining a machine to a network from a phone (section 10 of docs/ARCHITECTURE.md): no code typed, no camera
//! on the machine, no port opened anywhere.
//!
//! 1. The machine shows a QR with a fresh random 32-byte seed: the URL of the gangli wallet (a static web page
//!    that keeps public network codes only, in the phone's browser) with `#pair=<seed>`.
//! 2. The phone's camera opens the wallet; you pick one of its networks. The wallet encrypts that network's
//!    public code with AES-256-GCM under `SHA-256("gangli-pair-v1" | seed)` and publishes it on the pkarr
//!    relays, signed by the ed25519 key whose seed is the seed (TXT `_gangli`, `a` + base32 of
//!    `nonce | ciphertext`, as the network directory does).
//! 3. The machine polls that key on the relays only while its QR is on screen, decrypts and joins; a console
//!    of the network admits it as usual.
//!
//! The relays see a random key and ciphertext. Only who saw the machine's screen holds the seed, so nobody
//! else can send it a network. Only a public code ever travels: the wallet refuses private ones, and so does
//! the machine. The wallet (`web/index.html`) implements the same derivations: keep the two in step.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use data_encoding::BASE32_NOPAD;
use iroh::SecretKey;

/// Where the wallet is published (GitHub Pages of this repository); `GANGLI_WALLET_URL` overrides it.
pub const WALLET_URL: &str = "https://mefistofelix.github.io/gangli/";

const LABEL: &[u8] = b"gangli-pair-v1";
/// How often the machine looks for the phone's answer while its QR is shown.
const POLL: Duration = Duration::from_secs(3);

pub fn wallet_url() -> String {
    std::env::var("GANGLI_WALLET_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| WALLET_URL.to_string())
}

/// What a console's network QR carries: the wallet opens and offers to keep this public code.
pub fn network_url(public_code: &str, name: &str) -> String {
    format!("{}#net={}&name={}", wallet_url(), public_code, crate::ssh::url_encode(name))
}

pub struct Pairing {
    seed: [u8; 32],
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

impl Pairing {
    pub fn new() -> Pairing {
        let mut seed = [0u8; 32];
        seed[..16].copy_from_slice(&crate::util::random16());
        seed[16..].copy_from_slice(&crate::util::random16());
        Pairing { seed }
    }

    /// What the machine's QR carries.
    pub fn url(&self) -> String {
        format!("{}#pair={}", wallet_url(), BASE32_NOPAD.encode(&self.seed).to_ascii_lowercase())
    }

    /// The key the wallet publishes under.
    fn key(&self) -> SecretKey {
        SecretKey::from_bytes(&self.seed)
    }

    fn aes_key(&self) -> [u8; 32] {
        let mut data = LABEL.to_vec();
        data.extend_from_slice(&self.seed);
        ring::digest::digest(&ring::digest::SHA256, &data).as_ref().try_into().expect("32 bytes")
    }

    /// The wallet's message: `nonce (12) | AES-256-GCM(public code) with its tag`.
    fn open(&self, blob: &[u8]) -> Result<String> {
        use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
        if blob.len() < 12 + 16 {
            bail!("malformed pairing message");
        }
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.aes_key()).map_err(|_| anyhow!("pairing key"))?);
        let nonce = Nonce::try_assume_unique_for_key(&blob[..12]).map_err(|_| anyhow!("malformed pairing message"))?;
        let mut buf = blob[12..].to_vec();
        let plain = key.open_in_place(nonce, Aad::empty(), &mut buf).map_err(|_| anyhow!("pairing message not for this machine"))?;
        let code = String::from_utf8(plain.to_vec()).map_err(|_| anyhow!("malformed pairing message"))?;
        // Only a public code may come this way (the wallet refuses private ones; so does the machine).
        match crate::code::decode(&code)? {
            crate::code::Code::Public { .. } => Ok(code),
            crate::code::Code::Private { .. } => bail!("the phone sent a private code: refused (never put a private code on a machine)"),
        }
    }

    /// Wait for the phone's answer (polling the relays while the QR is shown) and return the public code.
    pub async fn wait(&self, timeout: Duration, cancel: Option<Arc<AtomicBool>>) -> Result<String> {
        let pk = self.key().public();
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                bail!("pairing cancelled");
            }
            if let Ok(blob) = crate::directory::resolve_blob(&pk).await {
                match self.open(&blob) {
                    Ok(code) => return Ok(code),
                    Err(e) => crate::log_line(&format!("pairing: {e:#}")),
                }
            }
            if Instant::now() >= deadline {
                bail!("no answer from the phone in time: show the QR again (`gangli pair`)");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    #[cfg(test)]
    fn from_seed(seed: [u8; 32]) -> Pairing {
        Pairing { seed }
    }

    /// What the wallet does, for the tests: encrypt `code` with a given nonce.
    #[cfg(test)]
    fn seal(&self, code: &str, nonce: [u8; 12]) -> Vec<u8> {
        use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.aes_key()).unwrap());
        let mut buf = code.as_bytes().to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf).unwrap();
        let mut out = nonce.to_vec();
        out.extend(buf);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_refuses_private_and_strangers() {
        let net = SecretKey::generate();
        let public = crate::code::public_code(&net);
        let p = Pairing::new();
        assert_eq!(p.open(&p.seal(&public, [7; 12])).unwrap(), public);
        // another machine's seed cannot read it
        assert!(Pairing::new().open(&p.seal(&public, [7; 12])).is_err());
        // a private code is refused even when correctly encrypted
        assert!(p.open(&p.seal(&crate::code::private_code(&net), [7; 12])).is_err());
        assert!(p.url().contains("#pair="));
    }

    /// Fixed vectors, computed independently (Python `cryptography` + hashlib) and checked by the wallet too
    /// (web/index.html, WebCrypto): seed = 32 bytes of 0x01.
    #[test]
    fn vectors_for_the_wallet() {
        let p = Pairing::from_seed([1; 32]);
        assert_eq!(data_encoding::HEXLOWER.encode(&p.aes_key()), "1eb0004489261f88d0c4b6f7b7f8c7a49ac7b4982c988ef45090f9903c96a72b");
        assert_eq!(data_encoding::HEXLOWER.encode(p.key().public().as_bytes()), "8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c");
        // the name the wallet publishes under (pkarr's z-base32 of that key)
        assert_eq!(pkarr::Keypair::from_secret_key(&[1; 32]).public_key().to_string(), "tkrq8zmwb8a3m9k15csu3q17qmfgqnp9dskbrg9uq1rydpyxp7qy");
        assert!(p.url().ends_with("#pair=aeaqcaibaeaqcaibaeaqcaibaeaqcaibaeaqcaibaeaqcaibaeaq"));
    }
}
