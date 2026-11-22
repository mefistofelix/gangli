//! Running a shell or a command on the node for a console session (section 7 of docs/ARCHITECTURE.md),
//! with or without a pseudo terminal, as the requested account:
//! - `system`: the account the node runs as (SYSTEM / root, or the user who runs it by hand);
//! - `desktop`: whoever uses the machine's desktop now (Windows: the active console session, else any
//!   logged-in session; macOS: the console user; Linux: a user with a running login session);
//! - any other name: that local account (Windows: it must have a logged-in session — there is no password).
//!
//! Only a node running as SYSTEM / root can act as someone else. The PTY is ConPTY on Windows, `/dev/ptmx`
//! (rustix) on Linux and macOS. Blocking I/O runs on plain threads; the session task sees two channels.

use anyhow::{bail, Result};

pub struct Spec {
    pub user: String,
    /// `None`: an interactive login shell.
    pub command: Option<String>,
    /// Columns, rows (with a terminal) or `None` (plain pipes).
    pub pty: Option<(u16, u16)>,
    pub term: String,
}

pub enum In {
    Data(Vec<u8>),
    Resize(u16, u16),
    Eof,
}

pub enum Out {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

pub struct Child {
    /// Dropping it (session gone) kills the process.
    pub input: std::sync::mpsc::Sender<In>,
    pub output: tokio::sync::mpsc::UnboundedReceiver<Out>,
}

pub fn spawn(spec: &Spec) -> Result<Child> {
    if spec.user.is_empty() {
        bail!("no user given");
    }
    imp::spawn(spec)
}

/// Who is logged in now, for the inventory: Windows its sessions (one not on the screen marked
/// "(disconnected)"), macOS the console user, Linux the users with a login session.
pub fn logged_in() -> Vec<String> {
    imp::logged_in()
}

fn reader_thread(mut r: impl std::io::Read + Send + 'static, tx: tokio::sync::mpsc::UnboundedSender<Out>, err: bool) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16384];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    if tx.send(if err { Out::Stderr(chunk) } else { Out::Stdout(chunk) }).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(unix)]
mod imp {
    use std::io::Write;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};
    use rustix::termios::Winsize;

    use super::{reader_thread, Child, In, Out, Spec};

    struct Account {
        name: String,
        uid: u32,
        gid: u32,
        home: String,
        shell: String,
    }

    fn passwd_entry(pred: impl Fn(&[&str]) -> bool) -> Option<Account> {
        let text = std::fs::read_to_string("/etc/passwd").ok()?;
        text.lines().map(|l| l.split(':').collect::<Vec<_>>()).filter(|f| f.len() >= 7).find(|f| pred(f)).map(|f| Account {
            name: f[0].to_string(),
            uid: f[2].parse().unwrap_or(u32::MAX),
            gid: f[3].parse().unwrap_or(u32::MAX),
            home: f[5].to_string(),
            shell: if f[6].is_empty() { "/bin/sh".into() } else { f[6].to_string() },
        })
    }

    fn id_of(args: &[&str]) -> Option<String> {
        let out = Command::new("id").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// A local account by name: `/etc/passwd`, else (macOS directory services) the standard `id` tool.
    fn lookup(name: &str) -> Result<Account> {
        if let Some(a) = passwd_entry(|f| f[0] == name) {
            return Ok(a);
        }
        let uid: u32 = id_of(&["-u", name]).and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("no local account {name}"))?;
        let gid: u32 = id_of(&["-g", name]).and_then(|s| s.parse().ok()).unwrap_or(uid);
        let home = if uid == 0 { "/var/root".to_string() } else { format!("/Users/{name}") };
        Ok(Account { name: name.to_string(), uid, gid, home, shell: "/bin/zsh".into() })
    }

    fn name_of_uid(uid: u32) -> Option<String> {
        passwd_entry(|f| f[2] == uid.to_string()).map(|a| a.name).or_else(|| id_of(&["-un", &uid.to_string()]))
    }

    fn current_uid() -> u32 {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::MetadataExt;
            let p = std::env::temp_dir().join(format!(".gangli-uid-{}", std::process::id()));
            let uid = std::fs::write(&p, b"").and_then(|_| std::fs::metadata(&p)).map(|m| m.uid()).unwrap_or(u32::MAX);
            let _ = std::fs::remove_file(&p);
            uid
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).and_then(|l| l.split_whitespace().nth(2)).and_then(|u| u.parse().ok()))
                .unwrap_or(u32::MAX)
        }
    }

    /// The user at the desktop: macOS owner of /dev/console; Linux a user with a runtime dir (a login session).
    fn desktop_user() -> Result<String> {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::MetadataExt;
            let uid = std::fs::metadata("/dev/console").map(|m| m.uid()).context("no console user")?;
            if uid == 0 {
                bail!("nobody is logged in at the desktop");
            }
            name_of_uid(uid).ok_or_else(|| anyhow!("unknown console user"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut uids: Vec<u32> = std::fs::read_dir("/run/user")
                .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().parse().ok()).filter(|u| *u >= 1000).collect())
                .unwrap_or_default();
            uids.sort();
            let uid = uids.first().copied().ok_or_else(|| anyhow!("nobody is logged in at the desktop"))?;
            name_of_uid(uid).ok_or_else(|| anyhow!("unknown desktop user"))
        }
    }

    pub fn logged_in() -> Vec<String> {
        #[cfg(target_os = "macos")]
        {
            desktop_user().map(|u| vec![u]).unwrap_or_default()
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut uids: Vec<u32> = std::fs::read_dir("/run/user")
                .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().parse().ok()).filter(|u| *u >= 1000).collect())
                .unwrap_or_default();
            uids.sort();
            uids.into_iter().filter_map(name_of_uid).collect()
        }
    }

    /// The account to run as; `None` means "as this process".
    fn resolve(user: &str) -> Result<Option<Account>> {
        let me = current_uid();
        let name = match user {
            "system" => return Ok(None),
            // No user given: the desktop user if there is one, else the node's own account.
            "default" => match desktop_user() {
                Ok(n) if me == 0 => n,
                _ => return Ok(None),
            },
            "desktop" => desktop_user()?,
            other => other.to_string(),
        };
        let acc = lookup(&name)?;
        if acc.uid == me {
            return Ok(Some(acc));
        }
        if me != 0 {
            let who = name_of_uid(me).unwrap_or_else(|| me.to_string());
            bail!("this machine's node runs as {who}: it can only open sessions as {who} (install the service for any user)");
        }
        Ok(Some(acc))
    }

    fn base_command(acc: &Option<Account>, spec: &Spec) -> Command {
        let shell = acc.as_ref().map(|a| a.shell.clone()).or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/sh".into());
        let sftp = spec.command.as_deref() == Some(crate::sftp::COMMAND);
        let mut cmd = if sftp {
            let mut c = Command::new(std::env::current_exe().unwrap_or_else(|_| "gangli".into()));
            c.arg("sftp-server");
            c
        } else {
            Command::new(&shell)
        };
        match &spec.command {
            Some(_) if sftp => {}
            Some(c) => {
                cmd.arg("-c").arg(c);
            }
            None => {
                // A login shell: argv[0] starting with '-'.
                let base = shell.rsplit('/').next().unwrap_or("sh");
                cmd.arg0(format!("-{base}"));
            }
        }
        if let Some(a) = acc {
            cmd.env_clear();
            cmd.env("HOME", &a.home).env("USER", &a.name).env("LOGNAME", &a.name).env("SHELL", &a.shell);
            cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/homebrew/bin");
            cmd.current_dir(&a.home);
            if a.uid != current_uid() {
                cmd.uid(a.uid).gid(a.gid);
            }
        }
        cmd.env("TERM", &spec.term);
        // No login manager sets the language here: without it shells run in the C locale and drop accented
        // input. Use the system's (or UTF-8), unless this process already has one.
        if acc.is_some() || std::env::var_os("LANG").is_none() {
            cmd.env("LANG", system_lang());
        }
        cmd
    }

    /// The system language (`/etc/default/locale`, `/etc/locale.conf`), else `C.UTF-8`; `en_US.UTF-8` on macOS.
    fn system_lang() -> String {
        if let Ok(l) = std::env::var("LANG") {
            if l.to_uppercase().contains("UTF") {
                return l;
            }
        }
        for f in ["/etc/default/locale", "/etc/locale.conf"] {
            if let Ok(s) = std::fs::read_to_string(f) {
                for line in s.lines() {
                    if let Some(v) = line.trim().strip_prefix("LANG=") {
                        let v = v.trim_matches('"').trim();
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }
        if cfg!(target_os = "macos") {
            "en_US.UTF-8".into()
        } else {
            "C.UTF-8".into()
        }
    }

    pub fn spawn(spec: &Spec) -> Result<Child> {
        let acc = resolve(&spec.user)?;
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::channel::<In>();
        let mut cmd = base_command(&acc, spec);
        let mut readers = vec![];
        let mut writer: Box<dyn Write + Send>;
        let mut master_for_resize: Option<OwnedFd> = None;
        match spec.pty {
            Some((cols, rows)) => {
                use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
                let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).context("open a pseudo terminal")?;
                grantpt(&master).context("grantpt")?;
                unlockpt(&master).context("unlockpt")?;
                let name = ptsname(&master, Vec::new()).context("ptsname")?;
                let slave = rustix::fs::open(name.as_c_str(), rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY, rustix::fs::Mode::empty())
                    .context("open the terminal")?;
                let _ = rustix::termios::tcsetwinsize(&slave, Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 });
                cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
                // New session with the terminal as controlling tty (stdio is already in place here).
                unsafe {
                    cmd.pre_exec(|| {
                        rustix::process::setsid().map_err(std::io::Error::from)?;
                        #[cfg(not(target_os = "macos"))]
                        rustix::process::ioctl_tiocsctty(BorrowedFd::borrow_raw(0)).map_err(std::io::Error::from)?;
                        Ok(())
                    });
                }
                let reader = std::fs::File::from(master.try_clone()?);
                writer = Box::new(std::fs::File::from(master.try_clone()?));
                readers.push(reader_thread(reader, out_tx.clone(), false));
                master_for_resize = Some(master);
                let child = cmd.spawn().context("start the shell")?;
                drop(cmd);
                return Ok(finish(child, readers, writer, master_for_resize, in_tx, in_rx, out_tx, out_rx, true));
            }
            None => {
                cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = cmd.spawn().context("start the command")?;
                writer = Box::new(child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?);
                readers.push(reader_thread(child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?, out_tx.clone(), false));
                readers.push(reader_thread(child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?, out_tx.clone(), true));
                let _ = &mut master_for_resize;
                Ok(finish(child, readers, writer, None, in_tx, in_rx, out_tx, out_rx, false))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        child: std::process::Child,
        readers: Vec<std::thread::JoinHandle<()>>,
        mut writer: Box<dyn Write + Send>,
        master: Option<OwnedFd>,
        in_tx: mpsc::Sender<In>,
        in_rx: mpsc::Receiver<In>,
        out_tx: tokio::sync::mpsc::UnboundedSender<Out>,
        out_rx: tokio::sync::mpsc::UnboundedReceiver<Out>,
        pty: bool,
    ) -> Child {
        let child = Arc::new(Mutex::new(child));
        let kill = Arc::new(AtomicBool::new(false));
        // Input: data, resizes, EOF; the sender dropped (session gone) kills the process.
        {
            let kill = kill.clone();
            std::thread::spawn(move || {
                loop {
                    match in_rx.recv() {
                        Ok(In::Data(d)) => {
                            if writer.write_all(&d).is_err() {
                                break;
                            }
                            let _ = writer.flush();
                        }
                        Ok(In::Resize(cols, rows)) => {
                            if let Some(m) = &master {
                                let _ = rustix::termios::tcsetwinsize(m.as_fd(), Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 });
                            }
                        }
                        Ok(In::Eof) => {
                            if pty {
                                let _ = writer.write_all(&[4]);
                            } else {
                                break; // closes stdin
                            }
                        }
                        Err(_) => {
                            kill.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            });
        }
        // Waiter: exit code once the process ends, after its output.
        std::thread::spawn(move || {
            let status = loop {
                if kill.load(Ordering::SeqCst) {
                    let _ = child.lock().unwrap().kill();
                }
                match child.lock().unwrap().try_wait() {
                    Ok(Some(s)) => break s,
                    Ok(None) => {}
                    Err(_) => break std::process::ExitStatus::from_raw(1 << 8),
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            if !pty {
                for r in readers {
                    let _ = r.join();
                }
            } else {
                // The terminal reader ends when the last holder of the slave closes; give it a moment.
                std::thread::sleep(Duration::from_millis(150));
            }
            let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
            let _ = out_tx.send(Out::Exit(code));
        });
        Child { input: in_tx, output: out_rx }
    }
}

#[cfg(windows)]
mod imp {
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    use anyhow::{anyhow, bail, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON};
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSQueryUserToken,
        WTSUserName, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
        TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW, STARTUPINFOW,
    };

    use super::{reader_thread, Child, In, Out, Spec};

    const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn last_err(what: &str) -> anyhow::Error {
        anyhow!("{what}: {}", std::io::Error::last_os_error())
    }

    fn session_user(id: u32) -> Option<String> {
        unsafe {
            let mut buf: *mut u16 = std::ptr::null_mut();
            let mut len = 0u32;
            if WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, id, WTSUserName, &mut buf, &mut len) == 0 || buf.is_null() {
                return None;
            }
            let n = (0..).take_while(|i| *buf.add(*i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(buf, n));
            WTSFreeMemory(buf as _);
            (!s.is_empty()).then_some(s)
        }
    }

    /// Logged-in sessions: (session id, user, active).
    fn sessions() -> Vec<(u32, String, bool)> {
        let mut out = vec![];
        unsafe {
            let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
            let mut count = 0u32;
            if WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) == 0 {
                return out;
            }
            for i in 0..count as usize {
                let s = &*info.add(i);
                if let Some(u) = session_user(s.SessionId) {
                    out.push((s.SessionId, u, s.State == WTSActive));
                }
            }
            WTSFreeMemory(info as _);
        }
        out
    }

    fn me() -> String {
        std::env::var("USERNAME").unwrap_or_default()
    }

    pub fn logged_in() -> Vec<String> {
        let mut list = sessions();
        list.sort_by_key(|(_, _, active)| !*active);
        let mut out: Vec<String> = vec![];
        for (_, u, active) in list {
            let label = if active { u.clone() } else { format!("{u} (disconnected)") };
            if !out.iter().any(|x| x == &u || x == &label) {
                out.push(label);
            }
        }
        out
    }

    /// The session token for `user` (`desktop`: the active console session), or `None` for "as this process".
    fn token_for(user: &str) -> Result<Option<HANDLE>> {
        if user.eq_ignore_ascii_case("system") || user.eq_ignore_ascii_case(&me()) {
            return Ok(None);
        }
        // No user given: the desktop user if there is one (and we may act as them), else this account.
        if user == "default" {
            if !crate::elevate::is_root() || sessions().is_empty() {
                return Ok(None);
            }
            return token_for("desktop");
        }
        if !crate::elevate::is_root() {
            bail!("this machine's node runs as {}: it can only open sessions as {} (install the service for any user)", me(), me());
        }
        let list = sessions();
        let id = if user.eq_ignore_ascii_case("desktop") {
            let console = unsafe { WTSGetActiveConsoleSessionId() };
            if list.iter().any(|(id, _, _)| *id == console) {
                console
            } else {
                list.iter().find(|(_, _, active)| *active).or(list.first()).map(|(id, _, _)| *id).ok_or_else(|| anyhow!("nobody is logged in"))?
            }
        } else {
            let mut hits: Vec<&(u32, String, bool)> = list.iter().filter(|(_, u, _)| u.eq_ignore_ascii_case(user)).collect();
            hits.sort_by_key(|(_, _, active)| !*active);
            hits.first().map(|(id, _, _)| *id).ok_or_else(|| anyhow!("{user} is not logged in on this machine (Windows needs a session to act as a user without a password)"))?
        };
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { WTSQueryUserToken(id, &mut token) } == 0 {
            return Err(last_err("cannot get the session's token"));
        }
        Ok(Some(token))
    }

    /// `USERPROFILE` from an environment block (UTF-16, NUL-separated, double NUL at the end).
    fn profile_dir(env: *const u16) -> Option<String> {
        if env.is_null() {
            return None;
        }
        unsafe {
            let mut p = env;
            loop {
                let n = (0..).take_while(|i| *p.add(*i) != 0).count();
                if n == 0 {
                    return None;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                if let Some(v) = s.strip_prefix("USERPROFILE=") {
                    return Some(v.to_string());
                }
                p = p.add(n + 1);
            }
        }
    }

    /// A pipe; `child_end_inheritable` makes the child's end inheritable (pipes without a terminal).
    fn pipe(read_inheritable: bool, write_inheritable: bool) -> Result<(HANDLE, HANDLE)> {
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: std::ptr::null_mut(), bInheritHandle: 1 };
        let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
            return Err(last_err("CreatePipe"));
        }
        unsafe {
            if !read_inheritable {
                SetHandleInformation(r, HANDLE_FLAG_INHERIT, 0);
            }
            if !write_inheritable {
                SetHandleInformation(w, HANDLE_FLAG_INHERIT, 0);
            }
        }
        Ok((r, w))
    }

    pub fn spawn(spec: &Spec) -> Result<Child> {
        let token = token_for(&spec.user)?;
        let cmdline = match &spec.command {
            Some(c) if c == crate::sftp::COMMAND => {
                format!("\"{}\" sftp-server", std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "gangli.exe".into()))
            }
            Some(c) => format!("cmd.exe /d /c {c}"),
            None => "powershell.exe -NoLogo".to_string(),
        };
        let mut cmdline_w = wide(&cmdline);

        // Environment and folder of the user (for another session), else inherited.
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        if let Some(t) = token {
            if unsafe { CreateEnvironmentBlock(&mut env, t, 0) } == 0 {
                env = std::ptr::null_mut();
            }
        }
        let cwd = profile_dir(env as *const u16).or_else(|| std::env::var("USERPROFILE").ok()).filter(|_| token.is_some() || std::env::var("USERPROFILE").is_ok());
        let cwd_w = cwd.as_deref().map(wide);

        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::channel::<In>();
        let mut readers = vec![];
        let mut hpc: HPCON = 0;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut flags = if env.is_null() { 0 } else { CREATE_UNICODE_ENVIRONMENT };
        let stdin_w: HANDLE;

        let ok = unsafe {
            match spec.pty {
                Some((cols, rows)) => {
                    let (in_r, in_w) = pipe(false, false)?;
                    let (out_r, out_w) = pipe(false, false)?;
                    let size = COORD { X: cols as i16, Y: rows as i16 };
                    if CreatePseudoConsole(size, in_r, out_w, 0, &mut hpc) < 0 {
                        return Err(last_err("CreatePseudoConsole"));
                    }
                    CloseHandle(in_r);
                    CloseHandle(out_w);
                    let mut size_needed = 0usize;
                    InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size_needed);
                    let mut attr_buf = vec![0u8; size_needed];
                    let attrs = attr_buf.as_mut_ptr() as *mut core::ffi::c_void;
                    if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size_needed) == 0 {
                        return Err(last_err("InitializeProcThreadAttributeList"));
                    }
                    if UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize, hpc as *const core::ffi::c_void, std::mem::size_of::<HPCON>(), std::ptr::null_mut(), std::ptr::null()) == 0 {
                        return Err(last_err("UpdateProcThreadAttribute"));
                    }
                    let mut si: STARTUPINFOEXW = std::mem::zeroed();
                    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                    // The child must use the pseudo console, not inherit this process's stdin / stdout.
                    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
                    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
                    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
                    si.lpAttributeList = attrs;
                    flags |= EXTENDED_STARTUPINFO_PRESENT;
                    let cwd_p = cwd_w.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
                    let r = match token {
                        Some(t) => CreateProcessAsUserW(t, std::ptr::null(), cmdline_w.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, flags, env, cwd_p, &si as *const STARTUPINFOEXW as *const STARTUPINFOW, &mut pi),
                        None => CreateProcessW(std::ptr::null(), cmdline_w.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, flags, env, cwd_p, &si as *const STARTUPINFOEXW as *const STARTUPINFOW, &mut pi),
                    };
                    DeleteProcThreadAttributeList(attrs);
                    readers.push(reader_thread(std::fs::File::from_raw_handle(out_r as _), out_tx.clone(), false));
                    stdin_w = in_w;
                    r
                }
                None => {
                    let (in_r, in_w) = pipe(true, false)?;
                    let (out_r, out_w) = pipe(false, true)?;
                    let (err_r, err_w) = pipe(false, true)?;
                    let mut si: STARTUPINFOW = std::mem::zeroed();
                    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
                    si.dwFlags = STARTF_USESTDHANDLES;
                    si.hStdInput = in_r;
                    si.hStdOutput = out_w;
                    si.hStdError = err_w;
                    flags |= CREATE_NO_WINDOW;
                    let cwd_p = cwd_w.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
                    let r = match token {
                        Some(t) => CreateProcessAsUserW(t, std::ptr::null(), cmdline_w.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, env, cwd_p, &si, &mut pi),
                        None => CreateProcessW(std::ptr::null(), cmdline_w.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 1, flags, env, cwd_p, &si, &mut pi),
                    };
                    CloseHandle(in_r);
                    CloseHandle(out_w);
                    CloseHandle(err_w);
                    readers.push(reader_thread(std::fs::File::from_raw_handle(out_r as _), out_tx.clone(), false));
                    readers.push(reader_thread(std::fs::File::from_raw_handle(err_r as _), out_tx.clone(), true));
                    stdin_w = in_w;
                    r
                }
            }
        };
        if !env.is_null() {
            unsafe { DestroyEnvironmentBlock(env) };
        }
        if let Some(t) = token {
            unsafe { CloseHandle(t) };
        }
        if ok == 0 {
            let e = last_err("cannot start the process");
            if hpc != 0 {
                unsafe { ClosePseudoConsole(hpc) };
            }
            return Err(e);
        }
        unsafe { CloseHandle(pi.hThread) };

        let process = pi.hProcess as usize;
        let hpc_v = hpc;
        let pty = spec.pty.is_some();
        let kill = Arc::new(AtomicBool::new(false));
        let mut writer = unsafe { std::fs::File::from_raw_handle(stdin_w as _) };
        {
            let kill = kill.clone();
            std::thread::spawn(move || loop {
                match in_rx.recv() {
                    Ok(In::Data(d)) => {
                        if writer.write_all(&d).is_err() {
                            break;
                        }
                    }
                    Ok(In::Resize(cols, rows)) => {
                        if hpc_v != 0 {
                            unsafe { ResizePseudoConsole(hpc_v, COORD { X: cols as i16, Y: rows as i16 }) };
                        }
                    }
                    Ok(In::Eof) => {
                        if pty {
                            let _ = writer.write_all(&[26, b'\r']);
                        } else {
                            break;
                        }
                    }
                    Err(_) => {
                        kill.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            });
        }
        std::thread::spawn(move || {
            let h = process as HANDLE;
            loop {
                if kill.load(Ordering::SeqCst) {
                    unsafe { TerminateProcess(h, 1) };
                }
                if unsafe { WaitForSingleObject(h, 200) } == 0 {
                    break;
                }
            }
            let mut code = 1u32;
            unsafe {
                GetExitCodeProcess(h, &mut code);
                CloseHandle(h);
                // Closing the pseudo console flushes its output and ends the reader.
                if hpc_v != 0 {
                    ClosePseudoConsole(hpc_v);
                }
            }
            for r in readers {
                let _ = r.join();
            }
            let _ = out_tx.send(Out::Exit(code as i32));
        });
        Ok(Child { input: in_tx, output: out_rx })
    }
}
