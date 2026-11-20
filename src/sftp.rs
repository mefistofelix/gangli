//! A small SFTP v3 server (draft-ietf-secsh-filexfer-02) on stdin / stdout, for WinSCP, `sftp`, `scp -s`.
//! The node starts it as `gangli sftp-server` in a process of the session user, so file access is exactly
//! that user's. Paths are `/`-separated; on Windows `/C:/Users/x` is `C:\Users\x` and `/` lists the drives.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// Marker command a console sends for the `sftp` subsystem; the node runs this binary instead of a shell.
pub const COMMAND: &str = "\u{1}gangli-sftp-server";

const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const LSTAT: u8 = 7;
const FSTAT: u8 = 8;
const SETSTAT: u8 = 9;
const FSETSTAT: u8 = 10;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const READLINK: u8 = 19;
const SYMLINK: u8 = 20;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;

const OK: u32 = 0;
const EOF: u32 = 1;
const NO_SUCH_FILE: u32 = 2;
const PERMISSION_DENIED: u32 = 3;
const FAILURE: u32 = 4;
const BAD_MESSAGE: u32 = 5;
const OP_UNSUPPORTED: u32 = 8;

const A_SIZE: u32 = 0x1;
const A_UIDGID: u32 = 0x2;
const A_PERMISSIONS: u32 = 0x4;
const A_ACMODTIME: u32 = 0x8;
const A_EXTENDED: u32 = 0x8000_0000;

const F_READ: u32 = 0x1;
const F_WRITE: u32 = 0x2;
const F_APPEND: u32 = 0x4;
const F_CREAT: u32 = 0x8;
const F_TRUNC: u32 = 0x10;
const F_EXCL: u32 = 0x20;

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_be_bytes(self.b.get(self.p..self.p + 4)?.try_into().ok()?);
        self.p += 4;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        let v = u64::from_be_bytes(self.b.get(self.p..self.p + 8)?.try_into().ok()?);
        self.p += 8;
        Some(v)
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        let v = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(v)
    }
    fn string(&mut self) -> Option<String> {
        Some(String::from_utf8_lossy(self.bytes()?).to_string())
    }
    /// Attributes; returns (permissions, atime/mtime) when present.
    fn attrs(&mut self) -> Option<(Option<u32>, Option<(u32, u32)>, Option<u64>)> {
        let flags = self.u32()?;
        let mut size = None;
        let mut perm = None;
        let mut times = None;
        if flags & A_SIZE != 0 {
            size = Some(self.u64()?);
        }
        if flags & A_UIDGID != 0 {
            self.u32()?;
            self.u32()?;
        }
        if flags & A_PERMISSIONS != 0 {
            perm = Some(self.u32()?);
        }
        if flags & A_ACMODTIME != 0 {
            times = Some((self.u32()?, self.u32()?));
        }
        if flags & A_EXTENDED != 0 {
            let n = self.u32()?;
            for _ in 0..n {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Some((perm, times, size))
    }
}

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
}

struct Stat {
    size: u64,
    perm: u32,
    mtime: u32,
    atime: u32,
    dir: bool,
}

fn to_secs(t: std::io::Result<std::time::SystemTime>) -> u32 {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as u32).unwrap_or(0)
}

fn stat_of(m: &std::fs::Metadata) -> Stat {
    #[cfg(unix)]
    let perm = {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    };
    #[cfg(not(unix))]
    let perm = if m.is_dir() {
        0o40755
    } else if m.permissions().readonly() {
        0o100444
    } else {
        0o100644
    };
    Stat { size: m.len(), perm, mtime: to_secs(m.modified()), atime: to_secs(m.accessed()), dir: m.is_dir() }
}

fn put_attrs(o: &mut Out, s: &Stat) {
    o.u32(A_SIZE | A_PERMISSIONS | A_ACMODTIME).u64(s.size).u32(s.perm).u32(s.atime).u32(s.mtime);
}

fn longname(name: &str, s: &Stat) -> String {
    let kind = if s.dir { 'd' } else { '-' };
    let bits = ['r', 'w', 'x'];
    let mut mode = String::new();
    for i in (0..9).rev() {
        mode.push(if s.perm & (1 << i) != 0 { bits[(8 - i) % 3] } else { '-' });
    }
    format!("{kind}{mode} 1 owner group {:>10} Jan  1 00:00 {name}", s.size)
}

/// SFTP path → local path.
fn local(p: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let t = p.trim_start_matches('/');
        if t.len() >= 2 && t.as_bytes()[1] == b':' {
            let mut s = t.replace('/', "\\");
            if s.len() == 2 {
                s.push('\\');
            }
            return PathBuf::from(s);
        }
        PathBuf::from(t.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(if p.is_empty() { "." } else { p })
    }
}

/// Local absolute path → SFTP path.
fn remote(p: &Path) -> String {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s.trim_start_matches("//?/").to_string();
        format!("/{}", s.trim_end_matches('/'))
    }
    #[cfg(not(windows))]
    {
        p.to_string_lossy().to_string()
    }
}

fn is_drive_root(p: &str) -> bool {
    cfg!(windows) && (p.is_empty() || p == "/")
}

fn status_code(e: &std::io::Error) -> u32 {
    match e.kind() {
        std::io::ErrorKind::NotFound => NO_SUCH_FILE,
        std::io::ErrorKind::PermissionDenied => PERMISSION_DENIED,
        _ => FAILURE,
    }
}

enum Handle {
    File(File),
    Dir(Vec<(String, Stat)>, usize),
}

/// Serve SFTP on stdin / stdout until the client closes.
pub fn serve() -> std::io::Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut handles: HashMap<String, Handle> = HashMap::new();
    let mut next = 0u64;
    loop {
        let mut len = [0u8; 4];
        if input.read_exact(&mut len).is_err() {
            return Ok(());
        }
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > 1 << 20 {
            return Ok(());
        }
        let mut pkt = vec![0u8; len];
        input.read_exact(&mut pkt)?;
        let mut r = Reader { b: &pkt, p: 0 };
        let kind = r.u8().unwrap_or(0);
        let mut o = Out::default();
        if kind == INIT {
            o.u8(VERSION).u32(3);
        } else {
            let id = r.u32().unwrap_or(0);
            handle_one(kind, id, &mut r, &mut o, &mut handles, &mut next);
        }
        output.write_all(&(o.0.len() as u32).to_be_bytes())?;
        output.write_all(&o.0)?;
        output.flush()?;
    }
}

fn status(o: &mut Out, id: u32, code: u32, msg: &str) {
    o.u8(STATUS).u32(id).u32(code).str(msg).str("en");
}

fn io_status(o: &mut Out, id: u32, r: std::io::Result<()>) {
    match r {
        Ok(()) => status(o, id, OK, "ok"),
        Err(e) => status(o, id, status_code(&e), &e.to_string()),
    }
}

fn set_attrs(path: &Path, perm: Option<u32>, times: Option<(u32, u32)>, size: Option<u64>) -> std::io::Result<()> {
    if let Some(sz) = size {
        OpenOptions::new().write(true).open(path)?.set_len(sz)?;
    }
    #[cfg(unix)]
    if let Some(p) = perm {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(p & 0o7777))?;
    }
    #[cfg(not(unix))]
    let _ = perm;
    if let Some((_, mtime)) = times {
        let f = OpenOptions::new().write(true).open(path).or_else(|_| File::open(path))?;
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime as u64));
    }
    Ok(())
}

fn new_handle(h: Handle, handles: &mut HashMap<String, Handle>, o: &mut Out, id: u32, next: &mut u64) {
    *next += 1;
    let name = format!("h{next}");
    handles.insert(name.clone(), h);
    o.u8(HANDLE).u32(id).str(&name);
}

fn handle_one(kind: u8, id: u32, r: &mut Reader, o: &mut Out, handles: &mut HashMap<String, Handle>, next: &mut u64) {
    match kind {
        OPEN => {
            let (Some(path), Some(pflags)) = (r.string(), r.u32()) else { return status(o, id, BAD_MESSAGE, "bad open") };
            let mut oo = OpenOptions::new();
            oo.read(pflags & F_READ != 0)
                .write(pflags & F_WRITE != 0 || pflags & F_APPEND != 0)
                .append(pflags & F_APPEND != 0)
                .truncate(pflags & F_TRUNC != 0);
            if pflags & F_EXCL != 0 {
                oo.create_new(true);
            } else if pflags & F_CREAT != 0 {
                oo.create(true);
            }
            match oo.open(local(&path)) {
                Ok(f) => new_handle(Handle::File(f), handles, o, id, next),
                Err(e) => status(o, id, status_code(&e), &e.to_string()),
            }
        }
        CLOSE => {
            let h = r.string().unwrap_or_default();
            if handles.remove(&h).is_some() {
                status(o, id, OK, "ok")
            } else {
                status(o, id, FAILURE, "no such handle")
            }
        }
        READ => {
            let (Some(h), Some(off), Some(len)) = (r.string(), r.u64(), r.u32()) else { return status(o, id, BAD_MESSAGE, "bad read") };
            let Some(Handle::File(f)) = handles.get_mut(&h) else { return status(o, id, FAILURE, "no such handle") };
            let mut buf = vec![0u8; (len as usize).min(256 * 1024)];
            let res = f.seek(SeekFrom::Start(off)).and_then(|_| f.read(&mut buf));
            match res {
                Ok(0) => status(o, id, EOF, "eof"),
                Ok(n) => {
                    o.u8(DATA).u32(id).bytes(&buf[..n]);
                }
                Err(e) => status(o, id, status_code(&e), &e.to_string()),
            }
        }
        WRITE => {
            let (Some(h), Some(off), Some(data)) = (r.string(), r.u64(), r.bytes()) else { return status(o, id, BAD_MESSAGE, "bad write") };
            let Some(Handle::File(f)) = handles.get_mut(&h) else { return status(o, id, FAILURE, "no such handle") };
            io_status(o, id, f.seek(SeekFrom::Start(off)).and_then(|_| f.write_all(data)));
        }
        LSTAT | STAT => {
            let path = r.string().unwrap_or_default();
            if is_drive_root(&path) {
                o.u8(ATTRS).u32(id);
                put_attrs(o, &Stat { size: 0, perm: 0o40755, mtime: 0, atime: 0, dir: true });
                return;
            }
            let m = if kind == LSTAT { std::fs::symlink_metadata(local(&path)) } else { std::fs::metadata(local(&path)) };
            match m {
                Ok(m) => {
                    o.u8(ATTRS).u32(id);
                    put_attrs(o, &stat_of(&m));
                }
                Err(e) => status(o, id, status_code(&e), &e.to_string()),
            }
        }
        FSTAT => {
            let h = r.string().unwrap_or_default();
            match handles.get(&h) {
                Some(Handle::File(f)) => match f.metadata() {
                    Ok(m) => {
                        o.u8(ATTRS).u32(id);
                        put_attrs(o, &stat_of(&m));
                    }
                    Err(e) => status(o, id, status_code(&e), &e.to_string()),
                },
                _ => status(o, id, FAILURE, "no such handle"),
            }
        }
        SETSTAT => {
            let path = r.string().unwrap_or_default();
            let Some((perm, times, size)) = r.attrs() else { return status(o, id, BAD_MESSAGE, "bad attrs") };
            io_status(o, id, set_attrs(&local(&path), perm, times, size));
        }
        FSETSTAT => {
            let h = r.string().unwrap_or_default();
            let Some((_, times, size)) = r.attrs() else { return status(o, id, BAD_MESSAGE, "bad attrs") };
            let Some(Handle::File(f)) = handles.get_mut(&h) else { return status(o, id, FAILURE, "no such handle") };
            let mut res = Ok(());
            if let Some(sz) = size {
                res = f.set_len(sz);
            }
            if let Some((_, mtime)) = times {
                let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime as u64));
            }
            io_status(o, id, res);
        }
        OPENDIR => {
            let path = r.string().unwrap_or_default();
            let mut entries = vec![];
            if is_drive_root(&path) {
                for d in b'A'..=b'Z' {
                    let root = format!("{}:\\", d as char);
                    if Path::new(&root).exists() {
                        entries.push((format!("{}:", d as char), Stat { size: 0, perm: 0o40755, mtime: 0, atime: 0, dir: true }));
                    }
                }
            } else {
                match std::fs::read_dir(local(&path)) {
                    Ok(rd) => {
                        for e in rd.flatten() {
                            if let Ok(m) = e.metadata() {
                                entries.push((e.file_name().to_string_lossy().to_string(), stat_of(&m)));
                            }
                        }
                    }
                    Err(e) => return status(o, id, status_code(&e), &e.to_string()),
                }
                for dot in [".", ".."] {
                    entries.push((dot.to_string(), Stat { size: 0, perm: 0o40755, mtime: 0, atime: 0, dir: true }));
                }
            }
            new_handle(Handle::Dir(entries, 0), handles, o, id, next);
        }
        READDIR => {
            let h = r.string().unwrap_or_default();
            let Some(Handle::Dir(entries, pos)) = handles.get_mut(&h) else { return status(o, id, FAILURE, "no such handle") };
            if *pos >= entries.len() {
                return status(o, id, EOF, "eof");
            }
            let chunk: Vec<&(String, Stat)> = entries[*pos..].iter().take(100).collect();
            o.u8(NAME).u32(id).u32(chunk.len() as u32);
            for (name, st) in &chunk {
                o.str(name).str(&longname(name, st));
                put_attrs(o, st);
            }
            *pos += chunk.len();
        }
        REMOVE => {
            let p = r.string().unwrap_or_default();
            io_status(o, id, std::fs::remove_file(local(&p)));
        }
        MKDIR => {
            let p = r.string().unwrap_or_default();
            io_status(o, id, std::fs::create_dir(local(&p)));
        }
        RMDIR => {
            let p = r.string().unwrap_or_default();
            io_status(o, id, std::fs::remove_dir(local(&p)));
        }
        REALPATH => {
            let p = r.string().unwrap_or_default();
            let resolved = if p.is_empty() || p == "." {
                let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"));
                remote(&home)
            } else if is_drive_root(&p) {
                "/".to_string()
            } else {
                match std::fs::canonicalize(local(&p)) {
                    Ok(c) => remote(&c),
                    Err(_) => {
                        // Not existing yet (a file about to be uploaded): normalize lexically.
                        let mut parts: Vec<&str> = vec![];
                        for seg in p.split('/') {
                            match seg {
                                "" | "." => {}
                                ".." => {
                                    parts.pop();
                                }
                                s => parts.push(s),
                            }
                        }
                        format!("/{}", parts.join("/"))
                    }
                }
            };
            o.u8(NAME).u32(id).u32(1).str(&resolved).str(&resolved);
            put_attrs(o, &Stat { size: 0, perm: 0o40755, mtime: 0, atime: 0, dir: true });
        }
        RENAME => {
            let (from, to) = (r.string().unwrap_or_default(), r.string().unwrap_or_default());
            io_status(o, id, std::fs::rename(local(&from), local(&to)));
        }
        READLINK => {
            let p = r.string().unwrap_or_default();
            match std::fs::read_link(local(&p)) {
                Ok(t) => {
                    let s = t.to_string_lossy().to_string();
                    o.u8(NAME).u32(id).u32(1).str(&s).str(&s);
                    put_attrs(o, &Stat { size: 0, perm: 0o120777, mtime: 0, atime: 0, dir: false });
                }
                Err(e) => status(o, id, status_code(&e), &e.to_string()),
            }
        }
        SYMLINK => {
            let (link, target) = (r.string().unwrap_or_default(), r.string().unwrap_or_default());
            #[cfg(unix)]
            io_status(o, id, std::os::unix::fs::symlink(local(&target), local(&link)));
            #[cfg(not(unix))]
            {
                let _ = (link, target);
                status(o, id, OP_UNSUPPORTED, "symlinks are not supported here");
            }
        }
        _ => status(o, id, OP_UNSUPPORTED, "unsupported"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(kind: u8, id: u32, f: impl FnOnce(&mut Out)) -> Vec<u8> {
        let mut o = Out::default();
        o.u8(kind).u32(id);
        f(&mut o);
        o.0
    }

    fn run(p: &[u8], handles: &mut HashMap<String, Handle>, next: &mut u64) -> Vec<u8> {
        let mut r = Reader { b: p, p: 0 };
        let kind = r.u8().unwrap();
        let id = r.u32().unwrap();
        let mut o = Out::default();
        handle_one(kind, id, &mut r, &mut o, handles, next);
        o.0
    }

    #[test]
    fn write_then_read_back() {
        let dir = std::env::temp_dir().join(format!("gangli-sftp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = remote(&dir.join("a.txt"));
        let (mut h, mut n) = (HashMap::new(), 0);
        let open = run(&pkt(OPEN, 1, |o| { o.str(&file).u32(F_WRITE | F_CREAT | F_TRUNC).u32(0); }), &mut h, &mut n);
        assert_eq!(open[0], HANDLE);
        let handle = String::from_utf8(open[9..].to_vec()).unwrap();
        let w = run(&pkt(WRITE, 2, |o| { o.str(&handle).u64(0).bytes(b"hello"); }), &mut h, &mut n);
        assert_eq!((w[0], u32::from_be_bytes(w[5..9].try_into().unwrap())), (STATUS, OK));
        run(&pkt(CLOSE, 3, |o| { o.str(&handle); }), &mut h, &mut n);
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"hello");
        let st = run(&pkt(STAT, 4, |o| { o.str(&file); }), &mut h, &mut n);
        assert_eq!(st[0], ATTRS);
        let ls = run(&pkt(OPENDIR, 5, |o| { o.str(&remote(&dir)); }), &mut h, &mut n);
        assert_eq!(ls[0], HANDLE);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
