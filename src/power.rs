//! Power on a node (section 9 of docs/ARCHITECTURE.md): mains or battery, the system's own sleep /
//! hibernation / screen timeouts (read only), and *keep awake*: a temporary request to the OS not to go to
//! sleep while idle, held by the running node while the owner wants it. No system setting is ever changed:
//! the request ends when it is turned off or the node stops, and everything is as the user set it. The
//! screen still turns off. It shows in the system's own tools (Windows `powercfg /requests`, macOS
//! `pmset -g assertions`, Linux `systemd-inhibit --list`).
//!
//! Windows: `SetThreadExecutionState(ES_SYSTEM_REQUIRED)` on a thread of its own; macOS: an IOKit power
//! assertion (PreventUserIdleSystemSleep); Linux: a logind inhibitor (sleep:idle, over D-Bus).

use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

/// When the node keeps its machine awake (`[node] keep_awake` in `gangli.node`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeepAwake {
    /// As the machine is set (default).
    #[default]
    Off,
    /// While on mains (a laptop on battery sleeps as usual).
    Plugged,
    Always,
}

impl KeepAwake {
    pub fn label(self) -> &'static str {
        match self {
            KeepAwake::Off => "off",
            KeepAwake::Plugged => "when plugged in",
            KeepAwake::Always => "always",
        }
    }

    /// Whether to hold the request now.
    pub fn wanted(self, on_ac: Option<bool>) -> bool {
        match self {
            KeepAwake::Off => false,
            KeepAwake::Plugged => on_ac != Some(false),
            KeepAwake::Always => true,
        }
    }
}

/// Minutes (0: never; `None`: not available on this system).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Power {
    /// Runs on mains now (`None`: unknown, e.g. a desktop without a battery on Linux).
    pub on_ac: Option<bool>,
    /// Battery charge, percent (`None`: no battery).
    pub battery: Option<u8>,
    /// The system's own timeouts, plugged in (`_ac`) and on battery (`_dc`): shown, never changed.
    pub sleep_ac: Option<u32>,
    pub sleep_dc: Option<u32>,
    pub hibernate_ac: Option<u32>,
    pub hibernate_dc: Option<u32>,
    pub display_ac: Option<u32>,
    pub display_dc: Option<u32>,
    /// gangli's temporary request, as set and whether it is held now.
    pub keep_awake: KeepAwake,
    pub awake: bool,
}

/// Mains or battery and the charge: cheap, for the inventory and the keep-awake check.
pub fn source() -> (Option<bool>, Option<u8>) {
    imp::source()
}

/// What the machine is set to (and, from the caller, gangli's keep-awake).
pub fn read() -> Power {
    let mut p = imp::timeouts();
    (p.on_ac, p.battery) = source();
    p.awake = held();
    p
}

static HELD: AtomicBool = AtomicBool::new(false);

/// gangli's request is held now.
pub fn held() -> bool {
    HELD.load(Ordering::SeqCst)
}

/// Hold the request (`true`) or let it go; called by the node's loop every few seconds, cheap when nothing
/// changes. Errors are logged once per change.
pub async fn hold(want: bool) {
    if want == held() {
        return;
    }
    match imp::hold(want).await {
        Ok(()) => {
            HELD.store(want, Ordering::SeqCst);
            crate::log_line(if want { "keeping this machine awake (a temporary request; its settings are untouched)" } else { "no longer keeping this machine awake" });
        }
        Err(e) => {
            // Not retried every loop: marked as done, the next change tries again.
            HELD.store(want, Ordering::SeqCst);
            crate::log_line(&format!("keep awake: {e:#}"));
        }
    }
}

/// Start the machine's idle time again from zero (Windows): when the keep-awake request ends (turned off, or
/// the process restarting for an update), a machine idle for longer than its sleep timeout would otherwise go
/// to sleep that same second. Harmless when nothing is held.
pub fn nudge() {
    imp::nudge();
}

/// "never", "10 min", "2 h".
pub fn minutes_label(m: Option<u32>) -> String {
    match m {
        None => "—".into(),
        Some(0) => "never".into(),
        Some(m) if m % 60 == 0 => format!("{} h", m / 60),
        Some(m) => format!("{m} min"),
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use std::time::Duration;

    use anyhow::Result;
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::System::Power::{
        GetSystemPowerStatus, PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex, SetThreadExecutionState, ES_CONTINUOUS,
        ES_SYSTEM_REQUIRED, SYSTEM_POWER_STATUS,
    };

    use super::Power;

    const SUB_SLEEP: GUID = GUID::from_u128(0x238c9fa8_0aad_41ed_83f4_97be242c8f20);
    const STANDBY_IDLE: GUID = GUID::from_u128(0x29f6c1db_86da_48c5_9fdb_f2b67b1f44da);
    const HIBERNATE_IDLE: GUID = GUID::from_u128(0x9d7815a6_7ee4_497e_8888_515a05f02364);
    const SUB_VIDEO: GUID = GUID::from_u128(0x7516b95f_f776_4464_8c53_06167f40cc99);
    const VIDEO_IDLE: GUID = GUID::from_u128(0x3c0bc021_c8a8_4e07_a973_6b14cbcb2b7e);

    pub fn source() -> (Option<bool>, Option<u8>) {
        let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
            return (None, None);
        }
        let on_ac = match s.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        // 128: no system battery; 255: unknown.
        let battery = (s.BatteryFlag & 128 == 0 && s.BatteryFlag != 255 && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent);
        (on_ac, battery)
    }

    /// The active power plan (its GUID).
    fn active() -> Option<GUID> {
        let mut p: *mut GUID = std::ptr::null_mut();
        if unsafe { PowerGetActiveScheme(std::ptr::null_mut(), &mut p) } != 0 || p.is_null() {
            return None;
        }
        let g = unsafe { *p };
        unsafe { LocalFree(p as _) };
        Some(g)
    }

    fn get(scheme: &GUID, sub: &GUID, setting: &GUID, ac: bool) -> Option<u32> {
        let mut v = 0u32;
        let r = unsafe {
            if ac {
                PowerReadACValueIndex(std::ptr::null_mut(), scheme, sub, setting, &mut v)
            } else {
                PowerReadDCValueIndex(std::ptr::null_mut(), scheme, sub, setting, &mut v)
            }
        };
        // Seconds in the plan.
        (r == 0).then_some(v / 60)
    }

    pub fn timeouts() -> Power {
        let Some(s) = active() else { return Power::default() };
        let battery = source().1.is_some();
        let dc = |sub: &GUID, setting: &GUID| if battery { get(&s, sub, setting, false) } else { None };
        Power {
            sleep_ac: get(&s, &SUB_SLEEP, &STANDBY_IDLE, true),
            sleep_dc: dc(&SUB_SLEEP, &STANDBY_IDLE),
            hibernate_ac: get(&s, &SUB_SLEEP, &HIBERNATE_IDLE, true),
            hibernate_dc: dc(&SUB_SLEEP, &HIBERNATE_IDLE),
            display_ac: get(&s, &SUB_VIDEO, &VIDEO_IDLE, true),
            display_dc: dc(&SUB_VIDEO, &VIDEO_IDLE),
            ..Default::default()
        }
    }

    /// Without ES_CONTINUOUS: resets the idle timer once.
    pub fn nudge() {
        unsafe { SetThreadExecutionState(ES_SYSTEM_REQUIRED) };
    }

    static WANT: AtomicBool = AtomicBool::new(false);
    static THREAD: OnceLock<()> = OnceLock::new();

    /// The execution state belongs to a thread: one thread of its own sets it and keeps it as wanted.
    pub async fn hold(want: bool) -> Result<()> {
        WANT.store(want, Ordering::SeqCst);
        THREAD.get_or_init(|| {
            std::thread::spawn(|| {
                let mut was = false;
                loop {
                    let want = WANT.load(Ordering::SeqCst);
                    if want != was {
                        let flags = if want { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS };
                        unsafe { SetThreadExecutionState(flags) };
                        if !want {
                            nudge();
                        }
                        was = want;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
        });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void};
    use std::process::Command;
    use std::sync::Mutex;

    use anyhow::{bail, Result};

    use super::Power;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> *const c_void;
        fn CFRelease(p: *const c_void);
    }
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }
    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;

    fn cfstr(s: &str) -> *const c_void {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) }
    }

    fn pmset(args: &[&str]) -> Option<String> {
        let out = Command::new("pmset").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn source() -> (Option<bool>, Option<u8>) {
        let Some(t) = pmset(&["-g", "batt"]) else { return (None, None) };
        let on_ac = if t.contains("'AC Power'") {
            Some(true)
        } else if t.contains("'Battery Power'") {
            Some(false)
        } else {
            None
        };
        let battery = t.lines().find(|l| l.contains("InternalBattery")).and_then(|l| l.split('\t').nth(1)).and_then(|f| f.split('%').next()).and_then(|n| n.trim().parse().ok());
        (on_ac, battery)
    }

    /// `pmset -g custom`: "Battery Power:" and "AC Power:" sections of "key value" lines.
    pub fn timeouts() -> Power {
        let (mut ac, mut dc): (Vec<(String, u32)>, Vec<(String, u32)>) = (vec![], vec![]);
        let mut cur = 0;
        for l in pmset(&["-g", "custom"]).unwrap_or_default().lines() {
            if l.starts_with("AC Power") {
                cur = 1;
            } else if l.starts_with("Battery Power") {
                cur = 2;
            } else {
                let mut f = l.split_whitespace();
                if let (Some(k), Some(v)) = (f.next(), f.next().and_then(|v| v.parse::<u32>().ok())) {
                    match cur {
                        1 => ac.push((k.to_string(), v)),
                        2 => dc.push((k.to_string(), v)),
                        _ => {}
                    }
                }
            }
        }
        let get = |l: &[(String, u32)], k: &str| l.iter().find(|(x, _)| x == k).map(|(_, v)| *v);
        Power { sleep_ac: get(&ac, "sleep"), sleep_dc: get(&dc, "sleep"), display_ac: get(&ac, "displaysleep"), display_dc: get(&dc, "displaysleep"), ..Default::default() }
    }

    /// macOS restarts its idle count when an assertion is released.
    pub fn nudge() {}

    static ID: Mutex<Option<u32>> = Mutex::new(None);

    pub async fn hold(want: bool) -> Result<()> {
        let mut id = ID.lock().unwrap();
        if want && id.is_none() {
            let (kind, name) = (cfstr("PreventUserIdleSystemSleep"), cfstr("gangli: kept awake from a gangli console"));
            let mut a = 0u32;
            let r = unsafe { IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut a) };
            unsafe {
                CFRelease(kind);
                CFRelease(name);
            }
            if r != 0 {
                bail!("power assertion refused ({r})");
            }
            *id = Some(a);
        } else if !want {
            if let Some(a) = id.take() {
                unsafe { IOPMAssertionRelease(a) };
            }
        }
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::sync::Mutex;

    use anyhow::{Context, Result};

    use super::Power;

    pub fn source() -> (Option<bool>, Option<u8>) {
        let (mut mains, mut charging, mut battery) = (None, None, None);
        let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return (None, None) };
        for e in rd.flatten() {
            let p = e.path();
            let read = |f: &str| std::fs::read_to_string(p.join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
            match read("type").as_str() {
                "Mains" | "USB" => mains = Some(mains.unwrap_or(false) || read("online") == "1"),
                "Battery" if read("scope") != "Device" => {
                    battery = read("capacity").parse::<u8>().ok().or(battery);
                    charging = Some(read("status") != "Discharging");
                }
                _ => {}
            }
        }
        (mains.or(charging), battery)
    }

    /// Idle timeouts belong to the desktop session on Linux: none to show.
    pub fn timeouts() -> Power {
        Power::default()
    }

    /// logind's idle action counts from the last input, not from the inhibitor: nothing to do.
    pub fn nudge() {}

    /// The logind inhibitor lives as long as this descriptor.
    static FD: Mutex<Option<zbus::zvariant::OwnedFd>> = Mutex::new(None);

    pub async fn hold(want: bool) -> Result<()> {
        if !want {
            FD.lock().unwrap().take();
            return Ok(());
        }
        if FD.lock().unwrap().is_some() {
            return Ok(());
        }
        let conn = zbus::Connection::system().await.context("system D-Bus")?;
        let msg = conn
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Inhibit",
                &("sleep:idle", "gangli", "kept awake from a gangli console", "block"),
            )
            .await
            .context("logind inhibitor")?;
        let fd: zbus::zvariant::OwnedFd = msg.body().deserialize().context("logind inhibitor")?;
        *FD.lock().unwrap() = Some(fd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_modes() {
        assert_eq!(minutes_label(Some(0)), "never");
        assert_eq!(minutes_label(Some(120)), "2 h");
        assert_eq!(minutes_label(Some(15)), "15 min");
        assert!(!KeepAwake::Off.wanted(Some(true)));
        assert!(KeepAwake::Plugged.wanted(Some(true)) && KeepAwake::Plugged.wanted(None));
        assert!(!KeepAwake::Plugged.wanted(Some(false)));
        assert!(KeepAwake::Always.wanted(Some(false)));
        assert_eq!(serde_json::to_string(&KeepAwake::Plugged).unwrap(), "\"plugged\"");
        // Reading never fails, whatever the system (CI runners included).
        let _ = read();
    }
}
