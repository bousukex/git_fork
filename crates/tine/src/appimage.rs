//! Running from an AppImage: the variables its runtime sets (`APPIMAGE`,
//! `APPDIR`, `ARGV0`, `OWD`) are noted for the About dialog and taken out
//! of Tine's environment, so git, hooks, terminals and editors started from
//! Tine get the session's environment as it was.

use std::{ffi::OsString, sync::OnceLock};

/// The variables an AppImage's runtime sets.
pub const VARIABLES: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];

static NOTED: OnceLock<Vec<(&'static str, OsString)>> = OnceLock::new();

/// Notes and removes the AppImage variables (when `APPIMAGE` is set).
///
/// # Safety
/// Call it before any other thread starts (it changes the environment).
pub unsafe fn take_environment() {
    let mut noted = Vec::new();
    if std::env::var_os("APPIMAGE").is_some() {
        for name in VARIABLES {
            if let Some(v) = std::env::var_os(name) {
                noted.push((name, v));
                // SAFETY: the caller promises no other thread runs yet.
                unsafe { std::env::remove_var(name) };
            }
        }
    }
    let _ = NOTED.set(noted);
}

/// The AppImage file Tine runs from, if it does.
pub fn image() -> Option<&'static std::ffi::OsStr> {
    NOTED.get()?.iter().find(|(n, _)| *n == "APPIMAGE").map(|(_, v)| v.as_os_str())
}
