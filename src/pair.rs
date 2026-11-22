//! Joining a machine to a network from a phone (section 10 of docs/ARCHITECTURE.md): no code typed, no camera
//! on the machine, no port opened anywhere.
//!
//! 1. The machine shows a QR with a fresh random 32-byte seed: the URL of the gangli wallet (a static web page
//!    that keeps public network codes only, in the phone's browser) with `#pair=<seed>&name=<machine>`.
//! 2. The phone's camera opens the wallet; you tick one or more of its networks. The wallet encrypts them
//!    with AES-256-GCM under `SHA-256("gangli-pair-v1" | seed)` and publishes the result on the pkarr relays,
//!    signed by the ed25519 key whose seed is the seed (TXT `_gangli`, parts `a…`, `b…` of the base32 of
//!    `nonce | ciphertext`, as the network directory does).
//!    Plaintext: `0x02`, then per network the 49 bytes of its public code, a name length byte and the name as
//!    the phone calls it (UTF-8, at most 24 bytes); at most 6 networks (a pkarr packet holds 1000 bytes).
//! 3. The machine polls that key on the relays only while its QR is on screen and decrypts; the person at
//!    the machine confirms (GUI dialog, or `gangli pair` in a terminal) and it joins; a console of each
//!    network admits it as usual.
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

/// A network the phone sends: its public code and the name the phone knows it by (may be empty).
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub code: String,
    pub name: String,
}

impl Offer {
    /// The name to show: the phone's, else the start of the network id.
    pub fn label(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        match crate::code::decode(&self.code) {
            Ok(crate::code::Code::Public { network, .. }) => format!("network {}", &network.to_string()[..8]),
            _ => "a network".into(),
        }
    }
}

const MAX_OFFERS: usize = 6;

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

    /// What the machine's QR carries: the seed and the machine's name (for the wallet's "Add <name> to
    /// these networks").
    pub fn url(&self) -> String {
        let host = crate::util::short_host(&sysinfo::System::host_name().unwrap_or_default()).to_string();
        format!("{}#pair={}&name={}", wallet_url(), BASE32_NOPAD.encode(&self.seed).to_ascii_lowercase(), crate::ssh::url_encode(&host))
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

    /// The wallet's message: `nonce (12) | AES-256-GCM(plaintext) with its tag`; the networks in it.
    fn open(&self, blob: &[u8]) -> Result<Vec<Offer>> {
        use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
        let bad = || anyhow!("malformed pairing message");
        if blob.len() < 12 + 16 {
            bail!("malformed pairing message");
        }
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.aes_key()).map_err(|_| anyhow!("pairing key"))?);
        let nonce = Nonce::try_assume_unique_for_key(&blob[..12]).map_err(|_| bad())?;
        let mut buf = blob[12..].to_vec();
        let plain = key.open_in_place(nonce, Aad::empty(), &mut buf).map_err(|_| anyhow!("pairing message not for this machine"))?;
        let mut offers: Vec<Offer> = vec![];
        if plain.first() != Some(&2) {
            return Err(bad());
        }
        let mut p = &plain[1..];
        while !p.is_empty() {
            if p.len() < 50 || offers.len() >= MAX_OFFERS {
                return Err(bad());
            }
            let code = BASE32_NOPAD.encode(&p[..49]).to_ascii_lowercase();
            let n = p[49] as usize;
            let name = p.get(50..50 + n).ok_or_else(bad)?;
            offers.push(Offer { code, name: String::from_utf8_lossy(name).chars().filter(|c| !c.is_control()).collect() });
            p = &p[50 + n..];
        }
        if offers.is_empty() {
            return Err(bad());
        }
        // Only public codes may come this way (the wallet refuses private ones; so does the machine).
        let mut seen = vec![];
        for o in &offers {
            match crate::code::decode(&o.code)? {
                crate::code::Code::Public { network, .. } => {
                    if seen.contains(&network) {
                        bail!("the phone sent the same network twice");
                    }
                    seen.push(network);
                }
                crate::code::Code::Private { .. } => bail!("the phone sent a private code: refused (never put a private code on a machine)"),
            }
        }
        Ok(offers)
    }

    /// Wait for the phone's answer (polling the relays while the QR is shown) and return the networks in it.
    pub async fn wait(&self, timeout: Duration, cancel: Option<Arc<AtomicBool>>) -> Result<Vec<Offer>> {
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

    /// What the wallet does, for the tests: encrypt `plain` with a given nonce.
    #[cfg(test)]
    fn seal(&self, plain: &[u8], nonce: [u8; 12]) -> Vec<u8> {
        use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.aes_key()).unwrap());
        let mut buf = plain.to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf).unwrap();
        let mut out = nonce.to_vec();
        out.extend(buf);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v2 plaintext, as the wallet builds it.
    fn v2(nets: &[(&str, &str)]) -> Vec<u8> {
        let mut out = vec![2u8];
        for (code, name) in nets {
            out.extend(BASE32_NOPAD.decode(code.to_ascii_uppercase().as_bytes()).unwrap());
            out.push(name.len() as u8);
            out.extend(name.as_bytes());
        }
        out
    }

    #[test]
    fn roundtrip_refuses_private_and_strangers() {
        let net = SecretKey::generate();
        let public = crate::code::public_code(&net);
        let p = Pairing::new();
        let msg = p.seal(&v2(&[(&public, "")]), [7; 12]);
        assert_eq!(p.open(&msg).unwrap(), vec![Offer { code: public.clone(), name: String::new() }]);
        // another machine's seed cannot read it
        assert!(Pairing::new().open(&msg).is_err());
        // a bare code (not the format) is refused
        assert!(p.open(&p.seal(public.as_bytes(), [7; 12])).is_err());
        // a private code is refused even when correctly encrypted
        let mut private = vec![2u8];
        private.extend(BASE32_NOPAD.decode(crate::code::private_code(&net).to_ascii_uppercase().as_bytes()).unwrap());
        private.push(0);
        assert!(p.open(&p.seal(&private, [7; 12])).is_err());
        assert!(p.url().contains("#pair=") && p.url().contains("&name="));
    }

    #[test]
    fn several_networks() {
        let (a, b) = (crate::code::public_code(&SecretKey::generate()), crate::code::public_code(&SecretKey::generate()));
        let p = Pairing::new();
        let got = p.open(&p.seal(&v2(&[(&a, "family"), (&b, "uffìcio")]), [9; 12])).unwrap();
        assert_eq!(got, vec![Offer { code: a.clone(), name: "family".into() }, Offer { code: b.clone(), name: "uffìcio".into() }]);
        assert_eq!(got[1].label(), "uffìcio");
        assert!(Offer { code: a.clone(), name: String::new() }.label().starts_with("network "));
        // twice the same network, a truncated message: refused
        assert!(p.open(&p.seal(&v2(&[(&a, "x"), (&a, "y")]), [9; 12])).is_err());
        let mut cut = v2(&[(&a, "family")]);
        cut.truncate(30);
        assert!(p.open(&p.seal(&cut, [9; 12])).is_err());
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
        assert!(p.url().contains("#pair=aeaqcaibaeaqcaibaeaqcaibaeaqcaibaeaqcaibaeaqcaibaeaq&name="));
    }
}
