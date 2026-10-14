//! `tine --self-check`: whether git and Tine's own helpers can run, without
//! a window. Install scripts and the packages' tests run it; so can anyone
//! who wonders why a push or a rebase can't ask what it needs.

use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

/// Runs the checks, printing each; the exit code (0: all fine).
pub fn run() -> i32 {
    let mut ok = true;
    println!("{}", crate::cli::version());
    if let Some(image) = crate::appimage::image() {
        println!("running from {}", image.to_string_lossy());
    }
    let git = tine_core::version::git_version(OsStr::new("git"), Duration::from_secs(5));
    match (&git, tine_core::version::git_problem(&git)) {
        (_, Some(problem)) => {
            ok = false;
            println!("git: {problem}");
        }
        (Ok(v), None) => println!("git: {v}"),
        (Err(_), None) => unreachable!("an error is a problem"),
    }
    match crate::askpass::program() {
        Ok(helper) => {
            println!("helper: {}", helper.display());
            // Tests replace or remove the installed file here.
            if let Some(gate) = std::env::var_os("TINE_SELF_CHECK_PAUSE") {
                wait_until_gone(Path::new(&gate));
            }
            match helper_answers(&helper) {
                Ok(()) => println!("helper: runs"),
                Err(e) => {
                    ok = false;
                    println!("helper: {e}");
                }
            }
        }
        Err(e) => {
            ok = false;
            println!("helper: {e:#}");
        }
    }
    let leaked = leaked_variables();
    if leaked.is_empty() {
        println!("environment: clean");
    } else {
        ok = false;
        println!("environment: {} reach programs Tine starts", leaked.join(", "));
    }
    if ok { 0 } else { 1 }
}

fn wait_until_gone(path: &Path) {
    let start = Instant::now();
    while path.exists() && start.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `'…'` for the shell (as git gets the editor's command).
fn shell_quoted(path: &Path) -> OsString {
    let mut out = b"'".to_vec();
    for &b in path.as_os_str().as_bytes() {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    OsString::from_vec(out)
}

/// The helper, started the way git starts an editor (a shell command
/// line), says the version of this Tine.
fn helper_answers(helper: &Path) -> Result<(), String> {
    let mut line = shell_quoted(helper);
    line.push(" --version");
    let out = Command::new("sh").arg("-c").arg(&line).output().map_err(|e| format!("can't run the shell: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() {
        return Err(format!("doesn't run ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim()));
    }
    if said != crate::cli::version() {
        return Err(format!("is another version ({said:?})"));
    }
    Ok(())
}

/// AppImage variables a program started from Tine would get.
fn leaked_variables() -> Vec<&'static str> {
    let Ok(out) = Command::new("sh").arg("-c").arg("env").output() else { return Vec::new() };
    let env = String::from_utf8_lossy(&out.stdout);
    crate::appimage::VARIABLES.into_iter().filter(|v| env.lines().any(|l| l.starts_with(&format!("{v}=")))).collect()
}
