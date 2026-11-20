//! Owner-only JSON files with a cross-process lock. Both the service state and the client state use it: the
//! CLI (elevated) and the running service change the same state file, so every change takes the lock, reads,
//! modifies and writes atomically (temp file + rename). Readers that only look (the service's periodic scan)
//! read without the lock: the rename makes every read see a whole file.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use data_encoding::HEXLOWER;
use iroh::SecretKey;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub fn ensure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write a file readable only by its owner, atomically.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut o = OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn load<T: DeserializeOwned + Default>(dir: &Path, name: &str) -> Result<T> {
    let p = dir.join(name);
    match fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

fn lock(dir: &Path) -> Result<File> {
    ensure_dir(dir)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .context("open lock file")?;
    f.lock().context("lock state")?;
    Ok(f)
}

/// Read-modify-write under the lock.
pub fn update<T, R>(dir: &Path, name: &str, f: impl FnOnce(&mut T) -> Result<R>) -> Result<R>
where
    T: DeserializeOwned + Serialize + Default,
{
    let _lock = lock(dir)?;
    let mut v: T = load(dir, name)?;
    let r = f(&mut v)?;
    write_private(&dir.join(name), &serde_json::to_vec_pretty(&v)?)?;
    Ok(r)
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

/// Load the key in `dir/name`, creating it on first use. Never overwrites an existing key.
pub fn secret_key(dir: &Path, name: &str) -> Result<SecretKey> {
    let _lock = lock(dir)?;
    let p = dir.join(name);
    match fs::read_to_string(&p) {
        Ok(s) => secret_from_hex(&s).with_context(|| format!("key in {}", p.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let k = SecretKey::generate();
            write_private(&p, secret_to_hex(&k).as_bytes())?;
            Ok(k)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}
