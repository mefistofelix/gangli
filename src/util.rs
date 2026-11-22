//! Small helpers shared by every module.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turn any `Result<T, E: Display>` into an anyhow result with a short prefix. Used for iroh calls, whose
/// error types are only guaranteed to be `Display`.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> anyhow::Result<T>;
}

impl<T, E: std::fmt::Display> Ctx<T> for Result<T, E> {
    fn ctx(self, what: &str) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{what}: {e}"))
    }
}

/// Compare two secrets without an early exit on the first differing byte.
pub fn eq_ct(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 16 random bytes from the OS CSPRNG (through iroh's key generator, which uses it).
pub fn random16() -> [u8; 16] {
    let k = iroh::SecretKey::generate().to_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&k[..16]);
    out
}

/// Hostname → simple node name: lowercase letters, digits and dashes.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "node".into()
    } else {
        out
    }
}

/// A host name without its domain ("Mac-mini.local" → "Mac-mini"): the name a machine gets.
pub fn short_host(h: &str) -> &str {
    h.split('.').next().filter(|s| !s.is_empty()).unwrap_or(h)
}

/// `base`, or `base-2`, `base-3`... whichever is not in `taken`.
pub fn unique_name<'a>(base: &str, taken: impl Iterator<Item = &'a str> + Clone) -> String {
    let base = slug(base);
    if !taken.clone().any(|t| t == base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !taken.clone().any(|t| t == n))
        .unwrap()
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

pub fn human_ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Michele-PC.local"), "michele-pc-local");
        assert_eq!(slug("  "), "node");
        assert_eq!(unique_name("nas", ["nas", "nas-2"].into_iter()), "nas-3");
        assert_eq!(unique_name("NAS", ["pc"].into_iter()), "nas");
    }

    #[test]
    fn relay_lists() {
        let l = relay_list(&["https://relay.example.org".into(), " https://relay.example.org/ ".into(), "".into()]).unwrap();
        assert_eq!(l, vec!["https://relay.example.org/".to_string()]);
        assert!(relay_url("not a url").is_err());
        assert!(relay_url("ftp://relay.example.org").is_err());
    }

    #[test]
    fn constant_time_eq() {
        assert!(eq_ct("abc", "abc"));
        assert!(!eq_ct("abc", "abd"));
        assert!(!eq_ct("abc", "ab"));
    }
}

/// A relay address as gangli keeps it (iroh's normal form), or why it is not one.
pub fn relay_url(u: &str) -> anyhow::Result<String> {
    let url: iroh::RelayUrl = u.trim().parse().map_err(|e| anyhow::anyhow!("relay {u}: {e}"))?;
    let s = url.to_string();
    if !s.starts_with("https://") && !s.starts_with("http://") {
        anyhow::bail!("relay {u}: an https:// address, e.g. https://relay.example.org");
    }
    Ok(s)
}

/// Normal form of a relay list: valid, no duplicates, order kept.
pub fn relay_list(urls: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = vec![];
    for u in urls.iter().filter(|u| !u.trim().is_empty()) {
        let u = relay_url(u)?;
        if !out.contains(&u) {
            out.push(u);
        }
    }
    Ok(out)
}

/// Bring an endpoint's relays pushed by the networks to `want`, at runtime: they come on top of the relays it
/// started with (the n0 ones and the local configuration), which are never removed. `applied`: what earlier
/// calls added.
pub async fn apply_relays(ep: &iroh::Endpoint, want: &[String], applied: &mut Vec<String>) {
    for u in want {
        if applied.contains(u) {
            continue;
        }
        let Ok(url) = u.parse::<iroh::RelayUrl>() else { continue };
        // A relay the endpoint already had (it started with it) comes back as the previous configuration:
        // put it back as it was and leave it out of `applied`, so it is never removed.
        if let Some(prev) = ep.insert_relay(url.clone(), std::sync::Arc::new(iroh::RelayConfig::from(url.clone()))).await {
            ep.insert_relay(url, prev).await;
            continue;
        }
        applied.push(u.clone());
    }
    for u in applied.clone() {
        if !want.contains(&u) {
            if let Ok(url) = u.parse::<iroh::RelayUrl>() {
                ep.remove_relay(&url).await;
            }
            applied.retain(|x| *x != u);
        }
    }
}

/// How this endpoint reaches the Internet now (iroh's net report through the relays).
#[allow(unused_mut)]
pub fn net_info(ep: &iroh::Endpoint) -> crate::inventory::NetInfo {
    use iroh::Watcher;
    let port = ep.bound_sockets().iter().find(|a| a.is_ipv4()).map(|a| a.port()).unwrap_or(0);
    let mut w = ep.net_report();
    match w.get() {
        Some(r) => crate::inventory::NetInfo {
            udp_v4: r.udp_v4,
            udp_v6: r.udp_v6,
            public_v4: r.global_v4.map(|a| a.to_string()).unwrap_or_default(),
            public_v6: r.global_v6.map(|a| a.to_string()).unwrap_or_default(),
            nat_varies: r.mapping_varies_by_dest_ipv4,
            port,
        },
        None => crate::inventory::NetInfo { port, ..Default::default() },
    }
}

/// Bind an endpoint, on the IPv4 UDP port `port` when one is set (a port the router forwards); when that
/// port cannot be had, on a random one as usual.
pub async fn bind_endpoint(builder: impl Fn() -> iroh::endpoint::Builder, port: Option<u16>) -> anyhow::Result<iroh::Endpoint> {
    if let Some(p) = port.filter(|p| *p > 0) {
        match builder().bind_addr(std::net::SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, p)) {
            Ok(b) => match b.bind().await {
                Ok(ep) => return Ok(ep),
                Err(e) => crate::log_to_file(&format!("UDP port {p} not available ({e}): a random one")),
            },
            Err(e) => crate::log_to_file(&format!("UDP port {p}: {e}")),
        }
    }
    builder().bind().await.map_err(|e| anyhow::anyhow!("start endpoint: {e}"))
}

/// The relay this endpoint is reachable at now (its home relay), empty if none yet.
#[allow(unused_mut)]
pub fn home_relay(ep: &iroh::Endpoint) -> String {
    use iroh::Watcher;
    let mut w = ep.home_relay_status();
    w.get().first().map(|s| s.url().to_string()).unwrap_or_default()
}

/// Relays from the configuration (section 4 of docs/ARCHITECTURE.md, docs/RELAY.md): `None` keeps the
/// n0 relays; with URLs, ours first and, if `n0_fallback`, the n0 ones as well.
pub fn relay_mode(urls: &[String], n0_fallback: bool) -> anyhow::Result<Option<iroh::endpoint::RelayMode>> {
    use iroh::endpoint::RelayMode;
    if urls.is_empty() {
        return Ok(None);
    }
    let mut ours = vec![];
    for u in urls {
        let url: iroh::RelayUrl = u.trim().parse().map_err(|e| anyhow::anyhow!("relay {u}: {e}"))?;
        ours.push(url);
    }
    if !n0_fallback {
        return Ok(Some(RelayMode::custom(ours)));
    }
    let map = RelayMode::Default.relay_map();
    for url in ours {
        map.insert(url.clone(), std::sync::Arc::new(iroh::RelayConfig::from(url)));
    }
    Ok(Some(RelayMode::Custom(map)))
}
