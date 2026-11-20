//! The two configuration files (`gangli.node`, `gangli.console`, TOML, next to the binary). A change takes an
//! exclusive lock on the file itself, reads it, applies the change and rewrites it in place, so no other file
//! (lock, temp) ever appears next to it. Readers that only look (the node's 2-second rescan) read without
//! the lock and simply retry on the next scan if they catch a half-written file.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use data_encoding::HEXLOWER;
use iroh::SecretKey;
use serde::de::DeserializeOwned;
use serde::Serialize;

fn open_rw(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path).with_context(|| format!("open {}", path.display()))
}

fn parse<T: DeserializeOwned + Default>(path: &Path, text: &str) -> Result<T> {
    if text.trim().is_empty() {
        return Ok(T::default());
    }
    toml::from_str(text).with_context(|| format!("{} is not valid (fix or delete it)", path.display()))
}

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(s) => parse(path, &s),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn render<T: Serialize>(header: &str, v: &T) -> Result<String> {
    let body = toml::to_string_pretty(v).map_err(|e| anyhow!("cannot write the configuration: {e}"))?;
    let mut out = String::new();
    for line in header.lines() {
        out.push_str("# ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

/// Read-modify-write under an exclusive lock on the file itself.
pub fn update<T, R>(path: &Path, header: &str, f: impl FnOnce(&mut T) -> Result<R>) -> Result<R>
where
    T: DeserializeOwned + Serialize + Default,
{
    let mut file = open_rw(path)?;
    file.lock().context("lock the configuration")?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut v: T = parse(path, &text)?;
    let r = f(&mut v)?;
    let out = render(header, &v)?;
    if out != text {
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
    }
    Ok(r)
}

/// Overwrite with `v` (the console, which is the only writer of its file while open).
pub fn save<T: Serialize>(path: &Path, header: &str, v: &T) -> Result<()> {
    let out = render(header, v)?;
    let mut file = open_rw(path)?;
    file.lock().context("lock the configuration")?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if out != text {
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}

pub fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn secret_from_hex(s: &str) -> Result<SecretKey> {
    let b = HEXLOWER
        .decode(s.trim().as_bytes())
        .map_err(|_| anyhow!("malformed key"))?;
    let arr: [u8; 32] = b.try_into().map_err(|_| anyhow!("malformed key length"))?;
    Ok(SecretKey::from_bytes(&arr))
}

pub fn secret_to_hex(k: &SecretKey) -> String {
    HEXLOWER.encode(&k.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
    struct T {
        #[serde(default)]
        a: String,
        #[serde(default)]
        list: Vec<Item>,
    }
    #[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
    struct Item {
        name: String,
        n: u32,
    }

    #[test]
    fn update_in_place() {
        let dir = std::env::temp_dir().join(format!("gangli-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.toml");
        update(&p, "header line", |t: &mut T| {
            t.a = "hello".into();
            t.list.push(Item { name: "one".into(), n: 1 });
            Ok(())
        })
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# header line\n"));
        let back: T = load(&p).unwrap();
        assert_eq!(back.a, "hello");
        assert_eq!(back.list.len(), 1);
        // shorter content after a change must not leave garbage behind
        update(&p, "h", |t: &mut T| {
            t.list.clear();
            Ok(())
        })
        .unwrap();
        let back: T = load(&p).unwrap();
        assert!(back.list.is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no extra files next to the configuration");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
