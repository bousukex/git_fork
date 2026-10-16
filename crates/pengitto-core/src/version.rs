//! Which git is there, and whether it is new enough for pengitto.

use std::{
    ffi::OsStr,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// The oldest git pengitto works with (it relies on options of 2.43).
pub const MIN_GIT: GitVersion = GitVersion(2, 43, 0);

/// A git version (major, minor, patch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion(pub u32, pub u32, pub u32);

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Reads `git version 2.43.0` (and the like: `2.39.3 (Apple Git-146)`,
/// `2.44.0.windows.1`, `2.45.0-rc1`).
pub fn parse(out: &str) -> Option<GitVersion> {
    let rest = out.trim().strip_prefix("git version ")?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // The patch number may be missing (`2.44`).
    let patch = rest
        .split_whitespace()
        .next()
        .and_then(|v| v.split('.').nth(2))
        .and_then(|p| p.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    Some(GitVersion(major, minor, patch))
}

/// Asks `program` (git) for its version, giving up after `timeout` — for
/// the whole of it: its exit and its output (a wrapper may leave a process
/// behind that holds the output open; what was written by then counts).
pub fn git_version(program: &OsStr, timeout: Duration) -> Result<GitVersion, String> {
    use std::{io::Read as _, os::unix::process::CommandExt as _};
    let name = program.to_string_lossy();
    let mut child = Command::new(program)
        .arg("--version")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own group: whatever it leaves behind is stopped with it.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("can't run {name}: {e}"))?;
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let mut stdout = child.stdout.take().expect("piped");
    let sink = out.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 256];
        while let Ok(n) = stdout.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
        }
        let _ = done_tx.send(());
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => break None,
            Err(e) => return Err(e.to_string()),
        }
    };
    // The output's end, within what is left of the time.
    let finished = status.is_some() && done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok();
    if !finished {
        // SAFETY: a signal to the process group just made for it.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = child.wait();
    }
    let Some(status) = status else { return Err(format!("{name} --version didn't answer in {timeout:?}")) };
    if !status.success() {
        return Err(format!("{name} --version failed ({status})"));
    }
    let out = String::from_utf8_lossy(&out.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    parse(&out).ok_or_else(|| format!("can't read the version in {:?}", out.trim()))
}

/// What is wrong with the git at hand for pengitto, if anything.
pub fn git_problem(found: &Result<GitVersion, String>) -> Option<String> {
    match found {
        Ok(v) if *v >= MIN_GIT => None,
        Ok(v) => Some(format!("git {v} is older than {}.{}, which pengitto needs: some actions will fail", MIN_GIT.0, MIN_GIT.1)),
        Err(e) => Some(format!("git isn't usable ({e}): pengitto needs git {}.{} or newer", MIN_GIT.0, MIN_GIT.1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_as_git_prints_them() {
        assert_eq!(parse("git version 2.43.0\n"), Some(GitVersion(2, 43, 0)));
        assert_eq!(parse("git version 2.39.3 (Apple Git-146)"), Some(GitVersion(2, 39, 3)));
        assert_eq!(parse("git version 2.44.0.windows.1"), Some(GitVersion(2, 44, 0)));
        assert_eq!(parse("git version 2.45.0-rc1"), Some(GitVersion(2, 45, 0)));
        assert_eq!(parse("git version 2.44"), Some(GitVersion(2, 44, 0)));
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("git version x"), None);
        assert!(GitVersion(2, 43, 0) >= MIN_GIT && GitVersion(2, 42, 9) < MIN_GIT && GitVersion(3, 0, 0) > MIN_GIT);
        assert!(git_problem(&Ok(GitVersion(2, 51, 0))).is_none());
        assert!(git_problem(&Ok(GitVersion(2, 34, 1))).unwrap().contains("older than 2.43"));
        assert!(git_problem(&Err("not found".into())).unwrap().contains("isn't usable"));
    }

    #[test]
    fn asks_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            p
        };
        let old = script("old", "echo 'git version 2.34.1'");
        assert_eq!(git_version(old.as_os_str(), Duration::from_secs(5)), Ok(GitVersion(2, 34, 1)));
        let garbage = script("garbage", "echo nope");
        assert!(git_version(garbage.as_os_str(), Duration::from_secs(5)).unwrap_err().contains("can't read"));
        let slow = script("slow", "exec sleep 10");
        let t = Instant::now();
        assert!(git_version(slow.as_os_str(), Duration::from_millis(200)).unwrap_err().contains("didn't answer"));
        assert!(t.elapsed() < Duration::from_secs(3));
        // A wrapper that answers and leaves something behind holding the
        // output open: within the time, with what it said.
        let lingering = script("lingering", "echo 'git version 2.44.1'\nsleep 10 &");
        let t = Instant::now();
        assert_eq!(git_version(lingering.as_os_str(), Duration::from_millis(500)), Ok(GitVersion(2, 44, 1)));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        // It answers, but fails.
        let failing = script("failing", "echo 'git version 2.44.1'\nexit 3");
        assert!(git_version(failing.as_os_str(), Duration::from_secs(5)).unwrap_err().contains("failed"));
        assert!(git_version(dir.path().join("missing").as_os_str(), Duration::from_secs(1)).unwrap_err().contains("can't run"));
        assert!(git_version(OsStr::new("git"), Duration::from_secs(10)).is_ok());
    }
}
