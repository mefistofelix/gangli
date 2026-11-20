//! GUI language: Italian or English, from the system (`GANGLI_LANG=it|en` overrides); switchable in the window.

use std::sync::atomic::{AtomicBool, Ordering};

static ITALIAN: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let lang = std::env::var("GANGLI_LANG").ok().unwrap_or_else(system_language);
    ITALIAN.store(lang.to_lowercase().starts_with("it"), Ordering::Relaxed);
}

pub fn italian() -> bool {
    ITALIAN.load(Ordering::Relaxed)
}

pub fn set_italian(on: bool) {
    ITALIAN.store(on, Ordering::Relaxed);
}

/// The text in the current language: `tr("Machines", "Macchine")`.
pub fn tr(en: &'static str, it: &'static str) -> &'static str {
    if italian() {
        it
    } else {
        en
    }
}

/// Owned variant for formatted text.
pub fn trf(en: String, it: String) -> String {
    if italian() {
        it
    } else {
        en
    }
}

#[cfg(windows)]
fn system_language() -> String {
    // LANGID: the low 10 bits are the primary language (0x10 = Italian).
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    if id & 0x3ff == 0x10 {
        "it".into()
    } else {
        "en".into()
    }
}

#[cfg(unix)]
fn system_language() -> String {
    for v in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(s) = std::env::var(v) {
            if !s.is_empty() && s != "C" && s != "POSIX" {
                return s;
            }
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(o) = std::process::Command::new("defaults").args(["read", "-g", "AppleLocale"]).output() {
        return String::from_utf8_lossy(&o.stdout).trim().to_string();
    }
    "en".into()
}
