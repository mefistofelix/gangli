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
    fn constant_time_eq() {
        assert!(eq_ct("abc", "abc"));
        assert!(!eq_ct("abc", "abd"));
        assert!(!eq_ct("abc", "ab"));
    }
}
