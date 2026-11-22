//! Power on a node: mains or battery, and its sleep / hibernation settings, read and set from a console
//! (section 8 of docs/ARCHITECTURE.md). A managed machine that goes to sleep drops off the network until
//! someone wakes it; these settings let the owner keep it awake when plugged in. The screen's own timeout is
//! shown and never changed: the screen still turns off.
//!
//! Windows: the power API of the active plan (powrprof; no external program). macOS: `pmset`. Linux: sleep is
//! all or nothing for the whole system (systemd's sleep targets masked or not); idle timeouts belong to the
//! desktop session. Changing them needs the service (SYSTEM / root).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Minutes (0: never; `None`: not available on this system).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Power {
    /// Runs on mains now (`None`: unknown, e.g. a desktop without a battery on Linux).
    pub on_ac: Option<bool>,
    /// Battery charge, percent (`None`: no battery).
    pub battery: Option<u8>,
    /// Plugged in (`_ac`) and on battery (`_dc`).
    pub sleep_ac: Option<u32>,
    pub sleep_dc: Option<u32>,
    pub hibernate_ac: Option<u32>,
    pub hibernate_dc: Option<u32>,
    pub display_ac: Option<u32>,
    pub display_dc: Option<u32>,
    /// Linux: sleep and hibernation are turned off for the whole system.
    pub sleep_blocked: Option<bool>,
    /// What can be set here, or why not.
    pub note: String,
}

/// A change: only the fields given are set.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PowerSet {
    #[serde(default)]
    pub sleep_ac: Option<u32>,
    #[serde(default)]
    pub sleep_dc: Option<u32>,
    #[serde(default)]
    pub hibernate_ac: Option<u32>,
    #[serde(default)]
    pub hibernate_dc: Option<u32>,
    #[serde(default)]
    pub sleep_blocked: Option<bool>,
}

impl PowerSet {
    pub fn is_empty(&self) -> bool {
        *self == PowerSet::default()
    }
}

/// Mains or battery and the charge: cheap, for the inventory.
pub fn source() -> (Option<bool>, Option<u8>) {
    imp::source()
}

pub fn read() -> Power {
    let mut p = imp::read();
    (p.on_ac, p.battery) = source();
    p
}

pub fn set(s: &PowerSet) -> Result<Power> {
    if s.is_empty() {
        return Ok(read());
    }
    if [s.sleep_ac, s.sleep_dc, s.hibernate_ac, s.hibernate_dc].iter().flatten().any(|m| *m > 24 * 60 * 7) {
        bail!("a timeout of more than a week: use 0 for never");
    }
    imp::set(s)?;
    Ok(read())
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
    use anyhow::{bail, Result};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::System::Power::{
        GetSystemPowerStatus, PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex, PowerSetActiveScheme, PowerWriteACValueIndex,
        PowerWriteDCValueIndex, SYSTEM_POWER_STATUS,
    };

    use super::{Power, PowerSet};

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

    /// The active power plan (its GUID), freed after use.
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

    pub fn read() -> Power {
        let Some(s) = active() else {
            return Power { note: "cannot read the power plan".into(), ..Default::default() };
        };
        let battery = source().1.is_some();
        let dc = |sub: &GUID, set: &GUID| if battery { get(&s, sub, set, false) } else { None };
        Power {
            sleep_ac: get(&s, &SUB_SLEEP, &STANDBY_IDLE, true),
            sleep_dc: dc(&SUB_SLEEP, &STANDBY_IDLE),
            hibernate_ac: get(&s, &SUB_SLEEP, &HIBERNATE_IDLE, true),
            hibernate_dc: dc(&SUB_SLEEP, &HIBERNATE_IDLE),
            display_ac: get(&s, &SUB_VIDEO, &VIDEO_IDLE, true),
            display_dc: dc(&SUB_VIDEO, &VIDEO_IDLE),
            note: if crate::elevate::is_root() { String::new() } else { "the node runs as a user: changing these may need the service".into() },
            ..Default::default()
        }
    }

    pub fn set(p: &PowerSet) -> Result<()> {
        if p.sleep_blocked.is_some() {
            bail!("on Windows sleep is set with its timeouts (0: never)");
        }
        let Some(s) = active() else { bail!("cannot read the power plan") };
        let w = |setting: &GUID, ac: bool, min: Option<u32>| -> Result<()> {
            let Some(m) = min else { return Ok(()) };
            let r = unsafe {
                if ac {
                    PowerWriteACValueIndex(std::ptr::null_mut(), &s, &SUB_SLEEP, setting, m * 60)
                } else {
                    PowerWriteDCValueIndex(std::ptr::null_mut(), &s, &SUB_SLEEP, setting, m * 60)
                }
            };
            if r != 0 {
                bail!("cannot change the power plan: {}", std::io::Error::from_raw_os_error(r as i32));
            }
            Ok(())
        };
        w(&STANDBY_IDLE, true, p.sleep_ac)?;
        w(&STANDBY_IDLE, false, p.sleep_dc)?;
        w(&HIBERNATE_IDLE, true, p.hibernate_ac)?;
        w(&HIBERNATE_IDLE, false, p.hibernate_dc)?;
        // Applying the plan again makes the change take effect now.
        unsafe { PowerSetActiveScheme(std::ptr::null_mut(), &s) };
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    use anyhow::{bail, Result};

    use super::{Power, PowerSet};

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
    fn custom() -> (Vec<(String, u32)>, Vec<(String, u32)>) {
        let (mut ac, mut dc) = (vec![], vec![]);
        let Some(t) = pmset(&["-g", "custom"]) else { return (ac, dc) };
        let mut cur = 0;
        for l in t.lines() {
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
        (ac, dc)
    }

    pub fn read() -> Power {
        let (ac, dc) = custom();
        let get = |l: &[(String, u32)], k: &str| l.iter().find(|(x, _)| x == k).map(|(_, v)| *v);
        Power {
            sleep_ac: get(&ac, "sleep"),
            sleep_dc: get(&dc, "sleep"),
            display_ac: get(&ac, "displaysleep"),
            display_dc: get(&dc, "displaysleep"),
            note: "macOS: hibernation follows sleep (hibernatemode)".into(),
            ..Default::default()
        }
    }

    pub fn set(p: &PowerSet) -> Result<()> {
        if p.hibernate_ac.is_some() || p.hibernate_dc.is_some() || p.sleep_blocked.is_some() {
            bail!("on macOS only the sleep timeouts can be set");
        }
        for (flag, m) in [("-c", p.sleep_ac), ("-b", p.sleep_dc)] {
            if let Some(m) = m {
                let out = Command::new("pmset").args([flag, "sleep", &m.to_string()]).output()?;
                if !out.status.success() {
                    bail!("pmset: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::process::Command;

    use anyhow::{bail, Result};

    use super::{Power, PowerSet};

    const TARGETS: [&str; 5] = ["sleep.target", "suspend.target", "hibernate.target", "hybrid-sleep.target", "suspend-then-hibernate.target"];

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

    pub fn read() -> Power {
        let blocked = Command::new("systemctl").args(["is-enabled", "suspend.target"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim() == "masked");
        Power {
            sleep_blocked: blocked,
            note: "Linux: sleep and hibernation are on or off for the whole system; idle timeouts belong to the desktop".into(),
            ..Default::default()
        }
    }

    pub fn set(p: &PowerSet) -> Result<()> {
        if [p.sleep_ac, p.sleep_dc, p.hibernate_ac, p.hibernate_dc].iter().any(Option::is_some) {
            bail!("on Linux sleep is on or off for the whole system (sleep_blocked)");
        }
        if let Some(b) = p.sleep_blocked {
            let mut args = vec![if b { "mask" } else { "unmask" }];
            args.extend(TARGETS);
            let out = Command::new("systemctl").args(&args).output()?;
            if !out.status.success() {
                bail!("systemctl: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_sets() {
        assert_eq!(minutes_label(Some(0)), "never");
        assert_eq!(minutes_label(Some(120)), "2 h");
        assert_eq!(minutes_label(Some(15)), "15 min");
        assert!(PowerSet::default().is_empty());
        let s: PowerSet = serde_json::from_str(r#"{"sleep_ac":0}"#).unwrap();
        assert_eq!(s.sleep_ac, Some(0));
        assert!(set(&PowerSet { sleep_ac: Some(100_000), ..Default::default() }).is_err());
        // Reading never fails, whatever the system (CI runners included).
        let _ = read();
    }
}
